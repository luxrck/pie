# AGENTS.md

面向进入本仓库的 AI agent（以及人类协作者）的项目说明。历史决策与变更见 `docs/CHANGELOG.md`。

## 项目概述

pie 是一个极简的 agent harness，**纯 Rust 实现**（[`pi`](https://github.com/earendil-works/pi) 的 Rust 重实现，名字取自 π 的谐音）：
内置 read / edit / **writ** / **bash** / **repl** 五个工具（`writ` / `bash` 的名字是用户点名的，**不是笔误**）、YOLO 模式（无权限确认、
不做沙箱）、模型走 OpenAI 兼容接口（DeepSeek / Qwen / vLLM / Ollama…）。

本仓库对外有三样东西：可执行文件 `pie`（CLI + TUI）、库 `pie`（核心层，lib 名 `pie`）、
Python 绑定的原生产物（`bindings/pie-py`，`import pie`）。**TUI 与 CLI 参数结构不算对外 API。**

## 目录结构

```text
pie/
├── src/
│   ├── lib.rs        # 核心库入口（模块边界在这里声明）
│   ├── main.rs       # CLI：一次性（子 agent）/ TUI 入口 + sessions/context/files 子命令
│   ├── config.rs     # ~/.pie/config.toml + 数据存储（Storage：目录 + 落盘）+ 分层 system prompt + 全局记忆种子 + 项目根/时间工具
│   ├── llm.rs        # OpenAI 兼容客户端（reqwest + 手写 SSE，**无 SDK**）+ 重试 + Files API + 余额
│   ├── tools.rs      # 工具层：Tool trait + ToolRegistry + read/edit/writ/bash（+ `SessionState` 会话级状态槽）
│   ├── repl.rs       # repl 工具：会话内持久的 IPython（长活子进程 + owner task；驱动在 src/repl_driver.py）
│   ├── session.rs    # 会话 JSONL + resume + 回合循环（Session::aturn）+ 图片记录/本地副本
│   ├── cli.rs        # 磁盘维护：会话 / 图片 / 压缩原文的列表与 GC（`pie sessions` / `files list|gc` /
│   │                 #   `context info|verify|gc` 的数据源；只读磁盘，不改会话状态。TUI 与绑定同样用它）
│   ├── context.rs    # 三级压缩 + 落盘指针 + gc + 窗口归档目录
│   ├── cancel.rs     # 取消信号（Esc / Cancel）
│   ├── log.rs        # 告警出口（TUI 期间不能直接写 stderr）
│   └── tui/          # ratatui 界面（模块划分见 src/tui/mod.rs 顶部注释）
│       ├── app.rs        # 状态机 + 事件循环 + 全部渲染（最大的文件）
│       ├── pane.rs       # 消息流面板：每消息一条 Cell + **自己折行** + 选区切片
│       ├── repl.rs       # REPL 画布（tab 视图：repl 工具的 code / 输出 + matplotlib 图的固定渲染区）
│       ├── markdown.rs   # markdown → Text（按宽度缓存；表格宽度交上游 table_width）
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
cargo test                                            # 198 例（lib 194 + bin 4），不联网
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

- 核心层（`config` / `llm` / `tools` / `session` / `context` / `cli` / `cancel` / `log`）放 lib；
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
  模块级只留 `format_output` / `head_prefix` 这种被多个工具共用的小事。
- **会话级工具状态**：`ToolCtx.state: Arc<SessionState>`（`Session` 建一份、每次工具调用 clone 进去）。
  有状态的工具（`repl` 的长活 IPython）用 `ctx.state.get_or_init::<T>(…)` 存长活对象——同一 `Session`
  共享、不同 `Session` 各一份。**别把它放 `ToolRegistry`**：registry 会被多个 session 复用（绑定的常见用法）
  → 状态会串台。
- **`repl` 的输出头区有一行 `[解释器] …`**（`repl.rs::headers`，数据来自 driver 的 `Namespace`）：
  当前命名空间 + 本次新增。为什么每帧都报：模型看不到解释器内部，而**压缩会把那些代码块一起卷走**
  —— 状态得由每次执行自己带回来（让模型另发一次询问 = 白搭一次往返）。
  为什么放**头区**：`head_prefix` 保留的是开头，超限截断/落盘时它得跟着走。
  ⚠ driver 没上报（`state: None`）就不加那行 —— 与「上报了、是空的」（`[解释器] 空`）是两回事。
- **解释器里有 `history()`（历史即数据）**：读的是 `Session::write_transcript` **每轮开头**
  重写的转录快照（临时目录 + 会话路径 hash，经 `PIE_TRANSCRIPT` 交给 driver）。
  ⚠ 它同时同步进 `user_ns_hidden`（IPython 藏 `exit` / `quit` / `open` 的同一招）→ 不出现在
  `[解释器] …` 的名字清单里（发现渠道是工具描述）；⚠ 只管**显示**，`history = 5` 照样盖掉它。
  为什么不直接读会话文件：
  它**只在退出 / `/save` 时**落盘，会话进行中根本不存在（一次性会话更是永远不落盘）。
  压缩指针（`turn` / `session`）在 **Python 侧展开**（`_read_records` 认 JSONL 与 JSON 数组两种形状）；
  工具输出只给 `full_output_path`、**不**把全文塞回来（那正是它当初落盘的原因）。
  为什么值得做：我们的压缩本来就**无损**（原文都在盘上），缺的是**可达性** ——
  `[轮次原文已保存: path]` 是散文里的一个路径，要模型记得去 `read`；`history()` 把引用变成**可编程遍历的数据**。
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
- **落盘路径是结构化的**：`Tool::call` 返回 `ToolOutput { text, spill }`（`spill` 只有 bash / repl 超限时给），
  消息层**进历史前**就把压缩元数据设好（`msg.compaction = Some(Compaction::tool(&path))`
  —— 变体即级别，与 `CompactEvent` 的 Tool / Turn / Session 一一对应）
  —— 别再拿正文里那行指针去嗅探（`extract_spill_path` / `mark_tool_spill` 已删，连带那个
  「read 回来的源码字面量会被误判」的坑一起没了）。⚠ **没落盘就别设**：`Compaction::Tool` 的定义就是
  「输出已落盘」，而 `compact_tools` 靠 `>= 1` 跳过已压过的消息 → 乱标会让工具级压缩永远压不动。`ToolOutput` 实现 `Deref<Target = str>` + `Display`，
  所以旧的 `out.starts_with(…)` / `format!("{out}")` 照常能用。
- bash 起进程必须独立进程组（`process_group(0)`）+ 取消/超时 `killpg(SIGKILL)`：只杀 `bash` 会让
  孙进程变孤儿并持有管道写端，等待被卡到子孙自然退出。shell 名/选项只住 `impl Bash` 的
  `const SHELL` / `SHELL_FLAG`（Unix `bash -c` / 否则 `cmd /C`），别在别处再写字面量。

### 会话、上下文、模型层

- 会话 JSONL 固定在 `~/.pie/sessions/chat-<unix 秒>-<微秒>.jsonl`（首行 `__meta__`）；
  resume 时按 `__meta__.windows` 重建窗口摘要（丢掉旧 system 后必须重建，否则模型看不到归档历史）。
- **JSONL 落盘要转义「行分隔类」字符**（`session::json_line` / `escape_control_chars`）：serde_json 只转义 C0，
  C1（U+0080–U+009F）与 U+2028/U+2029 会裸着落盘——JSON 里合法，但 `splitlines()` 那一类读者会把
  **U+0085 当换行**，一条消息被劈成两半、整份会话读不出来。session 文件与 `/clear` 的窗口块都走 `json_line`。
- 本地专有字段（`compaction` / `synthetic` / `thought_ms`）
  **绝不能进 API 请求体**：发给模型前统一过 `Message::to_api()`。`thought_ms` = 这条回复「思考」了多久
  （`session::ThoughtClock` 量：首个 reasoning 增量起算、首个正文增量停下）；**回放靠它还原**
  `• Thought for 3.4s` 那行——不落盘的话退出再 `-r` 就没了（非流式没有增量事件 → None）。
- 目录分工别混：压缩落盘在 `~/.pie/context/`（`context gc` 的地盘），`/clear` 归档的窗口块在
  `~/.pie/windows/`（用户主动归档的原文，gc 不碰），本地图片副本在 `~/.pie/files/`（有 24h mtime 保护窗）。
- **压缩水位只认服务端上报，不做 token 估算**：`maybe_compact(messages, config, reported)` 只看上一次
  `usage.prompt_tokens`（`reported` ≥ `soft_limit()` 就压，工具级 → 轮次级，各级压到不能再压）；
  `reported` 为 `None`（本次会话还没发过请求）就**不压**。**整窗口归档（`/clear`）不在自动路径里**：
  换窗口会让当前轮的工作记忆只剩摘要（模型「失忆」），**只由用户手动触发** —— `Session::clear_window`
  调 `context::compact_session(messages, config, storage)`（`config` 就是 `&SessionCompaction`，head/tail 在它里；
  整窗口 → `windows/`，重开成 system + 摘要链；返回 `std::io::Result`——错误上下文由 `Storage::write_atomic` 统一带上路径）。
  `message_tokens` / `messages_tokens` / `Message.raw_len` / `raw_tokens` 已全删；单条消息的有界由工具层负责
  （`read` / `bash` 的 `_max_lines` / `_max_bytes` + 超限落盘），判据失手由「400 上下文超限 → 再强压一次工具级/轮次级
  再发，压不动就报错（提示 `/clear`）」兜底（`LlmError::is_context_overflow`）。
- **压缩事件是类型化的 `context::CompactEvent`**：落盘 `{kind, ts, path, hash, summary?}`
  （`#[serde(tag = "kind", rename_all = "lowercase")]`，variant `Tool` / `Turn` / `Session` ↔ `kind` 值）；
  `level` 与 `raw_` 前缀都已去掉（旧会话靠 serde alias 读回；`referenced_raw_paths` 裸读 `Value` → `path` 优先 / `raw_path` 回退）。
  `maybe_compact` / `compact` 返回 `(CompactStats, Vec<CompactEvent>)`（**没有回调参数**），
  `Session.compaction_events: Vec<CompactEvent>` 随 `__meta__.compaction_events` 落盘 ——
  它是**内部记账**（gc 保护原文 / `/stat` 计数 / resume 登记窗口），**别往 `TurnEvent` 里塞**（那是可丢的展示流）。
- **磁盘布局与落盘都住 `config::Storage`**（一个 root + `sessions()` / `context()` / `files()` / `windows()` + `store(StoreType)`；
  `StoreType::{Blob,Raw,Window}` 分别对应图片副本 / 压缩原文 / 窗口块，目录、命名、0o600、原子写都在那儿）。
  `Config.storage` 是默认值（`PIE_DIR` → `./.pie` → `~/.pie`），`ToolCtx.storage` 把它带进工具层（bash 全文落盘要用）。
  **别再读环境变量、别再拼目录字面量**；测试用 `Storage::at(临时目录)` 注入，不必改进程级 `PIE_DIR`。
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

- **视图是 tab**（`Chat` / `Repl`）：`←`/`→` 在**输入框为空时**切（非空时它们是输入框的光标键）、`/repl` 进、
  `Esc` 回、`PgUp`/`PgDn`/滚轮滚**当前**视图；状态栏最左是 `cr` 视图指示（`Tab::label()` 各一个字符，
  `c`=对话 / `r`=画布；当前那档 accent+bold、另一档 muted）。
  REPL 画布 = `src/tui/repl.rs`（`Repl`；记录 `repl` 工具的 code/output），`App::render` 在 Repl tab **整块跳过**
  消息流那条渲染路径（`else` 分支里）。**别把画布塞回消息流**：独立 Rect 是固定的 → 贴图
  （ratatui-image）不会被滚动反复重传。
- **REPL 画布能贴图**：`repl` 里 matplotlib 出的图走**结构化通道**（`repl_driver.py` 在 `post_execute`
  扫还开着的 figure → `Reply.images` → `ToolOutput.images` → `TurnEvent::ToolResult.images` → **浮在画布右下角**的**固定 Rect**
  （文本仍占**整块**画布；浮层为画布 4/5 宽、4/5 高且上限 60 行，`Image` 贴到该区右下角），
  `ratatui-image` 渲染；显示的是**跟随滚动位置**的那张（滚到哪条记录就显示那条的图，那条没图就往前找最近
  一张，见 `Repl::pick_image`）——固定 Rect 才不会因滚动重传，而 `Protocol` 编码一次、区域变了才重编）。⚠ 协议档位**只按环境变量定**
  （`tui::pick_terminal_protocol`）：**别用 `Picker::from_query_stdio`** —— 它把「读终端回应」丢到独立
  线程，超时后那个线程杀不掉、会跟 crossterm 抢 stdin（实测 `←`/`→` 直接失效）。WezTerm 系（含 Kaku）
  只有 iTerm2 无 bug。**图能活过重启**：driver 的临时 PNG 由 `repl.rs::format_reply` 转存成内容寻址副本
  `files/img-<hash>.png`，`Session::record_repl_images` 把 `{local, calls, src:"repl"}` 登记进
  `__meta__.files`（`local` 保 `files gc` 不回收、`calls` 供 `-r` 复原），`Repl::from_messages(messages, files)`
  按 `calls` 把图填回对应记录；与 `read` 那条路共表 → `ensure_image_file` **按字段合并**（别整条覆盖）。
- **贴图的 `FontSize` 必须等于终端真实字符格**（`pick_terminal_protocol` 用 `crossterm::terminal::window_size()`
  的 `xpixel/ypixel ÷ 列/行` 现问，问不到才退回 10×20）：iTerm2 协议是按**像素**发图的
  （`1337;File=…;width=Npx`），终端就照这个像素数 1:1 画 —— 猜错多少，图就按那个比例缩错多少
  （本机 Kaku 是 20×58，用默认 10×20 算 → 图只有应有尺寸的一半，还顶在浮层左上角，看上去像「小图浮在画布中间」）。
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
- 消息流**自己折行**（`Pane::layout` **增量**重排：`frame_key`（宽度/简洁/配色）+ 每条 cell 的 `cell_key` 都没变就整块复用；每条消息一个 `Cell`，`CellBlock::rebuild(&Cell, palette, width, lean)` 重建这一块）：折行**只有一份出口**
  `CellBlock::push_line` → `pane::wrap_segments`（`WrapMode` 两档，**生产走 `WordOrGlyph`**：词级断行 + 超长词退回逐字硬断，规则对齐
  `ratatui-textarea` 的 `wrap_word_chunks`；`Glyph` = 纯逐字硬断。没有 `WRAP_MODE` 常量，档位就是调用点那个字面量），`PREFIX_CELLS` / `char_width` 也在那儿；用户消息只交代 `indent = PREFIX_CELLS` + 首行 `› `（续行与其余逻辑行
  由 `CellBlock` 补等宽空白），折行宽度 = `width - indent`。`Row` 自描述：`indent`（复制时跳过）+ `continues`（软换行的续行，复制时拼成一行，
  `Pane::slice_text` 按它决定插不插换行）。**`Cell` 是纯数据**（`&[Cell]` 就能排）：markdown 渲染缓存不住 cell 里，而是与
  **per-cell 的显示行 + 它自己的 markdown 缓存**（`blocks: Vec<CellBlock{rows,key,md}>`，与 `cells` 下标一一对应）同居 `App.pane`；`Pane::total()` 是显示行总数、`Pane::iter()` 按顺序迭代、越界项每次 `layout` 清掉。滚动偏移按折行后的
  显示行算（`scroll_from_bottom` 两端都夹：0 = 贴底跟随、`max_scroll()` = 贴顶；`render` 里再按当帧夹一次，内容变短也不留旧偏移）。
- **输入框是普通编辑器**（`input.rs`）：软换行固定 `WrapMode::Glyph`（逐字断，**不跟消息流的档位对齐**——它复刻的是控件那份折行）、左边不留 gutter / `› `；
  边框交控件渲染（`set_block`），内容区（`inner_rect()`）就是渲染时从同一个块里取的那块（记在 `Input::inner`，不再手推边框）；插入符位置问控件 `screen_cursor()`。它的 `screen_rows`
  是**控件 Glyph 折行的复刻**（鼠标命中 / 框高 / 残影擦除 / 视口滚动复刻用），钉在 `input::screen_rows_matches_the_widget_wrapping`。
  滚轮**按位置分派**：悬在输入框上且它真能滚（`Input::overflows`）→ 滚输入框，否则滚消息流。
  鼠标捕获为滚轮常开 → 框选/拖选都得自己做，写完剪贴板用长活 `Copier`（每帧新建再 drop 会砸屏 + 复制不生效）。
- **滚动条（消息流与 REPL 画布各一份）**：当前视图右缘**固定**留 1 列（`bar_w = body.width > 2`）——因为折行宽度与 markdown 缓存键都吃 `width`，
  「有溢出才留」会让跨阈值那一下换宽度 → 整段重排 + 缓存全失效；因此没溢出时**只是不画**（`ScrollbarState` 的 thumb 会铺满，很难看）。
  画法只有一份 `App::render_scrollbar`——⚠ 它的 `ScrollbarState::new(...)` 传的是「**可滚位置数**」`total - height + 1`，
  **不是总行数**：ratatui 的 `position` 满量程是 `content_length - 1`（`thumb_start = position*track/(content-1+viewport)`），
  传总行数会让分母多算 `height - 1` → 滚到底 thumb 也只到轨道 `total/(total+height-1)` 处、永远贴不到底。
  换算只有一份 `App::scrollbar_jump`（线性映射，不按 thumb 尺寸抓取），它按 tab 分派：
  消息流改 `scroll_from_bottom`，画布交给 `Repl::jump_to_row`（行数/顶部行是 `Repl` 上一帧渲染时记的 `total` / `top`：
  画布视图下 `App.body` 是空的，滚动条只能问它自己）。
  `surface_at` 有三种面：输入框 > 滚动条 > 消息流（`body` 已经是**不含**滚动条列的那块；画布视图下 `body` 为空 → 画布本身不成框选面）
  → 输入框与状态栏在垂直布局的下面几行，完全不受影响。
- 多行粘贴必须自己 `EnableBracketedPaste`（`ratatui::init()` 不开）。
- ⚠ `App::render` **每帧全量重排**：`Pane::layout` 走完全部 cells，再把全部行克隆给 `Paragraph`（唯一缓存是 markdown 渲染与 per-cell 显示行，住 `App.pane`）。
  每个终端事件 / 每个流式 delta / 每 66ms tick 都跑一帧，**没有 dirty 标记**（ratatui 的 diff 只省终端写入）；长会话是已知瓶颈
  （实测与三条治法、以及「改用终端 scrollback」的取舍见 `docs/CHANGELOG.md` 2026-09-28）→ 别往 `render` 里再加 O(历史) 的活。
- lean 模式（`[tui] lean`，默认 true）只压工具活动那一行；`/help` 文案由 `palette::COMMANDS` 生成。
- **`/cd <路径>`**（切换工作目录）：`set_current_dir` + 重解析 `config.storage`（与 CLI `--cwd` 同口径：
  `PIE_DIR` 优先 → 新 cwd 下的 `.pie` → `~/.pie`）+ 消息流一条 Notice + 作废 `@` 补全索引；`~` 会展开。
  ⚠ **不往会话历史里插消息**：模型看到的 cwd 来自 system prompt 末尾那节「运行时状态」
  （`config::runtime_state`，由 `Session::aturn` 每轮按 `RUNTIME_STATE_HEADING` 重拼）——
  往历史里插一条 system 会被轮次级压缩卷进 `[轮次原文已保存]` 摘要（span 就是「两个 user 之间的**一切**」），
  模型就再也看不见了。`messages[0]` 是**压缩免疫**的（轮次级只在 user 之间动手、会话级「system 不动」）。
  同一套将来也给 repl 的状态摘要用。

### 配置、提示词、记忆

- 配置只从 `~/.pie/config.toml` 读（`-c` / `PIE_CONFIG_FILE` / `PIE_DIR` 可重定向）；加字段就写进
  `Default`（默认值一处定义，别另开 `DEFAULT_*` 常量）；`Config.tools` 按下划线私有参数注入工具默认值。
- `Config::load` 之后再按环境变量覆盖两项：`OPENAI_API_KEY` → `api_key`、`OPENAI_BASE_URL` → `base_url`
  （空串 / 全空白 = 没设，不动配置文件里的值；只改内存、不写回文件）。`Config::default()` 不受影响。
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
