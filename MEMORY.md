# MEMORY.md — 持久记忆

本文件是项目的持久记忆：记录用户偏好、当前系统概览、构建环境与踩坑。agent 每次会话开始读取，运行中用 edit/writ 更新。

保持简洁：只记跨会话仍然有效的事实。**历史关键决策与变更记录在 `docs/CHANGELOG.md`**（本文件只保「当前状态」；工作约定见 `AGENTS.md`）。

## 用户偏好

- 信奉 YOLO：不要权限确认、不要沙箱，工具直接执行。
- 偏好极简、可扩展的实现；能不引依赖就不引（自写 SSE、自建 `Cancel`、自算折行、零 C 依赖）。
- 工具名 `writ` / `bash`（**不是笔误，别再「修」回去**）。
- TUI 形态参考 codex：状态栏（chrome）在**最下方**，消息流从屏幕第一行开始。
- 内置提示词正文用**小写文件名**（`prompts/system.md`）——与运行时按名找的 `SYSTEM.md` 区分开。
- 命名：拿 `Config` 当参数 / 变量就叫 `config`（**不要 `cfg`**）；绑定内部的核心配置叫 `core_config`。

## 项目定位与仓库

- **纯 Rust 项目**（2026-09-23 完成「重构代码到 Rust」）：仓库根就是一个 Cargo 项目（package `pie`、lib 名 `pie`、bin 名 `pie`）+ Python 绑定（`bindings/pie-py`）。
- 本机位置：macOS `/Users/luxrck/Projects/pie`（**当前实际改的这树**）；远端 `git@github.com:luxrck/pie.git`（只能走 SSH）。旧记录里还有 WSL 树 `/mnt/d/pie-master`（从 macOS 看不到，别假设它与这里同步）。

## 当前系统概览

### 分层与工具层

- lib（`pie`）＝ 核心层：`config` / `llm` / `tools` / `session` / `context` / `cancel` / `log`；`tui` 在 `tui` feature 后面，clap 在 `cli` feature 后面（绑定 `default-features = false` → TUI 依赖不进依赖图）。`src/lib.rs` 是唯一声明模块的地方；原独立 `loop.rs` 已并入 `Session::aturn`。
- 内置 4 个工具：`read` / `edit` / `writ` / `bash`。**一个工具 = 一个结构体**（字段即参数、doc 首行即 description、非 `Option` 即必填、`#[schemars(skip)]` = 私有参数），实现直接写在 `impl Tool` 的 `call` 里，模块级只留共用的 `format_output`。
- 输出统一 `Headers\n\nBody`：headers 一行一个 `[key=value]`，body 空则连空行都不给。**bash 失败才给头**（一行 `[exit=N, os=…, shell=…]`），成功只有正文（成功且无输出 = 空串）；判成败只看第一行（`[exit=` 开头且不是 `[exit=0…` = 失败）。
- **落盘判据：不可再生才落盘**。bash 的 stdout → 全文落盘 + 指针（超限只留**开头**）；read 可再生 → 只补 `[已截断：可用 offset=N 继续读]`，不落盘。
- bash：`process_group(0)` + 取消/超时 `killpg(SIGKILL)`（只杀 `bash` 会被孙进程持有的管道卡住）；shell 名/选项只住 `impl Bash` 的 `const SHELL` / `SHELL_FLAG`。`--tools read,ls,grep` = read + 只允许 ls/grep 的受限 bash（复用 `_allow_cmds`，白名单在启动进程前校验）。
- edit 诊断齐（级联引用 / 出现多次 / 重叠 / 只差空白 + 最接近位置 + 简版 diff，自写不引依赖）；图片识别用 `image` crate 只读头部，**必须过格式白名单**（TIFF/ICO 它也认得，不该当图片）。

### 会话 / 上下文 / 图片

- 会话 JSONL 固定在 `~/.pie/sessions/chat-<unix 秒>-<微秒>.jsonl`（首行 `__meta__` = usage/windows/cwd/title；按 mtime 选最新）。`UsageTracker`：token 字段是**最近一次上报值**（不求和），只有 `calls` 累加。
- **JSONL 落盘统一走 `session::json_line`**（= serde_json + `escape_control_chars`）：只转义「行分隔类」字符——C1（U+0080–U+009F）+ U+2028/U+2029（serde_json 只转 C0），否则 `splitlines()` 那类读者会把 U+0085 当换行、一条消息被劈成两半（session 文件与 `/clear` 的窗口块都这么写）。
- resume 时按 `__meta__.windows` 重建窗口摘要（**不重建模型就看不到归档历史**）。`full_history()` 把压缩指针展开成完整转录（工具级回到落盘全文，并把原消息头区拼回去——`[exit=N]` 在头区、不在落盘件里）。
- 三级压缩（`context.rs`）：工具级（head/tail 指针）/ 轮次级 / 会话级；级别只升不降、内容 hash 落盘、软阈值与目标水位迟滞（相对 `context_budget = context_window - reserved_tokens`）、`keep_last_steps` 保护最近 N 个 step 批次。消息是扁平 `Vec<Message>`（靠 `role` + `compress_level` + `synthetic` 判定），压缩就地改写列表。
- 压缩元数据字段（`compress_level` / `raw_path` / `raw_hash` / `raw_len` / `raw_tokens` / `synthetic`）**绝不能进 API 请求体** → 发模型前统一过 `Message::to_api()`。
- 目录分工：压缩落盘 `~/.pie/context/`（`context gc` 的地盘）、`/clear` 归档的窗口块 `~/.pie/windows/`（gc 不碰）、图片副本 `~/.pie/files/`。manifest 落盘 `indent=1`、`ts` 用 **unix 秒数字**。`clear_window()` 写盘失败就不动窗口（宁可不清也不丢历史）。
- **图片只走 Files API，不回退 base64**：read 到图 → 本地内容寻址副本（`img-<sha256[:16]>`，0o600）→ 上传拿 `file_id` → 注入 `synthetic` user 消息 `{"type":"file","file_id":…}`。拿不到 `file_id` 就不注入图（标记文本仍在）；`file_id` 失效 → `downgrade_file_blocks` 把 `file` 块换成文本占位 + 重试一次。⚠ `expires_after` **只能用方括号展开的表单字段**发（`expires_after[anchor]=created_at` + `expires_after[seconds]=N`，发 JSON 串会被当永久件收下）。`files gc` 有 24h mtime 保护窗。
- 事件形状：`TurnEvent::{AssistantText, Reasoning, ToolCall{name, arguments}, ToolResult{name, content, arguments}, Answer}`（`ToolCall` 的 `turn` / `step` 已删——仓库内没有活着的读者）。
- 取消（`cancel.rs`）：`AtomicBool` + `Notify`（**`notify_waiters` 不补发** → 先查标志再 await）。取消点：`aturn` 每步开头、模型请求 `select!` race、shell 等待时 race + `killpg`。收尾保证 API 序列合法：未执行的 `tool_call` 补 `CANCEL_TEXT` 的 tool 消息 + 历史写一条 `CANCEL_TEXT` 的 assistant。TUI 侧取消只有 `Esc`（`/stop` 已移除）。
- **失败也要留一条 assistant**：两处 `model_call` 出错的退出路径都先 `push_error_turn`（内容 `[请求失败] <错误>`，常量 `ERROR_TURN_PREFIX`）再 `return Err` —— 不补就留下「没人应答的提问」。调用方仍拿到 `Err`。

### 模型层（llm.rs）

- reqwest + **手写 SSE**，不含任何 SDK；`GET /user/balance` 查余额（金额是字符串）、`GET /models` 列模型。
- 重试收敛成**唯一驱动器** `LlmClient::with_retry(what, op, decide)` + 枚举 `Retry::{Backoff, Now, Give}`：`complete`/`list_models` 用 `default_retry`（额度 = `1 + max_retries`，只认 408/409/429/5xx 与传输层异常），`stream` 的两条特例写在它自己的 `decide` 里（**已吐过增量 → Give**；400 拒 `stream_options` → 改写开关 + `Now`，不占额度）。`Retry-After` 优先并夹在 `[1.0, 60.0]`。重试进度走 `log::progress(key, …)` 分组，TUI 侧**就地更新同一块**。
- 上下文两笔账：`context_window`（输入+输出一起算）与 `reserved_tokens`（= 发给 API 的 `max_tokens`，`None`/`auto` = 不发）。服务端按「输入 + max_tokens ≤ 窗口」**预检**，超了直接 400 → 水位必须相对 `context_budget` 算。窗口大小服务端只在超限报错里告诉你（`GET /models` 只给 id）；本部署 deepseek-flash 实测窗口 1,048,576、`max_tokens` 合法区间 `[1, 393216]`。

### CLI（main.rs）

- 模式判定：**真 TTY 且无任务 → TUI**；有任务 → 一次性（子 agent，不落盘，stdout 只有答案、工具活动不打印；**带 `-r`/`-s` 就是会话模式**：载入 → 跑一回合 → 存回同一文件）；非 TTY 且无任务 → 提示 + 退出码 2；无任务时从 stdin 读。会话模式**失败也落盘**：`Err` 时先 `let _ = session.save()` 再 `return 1`（`aturn` 已补一条 `[请求失败] <错误>`）；成功路径照旧 `save()` 后返回 0。
- 覆盖项集中在 `apply_overrides`（只改内存 cfg、不写回文件）：`-m/-t/--reserved-tokens(--max-tokens)/--auto-compact-threshold/--timeout-seconds/--max-retries/--max-retry-delay-seconds/--cwd/--system-prompt/--append-system-prompt/--stat/--mode {text,json,transcript}`；`-t` 校验七档 + `none`（`off` 归一到 `none`）。
- `setup`（`config::ensure_config_file` 写 `Config::default()`，`config::ensure_global_memory_file` 写 `prompts/memory.md` 种子）在 `run()` 里**早于** `Config::load` 与启动时那发记忆种子（配置缺失/坏掉正是它要修的情形）；两助手都幂等、已存在一律不覆盖。
- ⚠ `--max-steps` / `--no-stream` **不进 Config**：按次旋钮，直接传给每个 `Session::aturn`（TUI 经 `tui::run(session, max_steps, stream)` 带下来）。
- 子命令：`setup`、`sessions [-l/--all/--json]`、`context info|verify|gc [--delete]`、`files list [--all]` / `files gc [--delete] [--all]`，另有 `--models`、`--stat`、`-r/--resume`、`-s/--session <id|路径>`（存在则载入否则新建）。

### TUI（src/tui/）

- 依赖：`ratatui 0.30` + `crossterm 0.29`（`event-stream`）+ `ratatui-textarea 0.9`（多行 + 软换行）+ `tui-markdown 0.3.9`（默认特性 `highlight-code` 开着 = syntect 高亮，代价是 oniguruma/`cc`）+ `catppuccin 2.8`（开 `ratatui` feature）+ `arboard` + `ignore`（`@` 索引）+ `unicode-width`。全在 `tui` feature 下。
- 状态栏在**最下方**：左侧常驻 `<模型> · <思考深度> · <目录> │ 用量 │ 余额`（千分位 + 百分比、不带 `tok`），右侧只有活动指示（spinner + 计时）贴右缘。余额查不到（非 DeepSeek 端点常见 404）就不显示且不再每回合白试。快照结构 `Snapshot::capture(&Session)` 是字段唯一来源。
- 输入框：`ENTER` 发送、`Shift+Enter`/`Ctrl+J` 换行、`Ctrl+A` 全选、`Ctrl+C`/`Ctrl+D`（空输入）退出、`Ctrl+G` **只认图片**；高度 = 文本行数 + 1 且 `MIN_H = 3`、上边框与光标**都跟随窗口焦点**（规则住在色板 `Palette::style_emphasis` / `style_caret`）：聚焦 = accent，失焦 = muted（**状态栏也一起变灰**）。
- **宽字符后半格的 diff 盲区**：`BufferDiff` 不重画宽字形后面那格（模型里它就是普通空格，与真空格全等）→ 被删汉字右半边 / placeholder 里 `⏎`·`⇧` 的碎片会**永久残留**。`input.rs` 的 `StaleTail` 每帧把「已写区间右边、上一帧写过的那段」标 `AlwaysUpdate` 强制重画（只在行变短那帧；静止帧零开销）；⚠ **只标已写区间之外**——写正文里那格会把汉字擦掉半个。光标压在宽字符上 = 2 格 accent **有意保留**。
- **终端光标每帧都摆到插入符上**（`App::run`：`draw` 之后 `terminal.set_cursor_position(Input::caret_position())`，光标本身仍隐藏）：IME 候选框的锚点是**终端光标单元格**，不摆就停在「上一帧 diff 最后写入的格子」——点一下消息流就会把候选框带到点击处。
- `/` 命令候选表 `palette::COMMANDS` 是唯一事实来源（`/help` 由它生成）；已移除的旧命令在 `palette::REMOVED`。`@` 路径补全用 `ignore`（`require_git(false)` + `git_global(false)` + `hidden(true)`）建 cwd 索引（`MAX_ENTRIES=20000`、`spawn_blocking`、回合结束后标 stale + 60s TTL）；接受目录时留「路径补全会话」可逐层钻。索引之外的目录也能列：`@..`/`@/`/`@~/` 走实时 `read_dir`（只列一层、不依赖索引；`~` 插入时展开成绝对路径，因为 `read` 不认 `~`）。
- 消息流**自己折行**（`history::layout` → 带逻辑行号的 `Row`）：滚动偏移按折行后的显示行数、复制按源文本切（`Layout::slice_text`：软换行不产生换行、宽字符不劈开）。鼠标捕获为滚轮常开 → 框选/拖选自算；写完剪贴板用长活 `Copier`（drop 就砸屏 + 复制不生效）；复制后右下角 Toast（`TOAST_TTL=3s`）。`Esc` 先收选区/面板再当停止键。
- lean 模式（`[tui] lean`，默认 true）只压工具活动那一行（失败/取消才跟正文块，`TOOL_BODY_LINES=12` 截断）；`!cmd` 手动 shell **不吃** lean。多行粘贴必须自己 `EnableBracketedPaste`（`ratatui::init()` 不开）。
- 超宽 Markdown 表格渲染后过 `markdown::fit_tables` 重排（`MarkdownCache` 的缓存键**要带宽度**）；`markdown.rs` 截断/指纹类字符串操作必须先退到字符边界（`floor_char_boundary`，裸切片遇中文会 panic 崩整屏）；`Line`/`Span` 里的 `\n` **不是换行**（会被挤掉）→ 多行提示要按 `text.lines()` 逐行 push。
- `Config.theme` **已被读取**（启动时 `Palette::resolve(&config.theme)`，认 `catppuccin` / `catppuccin-<flavor>` / 裸 flavor 名，认不出回 mocha + `[theme] …` 告警）：语义色板取自 `catppuccin` crate（槽位映射只有 `Palette::from_flavor` 一处；四个 flavor 快捷构造；不再手写 RGB）。映射 accent=blue / accent_text=crust / user=ok=green / assistant=text / tool=teal / fail=red / cancelled=overlay2 / muted=overlay0 / warn=yellow / code_bg=base。**仍缺** OSC 11 明暗自适应与 `/theme` 热切。视图层只认 `Palette` 字段名；**聚焦那几档样式也住在那儿**。

### 配置 / 提示词 / 记忆

- 配置只从 `~/.pie/config.toml` 读（`-c` / `PIE_CONFIG_FILE` / `PIE_DIR` 可重定向）。默认值只写在各自 `impl Default` 里（不再有 `DEFAULT_*` 常量）；整数字段统一 `usize`；`reserved_tokens` 认不出的字符串按「不发 max_tokens」处理；未知键忽略。`Config.tools`（`[tools.read]` 这种）按下划线私有参数注入，只注入下划线且不覆盖显式传参。
- 提示词分两类：`prompts/system.md` / `prompts/memory.md` 是**编译期 `include_str!`** 的内置正文（**小写文件名**），`SYSTEM.md` / `AGENTS.md` / `MEMORY.md` 是**运行时**从 cwd 往上找的文件（`find_project_root`：含任一提示词文件或 `.git` 的最近祖先）。本仓根没有 `SYSTEM.md` → 实际走内置那份；`AGENTS.md` 与 `MEMORY.md` 会被拼进 system prompt（改它们 = 改 agent 行为）。`ensure_global_memory()` 首跑写 `~/.pie/memory.md` 种子（已存在不覆盖）。
- **时间只有一个时钟出口**（`config.rs`）：`config::now() -> Duration` 是唯一碰 `SystemTime` 的地方，其余是**纯函数**——`fmt_local`（展示，本地偏移走 `localtime_r`）、`civil`（Hinnant 的 civil_from_days，私有）。**落盘统一 unix 秒数字**（manifest `ts` / `uploaded_at`），显示层才格式化；旧 manifest 里的 ISO `ts` 只被原样打印，不解析。

### Python 绑定（bindings/pie-py）

- PyO3 0.29 + maturin，包名 **`pie`**（`import pie`；扩展模块 `pie._pie_rs`），TUI 不进绑定；规划/进度见 `docs/python-bindings.md`。
- 已落地 M0–M3 + M5：`Config` / `LlmClient` / `ToolRegistry`（含 `@pie.tool` 注册 Python 工具）/ `Session` / `Cancel` / `run()` / `list_sessions()` + 事件回调 + 异常层级 + 类型存根（`mypy --strict` 干净）+ `aturn_async` / `events()`。**M4（abi3 wheel 分发）未做**。
- 三条约定：同步外观但**释放 GIL**（要并发用 `asyncio.to_thread`）；**一个 Session 同时只跑一个回合**（事件回调里别碰同一个 Session，要停就另线程 `stop()`）；`messages` / 事件都是 dict，字段名与 JSONL 一致。
- 构建/测试：`maturin develop`（`cargo build` 直接编 cdylib 会报一堆 Python 符号 undefined）+ `pytest tests`（本地假 SSE 端点，不联网；当前 27 例全绿）。asyncio 胶水在 `pie/_async.py`（事件从 tokio 线程经 `call_soon_threadsafe` 入队；`task.cancel()` 后要调 `session.stop()`）。
- ⚠ 本机 macOS 树里的 `bindings/pie-py/.venv` **是坏的**（`libpython3.12.dylib` 找不到，像是从 WSL 拷来的）→ 要跑绑定的 pytest 得另建 venv：`uv venv --python 3.12 /tmp/pie-py-venv` + `uv pip install --python … maturin pytest` + `VIRTUAL_ENV=… maturin develop`（实测 27 例全绿）。

## 构建与环境（本机特有）

- cargo 不在默认 PATH → 用 `~/.cargo/bin`。源码在 9p 盘时要 `CARGO_TARGET_DIR=$HOME/.cache/pie-target`（macOS 本地盘可省）。
- 公司代理 MITM crates.io → `~/.cargo/config.toml` 配了 `http.cainfo=~/.cargo/certs/bundle.pem` 与 `http.proxy`；`api.deepseek.com` **没被** MITM（系统根证书够用）→ reqwest 用 `rustls-tls-native-roots`（provider = ring），**不需要** libssl/pkg-config/cmake/perl，依赖只要 `cc`。
- 本机 nightly（1.100.0）格式串**不接受 `f` 类型**：`{x:.1f}` 报 `unknown format trait f` → 用 `{x:.1}`。
- 跨平台检查（rustup 装 target 会被代理证书挡住）：用 curl 下 `rust-std-<target>.tar.xz` 解到 `$(rustc --print sysroot)/lib/rustlib/`，再拿临时 crate 把要查的代码 `include!` 进去 `cargo check --target`（整包会被 `ring` 的 build script 挡住）。

## 踩坑记录

- **pty 驱动验证 TUI**：必须设 winsize（`ioctl(TIOCSWINSZ)`），否则界面根本不出现；断言屏幕内容用 pyte 还原（`screen.buffer[y][x].data` 自己拼文本），且**必须在 Ctrl+C 之前抓屏**（退出时 restore 会抹掉）。
- **字符串按字节切片前退到字符边界**（`markdown.rs::fingerprint` 曾因中文超 64 字节 panic 崩整屏、会话都没落盘）。
- **TUI 期间写 stderr = 砸屏** → 一律走 `log::warn`（进程级单槽 sink，`App::run` 装/卸，没装就 eprintln）。
- **PyO3**：`Python::with_gil`/`allow_threads` 改名 `attach`/`detach`，`detach` 闭包按引用捕获要求 `Sync`（`mpsc::Receiver` 得按值进闭包）；`bool::into_pyobject` 给 `Borrowed<PyBool>`（要 `.to_owned()`）；给异常实例挂属性得 `call1` 建实例再 `PyErr::from_value`。核心层现实：`Config::default()` 的 `compaction` 是**开的**（`Some`）、`Config`/`CompactStats` **没实现 `Serialize`**（绑定的 `to_dict`/统计 dict 手工拼，复用 `Config::to_toml()` 保证字段一处定义）。
- **修 TUI 的测试别用 `PIE_DIR` 环境变量隔离**（进程级，会与并行用例抢）；`/model`、`/thinking` 这类会写回配置的用例要自己给 `Config { config_file: Some(临时路径), .. }` —— 否则覆盖用户真实的 `~/.pie/config.toml`。
- **`prompts/` 里的内置正文用小写名**（`system.md` / `memory.md`）：代码是 `include_str!("../prompts/system.md")`，macOS 大小写不敏感照样编过，**Linux 上会直接编译失败**。
- 并行跑 `cargo test` 时偶发假失败（macOS）：刚 spawn 的子进程偶尔立刻以 SIGKILL 返回（`[exit=-1]`），表现为 `shell_*` 随机挂一条；单跑或 `--test-threads=1` 永远通过，与代码无关 → 挂了先重跑。

## 已知问题 / 待办

- **工具 panic 未文本化**（只捕 `Err(ToolError)`，panic 会带崩整个回合）。
- **工具执行进度未做**（TUI 看不到 bash 跑到一半的输出；`Tool::call` 已有 `ToolCtx`，加进度就在那里挂回调）。
- TUI 代码块**有**语法高亮（代价是带回 syntect → oniguruma/C 库）；`Config.theme` **已接线**，只差**明暗自适应**（无 OSC 11 探测）与 `/theme` 热切。
- 绑定 M4（abi3 wheel）未做（`pie setup` 只写默认值，不做交互式向导）。
- 工具 schema 没有逐字对拍契约测试（只留 `builtin_tool_names` 钉住注册名与顺序）→ 描述/参数一旦分叉就没有自动拦网。
