//! pie —— 极简 agent harness（YOLO，内置 read / edit / writ / bash / repl）的 CLI 入口。
//!
//! 这里只做参数解析与分发：一次性（子 agent）/ 会话 / TUI 三种形态，外加 `sessions` /
//! `context` / `files` / `setup` 维护类子命令。核心逻辑都在 `pie` 库（见 `src/lib.rs`）。

// 这里换成对 lib 的引用（核心层已提成 `pie` 库，CLI 只是它的一个消费者）。
// ⚠ 模块声明在 `src/lib.rs`，别在这里再写 `mod xxx;`——那会变成两份独立的编译单元。
use std::io::{IsTerminal, Write};

use clap::{Parser, Subcommand};
use serde_json::Value;

use pie::event::Event;
use pie::llm::{LlmClient, RequestOptions};
use pie::session::{Session, TurnEvent};
use pie::tools::{Registry, tools_from_spec};
use pie::{cli, config, context, tui};

/// 一次性模式的输出格式（`text` / `json` / `transcript`）。
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// 只输出答案本身（stdout 能被 shell 直接接住）
    Text,
    /// 一行 JSON：`answer` / `session` / `turns` / `usage`
    Json,
    /// 完整历史（`full_history()`）的 JSON，缩进 1
    Transcript,
}

#[derive(Parser, Debug)]
#[command(name = "pie", version, about = "pie 的 Rust 重构")]
struct Cli {
    /// 指定配置文件（默认 ~/.pie/config.toml）
    #[arg(short = 'c', long = "config")]
    config: Option<std::path::PathBuf>,

    /// 本次运行的模型（覆盖配置，不持久化）
    #[arg(short = 'm', long)]
    model: Option<String>,

    /// 本次思考强度（off / none / low / medium / high / xhigh / max；`off` = `none`，覆盖配置，不持久化）
    #[arg(short = 't', long, value_parser = config::THINKING_LEVELS)]
    thinking: Option<String>,

    /// 每次请求为输出预留的 token（即 API 的 max_tokens；例：131072 / 128k / auto；覆盖配置）
    #[arg(long, alias = "max-tokens", value_name = "N")]
    reserved_tokens: Option<String>,

    /// 单回合最多问几次模型（**本次运行**的旋钮，不写进配置）
    #[arg(long)]
    max_steps: Option<usize>,

    /// 强制非流式（一次性 `complete`；同样只是本次运行的旋钮）
    #[arg(long)]
    no_stream: bool,

    /// 上下文 token 估算超过该值即自动压缩（覆盖软阈值，不持久化）
    #[arg(long)]
    auto_compact_threshold: Option<usize>,

    /// 「等不到数据」的秒数上限（建连 / 响应头 / 流式数据块间隔；不是总时长）
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

    /// 限制可用工具（逗号分隔）：内置名（read/edit/writ/bash/repl）启用该工具，插件工具名（Python / shell）启用它，其余名字当 shell 子命令白名单
    /// （如 `--tools read,ls,grep` = read + 只允许 ls/grep 的受限 bash）
    #[arg(long)]
    tools: Option<String>,

    /// 恢复最近的会话继续（同工作目录优先）；不写这个就跑一次性模式（不落盘）
    #[arg(short = 'r', long = "resume")]
    resume: bool,

    /// 指定会话 id 或路径（已存在则载入，否则新建）
    #[arg(short = 's', long = "session")]
    session: Option<String>,

    /// 一次性模式的输出格式：`text` = 只输出答案（默认）；
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
    /// 列出本地图片副本（加 --all 改列云端上传件）
    List {
        /// 调 Files API 列出服务端本账号的全部上传件
        #[arg(long)]
        all: bool,
    },
    /// 删掉一张图的本地副本（加 --all 连云端那份一起删）
    Rm {
        /// 图片 id（`files list` 第一列，形如 `img-3f9a…`）
        id: String,
        /// 连服务端那份一起删（之后那条历史里的图会降级成占位文本）
        #[arg(long)]
        all: bool,
    },
    /// 把一个本地文件传到 Files API（打印 `file_id`；不写会话记录）
    Upload {
        /// 要上传的文件
        path: std::path::PathBuf,
        /// MIME 类型（缺省按扩展名推断：png / jpg / jpeg / gif / webp）
        #[arg(long)]
        mime: Option<String>,
    },
}

fn main() {
    // OHOS 的 `getsockopt` 兜底不需要在这里装：`pie::arch::ohos::net` 是**同名符号定义**，
    // 链接期就把 tokio 那句调用接过去了（见该模块注释），没有初始化步骤、也没有顺序要求。

    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let code = rt.block_on(run(cli));
    std::process::exit(code);
}

async fn run(cli: Cli) -> i32 {
    // `--cwd`：工具的工作目录（默认当前目录）。**放在一切之前**：数据根（[`config::pie_dir`]：
    // `PIE_HOME` → **这里**的 `<cwd>/.pie` → `~/.pie`）与配置路径都按当前目录算，`pie setup` 也得落在同一个根上。
    if let Some(cwd) = &cli.cwd
        && let Err(e) = std::env::set_current_dir(cwd)
    {
        eprintln!("切到工作目录失败 {}: {e}", cwd.display());
        return 1;
    }

    // `pie setup`：只补缺的默认件，**不读也不改**已有配置（配置缺失/坏掉正是它要处理的情形），
    // 所以要在 `Config::load` 之前、也在启动时那发记忆种子之前接住它。
    if matches!(cli.command.as_ref(), Some(Cmd::Setup)) {
        return setup_main(&cli);
    }

    let mut config = match config::Config::load(cli.config.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    // 首次运行写入全局记忆的种子文件（已存在则跳过）：它会被拼进 system prompt，
    // 没文件就白白少一层记忆。落点跟数据根走（与 `pie setup` 同一处：`config.storage`）。
    config.storage.ensure_global_memory();

    // 一次性覆盖（`-m` / `-t` / `--reserved-tokens` …）——只改内存里的 config，不写回文件
    if let Err(e) = apply_overrides(&mut config, &cli) {
        eprintln!("{e}");
        return 2;
    }

    // 维护类子命令（不需要模型）；files 要 config（建 Files API 客户端）
    if let Some(cmd) = &cli.command {
        return match cmd {
            // 正常走不到（`run()` 开头已提前返回）——但这条臂留着：万一提前返回被挪掉，
            // `setup` 依旧是对的（它对已有文件一律不动，与启动时那发种子同级）。
            Cmd::Setup => setup_main(&cli),
            Cmd::Files { action } => files_main(&config, action).await,
            Cmd::Sessions { limit, all, json } => sessions_main(
                &config.storage,
                if *all { None } else { Some(*limit) },
                *json,
                // `--all` 连子会话（工具开的）一起列
                *all,
            ),
        };
    }

    if cli.models {
        return list_models(&config).await;
    }

    // 任务：位置参数拼起来；没给就从 stdin 读（`echo "任务" | pie`）；都空才打印帮助
    let mut task = cli.task.join(" ");
    if task.trim().is_empty() && !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        if std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).is_ok() {
            task = buf;
        }
    }
    let session_mode = cli.resume || cli.session.is_some();

    // 两个**按次**的执行旋钮（不在配置里）：
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
    // 工具集：先装出**全量**（内置 + 用 Python 写的工具 + 用 shell 写的工具），再按 `--tools` 裁剪。
    // ⚠ 顺序要紧：`--tools` 里非内置的名字要先跟插件工具对（对上 = 放行该工具，
    // 对不上 = shell 子命令白名单），所以插件工具得先装、名字先知道。
    // Python 插件（工具 / 扩展）走一个长活宿主子进程（`[python] plugins`）；没配就是零开销，
    // 起不来 / 报错只告警。解释器 / 工具文件 / 给宿主的 `PIE_*` 快照都从 `config` 取。
    // shell 插件（`<root>/plugins/*.sh`）不起进程 —— 只读一遍文件头，一次调用才 spawn 一个脚本。
    let registry = pie::pyplugin::load(Registry::new(config.tool_defaults()), &config).await;
    let registry = pie::shplugin::load(registry, &config);
    let registry = tools_from_spec(cli.tools.as_deref(), registry);

    // TTY 且没有任务 → 进 TUI（`-r` / `-s` 就接着那个会话聊）；非 TTY 保持原来的行为
    if task.trim().is_empty() && std::io::stdin().is_terminal() {
        let session = if session_mode {
            match open_session(
                &config,
                cli.resume,
                cli.session.as_deref(),
                client,
                registry,
            ) {
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
        // 非 TTY 且没给任务：一行到 stderr + 退出码 2，别污染 stdout 管道
        eprintln!(
            "pie {}（配置 {}）",
            config.model,
            config
                .config_file
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
        eprintln!(
            "请提供任务描述（`pie \"任务\"`，或从 stdin 传入）；真实终端里不带任务运行会进 TUI"
        );
        return 2;
    }

    // 一次性模式：**stdout 只放结果**，工具活动／步骤
    // 与调试日志都不往 stderr 写（`--mode text` 时 stdout = 答案本身，shell 能直接接住；
    // `stream` 只影响到达时间）。
    let mode = cli.mode;

    /// 一次性 / 会话模式的 stdout 打印（`--mode text` 时 stdout = 答案本身）。
    ///
    /// 只**订两类**事件（见下面各分支的 `session.on(..)`）：`content_delta`（流式正文增量）
    /// 与 `answer`（非流式的最终答复）—— 工具活动与会话生命周期都不进 stdout，
    /// 所以它们**压根不会到这儿**（曾经的 `[tNsM]` 日志已随 `Config.verbose` 删掉）。
    fn print_event(ev: &Event, mode: Mode) {
        if mode != Mode::Text {
            return; // `json` / `transcript` 由调用方最后一次性输出
        }
        match ev {
            Event::Turn(TurnEvent::ContentDelta { text: delta }) => {
                print!("{delta}");
                let _ = std::io::stdout().flush();
            }
            // 非流式（`--no-stream`）时的最终答复：流式下走 ContentDelta 增量，这里不会来
            Event::Turn(TurnEvent::Answer { text }) => println!("{text}"),
            _ => {}
        }
    }

    // 会话模式（--resume / --session）：读写 `~/.pie/sessions/`，跑完落盘；一次性模式不碰磁盘。
    if session_mode {
        let mut session = match open_session(
            &config,
            cli.resume,
            cli.session.as_deref(),
            client,
            registry,
        ) {
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
        // 事件出口：只订两类（`print_event` 只写 stdout，不阻塞回合）
        let _deltas = session.on("content_delta", move |ev| {
            print_event(ev, mode);
            Ok(())
        });
        let _answers = session.on("answer", move |ev| {
            print_event(ev, mode);
            Ok(())
        });
        let answer = match session
            // `parallel_tools` 跟随配置（CLI 没有覆盖它的旗标）；`stream` 是**请求的属性**
            //（思考深度用客户端/配置的、`response_format` = text，其余走默认）
            .aturn(
                &task,
                max_steps,
                None,
                RequestOptions::default().with_stream(stream),
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

    // 一次性模式：临时会话（不落盘、压缩事件也只在内存），跑完就扔；
    // 回合循环现在就在 `Session::aturn` 里（原 loop.rs 已并入）；
    // `--system-prompt` / `--append-system-prompt` 已通过 `apply_overrides` 进了 config
    let mut session = Session::ephemeral(&config, client, registry);
    let _deltas = session.on("content_delta", move |ev| {
        print_event(ev, mode);
        Ok(())
    });
    let _answers = session.on("answer", move |ev| {
        print_event(ev, mode);
        Ok(())
    });
    match session
        .aturn(
            &task,
            max_steps,
            None,
            RequestOptions::default().with_stream(stream),
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

/// `pie setup`：把数据根（`PIE_HOME` → `<cwd>/.pie` → `~/.pie`）下缺的默认件补齐 ——
/// 默认配置文件 + 全局记忆种子 + `envs/base`（Python 环境）。
///
/// 落点与成败全交给 [`config::Storage::setup`]（路径跟 `pie_dir` 走，不在这儿拼），
/// 这里只负责打印。**非交互**（只写默认值，之后自己改），已存在的文件**一律不覆盖**
/// （里面可能有用户自己的 key 与记忆）→ 重复跑安全、幂等。
fn setup_main(cli: &Cli) -> i32 {
    let storage = config::Storage::from_env();
    let report = storage.setup(cli.config.as_deref());
    let mut code = 0;

    match &report.config_file {
        Ok((path, created)) => {
            let label = if *created { "已创建" } else { "已存在" };
            println!("配置文件  {label}  {}", path.display());
        }
        Err(e) => {
            eprintln!("{e}");
            code = 1;
        }
    }
    match &report.memory {
        Ok((path, created)) => {
            let label = if *created { "已创建" } else { "已存在" };
            println!("全局记忆  {label}  {}", path.display());
        }
        Err(e) => {
            eprintln!(
                "写入全局记忆失败（{}）: {e}",
                storage.global_memory().display()
            );
            code = 1;
        }
    }
    // 默认 Python 环境（`repl` 与 Python 工具共用）：`uv` 建的 `envs/base`。
    // 建不成**不算 setup 失败**——只是 `repl` / Python 工具没得用，说清楚就行（`Err` 只打告警）。
    match &report.python_env {
        Ok(msg) => println!("Python 环境  {msg}"),
        Err(msg) => eprintln!("Python 环境  跳过  {msg}"),
    }

    if code != 0 {
        return code;
    }
    if let Ok((_, true)) = report.config_file {
        println!("接着：按需改上面的 model / base_url / api_key，再跑 `pie \"任务\"`。");
        println!(
            "Python 依赖装进这一处就行（`repl` 与用 Python 写的工具共用同一个环境）：\n  uv pip install --python {} <包>",
            storage.base_env_python().display()
        );
    } else {
        println!("三样都在，未改动。");
    }
    0
}

/// `pie sessions`：列出历史会话。
fn sessions_main(
    storage: &config::Storage,
    limit: Option<usize>,
    json: bool,
    include_children: bool,
) -> i32 {
    let rows = cli::list_sessions(storage, limit, include_children);
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
        println!("暂无会话（{}）", storage.sessions().display());
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
            // 压成单行、超 80 字截断
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
fn result_text(
    session: &Session,
    answer: &str,
    mode: Mode,
    stream: Option<bool>,
) -> Option<String> {
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
        // `-t off` 是给人看的写法 → 归一到配置口径 `none`
        // （API 只认 none，直接发字面 `off` 服务端不认）
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

/// `pie files list|gc`：图片上传件维护。
async fn files_main(config: &config::Config, action: &FilesAction) -> i32 {
    match action {
        FilesAction::List { all } => {
            if *all {
                return files_remote_list(config).await;
            }
            // **以磁盘为准**：`files/` 下有什么就列什么（会话里不再存图片账本）
            let dir = config.storage.files();
            let mut rows: Vec<(String, u64)> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .filter(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().starts_with("img-"))
                        .unwrap_or(false)
                })
                .map(|p| {
                    let size = p.metadata().map(|m| m.len()).unwrap_or(0);
                    (
                        p.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        size,
                    )
                })
                .collect();
            rows.sort();
            if rows.is_empty() {
                println!("本地没有图片副本（{}）", dir.display());
                return 0;
            }
            for (name, size) in rows {
                println!(
                    "{}  {:>10} B",
                    name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s),
                    size
                );
            }
            0
        }
        FilesAction::Rm { id, all } => files_rm(config, id, *all).await,
        FilesAction::Upload { path, mime } => files_upload(config, path, mime.as_deref()).await,
    }
}

/// `pie files upload <path>`：把本地文件传到 Files API，打印 `file_id`。
///
/// **运维命令**：`file_id` 只能给人自己拼请求 / 排障用——它进不了会话（`read` 读图那条路是
/// 工具产出 `Attachment::Model`、由 `Session::settle_output` 在消息构造前上传）。
/// 所以这里既不写会话记录（`files list` 看不到它），也不落本地副本（`files rm` 不认它）。
async fn files_upload(config: &config::Config, path: &std::path::Path, mime: Option<&str>) -> i32 {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("读不了 {}: {e}", path.display());
            return 1;
        }
    };
    if data.is_empty() {
        eprintln!("{} 是空文件，没什么可传的", path.display());
        return 1;
    }
    // 不读文件头（魔数嗅探是 `read` 那条路的事）：这儿只按扩展名猜，猜不出就让人给 `--mime`
    let mime = match mime {
        Some(m) => m.to_string(),
        None => match path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .as_deref()
        {
            Some("png") => "image/png".to_string(),
            Some("jpg" | "jpeg") => "image/jpeg".to_string(),
            Some("gif") => "image/gif".to_string(),
            Some("webp") => "image/webp".to_string(),
            // Files API 只收 png / jpeg / gif / webp（bmp 之类服务端会 400）——猜不出就让人给 `--mime`
            _ => {
                eprintln!(
                    "猜不出 {} 的 MIME 类型（Files API 收 png / jpeg / gif / webp），请用 --mime 指定",
                    path.display()
                );
                return 1;
            }
        },
    };
    let client = match files_client(config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "upload".to_string());
    let bytes = data.len();
    let uploaded = match client
        .upload_file(data, &filename, &mime, config.files_ttl_days as i64)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("上传失败: {e}");
            return 1;
        }
    };
    let expires = uploaded
        .expires_at
        .map(config::fmt_local)
        .unwrap_or_else(|| "永久".to_string());
    println!("{}  {}  {bytes} B  过期={expires}", uploaded.id, mime);
    println!(
        "（只调了 Files API：不写会话记录，`files list` 不会列出它；`files rm <id> --all` 删的是它那种云端件）"
    );
    0
}

/// `pie files list --all`：列出服务端本账号的全部上传件（顺带标出哪个会话记着它）。
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
    println!(
        "服务端上传件 {} 个（云端那份；本地副本见不带 --all 的 `pie files list`）:",
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
        println!("    上传={created}  过期={expires}");
    }
    0
}

/// `pie files rm <id> [--all]`：删一张图的本地副本（`--all` 连云端那份一起删）。
///
/// - **默认只删本地**：无害 —— 历史里那条 tool 消息的 `file` 块带的是 `file_id`，服务端那份还在；
///   下次同图 `read` 会从源文件重新落一份副本（内容寻址，幂等）。唯一的损失是源文件没了时的那份备份。
/// - **`--all` 连云端一起删**：`file_id` 失效 → 下一条请求 400 → `downgrade_file_blocks` 把历史里的
///   图就地降级成占位文本（回合继续，自愈，但模型从此看不到那张图）。**这会改变历史效果**。
async fn files_rm(config: &config::Config, id: &str, all: bool) -> i32 {
    // 本地：`files/<id>.<ext>`（扩展名由内容定，同 id 只有一个）
    let dir = config.storage.files();
    let mut removed = 0usize;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if stem != id {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    removed += 1;
                    println!("已删除本地副本 {}", path.display());
                }
                Err(e) => eprintln!("删除失败 {}: {e}", path.display()),
            }
        }
    }
    if !all {
        if removed == 0 {
            eprintln!("{id} 没有本地副本（`pie files list` 看现有的）");
            return 1;
        }
        return 0;
    }
    // 云端：按文件名反查（我们上传的名字就是 `img-<hash><ext>`）
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
    for f in &files {
        let stem = f
            .filename
            .as_deref()
            .and_then(|n| n.rsplit_once('.').map(|(s, _)| s))
            .unwrap_or_default();
        if stem != id {
            continue;
        }
        match client.delete_file(&f.id).await {
            Ok(()) => {
                deleted += 1;
                println!("已删除云端上传件 {}（{}）", f.id, stem);
            }
            Err(e) => eprintln!("删除失败 {}: {e}", f.id),
        }
    }
    if deleted == 0 {
        eprintln!("服务端没有 {id} 对应的上传件");
        return 1;
    }
    println!("（历史里引用它的图会在下一次请求 400 后就地降级成占位文本）");
    0
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

/// 打开会话：`--resume` 恢复最近的；`--session <id|路径>` 已存在则载入、否则新建。
fn open_session(
    config: &config::Config,
    resume: bool,
    id: Option<&str>,
    llm: LlmClient,
    registry: Registry,
) -> Result<Session, String> {
    if resume {
        let s = Session::resume(config, llm, registry)?;
        return Ok(s);
    }
    // `-s <会话目录>/messages.jsonl` → 取父目录（方便从目录里点文件）
    let id = id.map(|raw| {
        let p = std::path::Path::new(raw);
        if p.file_name().is_some_and(|n| n == "messages.jsonl") {
            p.parent()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_else(|| raw.to_string())
        } else {
            raw.to_string()
        }
    });
    // 已存在 → 载入它。路径先算出来，同一个 `llm` / `registry` 直接交给 `load`
    //（`Session` 带 `Drop`，不能再建个壳再把字段拆出来复用）。
    let path = Session::path_for(config, id.as_deref());
    if path.exists() {
        // ⚠ 旧版会话是**一个 `.jsonl` 文件**：不报错的话就会被当成"新建一个同名目录"，
        // 用户以为打开了旧会话、实际是空的。这一行是必需的（破坏性更新只给这一句提示）。
        if path.is_file() {
            return Err(format!(
                "{} 是旧格式的会话文件，本版本不再支持（会话现在是一个目录；升级前备份或清空 sessions/）",
                path.display()
            ));
        }
        let mut session = Session::load(&path, config, llm, registry)?;
        // **接管**：显式打开的如果是工具开的子会话，就把它变回普通会话 —— 否则用户接着聊的
        // 这条永远进不了 `-r`（`latest_in` 跳过有 `parent` 的）。
        if session.parent.take().is_some() {
            pie::log::warn("这是工具开的子会话：已接管，从现在起按普通会话保存");
        }
        return Ok(session);
    }
    Ok(Session::new(config, id.as_deref(), llm, registry))
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
        Cli::parse_from(std::iter::once("pie").chain(args.iter().copied()))
    }

    /// `-t off` 归一成配置口径的 `none`，其余值原样透传。
    #[test]
    fn thinking_off_is_normalized_to_none() {
        let mut config = config::Config::default();
        apply_overrides(&mut config, &cli(&["-t", "off"])).expect("off 能过");
        assert_eq!(config.reasoning_effort, "none", "`off` 要归一到 `none`");

        let mut config = config::Config::default();
        apply_overrides(&mut config, &cli(&["-t", "xhigh"])).expect("xhigh 能过");
        assert_eq!(config.reasoning_effort, "xhigh", "服务端认的值原样透传");
    }

    /// `-t` 只收那七档 + `none`；乱写的值在解析期就被拒（不静默发给服务端）。
    #[test]
    fn thinking_levels_are_validated() {
        assert!(Cli::try_parse_from(["pie", "-t", "off"]).is_ok());
        assert!(Cli::try_parse_from(["pie", "-t", "xhigh"]).is_ok());
        assert!(Cli::try_parse_from(["pie", "-t", "none"]).is_ok());
        let err = Cli::try_parse_from(["pie", "-t", "乱写"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("minimal"), "报错要列出合法值：{err}");
    }

    /// `--mode` 的三种取值：`text` 补收尾换行、`json` 给一行结构化、`transcript` 给完整历史。
    #[test]
    fn result_text_covers_the_three_modes() {
        let config = config::Config::default();
        let llm = pie::llm::LlmClient::new(&config).expect("client");
        let tools = pie::tools::Registry::new(Default::default());
        let session = Session::ephemeral(&config, llm, tools);

        assert_eq!(
            result_text(&session, "答案", Mode::Text, None),
            Some(String::new()),
            "只补一个换行（println! 负责那个换行）"
        );
        assert_eq!(
            result_text(&session, "答案", Mode::Text, Some(false)),
            None,
            "非流式已有换行"
        );

        let json: Value =
            serde_json::from_str(&result_text(&session, "答案", Mode::Json, None).unwrap())
                .unwrap();
        assert_eq!(json["answer"], "答案");
        assert_eq!(json["turns"], 0);
        assert!(json["usage"]["calls"].is_number(), "{json}");
        assert!(
            json["session"]
                .as_str()
                .unwrap()
                .rsplit('/')
                .next()
                .is_some_and(|id| id.starts_with("ephemeral-")),
            "session 现在是**会话目录**：{json}"
        );

        let transcript = result_text(&session, "答案", Mode::Transcript, None).unwrap();
        assert!(
            transcript.starts_with("[\n "),
            "缩进 1 的 JSON 数组：{transcript}"
        );
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
