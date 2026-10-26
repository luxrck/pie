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
//!   - **参数走命令行**（`--名字=值`）：一条调用在日志 / `ps` / `set -x` 里**自解释**，
//!     照抄就能手工复现 —— 这正是 shell 工具好在哪里的地方（`docs/sh-plugins.md` §3 有与
//!     「走环境变量」的权衡）；
//!   - **私有参数（`_` 开头）走环境变量**（`PIE_CFG_<名字>`）：那是**站点配置**，不是这次调用
//!     的输入，本来就该在进程环境里；模型也看不见它们（`_` 不进 schema）；
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
    CallFn, Limits, Registry, ToolBody, ToolCtx, ToolError, ToolOutput, ToolResult, valid_tool_name,
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
    /// `PIE_HOME`：数据根（脚本要往 `files/` 放产物时用 —— 那是 `pie files gc` 认得的位置）。
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
/// 分拣规则只有一条：**`_` 开头的是站点配置**（→ `PIE_CFG_*` 环境变量），其余是这次调用的
/// 输入（→ `--名字=值`）；`_max_lines` / `_max_bytes` 是宿主旋钮（两边都不进）。
async fn run(script: &Script, args: Value, ctx: ToolCtx) -> ToolResult {
    let mut argv: Vec<String> = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();
    let mut limits = Limits::default();
    let mut timeout = script.timeout;
    if let Some(map) = args.as_object() {
        for (key, value) in map {
            // `null` = 模型显式说「没有」→ 当没给（让脚本自己的默认值说话）
            if value.is_null() {
                continue;
            }
            let text = as_text(value);
            match key.as_str() {
                "_max_lines" => limits.lines = value.as_i64(),
                "_max_bytes" => limits.bytes = value.as_i64(),
                // 超时是**两用**的：pie 用它杀脚本，脚本自己也常要它（它内部的 `timeout` 之类）
                "_timeout_seconds" => {
                    if let Some(secs) = value.as_i64()
                        && secs > 0
                    {
                        timeout = secs;
                    }
                    env.push((cfg_env(key), text));
                }
                _ if key.starts_with('_') => env.push((cfg_env(key), text)),
                _ => argv.push(format!("--{key}={text}")),
            }
        }
    }
    env.push(("PIE_HOME".to_string(), script.home.display().to_string()));
    env.push(("PIE_TOOL_NAME".to_string(), script.name.clone()));

    let child = match spawn_script(&script.path, &argv, &env) {
        Ok(child) => child,
        Err(e) => {
            return Err(ToolError(format!(
                "[{}] 启动失败 {}: {e}",
                script.name,
                script.path.display()
            )));
        }
    };
    match wait_script(child, timeout, ctx.cancel.clone()).await {
        // 成功时 stderr 是噪声（脚本自己的诊断）→ 丢掉；失败时它才是给模型的理由。
        Ok(Ended::Exited {
            out,
            err: diag,
            code,
        }) => {
            let mut body = out;
            let mut headers = Vec::new();
            if code != 0 {
                if !diag.trim().is_empty() {
                    if !body.is_empty() && !body.ends_with('\n') {
                        body.push('\n');
                    }
                    body.push_str(&diag);
                }
                // 失败却**什么都没说**（`test -f x` 这种）→ 补一行码，否则模型只拿到空串
                if body.trim().is_empty() {
                    headers.push(format!("[exit={code}]"));
                }
            }
            Ok(ToolOutput {
                headers,
                body: ToolBody::fit(&ctx, &body, limits),
                attachments: Vec::new(),
                failed: code != 0,
            })
        }
        Ok(Ended::TimedOut { secs }) => Ok(ToolOutput::text(format!(
            "[{}] 脚本超过 {secs}s 超时（可能仍在后台跑）：{}",
            script.name,
            script.path.display()
        ))
        .failed()),
        // 与 `bash` 同款：杀进程组、回哨兵文本，上层据此收尾（补全未执行的 tool 消息）
        Ok(Ended::Cancelled) => Ok(ToolOutput::text(CANCEL_TEXT)),
        Err(e) => Err(ToolError(format!("[{}] 等待脚本失败: {e}", script.name))),
    }
}

/// 脚本的结局（与 `bash` 工具的「跑完 / 超时 / 取消」三态同构，但**这里自己实现**：
/// 那个是「一条命令、一条流、`bash -c`」，这个是「一个脚本文件、两根流、exec」）。
enum Ended {
    Exited { out: String, err: String, code: i32 },
    TimedOut { secs: i64 },
    Cancelled,
}

/// 起一个跑脚本的进程：参数按位置传（**不经过 `-c`**，所以空格 / 引号 / 换行不会被再解释一遍）。
///
/// 脚本**可执行**（有 x 位）就直接 exec → shebang 说了算（「手工 `./tool.sh`」与「pie 跑」
/// 于是是同一条路）；否则用 **`sh`** 跑它（这里**不做** `bash`/`sh` 候选轮询：脚本工具的
/// 目标就是「哪台机器都有 sh」，要 bash 特性就 `chmod +x` 自己写 shebang）。
///
/// 与 `bash` 工具的两点不同（都是脚本工具需要的）：stdin 给 `/dev/null`（脚本不该吃到 pie 的
/// 终端）、stderr **不并进** stdout（诊断与正文分开，成功时那些噪声才丢得掉）。
///
/// 进程管线**住本模块**（与 `pyhost` 之于 `pyplugin` 同一套分工）：工具层只提供「一个工具长什么样」，
/// 不替每种后端管子进程。
fn spawn_script(
    path: &Path,
    args: &[String],
    envs: &[(String, String)],
) -> std::io::Result<tokio::process::Child> {
    let configure = |cmd: &mut std::process::Command| {
        cmd.args(args)
            .envs(envs.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // 独立进程组：取消 / 超时按**组**杀（否则脚本起的子孙抱着管道写端，等待会被卡到它们退出）
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let executable = std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
        if executable {
            let mut cmd = std::process::Command::new(path);
            configure(&mut cmd);
            return tokio::process::Command::from(cmd).spawn();
        }
    }
    let mut cmd = std::process::Command::new("sh");
    cmd.arg(path);
    configure(&mut cmd);
    tokio::process::Command::from(cmd).spawn()
}

/// 等一个脚本跑完：收齐 stdout / stderr，超时与取消都**杀整个进程组**。
///
/// 两根管道都得读（只读一根，另一根写满就把脚本堵住）；`timeout <= 0` 表示不限时。
/// 杀组用的原语只有 `cancel::kill_group` 那一份，退出码口径也只有 `tools::exit_code` 那一份。
async fn wait_script(
    mut child: tokio::process::Child,
    timeout: i64,
    cancel: Option<crate::cancel::Cancel>,
) -> std::io::Result<Ended> {
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    // tokio 的 id() 在进程已被回收后返回 None；0 表示“拿不到 pid”（不杀组）
    let pid = child.id().unwrap_or(0);
    let kill_group = || crate::cancel::kill_group(pid, crate::cancel::SIGKILL);

    let read_and_wait = async {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (_, _, status) = tokio::join!(
            drain(stdout, &mut out),
            drain(stderr, &mut err),
            child.wait(),
        );
        status.map(|status| Ended::Exited {
            out: String::from_utf8_lossy(&out).into_owned(),
            err: String::from_utf8_lossy(&err).into_owned(),
            code: crate::tools::exit_code(&status),
        })
    };
    tokio::pin!(read_and_wait);

    // 取消（`Esc`）：`notify_waiters` 不补发 → 先查标志再 await（见 `cancel`）
    let cancel_waiter = async {
        match cancel {
            Some(cancel) => cancel.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(cancel_waiter);

    if timeout > 0 {
        let limit = std::time::Duration::from_secs(timeout as u64);
        tokio::select! {
            waited = tokio::time::timeout(limit, &mut read_and_wait) => match waited {
                Ok(done) => done,
                Err(_) => {
                    kill_group();
                    Ok(Ended::TimedOut { secs: timeout })
                }
            },
            _ = &mut cancel_waiter => {
                kill_group();
                Ok(Ended::Cancelled)
            }
        }
    } else {
        tokio::select! {
            done = &mut read_and_wait => done,
            _ = &mut cancel_waiter => {
                kill_group();
                Ok(Ended::Cancelled)
            }
        }
    }
}

/// 读完一根管道（读失败就当读了一半：被杀的子进程本来就会提前关管道）。
async fn drain<R: tokio::io::AsyncRead + Unpin>(mut reader: R, buf: &mut Vec<u8>) {
    use tokio::io::AsyncReadExt;
    let _ = reader.read_to_end(buf).await;
}

/// 参数值 → 命令行 / 环境变量里的**字符串**：字符串原样，其余按 JSON 字面量
/// （`20` / `true` / `["a","b"]`）—— 一条规矩，脚本不用猜类型。
fn as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// 私有参数 `_hdc` → 环境变量 `PIE_CFG_hdc`（只去**一个**前导下划线：`__x` → `PIE_CFG__x`）。
fn cfg_env(key: &str) -> String {
    format!("PIE_CFG_{}", key.strip_prefix('_').unwrap_or(key))
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

    /// **参数走命令行**：`--名字=值`，只带这次真的给了的；`_` 开头的走环境变量。
    #[tokio::test]
    async fn args_go_to_argv_and_private_ones_to_env() {
        let fixture = Fixture::new("argv");
        fixture.script(
            "echo.sh",
            "#!/bin/sh\n# schema! {\n#   \"description\": \"回显\"\n# }\n\
             for arg in \"$@\"; do printf '[%s]' \"$arg\"; done\n\
             printf 'cfg=%s home=%s name=%s\\n' \"$PIE_CFG_hdc\" \"$PIE_HOME\" \"$PIE_TOOL_NAME\"\n",
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
                    "query": "两个 词",          // 空格原样过去（argv 不经过 shell）
                    "screenshot": true,
                    "max_items": 20,
                    "tags": ["a", "b"],          // 数组按 JSON 字面量给
                    "skipped": Value::Null,      // null = 没给
                    "unset": "x"
                }),
                ctx.clone(),
            )
            .await
            .expect("跑通");
        let text = out.to_text();
        for want in [
            "[--query=两个 词]",
            "[--screenshot=true]",
            "[--max_items=20]",
            "[--tags=[\"a\",\"b\"]]",
            "[--unset=x]",
        ] {
            assert!(text.contains(want), "缺 {want}：{text}");
        }
        assert!(!text.contains("--skipped"), "null 不该变成参数：{text}");

        // 环境事实：数据根 + 工具名（**没有**状态目录 —— 要状态就自己往 `$PIE_HOME` 里写）
        let want = format!("cfg=hdc 路径 home={} name=echo", fixture.dir.display());
        assert!(text.contains(&want), "该是 {want}：{text}");
        assert!(
            !text.contains("PIE_STATE_DIR"),
            "不该有 PIE_STATE_DIR：{text}"
        );
    }

    /// 成败与两条流：stdout 是正文、stderr 只在失败时补进来、静默失败补一行 `[exit=N]`。
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
        assert_eq!(ok.to_text(), "正文\n", "成功的 stderr 是噪声，丢掉");
        assert!(!ok.failed);

        let bad = registry
            .dispatch("bad", &json!({}), ctx.clone())
            .await
            .unwrap();
        assert_eq!(bad.to_text(), "一半\n出错原因\n", "失败的 stderr 是理由");
        assert!(bad.failed);

        let quiet = registry
            .dispatch("quiet", &json!({}), ctx.clone())
            .await
            .unwrap();
        assert_eq!(quiet.to_text(), "[exit=4]", "静默失败要补一行码");
        assert!(quiet.failed);
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
        assert!(out.failed);
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
        );
        cancel.cancel();
        let out = registry.dispatch("slow", &json!({}), ctx).await.unwrap();
        assert_eq!(out.to_text(), CANCEL_TEXT);
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
        assert!(text.contains("[工具输出全文已保存: "), "{text}");
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
    fn private_keys_become_env_names() {
        assert_eq!(cfg_env("_hdc"), "PIE_CFG_hdc");
        assert_eq!(cfg_env("__x"), "PIE_CFG__x");
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
}
