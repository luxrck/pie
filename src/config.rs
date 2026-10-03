//! 配置层：从 `~/.pie/config.toml` 读取 + 组装分层 system prompt。
//!
//! 默认值、文件名、以及「从 cwd 向上找项目根」的提示词解析规则都收敛在这里。
//!
//! 迁移取舍：只保留**当前**的配置形状 + 少量仍在用的容错，不背历史键迁移链
//! （早期那版 load() 里那一大堆 max_tokens→reserved_tokens、compress_tools→compaction … 已不再维护）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------- 常量
//
// ⚠ 默认值**不在这里开 `DEFAULT_*` 常量**：类型自己有 `Default`（`impl Default for Config` /
// `CompactionConfig` / …），值就写在那儿 —— 一处定义、改一处。这里只留**跨模块要用的**东西：
// `THINKING_LEVELS`（`-t` 的合法值，TUI 的 `/thinking` 候选从它派生）、`REASONING_NONE`
//（CLI 的 `-t off` 归一）、以及提示词文件名（`find_project_root` / `resolve_prompt_file` /
// `build_system_prompt` 三处共用）。

/// 思考深度的**全部合法写法**（`-t/--thinking` 收这些）—— **唯一事实来源**：
/// TUI 的 `/thinking` 候选也从它派生（见 [`reasoning_levels`]）。
///
/// 对外写法是 `off`（内部才归一到配置口径 `none`）；`none` 也收 —— 配置文件与 `/thinking`
/// 里写的就是 `none`，顺手敲 `-t none` 也应该能用。
pub const THINKING_LEVELS: [&str; 8] = [
    "off", "none", "minimal", "low", "medium", "high", "xhigh", "max",
];

/// `THINKING_LEVELS` 里**不是配置口径档位**的那些写法：`off` 是 `none` 的人写法，
/// 其余三个是 DeepSeek 的兼容名（服务端自己把 `minimal` 映射到 `low`、`medium`/`xhigh` 映射到 `high`）。
/// 它们照样**原样透传**给服务端（见 `main.rs::apply_overrides`）—— 这里只是不列进 `/thinking` 候选。
const THINKING_ALIASES: [&str; 4] = ["off", "minimal", "medium", "xhigh"];

/// 配置口径的档位（TUI `/thinking` 的候选）= [`THINKING_LEVELS`] 去掉别名。
pub fn reasoning_levels() -> impl Iterator<Item = &'static str> {
    THINKING_LEVELS
        .iter()
        .copied()
        .filter(|level| !THINKING_ALIASES.contains(level))
}

/// `none` = 关闭思考：不发 `reasoning_effort`，改发 `thinking: {type: disabled}`。
pub const REASONING_NONE: &str = "none";

/// 思考深度归一：空串 / `none` = 关闭思考（`None`）。
///
/// 三个消费者共用（`Config::normalized_reasoning_effort`、`LlmClient` 的构造与**按次覆盖**）——
/// 别在别处再写一份。
pub fn normalize_reasoning_effort(effort: &str) -> Option<&str> {
    let e = effort.trim();
    if e.is_empty() || e == REASONING_NONE {
        None
    } else {
        Some(e)
    }
}

/// 提示词文件名（位置不再可配）。
pub const SYSTEM_FILE: &str = "SYSTEM.md";
pub const AGENTS_FILE: &str = "AGENTS.md";
pub const MEMORY_FILE: &str = "MEMORY.md";

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

/// 数据存储：一个 root + 四个子目录 + 内容寻址落盘（[`Storage::store`]）。
///
/// **唯一职责是「用户数据放哪儿、怎么落盘」**（`config` 是底层，谁都能拿来用）。默认 root 来自
/// [`Storage::from_env`]（`PIE_DIR` → 当前目录下的 `.pie` → `~/.pie`）；测试用 [`Storage::at`]
/// 注入临时 root —— 不必再动进程级 `PIE_DIR`。
#[derive(Clone, Debug)]
pub struct Storage {
    pub root: PathBuf,
}

impl Default for Storage {
    fn default() -> Self {
        Self::from_env()
    }
}

/// 落盘请求：写哪个子目录、名字怎么起，都在这层定。
///
/// 三种都**内容寻址**（名字里都是 sha256 前 16 位：同内容 ⇒ 同名字 ⇒ 已存在则不动、只落一份）；
/// 区别只在目录、前缀与权限。⚠ `Window`（`/clear` 归档）本来名字带 unix 秒「每次新建」，2026-09-29
/// 起改成内容寻址——同名 ⇒ 同内容，重复归档同一段内容不会多出文件（"什么时候清的"由会话流水的
/// `ts` 记着）。
pub enum StoreType<'a> {
    /// 图片副本 → `files/`：`img-<hash><ext>`（0o600、原子写）。
    /// **只有它留扩展名**：那是用户数据（可能被双击 / 拖出去打开），扩展名是"格式"的唯一载体。
    Blob { data: &'a [u8], mime: &'a str },
    /// 压缩落盘文本 → `context/`：`<prefix>-<hash>`（`prefix` = 工具名 / `turn` / `session`）。
    Raw { prefix: &'a str, body: &'a str },
    /// `/clear` 归档的窗口块 → `windows/`：`window-<hash>`。
    Window(&'a str),
}

impl Storage {
    /// 从环境解析默认 root：`PIE_DIR` → 当前目录下的 `.pie`（存在即用）→ `~/.pie`。
    pub fn from_env() -> Self {
        Self { root: pie_dir() }
    }

    /// 指定 root（测试注入临时目录用）。
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 会话目录：`<root>/sessions/`（会话 JSONL 的落脚处，`sessions` 子命令与 `-r` 都扫它）。
    pub fn sessions(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// 压缩落盘正文目录：`<root>/context/`（`context info|verify|gc` 的地盘）。
    pub fn context(&self) -> PathBuf {
        self.root.join("context")
    }

    /// 本地图片副本目录：`<root>/files/`（`files list|gc` 的地盘；与 `context/` 分开：那边是文本）。
    pub fn files(&self) -> PathBuf {
        self.root.join("files")
    }

    /// `/clear` 归档的窗口块目录：`<root>/windows/`。
    ///
    /// **刻意放在 `context/` 之外**：那是压缩落盘 + `context gc` 的地盘，而窗口块是用户
    /// 主动归档的原文（`/clear` 产出）——`gc` 不该碰它。
    pub fn windows(&self) -> PathBuf {
        self.root.join("windows")
    }

    /// 内容寻址落盘：`Blob` 图片副本 / `Raw` 压缩原文 / `Window` 窗口块，见 [`StoreType`]。
    ///
    /// 返回**落盘路径**——文件名主干就是 id（`img-<hash>` / `<prefix>-<hash>` / `window-<hash>`，
    /// 全是内容寻址的：同内容只落一份，已存在则不动），要 hash 段用 [`hash_of`]。
    pub fn store(&self, what: StoreType<'_>) -> std::io::Result<PathBuf> {
        /// mime → 扩展名（图片副本是用户数据，名字里带扩展名才能双击打开；白名单之外给空串）。
        fn image_ext(mime: &str) -> &'static str {
            match mime {
                "image/jpeg" => ".jpg",
                "image/png" => ".png",
                "image/gif" => ".gif",
                "image/webp" => ".webp",
                "image/bmp" => ".bmp",
                _ => "",
            }
        }

        let (dir, name, body, mode): (PathBuf, String, &[u8], Option<u32>) = match what {
            StoreType::Blob { data, mime } => (
                self.files(),
                format!("img-{}{}", hash_id(data), image_ext(mime)),
                data,
                Some(0o600),
            ),
            StoreType::Raw { prefix, body } => (
                self.context(),
                format!("{prefix}-{}", hash_id(body.as_bytes())),
                body.as_bytes(),
                None,
            ),
            StoreType::Window(body) => (
                self.windows(),
                format!("window-{}", hash_id(body.as_bytes())),
                body.as_bytes(),
                None,
            ),
        };
        let path = dir.join(name);
        self.write_atomic(&path, body, mode)?;
        Ok(path)
    }

    /// 写文件：先写**同目录**临时件再 rename（读者不会看到写了一半的文件）；已存在则不动
    /// （名字都是内容寻址的：同名 ⇒ 同内容）。`mode` 只在 unix 生效（图片副本是用户数据 → 0o600）。
    ///
    /// 目录从 `path` 推（`store` 那边已经 join 好了），所以不用单独传。
    fn write_atomic(&self, path: &Path, body: &[u8], mode: Option<u32>) -> std::io::Result<()> {
        /// 给 io 错误补上路径上下文：std 的 `io::Error` 不带路径，而上层再包一层就又成了
        /// 「每个调用点手写 message」——所以写一次，所有 `store()` 的调用者都受益。
        fn with_path(path: &Path, e: std::io::Error) -> std::io::Error {
            std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))
        }
        if path.exists() {
            return Ok(());
        }
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| with_path(dir, e))?;
        let tmp = dir.join(format!(
            ".{}.tmp-{}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        std::fs::write(&tmp, body).map_err(|e| with_path(&tmp, e))?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
        }
        #[cfg(not(unix))]
        let _ = mode;
        std::fs::rename(&tmp, path).map_err(|e| with_path(path, e))
    }
}

// ---------------------------------------------------------------- 落盘名字（id）

/// 内容 id：sha256 前 16 位十六进制——**内容寻址的唯一依据**（同内容 ⇒ 同名字 ⇒ 同一份）。
///
/// 这一处三个一队（[`Storage::store`] 的命名约定：`<前缀>-<hash>[.扩展名]`）：
///   - [`hash_id`]：**内容 → hash**（起名用）；
///   - [`name_of`]：**路径 → 文件名主干**（= id，如 `img-<hash>` / `chat-<秒>-<微秒>`）；
///   - [`hash_of`]：**路径 → hash 段**（主干里最后一段，metadata 只想要 hash 时用）。
pub fn hash_id(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())[..16].to_string()
}

/// 从落盘路径取回**文件名主干**（= id，[`Storage::store`] 命名的正解）：
/// `img-<hash>` / `<前缀>-<hash>` / `chat-<秒>-<微秒>`（扩展名不算）。
///
/// ⚠ 别拿它当 [`hash_of`] 用：主干是整个 id（带 `img-` 那截前缀），`hash_of` 才只取 hash 段。
/// 消费方：图片副本的 id（`session.rs`，与 `__meta__.files` 的键同形）、会话列表的 id（`cli.rs`）。
pub fn name_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 从落盘路径取回 hash 段（[`Storage::store`] 那套命名的**逆运算**：`<前缀>-<hash>[.扩展名]`）。
///
/// 落盘只给路径、不返回 id，要 hash 的地方（`raw_hash` 之类的元数据）就靠这个反推；
/// 消费方：`session.rs` 的压缩流水（`__meta__.compaction_events`）与窗口块元数据。
pub fn hash_of(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.rsplit('-').next())
        .unwrap_or_default()
        .to_string()
}


/// 数据根目录：`PIE_DIR`（非空才算）→ 当前目录下的 `.pie`（**真的存在**才算，不凭空造）→ `~/.pie`。
///
/// 就这三步、都写在这儿：**别为了好测把顺序拆成单独的函数**（要测就改环境变量测能测的那两档，
/// 见用例；`<cwd>/.pie` 那档得改进程 cwd，代价比收益大）。
pub fn pie_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PIE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let local = cwd.join(".pie");
        if local.is_dir() {
            return local;
        }
    }
    home_dir().join(".pie")
}

pub fn default_config_file() -> PathBuf {
    pie_dir().join("config.toml")
}

// ---------------------------------------------------------------- 时间（没日期库，自己换算）
//
// 只有一个时钟出口 `now()`，其余都是**纯函数**（unix 秒 → 文本）。落盘的 unix 时间戳一律取
// `now().as_secs()`：数字没有「带不带时区后缀」「T 还是空格」这类歧义，跨版本也比大小省事。

/// 当前时刻——**唯一碰 `SystemTime` 的地方**。
///
/// 秒与纳秒都从这一个 `Duration` 取：落盘的 unix 时间戳用 `now().as_secs()`、文件名要微秒用
/// `now().subsec_micros()`、退避 jitter 用 `now().subsec_nanos()`。系统时钟早于 1970（实际
/// 不会发生）→ 0。
pub fn now() -> Duration {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
}

/// unix 秒 → 本地时间 `YYYY-MM-DD HH:MM`（`pie sessions` / `files list` 的时间列）。
///
/// 只给人看、不参与任何判断；换算就两步——先加本地时区偏移，再把 unix 秒拆成年月日时分。
/// （这两步原本拆成 `civil` / `local_utc_offset` 两个私有函数，消费方只有这里 → 按「别给单一消费者
/// 造私有辅助函数」合并了进来；日历换算那几行的来历见下面的注释。）
pub fn fmt_local(secs: i64) -> String {
    /// 本地时区相对 UTC 的偏移（秒，含夏令时）——**不给 UTC 时区**。
    ///
    /// 用 `localtime_r`（POSIX，线程安全）拿到带 DST 的 `struct tm`，取 `tm_gmtoff`；
    /// 非 unix 平台没这套东西 → 回退 0（显示成 UTC）：比引一个带时区库的依赖划算。
    /// ⚠ 它**不会**自己重读 `TZ`（glibc 下得显式 `tzset()`）——用例里改 `TZ` 后必须调一次。
    #[cfg(unix)]
    fn local_utc_offset(secs: i64) -> i64 {
        // SAFETY: `localtime_r` 把结果写进我们给的 `tm`（返回的不是共享缓冲）
        unsafe {
            let t = secs as libc::time_t;
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&t, &mut tm).is_null() {
                return 0;
            }
            tm.tm_gmtoff as i64
        }
    }
    #[cfg(not(unix))]
    fn local_utc_offset(_secs: i64) -> i64 {
        0
    }

    let secs = secs + local_utc_offset(secs);
    // unix 秒 → 年月日：Howard Hinnant 的 `civil_from_days`（不引日期库）。
    // `div_euclid`/`rem_euclid` 向下取整，负值（1970 前）也对。
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
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        tod / 3600,
        (tod % 3600) / 60
    )
}

/// 千分位（`1234567` → `1,234,567`）：Rust 的 format 没有千分位语法，只能自己加。
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

/// 首次运行创建全局记忆的种子文件（已存在则跳过，**不覆盖**）：每次启动时检查一次。
///
/// 种子正文放在 `prompts/memory.md`、`include_str!` 编进来（与内置 system prompt 同一套做法：
/// 模板里全是中文与骨架，写成转义串难读也容易碰格式）。
///
/// 写不了就算了（权限/只读家目录）：这只是个种子，不值得挡住启动。
pub fn ensure_global_memory() {
    let _ = ensure_global_memory_file();
}

/// 同上，但把结果交给调用方（`pie setup` 要报「已创建 / 已存在」）：返回 `(路径, 是否新建)`。
pub fn ensure_global_memory_file() -> std::io::Result<(PathBuf, bool)> {
    let path = global_memory_file();
    if path.exists() {
        return Ok((path, false));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&path, include_str!("../prompts/memory.md"))?;
    Ok((path, true))
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

/// 环境变量覆盖：`OPENAI_API_KEY` → `api_key`。空串视为没设（不覆盖配置文件里的值）。
pub fn env_api_key() -> Option<String> {
    non_empty_var("OPENAI_API_KEY")
}

/// 环境变量覆盖：`OPENAI_BASE_URL` → `base_url`。空串视为没设。
pub fn env_base_url() -> Option<String> {
    non_empty_var("OPENAI_BASE_URL")
}

fn non_empty_var(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

/// `pie setup` 用：配置文件不存在就写一份默认的（父目录 `~/.pie` 也一起建）。
///
/// 已存在**一律不覆盖**（里面可能有用户自己的 key / 注释），所以可以反复跑。
/// 默认值只有一个来源：`Config::default()` —— 与「没有配置文件时 `pie` 的行为」逐字一致。
/// 返回 `(路径, 是否新建)`。
pub fn ensure_config_file(explicit: Option<&Path>) -> Result<(PathBuf, bool), ConfigError> {
    let path = resolve_config_file(explicit);
    if path.exists() {
        return Ok((path, false));
    }
    let mut config = Config::default();
    config.config_file = Some(path.clone()); // 让 save() 写到解析出来的那个路径
    config.save()?;
    Ok((path, true))
}

// ---------------------------------------------------------------- 压缩配置

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ToolCompaction {
    pub head: usize,
    pub tail: usize,
    /// 保护窗口：最近 N 个 step 批次**不碰**（跨批次滚动保护——只工具级有这个概念）。
    pub keep_last_steps: usize,
}

impl Default for ToolCompaction {
    fn default() -> Self {
        Self {
            head: 30,
            tail: 50,
            keep_last_steps: 7,
        }
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

/// 子表/布尔 → `Option<子表>`。
///
/// - `xx = false` → `None`（关闭这一级）
/// - `xx = true`  → `Some(T::default())`（**开启用默认值**：只写布尔、不给参数 = 用该级的默认配置）
/// - `[compaction.xx]` 子表 → 用它
fn de_level<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(match TableOrBool::<T>::deserialize(d)? {
        TableOrBool::Bool(false) => None,
        TableOrBool::Bool(true) => Some(T::default()),
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
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            tool: Some(ToolCompaction::default()),
            turn: true,
            session: Some(SessionCompaction::default()),
            soft_ratio: 0.8,
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
    pub reserved_tokens: Option<usize>,
    pub context_window: usize,
    /// `None` = 不做任何上下文压缩。
    #[serde(deserialize_with = "de_compaction")]
    pub compaction: Option<CompactionConfig>,
    pub timeout_seconds: f64,
    pub max_retries: usize,
    pub max_retry_delay_seconds: f64,
    pub theme: String,
    /// 按工具名设默认私有参数（下划线开头，不进 schema）。
    pub tools: HashMap<String, toml::Value>,
    pub tui: TuiConfig,
    pub files_api: bool,
    pub files_ttl_days: usize,
    /// 同一批 tool_calls 是否并发执行。
    pub parallel_tools: bool,

    /// 运行时属性（不落盘）：配置来源路径。
    #[serde(skip)]
    pub config_file: Option<PathBuf>,
    /// 运行时属性（不落盘）：CLI `--auto-compact-threshold` 一次性覆盖软阈值。
    #[serde(skip)]
    pub auto_compact_threshold: Option<usize>,
    /// 运行时属性（不落盘）：CLI `--system-prompt`（替换基础提示：文本或文件内容）。
    #[serde(skip)]
    pub system_prompt: Option<String>,
    /// 运行时属性（不落盘）：CLI `--append-system-prompt`（可重复）。
    #[serde(skip)]
    pub append_system_prompt: Vec<String>,
    /// 运行时属性（不落盘）：数据存储（默认 `PIE_DIR` / `./.pie` / `~/.pie`；测试可注入临时 root）。
    #[serde(skip)]
    pub storage: Storage,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "deepseek-flash".to_string(),
            // 本部署内置的默认 key；空串 = 用户没配
            base_url: "https://api.deepseek.com/".to_string(),
            api_key: "<API_KEY>".to_string(),
            reasoning_effort: "high".to_string(),
            reserved_tokens: Some(128_000),
            context_window: 1024 * 1024,
            compaction: Some(CompactionConfig::default()),
            timeout_seconds: 60.0,
            max_retries: 5,
            max_retry_delay_seconds: 3.0,
            theme: "catppuccin".to_string(),
            tools: HashMap::new(),
            tui: TuiConfig::default(),
            files_api: true,
            files_ttl_days: 30,
            parallel_tools: true,
            config_file: None,
            auto_compact_threshold: None,
            system_prompt: None,
            append_system_prompt: Vec::new(),
            storage: Storage::default(),
        }
    }
}

impl Config {
    /// 从配置文件加载。文件不存在 → 全默认值（要不要补齐默认件由 CLI 层负责）。
    ///
    /// 加载后按环境变量覆盖两项（空串 / 全空白视为没设 → 不动配置文件里的值）：
    /// `OPENAI_API_KEY` → `api_key`、`OPENAI_BASE_URL` → `base_url`。
    /// 只改内存、不写回配置文件（`save()` 照旧落盘当前值）。
    pub fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let path = resolve_config_file(explicit);
        let mut config = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| ConfigError::Io(path.clone(), e.to_string()))?;
            toml::from_str::<Config>(&text)
                .map_err(|e| ConfigError::Parse(path.clone(), e.to_string()))?
        } else {
            Config::default()
        };
        if let Some(key) = env_api_key() {
            config.api_key = key;
        }
        if let Some(url) = env_base_url() {
            config.base_url = url;
        }
        config.config_file = Some(path);
        Ok(config)
    }

    /// 可用输入预算：服务端按「输入 tokens + max_tokens ≤ 窗口」判超限。
    pub fn context_budget(&self) -> usize {
        // 预留比窗口还大时（配置写错）按 1 算，别让减法下溢
        self.context_window
            .saturating_sub(self.reserved_tokens.unwrap_or(0))
            .max(1)
    }

    /// 软阈值比例（未配置 `[compaction]` 时给**默认那套**的值）——触发自动压缩的水位，`/stat` 也展示它。
    pub fn soft_ratio(&self) -> f64 {
        self.compaction.clone().unwrap_or_default().soft_ratio
    }

    /// 软阈值：触发自动压缩的 token 水位。
    pub fn soft_limit(&self) -> usize {
        // CLI 一次性覆盖（`--auto-compact-threshold`）优先
        if let Some(n) = self.auto_compact_threshold {
            return n.max(1);
        }
        (((self.context_budget() as f64) * self.soft_ratio()).max(1.0)) as usize
    }

    /// 工具私有默认参数（下划线开头，由 dispatch 注入）。
    /// 派生默认：`read` 的 `_max_image_bytes`（内联 32 MiB / Files API 64 MiB）。
    pub fn tool_defaults(&self) -> HashMap<String, toml::Table> {
        let mut out: HashMap<String, toml::Table> = self
            .tools
            .iter()
            .filter_map(|(k, v)| v.as_table().cloned().map(|t| (k.clone(), t)))
            .collect();
        // 图片字节上限：内联受单图 32 MiB 限制；开了 Files API 放宽到 64 MiB
        let image_cap = if self.files_api {
            64 * 1024 * 1024
        } else {
            32 * 1024 * 1024
        };
        out.entry("read".to_string())
            .or_default()
            .entry("_max_image_bytes".to_string())
            .or_insert(toml::Value::Integer(image_cap));
        out
    }

    /// 归一化后的思考深度：`none` → `None`（不发送 `reasoning_effort`）。
    pub fn normalized_reasoning_effort(&self) -> Option<&str> {
        normalize_reasoning_effort(&self.reasoning_effort)
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
    /// 配置 → TOML 值（**只含持久字段**：`#[serde(skip)]` 的运行时字段不在里面）。
    ///
    /// `save()` 用它落盘；绑定侧 `Config.to_dict()` / `update()` 也以它为基准（字段列表只有这一份）。
    pub fn to_toml(&self) -> toml::Value {
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
                Some(n) => V::Integer(n as i64),
                None => V::String("auto".to_string()),
            },
        );
        put("context_window", V::Integer(self.context_window as i64));
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
                    V::Table(comp)
                }
                None => V::Boolean(false),
            },
        );
        put("timeout_seconds", V::Float(self.timeout_seconds));
        put("max_retries", V::Integer(self.max_retries as i64));
        put("max_retry_delay_seconds", V::Float(self.max_retry_delay_seconds));
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
        put("files_ttl_days", V::Integer(self.files_ttl_days as i64));
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

/// `reserved_tokens` 容错：允许 `"auto"` / `"64k"` / `384K` 这类手写值；
/// 0/负数按「不发送 max_tokens」处理（`None`）。
///
/// 键**缺失**时不走这里 —— `Config` 上的 `#[serde(default)]` 会拿 `Config::default()`
/// 的值补上（`Some(128_000)`）—— 没写就用默认值。
fn de_reserved_tokens<'de, D>(d: D) -> Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match ReservedRaw::deserialize(d)? {
        ReservedRaw::Int(n) => (n >= 1).then_some(n as usize),
        ReservedRaw::Str(s) => parse_reserved_tokens(&s).ok().flatten(),
    })
}

/// 解析 `reserved_tokens` 输入：auto/default/none/0/空 → `None`；支持 `64k` / `384K` 简写。
pub fn parse_reserved_tokens(raw: &str) -> Result<Option<usize>, String> {
    let text = raw.trim().to_ascii_lowercase();
    if ["auto", "default", "none", "0", ""].contains(&text.as_str()) {
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
        .map(|v| (v * multiplier) as usize)
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

/// `[compaction]` 可以为 `false`（整体关闭）或一张表；不写 = 默认三级全开。
fn de_compaction<'de, D>(d: D) -> Result<Option<CompactionConfig>, D::Error>
where
    D: Deserializer<'de>,
{
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

/// 按分层组装 system prompt：SYSTEM.md（角色）+ AGENTS.md（项目）+ 记忆（存在即加载）
/// + **运行时状态节**（永远在最后，见 [`RUNTIME_STATE_HEADING`]）。
///
/// `system_prompt` 非空时替换 SYSTEM.md 基础提示；`append_system_prompt` 追加到记忆之后、运行时状态之前。
pub fn build_system_prompt(
    config: &Config,
    system_prompt: Option<&str>,
    append_system_prompt: &[String],
) -> String {
    let _ = config; // 预留：将来这里可拼一行上下文预算说明
    let base = match system_prompt {
        Some(s) => s.to_string(),
        None => read_text(resolve_prompt_file(SYSTEM_FILE)),
    };
    let base = if base.is_empty() {
        // 内置兜底 prompt（`prompts/system.md`）：编译期嵌入 → 仓库根没有 `SYSTEM.md` 时也能单文件跑
        // （本仓根就没有那个文件）
        include_str!("../prompts/system.md").to_string()
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
    // 永远**最后**一节：`Session::aturn` 每轮按 [`RUNTIME_STATE_HEADING`] 重拼它
    //（所以别把它插到中间，也别把 `append_system_prompt` 挪到它后面）。
    parts.push(runtime_state("")); // 初建：解释器那块是空的（`Session` 每轮再往里添）
    parts.join("\n\n")
}

/// 运行时状态节的小标题：**每轮重拼时按它定位**（取最后一次出现，见 `Session::refresh_runtime_state`）。
pub const RUNTIME_STATE_HEADING: &str = "## 运行时状态";

/// 运行时状态：进程级、会变、且**必须每轮现取**的东西（现在只有 cwd）。
///
/// 为什么不学「状态一变就往历史里插一条 system 消息」（`/cd` 当年就是这么干的）：
/// 那是 `messages` 里的一条，而轮次级压缩的 span 就是「相邻两个 user 之间的**一切**」
/// （`context::compact_turns` 不按 role 过滤）→ 它迟早被卷进 `[轮次原文已保存]` 的摘要里，
/// 模型再也看不见（原文只在盘上，模型没有回查途径 —— 它甚至不知道有这回事）。
///
/// `messages[0]` 则是**压缩免疫**的：轮次级只在两个 user 之间动手（`messages[0]` 在第一个 user
/// 之前），会话级明确「system 不动」。代价是它住在提示词前缀里 → **内容没变就别动**，
/// 否则每次请求都打掉服务端的前缀缓存。
///
/// `repl_blocks` = 解释器跑过的代码块（**只列已被压缩带走的那些**，由 `Session` 派生，可为空）——
/// 压缩会把 `tool_calls` 从消息里删掉，而代码是「会话级状态」里最不可从摘要里恢复的那部分，
/// 所以让它住进这节（跟着 cwd 一起现取，而不是插进历史）。
pub fn runtime_state(repl_blocks: &str) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".to_string());
    let mut out = format!(
        "{RUNTIME_STATE_HEADING}\n\n当前工作目录：{cwd}\n（`read` / `edit` / `bash` 的相对路径都相对它；它可能中途变化，别假设整段对话里不变。）"
    );
    if !repl_blocks.is_empty() {
        out.push_str("\n\n");
        out.push_str(repl_blocks);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 数据根目录：`PIE_DIR`（非空）→ `<cwd>/.pie`（存在才算）→ `<home>/.pie`。
    ///
    /// 只测「问环境」那两档：`<cwd>/.pie` 那档要改进程 cwd（会污染并行跑的用例），
    /// 而给 `pie_dir` 开注入参数是为了测试拆代码（不干）——那几行直接读代码。
    #[test]
    fn pie_dir_prefers_env_then_home() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old_dir = std::env::var_os("PIE_DIR");
        let old_home = std::env::var_os("HOME");

        // 1) `PIE_DIR` 非空 → 直接用它（本地就算有 `.pie` 也不看）
        std::env::set_var("PIE_DIR", "/tmp/pie-from-env");
        assert_eq!(pie_dir(), PathBuf::from("/tmp/pie-from-env"));

        // 2) 空串 = 没设 → 往下走；cwd 没有 `.pie` 时就是 `$HOME/.pie`
        std::env::set_var("PIE_DIR", "");
        std::env::set_var("HOME", "/tmp/pie-home-stub");
        let cwd_has_pie = std::env::current_dir()
            .map(|d| d.join(".pie").is_dir())
            .unwrap_or(false);
        if !cwd_has_pie {
            assert_eq!(pie_dir(), PathBuf::from("/tmp/pie-home-stub/.pie"));
        }

        match old_dir {
            Some(v) => std::env::set_var("PIE_DIR", v),
            None => std::env::remove_var("PIE_DIR"),
        }
        match old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    /// `fmt_local` 的日历换算钉住已知时刻：`TZ` 用 **POSIX 串**（`UTC0` / `CST-8`，不依赖 tzdata，
    /// 后者就是 UTC+8），并且**必须显式 `tzset()`**——glibc 的 `localtime_r` 不会自己重读 `TZ`。
    #[cfg(unix)]
    #[test]
    fn fmt_local_matches_known_instants() {
        // `libc` crate 只在 windows 那份里声明了 `tzset`（unix 那份没有）→ 自己声明
        extern "C" {
            fn tzset();
        }

        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("TZ");
        let set_tz = |tz: &str| {
            std::env::set_var("TZ", tz);
            // SAFETY: 只动 libc 的时区缓存；`ENV_LOCK` 保证没有别的用例同时读它
            unsafe { tzset() };
        };

        set_tz("UTC0");
        assert_eq!(fmt_local(0), "1970-01-01 00:00");
        assert_eq!(fmt_local(1_700_000_000), "2023-11-14 22:13");
        assert_eq!(fmt_local(-1), "1969-12-31 23:59"); // 负值（1970 前）也对
        set_tz("CST-8"); // POSIX 串里的符号与 `date +%z` 相反：`CST-8` = UTC+8
        assert_eq!(fmt_local(0), "1970-01-01 08:00");
        assert_eq!(fmt_local(1_700_000_000), "2023-11-15 06:13");

        match old {
            Some(v) => std::env::set_var("TZ", v),
            None => std::env::remove_var("TZ"),
        }
        unsafe { tzset() };
    }

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
    fn reasoning_levels_derive_from_thinking_levels() {
        // `/thinking` 的候选从**唯一那张表**（`-t` 的合法值）派生：四档配置口径，别名不列
        let levels: Vec<&str> = reasoning_levels().collect();
        assert_eq!(levels, ["none", "low", "high", "max"]);
        assert!(levels.iter().all(|lv| THINKING_LEVELS.contains(lv)));
        // `off` 只是人写法，`minimal` / `medium` / `xhigh` 是服务端兼容名：都收、都原样透传，
        // 但不进 `/thinking` 候选
        for alias in ["off", "minimal", "medium", "xhigh"] {
            assert!(THINKING_LEVELS.contains(&alias), "{alias} 要能被 -t 收下");
            assert!(!levels.contains(&alias), "{alias} 不该进候选");
        }
    }

    #[test]
    fn budget_and_limits() {
        let config = Config::default();
        assert_eq!(
            config.context_budget(),
            config.context_window - 128_000
        );
        assert_eq!(config.soft_limit(), (config.context_budget() as f64 * 0.8) as usize);
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
timeout_seconds = 60.0
max_retries = 5
parallel_tools = true

[compaction]
turn = true
soft_ratio = 0.8

[compaction.tool]
head = 30
tail = 50

[compaction.session]
head = 3
tail = 5

[tui]
lean = true
"#;
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.model, "deepseek-flash");
        assert_eq!(config.reserved_tokens, Some(128_000));
        let comp = config.compaction.unwrap();
        assert_eq!(comp.tool.unwrap().head, 30);
        assert_eq!(comp.session.unwrap().tail, 5);
        assert!(comp.turn);
        assert!(config.tui.lean);
    }

    /// `save` → `load` 往返：TOML 没有 null，`reserved_tokens = None` 要写成 `"auto"` 再读回 None；
    /// `compaction` 是 `false` / 表（子级关掉写成 `tool = false`）。
    #[test]
    fn config_round_trips_through_toml() {
        let dir = std::env::temp_dir().join(format!("pie-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let config = Config {
            model: "deepseek-v4-pro".into(),
            reserved_tokens: None, // → "auto"
            context_window: 12_345,
            compaction: Some(CompactionConfig {
                tool: None, // → tool = false
                ..Default::default()
            }),
            config_file: Some(path.clone()),
            ..Default::default()
        };
        config.save().expect("save");

        let back = Config::load(Some(&path)).expect("load");
        assert_eq!(back.model, "deepseek-v4-pro");
        assert_eq!(back.reserved_tokens, None, "\"auto\" 要读回 None");
        assert_eq!(back.context_window, 12_345);
        let comp = back.compaction.as_ref().expect("compaction");
        assert!(comp.tool.is_none(), "tool = false 读回 None（该级关闭）");
        assert!(comp.session.is_some(), "session 没关就还是表");
        assert_eq!(back.max_retries, config.max_retries);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_global_memory_seeds_once_and_never_overwrites() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pie-memory-{}", std::process::id()));
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
    fn ensure_global_memory_reports_whether_it_created_the_file() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pie-memory-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("PIE_DIR", &dir);

        // `~/.pie` 不存在：连父目录一起建，并报「已创建」
        let (path, created) = ensure_global_memory_file().expect("写种子");
        assert!(created, "第一次要报已创建");
        assert_eq!(path, global_memory_file());
        assert!(path.exists());

        // 再跑一次：文件已在，报「已存在」且内容不动
        std::fs::write(&path, "我的记忆").unwrap();
        let (_, created) = ensure_global_memory_file().expect("再跑一次");
        assert!(!created, "已存在就不算新建");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "我的记忆");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_config_file_writes_defaults_once_and_keeps_existing() {
        let dir = std::env::temp_dir().join(format!("pie-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // 父目录故意不存在：`pie setup` 要能把 `~/.pie/` 一起建出来
        let path = dir.join("nested").join("config.toml");

        let (written, created) = ensure_config_file(Some(&path)).expect("写默认配置");
        assert_eq!(written, path);
        assert!(created);
        // 写出来的必须能读回来（默认值往返，不是一份手写样板）
        let back = Config::load(Some(&path)).expect("读回默认配置");
        let dflt = Config::default();
        assert_eq!(back.model, dflt.model);
        // `load` 会被 `OPENAI_*` 环境变量覆盖：预期值也要过同样的门
        assert_eq!(back.base_url, env_base_url().unwrap_or_else(|| dflt.base_url.clone()));
        assert_eq!(back.api_key, env_api_key().unwrap_or_else(|| dflt.api_key.clone()));
        assert_eq!(back.reserved_tokens, dflt.reserved_tokens);
        assert_eq!(back.context_window, dflt.context_window);
        assert_eq!(back.tui.lean, dflt.tui.lean);

        // 已存在：原样保留，报「已存在」
        std::fs::write(&path, "model = \"mine\"\n").unwrap();
        let (_, created) = ensure_config_file(Some(&path)).expect("再跑一次");
        assert!(!created, "已存在就不算新建");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "model = \"mine\"\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `OPENAI_API_KEY` / `OPENAI_BASE_URL` 覆盖配置里对应的两项；**空串 / 全空白 = 没设**。
    #[test]
    fn env_overrides_api_key_and_base_url() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pie-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "base_url = \"https://file.example/\"\napi_key = \"sk-file\"\n",
        )
        .unwrap();

        let prev_key = std::env::var("OPENAI_API_KEY").ok();
        let prev_url = std::env::var("OPENAI_BASE_URL").ok();

        // 设了就用环境变量的值（压过配置文件）
        std::env::set_var("OPENAI_API_KEY", "sk-env");
        std::env::set_var("OPENAI_BASE_URL", "https://env.example/v1");
        let config = Config::load(Some(&path)).expect("load");
        assert_eq!(config.api_key, "sk-env");
        assert_eq!(config.base_url, "https://env.example/v1");
        assert_eq!(config.config_file.as_deref(), Some(path.as_path()));

        // 空串 / 全空白 = 没设 → 保留配置文件里的值
        std::env::set_var("OPENAI_API_KEY", "");
        std::env::set_var("OPENAI_BASE_URL", "   ");
        let config = Config::load(Some(&path)).expect("load");
        assert_eq!(config.api_key, "sk-file");
        assert_eq!(config.base_url, "https://file.example/");

        match prev_key {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
        match prev_url {
            Some(v) => std::env::set_var("OPENAI_BASE_URL", v),
            None => std::env::remove_var("OPENAI_BASE_URL"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seeded_global_memory_lands_in_the_system_prompt() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pie-memory-prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("PIE_DIR", &dir);

        let config = Config::default();
        // 按**路径**判有无：项目记忆里可能碰巧引用了「## 全局记忆（…）」这个形状
        // （本仓 MEMORY.md 就写过）——拿那串当断言会自打脸。
        let marker = format!("## 全局记忆（{}）", global_memory_file().display());
        let before = build_system_prompt(&config, None, &[]);
        assert!(!before.contains(&marker), "没文件就不拼这块");

        ensure_global_memory();
        let after = build_system_prompt(&config, None, &[]);
        assert!(after.contains(&marker), "种子文件要进 system prompt：{after}");
        assert!(after.contains("跨项目的持久记忆"), "连正文一起拼进去");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_can_be_disabled() {
        let config: Config = toml::from_str("compaction = false").unwrap();
        assert!(config.compaction.is_none());
        let config: Config =
            toml::from_str("[compaction]\ntool = false\nturn = false\nsession = false").unwrap();
        let c = config.compaction.unwrap();
        assert!(c.tool.is_none() && !c.turn && c.session.is_none());
    }

    /// `tool = true` / `session = true`（只写布尔、不给参数）= **开启用默认值**。
    #[test]
    fn level_boolean_true_means_enabled_with_defaults() {
        let config: Config =
            toml::from_str("[compaction]\ntool = true\nsession = true").unwrap();
        let c = config.compaction.unwrap();
        assert_eq!(c.tool.unwrap().head, 30, "默认 head");
        assert_eq!(c.session.unwrap().tail, 5, "默认 tail");
    }
}
