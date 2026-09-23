//! `Config`：配置快照。
//!
//! 形态取舍（见规划 §3）：**不逐字段做 pyclass**（Rust 结构体一改就得跟着改），
//! 常用项给 getter/setter，其余走 `to_dict()` / `update(dict)`（字段列表复用核心的 `to_toml()`）。
//! 改字段与 CLI 的覆盖项同语义：**只改内存，不写盘**（要写盘显式调 `save()`）。

use std::path::Path;

use pyo3::prelude::*;
use pyo3::types::PyDict;
use serde_json::Value;

use pie::config::{self, Config};

use crate::config_error;

#[pyclass(name = "Config", module = "pie")]
pub struct PyConfig {
    pub inner: Config,
}

impl PyConfig {
    pub(crate) fn wrap(inner: Config) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyConfig {
    /// 默认配置（不读文件；要读 `~/.pie/config.toml` 用 `Config.load()`）。
    #[new]
    fn new() -> Self {
        Self::wrap(Config::default())
    }

    /// 从配置文件加载（`path=None` → `~/.pie/config.toml`；与 CLI 的 `-c` 同一套解析）。
    ///
    /// 顺带写一次全局记忆种子文件（`~/.pie/memory.md`，已存在则不动）——与 CLI 每次启动同款。
    #[staticmethod]
    #[pyo3(signature = (path=None))]
    fn load(path: Option<&str>) -> PyResult<Self> {
        config::ensure_global_memory();
        Config::load(path.map(Path::new))
            .map(Self::wrap)
            .map_err(config_error)
    }

    // ------------------------------------------------------------ 常用字段

    #[getter]
    fn model(&self) -> &str {
        &self.inner.model
    }
    #[setter]
    fn set_model(&mut self, value: String) {
        self.inner.model = value;
    }

    #[getter]
    fn base_url(&self) -> &str {
        &self.inner.base_url
    }
    #[setter]
    fn set_base_url(&mut self, value: String) {
        self.inner.base_url = value;
    }

    #[getter]
    fn api_key(&self) -> &str {
        &self.inner.api_key
    }
    #[setter]
    fn set_api_key(&mut self, value: String) {
        self.inner.api_key = value;
    }

    /// 思考深度：`none` / `low` / `high` / `max`。
    #[getter]
    fn reasoning_effort(&self) -> &str {
        &self.inner.reasoning_effort
    }
    #[setter]
    fn set_reasoning_effort(&mut self, value: String) {
        self.inner.reasoning_effort = value;
    }

    #[getter]
    fn context_window(&self) -> usize {
        self.inner.context_window
    }
    #[setter]
    fn set_context_window(&mut self, value: usize) {
        self.inner.context_window = value;
    }

    /// 每次请求为输出预留的 token（即 API 的 `max_tokens`）；`None` = 不发、用服务端默认。
    #[getter]
    fn reserved_tokens(&self) -> Option<usize> {
        self.inner.reserved_tokens
    }
    #[setter]
    fn set_reserved_tokens(&mut self, value: Option<usize>) {
        self.inner.reserved_tokens = value;
    }

    #[getter]
    fn timeout_seconds(&self) -> f64 {
        self.inner.timeout_seconds
    }
    #[setter]
    fn set_timeout_seconds(&mut self, value: f64) {
        self.inner.timeout_seconds = value;
    }

    #[getter]
    fn max_retries(&self) -> usize {
        self.inner.max_retries
    }
    #[setter]
    fn set_max_retries(&mut self, value: usize) {
        self.inner.max_retries = value;
    }

    #[getter]
    fn keep_last_steps(&self) -> usize {
        self.inner.keep_last_steps
    }
    #[setter]
    fn set_keep_last_steps(&mut self, value: usize) {
        self.inner.keep_last_steps = value;
    }

    #[getter]
    fn auto_compact_threshold(&self) -> Option<usize> {
        self.inner.auto_compact_threshold
    }
    #[setter]
    fn set_auto_compact_threshold(&mut self, value: Option<usize>) {
        self.inner.auto_compact_threshold = value;
    }

    /// 是否开启上下文压缩（`True` → 三级全开，与配置里写了 `[compaction]` 等价）。
    ///
    /// ⚠ **核心默认就是开着的**（`Config::default()` 给的是 `Some(CompactionConfig::default())`，
    /// 所以配置里没写 `[compaction]` 也是三级压）。嵌入方不想让历史被改写就设 `False`。
    #[getter]
    fn compaction(&self) -> bool {
        self.inner.compaction.is_some()
    }
    #[setter]
    fn set_compaction(&mut self, value: bool) {
        self.inner.compaction = if value {
            Some(config::CompactionConfig::default())
        } else {
            None
        };
    }

    /// 本次配置的来源文件；`None` = 用的是默认路径（还没落盘过）。
    #[setter]
    fn set_config_file(&mut self, value: Option<&str>) {
        self.inner.config_file = value.map(std::path::PathBuf::from);
    }
    #[getter]
    fn config_file(&self) -> Option<String> {
        self.inner
            .config_file
            .as_ref()
            .map(|p| p.display().to_string())
    }

    /// 运行时追加的 system prompt（CLI 的 `--append-system-prompt` 同款；只在这一份配置里生效）。
    #[getter]
    fn append_system_prompt(&self) -> Vec<String> {
        self.inner.append_system_prompt.clone()
    }
    #[setter]
    fn set_append_system_prompt(&mut self, value: Vec<String>) {
        self.inner.append_system_prompt = value;
    }

    // ------------------------------------------------------------ 方法

    /// 可用输入预算 = `context_window` - `reserved_tokens`（压缩水位相对它算）。
    fn context_budget(&self) -> usize {
        self.inner.context_budget()
    }

    fn soft_limit(&self) -> usize {
        self.inner.soft_limit()
    }

    fn target_limit(&self) -> usize {
        self.inner.target_limit()
    }

    /// 工具默认私有参数（`[tools.<name>]` 段），如 `{"read": {"_max_lines": 200}}`。
    fn tool_defaults(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let dict = PyDict::new(py);
        for (name, table) in self.inner.tool_defaults() {
            let value = serde_json::to_value(&table).unwrap_or(serde_json::Value::Null);
            dict.set_item(name, crate::json_to_py(py, &value)?)?;
        }
        Ok(dict.into_any().unbind())
    }

    /// 写回配置文件（与 CLI 的 `Config::save` 同一套：`reserved_tokens=None` → `"auto"` 等）。
    fn save(&self) -> PyResult<String> {
        self.inner
            .save()
            .map(|p| p.display().to_string())
            .map_err(config_error)
    }

    /// 是否打调试日志（`[tNsM] 工具…` 那些；一次性模式恒关）。
    #[getter]
    fn verbose(&self) -> bool {
        self.inner.verbose
    }
    #[setter]
    fn set_verbose(&mut self, value: bool) {
        self.inner.verbose = value;
    }

    /// 替换基础 system prompt（运行时属性，**不落盘**）；`None` = 用内置/仓库里的那份。
    ///
    /// 想给某次会话换提示词：`cfg.system_prompt = "…"` 再 `Session.ephemeral(cfg, …)`。
    #[getter]
    fn system_prompt(&self) -> Option<String> {
        self.inner.system_prompt.clone()
    }
    #[setter]
    fn set_system_prompt(&mut self, value: Option<String>) {
        self.inner.system_prompt = value;
    }

    /// 同一批 `tool_calls` 是否并发执行（工具共享可变状态时必须设 False）。
    #[getter]
    fn parallel_tools(&self) -> bool {
        self.inner.parallel_tools
    }
    #[setter]
    fn set_parallel_tools(&mut self, value: bool) {
        self.inner.parallel_tools = value;
    }

    /// 配置 → `dict`（键名与 TOML 一致；`reserved_tokens=None` → `None`、`compaction=None` → `False`）。
    ///
    /// **只含持久字段**（`#[serde(skip)]` 的运行时字段不在里面），基准是核心的 `Config::to_toml()`
    /// —— 字段列表只有那一份，两边不会漂。
    fn to_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::to_py(py, &self.inner.to_toml())
    }

    /// 用 `dict` 覆盖配置：**只覆盖给出的键**，其余保持原值；键名写错直接报错。
    ///
    /// 走「当前值 → 打补丁 → 再 deserialize 一遍」，所以归一/校验与读配置文件完全同一条路
    ///（`reserved_tokens="64k"` 能用、`compaction=False` 能关、负数会被拒）。
    /// ⚠ 运行时字段（`config_file` / `system_prompt` / …）不在 `to_dict()` 里，也不会被这里改掉。
    fn update(&mut self, data: &Bound<'_, PyAny>) -> PyResult<()> {
        let Value::Object(patch) = crate::py_to_json(data)? else {
            return Err(crate::pie_error("update() 需要一个 dict"));
        };
        let mut merged = serde_json::to_value(self.inner.to_toml()).map_err(crate::pie_error)?;
        let Value::Object(base) = &mut merged else {
            return Err(crate::pie_error("配置序列化失败"));
        };
        for key in patch.keys() {
            if !base.contains_key(key) {
                return Err(crate::pie_error(format!(
                    "未知配置键：{key}（可用：{}）",
                    base.keys().cloned().collect::<Vec<_>>().join(", ")
                )));
            }
        }
        for (key, value) in patch {
            base.insert(key, value);
        }
        // 运行时字段原样保留（它们不在 to_toml() 里，重新 deserialize 会丢）
        let config_file = self.inner.config_file.clone();
        let auto_compact_threshold = self.inner.auto_compact_threshold;
        let system_prompt = self.inner.system_prompt.clone();
        let append_system_prompt = self.inner.append_system_prompt.clone();

        let mut updated: pie::config::Config = serde_json::from_value(merged)
            .map_err(|e| crate::pie_error(format!("配置更新失败: {e}")))?;
        updated.config_file = config_file;
        updated.auto_compact_threshold = auto_compact_threshold;
        updated.system_prompt = system_prompt;
        updated.append_system_prompt = append_system_prompt;
        self.inner = updated;
        Ok(())
    }

    fn __repr__(&self) -> String {
        // api_key 一律打码：默认配置里的 key 是本部署的真实 key，别让 repr 泄露到日志里。
        // （按**字符**取头尾，不用字节切片：key 里万ー有非 ASCII 就会 panic 在切字符边界上。）
        let key = &self.inner.api_key;
        let masked = if key.chars().count() > 8 {
            let head: String = key.chars().take(4).collect();
            let mut tail: Vec<char> = key.chars().rev().take(4).collect();
            tail.reverse();
            format!("{head}…{}", tail.into_iter().collect::<String>())
        } else {
            "…".to_string()
        };
        format!(
            "<pie.Config model={} base_url={} api_key={} context_window={} budget={}>",
            self.inner.model,
            self.inner.base_url,
            masked,
            self.inner.context_window,
            self.inner.context_budget()
        )
    }
}
