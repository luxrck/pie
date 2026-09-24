# MEMORY.md — 持久记忆

本文件是项目的持久记忆：记录用户偏好、当前系统概览、构建环境与踩坑。agent 每次会话开始读取，运行中用 edit/writ 更新。

保持简洁：只记跨会话仍然有效的事实。**历史关键决策与变更记录在 `docs/CHANGELOG.md`，旧 Python 版的历史与两版差异在 `docs/python-legacy.md`**（本文件只保「当前状态」）。

## 用户偏好

- 信奉 YOLO：不要权限确认、不要沙箱，工具直接执行。
- 偏好极简、可扩展的实现；能不引依赖就不引（自写 SSE、自建 `Cancel`、自算折行、零 C 依赖）。
- 工具名 `writ` / `bash`（旧 Python 版叫 `write` / `shell`，2026-09-23 用户点名改；**不是笔误，别再「修」回去**）。
- TUI 形态参考 codex：状态栏（chrome）在**最下方**，消息流从屏幕第一行开始。
- 内置提示词正文用**小写文件名**（`prompts/system.md`）——与运行时按名找的 `SYSTEM.md` 区分开。
- 命名口味：拿 `Config` 当参数 / 变量就叫 `config`（**不要 `cfg`**，用户点名改过）；绑定内部的核心配置叫 `core_config`。

## 项目定位与仓库

- **纯 Rust 项目**（2026-09-23 用户完成「重构代码到 Rust」）：Python 实现（`src/pie/*.py` + `tests/*.py` + `pyproject.toml` + `uv.lock`）已从本仓库删除；仓库根就是一个 Cargo 项目（package `pie-rs`、lib 名 `pie`、bin 名 `pie`）+ Python 绑定（`bindings/pie-py`）。
- 本机位置：macOS `/Users/luxrck/Projects/pie`（**当前实际改的这树**）；远端 `git@github.com:luxrck/pie.git`（只能走 SSH）。旧记录里还有 WSL 树 `/mnt/d/pie-master`（从 macOS 看不到，别假设它与这里同步）。
- 旧 **Python 版**（2026-09-23 删除）只作**历史参照**：「迁一块对一块」的 oracle 角色已完成——它的历史、旧版长什么样、与现版的逐条差异见 `docs/python-legacy.md`；查旧实现用 `git show b188058^:src/pie/xxx.py`，但**新代码一律以 Rust 为准**。

## 当前系统概览

### 分层与模块

- lib（`pie`）＝ 核心层：`config` / `llm` / `tools` / `session` / `context` / `cancel` / `log`；`tui` 在 `tui` feature 后面，clap 在 `cli` feature 后面。绑定用 `default-features = false` → TUI 依赖不进依赖图。
- `src/lib.rs` 是唯一声明模块的地方；`main.rs` 只 `use pie::…`（再写 `mod` 会变成第二份编译单元）。lib 目前**全 pub**（M0 状态，收紧要等绑定 API 稳定）。
- 原独立 `loop.rs` 已并入 `Session::aturn`；`Session` 自带 `llm` / `tools`，`new`/`load`/`resume` 都要传进来。

### 工具层

- 内置 4 个：`read` / `edit` / `writ` / `bash`。**一个工具 = 一个结构体**（字段即参数、doc 首行即 description、非 `Option` 即必填、`#[schemars(skip)]` = 私有参数），schema 由 serde + schemars 派生（无自写宏）；实现直接写在 `impl Tool` 的 `call` 里，辅助逻辑自包含在该 `call` 内，模块级只留共用的 `format_output`。
- 协议两条：trait 必须 `-> impl Future + Send`（impl 里可写 `async fn`）；`#[schemars(...)]` 必须写在 `#[derive(JsonSchema)]` 之后。分发用泛型 `erased::<T>` 单态化成函数指针（不经 `dyn Tool`）；注册 `.with_tool::<T>("名字")`。
- 输出统一 `Headers\n\nBody`：headers 一行一个 `[key=value]`，body 空则连空行都不给。**bash 失败才给头**（一行 `[exit=N, os=…, shell=…]`，2026-09-24 起三字段并一行），成功只有正文（成功且无输出 = 空串，端点实测 HTTP 200 收）；判成败只看第一行（`[exit=` 开头且不是 `[exit=0…` = 失败）。
- **落盘判据：不可再生才落盘**。bash 的 stdout → 全文落盘 + `[工具输出全文已保存: path]` 指针（超限只留**开头**，与 Python 保留尾部有意不同）；read 可再生 → 只补 `[已截断：可用 offset=N 继续读]`，不落盘。
- bash：`process_group(0)` + 取消/超时 `killpg(SIGKILL)`（只杀 `bash` 会被孙进程持有的管道卡住）；shell 名/选项只住 `impl Bash` 的 `const SHELL` / `SHELL_FLAG`（Unix `bash -c`，否则 `cmd /C`）。`--tools read,ls,grep` = read + 只允许 ls/grep 的受限 bash（复用 Bash 的私有参数 `_allow_cmds`，不另起工具类型，白名单在启动进程前校验）。
- edit 诊断齐（级联引用 / 出现多次 / 重叠 / 只差空白 + 「原文里最接近的位置 + 简版 diff」，自写不引依赖）。
- 图片识别用 `image` crate（`guess_format` + `into_dimensions`，只读头部）——**必须过格式白名单**（TIFF/ICO 它也认得，Python 不当图片）。

### 会话 / 上下文 / 图片

- 会话 JSONL 与 Python 版**同目录同格式**（`~/.pie/sessions/chat-<unix 秒>-<微秒>.jsonl`，首行 `__meta__` = usage/windows/cwd/title），两边可互读；文件名格式与 Python 不同（那边是本地时间），两边都按 mtime 选最新。`UsageTracker` 语义：token 字段是**最近一次上报值**（不求和），只有 `calls` 累加。
- **JSONL 落盘统一走 `session::json_line`**（= serde_json + `escape_control_chars`）：只转义「行分隔类」字符——C1（U+0080–U+009F）+ U+2028/U+2029；serde_json 只转义 C0，不补这一步则 `splitlines()` 那类读者会把 U+0085 当换行、一条消息被劈成两半（session 文件与 `/clear` 的窗口块都这么写）。
- resume 时按 `__meta__.windows` 重建窗口摘要（旧 system 会被丢掉后按当前提示词重建；**不重建模型就看不到归档历史**）。`full_history()` 把压缩指针展开成完整转录（工具级回到落盘全文，并把原消息头区拼回去——`[exit=N]` 在头区、不在落盘件里）。
- 三级压缩（`context.rs`）：工具级（head/tail 指针）/ 轮次级 / 会话级；级别只升不降、内容 hash 落盘、软阈值与目标水位迟滞（相对 `context_budget = context_window - reserved_tokens`）、`keep_last_steps` 保护最近 N 个 step 批次。消息是扁平 `Vec<Message>`（靠 `role` + `compress_level` + `synthetic` 判定），压缩就地改写列表；消息模型与 Python 的类层级不同。
- 压缩元数据字段（`compress_level` / `raw_path` / `raw_hash` / `raw_len` / `raw_tokens` / `synthetic`）**绝不能进 API 请求体** → 发模型前统一过 `Message::to_api()`（tool 的 content 兜空串、assistant 带 tool_calls 必须带 `reasoning_content`）。
- 目录分工：压缩落盘 `~/.pie/context/`（`context gc` 的地盘）、`/clear` 归档的窗口块 `~/.pie/windows/`（gc 不碰）、图片副本 `~/.pie/files/`。manifest 落盘 `indent=1`、`ts` 用 **unix 秒数字**（落盘时间统一数字，旧的 ISO 串只被原样显示）。`clear_window()` 写盘失败就不动窗口（宁可不清也不丢历史）。
- **图片只走 Files API，不回退 base64**：read 到图 → 本地内容寻址副本（`img-<sha256[:16]>`，0o600）→ 上传拿 `file_id` → 注入一条 `synthetic` user 消息 `{"type":"file","file_id":…}`。拿不到 `file_id` 就不注入图（标记文本仍在）；`file_id` 失效 → `downgrade_file_blocks` 把历史里的 `file` 块换成文本占位 + 标失效（下次同图重传）+ 重试一次。分层：协议在 `llm.rs`（`upload_file`/`list_files` 自动翻页/`delete_file` + `model_supports_files`/`key_fingerprint`/`TTL_MAX_DAYS`），本地文件管理在 `session.rs`（副本、`__meta__.files`、GC、`files list|gc` 数据源）；时间换算统一在 `config.rs`。
- ⚠ `expires_after` **只能用方括号展开的表单字段**发：`expires_after[anchor]=created_at` + `expires_after[seconds]=N`（发 JSON 串 → 响应 `expires_at` 为 null = 被当永久件收下，TTL 静默失效）。本地副本与 `__meta__.files` 字段名与 Python 逐字一致（两边记录互认）；`files gc` 有 24h mtime 保护窗（刚粘贴还没 read 的图不算垃圾）。
- 事件形状：`TurnEvent::{AssistantText, Reasoning, ToolCall{name, arguments}, ToolResult{name, content, arguments}, Answer}`。`ToolCall` 曾经还有 `turn` / `step`（第几轮 / 本回合第几次模型调用），**2026-09-24 删除**（仓库内没有活着的读者：TUI 忽略、CLI 那条 `[tNsM]` 日志跑不到，只有绑定的事件 dict 在转发）。
- 取消（`cancel.rs`）：`AtomicBool` + `Notify`（**`notify_waiters` 不补发** → 先查标志再 `notified().await`）。三个取消点：`aturn` 每步开头、模型请求 `select!` race（返回 `Ok(None)`、不 push 消息）、shell 等待时 race + `killpg`。收尾必须保证 API 序列合法：未执行的 `tool_call` 补 `CANCEL_TEXT` 的 tool 消息 + 历史写一条 `CANCEL_TEXT` 的 assistant 消息 + 作为本轮答复返回。
- **模型请求失败也留一条 assistant**：`aturn` 里两处 `model_call` 出错的退出路径都先 `push_error_turn`（内容 `[请求失败] <错误>`，前缀常量 `ERROR_TURN_PREFIX`）再 `return Err` —— `push_user` 已经进了一条 user，不补就留下没人应答的提问（历史里连续两条 user）。调用方仍拿到 `Err`。
- TUI 侧取消只有 `Esc`（`/stop` 已移除，busy 时输入会提示按 Esc）；退出时先 cancel 再等锁保存。

### 模型层（llm.rs）

- reqwest + **手写 SSE**，不含任何 SDK；`GET /user/balance` 查余额（响应金额是字符串）、`GET /models` 列模型。
- 重试收敛成**唯一驱动器** `LlmClient::with_retry(what, op, decide)` + 判定枚举 `Retry::{Backoff, Now, Give}`；`complete`/`list_models` 用默认判定 `default_retry`（额度 = `1 + max_retries`，只认 408/409/429/5xx 与传输层异常），`stream` 的两条特例写在它自己的 `decide` 里（**已吐过增量 → Give**；400 拒 `stream_options` → 改写开关 + `Now`，不占额度）。`Retry-After` 优先并夹在 `[1.0, 60.0]`，否则 `max(1.0, random(0, max_retry_delay_seconds))`。三个 `*_once` 已退役（一次尝试的流程就在闭包里）。
- 重试进度走 `log::progress(key, …)` 按「哪个请求」分组，TUI 侧**就地更新同一块**（`Cell::Retry`），不是每重试一行。
- 上下文两笔账：`context_window`（输入+输出一起算）与 `reserved_tokens`（= 发给 API 的 `max_tokens`，`None`/`auto` = 不发）。服务端按「输入 + max_tokens ≤ 窗口」**预检**，超了直接 400 → 水位必须相对 `context_budget` 算。窗口大小服务端只在超限报错里告诉你（`GET /models` 只给 id）；本部署 deepseek-flash 实测窗口 1,048,576、`max_tokens` 合法区间 `[1, 393216]`。

### CLI（main.rs）

- 模式判定：**真 TTY 且无任务 → TUI**；有任务 → 一次性（子 agent，不落盘，stdout 只有答案、工具活动不打印；**但带 `-r`/`-s` 就是会话模式**：载入会话 → 跑一个回合 → 存回同一文件）；非 TTY 且无任务 → 一行提示 + 退出码 2；无任务时从 stdin 读。
- 会话模式（`-r`/`-s` + 任务）**失败也落盘**：`aturn` 返回 `Err` 时先 `let _ = session.save()` 再 `return 1`（`aturn` 已补一条 `[请求失败] <错误>` 的 assistant，不存就整轮消失）；成功路径照旧 `save()` 后返回 0。
- 覆盖项（只改内存 cfg、不写回文件，集中在 `apply_overrides`）：`-m/-t/--reserved-tokens(--max-tokens)/--auto-compact-threshold/--timeout-seconds/--max-retries/--max-retry-delay-seconds/--cwd/--system-prompt/--append-system-prompt/--stat/--mode {text,json,transcript}`；`-t` 校验七档 + `none`（`off` 归一到 `none`）。
- `setup`（补齐 `~/.pie/` 缺的默认件：`config::ensure_config_file` 写一份 `Config::default()`，`config::ensure_global_memory_file` 写 `prompts/memory.md` 种子）在 `run()` 里**早于** `Config::load` 与启动时那发记忆种子——配置缺失/坏掉正是它要修的情形，也保证「已创建」是真的。两个助手都幂等、返回 `(路径, 是否新建)`、已存在一律不覆盖。
- ⚠ `--max-steps` / `--no-stream` **不进 Config**：它们是按次旋钮，直接传给每个 `Session::aturn`（TUI 经 `tui::run(session, max_steps, stream)` 带下来）。
- 子命令：`setup`、`sessions [-l/--all/--json]`、`context info|verify|gc [--delete]`、`files list [--all]` / `files gc [--delete] [--all]`。`--models`、`--stat`、`-r/--resume`、`-s/--session <id|路径>`（存在则载入否则新建）。
- 与旧 Python 版仍存的差异（有意）：TTY 下带任务这边走一次性（旧版进 TUI）、没有 `-p/--print`、`-V` 大小写不同、`--tools` 内置名是 `read/edit/writ/bash`；`setup` 是**非交互**的（只补齐缺的默认文件，不做旧版那个问答向导）。

### TUI（src/tui/）

- 依赖：`ratatui 0.30` + `crossterm 0.29`（`event-stream`）+ `ratatui-textarea 0.9`（多行 + 软换行 `WrapMode::Glyph`）+ `tui-markdown 0.3.9`（默认特性 `highlight-code` 开着 = syntect 高亮，代价是 oniguruma/`cc`）+ `catppuccin 2.8`（开 `ratatui` feature：色槽可直接 `into()` 成 `Color::Rgb`，只多一条已在树里的 ratatui-core 边）+ `arboard` + `ignore`（`@` 索引）+ `unicode-width`。全在 `tui` feature 下（绑定不开 `tui` → 这些都不进依赖图）。
- 状态栏在**最下方**：左侧常驻 `<模型> · <思考深度> · <目录> │ 用量 │ 余额`（千分位 + 百分比、不带 `tok`；没上报就按 0 算；没给窗口连分隔符都不画），右侧只有活动指示（spinner + 计时）贴右边缘。余额查不到（非 DeepSeek 端点常见 404）就不显示且不再每回合白试。快照结构 `Snapshot::capture(&Session)` 是字段唯一来源（`App` 不另存副本）。
- 输入框：`ENTER` 发送、`Shift+Enter`/`Ctrl+J` 换行、`Ctrl+A` 全选、`Ctrl+C`/`Ctrl+D`（空输入）退出、`Ctrl+G` **只认图片**（无图提示，不像 Python 的 TextArea 那样回退文本粘贴——文本粘贴交给终端自己的键经 bracketed paste）、高度 = 文本行数 + 1 且 `MIN_H = 3`、上边框与光标**都跟随窗口焦点**（规则住在色板：`Palette::style_emphasis` / `style_caret`）：聚焦 = 上边框 accent（`!` 开头切成工具色）+ 光标 accent 亮块（空输入也亮），失焦 = 两者降为 muted（**状态栏也一起变灰**：名字与 spinner/耗时换 muted，用量与余额本来就 muted）。
- **宽字符后半格的 diff 盲区**（2026-09-24 修）：ratatui 的 `BufferDiff` 不会重画宽字形后面那格（模型里它就是普通空格，`Cell::eq` 与真空格全等）→ 被删汉字右半边 / placeholder 里 `⏎`·`⇧` 的碎片 / 光标涂过的 accent 会**永久残留**。`input.rs` 的小控件 `StaleTail` 每帧把「已写区间右边、上一帧写过的那段」标 `CellDiffOption::AlwaysUpdate` 强制重画（只在行变短那帧；静止帧零开销）；⚠ **只标已写区间之外**——写正文里那格会把汉字擦掉半个。光标压在宽字符上 = 2 格 accent（控件把光标样式涂在字形那格，与选区同款）**有意保留**。
- **终端光标每帧都摆到插入符上**（`App::run`：`draw` 之后 `terminal.set_cursor_position(Input::caret_position())`，光标本身仍隐藏）：IME 候选框的锚点是**终端光标单元格**（VS Code 的 xterm 把隐藏 textarea 摆在那儿、`compositionstart` 时再同步一次），不摆就停在「上一帧 diff 最后写入的格子」——点一下消息流就会把候选框带到点击处。
- `/` 命令候选表 `palette::COMMANDS` 是唯一事实来源（`/help` 由它生成）；已移除的旧命令（`/stop` `/paste` `/quit`）在 `palette::REMOVED` 里给出替代提示。`@` 路径补全用 `ignore`（`require_git(false)` + `git_global(false)` + `hidden(true)`：非 git 目录也认 `.gitignore`、个人全局忽略不藏文件、点文件一概不收）建 cwd 索引（`MAX_ENTRIES=20000`，`spawn_blocking`、回合结束后标 stale + 60s TTL）；接受目录时留「路径补全会话」可逐层钻，接受文件则结束。索引之外的目录也能列：`@..`/`@/`/`@~/` 分别实时列上级/根/主目录（只列一层、不算忽略规则，也不依赖索引——所以不为它们扫盘；`~` 插入时展开成绝对路径，因为 `read` 不认 `~`）。
- 消息流**自己折行**（`history::layout` → 带逻辑行号的 `Row`）：滚动偏移按折行后的显示行数、复制按源文本切（`Layout::slice_text`：软换行不产生换行、不同逻辑行才换行、宽字符不劈开）。鼠标捕获为滚轮常开 → 框选/输入框拖选都得自己算；写完剪贴板用长活 `Copier`（写完就 drop 会砸屏 + 复制不生效）；复制后右下角 Toast（`TOAST_TTL=3s`，不占消息流）。`Esc` 先收选区/面板再当停止键。
- lean 模式（`[tui] lean`，默认 true）：成功的工具活动只留一行（失败/取消才跟正文块，缩进 2 空格、`TOOL_BODY_LINES=12` 截断）；`!cmd` 手动 shell **不吃** lean（用户主动看输出），命令行回显与结果都套盒子。
- 多行粘贴必须自己 `EnableBracketedPaste`（`ratatui::init()` 不开）：否则终端不用 `\x1b[200~` 包住粘贴内容，多行文本被拆成按键 → 在第一行就发出去。
- 超宽 Markdown 表格渲染后过 `markdown::fit_tables` 重排（`MarkdownCache` 的缓存键**要带宽度**）；`markdown.rs` 截断/指纹类字符串操作必须先退到字符边界（`floor_char_boundary`，裸切片遇中文会 panic 崩整屏）。`Line`/`Span` 里的 `\n` **不是换行**（会被挤掉）→ 多行提示要按 `text.lines()` 逐行 push。
- `Config.theme` **已被读取**（TUI 启动时 `Palette::resolve(&config.theme)`，认不出回 mocha + `[theme] …` 告警；认 `catppuccin` / `catppuccin-<flavor>` / 裸 flavor 名）：`theme.rs` 的语义色板取自 `catppuccin` crate（槽位映射只有 `Palette::from_flavor(FlavorName)` 一处；`mocha`/`macchiato`/`frappe`/`latte` 四个快捷构造；2026-09-24 起不再手写 RGB）。映射：accent=blue / accent_text=crust / user=ok=green / assistant=text / tool=teal / fail=red / cancelled=overlay2 / muted=overlay0 / warn=yellow / code_bg=base。**仍缺**：OSC 11 明暗自适应（族名固定按深色 Mocha）、`/theme` 热切（色板启动时定下，`markdown` 缓存不跟色板失效）。视图层只认 `Palette` 字段名，槽位名只出现在 `theme.rs`。**聚焦相关的那几档样式也住在那儿**（`style_emphasis` / `style_caret` / `style_selection`）：全界面共一条「失焦就 muted」的规则，别在视图里再写一遍。

### 配置 / 提示词 / 记忆

- 配置只从 `~/.pie/config.toml` 读（`-c` / `PIE_CONFIG_FILE` / `PIE_DIR` 可重定向）。默认值只写在各自 `impl Default` 里（不再有 `DEFAULT_*` 常量）；整数字段统一 `usize`（写负数在解析期就报错），`reserved_tokens` 认不出的字符串按「不发 max_tokens」处理；未知键忽略（如旧配置里的 `verbose`，2026-09-24 已删）。`Config.tools`（`[tools.read]` 这种）按下划线私有参数注入，只注入下划线且不覆盖显式传参。
- 提示词分两类：`prompts/system.md` / `prompts/memory.md` 是**编译期 `include_str!`** 的内置正文（**小写文件名**），`SYSTEM.md` / `AGENTS.md` / `MEMORY.md` 是**运行时**从 cwd 往上找的文件（`find_project_root`：含任一提示词文件或 `.git` 的最近祖先）。本仓根没有 `SYSTEM.md` → 实际走内置那份；`AGENTS.md` 与 `MEMORY.md` 会被拼进 system prompt（改它们 = 改 agent 行为）。
- `config::ensure_global_memory()` 首跑写 `~/.pie/memory.md` 种子（已存在不覆盖），在 `main::run` 开头调。
- **时间只有一个时钟出口**（`config.rs`，2026-09-24）：`config::now() -> Duration` 是唯一碰 `SystemTime` 的地方（秒 / `subsec_micros` / `subsec_nanos` 都从它取），其余是**纯函数**——`fmt_local`（展示，本地偏移走 `localtime_r`）、`civil`（Hinnant 的 civil_from_days，不引日期库，私有）。**落盘统一 unix 秒数字**（manifest `ts` / `uploaded_at`），显示层才格式化；旧 manifest 里的 ISO `ts` 只被原样打印，不解析。

### Python 绑定（bindings/pie-py）

- PyO3 0.29 + maturin，包名 **`pie`**（`import pie`；扩展模块 `pie._pie_rs`），TUI 不进绑定；规划/进度见 `docs/python-bindings.md`。
- 已落地 M0–M3 + M5：`Config` / `LlmClient` / `ToolRegistry`（含 `@pie.tool` 注册 Python 工具）/ `Session` / `Cancel` / `run()` / `list_sessions()` + 事件回调 + 异常层级 + 类型存根（`mypy --strict` 干净）+ `aturn_async` / `events()`。**M4（abi3 wheel 分发）未做**。
- 三条约定：同步外观但**释放 GIL**（要并发用 `asyncio.to_thread`）；**一个 Session 同时只跑一个回合**（事件回调里别碰同一个 Session，要停就另线程 `stop()`）；`messages` / 事件都是 dict，字段名与 JSONL 一致。
- 构建/测试：`maturin develop`（`cargo build` 直接编 cdylib 会报一堆 Python 符号 undefined）+ `pytest tests`（本地假 SSE 端点，不联网；当前 27 例全绿）。asyncio 胶水在 `pie/_async.py`（事件从 tokio 线程经 `call_soon_threadsafe` 入队；`task.cancel()` 后要调 `session.stop()`，否则 tokio 任务不停）。
- ⚠ 本机 macOS 树里的 `bindings/pie-py/.venv` **是坏的**（`libpython3.12.dylib` 找不到，像是从 WSL 拷来的）→ 要跑绑定的 pytest 得另建 venv：`uv venv --python 3.12 /tmp/pie-py-venv` + `uv pip install --python … maturin pytest` + `VIRTUAL_ENV=… maturin develop`（本次实测 27 例全绿）。

## 构建与环境（本机特有）

- cargo 不在默认 PATH → 用 `~/.cargo/bin`。源码在 9p 盘时要 `CARGO_TARGET_DIR=$HOME/.cache/pie-rs-target`（macOS 本地盘可省）。
- 公司代理 MITM crates.io → `~/.cargo/config.toml` 配了 `http.cainfo=~/.cargo/certs/bundle.pem` 与 `http.proxy`；`api.deepseek.com` **没被** MITM（系统根证书够用）→ reqwest 用 `rustls-tls-native-roots`（provider = ring），**不需要** libssl/pkg-config/cmake/perl，依赖只要 `cc`。
- 本机 nightly（1.100.0）格式串**不接受 `f` 类型**：`{x:.1f}` 报 `unknown format trait f` → 用 `{x:.1}`。
- 跨平台检查（rustup 装 target 会被代理证书挡住）：用 curl 下 `rust-std-<target>.tar.xz` 解到 `$(rustc --print sysroot)/lib/rustlib/`，再拿临时 crate 把要查的代码 `include!` 进去 `cargo check --target`（整包会被 `ring` 的 build script 挡住）。

## 踩坑记录

- **pty 驱动验证 TUI**：必须设 winsize（`ioctl(TIOCSWINSZ)`），否则 ratatui 拿不到尺寸、界面根本不出现；断言屏幕内容用 pyte 还原（`screen.buffer[y][x].data` 自己拼文本——这版 `screen.display` 会 IndexError），且**必须在 Ctrl+C 之前抓屏**（退出时 restore 会抹掉）。
- **字符串按字节切片前退到字符边界**（`markdown.rs::fingerprint` 曾因中文超 64 字节 panic 崩整屏、会话都没落盘）。
- **TUI 期间写 stderr = 砸屏**（raw mode + 交替屏 + ratatui 只重画变化格子）→ 一律走 `log::warn`（进程级单槽 sink，`App::run` 装/卸，没装就 eprintln）。
- **PyO3**：`Python::with_gil`/`allow_threads` 改名 `attach`/`detach`，`detach` 闭包按引用捕获要求 `Sync`（`mpsc::Receiver` 得按值进闭包）；`bool::into_pyobject` 给 `Borrowed<PyBool>`（要 `.to_owned()`）；给异常实例挂属性得 `call1` 建实例再 `PyErr::from_value`。核心层现实：`Config::default()` 的 `compaction` 是**开的**（`Some`）、`Config`/`CompactStats` **没实现 `Serialize`**（绑定的 `to_dict`/统计 dict 手工拼，复用 `Config::to_toml()` 保证字段一处定义）。
- **修 TUI 的测试别用 `PIE_DIR` 环境变量隔离**（进程级，会与并行用例抢）；`/model`、`/thinking` 这类会写回配置的用例要自己给 `Config { config_file: Some(临时路径), .. }` —— 否则覆盖用户真实的 `~/.pie/config.toml`。
- **`prompts/` 里的内置正文用小写名**（`system.md` / `memory.md`）：上次「搬到仓库根」的提交把它改成了 `SYSTEM.md`，而代码是 `include_str!("../prompts/system.md")` —— macOS 大小写不敏感照样编过，**Linux 上会直接编译失败**。已改回小写（与 README 的说法一致）。
- 并行跑 `cargo test` 时偶发假失败（macOS）：刚 spawn 的子进程偶尔立刻以 SIGKILL 返回（`[exit=-1]`），表现为 `shell_*` 随机挂一条；单跑或 `--test-threads=1` 永远通过，与代码无关 → 挂了先重跑。

## 已知问题 / 待办

- **工具 panic 未文本化**（只捕 `Err(ToolError)`，panic 会带崩整个回合）。
- **工具执行进度未做**（TUI 看不到 bash 跑到一半的输出；`Tool::call` 已有 `ToolCtx`，加进度就在那里挂回调）。
- TUI 代码块**有**语法高亮（`tui-markdown` 默认特性 `highlight-code`，2026-09-24 开；主题用内置 Base16 Ocean Dark）——代价是带回 syntect → oniguruma（C 库）；`Config.theme` **已接线**（2026-09-24；只差明暗自适应与 `/theme` 热切）。
- 绑定 M4（abi3 wheel）未做（`setup` 的问答向导没迁，也不需要：`pie setup` 只写默认值）。
- 工具 schema 与旧 Python 版的**逐字对拍契约测试已删**（2026-09-23 用户要求，连同 `fixtures/`），只留 `builtin_tool_names` 钉住注册名与顺序 → 描述/参数再分叉就没有自动拦网了。
- 查旧 Python 实现的入口：`git show b188058^:src/pie/…`（仓库里已没有 `.py` 实现，只剩绑定外壳与测试）；旧版历史/旧版长什么样/两版逐条差异见 `docs/python-legacy.md`。
