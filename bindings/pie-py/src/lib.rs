//! `pie._pie_rs` —— pie 核心层的 Python 绑定（PyO3）。
//!
//! 分工：**本 crate 只做桥接**（类型转换、GIL 纪律、事件分发、异常映射），
//! 一切业务逻辑仍在 `pie`（Rust 核心库）里。规划见仓库 `docs/python-bindings.md`。
//!
//! 三条贯穿全文件的纪律（别在别处破例）：
//!   1. **长任务必须释放 GIL**：模型请求 / 工具执行可能跑几十秒，持 GIL 会冻住整个解释器
//!      → 一律 `py.detach(|| runtime().block_on(fut))`。
//!   2. **不在 tokio 线程里跑 Python 代码**：事件推 `mpsc`，由**持有 GIL 的调用线程**取出来分发
//!      （见 `session::PySession::aturn` 的 pump 循环）。
//!   3. **借用冲突报错而不阻塞**：`PySession` 内部是 `tokio::sync::Mutex`，忙时 `try_lock` 失败
//!      → 抛 `RuntimeError("session 正忙")`，避免回调里回头调同一个 session 造成死锁。

mod config;
mod llm;
mod session;
mod tools;

use std::sync::OnceLock;

use pyo3::create_exception;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyList, PyTuple};
use serde_json::Value;

pub use config::PyConfig;
pub use llm::PyLlmClient;
pub use session::{PyCancel, PySession, PySubscription};
pub use tools::PyToolRegistry;

// ---------------------------------------------------------------- 异常层级

create_exception!(
    pie,
    PieError,
    pyo3::exceptions::PyException,
    "pie 的错误基类"
);
create_exception!(pie, ConfigError, PieError, "配置读取 / 保存失败");
create_exception!(pie, LlmError, PieError, "模型请求失败（实例上带 .status）");
create_exception!(pie, ToolError, PieError, "工具执行失败");

/// 会话正忙（同一个 session 上重入 `aturn`，或回合进行中读 `messages`）。
pub(crate) fn busy_error() -> PyErr {
    PyRuntimeError::new_err("session 正忙（回合进行中）：回调里不要碰同一个 Session")
}

pub(crate) fn pie_error(msg: impl std::fmt::Display) -> PyErr {
    PieError::new_err(msg.to_string())
}

/// 解析按次的 `response_format` 参数：`None` / `""` / `text` = 不发该字段，`json_object` = 要求合法 JSON。
///
/// ⚠ 别叫 `response_format`：同名局部变量会遮蔽函数（值命名空间同一个）。
pub(crate) fn parse_response_format(value: Option<&str>) -> PyResult<pie::llm::ResponseFormat> {
    match value {
        None => Ok(pie::llm::ResponseFormat::Text),
        Some(v) => pie::llm::ResponseFormat::from_name(v)
            .ok_or_else(|| pie_error(format!("response_format 只认 text / json_object：{v:?}"))),
    }
}

/// `ConfigError` → Python `ConfigError`。
pub(crate) fn config_error(e: pie::config::ConfigError) -> PyErr {
    ConfigError::new_err(e.to_string())
}

/// 核心的 `LlmError` → Python `LlmError`，并把 HTTP 状态码挂到实例的 `.status` 上。
///
/// ⚠ `status` **永远**挂上（不是服务端返回的错就是 `None`，如连接失败）—— 存根写的是
/// `status: int | None`，只挂 `Some` 会让「连不上」那条路读到 `AttributeError`（与存根/文档不符）。
pub(crate) fn llm_error(py: Python<'_>, e: pie::llm::LlmError) -> PyErr {
    let status = e.status();
    let type_object = py.get_type::<LlmError>();
    match type_object.call1((e.to_string(),)) {
        Ok(instance) => {
            let _ = instance.setattr("status", status);
            PyErr::from_value(instance)
        }
        // 连异常都造不出来（内存不足之类）：退回普通写法，别把原始错误吞了
        Err(err) => err,
    }
}

// ---------------------------------------------------------------- 运行时

/// 进程级 tokio runtime：`aturn` / 模型请求都跑在它上面。
///
/// **不随 Python 事件循环生死**（这是将来做原生 async 也要守住的一点：一个进程一个 runtime）。
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("pie")
            .build()
            .expect("建 tokio runtime 失败")
    })
}

// ---------------------------------------------------------------- 类型桥

/// `serde_json::Value` → Python 对象（dict / list / str / int / float / bool / None）。
///
/// 走它而不是给每个类型写 pyclass：`Message` / `TurnEvent` / `Usage` / `Config` 在 Rust 侧
/// 本来就都是 serde 的，字段名又与落盘的 dict 形状对齐 → 桥接零维护。
pub(crate) fn json_to_py(py: Python<'_>, value: &Value) -> PyResult<Py<PyAny>> {
    Ok(match value {
        Value::Null => py.None(),
        Value::Bool(b) => (*b).into_pyobject(py)?.to_owned().into_any().unbind(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_pyobject(py)?.into_any().unbind()
            } else if let Some(u) = n.as_u64() {
                u.into_pyobject(py)?.into_any().unbind()
            } else {
                n.as_f64()
                    .unwrap_or(f64::NAN)
                    .into_pyobject(py)?
                    .into_any()
                    .unbind()
            }
        }
        Value::String(s) => s.as_str().into_pyobject(py)?.into_any().unbind(),
        Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(json_to_py(py, item)?)?;
            }
            list.into_any().unbind()
        }
        Value::Object(map) => {
            let dict = PyDict::new(py);
            for (key, item) in map {
                dict.set_item(key, json_to_py(py, item)?)?;
            }
            dict.into_any().unbind()
        }
    })
}

/// 任意 serde 值 → Python 对象（转换失败只可能是内部错误）。
pub(crate) fn to_py<T: serde::Serialize>(py: Python<'_>, value: &T) -> PyResult<Py<PyAny>> {
    let json = serde_json::to_value(value).map_err(pie_error)?;
    json_to_py(py, &json)
}

/// Python 对象 → `serde_json::Value`（`json_to_py` 的逆向）——`Config.update(dict)` 用。
///
/// 只认 JSON 能表达的那些：`dict` / `list` / `tuple` / `str` / `int` / `float` / `bool` / `None`；
/// 别的类型直接报错（别悄悄塞个字符串进去）。
pub(crate) fn py_to_json(value: &Bound<'_, PyAny>) -> PyResult<Value> {
    if value.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(b) = value.cast::<PyBool>() {
        return Ok(Value::Bool(b.is_true()));
    }
    if let Ok(i) = value.extract::<i64>() {
        return Ok(Value::from(i));
    }
    // `u64` 也要认（`usize` 字段回填时可能是大正整数）
    if let Ok(u) = value.extract::<u64>() {
        return Ok(Value::from(u));
    }
    if let Ok(f) = value.extract::<f64>() {
        return Ok(serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number));
    }
    if let Ok(s) = value.extract::<String>() {
        return Ok(Value::String(s));
    }
    if let Ok(dict) = value.cast::<PyDict>() {
        let mut map = serde_json::Map::new();
        for (k, v) in dict.iter() {
            map.insert(k.extract::<String>()?, py_to_json(&v)?);
        }
        return Ok(Value::Object(map));
    }
    if let Ok(seq) = value.cast::<PyList>() {
        let mut items = Vec::with_capacity(seq.len());
        for item in seq.iter() {
            items.push(py_to_json(&item)?);
        }
        return Ok(Value::Array(items));
    }
    if let Ok(seq) = value.cast::<PyTuple>() {
        let mut items = Vec::with_capacity(seq.len());
        for item in seq.iter() {
            items.push(py_to_json(&item)?);
        }
        return Ok(Value::Array(items));
    }
    Err(pie_error(format!(
        "这个类型不能当配置值：{}",
        value.get_type().name()?
    )))
}

// ---------------------------------------------------------------- 模块

/// `run` / `arun` 共用的默认件：`config=None` 时读 `~/.pie/config.toml`（文件不在就用默认值）；
/// `llm` / `tools` 不传就按 config 造（内置五件）。
///
/// ⚠ 两者共用**同一处**解析：否则默认值（比如将来换默认模型/工具集）会分叉成两份。
fn resolve_runtime(
    py: Python<'_>,
    config: Option<&crate::config::PyConfig>,
    llm: Option<&crate::llm::PyLlmClient>,
    tools: Option<&crate::tools::PyToolRegistry>,
) -> PyResult<(
    pie::config::Config,
    pie::llm::LlmClient,
    pie::tools::ToolRegistry,
)> {
    let core_config = match config {
        Some(c) => c.inner.clone(),
        None => pie::config::Config::load(None).map_err(config_error)?,
    };
    let client = match llm {
        Some(l) => l.inner.clone(),
        None => pie::llm::LlmClient::new(&core_config).map_err(|e| llm_error(py, e))?,
    };
    let registry = match tools {
        Some(t) => t.inner.clone(),
        None => pie::tools::ToolRegistry::new(core_config.tool_defaults()),
    };
    Ok((core_config, client, registry))
}

/// 一次性任务（**无会话、不落盘**）：建个临时会话跑一回合，返回最终答复。**同步版**。
///
/// `config=None` 时读 `~/.pie/config.toml`
/// （文件不在就用默认值）；`llm` / `tools` 不传就按 config 造（内置五件套）。
///
/// 五个按次旋钮与 [`Session.turn`] / [`Session.aturn`] 同义（`max_steps=None` = 不限、
/// `stream=None` = 默认流式、`parallel_tools=None` = 跟随 `Config.parallel_tools`、
/// `reasoning_effort=None` = 用配置里的思考深度、`response_format=None` = `text`）。
// ⚠ 参数 10 个（clippy 会念）是故意的：Python 侧五个按次旋钮 + 三个可注入对象，与 `arun` / `Session.turn` 一一对应。
#[allow(clippy::too_many_arguments)]
#[pyfunction]
#[pyo3(signature = (task, config=None, llm=None, tools=None, max_steps=None, stream=None, parallel_tools=None, reasoning_effort=None, response_format=None))]
fn run(
    py: Python<'_>,
    task: String,
    config: Option<&crate::config::PyConfig>,
    llm: Option<&crate::llm::PyLlmClient>,
    tools: Option<&crate::tools::PyToolRegistry>,
    max_steps: Option<usize>,
    stream: Option<bool>,
    parallel_tools: Option<bool>,
    reasoning_effort: Option<&str>,
    response_format: Option<&str>,
) -> PyResult<String> {
    let (core_config, client, registry) = resolve_runtime(py, config, llm, tools)?;
    let session = crate::session::PySession::wrap(pie::session::Session::ephemeral(
        &core_config,
        client,
        registry,
    ));
    session.turn(
        py,
        task,
        None,
        max_steps,
        stream,
        parallel_tools,
        reasoning_effort.map(str::to_string),
        response_format.map(str::to_string),
    )
}

/// `run` 的**异步**版：语义完全一样（一次性、无会话、不落盘），但返回可 await 的对象。
///
/// 不绕线程：回合直接跑在**进程级** runtime 上（与 `Session.aturn` 同一条路）——
/// `await pie.arun(...)` 期间事件循环不阻塞、也不额外占线程。
/// ⚠ 与 `aturn` 一样要求调用时处于运行中的 asyncio loop（**一个进程一个 loop 最稳**）。
#[cfg(feature = "asyncio")]
// ⚠ 参数 10 个（clippy 会念）是故意的：与 `run` 同形（见上）。
#[allow(clippy::too_many_arguments)]
#[pyfunction]
#[pyo3(signature = (task, config=None, llm=None, tools=None, max_steps=None, stream=None, parallel_tools=None, reasoning_effort=None, response_format=None))]
fn arun(
    py: Python<'_>,
    task: String,
    config: Option<&crate::config::PyConfig>,
    llm: Option<&crate::llm::PyLlmClient>,
    tools: Option<&crate::tools::PyToolRegistry>,
    max_steps: Option<usize>,
    stream: Option<bool>,
    parallel_tools: Option<bool>,
    reasoning_effort: Option<String>,
    response_format: Option<String>,
) -> PyResult<Py<PyAny>> {
    let (core_config, client, registry) = resolve_runtime(py, config, llm, tools)?;
    // `response_format` 在持 GIL 这层解析（PyErr 不能穿过回合块）；`reasoning_effort` 是
    // `String`（按值带进 async 块——`Option<&str>` 借自 py 数据，进不了 `'static` future）
    let response_format = crate::parse_response_format(response_format.as_deref())?;
    let mut session = pie::session::Session::ephemeral(&core_config, client, registry);
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let request_options = pie::llm::RequestOptions::ChatCompletions {
            reasoning_effort: reasoning_effort.as_deref(),
            response_format,
            stream,
        };
        // 没有事件出口（`run` 本来就不订阅）→ 不注册监听者就行，总线自己会丢弃
        let result = session
            .aturn(&task, max_steps, parallel_tools, request_options)
            .await;
        result.map_err(|e| Python::attach(|py| llm_error(py, e)))
    })
    .map(|obj| obj.unbind())
}

/// 列出历史会话（按 mtime 降序）：`[{id, file, mtime, size, turns, api_calls, first_query}]`。
///
/// 键名与 CLI `pie sessions --json` 一致。`limit=None` = 全部。
#[pyfunction]
#[pyo3(signature = (limit=None))]
fn list_sessions(py: Python<'_>, limit: Option<usize>) -> PyResult<Py<PyAny>> {
    use pyo3::types::PyList;
    let list = PyList::empty(py);
    // 与 CLI 同口径：走默认数据根（`PIE_DIR` → `./.pie` → `~/.pie`）
    let storage = pie::config::Storage::default();
    for r in pie::cli::list_sessions(&storage, limit) {
        let d = PyDict::new(py);
        d.set_item("id", r.id)?;
        d.set_item("file", r.path.display().to_string())?;
        d.set_item("mtime", r.mtime)?;
        d.set_item("size", r.size)?;
        d.set_item("turns", r.turns)?;
        d.set_item("api_calls", r.api_calls)?;
        d.set_item("first_query", r.first_query)?;
        list.append(d)?;
    }
    Ok(list.into_any().unbind())
}

#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[pymodule]
fn _pie_rs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(run, m)?)?;
    #[cfg(feature = "asyncio")]
    m.add_function(wrap_pyfunction!(arun, m)?)?;
    m.add_function(wrap_pyfunction!(list_sessions, m)?)?;
    m.add_function(wrap_pyfunction!(crate::llm::llm, m)?)?;
    #[cfg(feature = "asyncio")]
    m.add_function(wrap_pyfunction!(crate::llm::allm, m)?)?;

    m.add_class::<PyConfig>()?;
    m.add_class::<PyLlmClient>()?;
    m.add_class::<PyToolRegistry>()?;
    m.add_class::<PySession>()?;
    m.add_class::<PyCancel>()?;
    m.add_class::<PySubscription>()?;

    m.add("PieError", py.get_type::<PieError>())?;
    m.add("ConfigError", py.get_type::<ConfigError>())?;
    m.add("LlmError", py.get_type::<LlmError>())?;
    m.add("ToolError", py.get_type::<ToolError>())?;
    Ok(())
}
