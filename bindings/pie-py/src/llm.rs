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
    /// 返回 `{"content", "reasoning_content", "tool_calls", "usage"}`——字段名与纯 Python 版
    /// `LLMResult` 一致。适合「拿模型当纯函数用」的场景（如生成评测清单），不要拿它跑回合
    /// （工具循环请用 `Session.aturn`）。
    ///
    /// ⚠ 阻塞（内部含重试），期间释放 GIL。
    #[pyo3(signature = (messages, tools=None))]
    fn complete(
        &self,
        py: Python<'_>,
        messages: &Bound<'_, PyAny>,
        tools: Option<&Bound<'_, PyAny>>,
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
        let got = py
            .detach(|| crate::runtime().block_on(inner.complete(&msgs, &specs)))
            .map_err(|e| llm_error(py, e))?;
        // `LlmResult` 没有 Serialize → 手拼（字段名与 Python 版 `LLMResult` 对齐）
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
