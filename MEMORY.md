# MEMORY.md — 持久记忆

本文件是项目的持久记忆：记录用户偏好、当前系统概览、构建环境与踩坑。agent 每次会话开始读取，运行中用 edit/writ 更新。

保持简洁：只记跨会话仍然有效的事实。**历史关键决策与变更记录在 `docs/CHANGELOG.md`**（本文件只保「当前状态」；工作约定见 `AGENTS.md`）。

## 用户偏好

- 信奉 YOLO：不要权限确认、不要沙箱，工具直接执行。
- 偏好极简、可扩展的实现；能不引依赖就不引（自写 SSE、自建 `Cancel`、自算折行、零 C 依赖）。
- **功能尽量内联**：不要给单一消费者写私有辅助函数（嵌套 `fn` 可以，多一层就得多一个理由）——`config.rs::fmt_local` 原本拆出的 `civil` / `local_utc_offset` 就是被点名合并的例子。
- **不要为了方便测试而拆功能**（用户明确要求）：判据只有「有没有第二个消费者」；`config.rs::pie_dir` 的搜索顺序当初拆出过 `pick_root` 只为好测 → 已内联，接受「`<cwd>/.pie` 那档没有单测」。
- 工具名 `writ` / `bash`（**不是笔误，别再「修」回去**）。
- TUI 形态参考 codex：状态栏（chrome）在**最下方**，消息流从屏幕第一行开始。
- 内置提示词正文用**小写文件名**（`prompts/system.md`）——与运行时按名找的 `SYSTEM.md` 区分开。
- 命名：拿 `Config` 当参数 / 变量就叫 `config`（**不要 `cfg`**）；绑定内部的核心配置叫 `core_config`。

## 项目定位与仓库

- **纯 Rust 项目**（2026-09-23 完成「重构代码到 Rust」；是 [`earendil-works/pi`](https://github.com/earendil-works/pi)（TypeScript 的 agent harness）的 **Rust 重实现**，名字取自 π 的谐音 → pie）：仓库根就是一个 Cargo 项目（package `pie`、lib 名 `pie`、bin 名 `pie`）+ Python 绑定（`bindings/pie-py`）。
- 本机位置：macOS `/Users/luxrck/Projects/pie`（**当前实际改的这树**）；远端 `git@github.com:luxrck/pie.git`（只能走 SSH）。旧记录里还有 WSL 树 `/mnt/d/pie-master`（从 macOS 看不到，别假设它与这里同步）。

## 当前系统概览

### 分层与工具层

- lib（`pie`）＝ 核心层：`config` / `llm` / `tools` / `session` / `context` / `cancel` / `log`；`tui` 在 `tui` feature 后面，clap 在 `cli` feature 后面（绑定 `default-features = false` → TUI 依赖不进依赖图）。`src/lib.rs` 是唯一声明模块的地方；原独立 `loop.rs` 已并入 `Session::aturn`。
- 内置 4 个工具：`read` / `edit` / `writ` / `bash`。**一个工具 = 一个结构体**（字段即参数、doc 首行即 description、非 `Option` 即必填、`#[schemars(skip)]` = 私有参数），实现直接写在 `impl Tool` 的 `call` 里，模块级只留共用的 `format_output`。
- 输出统一 `Headers\n\nBody`：headers 一行一个 `[key=value]`，body 空则连空行都不给。**bash 失败才给头**（一行 `[exit=N, os=…, shell=…]`），成功只有正文（成功且无输出 = 空串）；判成败只看第一行（`[exit=` 开头且不是 `[exit=0…` = 失败）。
- **落盘判据：不可再生才落盘**。bash 的 stdout → 全文落盘 + 指针（超限只留**开头**）；read 可再生 → 只补 `[已截断：可用 offset=N 继续读]`，不落盘。**落盘路径是结构化的**（2026-09-30）：`Tool::call` 返回 `ToolOutput { text, spill }`，消息层用 `Message::mark_compressed(1, spill)` 设 `raw_path` / `compress_level`（`mark_compressed` 是四条落盘路径的唯一出口）；`extract_spill_path` / `mark_tool_spill` 已删（文本嗅探 + 工具名 gate + `exists()` 双保险一起没了）。
- bash：`process_group(0)` + 取消/超时 `killpg(SIGKILL)`（只杀 `bash` 会被孙进程持有的管道卡住）；shell 名/选项只住 `impl Bash` 的 `const SHELL` / `SHELL_FLAG`。`--tools read,ls,grep` = read + 只允许 ls/grep 的受限 bash（复用 `_allow_cmds`，白名单在启动进程前校验）。
- edit 诊断齐（级联引用 / 出现多次 / 重叠 / 只差空白 + 最接近位置 + 简版 diff，自写不引依赖）；图片识别用 `image` crate 只读头部，**必须过格式白名单**（TIFF/ICO 它也认得，不该当图片）。

### 会话 / 上下文 / 图片

- **`cli.rs` = 磁盘维护的批量逻辑**（只读、不改会话状态）：`list_sessions` / `iter_session_files` / `file_id_index` / `collect_file_garbage` / `SessionInfo` / `GC_PROTECT_HOURS`（2026-09-29 从 `session.rs` 搬出来）+ **`referenced_raw_paths` / `collect_context_garbage`**（2026-09-30 从 `context.rs` 搬来，`context verify|gc` 的数据源；`absolutize` 是它的私有辅助）。模块名是**按用途**取的（CLI 子命令的数据源），TUI（同一二进制）与 Python 绑定也在用；`session.rs` 里留的是会话本身（加载 / 落盘 / 回合循环 / 图片上传）。
- 会话 JSONL 固定在 `~/.pie/sessions/chat-<unix 秒>-<微秒>.jsonl`（首行 `__meta__` = usage/windows/cwd/title；按 mtime 选最新）。`UsageTracker`：token 字段是**最近一次上报值**（不求和），只有 `calls` 累加。
- **JSONL 落盘统一走 `session::json_line`**（= serde_json + `escape_control_chars`）：只转义「行分隔类」字符——C1（U+0080–U+009F）+ U+2028/U+2029（serde_json 只转 C0），否则 `splitlines()` 那类读者会把 U+0085 当换行、一条消息被劈成两半（session 文件与 `/clear` 的窗口块都这么写）。
- resume 时按 `__meta__.windows` 重建窗口摘要（**不重建模型就看不到归档历史**）。`full_history()` 把压缩指针展开成完整转录（工具级回到落盘全文，并把原消息头区拼回去——`[exit=N]` 在头区、不在落盘件里）。
- 三级压缩（`context.rs`）：工具级（head/tail 指针）/ 轮次级 / 会话级（整窗口归档）；级别只升不降、内容 hash 落盘、`keep_last_steps`（住在 `[compaction.tool]`）保护最近 N 个 step 批次。消息是扁平 `Vec<Message>`（靠 `role` + `compress_level` + `synthetic` 判定），压缩就地改写列表。**水位只认服务端上报**（2026-09-30 起：`maybe_compact(messages, config, reported)` 只看上一次 `usage.prompt_tokens`，`None` = 本次会话还没发过请求 → 不压），**不做 token 估算**（`message_tokens` / `messages_tokens` / `content_tokens` / `Message.raw_len` / `raw_tokens` 全删）。**自动压缩只做工具级/轮次级**（A+，2026-09-30）：**第三级（会话级）只由用户手动 `/clear` 触发**（`Session::clear_window` → `context::compact_session(messages, config, storage)`：`config` 就是 `&SessionCompaction`；整窗口 → `windows/`，重开成 system + 摘要链）—— 换窗口会让当前轮的工作记忆只剩摘要（模型「失忆」），自动做不合适。判据失手靠「400 上下文超限 → 再强压一次工具级/轮次级再发；压不动就报错（提示 `/clear`）」兜底（`LlmError::is_context_overflow`）。
- 本地专有字段（`compress_level` / `raw_path` / `synthetic` / `thought_ms`）**绝不能进 API 请求体** → 发模型前统一过 `Message::to_api()`。`thought_ms` = 这条 assistant 回复前「思考」了多久（**只进会话文件**：实时视图那行 `• Thought for 3.4s` 靠它才能在 `-r` 之后还原；非流式量不到 → None）。
- 目录分工：压缩落盘 `~/.pie/context/`（`context gc` 的地盘）、`/clear` 归档的窗口块 `~/.pie/windows/`（gc 不碰）、图片副本 `~/.pie/files/`。压缩事件流水**住在会话文件首行**（`__meta__.compaction_events`，与消息里的 `raw_path` 同一趟车落盘；2026-09-29 起不再单独写 `context/*.manifest.jsonl`）→ `pie context info` / `verify` / `gc` 只统计 `sessions/` 里的会话（`-s <其它路径>` 存在别处的会话不在内，命令输出里会写明这一步）。`clear_window()` 写盘失败就不动窗口（宁可不清也不丢历史）。
- **压缩事件是类型化的 `context::CompactEvent`**（2026-09-30）：落盘形状 `{kind, ts, path, hash, summary?}`（`#[serde(tag = "kind", rename_all = "lowercase")]`，variant `Tool` / `Turn` / `Session` ↔ `kind` 值）。`level` 与 `raw_` 前缀**都已从落盘移除**（要数字用 `level()`、路径用 `raw_path()`；旧会话里的 `level` 被忽略、`raw_path`/`raw_hash` 靠 `#[serde(alias)]` 读回——`referenced_raw_paths` 是裸读 `Value`，那里 `path` 优先 / `raw_path` 回退）。`maybe_compact` / `compact` 返回 `(CompactStats, Vec<CompactEvent>)`，**没有 `on_compact` 回调参数**（事件是内部记账：gc 保护原文 / `/stat` 计数 / resume 登记窗口；不走 `on_event` 展示流）。`Session.compaction_events: Vec<CompactEvent>`；`/clear` 归档也走 `CompactEvent::session(...)`（不再手搓 JSON）。
- **磁盘布局 + 落盘都在 `config::Storage`**：`root` + `sessions()` / `context()` / `files()` / `windows()` + **`store(StoreType) -> PathBuf`**（只返回路径；`config` 那边三个一队：`hash_id(data)` 内容→hash、`name_of(path)` 路径→文件名主干（= id）、`hash_of(path)` 路径→hash 段）。`StoreType` 三形态**全是内容寻址**（名字里都是 sha256 前 16 位，同内容只落一份、已存在则不动）：`Blob`（图片副本，`files/img-<hash><ext>`，0o600 + 原子写；**只有它留扩展名**——那是用户数据，双击要能打开）/ `Raw`（压缩原文，`context/<前缀>-<hash>`）/ `Window`（`/clear` 窗口块，`windows/window-<hash>`；2026-09-29 起也内容寻址，不再带 unix 秒）。`Config.storage` 是默认值，`ToolCtx.storage` 把它带进工具层（bash 全文落盘用）。旧名 `context::write_raw` / `context::write_window_block` / `session::store_blob` **已删**（调用点直接 `storage.store(...)`）；`content_hash` / `image_hash_id` 已合并成 `config::hash_id(data)`（2026-09-29）；取 id / hash 段用 `config::name_of(path)` / `config::hash_of(path)`（2026-09-29 从 `session.rs` 搬进 `config` 并改名，三个函数挨着放）；四个 `config::*_dir()` 自由函数已删。`Message.raw_hash` 是无读者的死字段，已删（流水里的 `raw_hash` 保留：`context info` 会打印给人看）。
- **数据根搜索顺序**（`config::pie_dir`）：`PIE_DIR` → 当前目录下的 `.pie`（**存在**才算，不凭空造）→ `~/.pie`；顺序住纯函数 `pick_root(env, cwd, home)`（测试不必改进程 cwd）。`Storage::default()` / `Config::default()` 只解析一次，之后一路显式传。
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
- 覆盖项集中在 `apply_overrides`（只改内存 cfg、不写回文件）：`-m/-t/--reserved-tokens(--max-tokens)/--auto-compact-threshold/--timeout-seconds/--max-retries/--max-retry-delay-seconds/--system-prompt/--append-system-prompt/--stat/--mode {text,json,transcript}`；`-t` 校验七档 + `none`（`off` 归一到 `none`）。
- `--cwd` **不在** `apply_overrides`（它在 `run()` 里、子命令分派之前）：`set_current_dir` 之后**重解析 `config.storage`**（`Storage::from_env()`）——数据目录跟着工作目录走（`PIE_DIR` 仍优先；新 cwd 下有 `.pie` 才换）。TUI 里同一个口径的命令是 `/cd`。
- `setup`（`config::ensure_config_file` 写 `Config::default()`，`config::ensure_global_memory_file` 写 `prompts/memory.md` 种子）在 `run()` 里**早于** `Config::load` 与启动时那发记忆种子（配置缺失/坏掉正是它要修的情形）；两助手都幂等、已存在一律不覆盖。
- ⚠ `--max-steps` / `--no-stream` **不进 Config**：按次旋钮，直接传给每个 `Session::aturn`（TUI 经 `tui::run(session, max_steps, stream)` 带下来）。
- 子命令：`setup`、`sessions [-l/--all/--json]`、`context info|verify|gc [--delete]`、`files list [--all]` / `files gc [--delete] [--all]`，另有 `--models`、`--stat`、`-r/--resume`、`-s/--session <id|路径>`（存在则载入否则新建）。

### TUI（src/tui/）

- 依赖：`ratatui 0.30` + `crossterm 0.29`（`event-stream`）+ `ratatui-textarea 0.9`（多行 + 软换行）+ `tui-markdown 0.3.10`（默认特性 `highlight-code` 开着 = syntect 高亮，代价是 oniguruma/`cc`；**表格折行交上游** `Options::table_width(w)`，默认 `None` = 自然宽度）+ `catppuccin 2.8`（开 `ratatui` feature）+ `arboard` + `ignore`（`@` 索引）+ `unicode-width`（显示宽度）+ `unicode-segmentation`（**词级折行**的词边界：只给 `WrapMode::WordOrGlyph` 档用）。全在 `tui` feature 下。
- 状态栏在**最下方**：左侧常驻 `<模型> · <思考深度> · <目录> │ 用量 │ 余额`（千分位 + 百分比、不带 `tok`），右侧只有活动指示（spinner + 计时）贴右缘。余额查不到（非 DeepSeek 端点常见 404）就不显示且不再每回合白试。快照结构 `Snapshot::capture(&Session)` 是字段唯一来源。
- 输入框：`ENTER` 发送、`Shift+Enter`/`Ctrl+J` 换行、`Ctrl+A` 全选、`Ctrl+C`/`Ctrl+D`（空输入）退出、`Ctrl+G` **只认图片**；高度 = 文本行数 + 1 且 `MIN_H = 3`、上边框与光标**都跟随窗口焦点**（规则住在色板 `Palette::style_emphasis` / `style_caret`）：聚焦 = accent，失焦 = muted（**状态栏也一起变灰**）。边框交控件渲染（`set_block`），内容区（`inner_rect()`）就是渲染时从同一个块里取的那块（记在 `Input::inner`）；插入符位置问控件 `screen_cursor()`；滚轮悬在输入框上且它能滚时滚输入框，否则滚消息流。
- **宽字符后半格的 diff 盲区**：`BufferDiff` 不重画宽字形后面那格（模型里它就是普通空格，与真空格全等）→ 被删汉字右半边 / placeholder 里 `⏎`·`⇧` 的碎片会**永久残留**。`input.rs` 的 `StaleTail` 每帧把「已写区间右边、上一帧写过的那段」标 `AlwaysUpdate` 强制重画（只在行变短那帧；静止帧零开销）；⚠ **只标已写区间之外**——写正文里那格会把汉字擦掉半个。光标压在宽字符上 = 2 格 accent **有意保留**。
- **终端光标每帧都摆到插入符上**（`App::run`：`draw` 之后 `terminal.set_cursor_position(Input::caret_position())`，光标本身仍隐藏）：IME 候选框的锚点是**终端光标单元格**，不摆就停在「上一帧 diff 最后写入的格子」——点一下消息流就会把候选框带到点击处。
- `/` 命令候选表 `palette::COMMANDS` 是唯一事实来源（`/help` 由它生成）；已移除的旧命令在 `palette::REMOVED`。`/cd <路径>` = **切工作目录**：`set_current_dir` + 重解析 `storage`（跟 `--cwd` 同口径）+ 往**会话历史与消息流**各插一条 system 说明（`已切换工作目录：<路径>`；历史那条是给模型的上下文）+ 作废 `@` 索引（`file_index = None`）；`~` 会展开。`@` 路径补全用 `ignore`（`require_git(false)` + `git_global(false)` + `hidden(true)`）建 cwd 索引（`MAX_ENTRIES=20000`、`spawn_blocking`、回合结束后标 stale + 60s TTL）；接受目录时留「路径补全会话」可逐层钻。索引之外的目录也能列：`@..`/`@/`/`@~/` 走实时 `read_dir`（只列一层、不依赖索引；`~` 插入时展开成绝对路径，因为 `read` 不认 `~`）。
- 消息流**自己折行**（`Pane::layout`，每条消息一个 `Cell`，`CellBlock::rebuild(&Cell, palette, width, lean)` 重建自己的显示行（`CellBlock::render` 里那个 match 就是"这条消息长什么样"）；**增量**：宽度/简洁/配色没变且某条 cell 的指纹没变 → 那条整块复用，只有变了的才重折）：滚动偏移按折行后的显示行数、复制按源文本切（`Pane::slice_text`：软换行不产生换行、宽字符不劈开、行首装饰按 `Row::indent` 跳过）。**消息流的折行与行首宽度都只住 `pane.rs` 一处**：`CellBlock::push_line` 是唯一折行出口（折行宽度 = `width - indent`），`WrapMode`（两档：**生产走 `WordOrGlyph`** = 词级断 + 超长词退回逐字断，规则对齐 `ratatui-textarea`；`Glyph` = 纯逐字硬断。没有 `WRAP_MODE` 常量，档位就是 `CellBlock::push_line` 里那个字面量）、`wrap_segments`、`PREFIX_CELLS`（用户消息 `› ` 悬挂缩进宽度）、`char_width` 都在那儿；`Row` 自描述（`indent` + `continues`）；`Cell` 是**纯数据**（`&[Cell]` 即可排版），`App.pane: pane::Pane` 只持 `frame_key` + **`blocks: Vec<CellBlock>`**（与 `cells` 下标一一对应）；每块 = 那条 cell 的显示行 + 指纹 + **它自己的 markdown 缓存**（三样捆一起，不可能错位），`is_stale` / `rebuild` / `render` / `push_line` / `end_cell` / `markdown_lines` 都是它的方法——**拿到 `&mut CellBlock` 就物理上只能写这一条**。`Pane::total()` = 行总数、`Pane::iter()` = 按序迭代（`App` 只用这三个接口）。**输入框是普通编辑器**：软换行固定 `WrapMode::Glyph`（**不跟消息流对齐**：它复刻控件的字素级逐字折行）、不带 `› `；**几何不共用**（它的 `screen_rows` 只复刻控件那份，供命中 / 框高 / 残影 / 视口滚动用）。鼠标捕获为滚轮常开 → 框选/拖选自算；写完剪贴板用长活 `Copier`（drop 就砸屏 + 复制不生效）；复制后右下角 Toast（`TOAST_TTL=3s`）。`Esc` 先收选区/面板再当停止键。消息流右缘**固定**留 1 列当**滚动条**（只有溢出才画 thumb；点/拖只改消息流偏移，输入框/状态栏不受影响）——固定留是因为折行宽度与 md 缓存键都吃 `width`。
- ⚠ **主循环一轮 = 一帧 × 一整队回合事件**：`run` 的 `select!` 收到一个 `UiEvent::Turn` 后立刻 `drain_turn_events` 把积压的整队吃掉（**别退化成一帧一个事件**：长思考一次几百上千个增量，会被帧率钉死成每秒几十个 → 界面落后现实几十秒；实测 debug 构建、19k 行时一帧 ≈ 13ms）。事件的状态更新很便宜（`push_str` / 压 cell），贵的是**渲染**（那一帧），所以「消化整队 + 一帧只画一次」还顺带省掉中间的 markdown 重解析。
- ⚠ **渲染是每帧全量重排**（`Pane::layout` 走完全部 cells + 全量克隆行给 `Paragraph`；唯一缓存是 markdown 渲染，住 `App.pane.md`）：每个终端事件 / 流式 delta / 66ms tick 都一帧，无 dirty 标记（ratatui diff 只省终端写入）。实测与治法（① 复用上一帧排版——`rows` 与缓存在 `Pane` 里同住，最容易做 ② 只重排尾部 cell ③ 只克隆可见行）见 `docs/CHANGELOG.md` 2026-09-28。
- lean 模式（`[tui] lean`，默认 true）只压工具活动那一行（失败/取消才跟正文块，`TOOL_BODY_LINES=12` 截断）；`!cmd` 手动 shell **不吃** lean。多行粘贴必须自己 `EnableBracketedPaste`（`ratatui::init()` 不开）。
- Markdown 表格的宽度预算交给上游（`tui-markdown 0.3.10` 的 `Options::table_width`，含边框/内边距/外层引用前缀；pie 自写的 `fit_tables` 已删）；`MarkdownCache` 的缓存键**要带宽度**（表的折行结果依赖宽度）；`markdown.rs` 截断/指纹类字符串操作必须先退到字符边界（`floor_char_boundary`，裸切片遇中文会 panic 崩整屏）；`Line`/`Span` 里的 `\n` **不是换行**（会被挤掉）→ 多行提示要按 `text.lines()` 逐行 push。
- `Config.theme` **已被读取**（启动时 `Palette::resolve(&config.theme)`，认 `catppuccin` / `catppuccin-<flavor>` / 裸 flavor 名，认不出回 mocha + `[theme] …` 告警）：语义色板取自 `catppuccin` crate（槽位映射只有 `Palette::from_flavor` 一处；四个 flavor 快捷构造；不再手写 RGB）。映射 accent=blue / accent_text=crust / user=ok=green / assistant=text / tool=teal / fail=red / cancelled=overlay2 / muted=overlay0 / warn=yellow / code_bg=base。**仍缺** OSC 11 明暗自适应与 `/theme` 热切。视图层只认 `Palette` 字段名；**聚焦那几档样式也住在那儿**。

### 配置 / 提示词 / 记忆

- 配置只从 `~/.pie/config.toml` 读（`-c` / `PIE_CONFIG_FILE` / `PIE_DIR` 可重定向）。默认值只写在各自 `impl Default` 里（不再有 `DEFAULT_*` 常量）；整数字段统一 `usize`；`reserved_tokens` 认不出的字符串按「不发 max_tokens」处理；未知键忽略。`Config::load` 读完后按环境变量覆盖 `OPENAI_API_KEY` → `api_key` 与 `OPENAI_BASE_URL` → `base_url`（空串/全空白 = 没设；只改内存、不写回文件；`Config::default()` 不受影响）。`Config.tools`（`[tools.read]` 这种）按下划线私有参数注入，只注入下划线且不覆盖显式传参。`Config.storage: Storage` 是**运行时属性**（`#[serde(skip)]`，与 `config_file` 同批）——默认 root 住 `config::Storage::from_env()`（搜索顺序见「会话 / 上下文 / 图片」那条）。
- 提示词分两类：`prompts/system.md` / `prompts/memory.md` 是**编译期 `include_str!`** 的内置正文（**小写文件名**），`SYSTEM.md` / `AGENTS.md` / `MEMORY.md` 是**运行时**从 cwd 往上找的文件（`find_project_root`：含任一提示词文件或 `.git` 的最近祖先）。本仓根没有 `SYSTEM.md` → 实际走内置那份；`AGENTS.md` 与 `MEMORY.md` 会被拼进 system prompt（改它们 = 改 agent 行为）。`ensure_global_memory()` 首跑写 `~/.pie/memory.md` 种子（已存在不覆盖）。
- **时间只有一个时钟出口**（`config.rs`）：`config::now() -> Duration` 是唯一碰 `SystemTime` 的地方，其余是**纯函数**——`fmt_local`（展示，本地偏移走 `localtime_r`）、`civil`（Hinnant 的 civil_from_days，私有）。**落盘统一 unix 秒数字**（压缩事件的 `ts` / `uploaded_at`），显示层才格式化；旧会话里的 ISO `ts` 只被原样打印，不解析。

### Python 绑定（bindings/pie-py）

- PyO3 0.29 + maturin，包名 **`pie`**（`import pie`；扩展模块 `pie._pie_rs`），TUI 不进绑定；规划/进度见 `docs/python-bindings.md`。
- 已落地 M0–M3 + M5：`Config` / `LlmClient` / `ToolRegistry`（含 `@pie.tool` 注册 Python 工具）/ `Session` / `Cancel` / `run()` / `list_sessions()` + 事件回调 + 异常层级 + 类型存根（`mypy --strict` 干净）+ `aturn_async` / `events()`。**M4（abi3 wheel 分发）未做**。
- 三条约定：同步外观但**释放 GIL**（要并发用 `asyncio.to_thread`）；**一个 Session 同时只跑一个回合**（事件回调里别碰同一个 Session，要停就另线程 `stop()`）；`messages` / 事件都是 dict，字段名与 JSONL 一致。
- 构建/测试：`maturin develop`（`cargo build` 直接编 cdylib 会报一堆 Python 符号 undefined）+ `pytest tests`（本地假 SSE 端点，不联网；当前 27 例全绿）。asyncio 胶水在 `pie/_async.py`（事件从 tokio 线程经 `call_soon_threadsafe` 入队；`task.cancel()` 后要调 `session.stop()`）。
- ✅ 本机 `bindings/pie-py/.venv` 已用 uv 重建可用（CPython 3.12 + maturin/pytest，editable 安装）——直接 `.venv/bin/python -m pytest tests -q`（27 例全绿）。重建法：`uv venv --python 3.12 .venv` → `VIRTUAL_ENV=$PWD/.venv uv pip install maturin pytest` → `VIRTUAL_ENV=$PWD/.venv .venv/bin/maturin develop`。改 Python 外壳（`python/pie/*.py`）即时生效，改 Rust 要重跑 `maturin develop`（默认 debug；要快就 `--release`）。
- 陈旧残留：`python/pie_rs/`（无 `__init__.py`）是旧 module-name 时期的未跟踪 `.so`，被当 namespace package 收进来，`import pie_rs._pie_rs` 会加载过期二进制——与本包无关，别被它误导。

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
- **修 TUI 的测试别用 `PIE_DIR` 环境变量隔离**（进程级，会与并行用例抢）；`/model`、`/thinking` 这类会写回配置的用例要自己给 `Config { config_file: Some(临时路径), .. }` —— 否则覆盖用户真实的 `~/.pie/config.toml`。目录隔离现在有更好的路子：`Config { storage: config::Storage::at(临时目录), .. }`（落盘都走 `storage.store(...)`，不再读 `PIE_DIR`）——存量用例仍沿用 `PIE_DIR` + `env_lock`（只迁了 `context::tests::gc_keeps_referenced_files_only` 作示范）。
- **`prompts/` 里的内置正文用小写名**（`system.md` / `memory.md`）：代码是 `include_str!("../prompts/system.md")`，macOS 大小写不敏感照样编过，**Linux 上会直接编译失败**。
- 并行跑 `cargo test` 时偶发假失败（macOS）：刚 spawn 的子进程偶尔立刻以 SIGKILL 返回（`[exit=-1]`），表现为 `shell_*` 随机挂一条；单跑或 `--test-threads=1` 永远通过，与代码无关 → 挂了先重跑。

## 已知问题 / 待办

- **工具 panic 未文本化**（只捕 `Err(ToolError)`，panic 会带崩整个回合）。
- **工具执行进度未做**（TUI 看不到 bash 跑到一半的输出；`Tool::call` 已有 `ToolCtx`，加进度就在那里挂回调）。
- TUI 代码块**有**语法高亮（代价是带回 syntect → oniguruma/C 库）；`Config.theme` **已接线**，只差**明暗自适应**（无 OSC 11 探测）与 `/theme` 热切。
- 绑定 M4（abi3 wheel）未做（`pie setup` 只写默认值，不做交互式向导）。
- 工具 schema 没有逐字对拍契约测试（只留 `builtin_tool_names` 钉住注册名与顺序）→ 描述/参数一旦分叉就没有自动拦网。
- **`-s <外部路径>` 的会话不在 `context gc` / `verify` 的统计里**（2026-09-29 引入：压缩流水从 `context/*.manifest.jsonl` 搬进会话首行 `__meta__.compaction_events`，于是“记录跟着会话文件走”，不再有固定落在 `context/` 的 manifest 顺带保护它）→ 那种会话引用的原文会被 `gc --delete` 回收（指针落空：优雅降级、但输出查不回来）。**用户拍板：先记 TODO、暂不修**（现在 `context info` / `gc` / `--help` 都已写明“只统计 `<sessions>` 里的会话”）。修法（待定）：`save()` 时给不落在 `sessions/` 里的会话另落一份引用索引（一行路径 或 `<stem>.refs`），`gc` / `verify` 读它。
