# AGENTS.md

面向进入本仓库的 AI agent（以及人类协作者）的项目说明。历史决策与变更见 `docs/CHANGELOG.md`。

## 项目概述

pie 是一个极简的 agent harness，**纯 Rust 实现**：
内置 read / edit / **writ** / **bash** 四个工具（后两个名字是用户点名的，**不是笔误**）、YOLO 模式（无权限确认、
不做沙箱）、模型走 OpenAI 兼容接口（DeepSeek / Qwen / vLLM / Ollama…）。

本仓库对外有三样东西：可执行文件 `pie`（CLI + TUI）、库 `pie`（核心层，lib 名 `pie`）、
Python 绑定的原生产物（`bindings/pie-py`，`import pie`）。**TUI 与 CLI 参数结构不算对外 API。**

## 目录结构

```text
pie/
├── src/
│   ├── lib.rs        # 核心库入口（模块边界在这里声明）
│   ├── main.rs       # CLI：一次性（子 agent）/ TUI 入口 + sessions/context/files 子命令
│   ├── config.rs     # ~/.pie/config.toml + 分层 system prompt + 全局记忆种子 + 项目根/时间工具
│   ├── llm.rs        # OpenAI 兼容客户端（reqwest + 手写 SSE，**无 SDK**）+ 重试 + Files API + 余额
│   ├── tools.rs      # 工具层：Tool trait + ToolRegistry + read/edit/writ/bash
│   ├── session.rs    # 会话 JSONL + resume + 回合循环（Session::aturn）+ 图片记录/本地副本
│   ├── context.rs    # 三级压缩 + 落盘指针 + manifest + gc + 窗口归档目录
│   ├── cancel.rs     # 取消信号（Esc / Cancel）
│   ├── log.rs        # 告警出口（TUI 期间不能直接写 stderr）
│   └── tui/          # ratatui 界面（模块划分见 src/tui/mod.rs 顶部注释）
│       ├── app.rs        # 状态机 + 事件循环 + 全部渲染（最大的文件）
│       ├── history.rs    # 消息流单元格 + **自己折行** + 选区切片
│       ├── markdown.rs   # markdown → Text（按宽度缓存 + 超宽表格重排）
│       ├── palette.rs    # `/` 命令补全（候选表是唯一事实来源，/help 由它生成）
│       ├── files.rs      # `@` 文件路径补全（gitignore-aware 索引，后台建；`@..`/`@/`/`@~/` 实时列目录）
│       ├── input.rs      # 输入框（ratatui-textarea，多行 + 软换行）
│       ├── status.rs     # 底部状态栏 + 输入框 placeholder 文案
│       ├── theme.rs      # 语义色板 / 图标 / spinner 帧
│       └── clipboard.rs  # 读剪贴板图片 + 写剪贴板（长活 Copier）
├── prompts/          # 内置提示词正文（**小写文件名**，`include_str!` 编译期嵌入）
├── bindings/pie-py/  # PyO3 绑定（**自己的 Cargo workspace**，被根 workspace exclude）
├── docs/             # CHANGELOG.md（历史决策）/ python-bindings.md（绑定规划）
├── AGENTS.md         # 本文件（运行时被拼进 system prompt）
├── MEMORY.md         # 项目持久记忆（同上）
└── README.md         # 面向人的总说明
```

## 常用命令

```bash
export PATH="$HOME/.cargo/bin:$PATH"                  # 本机 cargo 不在默认 PATH
export CARGO_TARGET_DIR=$HOME/.cache/pie-target       # 可选：产物放大盘（源码在 9p 盘时才必要）
cargo test                                            # 168 例（lib 164 + bin 4），不联网
cargo build && cargo run -- --models                  # 端点可用模型
cargo run                                             # 真 TTY + 无任务 → 进 TUI
cargo run -- "任务"                                   # 一次性（子 agent；不落盘、stderr 静音）
cargo run -- -r "接着聊"                              # 恢复最近会话；-s <id|路径> 指定会话
cargo run -- setup                                    # 补齐默认配置文件 + 全局记忆（缺什么写什么）
cargo run -- sessions -l 5 | context info | files list   # 维护类子命令（另有 verify / gc）

cd bindings/pie-py                                    # Python 绑定（不联网测试，本地假 SSE 端点）
VIRTUAL_ENV=$PWD/.venv .venv/bin/maturin develop && .venv/bin/python -m pytest tests -q
```

## 约定

### 分层与 feature

- 核心层（`config` / `llm` / `tools` / `session` / `context` / `cancel` / `log`）放 lib；
  `src/tui/` 在 `tui` feature 后面，clap 在 `cli` feature 后面。绑定侧用 `default-features = false`
  依赖本库 → TUI 那堆依赖**真的**不进依赖图。
- `main.rs` 只 `use pie::…`，**不要**再写 `mod xxx;`（那会变成第二份编译单元）。模块声明只住 `src/lib.rs`。
- TUI / CLI 参数结构不进 lib 的对外承诺；lib 目前**全 pub**（M0 的临时状态，收紧要等绑定 API 稳定）。
- **命名**：拿 `Config` 当参数 / 变量就叫 `config`（**不要 `cfg`**）；绑定内部从 `PyConfig` 里取出的核心配置叫 `core_config`。

### 工具层

- **一个工具 = 一个结构体**：字段即参数、doc 首行即 `function.description`、非 `Option<T>` 即必填、
  `#[schemars(skip)]` 即私有参数；schema 由 `#[derive(Deserialize, JsonSchema)]` 派生，**没有自写宏**。
  加工具 = 写结构体 + `impl Tool` + `ToolRegistry::new()` 里加一行 `.with_tool::<T>("名字")`。
- 实现**直接写在 `impl Tool` 的 `call` 里**（`let Self { .. } = self;` 解构，没有 `_impl` 转发层）；
  只服务单个工具的辅助逻辑（图片嗅探、字节预算、头部截断、全文落盘、edit 诊断）都就地写，
  模块级只留 `format_output` 这种被所有工具共用的小事。
- 两条编译器定的规矩：trait 里必须写 `-> impl Future<Output=…> + Send`（`async fn` 表达不出 `Send`；
  impl 里仍可写 `async fn`）；`#[schemars(...)]` 必须写在 `#[derive(JsonSchema)]` **之后**。
  schemars 另有两个坑：doc 的单换行会被合并成空格（多行描述用 `#[schemars(description=…)]`）、
  嵌套结构体**不能**加 doc。
- **输出协议**：`Headers\n\nBody`——headers 一行一个 `[key=value]`，body 为空时连空行都不给。
  成败只看**第一行**：bash **失败才给头**（一行 `[exit=N, os=…, shell=…]`），成功只有正文
  （成功且无输出 = 空串，端点是收的）。判据是「`[exit=` 开头且不是 `[exit=0…`」= 失败
  （`[exit=0]` 只来自旧会话，也得认）；那行没有「纯值」约束，别写 `^\[exit=(\d+)\]$`。
- **落盘判据：不可再生才落盘**。bash 的 stdout 进程一结束就没了 → 全文落盘 + `[工具输出全文已保存: path]`
  指针（超限只留开头）；read 的内容可再生（原文件还在、自带 offset 分页）→ 只补
  `[已截断：可用 offset=N 继续读]`，**不落盘**。
- bash 起进程必须独立进程组（`process_group(0)`）+ 取消/超时 `killpg(SIGKILL)`：只杀 `bash` 会让
  孙进程变孤儿并持有管道写端，等待被卡到子孙自然退出。shell 名/选项只住 `impl Bash` 的
  `const SHELL` / `SHELL_FLAG`（Unix `bash -c` / 否则 `cmd /C`），别在别处再写字面量。

### 会话、上下文、模型层

- 会话 JSONL 固定在 `~/.pie/sessions/chat-<unix 秒>-<微秒>.jsonl`（首行 `__meta__`）；
  resume 时按 `__meta__.windows` 重建窗口摘要（丢掉旧 system 后必须重建，否则模型看不到归档历史）。
- **JSONL 落盘要转义「行分隔类」字符**（`session::json_line` / `escape_control_chars`）：serde_json 只转义 C0，
  C1（U+0080–U+009F）与 U+2028/U+2029 会裸着落盘——JSON 里合法，但 `splitlines()` 那一类读者会把
  **U+0085 当换行**，一条消息被劈成两半、整份会话读不出来。session 文件与 `/clear` 的窗口块都走 `json_line`。
- 压缩元数据字段（`compress_level` / `raw_path` / `raw_hash` / `raw_len` / `raw_tokens` / `synthetic`）
  **绝不能进 API 请求体**：发给模型前统一过 `Message::to_api()`。
- 目录分工别混：压缩落盘在 `~/.pie/context/`（`context gc` 的地盘），`/clear` 归档的窗口块在
  `~/.pie/windows/`（用户主动归档的原文，gc 不碰），本地图片副本在 `~/.pie/files/`（有 24h mtime 保护窗）。
- **执行旋钮按次传**：`Session::aturn(input, on_event, cancel, max_steps, stream, parallel_tools)`；
  `--max-steps` / `--no-stream` **不进 Config**（CLI 直接传，TUI 经 `tui::run` 带下来）。
- 重试收敛成唯一驱动器 `LlmClient::with_retry(what, op, decide)` + `Retry::{Backoff, Now, Give}`；
  流式只在**还没吐过增量**时重试；400 拒 `stream_options` → 摘参数立刻重来（不占重试额度）。
  `Retry-After` 优先，夹在 `[1.0, 60.0]`。
- 图片只走 Files API（无 base64 回退）。⚠ `expires_after` 只能用**方括号展开的表单字段**
  `expires_after[anchor]=created_at` + `expires_after[seconds]=N`（发 JSON 串会被当永久件收下）。
- 取消：`cancel::Cancel`（`AtomicBool` + `Notify`，`notify_waiters` 不补发 → 先查标志再 await）。
  收尾必须保证序列合法：未执行的 `tool_call` 补 `CANCEL_TEXT` 的 tool 消息 + 历史写一条 `CANCEL_TEXT`
  的 assistant 消息，并作为本轮答复返回。
- **失败也要留一条 assistant**：模型请求出错（网络 / 服务端 / 协议）时不直接 `return Err`，
  先 `push_error_turn` 把 `[请求失败] <错误>` 写进历史再抛——`push_user` 已经进了一条 user，
  不补就留下「没人应答的提问」。CLI 的会话模式（`-r` / `-s` + 任务）在 `Err` 上**也要 `save()`**
  再 `return 1`；TUI 本来就是退出时一定存。

### TUI

- **告警不能直接写 stderr**：raw mode + 交替屏下写 stderr 会砸花屏幕（ratatui 只重画变化的单元格，
  那些行永不恢复）。一律走 `log::warn`（`App::run` 装了出口就是消息流里一条 `· …`，没装就 `eprintln!`）。
- **「聚焦才亮」的样式只在色板里定义一次**（`Palette::style_emphasis` / `style_caret` / `style_selection`）：
  `tui::run` 开 `EnableFocusChange` → `Event::FocusGained/FocusLost` 进 `App::focused`（初值 `true`），
  再一路传给各视图；各视图**不该**自己再写一遍「失焦就 muted」。
- **宽字符的后半格得自己擦**：`BufferDiff` 不重画宽字形后面那格 → `Input::render` 每帧用 `StaleTail`
  把「已写区间右边、上一帧写过的那段」标 `CellDiffOption::AlwaysUpdate` 强制重画（只在**行变短**那帧，
  静止帧零开销；且**只标已写区间之外**——写正文里那格会把汉字擦掉半个）。
- **终端光标位置 = 输入法的锚点**：每帧 `App::run` 在 `terminal.draw` **之后** `terminal.set_cursor_position(插入符)`，
  否则 IME 候选框会停在「上一帧 diff 最后写入的那一格」（点一下消息流候选框就会跑过去）。
- 消息流**自己折行**（`history::layout` → 带逻辑行号的 `Row`）：滚动偏移按折行后的显示行算、复制按源文本切。
  鼠标捕获为滚轮常开 → 框选/拖选都得自己做，写完剪贴板用长活 `Copier`（每帧新建再 drop 会砸屏 + 复制不生效）。
- 多行粘贴必须自己 `EnableBracketedPaste`（`ratatui::init()` 不开）。
- lean 模式（`[tui] lean`，默认 true）只压工具活动那一行；`/help` 文案由 `palette::COMMANDS` 生成。

### 配置、提示词、记忆

- 配置只从 `~/.pie/config.toml` 读（`-c` / `PIE_CONFIG_FILE` / `PIE_DIR` 可重定向）；加字段就写进
  `Default`（默认值一处定义，别另开 `DEFAULT_*` 常量）；`Config.tools` 按下划线私有参数注入工具默认值。
- 提示词分两类，**别搞混**：`prompts/system.md` / `prompts/memory.md` 是**编译期嵌入**的内置正文
  （小写文件名，`include_str!`），`SYSTEM.md` / `AGENTS.md` / `MEMORY.md` 是**运行时**从 cwd 往上找的文件。
  本仓根没有 `SYSTEM.md` → 走内置那份；而 `AGENTS.md`（本文件）与 `MEMORY.md` 会被拼进 system prompt
  ——**改它们等于改 agent 的行为**。
- 记忆分工：项目决策/踩坑写本仓 `MEMORY.md`，历史与来龙去脉写 `docs/CHANGELOG.md`，
  跨项目习惯写 `~/.pie/memory.md`。与代码冲突时**以代码为准**，过时条目就地改或删。

## 已知限制

- **工具 panic 未文本化**：只捕 `Err(ToolError)`，工具里 panic 会带崩整个回合。
- **无工具执行进度**：bash 跑到一半看不到输出（丢的只是实时性，结果本身完整）；`Tool::call` 已有
  `ToolCtx`，要加就在那里挂回调。
- 没有交互式配置向导：`pie setup` 只把缺的默认件补上（`~/.pie/config.toml` + `~/.pie/memory.md`，非交互、已存在不覆盖），模型/地址/key 靠手改。
- TUI 代码块**有**语法高亮（`tui-markdown` 的 `highlight-code`）：代价是带回 syntect → oniguruma（C 库，要 `cc`）。
- `Config.theme` **已接线**（启动时 `Palette::resolve(&config.theme)`，认 `catppuccin` / `catppuccin-<flavor>` / 裸 flavor 名）；**仍缺**明暗自适应（无 OSC 11 探测）与 `/theme` 运行中热切。
- Python 绑定 M4（abi3 wheel 分发）未做；`docs/python-bindings.md` 是它的规划与进度。
