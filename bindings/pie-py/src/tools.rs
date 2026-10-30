//! `Registry`：发给模型的工具集 —— 内置工具 + **从 Python 注册的工具**（M3）。
//!
//! 注册进来的工具走核心的 `Registry::with_dynamic`：名字/描述/schema 由 Python 侧给
//!（`@pie.tool` 从函数签名 + 类型注解生成），调用体就是下面这段「往 Python 里再叫一次」。
//! 两条路（内置 / Python）在核心那边是同一条分发链，模型看不出区别。

use std::collections::HashMap;
use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use serde_json::Value;

use pie::tools::{self, CallFn, Registry, ToolError};

use crate::config::PyConfig;

/// 工具名得是 `[A-Za-z0-9_-]{1,64}`：OpenAI 兼容端点对 `function.name` 有这条硬要求，
/// 注册期就拦下来（不然要等整轮请求 400 才知道）。
fn check_tool_name(name: &str) -> PyResult<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(PyValueError::new_err(format!(
            "非法的工具名 {name:?}：只能用 [A-Za-z0-9_-]、长度 1..=64（API 的 function.name 要求）"
        )))
    }
}

/// Python 工具返回值 → `ToolOutput`。
///
/// 与 `pyplugin` 宿主**同一套契约**（`docs/extension.md`「工具返回值」）：
///
/// - `str`：原样当正文（作者自己排 `header!` 块）；
/// - `dict`：`{"body": str, "header": {…}, "state": {…}}`，键都可选。头区由 Rust 渲染
///   （`header::render` 是唯一一份），`state` 落进会话槽（`meta["state"][<工具名>]`）。
///
/// 不认识的键直接报错 —— `{"bdy": …}` 静默忽略只会让人查半天。
fn result_of(
    raw: &Bound<'_, PyAny>,
    tool: &str,
    ctx: &tools::ToolCtx,
) -> PyResult<tools::ToolOutput> {
    if let Ok(text) = raw.extract::<String>() {
        return Ok(tools::ToolOutput::text(text));
    }
    let dict = raw.cast::<PyDict>().map_err(|_| {
        PyValueError::new_err(format!(
            "工具 {tool} 必须返回 str 或 dict（{{'body','header','state'}}），拿到 {}",
            raw.get_type()
                .name()
                .map(|n| n.to_string())
                .unwrap_or_default()
        ))
    })?;
    for key in dict.keys() {
        let key: String = key.extract()?;
        if !matches!(key.as_str(), "body" | "header" | "state") {
            return Err(PyValueError::new_err(format!(
                "工具 {tool} 返回 dict 里有不认识的键：{key:?}（只认 body / header / state）"
            )));
        }
    }
    let field = |name: &str| -> PyResult<Option<Bound<'_, PyAny>>> {
        Ok(match dict.get_item(name)? {
            Some(v) if !v.is_none() => Some(v),
            _ => None,
        })
    };
    let body = match field("body")? {
        Some(v) => v.extract::<String>()?,
        None => String::new(),
    };
    let header = match field("header")? {
        Some(v) => {
            let value = crate::py_to_json(&v)?;
            if !value.is_object() {
                return Err(PyValueError::new_err(format!(
                    "工具 {tool} 的 header 必须是 dict"
                )));
            }
            Some(value)
        }
        None => None,
    };
    if let Some(state) = field("state")? {
        let value = crate::py_to_json(&state)?;
        if !value.is_object() {
            return Err(PyValueError::new_err(format!(
                "工具 {tool} 的 state 必须是 dict"
            )));
        }
        ctx.state.tool_states().commit(tool, value);
    }
    let mut headers = tools::Header::new();
    if let Some(Value::Object(map)) = header {
        headers.extend(map);
    }
    Ok(tools::ToolOutput {
        headers,
        body: tools::ToolBody {
            content: body,
            spill: None,
        },
        attachments: Vec::new(),
        status: tools::Status::Success,
    })
}

#[pyclass(name = "Registry", module = "pie")]
pub struct PyRegistry {
    pub inner: Registry,
}

#[pymethods]
impl PyRegistry {
    /// 空注册表（自己往里 `register(...)`）。内置工具用 `builtins()`。
    #[new]
    fn new() -> Self {
        Self {
            inner: Registry::empty(HashMap::new()),
        }
    }

    /// 内置工具集（read / edit / writ / bash / repl），私有参数从配置的 `[tools.<名字>]` 段注入。
    #[staticmethod]
    fn builtins(config: &PyConfig) -> Self {
        Self {
            inner: Registry::new(config.inner.tool_defaults()),
        }
    }

    /// 按 `--tools` 那套说明裁剪：内置名启用该工具，其它名字当 shell 子命令白名单
    /// （`"read,ls,grep"` = read + 只允许 ls/grep 的 shell）。
    /// Python 工具由你自己 `register(...)` 加进来，所以这里只能裁内置那几件。
    #[staticmethod]
    #[pyo3(signature = (spec, config=None))]
    fn from_spec(spec: &str, config: Option<&PyConfig>) -> Self {
        let defaults = config.map(|c| c.inner.tool_defaults()).unwrap_or_default();
        Self {
            inner: tools::tools_from_spec(Some(spec), Registry::new(defaults)),
        }
    }

    fn names(&self) -> Vec<String> {
        self.inner.names().iter().map(|n| n.to_string()).collect()
    }

    /// 发给模型的 `tools` 数组（OpenAI 线上形状），可直接拿去自查工具 schema。
    fn tool_specs(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::to_py(py, &self.inner.tool_specs())
    }

    /// 注册一个 **Python 函数**当工具。
    ///
    /// 两种用法（都行）：
    ///   - `reg.register(pie.tool()(fn))`：传 `@pie.tool` 生成的封装（读它的
    ///     `.name` / `.description` / `.parameters` / `.handler`）；
    ///   - `reg.register(name="x", handler=fn, description="…", parameters={...})`：直接给零件。
    ///
    /// 注册语义：重名报 `ValueError`；handler 的返回
    /// 值当工具结果文本（必须是 `str`）；**抛异常会被文本化**回给模型（`[工具错误] …`），
    /// 不打断整个回合。
    ///
    /// ⚠ 三条纪律：
    ///   1. handler 得是**同步**函数（绑定这条链是同步调用它；`async def` 只有 pyplugin 宿主能跑）；
    ///   2. 它跑在 runtime 的 worker 线程上、期间持有 GIL —— 别在里面等别的线程；
    ///   3. **别在 handler 里碰同一个 Session**（会拿到「session 正忙」，那是防死锁）。
    #[pyo3(signature = (tool=None, *, name=None, description=None, parameters=None, handler=None))]
    fn register(
        &mut self,
        py: Python<'_>,
        tool: Option<&Bound<'_, PyAny>>,
        name: Option<String>,
        description: Option<String>,
        parameters: Option<&Bound<'_, PyAny>>,
        handler: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        // ① 解析出四个零件（两种用法都收敛到这儿）
        let (name, description, parameters, handler) = match tool {
            Some(t) => (
                t.getattr("name")?.extract::<String>()?,
                t.getattr("description")?.extract::<String>()?,
                t.getattr("parameters")?,
                t.getattr("handler")?.unbind(),
            ),
            None => {
                let name = name.ok_or_else(|| {
                    PyValueError::new_err("register() 要么给 tool=，要么给 name= 与 handler=")
                })?;
                let handler = handler.ok_or_else(|| {
                    PyValueError::new_err("register() 要么给 tool=，也要么给 name= 与 handler=")
                })?;
                let parameters = match parameters {
                    Some(p) => p.clone(),
                    // 没给 schema 就按「无参数对象」发（模型至少知道有这么个工具）
                    None => PyDict::new(py).into_any(),
                };
                (name, description.unwrap_or_default(), parameters, handler)
            }
        };
        check_tool_name(&name)?;
        if self.inner.names().contains(&name.as_str()) {
            return Err(PyValueError::new_err(format!("工具已存在: {name}")));
        }
        // ② 同步 handler 才收（async 的要等 M5 的 async 入口，不然只能在一个跑着的 loop 上 await）
        let is_async = py
            .import("inspect")?
            .getattr("iscoroutinefunction")?
            .call1((handler.bind(py),))?
            .is_truthy()?;
        if is_async {
            return Err(PyValueError::new_err(
                format!(
                    "工具 {name:?} 是 async 函数：绑定是同步调用 handler 的（拿到的会是个没 await 的协程）"
                ) + "——写成同步函数，或把工具文件交给 `pie` 二进制的 pyplugin（`~/.pie/plugins/`）跑",
            ));
        }
        let parameters_json = crate::py_to_json(&parameters)?;

        // ③ 调用体：tokio 线程上 attach 回 GIL 调 Python（handler 按**关键字**传参）
        let handler_for_call = handler.clone_ref(py);
        let tool_name = name.clone();
        let call: CallFn = Arc::new(move |args: Value, ctx: tools::ToolCtx| {
            let handler = Python::attach(|py| handler_for_call.clone_ref(py));
            let tool_name = tool_name.clone();
            Box::pin(async move {
                Python::attach(|py| -> tools::ToolResult {
                    let out = (|| -> PyResult<tools::ToolOutput> {
                        let kwargs = PyDict::new(py);
                        if let Some(map) = args.as_object() {
                            for (key, value) in map {
                                kwargs.set_item(key, crate::json_to_py(py, value)?)?;
                            }
                        } else {
                            return Err(PyValueError::new_err("工具参数必须是 JSON 对象"));
                        }
                        let raw = handler.bind(py).call((), Some(&kwargs))?;
                        result_of(&raw, &tool_name, &ctx)
                    })();
                    out.map_err(|e| ToolError(format!("{e}")))
                })
            })
        });

        // ④ 注册进核心（`with_dynamic` 要 self，所以先 clone 再换回）
        self.inner = self
            .inner
            .clone()
            .with_dynamic(&name, &description, parameters_json, call);
        Ok(())
    }

    fn __repr__(&self) -> String {
        format!("<pie.Registry {:?}>", self.inner.names())
    }
}
