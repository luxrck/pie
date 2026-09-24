//! pie 的 Rust 重构 —— 极简 agent harness（YOLO，内置 read / edit / write / shell）。
//!
//! 分阶段迁移进度（详见 pie-rs/README.md）：
//!   ✅ config  —— 配置加载 + 分层 system prompt
//!   ✅ llm     —— reqwest + 手写 SSE 的 OpenAI 兼容客户端（不含任何 SDK）
//!   ✅ tools   —— read / edit / write / shell（统一 Headers\n\nBody 输出）
//!   ✅ session —— JSONL 持久化（`~/.pie/sessions/`，与 Python 同格式）+ resume + **回合循环**（原 loop.rs 并入）
//!   ✅ cli     —— 一次性模式（子 agent）；`-t/--tools`、`-r/--resume`、`-s/--session`
//!   ✅ context —— 三级压缩（工具/轮次/会话级） + 落盘指针 + manifest + `context info|verify|gc`
//!   ✅ 图片   —— read 图 → Files API 上传（`file` 块注入，不回退内联）+ `files list|gc`
//!   ⬜ 并行工具 / TUI

// 这里换成对 lib 的引用（核心层已提成 `pie` 库，CLI 只是它的一个消费者）。
// ⚠ 模块声明在 `src/lib.rs`，别在这里再写 `mod xxx;`——那会变成两份独立的编译单元。
use std::io::{IsTerminal, Write};

use clap::{Parser, Subcommand};
use serde_json::Value;

use pie::llm::LlmClient;
use pie::session::{Session, TurnEvent};
use pie::tools::{tools_from_spec, ToolRegistry};
use pie::{cancel, config, context, session, tui};

/// 一次性模式的输出格式（对齐 Python `--mode {text,json,transcript}`）。
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// 只输出答案本身（stdout 能被 shell 直接接住）
    Text,
    /// 一行 JSON：`answer` / `session` / `turns` / `usage`
    Json,
    /// 完整历史（`full_history()`）的 JSON，缩进 1
    Transcript,
}

/// `-t/--thinking` 的合法值（对齐 Python `cli.py` 的 `THINKING_LEVELS`）。
///
/// Python 用的是 `off`（内部才归一到配置口径 `none`）；这里**两个都收** ——
/// 配置文件与 `/thinking` 里写的就是 `none`，顺手敲 `-t none` 也应该能用。
const THINKING_LEVELS: [&str; 8] = [
    "off", "none", "minimal", "low", "medium", "high", "xhigh", "max",
];

#[derive(Parser, Debug)]
#[command(name = "pie-rs", version, about = "pie 的 Rust 重构")]
struct Cli {
    /// 指定配置文件（默认 ~/.pie/config.toml）
    #[arg(short = 'c', long = "config")]
    config: Option<std::path::PathBuf>,

    /// 本次运行的模型（覆盖配置，不持久化）
    #[arg(short = 'm', long)]
    model: Option<String>,

    /// 本次思考强度（off / none / low / medium / high / xhigh / max；`off` = `none`，覆盖配置，不持久化）
    #[arg(short = 't', long, value_parser = THINKING_LEVELS)]
    thinking: Option<String>,

    /// 每次请求为输出预留的 token（即 API 的 max_tokens；例：131072 / 128k / auto；覆盖配置）
    #[arg(long, alias = "max-tokens", value_name = "N")]
    reserved_tokens: Option<String>,

    /// 单回合最多问几次模型（**本次运行**的旋钮，不写进配置；对齐 Python `loop.aturn(max_steps=…)`）
    #[arg(long)]
    max_steps: Option<usize>,

    /// 强制非流式（一次性 `complete`；同样只是本次运行的旋钮）
    #[arg(long)]
    no_stream: bool,

    /// 上下文 token 估算超过该值即自动压缩（覆盖软阈值，不持久化）
    #[arg(long)]
    auto_compact_threshold: Option<usize>,

    /// HTTP 超时秒数（覆盖配置）
    #[arg(long)]
    timeout_seconds: Option<f64>,

    /// 请求重试次数（覆盖配置）
    #[arg(long)]
    max_retries: Option<usize>,

    /// 重试等待上限秒数（覆盖配置）
    #[arg(long)]
    max_retry_delay_seconds: Option<f64>,

    /// 内置工具的工作目录（默认当前目录）
    #[arg(long)]
    cwd: Option<std::path::PathBuf>,

    /// 替换默认基础提示（SYSTEM.md）：字面文本或 UTF-8 文件路径
    #[arg(long)]
    system_prompt: Option<String>,

    /// 追加到 system prompt（可重复；字面文本或 UTF-8 文件路径）
    #[arg(long = "append-system-prompt", action = clap::ArgAction::Append)]
    append_system_prompt: Vec<String>,

    /// 列出端点可用的模型 id
    #[arg(long)]
    models: bool,

    /// 限制可用工具（逗号分隔）：内置名（read/edit/writ/bash）启用该工具，其他名字当 shell 子命令白名单
    /// （如 `--tools read,ls,grep` = read + 只允许 ls/grep 的受限 bash）
    #[arg(long)]
    tools: Option<String>,

    /// 恢复最近的会话继续（同工作目录优先）；不写这个就跑一次性模式（不落盘）
    #[arg(short = 'r', long = "resume")]
    resume: bool,

    /// 指定会话 id 或路径（已存在则载入，否则新建）
    #[arg(short = 's', long = "session")]
    session: Option<String>,

    /// 一次性模式的输出格式（对齐 Python 的 `--mode`）：`text` = 只输出答案（默认）；
    /// `json` = 一行 JSON：`answer/session/turns/usage`；`transcript` = 完整历史 JSON
    #[arg(long, value_name = "MODE", default_value = "text")]
    mode: Mode,

    /// 结束时把 `/stat` 报告打到 **stderr**（上下文占用 / 水位 / 压缩事件 / API 用量）
    #[arg(long)]
    stat: bool,

    /// 一次性任务（子 agent 模式；不写则从 stdin 读，stdin 也空才打印帮助）
    task: Vec<String>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 生成默认配置文件与全局记忆（已存在则原样保留）
    Setup,
    /// 上下文压缩维护
    Context {
        #[command(subcommand)]
        action: ContextAction,
    },
    /// 图片上传件维护
    Files {
        #[command(subcommand)]
        action: FilesAction,
    },
    /// 列出历史会话
    Sessions {
        /// 最多列出 N 个（默认 20）
        #[arg(short = 'l', long, default_value_t = 20)]
        limit: usize,
        /// 列出全部会话（忽略 --limit）
        #[arg(long)]
        all: bool,
        /// 输出 JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum FilesAction {
    /// 列出图片记录（默认：本地会话记录；加 --all 改列云端）
    List {
        /// 调 Files API 列出服务端本账号的全部上传件
        #[arg(long)]
        all: bool,
    },
    /// 回收本地副本（加 --delete 真删；加 --all 另清空云端）
    Gc {
        /// 真正删除未引用副本
        #[arg(long)]
        delete: bool,
        /// 调 Files API 清空服务端本账号的全部上传件
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ContextAction {
    /// 列出所有压缩事件
    Info,
    /// 校验 manifest 引用的原文文件是否存在
    Verify,
    /// 列出 / 删除未被引用的 context 文件
    Gc {
        /// 真正删除未引用文件
        #[arg(long)]
        delete: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let code = rt.block_on(run(cli));
    std::process::exit(code);
}

async fn run(cli: Cli) -> i32 {
    // `pie setup`：只补缺的默认文件，**不读也不改**已有配置（配置缺失/坏掉正是它要处理的情形），
    // 所以要在 `Config::load` 之前、也在启动时那发记忆种子之前接住它。
    if matches!(cli.command.as_ref(), Some(Cmd::Setup)) {
        return setup_main(&cli);
    }

    // 首次运行写入全局记忆的种子文件（已存在则跳过）：对齐 Python 每次启动先
    // `_ensure_global_memory()`——它会被拼进 system prompt，没文件就白白少一层记忆。
    config::ensure_global_memory();
    let mut config = match config::Config::load(cli.config.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    // 一次性覆盖（`-m` / `-t` / `--reserved-tokens` …）——只改内存里的 config，不写回文件
    if let Err(e) = apply_overrides(&mut config, &cli) {
        eprintln!("{e}");
        return 2;
    }
    // `--cwd`：工具的工作目录（默认当前目录）
    if let Some(cwd) = &cli.cwd {
        if let Err(e) = std::env::set_current_dir(cwd) {
            eprintln!("切到工作目录失败 {}: {e}", cwd.display());
            return 1;
        }
    }

    // 维护类子命令（不需要模型）；files 要 config（建 Files API 客户端）
    if let Some(cmd) = &cli.command {
        return match cmd {
            // 正常走不到（`run()` 开头已提前返回）——但这条臂留着：万一提前返回被挪掉，
            // `setup` 依旧是对的（它对已有文件一律不动，与启动时那发种子同级）。
            Cmd::Setup => setup_main(&cli),
            Cmd::Context { action } => context_main(action),
            Cmd::Files { action } => files_main(&config, action).await,
            Cmd::Sessions { limit, all, json } => {
                sessions_main(if *all { None } else { Some(*limit) }, *json)
            }
        };
    }

    if cli.models {
        return list_models(&config).await;
    }

    // 任务：位置参数拼起来；没给就从 stdin 读（`echo "任务" | pie-rs`）；都空才打印帮助
    let mut task = cli.task.join(" ");
    if task.trim().is_empty() && !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        if std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).is_ok() {
            task = buf;
        }
    }
    let session_mode = cli.resume || cli.session.is_some();

    // 两个**按次**的执行旋钮（不在配置里，对齐 Python `loop.aturn` 的形参）：
    // `--max-steps` / `--no-stream` 直接传给每个回合（含 TUI 里的回合）。
    let max_steps = cli.max_steps;
    let stream = cli.no_stream.then_some(false);

    // 客户端与工具集（TUI / 会话 / 一次性三种模式共用；`--tools` 裁出来的工具集就从这里进）
    let client = match LlmClient::new(&config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let registry = tools_from_spec(cli.tools.as_deref(), config.tool_defaults());

    // TTY 且没有任务 → 进 TUI（`-r` / `-s` 就接着那个会话聊）；非 TTY 保持原来的行为
    if task.trim().is_empty() && std::io::stdin().is_terminal() {
        let session = if session_mode {
            match open_session(&config, cli.resume, cli.session.as_deref(), client, registry) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            }
        } else {
            Session::new(&config, None, client, registry)
        };
        return match tui::run(session, max_steps, stream).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("TUI 启动失败: {e}");
                1
            }
        };
    }

    if task.trim().is_empty() && !session_mode {
        // 非 TTY 且没给任务：对齐 Python（一行到 stderr + 退出码 2），别污染 stdout 管道
        eprintln!(
            "pie-rs {}（配置 {}）",
            config.model,
            config.config_file
                .as_deref()
                .unwrap_or(std::path::Path::new("(默认)"))
                .display()
        );
        eprintln!(
            "上下文窗口 {} tokens，可用输入预算 {}（为输出预留 {}）",
            config.context_window,
            config.context_budget(),
            config.reserved_tokens.unwrap_or(0)
        );
        eprintln!("请提供任务描述（`pie-rs \"任务\"`，或从 stdin 传入）；真实终端里不带任务运行会进 TUI");
        return 2;
    }

    // 一次性模式：与 Python 的 print 模式一个口径 —— **stdout 只放结果**，工具活动／步骤
    // 与调试日志都不往 stderr 写（`--mode text` 时 stdout = 答案本身，shell 能直接接住；
    // `stream` 只影响到达时间）。
    let mode = cli.mode;
    let mut printer = move |ev: TurnEvent| match ev {
        TurnEvent::AssistantText(delta) => {
            if mode == Mode::Text {
                print!("{delta}");
                let _ = std::io::stdout().flush();
            }
        }
        TurnEvent::Reasoning(_) => {}
        // 非流式（`--no-stream`）时的最终答复：流式下走 AssistantText 增量，这里不会来
        TurnEvent::Answer(text) => {
            if mode == Mode::Text {
                println!("{text}");
            }
        }
        // 工具活动不打印（stdout 只放结果；曾经的 `[tNsM]` / `[tool] …←…` 日志是 verbose 门控的，
        // 已随 `Config.verbose` 一起删除）
        TurnEvent::ToolCall { .. } | TurnEvent::ToolResult { .. } => {}
    };

    // 会话模式（--resume / --session）：读写 `~/.pie/sessions/`，跑完落盘；一次性模式不碰磁盘。
    if session_mode {
        let mut session =
            match open_session(&config, cli.resume, cli.session.as_deref(), client, registry) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
        if task.trim().is_empty() {
            // 只恢复/建个文件：打印状态即可（交互式 REPL 属 TUI 那一块）
            eprintln!(
                "[session] {}（{}）",
                session.path.display(),
                session.summary()
            );
            if cli.stat {
                eprintln!("{}", session.usage_report());
            }
            return match session.save() {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            };
        }
        let answer = match session
            // `parallel_tools: None` = 跟随配置（CLI 没有覆盖它的旗标，对齐 Python）
            .aturn(
                &task,
                &mut printer,
                &cancel::Cancel::new(),
                max_steps,
                stream,
                None,
            )
            .await
        {
            Ok(answer) => answer,
            Err(e) => {
                eprintln!("\n{e}");
                // 失败也把历史写回去：`aturn` 已经补了一条 `[请求失败] <错误>` 的 assistant，落盘
                // 才留得住（否则这次提问连同失败原因一起消失，下次 resume 莫名其妙）。写不进去
                // 就算了——失败原因可能正是磁盘 / 权限；退出码仍是 1。
                let _ = session.save();
                return 1;
            }
        };
        dump_result(&session, &answer, mode, cli.stat, stream);
        if let Err(e) = session.save() {
            eprintln!("{e}");
            return 1;
        }
        return 0;
    }

    // 一次性模式：临时会话（不落盘、不写 manifest），跑完就扔；
    // 回合循环现在就在 `Session::aturn` 里（原 loop.rs 已并入）；
    // `--system-prompt` / `--append-system-prompt` 已通过 `apply_overrides` 进了 config
    let mut session = Session::ephemeral(&config, client, registry);
    match session
        .aturn(
            &task,
            &mut printer,
            &cancel::Cancel::new(),
            max_steps,
            stream,
            None,
        )
        .await
    {
        Ok(answer) => {
            dump_result(&session, &answer, mode, cli.stat, stream);
            0
        }
        Err(e) => {
            eprintln!("\n{e}");
            1
        }
    }
}

/// `pie setup`：把 `~/.pie/` 下缺的默认件补齐 —— 默认配置文件 + 全局记忆种子。
///
/// **非交互**（Python 版那个会逐个问模型 / 地址 / key；这边只写默认值，之后自己改），
/// 已存在的文件**一律不覆盖**（里面可能有用户自己的 key 与记忆）→ 重复跑安全、幂等。
fn setup_main(cli: &Cli) -> i32 {
    let mut code = 0;
    let mut created_config = false;

    match config::ensure_config_file(cli.config.as_deref()) {
        Ok((path, created)) => {
            created_config = created;
            let label = if created { "已创建" } else { "已存在" };
            println!("配置文件  {label}  {}", path.display());
        }
        Err(e) => {
            eprintln!("{e}");
            code = 1;
        }
    }
    match config::ensure_global_memory_file() {
        Ok((path, created)) => {
            let label = if created { "已创建" } else { "已存在" };
            println!("全局记忆  {label}  {}", path.display());
        }
        Err(e) => {
            eprintln!("写入全局记忆失败（{}）: {e}", config::global_memory_file().display());
            code = 1;
        }
    }

    if code != 0 {
        return code;
    }
    if created_config {
        println!("接着：按需改上面的 model / base_url / api_key，再跑 `pie \"任务\"`。");
    } else {
        println!("两份文件都在，未改动。");
    }
    0
}

/// `pie-rs sessions`：列出历史会话（文案对齐 Python `pie sessions`）。
fn sessions_main(limit: Option<usize>, json: bool) -> i32 {
    let rows = session::list_sessions(limit);
    if json {
        let values: Vec<Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.id,
                    "file": r.path.display().to_string(),
                    "mtime": r.mtime,
                    "size": r.size,
                    "turns": r.turns,
                    "api_calls": r.api_calls,
                    "first_query": r.first_query,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&values).unwrap_or_default()
        );
        return 0;
    }
    if rows.is_empty() {
        println!("暂无会话（{}）", session::sessions_dir().display());
        return 0;
    }
    for r in rows {
        println!(
            "{}  turns={}  api_calls={}  {}",
            r.id,
            r.turns,
            r.api_calls,
            config::fmt_local(r.mtime)
        );
        if r.first_query.is_empty() {
            println!("    ↳ (无用户消息)");
        } else {
            // 压成单行、超 80 字截断（Python 同款）
            let one_line: String = r
                .first_query
                .chars()
                .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                .collect();
            let disp: String = if one_line.chars().count() > 80 {
                format!("{}…", one_line.chars().take(80).collect::<String>())
            } else {
                one_line
            };
            println!("    ↳ {disp}");
        }
    }
    0
}

/// 一次性覆盖（CLI 参数 → 内存里的 config，**不写回文件**）。
/// 一次性模式跑完后要往 **stdout** 写的那段文本（`None` = 什么都不用写）。
///
/// `--mode text` 时答案已经在流式增量 / `Answer` 事件里打过了，这里只补个收尾换行
/// （非流式时 `Answer` 事件自己已经是 `println!`，不补）；`json` / `transcript` 时
/// stdout **一个字也不多打**（给脚本接）。`--stat` 报告走 stderr，不在这里。
fn result_text(session: &Session, answer: &str, mode: Mode, stream: Option<bool>) -> Option<String> {
    match mode {
        Mode::Text if stream.unwrap_or(true) => Some(String::new()),
        Mode::Text => None,
        Mode::Json => Some(
            serde_json::json!({
                "answer": answer,
                "session": session.path.display().to_string(),
                "turns": session.turn_count,
                "usage": session.usage,
            })
            .to_string(),
        ),
        Mode::Transcript => {
            let value = serde_json::to_value(session.full_history()).unwrap_or(Value::Null);
            Some(context::pretty_indent1(&value))
        }
    }
}

/// 把 `result_text` + `--stat` 报告落到各自的流上（调用点两处共用）。
fn dump_result(session: &Session, answer: &str, mode: Mode, stat: bool, stream: Option<bool>) {
    if let Some(text) = result_text(session, answer, mode, stream) {
        println!("{text}");
    }
    if stat {
        // 走 stderr：stdout 的语义只有「结果」（与 `--mode json` 共存不打架）
        eprintln!("{}", session.usage_report());
    }
}

fn apply_overrides(config: &mut config::Config, cli: &Cli) -> Result<(), String> {
    if let Some(m) = &cli.model {
        config.model = m.clone();
    }
    if let Some(t) = &cli.thinking {
        // `-t off` 是给人看的写法 → 归一到配置口径 `none`（对齐 Python `cli.py`：
        // 「API 只认 none」，直接发字面 `off` 服务端不认）
        config.reasoning_effort = if t == "off" {
            config::REASONING_NONE.to_string()
        } else {
            t.clone()
        };
    }
    if let Some(raw) = &cli.reserved_tokens {
        config.reserved_tokens = config::parse_reserved_tokens(raw)?;
    }
    if let Some(n) = cli.auto_compact_threshold {
        config.auto_compact_threshold = Some(n);
    }
    if let Some(t) = cli.timeout_seconds {
        config.timeout_seconds = t;
    }
    if let Some(n) = cli.max_retries {
        config.max_retries = n;
    }
    if let Some(d) = cli.max_retry_delay_seconds {
        config.max_retry_delay_seconds = d;
    }
    if let Some(sp) = &cli.system_prompt {
        config.system_prompt = Some(read_text_or_path(sp));
    }
    if !cli.append_system_prompt.is_empty() {
        config.append_system_prompt = cli
            .append_system_prompt
            .iter()
            .map(|v| read_text_or_path(v))
            .collect();
    }
    Ok(())
}

/// `--system-prompt` / `--append-system-prompt` 的值：是存在的文件就当路径读内容，否则当字面文本。
fn read_text_or_path(value: &str) -> String {
    let path = std::path::Path::new(value);
    if path.is_file() {
        match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                eprintln!(
                    "[warn] 读取提示词文件失败 {}: {e}（当作字面文本）",
                    path.display()
                );
                value.to_string()
            }
        }
    } else {
        value.to_string()
    }
}

/// `pie-rs context info|verify|gc`：上下文压缩维护（与 Python 版同一套输出文案）。
fn context_main(action: &ContextAction) -> i32 {
    let dir = context::context_dir();
    if !dir.exists() {
        println!("暂无压缩记录（{} 不存在）", dir.display());
        return 0;
    }
    match action {
        ContextAction::Info => {
            let mut manifests: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().ends_with(".manifest.jsonl"))
                .collect();
            manifests.sort();
            for manifest in manifests {
                let name = manifest
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                println!("# {name}");
                if let Ok(text) = std::fs::read_to_string(&manifest) {
                    for line in text.lines().filter(|l| !l.trim().is_empty()) {
                        println!("  {line}");
                    }
                }
            }
            0
        }
        ContextAction::Verify => {
            let mut missing: Vec<std::path::PathBuf> = context::referenced_raw_paths()
                .into_iter()
                .filter(|p| !p.exists())
                .collect();
            missing.sort();
            if missing.is_empty() {
                println!("OK：所有 manifest 引用的原文文件都在");
                0
            } else {
                println!("缺失 {} 个原文文件:", missing.len());
                for p in missing {
                    println!("  {}", p.display());
                }
                1
            }
        }
        ContextAction::Gc { delete } => {
            let garbage = context::collect_context_garbage();
            println!("未引用文件 {} 个:", garbage.len());
            for p in &garbage {
                println!("  {}", p.display());
            }
            if *delete {
                for p in &garbage {
                    let _ = std::fs::remove_file(p);
                }
                println!("已删除 {} 个文件", garbage.len());
            }
            0
        }
    }
}

/// `pie-rs files list|gc`：图片上传件维护（文案对齐 Python 版 `pie files`）。
async fn files_main(config: &config::Config, action: &FilesAction) -> i32 {
    match action {
        FilesAction::List { all } => {
            if *all {
                return files_remote_list(config).await;
            }
            let rows = session::iter_session_files(&session::sessions_dir());
            if rows.is_empty() {
                println!(
                    "暂无图片记录（会话 __meta__.files 为空；副本目录 {}）",
                    session::files_dir().display()
                );
                return 0;
            }
            for (session_file, image_hash, entry) in rows {
                let size = entry.get("size").and_then(Value::as_i64).unwrap_or(0);
                let mime = entry.get("mime").and_then(Value::as_str).unwrap_or("?");
                let local = entry.get("local").and_then(Value::as_str).unwrap_or("");
                let local_name = std::path::Path::new(local)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                println!("{image_hash}  {size:>10} B  {mime}  {local_name}");
                let expires = entry.get("expires_at").and_then(Value::as_f64);
                let expires_txt = match expires {
                    Some(t) if t > 0.0 => config::fmt_local(t as i64),
                    _ => "永久".to_string(),
                };
                println!(
                    "    file_id={}  过期={expires_txt}  源={}",
                    entry.get("file_id").and_then(Value::as_str).unwrap_or("?"),
                    entry.get("src").and_then(Value::as_str).unwrap_or("")
                );
                println!("    会话={}", session_stem(&session_file));
            }
            0
        }
        FilesAction::Gc { delete, all } => {
            let garbage = session::collect_file_garbage(session::GC_PROTECT_HOURS);
            println!(
                "可回收的本地副本 {} 个（未被任何会话引用、且已放置超过 {} 小时）:",
                garbage.len(),
                session::GC_PROTECT_HOURS
            );
            for path in &garbage {
                println!("  {}", path.display());
            }
            if *delete {
                for path in &garbage {
                    let _ = std::fs::remove_file(path);
                }
                println!("已删除 {} 个文件", garbage.len());
            }
            if *all {
                return files_gc_remote(config).await;
            }
            0
        }
    }
}

/// `pie-rs files list --all`：列出服务端本账号的全部上传件（顺带标出哪个会话记着它）。
async fn files_remote_list(config: &config::Config) -> i32 {
    let client = match files_client(config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let files = match client.list_files().await {
        Ok(f) => f,
        Err(e) => {
            eprintln!("调用 Files API 失败: {e}");
            return 1;
        }
    };
    if files.is_empty() {
        println!("服务端没有上传件（云端为空）");
        return 0;
    }
    let index = session::file_id_index();
    println!(
        "服务端上传件 {} 个（云端那份；本地记录见不带 --all 的 `pie-rs files list`）:",
        files.len()
    );
    for f in files {
        println!(
            "{}  {:>10} B  {}",
            f.id,
            f.bytes.unwrap_or(0),
            f.filename.clone().unwrap_or_else(|| "?".into())
        );
        let created = f
            .created_at
            .map(config::fmt_local)
            .unwrap_or_else(|| "?".into());
        let expires = f
            .expires_at
            .map(config::fmt_local)
            .unwrap_or_else(|| "永久".into());
        let sessions = index
            .get(&f.id)
            .map(|v| v.join("、"))
            .unwrap_or_else(|| "未记录".into());
        println!("    上传={created}  过期={expires}  会话={sessions}");
    }
    0
}

/// `pie-rs files gc --all`：调 Files API 清空服务端上传件（本地副本 / 会话记录不动）。
/// 单个删除失败不中断，有失败返回 1。
async fn files_gc_remote(config: &config::Config) -> i32 {
    let client = match files_client(config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let files = match client.list_files().await {
        Ok(f) => f,
        Err(e) => {
            eprintln!("调用 Files API 失败: {e}");
            return 1;
        }
    };
    let mut deleted = 0usize;
    let mut failed = 0usize;
    for f in &files {
        match client.delete_file(&f.id).await {
            Ok(()) => {
                deleted += 1;
                println!(
                    "  已删除 {}  {}",
                    f.id,
                    f.filename.clone().unwrap_or_else(|| "?".into())
                );
            }
            Err(e) => {
                failed += 1;
                eprintln!("  删除失败 {}: {e}", f.id);
            }
        }
    }
    println!("服务端上传件：已删除 {deleted} 个");
    if failed > 0 {
        eprintln!("{failed} 个删除失败（见 stderr）");
        1
    } else {
        0
    }
}

/// 建一个调 Files API 的客户端（不可逆操作前先把「打到哪个账号」写清楚：多套配置时看得出来）。
fn files_client(config: &config::Config) -> Result<LlmClient, String> {
    if config.api_key.is_empty() {
        return Err("未配置 api_key，无法调用 Files API（先跑 pie setup）".to_string());
    }
    let tail: String = config
        .api_key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    println!("Files API: {}  key=…{tail}", config.base_url);
    LlmClient::new(config).map_err(|e| e.to_string())
}

/// `file_id` → 记着它的会话名（给 `list --all` 标注用）。
fn session_stem(path: &std::path::Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 打开会话：`--resume` 恢复最近的；`--session <id|路径>` 已存在则载入、否则新建。
fn open_session(
    config: &config::Config,
    resume: bool,
    id: Option<&str>,
    llm: LlmClient,
    tools: ToolRegistry,
) -> Result<Session, String> {
    if resume {
        let s = Session::resume(config, llm, tools)?;
        return Ok(s);
    }
    let candidate = Session::new(config, id, llm, tools);
    if candidate.path.exists() {
        // 已存在 → 载入它（把客户端 / 工具集从刚建好的壳里取出来再用）
        let Session {
            path, llm, tools, ..
        } = candidate;
        let s = Session::load(&path, config, llm, tools)?;
        Ok(s)
    } else {
        Ok(candidate)
    }
}

async fn list_models(config: &config::Config) -> i32 {
    let client = match LlmClient::new(config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    match client.list_models().await {
        Ok(models) => {
            for m in models {
                println!("{m}");
            }
            0
        }
        Err(e) => {
            eprintln!("拉取模型列表失败: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("pie-rs").chain(args.iter().copied()))
    }

    /// `-t off` 归一成配置口径的 `none`（对齐 Python `cli.py`），其余值原样透传。
    #[test]
    fn thinking_off_is_normalized_to_none() {
        let mut config = config::Config::default();
        apply_overrides(&mut config, &cli(&["-t", "off"])).expect("off 能过");
        assert_eq!(config.reasoning_effort, "none", "`off` 要归一到 `none`");

        let mut config = config::Config::default();
        apply_overrides(&mut config, &cli(&["-t", "xhigh"])).expect("xhigh 能过");
        assert_eq!(config.reasoning_effort, "xhigh", "服务端认的值原样透传");
    }

    /// `-t` 只收 Python 那七档 + `none`；乱写的值在解析期就被拒（不静默发给服务端）。
    #[test]
    fn thinking_levels_are_validated() {
        assert!(Cli::try_parse_from(["pie-rs", "-t", "off"]).is_ok());
        assert!(Cli::try_parse_from(["pie-rs", "-t", "xhigh"]).is_ok());
        assert!(Cli::try_parse_from(["pie-rs", "-t", "none"]).is_ok());
        let err = Cli::try_parse_from(["pie-rs", "-t", "乱写"]).unwrap_err().to_string();
        assert!(err.contains("minimal"), "报错要列出合法值：{err}");
    }

    /// `--mode` 的三种取值：`text` 补收尾换行、`json` 给一行结构化、`transcript` 给完整历史。
    #[test]
    fn result_text_covers_the_three_modes() {
        let config = config::Config::default();
        let llm = pie::llm::LlmClient::new(&config).expect("client");
        let tools = pie::tools::ToolRegistry::new(Default::default());
        let session = Session::ephemeral(&config, llm, tools);

        assert_eq!(
            result_text(&session, "答案", Mode::Text, None),
            Some(String::new()),
            "只补一个换行（println! 负责那个换行）"
        );
        assert_eq!(result_text(&session, "答案", Mode::Text, Some(false)), None, "非流式已有换行");

        let json: Value =
            serde_json::from_str(&result_text(&session, "答案", Mode::Json, None).unwrap()).unwrap();
        assert_eq!(json["answer"], "答案");
        assert_eq!(json["turns"], 0);
        assert!(json["usage"]["calls"].is_number(), "{json}");
        assert!(json["session"].as_str().unwrap().ends_with(".jsonl"), "{json}");

        let transcript = result_text(&session, "答案", Mode::Transcript, None).unwrap();
        assert!(transcript.starts_with("[\n "), "缩进 1 的 JSON 数组：{transcript}");
    }

    /// `--reserved-tokens` 认 `N` / `128k` / `auto`；坏值直接报错（不静默改配置）。
    #[test]
    fn reserved_tokens_overrides_are_parsed() {
        let mut config = config::Config::default();
        apply_overrides(&mut config, &cli(&["--reserved-tokens", "64k"])).expect("64k");
        assert_eq!(config.reserved_tokens, Some(64_000));
        apply_overrides(&mut config, &cli(&["--reserved-tokens", "auto"])).expect("auto");
        assert_eq!(config.reserved_tokens, None);
        assert!(apply_overrides(&mut config, &cli(&["--reserved-tokens", "不是数"])).is_err());
    }
}
