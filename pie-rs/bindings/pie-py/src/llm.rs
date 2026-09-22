//! `LlmClient`：模型后端（OpenAI 兼容）。
//!
//! 这里是同步外观：内部 `async fn` 交给进程级 runtime 跑，**期间释放 GIL**（§5.4）。

use pyo3::prelude::*;

use pie_rs::llm::LlmClient;

use crate::config::PyConfig;
use crate::llm_error;

#[pyclass(name = "LlmClient", module = "pie_rs")]
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
        pie_rs::llm::model_supports_files(model)
    }

    /// 列出端点可用模型 id。
    fn list_models(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        let inner = &self.inner;
        py.detach(|| crate::runtime().block_on(inner.list_models()))
            .map_err(|e| llm_error(py, e))
    }

    /// 切换思考深度（`None` / `"none"` = 关闭思考）。
    #[pyo3(signature = (level=None))]
    fn set_reasoning_effort(&mut self, level: Option<&str>) {
        self.inner.set_reasoning_effort(level);
    }

    fn __repr__(&self) -> String {
        format!("<pie_rs.LlmClient model={}>", self.inner.model)
    }
}
