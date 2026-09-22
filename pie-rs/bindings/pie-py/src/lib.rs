//! `pie_rs._pie_rs` —— pie-rs 核心层的 Python 绑定（PyO3）。
//!
//! 分工：**本 crate 只做桥接**（类型转换、GIL 纪律、事件分发、异常映射），
//! 一切业务逻辑仍在 `pie_rs`（Rust 核心库）里。规划见仓库 `docs/python-bindings.md`。
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
use pyo3::types::{PyDict, PyList};
use serde_json::Value;

pub use config::PyConfig;
pub use llm::PyLlmClient;
pub use session::{PyCancel, PySession};
pub use tools::PyToolRegistry;

// ---------------------------------------------------------------- 异常层级

create_exception!(pie_rs, PieError, pyo3::exceptions::PyException, "pie 的错误基类");
create_exception!(pie_rs, ConfigError, PieError, "配置读取 / 保存失败");
create_exception!(pie_rs, LlmError, PieError, "模型请求失败（实例上带 .status）");
create_exception!(pie_rs, ToolError, PieError, "工具执行失败");

/// 会话正忙（同一个 session 上重入 `aturn`，或回合进行中读 `messages`）。
pub(crate) fn busy_error() -> PyErr {
    PyRuntimeError::new_err("session 正忙（回合进行中）：回调里不要碰同一个 Session")
}

pub(crate) fn pie_error(msg: impl std::fmt::Display) -> PyErr {
    PieError::new_err(msg.to_string())
}

/// `ConfigError` → Python `ConfigError`。
pub(crate) fn config_error(e: pie_rs::config::ConfigError) -> PyErr {
    ConfigError::new_err(e.to_string())
}

/// 核心的 `LlmError` → Python `LlmError`，并把 HTTP 状态码挂到实例的 `.status` 上
/// （`None` = 不是服务端返回的错误，比如连接失败）。
pub(crate) fn llm_error(py: Python<'_>, e: pie_rs::llm::LlmError) -> PyErr {
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

// ---------------------------------------------------------------- 模块

#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[pymodule]
fn _pie_rs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add_function(wrap_pyfunction!(version, m)?)?;

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
