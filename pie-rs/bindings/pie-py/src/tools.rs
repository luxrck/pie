//! `ToolRegistry`：发给模型的工具集。
//!
//! M1 只有内置四件套（read / edit / write / shell）；**从 Python 注册工具**在 M3
//! （要给核心的 `Entry` 加一个动态变体，见规划 §5.5）。

use std::collections::HashMap;

use pyo3::prelude::*;

use pie_rs::tools::{self, ToolRegistry};

use crate::config::PyConfig;

#[pyclass(name = "ToolRegistry", module = "pie_rs")]
pub struct PyToolRegistry {
    pub inner: ToolRegistry,
}

#[pymethods]
impl PyToolRegistry {
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

    fn __repr__(&self) -> String {
        format!("<pie_rs.ToolRegistry {:?}>", self.inner.names())
    }
}
