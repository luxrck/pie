//! `pie._pie_rs` —— pie-rs 核心层的 Python 绑定（PyO3）。
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
pub use session::{PyCancel, PySession};
pub use tools::PyToolRegistry;

// ---------------------------------------------------------------- 异常层级

create_exception!(pie, PieError, pyo3::exceptions::PyException, "pie 的错误基类");
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

/// `ConfigError` → Python `ConfigError`。
pub(crate) fn config_error(e: pie::config::ConfigError) -> PyErr {
    ConfigError::new_err(e.to_string())
}

/// 核心的 `LlmError` → Python `LlmError`，并把 HTTP 状态码挂到实例的 `.status` 上
/// （`None` = 不是服务端返回的错误，比如连接失败）。
pub(crate) fn llm_error(py: Python<'_>, e: pie::llm::LlmError) -> PyErr {
    let status = e.status();
    let type_object = py.get_type::<LlmError>();
    match type_object.call1((e.to_string(),)) {
        Ok(instance) => {
            if let Some(code) = status {
                let _ = instance.setattr("status", code);
            }
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
            .thread_name("pie-rs")
            .build()
            .expect("建 tokio runtime 失败")
    })
}

// ---------------------------------------------------------------- 类型桥

/// `serde_json::Value` → Python 对象（dict / list / str / int / float / bool / None）。
///
/// 走它而不是给每个类型写 pyclass：`Message` / `TurnEvent` / `Usage` / `Config` 在 Rust 侧
/// 本来就都是 serde 的，字段名又与 Python 版 `to_dict()` 逐字对齐 → 桥接零维护。
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
                n.as_f64().unwrap_or(f64::NAN).into_pyobject(py)?.into_any().unbind()
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

/// 一次性任务（**无会话、不落盘**）：建个临时会话跑一回合，返回最终答复。
///
/// 与纯 Python 版 `pie.run(task, config=…)` 同形；`config=None` 时读 `~/.pie/config.toml`
/// （文件不在就用默认值）；`llm` / `tools` 不传就按 config 造（内置四件套）。
///
/// 三个执行旋钮与 [`Session.aturn`] 同义（`max_steps=None` = 不限、`stream=None` = 默认流式、
/// `parallel_tools=None` = 跟随 `Config.parallel_tools`）。
#[pyfunction]
#[pyo3(signature = (task, config=None, llm=None, tools=None, max_steps=None, stream=None, parallel_tools=None))]
fn run(
    py: Python<'_>,
    task: String,
    config: Option<&crate::config::PyConfig>,
    llm: Option<&crate::llm::PyLlmClient>,
    tools: Option<&crate::tools::PyToolRegistry>,
    max_steps: Option<usize>,
    stream: Option<bool>,
    parallel_tools: Option<bool>,
) -> PyResult<String> {
    // 默认：读配置文件（不在就用默认值）——与 Python `resolve_config()` 同语义
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
    let session = crate::session::PySession::wrap(pie::session::Session::ephemeral(
        &core_config, client, registry,
    ));
    session.aturn(py, task, None, None, max_steps, stream, parallel_tools)
}

/// 列出历史会话（按 mtime 降序）：`[{id, file, mtime, size, turns, api_calls, first_query}]`。
///
/// 键名与 CLI `pie-rs sessions --json` / Python `pie sessions -j` 一致。`limit=None` = 全部。
#[pyfunction]
#[pyo3(signature = (limit=None))]
fn list_sessions(py: Python<'_>, limit: Option<usize>) -> PyResult<Py<PyAny>> {
    use pyo3::types::PyList;
    let list = PyList::empty(py);
    for r in pie::session::list_sessions(limit) {
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
    m.add_function(wrap_pyfunction!(list_sessions, m)?)?;

    m.add_class::<PyConfig>()?;
    m.add_class::<PyLlmClient>()?;
    m.add_class::<PyToolRegistry>()?;
    m.add_class::<PySession>()?;
    m.add_class::<PyCancel>()?;

    m.add("PieError", py.get_type::<PieError>())?;
    m.add("ConfigError", py.get_type::<ConfigError>())?;
    m.add("LlmError", py.get_type::<LlmError>())?;
    m.add("ToolError", py.get_type::<ToolError>())?;
    Ok(())
}
