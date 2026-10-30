//! `shplugin` —— **用 shell 脚本写的工具**：一个 `*.sh` = 一个工具。
//!
//! 存在的理由只有一条：**环境受限时也能加工具**。`pyplugin` 那条路要一个装着 `pie` 的
//! 解释器（`[python] interpreter` / `envs/base`），设备、容器、`alpine`、离线机上未必有；
//! 而 `sh` + coreutils 几乎哪里都有。所以这里**不起长活进程、不需要任何解释器**：
//! 一次调用 = 起一个脚本、等它退出。
//!
//! 位置：`<root>/plugins/*.sh`（与 Python 插件同一个约定目录，存在即加载、不递归、排过序）。
//!
//! 形状（完整契约见 `docs/sh-plugins.md`）：
//!
//! ```sh
//! #!/bin/sh
//! # schema! {
//! #   "description": ["第一行", "第二行"],
//! #   "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}
//! # }
//! query=''
//! while [ $# -gt 0 ]; do
//!     case $1 in
//!         --query=*) query=${1#*=} ;;
//!         *) echo "未知参数：$1" >&2; exit 2 ;;
//!     esac
//!     shift
//! done
//! ```
//!
//!   - **schema 住 shebang 后面那段注释里**（`# schema! { … }`）：脚本自己就能被模型读懂、
//!     也不需要一个额外的清单文件；注释里的块对 shell 完全无副作用（`sh` 只把它当注释）；
//!   - **参数走命令行 `--键名=值`**：顶层标量一个参数一个（bool → `1`/`0`、数字按 JSON 字面量、
//!     字符串原样；**不经过 shell**，空格与换行都安全）；数组 / 对象只在 **stdin 的一行
//!     compact JSON** 里；`PIE_ARG_<键名>` 环境变量**仍然导出**（老脚本兼容，新脚本别依赖）；
//!   - **私有参数（`_` 开头）是站点配置**：`--_file=…`（环境变量另给 `PIE_ARG__file`，多一个
//!     下划线），模型看不见它们（`_` 不进 schema）；
//!   - stdout = 正文、stderr = 失败时才补进正文、`exit != 0` = 失败。超限落盘与 bash 同路
//!     （`ToolBody::fit`）。
//!
//! 进程管线（起脚本 / 等它、超时与取消杀组）**住本模块**，不进口具层 —— 与 `pyhost` 之于
//! `pyplugin` 同一套分工：`tools.rs` 只提供「一个工具长什么样」（`Registry` / `ToolOutput` /
//! `ToolBody::fit`），不为每种插件后端管子进程。
//!
//! 与 `pyplugin` 的两点差别（都是**有意的**）：**没有跨调用状态、也没有状态目录** —— 一次调用
//! 一个进程，要留东西就自己往 `$PIE_HOME` 里写（`PIE_HOME` 是唯一给你的路径）；`cwd` 跟着 pie 走
//! （`/cd` 之后脚本看到的也是新目录）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::cancel::CANCEL_TEXT;
use crate::config::Config;
use crate::tools::{
    Attachment, CallFn, Ended, Limits, Registry, Status, ToolBody, ToolCtx, ToolError, ToolOutput,
    ToolResult, valid_tool_name,
};

/// 默认超时（秒）：`# schema!` 里没写 `timeout_seconds` 就用它。
///
/// shell 工具干的常是「连设备 / 拉文件」这类可能**永远不回话**的活，所以默认不是「不限时」。
const DEFAULT_TIMEOUT_SECONDS: i64 = 120;

/// 装载 shell 工具：扫 `<root>/plugins/*.sh`，逐个解析 `# schema!` 并注册进 `registry`。
///
/// **任何失败都只告警**（文件读不了 / 没有 schema / JSON 不合法 / 名字非法 / 重名），
/// 原样返回 `registry` —— 一个坏脚本不该让会话起不来。
pub fn load(registry: Registry, config: &Config) -> Registry {
    let mut registry = registry;
    for spec in specs(&config.storage.plugins()) {
        if !valid_tool_name(&spec.name) {
            crate::log::warn(format!(
                "[shplugin] 跳过非法工具名 {:?}（{}）：只能用 [A-Za-z0-9_-]、长度 1..=64",
                spec.name,
                spec.path.display()
            ));
            continue;
        }
        // `with_dynamic` 撞重名会 panic（那是 Rust 侧注册的编程错误）——外部来的名字必须自己挡
        if registry.names().contains(&spec.name.as_str()) {
            crate::log::warn(format!(
                "[shplugin] 跳过重名工具 {}（{}，与已有工具同名）",
                spec.name,
                spec.path.display()
            ));
            continue;
        }
        let script = Arc::new(Script {
            path: spec.path,
            name: spec.name.clone(),
            timeout: spec.timeout,
            home: config.storage.root.clone(),
        });
        let call: CallFn = Arc::new(move |args, ctx| {
            let script = Arc::clone(&script);
            Box::pin(async move { run(&script, args, ctx).await })
        });
        registry = registry.with_dynamic(&spec.name, &spec.description, spec.parameters, call);
    }
    registry
}

/// 注册进 `Registry` 的那条调用体捕获的东西（`Clone` 便宜：整个结构住 `Arc` 里）。
struct Script {
    path: PathBuf,
    name: String,
    /// pie 什么时候杀它（秒；`[tools.<名字>] _timeout_seconds` 能覆盖）。
    timeout: i64,
    /// `PIE_HOME`：数据根（脚本要往 `files/` 放产物时用 —— `pie files list` / `rm` 认的就是那个位置）。
    ///
    /// ⚠ 没有「状态目录」：一次调用一个进程，**跨调用状态该不该有、放哪儿、怎么清理，都是脚本
    /// 自己的事**（要就自己 `mkdir -p "$PIE_HOME/state/$PIE_TOOL_NAME"`）。给壳体加一层状态目录
    /// 等于又多一种没人 GC 的磁盘概念，而它并不是「工具」这件事的必要部分。
    home: PathBuf,
}

/// 一个脚本工具解析完 schema 之后的形状。
struct Spec {
    path: PathBuf,
    name: String,
    description: String,
    parameters: Value,
    timeout: i64,
}

/// 扫目录里的 `*.sh`（不递归、排过序）：一个文件 = 一个工具。
///
/// 目录不存在 / 里面没有 `*.sh` = 没装插件（正常状态）→ 静默返回空（与 `pyplugin` 同规矩：
/// 约定目录常年空着，每次启动刷一句「没有 *.sh」只是噪音）；真错（目录读不了）照样告警。
fn specs(dir: &Path) -> Vec<Spec> {
    if !dir.is_dir() {
        return Vec::new();
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            crate::log::warn(format!("[shplugin] 读不了目录 {}: {e}", dir.display()));
            return Vec::new();
        }
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "sh"))
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|path| match read_spec(&path) {
            Ok(spec) => Some(spec),
            Err(e) => {
                crate::log::warn(format!("[shplugin] 跳过 {}: {e}", path.display()));
                None
            }
        })
        .collect()
}

/// 读一个脚本的 schema（`# schema!` 块），名字缺省取文件名主干。
fn read_spec(path: &Path) -> Result<Spec, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("读不了: {e}"))?;
    let decl: Decl = serde_json::from_str(&schema_block(&text)?)
        .map_err(|e| format!("schema 不是合法 JSON: {e}"))?;
    let name = decl.name.unwrap_or_else(|| {
        path.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    Ok(Spec {
        path: path.to_path_buf(),
        name,
        description: decl
            .description
            .map(DeclText::into_lines)
            .unwrap_or_default(),
        parameters: decl
            .parameters
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        timeout: decl.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS),
    })
}

/// 从脚本正文里抠出 `# schema! { … }` 那块注释的**内容**（不含注释前缀，就是一段 JSON）。
///
/// 契约：**第 1 行必须是 shebang**（`#!…`）；`schema!` 住 shebang 之后、第一行代码之前的
/// 注释区里；块从 `schema!` 那一行起、到**括号配平**为止（`{` / `[` 算一层，字符串里的不算
/// —— 否则描述里写个 `}` 就把块截断了）；每行开头的 `#` 与紧跟的**一个**空格由这里剥掉。
/// 放在 shebang 后面是为了「文件从第一行起就是可执行脚本」：`./tool.sh` 与「被 pie 加载」
/// 看到的是同一份文件，不需要额外的清单。
fn schema_block(text: &str) -> Result<String, String> {
    let mut lines = text.lines();
    if !lines.next().is_some_and(|line| line.starts_with("#!")) {
        return Err("第 1 行必须是 shebang（如 `#!/bin/sh`）".to_string());
    }
    let mut body = String::new();
    let mut depth = 0i32;
    let (mut in_string, mut escaped) = (false, false);
    for line in lines {
        // 注释区之外（= 真正的代码开始了）→ 不再找
        let Some(comment) = line.strip_prefix('#') else {
            break;
        };
        let code = comment.strip_prefix(' ').unwrap_or(comment);
        if depth == 0 {
            let Some(rest) = code.trim().strip_prefix("schema!") else {
                continue;
            };
            let rest = rest.trim();
            if !rest.is_empty() && rest != "{" {
                return Err("`schema!` 后面只能紧跟 `{`".to_string());
            }
            body.push_str("{\n");
            depth = 1;
            continue;
        }
        body.push_str(code);
        body.push('\n');
        for ch in code.chars() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
            } else if ch == '"' {
                in_string = true;
            } else if matches!(ch, '{' | '[') {
                depth += 1;
            } else if matches!(ch, '}' | ']') {
                depth -= 1;
            }
        }
        if depth <= 0 {
            return serde_json::from_str::<Value>(&body)
                .map(|_| body)
                .map_err(|e| format!("schema 不是合法 JSON: {e}"));
        }
    }
    Err(if depth > 0 {
        "`# schema!` 块没闭合（括号没配平）".to_string()
    } else {
        "shebang 后面的注释区里没有 `# schema! { … }` 块".to_string()
    })
}

/// `# schema!` 块里认得的几个键（其余忽略，将来加键不用改别处）。
#[derive(Deserialize)]
struct Decl {
    /// 工具名（缺省 = 文件名主干，`computer_use.sh` → `computer_use`）。
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<DeclText>,
    /// 发给模型的 JSON Schema（缺省 = 无参数对象）。
    #[serde(default)]
    parameters: Option<Value>,
    /// pie 什么时候杀掉它（秒）；默认 [`DEFAULT_TIMEOUT_SECONDS`]。
    #[serde(default)]
    timeout_seconds: Option<i64>,
}

/// `description` 两种写法都收：一行字符串，或多行数组（脚本里写数组更整齐）。
#[derive(Deserialize)]
#[serde(untagged)]
enum DeclText {
    One(String),
    Many(Vec<String>),
}

impl DeclText {
    fn into_lines(self) -> String {
        match self {
            DeclText::One(one) => one,
            DeclText::Many(rows) => rows.join("\n"),
        }
    }
}

/// 一次调用：参数分拣 → 起脚本 → 收输出。
///
/// 分拣规则：`_max_lines` / `_max_bytes` 是**宿主旋钮**（摘掉、不投递）；`_timeout_seconds`
/// 既管宿主超时、也原样给脚本（它自己内部的 `timeout` 常要）；其余键全部投递。
///
/// 投递三种投影（同一个 args）：**argv** 里每个顶层标量一个 `--键名=值`（主通道，规则见
/// [`scalar_text`]）；**stdin** 一行 compact JSON（数组 / 对象只能从这儿拿）；
/// **`PIE_ARG_<键名>`** 环境变量（兼容旧脚本，规则见 [`env_name`]）。
async fn run(script: &Script, args: Value, ctx: ToolCtx) -> ToolResult {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut argv: Vec<String> = Vec::new();
    let mut limits = Limits::default();
    let mut timeout = script.timeout;
    let mut payload = serde_json::Map::new();
    if let Some(map) = args.as_object() {
        for (key, value) in map {
            // `null` = 模型显式说「没有」→ 当没给（让脚本自己的默认值说话）
            if value.is_null() {
                continue;
            }
            match key.as_str() {
                "_max_lines" => {
                    limits.lines = value.as_i64();
                    continue;
                }
                "_max_bytes" => {
                    limits.bytes = value.as_i64();
                    continue;
                }
                // 超时是**两用**的：pie 用它杀脚本，脚本自己也常要它（它内部的 `timeout` 之类）
                "_timeout_seconds" => {
                    if let Some(secs) = value.as_i64()
                        && secs > 0
                    {
                        timeout = secs;
                    }
                }
                _ => {}
            }
            payload.insert(key.clone(), value.clone());
            if let Some(text) = scalar_text(value) {
                // 主通道：`--键名=值`（一个参数一个值，不经过 shell）
                argv.push(format!("--{key}={text}"));
                // 兼容通道：`PIE_ARG_<键名>`（键名不是合法标识符就导不出）
                if let Some(name) = env_name(key) {
                    env.push((name, text));
                }
            }
        }
    }
    let state_dir = script.home.join("state").join(&script.name);
    // `PIE_STATE_DIR` 是跨调用状态的唯一去处（短命进程没有内存）：存在即建
    if let Err(e) = std::fs::create_dir_all(&state_dir) {
        crate::log::warn(format!(
            "[shplugin] 建状态目录失败 {}: {e}",
            state_dir.display()
        ));
    }
    env.push(("PIE_HOME".to_string(), script.home.display().to_string()));
    env.push(("PIE_TOOL".to_string(), script.name.clone()));
    env.push(("PIE_STATE_DIR".to_string(), state_dir.display().to_string()));
    if let Some(dir) = script.path.parent() {
        env.push(("PIE_PLUGIN_DIR".to_string(), dir.display().to_string()));
    }
    // 会话身份：脚本要"跟着这次对话"记东西（或开子会话 `pie -s …`）时用它。
    // ⚠ 目录**存在即建** —— 首次 `save` 之前它还不存在，直接给路径脚本会写失败。
    if let Some(dir) = &ctx.session_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            crate::log::warn(format!("[shplugin] 建会话目录失败 {}: {e}", dir.display()));
        }
        env.push(("PIE_SESSION_ID".to_string(), crate::config::name_of(dir)));
        env.push(("PIE_SESSION_DIR".to_string(), dir.display().to_string()));
    }

    let mut child = match spawn_script(&script.path, &env, &argv) {
        Ok(child) => child,
        Err(e) => {
            return Err(ToolError(format!(
                "[{}] 启动失败 {}: {e}",
                script.name,
                script.path.display()
            )));
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        // stdin = 一行 compact JSON：`{"args": {…}, "state": {…}}`（**契约 v3**）。
        // `state` 是本会话该工具上次交回来的那份（没有就是 `{}`）。
        // 脚本可以不读就退出 → 写入拿到 EPIPE，那是「参数没被消费」，**不是失败**。
        let json = json!({
            "args": Value::Object(payload),
            "state": ctx.state.tool_states().get(&script.name).unwrap_or_else(|| json!({})),
        });
        let _ = stdin.write_all(json.to_string().as_bytes()).await;
        let _ = stdin.write_all(b"\n").await;
        drop(stdin);
    }
    let limit = (timeout > 0).then(|| std::time::Duration::from_secs(timeout as u64));
    let ended = match crate::tools::collect(child, limit, ctx.cancel.clone()).await {
        Ok(ended) => ended,
        Err(e) => return Err(ToolError(format!("[{}] 等待脚本失败: {e}", script.name))),
    };
    match ended {
        // stderr 已由 `collect` 并进 output（Unix `dup2`；Windows 两根读完拼接）。
        Ended::Done { output, code } => {
            // 头区里的 `image`（字符串或数组）→ 送给模型的图：与 `read` 读到图同一条投递路
            // （`Session::settle_output` 上传 → `file_id` 挂进这条 tool 消息）。脚本因此不需要
            // 知道 Files API 存在，模型也不必再 `read` 一遍截图。
            let (headers, body) = crate::header::split_header_body(&output);
            // `state` 是**宿主消费的键**：取出来存进会话，再从正文里摘掉 —— 它不是给模型看的
            // （工具的内部记账），留在头区模型每轮都会读到一遍。
            let mut headers = headers;
            let state_values: Vec<Value> = headers
                .iter()
                .filter(|(key, _)| key == "state")
                .map(|(_, value)| value.clone())
                .collect();
            headers.retain(|(key, _)| key != "state");
            if let Some(last) = state_values.last() {
                ctx.state.tool_states().commit(&script.name, last.clone());
            }
            let mut attachments = Vec::new();
            for (_, value) in headers.iter().filter(|(key, _)| key == "image") {
                let paths: Vec<&str> = match value {
                    Value::String(path) => vec![path.as_str()],
                    Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
                    _ => Vec::new(),
                };
                for path in paths {
                    let path = Path::new(path);
                    let mime = match path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.to_ascii_lowercase())
                        .as_deref()
                    {
                        Some("png") => "image/png",
                        Some("jpg" | "jpeg") => "image/jpeg",
                        Some("gif") => "image/gif",
                        Some("webp") => "image/webp",
                        Some("bmp") => "image/bmp",
                        _ => {
                            crate::log::warn(format!(
                                "[shplugin] {}: image={} 不是认识的图片扩展名，不附",
                                script.name,
                                path.display()
                            ));
                            continue;
                        }
                    };
                    match std::fs::metadata(path) {
                        Ok(meta) if meta.is_file() => attachments.push(Attachment::Model {
                            path: path.to_path_buf(),
                            mime: mime.to_string(),
                            size: meta.len(),
                            dim: None,
                        }),
                        _ => crate::log::warn(format!(
                            "[shplugin] {}: image={} 文件不在，不附",
                            script.name,
                            path.display()
                        )),
                    }
                }
            }
            Ok(ToolOutput {
                headers,
                body: ToolBody::fit(&ctx, body, limits),
                attachments,
                status: if code == 0 {
                    Status::Success
                } else {
                    Status::Failed
                },
            })
        }
        Ended::TimedOut { output, secs } => {
            let mut text = format!(
                "[{}] 脚本超过 {secs}s 超时（可能仍在后台跑）：{}",
                script.name,
                script.path.display()
            );
            let tail = crate::tools::tail_lines(&output, 20);
            if !tail.trim().is_empty() {
                text.push_str("\n\n");
                text.push_str(&tail);
            }
            Ok(ToolOutput::text(text).with_status(Status::Timeout { secs }))
        }
        // 与 `bash` 同款：杀进程组、回哨兵文本，上层据此收尾（补全未执行的 tool 消息）
        Ended::Cancelled { .. } => Ok(ToolOutput::text(CANCEL_TEXT).cancelled()),
    }
}

/// 起一个跑脚本的进程（参数走**命令行**：每个顶层标量一个 `--键名=值`，排在脚本路径之后；
/// 数组 / 对象与完整 args 另走 stdin）。
///
/// 脚本**可执行**（有 x 位）就直接 exec → shebang 说了算（「手工 `./tool.sh`」与「pie 跑」
/// 于是是同一条路）；否则用 **`sh`** 跑它（这里**不做** `bash`/`sh` 候选轮询：脚本工具的
/// 目标就是「哪台机器都有 sh」，要 bash 特性就 `chmod +x` 自己写 shebang）。
///
/// 与 `bash` 工具的不同：stdin 接的是 args 的 JSON（不接 pie 的终端）；**stderr 同样并进 stdout**
/// （`grouped` 里的 `dup2`，与 bash 一致——成功时的 stderr 也是正文，不再丢弃）。
///
/// 进程管线**住本模块**（与 `pyhost` 之于 `pyplugin` 同一套分工）：工具层只提供「一个工具长什么样」，
/// 不替每种后端管子进程。
fn spawn_script(
    path: &Path,
    envs: &[(String, String)],
    argv: &[String],
) -> std::io::Result<tokio::process::Child> {
    let configure = |cmd: &mut std::process::Command| {
        cmd.envs(envs.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // 独立进程组 + stderr 并进 stdout（与 bash 同一份 configure）
        crate::tools::grouped(cmd);
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let executable = std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
        if executable {
            let mut cmd = std::process::Command::new(path);
            cmd.args(argv);
            configure(&mut cmd);
            return tokio::process::Command::from(cmd).spawn();
        }
    }
    let mut cmd = std::process::Command::new("sh");
    cmd.arg(path);
    cmd.args(argv);
    configure(&mut cmd);
    tokio::process::Command::from(cmd).spawn()
}

/// 参数值 → 命令行 / 环境变量里的**文本**：只处理顶层标量。
///
/// bool → `1`/`0`（`[ "$V" = 1 ]`）、数字按 JSON 字面量（整数不带 `.0`）、字符串原样；
/// **不给**：数组 / 对象（只能在 stdin 的 JSON 里拿）、含 NUL（argv 和 environ 都装不下）、
/// 超过 4096 字节（环境变量侧会撞 `E2BIG`）。
fn scalar_text(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(true) => "1".to_string(),
        Value::Bool(false) => "0".to_string(),
        _ => return None,
    };
    (text.len() <= 4096 && !text.contains('\0')).then_some(text)
}

/// 键名 → `PIE_ARG_<键名>`：只认 `[A-Za-z_][A-Za-z0-9_]*`（别的名字在 sh 里取不到）。
/// 这类键仍会出现在 argv 与 stdin 里，只是没有环境变量那份。
fn env_name(key: &str) -> Option<String> {
    let mut chars = key.chars();
    if !matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(format!("PIE_ARG_{key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 临时插件目录 + 配置（`Storage::at` 注入，不碰进程级 `PIE_HOME`）。
    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("pie-shplugin-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("plugins")).unwrap();
            Self { dir }
        }

        /// 往插件目录里丢一个脚本（默认不可执行 → 走 `sh <file>`，与用户「拷进来」时一样）。
        fn script(&self, file: &str, body: &str) -> PathBuf {
            let path = self.dir.join("plugins").join(file);
            std::fs::write(&path, body).unwrap();
            path
        }

        fn config(&self) -> Config {
            Config {
                storage: crate::config::Storage::at(&self.dir),
                ..Default::default()
            }
        }

        fn registry(&self, defaults: HashMap<String, toml::Table>) -> Registry {
            load(Registry::empty(defaults), &self.config())
        }

        fn ctx(&self) -> ToolCtx {
            ToolCtx::with_cancel(
                crate::cancel::Cancel::new(),
                crate::config::Storage::at(&self.dir),
                Arc::new(crate::state::SessionState::default()),
            )
            .in_session(&self.dir.join("session"))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// schema 块的位置与写法：shebang 之后、注释区里，JSON 原样（描述可以写成数组）。
    #[test]
    fn schema_block_lives_in_the_comment_run_after_the_shebang() {
        let body = schema_block(
            "#!/bin/sh\n\
             # 一行随便的说明\n\
             # schema! {\n\
             #   \"description\": [\"甲\", \"乙\"],\n\
             #   \"parameters\": {\"type\": \"object\"}\n\
             # }\n\
             # 后面还能有注释\n\
             echo hi\n",
        )
        .expect("该解析出来");
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["description"][1], "乙");

        // 没 shebang / 没块 / 块没闭合 → 都是错（不是「默默不注册」）
        assert!(schema_block("# schema! {\n# }\n").is_err());
        assert!(schema_block("#!/bin/sh\necho hi\n").is_err());
        assert!(schema_block("#!/bin/sh\n# schema! {\n#   \"a\": 1\n").is_err());
        // 块里的 JSON 不合法 → 报的是 JSON 的错
        assert!(
            schema_block("#!/bin/sh\n# schema! {\n# oops\n# }\n")
                .unwrap_err()
                .contains("JSON")
        );
    }

    /// 注册：文件名主干是工具名，描述 / 参数 / 超时都来自 schema 块。
    #[test]
    fn registers_a_script_as_a_tool() {
        let fixture = Fixture::new("register");
        fixture.script(
            "hello.sh",
            "#!/bin/sh\n# schema! {\n#   \"description\": [\"第一行\", \"第二行\"],\n#   \"parameters\": {\"type\": \"object\", \"properties\": {\"who\": {\"type\": \"string\"}}},\n#   \"timeout_seconds\": 7\n# }\necho hi\n",
        );
        let registry = fixture.registry(HashMap::new());
        assert_eq!(registry.names(), ["hello"]);

        let spec = registry
            .tool_specs()
            .into_iter()
            .find(|s| s["function"]["name"] == "hello")
            .expect("hello 的 schema");
        assert_eq!(spec["function"]["description"], "第一行\n第二行");
        assert_eq!(
            spec["function"]["parameters"]["properties"]["who"]["type"],
            "string"
        );
        assert_eq!(spec["function"]["parameters"]["required"], Value::Null);

        // 名字能在 schema 里改
        fixture.script(
            "other.sh",
            "#!/bin/sh\n# schema! {\n#   \"name\": \"renamed\"\n# }\ntrue\n",
        );
        let registry = fixture.registry(HashMap::new());
        assert!(
            registry.names().contains(&"renamed"),
            "{:?}",
            registry.names()
        );
        // 没写 parameters → 发一个空对象（模型至少知道有这么个工具）
        let spec = registry
            .tool_specs()
            .into_iter()
            .find(|s| s["function"]["name"] == "renamed")
            .unwrap();
        assert_eq!(spec["function"]["parameters"]["type"], "object");
    }

    /// **参数走命令行**：每个顶层标量一个 `--键名=值`（bool → `1`/`0`、`null` 不设、`_` 原样带下划线）；
    /// 数组 / 对象只在 stdin 那一行 JSON 里；`PIE_ARG_*` 仍导出（兼容）。
    #[tokio::test]
    async fn args_go_to_argv_and_stdin() {
        let fixture = Fixture::new("args");
        fixture.script(
            "echo.sh",
            "#!/bin/sh\n# schema! {\n#   \"description\": \"回显\"\n# }\n\
             printf 'stdin=%s\\n' \"$(cat)\"\n\
             for a in \"$@\"; do printf 'arg=%s\\n' \"$a\"; done\n\
             printf 'q=%s b=%s n=%s tags=%s cfg=%s tool=%s\\n' \\\n\
               \"$PIE_ARG_query\" \"$PIE_ARG_screenshot\" \"$PIE_ARG_max_items\" \\\n\
               \"${PIE_ARG_tags-<unset>}\" \"$PIE_ARG__hdc\" \"$PIE_TOOL\"\n",
        );
        let mut table = toml::Table::new();
        table.insert("_hdc".to_string(), toml::Value::String("hdc 路径".into()));
        let mut defaults = HashMap::new();
        defaults.insert("echo".to_string(), table);
        let registry = fixture.registry(defaults);
        let ctx = fixture.ctx().named("echo");

        let out = registry
            .dispatch(
                "echo",
                &json!({
                    "query": "两个 词",     // 空格原样过去（不经过 shell）
                    "screenshot": true,     // bool → 1
                    "max_items": 20,        // 数字不带 .0
                    "tags": ["a", "b"],    // 数组只在 stdin 里
                    "skipped": Value::Null, // null = 没给
                }),
                ctx.clone(),
            )
            .await
            .expect("跑通");
        let text = out.to_text();
        assert!(
            text.contains(r#""query":"两个 词""#),
            "stdin 仍是完整 args 的 JSON：{text}"
        );
        assert!(
            text.contains(r#""tags":["a","b"]"#),
            "数组只能从 stdin 拿：{text}"
        );
        assert!(!text.contains("skipped"), "null 不该出现在 stdin：{text}");
        assert!(
            text.contains("arg=--query=两个 词\n"),
            "字符串原样进 argv（不经过 shell）：{text}"
        );
        assert!(text.contains("arg=--screenshot=1\n"), "bool → 1：{text}");
        assert!(
            text.contains("arg=--max_items=20\n"),
            "数字按 JSON 字面量（不带 .0）：{text}"
        );
        assert!(
            text.contains("arg=--_hdc=hdc 路径\n"),
            "私有参数原样带下划线：{text}"
        );
        assert!(
            !text.contains("--tags"),
            "数组只在 stdin，不进 argv：{text}"
        );
        assert!(
            text.contains("q=两个 词 b=1 n=20 tags=<unset> cfg=hdc 路径 tool=echo"),
            "PIE_ARG_* 仍导出（兼容；bool → 1、`_hdc` → `PIE_ARG__hdc`）：{text}"
        );
        // 状态目录：跨调用状态的唯一去处，存在即建
        assert!(
            fixture.dir.join("state/echo").is_dir(),
            "PIE_STATE_DIR 该建出来"
        );
    }

    /// 成败与两条流：stdout 是正文、stderr 并进正文（`2>&1`）、静默失败自动补 `status: failed`。
    #[tokio::test]
    async fn stdout_is_the_body_and_stderr_only_hurts_on_failure() {
        let fixture = Fixture::new("streams");
        fixture.script(
            "ok.sh",
            "#!/bin/sh\n# schema! {\n#   \"description\": \"成功\"\n# }\necho 正文\necho 诊断 1>&2\n",
        );
        fixture.script(
            "bad.sh",
            "#!/bin/sh\n# schema! {\n#   \"description\": \"失败\"\n# }\necho 一半\necho 出错原因 1>&2\nexit 3\n",
        );
        fixture.script(
            "quiet.sh",
            "#!/bin/sh\n# schema! {\n#   \"description\": \"静默失败\"\n# }\nexit 4\n",
        );
        let registry = fixture.registry(HashMap::new());
        let ctx = fixture.ctx();

        let ok = registry
            .dispatch("ok", &json!({}), ctx.clone())
            .await
            .unwrap();
        assert_eq!(
            ok.to_text(),
            "正文\n诊断\n",
            "stderr 现在并进 stdout（2>&1），成功也保留"
        );
        assert_eq!(ok.status, Status::Success);

        let bad = registry
            .dispatch("bad", &json!({}), ctx.clone())
            .await
            .unwrap();
        assert_eq!(bad.to_text(), "一半\n出错原因\n", "失败的 stderr 是理由");
        assert_eq!(bad.status, Status::Failed);

        let quiet = registry
            .dispatch("quiet", &json!({}), ctx.clone())
            .await
            .unwrap();
        assert_eq!(
            quiet.to_text(),
            "header! { \"status\": \"failed\" }",
            "静默失败要补一行 status"
        );
        assert_eq!(quiet.status, Status::Failed);
    }

    /// 首行头区的 `image=<路径>` → `Attachment::Model`（与 `read` 读到图同一条投递路：
    /// `Session::settle_output` 上传后把 `file_id` 挂进 tool 消息，模型不必再 `read` 一遍）。
    /// 不是图片扩展名 / 文件不在的只告警跳过；声明行本身仍留在正文里（模型看得到路径）。
    #[tokio::test]
    async fn image_in_the_header_becomes_an_attachment() {
        let fixture = Fixture::new("image");
        let png = fixture.dir.join("shot.png");
        std::fs::write(&png, b"fake-png").unwrap();
        let missing = fixture.dir.join("missing.png");
        // 头区是 `header! { … }` 块（纯 JSON），`image` 可以是字符串或数组。
        fixture.script(
            "shot.sh",
            &format!(
                "#!/bin/sh\n# schema! {{\n#   \"description\": \"截图\"\n# }}\nprintf 'header! {{ \"shot\": \"%s\", \"image\": [\"%s\", \"%s\"] }}\\n\\n正文\\n' '{}' '{}' '{}'\n",
                png.display(),
                png.display(),
                missing.display()
            ),
        );
        let out = fixture
            .registry(HashMap::new())
            .dispatch("shot", &json!({}), fixture.ctx())
            .await
            .unwrap();

        assert_eq!(
            out.attachments.len(),
            1,
            "不存在的图不附：{:?}",
            out.attachments
        );
        match &out.attachments[0] {
            Attachment::Model {
                path,
                mime,
                size,
                dim,
            } => {
                assert_eq!(path, &png);
                assert_eq!(mime, "image/png");
                assert_eq!(*size, 8);
                assert_eq!(*dim, None, "shplugin 不做魔数嗅探：没有尺寸可给");
            }
            other => panic!("该是 Model：{other:?}"),
        }
        // 头区原样保留（`shot` 与 `image` 都在），正文只剩「正文」
        let text = out.to_text();
        assert!(text.contains("\"shot\""), "{text}");
        assert!(text.contains("\"image\""), "{text}");
        assert!(text.ends_with("正文\n"), "{text}");
        let (headers, body) = crate::header::split_header_body(&text);
        assert!(headers.iter().any(|(k, _)| k == "image"), "{text}");
        assert_eq!(body, "正文\n");
    }

    /// 超时（`_timeout_seconds` 覆盖 schema）与取消都杀掉整个进程组。
    #[tokio::test]
    async fn timeout_and_cancel_kill_the_script() {
        let fixture = Fixture::new("timeout");
        fixture.script(
            "slow.sh",
            "#!/bin/sh\n# schema! {\n#   \"description\": \"慢\"\n# }\nsleep 30\n",
        );
        let mut table = toml::Table::new();
        table.insert("_timeout_seconds".to_string(), toml::Value::Integer(1));
        let mut defaults = HashMap::new();
        defaults.insert("slow".to_string(), table);
        let registry = fixture.registry(defaults);

        let started = std::time::Instant::now();
        let out = registry
            .dispatch("slow", &json!({}), fixture.ctx())
            .await
            .unwrap();
        assert!(out.to_text().contains("超时"), "{}", out.to_text());
        assert_eq!(out.status, Status::Timeout { secs: 1 }, "超时是一等状态");
        assert!(
            started.elapsed().as_secs() < 10,
            "超时路径被卡住了: {:?}",
            started.elapsed()
        );

        // 取消：与 bash 同款回哨兵文本
        let cancel = crate::cancel::Cancel::new();
        let ctx = ToolCtx::with_cancel(
            cancel.clone(),
            crate::config::Storage::at(&fixture.dir),
            Arc::new(crate::state::SessionState::default()),
        )
        .in_session(&fixture.dir.join("session"));
        cancel.cancel();
        let out = registry.dispatch("slow", &json!({}), ctx).await.unwrap();
        assert_eq!(out.status, Status::Cancelled);
        assert!(out.to_text().contains(CANCEL_TEXT), "{}", out.to_text());
    }

    /// 超限走与 bash 同一条路：留头段 + 全文落盘。
    #[tokio::test]
    async fn long_output_is_truncated_and_spilled() {
        let fixture = Fixture::new("spill");
        fixture.script(
            "big.sh",
            "#!/bin/sh\n# schema! {\n#   \"description\": \"很多行\"\n# }\nseq 1 100\n",
        );
        let mut table = toml::Table::new();
        table.insert("_max_lines".to_string(), toml::Value::Integer(2));
        let mut defaults = HashMap::new();
        defaults.insert("big".to_string(), table);
        let registry = fixture.registry(defaults);

        let out = registry
            .dispatch("big", &json!({}), fixture.ctx().named("big"))
            .await
            .unwrap();
        let text = out.to_text();
        assert!(text.contains("\"saved\""), "{text}");
        assert!(text.contains("1\n2\n"), "只留开头：{text}");
        // 尾巴被截掉（注意别拿 “99” 直接查：落盘路径里的临时目录名可能就带着它）
        assert!(
            !text.contains("\n99\n") && !text.contains("\n100\n"),
            "尾巴被截掉：{text}"
        );
        let spill = out.body.spill.expect("落盘路径要结构化带上");
        assert!(std::fs::read_to_string(&spill).unwrap().contains("100"));
    }

    /// 可执行的脚本**直接 exec**（认 shebang 与 x 位），不可执行的走 `sh <文件>`。
    #[cfg(unix)]
    #[tokio::test]
    async fn executable_scripts_honour_their_shebang() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new("shebang");
        let path = fixture.script(
            "direct.sh",
            "#!/bin/sh\n# schema! {\n#   \"description\": \"直接跑\"\n# }\necho \"$0\"\n",
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let registry = fixture.registry(HashMap::new());
        let out = registry
            .dispatch("direct", &json!({}), fixture.ctx())
            .await
            .unwrap();
        assert_eq!(
            out.to_text().trim(),
            path.display().to_string(),
            "$0 该是脚本自己（= 吃了 shebang 的直接 exec）"
        );
    }

    /// 坏文件 / 非法名字 / 重名 → 只跳过那一条，注册表原样。
    #[test]
    fn broken_scripts_are_skipped() {
        let fixture = Fixture::new("broken");
        // 没有 schema 块
        fixture.script("no_schema.sh", "#!/bin/sh\necho hi\n");
        // schema 不是合法 JSON
        fixture.script("bad_json.sh", "#!/bin/sh\n# schema! {\n# nope\n# }\n");
        // 非法名字
        fixture.script(
            "ugly.sh",
            "#!/bin/sh\n# schema! {\n#   \"name\": \"有中文\"\n# }\n",
        );
        // 与内置工具重名
        fixture.script("read.sh", "#!/bin/sh\n# schema! {\n# }\n");
        // 好的那个照样进
        fixture.script("fine.sh", "#!/bin/sh\n# schema! {\n# }\n");

        let registry = load(Registry::new(HashMap::new()), &fixture.config());
        let names = registry.names();
        assert_eq!(
            names.iter().filter(|n| **n == "read").count(),
            1,
            "内置 read 不能被顶掉：{names:?}"
        );
        assert!(names.contains(&"fine"), "{names:?}");
        assert!(!names.contains(&"no_schema"));
        assert!(!names.contains(&"bad_json"));
        assert!(!names.contains(&"有中文"));
    }

    #[test]
    fn scalar_text_and_env_name_follow_the_contract() {
        // 文本：bool → 1/0、数字按 JSON 字面量、字符串原样
        assert_eq!(scalar_text(&json!("a b")).as_deref(), Some("a b"));
        assert_eq!(scalar_text(&json!(20)).as_deref(), Some("20"));
        assert_eq!(scalar_text(&json!(true)).as_deref(), Some("1"));
        assert_eq!(scalar_text(&json!(false)).as_deref(), Some("0"));
        assert_eq!(scalar_text(&json!(["a"])), None, "数组只走 stdin");
        assert_eq!(scalar_text(&json!({"a": 1})), None, "对象只走 stdin");
        assert_eq!(scalar_text(&json!("x".repeat(4097))), None, "防 E2BIG");
        assert_eq!(
            scalar_text(&json!("a\u{0}b")),
            None,
            "NUL 装不进 argv/environ"
        );

        // 环境变量名：只认标识符（argv 不受这条限制）
        assert_eq!(env_name("x").as_deref(), Some("PIE_ARG_x"));
        assert_eq!(env_name("_file").as_deref(), Some("PIE_ARG__file"));
        assert_eq!(env_name("有中文"), None);
        assert_eq!(env_name("a-b"), None);
    }

    /// 仓库里的示例脚本得一直能被加载（schema 块写错 = 模型那边就少一个工具，而且还不好发现）。
    ///
    /// 扫 `examples/*.sh` 而不是列文件名：加一个示例不该还要回来改测试。
    #[test]
    fn repo_examples_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).expect("examples/ 要存在") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("sh") {
                continue;
            }
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            let spec = read_spec(&path).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert!(!spec.description.is_empty(), "{file} 没写 description");
            assert_eq!(spec.parameters["type"], "object", "{file} 的参数不是对象");
            seen += 1;
        }
        assert!(seen > 0, "examples/ 里至少得有一个 .sh 示例");

        let spec = read_spec(&dir.join("ohos_vision.sh")).unwrap();
        assert_eq!(spec.name, "ohos_vision", "名字默认取文件名主干");
        assert_eq!(spec.timeout, 120, "schema 里的 timeout_seconds 要生效");
    }
    /// `state` 走**纯函数模型**：stdin 的 `{"args", "state"}` 进、头区 `state` 出 —— 宿主消费掉，
    /// 模型看不到（那是工具的内部记账），第二次调用读得到上次写的那份。
    #[tokio::test]
    async fn state_round_trips_through_stdin_and_the_header() {
        let fixture = Fixture::new("state");
        fixture.script(
            "counter.sh",
            "#!/bin/sh\n# schema! {\n#   \"description\": \"数数\"\n# }\n\
             seen=$(cat)\n\
             printf 'header! { \"state\": {\"n\": 2} }\\n\\n'\n\
             printf 'stdin=%s\\n' \"$seen\"\n",
        );
        let registry = fixture.registry(HashMap::new());
        let state = Arc::new(crate::state::SessionState::default());
        let ctx = || {
            ToolCtx::with_cancel(
                crate::cancel::Cancel::new(),
                crate::config::Storage::at(&fixture.dir),
                state.clone(),
            )
            .in_session(&fixture.dir.join("session"))
        };

        // 第一次：stdin 里的 state 是空的；交回来的那份落进会话槽
        let out = registry
            .dispatch("counter", &json!({"q": "x"}), ctx())
            .await
            .unwrap();
        // `state` 被宿主摘掉 → 头区空（脚本自己把 stdin 打出来是它的事，见下面那条）
        let text = out.to_text();
        let (headers, body) = crate::header::split_header_body(&text);
        assert!(
            headers.is_empty(),
            "state 是宿主消费的键，头区该是空的：{headers:?}"
        );
        assert!(body.contains("\"args\":{\"q\":\"x\"}"), "{body}");
        assert_eq!(state.tool_states().get("counter"), Some(json!({"n": 2})));

        // 第二次：stdin 里带上了上次的 state（脚本把 stdin 原样打出来，所以看得到）
        let out = registry
            .dispatch("counter", &json!({}), ctx())
            .await
            .unwrap();
        let text = out.to_text();
        let (_, body) = crate::header::split_header_body(&text);
        assert!(
            body.contains("\"state\":{\"n\":2}"),
            "第二次的 stdin 该带上上次交回去的 state：{body}"
        );
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    /// `PIE_SESSION_ID` / `PIE_SESSION_DIR`：脚本要知道"这是哪个会话"（开子会话、写会话级文件）。
    /// ⚠ 目录**存在即建** —— 首次 `save` 之前它本来不存在。
    #[tokio::test]
    async fn session_identity_is_exported_and_the_dir_exists() {
        let fixture = Fixture::new("session-env");
        fixture.script(
            "who.sh",
            "#!/bin/sh\n# schema! {\n#   \"description\": \"报身份\"\n# }\n\
             printf 'id=%s\\n' \"$PIE_SESSION_ID\"\n\
             [ -d \"$PIE_SESSION_DIR\" ] && echo dir=yes || echo dir=no\n",
        );
        let registry = fixture.registry(HashMap::new());
        let out = registry
            .dispatch("who", &json!({}), fixture.ctx())
            .await
            .unwrap();
        let session_dir = fixture.dir.join("session");
        assert!(
            out.to_text()
                .contains(&format!("id={}", crate::config::name_of(&session_dir))),
            "{}",
            out.to_text()
        );
        assert!(out.to_text().contains("dir=yes"), "{}", out.to_text());
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }
}
