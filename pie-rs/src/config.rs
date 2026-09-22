//! 配置层：从 `~/.pie/config.toml` 读取 + 组装分层 system prompt。
//!
//! 对齐 Python 版 `src/pie/config.py`：同样的默认值、同样的文件名、同样的
//! 「从 cwd 向上找项目根」提示词解析规则。差异都写在注释里。
//!
//! 迁移取舍：Python 那版 load() 里有一大堆旧键迁移（max_tokens→reserved_tokens、
//! compress_tools→compaction、compaction = true/false …）。这里只保留**当前**的配置形状
//! 加少量仍在用的容错，历史迁移链不再背——重构的目标是活在当下，不是兼容三年前。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};

// ---------------------------------------------------------------- 默认值

pub const DEFAULT_MODEL: &str = "deepseek-flash";
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com/";
/// 本部署内置的默认 key（与 Python 版一致）。空串表示"用户没配"。
pub const DEFAULT_API_KEY: &str = "<API_KEY>";
pub const DEFAULT_REASONING_EFFORT: &str = "high";

/// 思考深度合法值（`/reasoning` 与配置 `reasoning_effort`）。
pub const REASONING_LEVELS: [&str; 4] = ["none", "low", "high", "max"];
/// `none` = 关闭思考：不发 `reasoning_effort`，改发 `thinking: {type: disabled}`。
pub const REASONING_NONE: &str = "none";

pub const DEFAULT_CONTEXT_WINDOW: i64 = 1024 * 1024;
pub const DEFAULT_KEEP_LAST_STEPS: usize = 7;
pub const DEFAULT_SOFT_RATIO: f64 = 0.8;
pub const DEFAULT_TARGET_RATIO: f64 = 0.55;

/// 每次请求为输出预留的 token（= 发给 API 的 `max_tokens`）。`None` = 不发、用服务端默认。
pub const DEFAULT_RESERVED_TOKENS: i64 = 128_000;
const RESERVED_TOKENS_AUTO: [&str; 5] = ["auto", "default", "none", "0", ""];

pub const DEFAULT_FILES_API: bool = true;
pub const DEFAULT_FILES_TTL_DAYS: i64 = 30;

/// read 图片的字节上限：内联受单图 32 MiB 限制；开了 Files API 后放宽到 64 MiB。
pub const IMAGE_MAX_BYTES_INLINE: i64 = 32 * 1024 * 1024;
pub const IMAGE_MAX_BYTES_FILES: i64 = 64 * 1024 * 1024;

/// 提示词文件名（位置不再可配）。
pub const SYSTEM_FILE: &str = "SYSTEM.md";
pub const AGENTS_FILE: &str = "AGENTS.md";
pub const MEMORY_FILE: &str = "MEMORY.md";

/// 内置兜底 system prompt：与 Python 版 `config.SYSTEM_PROMPT` 常量**逐字一致**（用 ast 抽出来比过），
/// 编译期嵌入 → 仓库根没有 `SYSTEM.md` 时也能单文件跑（本仓根就没有那个文件）。
///
/// 正文文件名小写（`prompts/system.md`）与 `prompts/memory.md` 保持一致；但**运行时找的那个
/// 文件名仍是 `SYSTEM_FILE = "SYSTEM.md"`**（从 cwd 往上的仓库根，与 Python 版共用同一套）。
pub const DEFAULT_SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

// ---------------------------------------------------------------- 路径

/// 家目录：HOME → USERPROFILE → 当前目录。
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 测试专用：把进程级 `PIE_DIR` 改来改去的用例共用这一把锁（跨模块串行化，
/// 否则 `context` / `llm` 两边的测试并行跑会互相覆盖环境变量）。
#[cfg(test)]
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 数据根目录：`PIE_DIR` 环境变量可重定向（测试/多环境），默认 `~/.pie`。
pub fn pie_dir() -> PathBuf {
    match std::env::var_os("PIE_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home_dir().join(".pie"),
    }
}

pub fn default_config_file() -> PathBuf {
    pie_dir().join("config.toml")
}

// ---------------------------------------------------------------- 时间（没日期库，自己换算）

/// 当前 unix 秒。
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// unix 秒 → `YYYY-MM-DDTHH:MM:SS`（UTC）。
///
/// 用 Howard Hinnant 的 civil_from_days（不引日期库）；`context` 的 manifest 时间戳、
/// `files list` 显示的时间都走这一份，不开二份换算。
pub fn iso_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// unix 秒 → `YYYY-MM-DD HH:MM`（**UTC**）。
///
/// ⚠ Python 版这里用本地时间（`datetime.fromtimestamp`）；Rust 没有时区库，统一按 UTC
/// 显示——只是给人看的时间戳，不参与任何判断。
pub fn fmt_unix_ts(secs: i64) -> String {
    let iso = iso_utc(secs);
    format!("{} {}", &iso[..10], &iso[11..16])
}

/// 千分位（`1234567` → `1,234,567`）：Rust 的 format 没有 Python 的 `{:,}`，只能自己加。
pub fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

pub fn global_memory_file() -> PathBuf {
    pie_dir().join("memory.md")
}

/// 全局记忆的**种子文件**内容（首跑写一份，之后不再动）——与 Python 版
/// `config.GLOBAL_MEMORY_TEMPLATE` **逐字一致**：两边共用一个 `~/.pie/memory.md`，
/// 骨架不一样就白搭。
///
/// 与 `DEFAULT_SYSTEM_PROMPT` 同一套做法：正文放在 `prompts/memory.md`，`include_str!` 编进来
/// —— 模板里全是中文与骨架，写成转义串难读也容易碰格式。
pub const DEFAULT_GLOBAL_MEMORY: &str = include_str!("../prompts/memory.md");

/// 首次运行创建全局记忆的种子文件（已存在则跳过，**不覆盖**）——对齐 Python
/// `config._ensure_global_memory()`（它在 `ensure_config()` 里调，即每次启动）。
///
/// 写不了就算了（权限/只读家目录）：这只是个种子，不值得挡住启动。
pub fn ensure_global_memory() {
    let path = global_memory_file();
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, DEFAULT_GLOBAL_MEMORY);
}

/// 配置路径：显式 `-c` > `PIE_CONFIG_FILE` 环境变量 > 默认 `~/.pie/config.toml`。
pub fn resolve_config_file(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Some(v) = std::env::var_os("PIE_CONFIG_FILE") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    default_config_file()
}

// ---------------------------------------------------------------- 压缩配置

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ToolCompaction {
    pub head: usize,
    pub tail: usize,
}

impl Default for ToolCompaction {
    fn default() -> Self {
        Self { head: 30, tail: 50 }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct SessionCompaction {
    pub head: usize,
    pub tail: usize,
}

impl Default for SessionCompaction {
    fn default() -> Self {
        Self { head: 3, tail: 5 }
    }
}

/// `true`/`false` 或一张子表 —— 兼容 `tool = false` 这种「显式关闭某级」的写法。
#[derive(Deserialize)]
#[serde(untagged)]
enum TableOrBool<T> {
    Bool(bool),
    Table(T),
}

/// 子表/布尔 → `Option<子表>`：`false` 或缺失即「关闭这一级」。
fn de_level<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(match TableOrBool::<T>::deserialize(d)? {
        TableOrBool::Bool(false) => None,
        TableOrBool::Bool(true) => None, // 只写 `xx = true` 无参数：按关闭处理（Python 版里也无参数可给）
        TableOrBool::Table(t) => Some(t),
    })
}

/// 三级压缩的总配置。写了 `[compaction]` 即开启；某一级为 `false` 表示关闭该级；
/// 整个 compaction 为 `false` 表示不做任何压缩。
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct CompactionConfig {
    #[serde(deserialize_with = "de_level")]
    pub tool: Option<ToolCompaction>,
    /// 轮次级（level 2）：摘要只保留用户输入 + 模型最终输出。
    pub turn: bool,
    #[serde(deserialize_with = "de_level")]
    pub session: Option<SessionCompaction>,
    /// 软阈值比例（相对可用输入预算），触发自动压缩。
    pub soft_ratio: f64,
    /// 压缩后的目标水位（迟滞防抖，应小于 soft_ratio）。
    pub target_ratio: f64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            tool: Some(ToolCompaction::default()),
            turn: true,
            session: Some(SessionCompaction::default()),
            soft_ratio: DEFAULT_SOFT_RATIO,
            target_ratio: DEFAULT_TARGET_RATIO,
        }
    }
}

/// `[tui]` 段（TUI 还没移植，先把配置形状收着，保证老配置文件能读进来）。
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct TuiConfig {
    pub lean: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self { lean: true }
    }
}

// ---------------------------------------------------------------- Config

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub reasoning_effort: String,
    #[serde(deserialize_with = "de_reserved_tokens")]
    pub reserved_tokens: Option<i64>,
    pub context_window: i64,
    pub keep_last_steps: usize,
    /// 单回合最多问几次模型（不写 = 不限）。
    ///
    /// Python 版这里是 `loop.aturn(max_steps=…)` 的形参（由嵌入方按次传），Rust 收进配置：
    /// 要按次覆盖，调用前改一下 `cfg.max_steps` 就行（等价，且少一个形参）。
    pub max_steps: Option<usize>,
    /// 是否流式请求（`false` = 一次性 `complete`，`on_event` 不再收到增量）。同上：Python 是形参。
    pub stream: bool,
    /// `None` = 不做任何上下文压缩。
    #[serde(deserialize_with = "de_compaction")]
    pub compaction: Option<CompactionConfig>,
    pub timeout_seconds: f64,
    pub max_retries: u32,
    pub max_retry_delay_seconds: f64,
    pub verbose: bool,
    pub theme: String,
    /// 按工具名设默认私有参数（下划线开头，不进 schema）。
    pub tools: HashMap<String, toml::Value>,
    pub tui: TuiConfig,
    pub files_api: bool,
    pub files_ttl_days: i64,
    /// 同一批 tool_calls 是否并发执行。
    pub parallel_tools: bool,

    /// 运行时属性（不落盘）：配置来源路径。
    #[serde(skip)]
    pub config_file: Option<PathBuf>,
    /// 运行时属性（不落盘）：CLI `--auto-compact-threshold` 一次性覆盖软阈值。
    #[serde(skip)]
    pub auto_compact_threshold: Option<i64>,
    /// 运行时属性（不落盘）：CLI `--system-prompt`（替换基础提示：文本或文件内容）。
    #[serde(skip)]
    pub system_prompt: Option<String>,
    /// 运行时属性（不落盘）：CLI `--append-system-prompt`（可重复）。
    #[serde(skip)]
    pub append_system_prompt: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: DEFAULT_API_KEY.to_string(),
            reasoning_effort: DEFAULT_REASONING_EFFORT.to_string(),
            reserved_tokens: Some(DEFAULT_RESERVED_TOKENS),
            context_window: DEFAULT_CONTEXT_WINDOW,
            keep_last_steps: DEFAULT_KEEP_LAST_STEPS,
            max_steps: None,
            stream: true,
            compaction: Some(CompactionConfig::default()),
            timeout_seconds: 60.0,
            max_retries: 5,
            max_retry_delay_seconds: 3.0,
            verbose: true,
            theme: "catppuccin".to_string(),
            tools: HashMap::new(),
            tui: TuiConfig::default(),
            files_api: DEFAULT_FILES_API,
            files_ttl_days: DEFAULT_FILES_TTL_DAYS,
            parallel_tools: true,
            config_file: None,
            auto_compact_threshold: None,
            system_prompt: None,
            append_system_prompt: Vec::new(),
        }
    }
}

impl Config {
    /// 从配置文件加载。文件不存在 → 全默认值（Python 版这里会走交互式向导，CLI 层负责）。
    pub fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let path = resolve_config_file(explicit);
        let mut cfg = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| ConfigError::Io(path.clone(), e.to_string()))?;
            toml::from_str::<Config>(&text)
                .map_err(|e| ConfigError::Parse(path.clone(), e.to_string()))?
        } else {
            Config::default()
        };
        cfg.config_file = Some(path);
        Ok(cfg)
    }

    /// 可用输入预算：服务端按「输入 tokens + max_tokens ≤ 窗口」判超限。
    pub fn context_budget(&self) -> i64 {
        (self.context_window - self.reserved_tokens.unwrap_or(0)).max(1)
    }

    fn ratio(&self, soft: bool) -> f64 {
        match &self.compaction {
            Some(c) if soft => c.soft_ratio,
            Some(c) => c.target_ratio,
            None => {
                if soft {
                    DEFAULT_SOFT_RATIO
                } else {
                    DEFAULT_TARGET_RATIO
                }
            }
        }
    }

    /// 软阈值：触发自动压缩的 token 水位。
    /// 软阈值比例（未配置 `[compaction]` 时给默认值）——`/stat` 展示用。
    pub fn soft_ratio(&self) -> f64 {
        self.ratio(true)
    }

    /// 目标水位比例（同上）。
    pub fn target_ratio(&self) -> f64 {
        self.ratio(false)
    }

    pub fn soft_limit(&self) -> i64 {
        // CLI 一次性覆盖（`--auto-compact-threshold`）优先——与 Python 的 `maybe_compact` 同语义
        if let Some(n) = self.auto_compact_threshold {
            return n.max(1);
        }
        ((self.context_budget() as f64) * self.ratio(true)).max(1.0) as i64
    }

    /// 目标水位：压缩后应降到该值以下。
    pub fn target_limit(&self) -> i64 {
        ((self.context_budget() as f64) * self.ratio(false)).max(1.0) as i64
    }

    /// 工具私有默认参数（下划线开头，由 dispatch 注入）。
    /// 派生默认：`read` 的 `_max_image_bytes`（内联 32 MiB / Files API 64 MiB）。
    pub fn tool_defaults(&self) -> HashMap<String, toml::Table> {
        let mut out: HashMap<String, toml::Table> = self
            .tools
            .iter()
            .filter_map(|(k, v)| v.as_table().cloned().map(|t| (k.clone(), t)))
            .collect();
        let image_cap = if self.files_api {
            IMAGE_MAX_BYTES_FILES
        } else {
            IMAGE_MAX_BYTES_INLINE
        };
        out.entry("read".to_string())
            .or_default()
            .entry("_max_image_bytes".to_string())
            .or_insert(toml::Value::Integer(image_cap));
        out
    }

    /// 归一化后的思考深度：`none` → `None`（不发送 `reasoning_effort`）。
    pub fn normalized_reasoning_effort(&self) -> Option<&str> {
        let e = self.reasoning_effort.trim();
        if e.is_empty() || e == REASONING_NONE {
            None
        } else {
            Some(e)
        }
    }

    /// 写回配置文件（`config_file`，默认 `~/.pie/config.toml`）——`/model`、`/thinking` 用。
    ///
    /// ⚠ 入口目前只有交互层（与 `Session::set_model` 一起）；一次性 CLI 不写配置。
    /// 两个 TOML 没有的东西要转写（与 `Deserialize` 侧对称）：
    ///   - `reserved_tokens = None` 写成 `"auto"`（TOML 没有 null）；
    ///   - `compaction = None` 写成 `false`，某一级关掉写成 `tool = false`；
    ///
    /// 运行时字段（`config_file`）不写进去。    #[allow(dead_code)]
    pub fn save(&self) -> Result<PathBuf, ConfigError> {
        let path = self.config_file.clone().unwrap_or_else(default_config_file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ConfigError::Io(parent.to_path_buf(), e.to_string()))?;
        }
        let text = toml::to_string(&self.to_toml())
            .map_err(|e| ConfigError::Parse(path.clone(), e.to_string()))?;
        std::fs::write(&path, text).map_err(|e| ConfigError::Io(path.clone(), e.to_string()))?;
        Ok(path)
    }

    /// `Config` → TOML 值（形状与配置文件一致）。
    fn to_toml(&self) -> toml::Value {
        use toml::Value as V;
        let mut t = toml::map::Map::new();
        let mut put = |key: &str, value: V| {
            t.insert(key.to_string(), value);
        };
        put("model", V::String(self.model.clone()));
        put("base_url", V::String(self.base_url.clone()));
        put("api_key", V::String(self.api_key.clone()));
        put("reasoning_effort", V::String(self.reasoning_effort.clone()));
        put(
            "reserved_tokens",
            match self.reserved_tokens {
                Some(n) => V::Integer(n),
                None => V::String("auto".to_string()),
            },
        );
        put("context_window", V::Integer(self.context_window));
        put("keep_last_steps", V::Integer(self.keep_last_steps as i64));
        if let Some(max) = self.max_steps {
            put("max_steps", V::Integer(max as i64));
        }
        put("stream", V::Boolean(self.stream));
        put(
            "compaction",
            match &self.compaction {
                Some(c) => {
                    let mut comp = toml::map::Map::new();
                    comp.insert(
                        "tool".to_string(),
                        match &c.tool {
                            Some(tool) => {
                                let mut m = toml::map::Map::new();
                                m.insert("head".to_string(), V::Integer(tool.head as i64));
                                m.insert("tail".to_string(), V::Integer(tool.tail as i64));
                                V::Table(m)
                            }
                            None => V::Boolean(false),
                        },
                    );
                    comp.insert("turn".to_string(), V::Boolean(c.turn));
                    comp.insert(
                        "session".to_string(),
                        match &c.session {
                            Some(s) => {
                                let mut m = toml::map::Map::new();
                                m.insert("head".to_string(), V::Integer(s.head as i64));
                                m.insert("tail".to_string(), V::Integer(s.tail as i64));
                                V::Table(m)
                            }
                            None => V::Boolean(false),
                        },
                    );
                    comp.insert("soft_ratio".to_string(), V::Float(c.soft_ratio));
                    comp.insert("target_ratio".to_string(), V::Float(c.target_ratio));
                    V::Table(comp)
                }
                None => V::Boolean(false),
            },
        );
        put("timeout_seconds", V::Float(self.timeout_seconds));
        put("max_retries", V::Integer(self.max_retries as i64));
        put(
            "max_retry_delay_seconds",
            V::Float(self.max_retry_delay_seconds),
        );
        put("verbose", V::Boolean(self.verbose));
        put("theme", V::String(self.theme.clone()));
        put(
            "tools",
            V::Table(
                self.tools
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
        );
        let mut tui = toml::map::Map::new();
        tui.insert("lean".to_string(), V::Boolean(self.tui.lean));
        put("tui", V::Table(tui));
        put("files_api", V::Boolean(self.files_api));
        put("files_ttl_days", V::Integer(self.files_ttl_days));
        put("parallel_tools", V::Boolean(self.parallel_tools));
        V::Table(t)
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(PathBuf, String),
    Parse(PathBuf, String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(p, e) => write!(f, "读取配置失败 {}: {e}", p.display()),
            ConfigError::Parse(p, e) => write!(f, "解析配置失败 {}: {e}", p.display()),
        }
    }
}

impl std::error::Error for ConfigError {}

// ---------------------------------------------------------------- serde 助手

#[derive(Deserialize)]
#[serde(untagged)]
enum ReservedRaw {
    Int(i64),
    Str(String),
}

fn default_reserved_tokens() -> Option<i64> {
    Some(DEFAULT_RESERVED_TOKENS)
}

/// `reserved_tokens` 容错：允许 `"auto"` / `"64k"` / `384K` 这类手写值；
/// 0/负数按「不发送 max_tokens」处理。
fn de_reserved_tokens<'de, D>(d: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let _ = default_reserved_tokens;
    Ok(match ReservedRaw::deserialize(d)? {
        ReservedRaw::Int(n) => {
            if n < 1 {
                None
            } else {
                Some(n)
            }
        }
        ReservedRaw::Str(s) => parse_reserved_tokens(&s).ok().flatten(),
    })
}

/// 解析 `reserved_tokens` 输入：auto/default/none/0/空 → `None`；支持 `64k` / `384K` 简写。
pub fn parse_reserved_tokens(raw: &str) -> Result<Option<i64>, String> {
    let text = raw.trim().to_ascii_lowercase();
    if RESERVED_TOKENS_AUTO.contains(&text.as_str()) {
        return Ok(None);
    }
    let (multiplier, digits) = if let Some(rest) = text.strip_suffix('k') {
        (1000.0, rest)
    } else if let Some(rest) = text.strip_suffix('m') {
        (1_000_000.0, rest)
    } else {
        (1.0, text.as_str())
    };
    let value = digits
        .parse::<f64>()
        .map(|v| (v * multiplier) as i64)
        .map_err(|_| format!("无法识别的 token 数: {raw:?}（例：65536 / 64k / auto）"))?;
    if value < 1 {
        return Err("reserved_tokens 必须 ≥ 1（要恢复默认请用 auto）".to_string());
    }
    Ok(Some(value))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CompactionRaw {
    Off(bool),
    Table(CompactionConfig),
}

fn default_compaction() -> Option<CompactionConfig> {
    Some(CompactionConfig::default())
}

/// `[compaction]` 可以为 `false`（整体关闭）或一张表；不写 = 默认三级全开。
fn de_compaction<'de, D>(d: D) -> Result<Option<CompactionConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    let _ = default_compaction;
    Ok(match CompactionRaw::deserialize(d)? {
        CompactionRaw::Off(false) => None,
        CompactionRaw::Off(true) => Some(CompactionConfig::default()),
        CompactionRaw::Table(c) => Some(c),
    })
}

// ---------------------------------------------------------------- system prompt

/// 从 cwd 向上找最近的「项目根」：含任一提示词文件或 `.git` 的目录。
pub fn find_project_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    for dir in cwd.ancestors() {
        if [AGENTS_FILE, MEMORY_FILE, SYSTEM_FILE, ".git"]
            .iter()
            .any(|m| dir.join(m).exists())
        {
            return dir.to_path_buf();
        }
    }
    cwd
}

/// 解析提示词文件：绝对路径直接用；相对路径先找项目根，再退回 cwd。
pub fn resolve_prompt_file(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() {
        return p.exists().then(|| p.to_path_buf());
    }
    let root = find_project_root();
    for candidate in [root.join(p), PathBuf::from(".").join(p)] {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn read_text(path: Option<PathBuf>) -> String {
    path.and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
}

/// 按分层组装 system prompt：SYSTEM.md（角色）+ AGENTS.md（项目）+ 记忆（存在即加载）。
///
/// `system_prompt` 非空时替换 SYSTEM.md 基础提示；`append_system_prompt` 追加到最末。
pub fn build_system_prompt(
    config: &Config,
    system_prompt: Option<&str>,
    append_system_prompt: &[String],
) -> String {
    let _ = config; // 预留：Python 版这里会拼一行上下文预算说明（当前被注释掉）
    let base = match system_prompt {
        Some(s) => s.to_string(),
        None => read_text(resolve_prompt_file(SYSTEM_FILE)),
    };
    let base = if base.is_empty() {
        DEFAULT_SYSTEM_PROMPT.to_string()
    } else {
        base
    };

    let mut parts = vec![base];

    let agents = read_text(resolve_prompt_file(AGENTS_FILE));
    if !agents.is_empty() {
        parts.push(agents);
    }

    let mut memories: Vec<String> = Vec::new();
    let global_memory = std::fs::read_to_string(global_memory_file()).unwrap_or_default();
    if !global_memory.is_empty() {
        memories.push(format!(
            "## 全局记忆（{}）\n{global_memory}",
            global_memory_file().display()
        ));
    }
    let project_memory = read_text(resolve_prompt_file(MEMORY_FILE));
    if !project_memory.is_empty() {
        memories.push(format!("## 项目记忆（{MEMORY_FILE}）\n{project_memory}"));
    }
    if !memories.is_empty() {
        parts.push(memories.join("\n\n"));
    }

    parts.extend(append_system_prompt.iter().filter(|s| !s.is_empty()).cloned());
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reserved_tokens_variants() {
        assert_eq!(parse_reserved_tokens("auto").unwrap(), None);
        assert_eq!(parse_reserved_tokens("").unwrap(), None);
        assert_eq!(parse_reserved_tokens("0").unwrap(), None);
        assert_eq!(parse_reserved_tokens("64k").unwrap(), Some(64_000));
        assert_eq!(parse_reserved_tokens("384K").unwrap(), Some(384_000));
        assert_eq!(parse_reserved_tokens("65536").unwrap(), Some(65_536));
        assert!(parse_reserved_tokens("abc").is_err());
    }

    #[test]
    fn budget_and_limits() {
        let cfg = Config::default();
        assert_eq!(
            cfg.context_budget(),
            DEFAULT_CONTEXT_WINDOW - DEFAULT_RESERVED_TOKENS
        );
        assert_eq!(cfg.soft_limit(), (cfg.context_budget() as f64 * 0.8) as i64);
        assert_eq!(
            cfg.target_limit(),
            (cfg.context_budget() as f64 * 0.55) as i64
        );
    }

    #[test]
    fn real_config_toml_loads() {
        // 用户现有的 config.toml 形状必须能读进来
        let text = r#"
model = "deepseek-flash"
base_url = "https://api.deepseek.com/"
api_key = "sk-x"
reasoning_effort = "high"
reserved_tokens = 128000
context_window = 1048576
keep_last_steps = 7
timeout_seconds = 60.0
max_retries = 5
parallel_tools = true

[compaction]
turn = true
soft_ratio = 0.8
target_ratio = 0.55

[compaction.tool]
head = 30
tail = 50

[compaction.session]
head = 3
tail = 5

[tui]
lean = true
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.model, "deepseek-flash");
        assert_eq!(cfg.reserved_tokens, Some(128_000));
        let comp = cfg.compaction.unwrap();
        assert_eq!(comp.tool.unwrap().head, 30);
        assert_eq!(comp.session.unwrap().tail, 5);
        assert!(comp.turn);
        assert!(cfg.tui.lean);
    }

    /// `save` → `load` 往返：TOML 没有 null，`reserved_tokens = None` 要写成 `"auto"` 再读回 None；
    /// `compaction` 是 `false` / 表（子级关掉写成 `tool = false`）。
    #[test]
    fn config_round_trips_through_toml() {
        let dir = std::env::temp_dir().join(format!("pie-rs-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let cfg = Config {
            model: "deepseek-v4-pro".into(),
            reserved_tokens: None, // → "auto"
            max_steps: Some(7),
            context_window: 12_345,
            stream: false,
            compaction: Some(CompactionConfig {
                tool: None, // → tool = false
                ..Default::default()
            }),
            config_file: Some(path.clone()),
            ..Default::default()
        };
        cfg.save().expect("save");

        let back = Config::load(Some(&path)).expect("load");
        assert_eq!(back.model, "deepseek-v4-pro");
        assert_eq!(back.reserved_tokens, None, "\"auto\" 要读回 None");
        assert_eq!(back.max_steps, Some(7));
        assert_eq!(back.context_window, 12_345);
        assert!(!back.stream);
        let comp = back.compaction.as_ref().expect("compaction");
        assert!(comp.tool.is_none(), "tool = false 读回 None（该级关闭）");
        assert!(comp.session.is_some(), "session 没关就还是表");
        assert_eq!(back.keep_last_steps, cfg.keep_last_steps);
        assert_eq!(back.max_retries, cfg.max_retries);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_global_memory_seeds_once_and_never_overwrites() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pie-rs-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("PIE_DIR", &dir);

        ensure_global_memory();
        let path = global_memory_file();
        let text = std::fs::read_to_string(&path).expect("种子文件已写入");
        assert!(text.starts_with("# 全局记忆"), "{text}");
        assert!(text.contains("## 编码风格／工具链"), "带分类骨架：{text}");
        assert!(text.contains("说明段落不要删除"), "{text}");

        // 已有内容绝不能被动（这是用户自己的记忆文件）
        std::fs::write(&path, "我的记忆").unwrap();
        ensure_global_memory();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "我的记忆");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seeded_global_memory_lands_in_the_system_prompt() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pie-rs-memory-prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("PIE_DIR", &dir);

        let cfg = Config::default();
        // 按**路径**判有无：项目记忆里可能碰巧引用了「## 全局记忆（…）」这个形状
        // （本仓 MEMORY.md 就写过）——拿那串当断言会自打脸。
        let marker = format!("## 全局记忆（{}）", global_memory_file().display());
        let before = build_system_prompt(&cfg, None, &[]);
        assert!(!before.contains(&marker), "没文件就不拼这块");

        ensure_global_memory();
        let after = build_system_prompt(&cfg, None, &[]);
        assert!(after.contains(&marker), "种子文件要进 system prompt：{after}");
        assert!(after.contains("跨项目的持久记忆"), "连正文一起拼进去");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_can_be_disabled() {
        let cfg: Config = toml::from_str("compaction = false").unwrap();
        assert!(cfg.compaction.is_none());
        let cfg: Config =
            toml::from_str("[compaction]\ntool = false\nturn = false\nsession = false").unwrap();
        let c = cfg.compaction.unwrap();
        assert!(c.tool.is_none() && !c.turn && c.session.is_none());
    }
}
