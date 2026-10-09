//! `LlmClient`：模型后端（OpenAI 兼容）。
//!
//! 这里是同步外观：内部 `async fn` 交给进程级 runtime 跑，**期间释放 GIL**（§5.4）。

use pyo3::prelude::*;
use serde_json::Value;

use pie::llm::LlmClient;

use crate::config::PyConfig;
use crate::{llm_error, pie_error};

#[pyclass(name = "LlmClient", module = "pie")]
pub struct PyLlmClient {
    pub inner: LlmClient,
}

#[pymethods]
impl PyLlmClient {
    #[new]
    fn new(py: Python<'_>, config: &PyConfig) -> PyResult<Self> {
        LlmClient::new(&config.inner)
            .map(|inner| Self { inner })
            .map_err(|e| llm_error(py, e))
    }

    /// 当前模型 id（会话切模型时会话层会同步它）。
    #[getter]
    fn model(&self) -> &str {
        &self.inner.model
    }

    /// 这个模型支不支持 Files API（图片走 `file` 块而不是内联 base64）。
    #[staticmethod]
    fn model_supports_files(model: &str) -> bool {
        pie::llm::model_supports_files(model)
    }

    /// 列出端点可用模型 id。
    fn list_models(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        let inner = &self.inner;
        py.detach(|| crate::runtime().block_on(inner.list_models()))
            .map_err(|e| llm_error(py, e))
    }

    /// 同步调一次模型（**不吃工具循环**）：`messages` 是 API 形状的 dict 列表，`tools` 可选。
    ///
    /// 返回 `{"content", "reasoning_content", "tool_calls", "usage"}`。适合「拿模型当纯函数用」的场景（如生成评测清单），不要拿它跑回合
    /// （工具循环请用 `Session.aturn`）。
    ///
    /// `reasoning_effort` / `response_format` 是**按次**覆盖（口径同 `Session.aturn`）：
    /// 前者 `None` = 用客户端的思考深度，后者 `None` = `text`。
    ///
    /// ⚠ 阻塞（内部含重试），期间释放 GIL。
    #[pyo3(signature = (messages, tools=None, reasoning_effort=None, response_format=None))]
    fn complete(
        &self,
        py: Python<'_>,
        messages: &Bound<'_, PyAny>,
        tools: Option<&Bound<'_, PyAny>>,
        reasoning_effort: Option<&str>,
        response_format: Option<&str>,
    ) -> PyResult<Py<PyAny>> {
        let msgs: Vec<pie::llm::Message> = serde_json::from_value(crate::py_to_json(messages)?)
            .map_err(|e| pie_error(format!("messages 解析失败（要 API 形状的 dict 列表）: {e}")))?;
        let specs: Vec<Value> = match tools {
            Some(t) => crate::py_to_json(t)?
                .as_array()
                .cloned()
                .ok_or_else(|| pie_error("tools 要是数组"))?,
            None => Vec::new(),
        };
        let inner = &self.inner;
        // 按次覆盖的两个值拼成 [`RequestOptions`]（解析放在 `detach` 之前：PyErr 要 GIL）
        let request_options = pie::llm::RequestOptions::ChatCompletions {
            reasoning_effort,
            response_format: crate::parse_response_format(response_format)?,
        };
        let got = py
            .detach(|| crate::runtime().block_on(inner.complete(&msgs, &specs, request_options)))
            .map_err(|e| llm_error(py, e))?;
        // `LlmResult` 没有 Serialize → 手拼
        let dict = pyo3::types::PyDict::new(py);
        dict.set_item("content", got.content)?;
        dict.set_item("reasoning_content", got.reasoning_content)?;
        dict.set_item("tool_calls", crate::to_py(py, &got.tool_calls)?)?;
        dict.set_item("usage", crate::to_py(py, &got.usage)?)?;
        Ok(dict.into_any().unbind())
    }

    /// 查询账号余额（`GET /user/balance`，DeepSeek 扩展）。
    ///
    /// 返回 `{"is_available": bool, "balance_infos": [{"currency", "total_balance",
    /// "granted_balance", "topped_up_balance"}]}`（金额是**字符串**，与服务端一致）；
    /// 非 OpenAI 兼容端点大多没这个接口（一般 404，抛 `LlmError`）。
    fn fetch_balance(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let inner = &self.inner;
        let balance = py
            .detach(|| crate::runtime().block_on(inner.fetch_balance()))
            .map_err(|e| llm_error(py, e))?;
        let dict = pyo3::types::PyDict::new(py);
        dict.set_item("is_available", balance.is_available)?;
        let infos: Vec<Py<PyAny>> = balance
            .balance_infos
            .iter()
            .map(|b| {
                let d = pyo3::types::PyDict::new(py);
                d.set_item("currency", &b.currency)?;
                d.set_item("total_balance", &b.total_balance)?;
                d.set_item("granted_balance", &b.granted_balance)?;
                d.set_item("topped_up_balance", &b.topped_up_balance)?;
                Ok(d.into_any().unbind())
            })
            .collect::<PyResult<_>>()?;
        dict.set_item("balance_infos", infos)?;
        Ok(dict.into_any().unbind())
    }

    /// 切换思考深度（`None` / `"none"` = 关闭思考）。
    #[pyo3(signature = (level=None))]
    fn set_reasoning_effort(&mut self, level: Option<&str>) {
        self.inner.set_reasoning_effort(level);
    }

    fn __repr__(&self) -> String {
        format!("<pie.LlmClient model={}>", self.inner.model)
    }
}

// ---------------------------------------------------------------- 一次性问一句

/// `llm` / `allm` 共用的前半段：参数 →（配置 → 客户端）+ 消息列表。
///
/// `config=None` → 读 `~/.pie/config.toml`（与 `pie.run` 同一处解析）；
/// `model` / `max_tokens` 是按次覆盖（`max_tokens` 就是发给 API 的 `max_tokens`＝配置里的 `reserved_tokens`）。
fn prepare(
    py: Python<'_>,
    prompt: &str,
    system: Option<&str>,
    model: Option<&str>,
    config: Option<&PyConfig>,
    max_tokens: Option<usize>,
) -> PyResult<(LlmClient, Vec<pie::llm::Message>)> {
    let mut core_config = match config {
        Some(c) => c.inner.clone(),
        None => pie::config::Config::load(None).map_err(crate::config_error)?,
    };
    if let Some(model) = model {
        core_config.model = model.to_string();
    }
    if let Some(max_tokens) = max_tokens {
        core_config.reserved_tokens = Some(max_tokens);
    }
    let client = LlmClient::new(&core_config).map_err(|e| llm_error(py, e))?;
    let mut messages = Vec::new();
    if let Some(system) = system {
        messages.push(pie::llm::Message::system(system));
    }
    messages.push(pie::llm::Message::user(prompt));
    Ok((client, messages))
}

/// 一次性问一句（**同步**，返回 assistant 正文）。
///
/// 内部就是 [`PyLlmClient::complete`] 那条路（进程级 runtime + 重试 + 期间释放 GIL），
/// 只是替你把消息拼好、把 `content` 取出来 —— 「拿模型当纯函数用」的最短写法。
/// 要 `usage` / `tool_calls` 等原始字段用 `pie.LlmClient.complete`；要工具循环 / 事件 / 流式用 `pie.Session`。
///
/// `config=None` → 读 `~/.pie/config.toml`；`model` / `max_tokens` 是按次覆盖；
/// `reasoning_effort` / `response_format` 口径同 `Session.aturn`。
#[pyfunction]
#[pyo3(signature = (prompt, *, system=None, model=None, config=None, max_tokens=None, reasoning_effort=None, response_format=None))]
#[allow(clippy::too_many_arguments)] // 与 `run` / `Session.turn` 的按次旋钮对齐，参数多是故意的
pub(crate) fn llm(
    py: Python<'_>,
    prompt: String,
    system: Option<String>,
    model: Option<String>,
    config: Option<&PyConfig>,
    max_tokens: Option<usize>,
    reasoning_effort: Option<String>,
    response_format: Option<String>,
) -> PyResult<String> {
    let (client, messages) = prepare(
        py,
        &prompt,
        system.as_deref(),
        model.as_deref(),
        config,
        max_tokens,
    )?;
    let request_options = pie::llm::RequestOptions::ChatCompletions {
        reasoning_effort: reasoning_effort.as_deref(),
        response_format: crate::parse_response_format(response_format.as_deref())?,
    };
    let got = py
        .detach(|| crate::runtime().block_on(client.complete(&messages, &[], request_options)))
        .map_err(|e| llm_error(py, e))?;
    Ok(got.content.unwrap_or_default())
}

/// `llm` 的**异步**版：同样返回正文，但返回可 await 的对象（不绕线程，跑在**进程级** runtime 上）。
///
/// ⚠ 与 `arun` / `aturn` 一样，要求调用时处于运行中的 asyncio loop（**一个进程一个 loop 最稳**）。
#[cfg(feature = "asyncio")]
#[pyfunction]
#[pyo3(signature = (prompt, *, system=None, model=None, config=None, max_tokens=None, reasoning_effort=None, response_format=None))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn allm(
    py: Python<'_>,
    prompt: String,
    system: Option<String>,
    model: Option<String>,
    config: Option<&PyConfig>,
    max_tokens: Option<usize>,
    reasoning_effort: Option<String>,
    response_format: Option<String>,
) -> PyResult<Py<PyAny>> {
    let (client, messages) = prepare(
        py,
        &prompt,
        system.as_deref(),
        model.as_deref(),
        config,
        max_tokens,
    )?;
    // PyErr 不能穿过 async 块 → `response_format` 在持 GIL 这层先解析掉
    let response_format = crate::parse_response_format(response_format.as_deref())?;
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let request_options = pie::llm::RequestOptions::ChatCompletions {
            reasoning_effort: reasoning_effort.as_deref(),
            response_format,
        };
        let result = client.complete(&messages, &[], request_options).await;
        result
            .map(|got| got.content.unwrap_or_default())
            .map_err(|e| Python::attach(|py| llm_error(py, e)))
    })
    .map(|obj| obj.unbind())
}
