//! `ToolRegistry`：发给模型的工具集 —— 内置四件套 + **从 Python 注册的工具**（M3）。
//!
//! 注册进来的工具走核心的 `ToolRegistry::with_dynamic`：名字/描述/schema 由 Python 侧给
//!（`@pie.tool` 从函数签名 + 类型注解生成），调用体就是下面这段「往 Python 里再叫一次」。
//! 两条路（内置 / Python）在核心那边是同一条分发链，模型看不出区别。

use std::collections::HashMap;
use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use serde_json::Value;

use pie::tools::{self, CallFn, ToolError, ToolRegistry};

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

#[pyclass(name = "ToolRegistry", module = "pie")]
pub struct PyToolRegistry {
    pub inner: ToolRegistry,
}

#[pymethods]
impl PyToolRegistry {
    /// 空注册表（自己往里 `register(...)`）。内置四件套用 `builtins()`。
    #[new]
    fn new() -> Self {
        Self {
            inner: ToolRegistry::empty(HashMap::new()),
        }
    }

    /// 内置工具集（read / edit / write / shell），私有参数从配置的 `[tools.<名字>]` 段注入。
    #[staticmethod]
    fn builtins(config: &PyConfig) -> Self {
        Self {
            inner: ToolRegistry::new(config.inner.tool_defaults()),
        }
    }

    /// 按 `--tools` 那套说明裁剪：内置名启用该工具，其它名字当 shell 子命令白名单
    /// （`"read,ls,grep"` = read + 只允许 ls/grep 的 shell）。
    #[staticmethod]
    #[pyo3(signature = (spec, config=None))]
    fn from_spec(spec: &str, config: Option<&PyConfig>) -> Self {
        let defaults = config
            .map(|c| c.inner.tool_defaults())
            .unwrap_or_else(HashMap::new);
        Self {
            inner: tools::tools_from_spec(Some(spec), defaults),
        }
    }

    fn names(&self) -> Vec<String> {
        self.inner.names().iter().map(|n| n.to_string()).collect()
    }

    /// 发给模型的 `tools` 数组（OpenAI 线上形状），可直接拿去自查工具 schema。
    fn specs(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::to_py(py, &self.inner.specs())
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
    ///   1. handler 得是**同步**函数（`async def` 要等 M5 的 async 入口）；
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
            return Err(PyValueError::new_err(format!(
                "工具 {name:?} 是 async 函数：绑定现在只支持同步 handler（async 走 M5 的 aturn_async）"
            )));
        }
        let parameters_json = crate::py_to_json(&parameters)?;

        // ③ 调用体：tokio 线程上 attach 回 GIL 调 Python（handler 按**关键字**传参）
        let handler_for_call = handler.clone_ref(py);
        let tool_name = name.clone();
        let call: CallFn = Arc::new(move |args: Value, _ctx: tools::ToolCtx| {
            let handler = Python::attach(|py| handler_for_call.clone_ref(py));
            let tool_name = tool_name.clone();
            Box::pin(async move {
                Python::attach(|py| -> tools::ToolResult {
                    let out = (|| -> PyResult<String> {
                        let kwargs = PyDict::new(py);
                        if let Some(map) = args.as_object() {
                            for (key, value) in map {
                                kwargs.set_item(key, crate::json_to_py(py, value)?)?;
                            }
                        } else {
                            return Err(PyValueError::new_err("工具参数必须是 JSON 对象"));
                        }
                        let raw = handler.bind(py).call((), Some(&kwargs))?;
                        raw.extract::<String>().map_err(|_| {
                            PyValueError::new_err(format!(
                                "工具 {tool_name} 必须返回 str，拿到 {}",
                                raw.get_type().name().map(|n| n.to_string()).unwrap_or_default()
                            ))
                        })
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
        format!("<pie.ToolRegistry {:?}>", self.inner.names())
    }
}
