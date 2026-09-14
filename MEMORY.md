# MEMORY.md — 持久记忆

本文件是项目的持久记忆：记录用户偏好、当前系统概览与踩坑记录。agent 在每次会话开始时读取，并可在运行中用 edit/write 更新。

保持简洁：只记跨会话仍然有效的事实，不要记临时状态。
历史关键决策与变更记录已移至 `docs/CHANGELOG.md`（本文件只保留当前状态）。

## 用户偏好

- 信奉 YOLO：不要权限确认，工具直接执行。
- 偏好极简、可扩展的实现。
- TUI 背景透明：露出终端窗口背景色（Ubuntu 终端默认深红色），不画自己的背景色。

## 当前系统概览

- **架构**：分层包（tools / llm / loop / config / chat / context / cli），src 布局，可全局安装（`uv tool install . --editable`）。内置工具仅 read / edit / write / shell，`@tool()` 按签名生成 schema，LLM 协议可注入。
  **Session 的三个路径/集合字段**（名字要一眼分清）：`path` = 会话 JSONL 自身的文件路径；`windows` = `/clear` 归档的历史窗口块（落在 `~/.pie/windows/`）；`files` = 图片 id 表（hash_id → file_id/本地副本/过期时间，见 `files.py`，存进 `__meta__`）。旧名 `file` / `fs` 已于 2026-09 改名（`__meta__` 里的 `fs` 键仍兼容读）。
- **CLI 与交互**：`pie`（新对话）/ `pie resume`（恢复最近）/ `pie` 带参数（一次性子 agent，不写 sessions）/ `pie context info|verify|gc`；`-c/--config` 指定配置。真实终端走 Textual TUI，非 TTY 回退 readline/prompt_toolkit；输入以 `!` 开头走 shell 模式（不进上下文）。运行时命令：`/stat`（用量）、`/thinking`（切换思考深度）、`/model <id>`（切换模型，启动自动拉取可用列表）、`/compact [tools|turns]` 手动压缩（无参 = 工具级+轮次级）、`/stop` 或 `Esc`（取消当前任务：补全面板开着时 Esc 只收起面板）、`/clear`（切换窗口）。
- **工具**：所有内置工具输出统一为 `Headers\n\nBody`（headers 一行一个 `[...]`，body 空则省略空行；状态/元数据放 header 区，如 shell 首行 `[exit=..]`、截断时 `[工具输出全文已保存: path]`、行号 `[行 a-b，共 N 行]`）。read 支持 offset/limit 分页 + 图片多模态 + 私有容量上限（_max_lines/_max_bytes/_max_image_bytes）；edit 为 edits 数组（oldText 唯一、防重叠、按原文非增量）；shell 支持私有容量上限（超限只保留尾部，全文落盘指针）。
- **上下文压缩**：三级——工具级（按行保留 head+tail 落盘指针）/ 轮次级 / 会话级，摘要全部规则式（零模型调用）。工具级保护窗口由 `keep_last_steps` 决定（最近 N 个 step 批次，跨轮次滚动）；轮次级只保护当前轮、已完成轮次可压。内容 hash 落盘 + 指针链 + 压缩级别只升不降 + 软阈值 / 目标水位迟滞（比例在 `[compaction]` 的 `soft_ratio` / `target_ratio`，**相对可用输入预算 = `context_window` - `reserved_tokens`**，默认 80% / 55%）；manifest 索引 + verify/gc。`compaction` 为显式配置：默认 None 不做任何压缩，写了则三级全开（子表调 head/tail，`tool/turn/session = false` 关闭对应级）。
- **模型层**：OpenAI 兼容接口，`reasoning_effort` 透传；`reasoning_content` 捕获/持久化/原样回传（thinking 模式 400 已修）。采集 provider usage（prompt/completion）+ UsageTracker 跨 resume 累计。**上下文账**：`context_window`（模型窗口，输入 + 输出一起算）+ `reserved_tokens`（每次请求为输出预留，就是发给 API 的 `max_tokens`，`None`/`"auto"` = 不发、用服务端默认）→ `Config.context_budget() = 二者相减`；`/stat` 与 system prompt 都按这份预算展示。
- **图片（`read` → 多模态注入）**：read 返回标记文本，harness 把图片作为**多模态 user 消息**注入（图片只能在 user 消息里）。默认走 **Files API**：先落本地内容寻址副本 `~/.pie/files/<hash_id>`（`hash_id = img-<sha256[:16]>`）→ **从副本上传**拿 `file_id` → 历史里只留 `{"type":"file","file_id":…}`。记录按会话存 `__meta__.files`（无全局缓存）；未命中/过期/换 key/模型不支持/上传失败 → 静默回退内联 base64。维护：`pie files list|gc`。
- **一套主题适配深色/浅色终端（主题族 + 终端背景探测）**：`termbg.py` 的 `detect_dark_background()`（进程内缓存）按 OSC 11 查询 → `COLORFGBG` → None 的顺序判断终端背景明暗（OSC 11 直接读写 `/dev/tty`，临时关规范模式/回显、0.2s 超时、读完恢复；纯函数 `parse_osc11` 认 `rgb:`/`rgba:` 的 2/4 位分量，按 sRGB 加权亮度 < 0.5 判深色；`parse_colorfgbg` 认背景索引 < 8 为深色）。`theme.py` 引入**主题族**：`THEME_FAMILIES = {"catppuccin": (CATPPUCCIN_MOCHA, CATPPUCCIN_LATTE)}`，`get_theme(name, dark=None)` 对族名按明暗选变体（探测不到按深色），具体变体名（`catppuccin-mocha`）固定不受影响；`Config.theme` 默认 `"catppuccin"`，`PieApp.__init__` 探测一次并据此选 palette + Textual 主题（`ansi-dark`/`ansi-light`，两者 background 都是 ansi_default → 仍透明）。用户配置已由 `catppuccin-mocha` 改为 `catppuccin`（否则不会自适应）。
- **TUI 主题**：`theme.py` 集中所有展示数据（配色 + 图标，不依赖 Textual/Rich），四类取用：`Theme.role_border(role)`（未知 role 回退 system；`cancelled` 与 system 同色）、`Theme.role_icon(role)`、`Theme.tool_icon(name)`（**调用**框图标：`tool_icons` 按工具名覆盖，未配置回退 `icon_tool_call`，值 `""` = 无图标）、`Theme.result_icon(role, name)`（**结果**框图标：按 role 取状态字形，可被 `tool_result_icons` 覆盖）、`Theme.lean_mark(status)`（简洁模式行首标记，status ∈ ok/fail/cancelled，未知回退 ok）。
  **「执行结果」字形只有一套**：`icon_ok(✅)` / `icon_error(❌)` / `icon_cancelled(⏹)`——盒子模式作 role=tool_result/error/cancelled 的标题图标（`↳` 那套「这是上一个框的产出」已废），简洁模式同一套放行首；取哪个由 tui 侧按执行结果判定（退出码/失败前缀/取消）。角色轴与结果轴分开：**调用**框图标回答「调的是哪个工具」（`tool_icons` + `tool_result_icons` 是对结果字形的 opt-in 覆盖），结果框图标回答「结果如何」。默认主题：`tool_icons={"read": "✧", "edit": "⟱", "write": "✦", "shell": "❯"}`，`tool_result_icons={}`。原则：**视图层不再出现图标字面量**。tui.py 的 `_box(icon=None, tool="")` 优先级：显式 icon > 工具图标 > role 图标。
- **TUI 显示**：断行是 CJK 友好的——tui.py 导入时调 `textkit.install_cjk_wrap()`（全角字逐字可断、英文词不断，纯 ASCII 交回 Rich 原实现）；`tui.py` 的 `SelectableRichLog` 框选复制按「源文本 + 显示行对齐」切来源文本（不是拼显示行），长行复制为一行。tui.py 只做应用编排（布局/命令/回合 worker/流式），live 事件与 resume 历史共用 `_render_tool_call/_render_tool_result/_notify`。
- **TUI 简洁模式**（`[tui] lean = true`）：只给 user / assistant 套盒子，工具调用/结果压成单行 `Text`（`[状态标记|工具图标] 工具名 摘要`——结果行 ✅/❌/⏹ 在**行首**，失败时下方 `→ 正文` 续行），外面套 `Padding(text, (0, BOX_INSET))` 与盒内正文对齐。`BOX_INSET` 由盒子几何推导（`_BOX_BORDER + _BOX_PADDING[1]`，`_box()` / `_render_assistant_stream()` 共用 `_BOX_PADDING`）——别另写 2；`#log` 的 CSS padding 与这件事无关（同一内容区，改它不破坏对齐）。标记放行首是关键：`Text(no_wrap=True, overflow="ellipsis")` 的右截断天然保住它，所以没有自定义 renderable（`_LeanLine` 已删，`Measurement`/`set_cell_size`/`LEAN_RIGHT_MARGIN` 一并删；`_single_line()` 仍必须：真换行在 `no_wrap` 下照样断行、tab 会撑破宽度）。代价：lean 下结果行不再用工具身份图标——状态即图标。状态标记字形在主题里（`icon_ok/icon_error/icon_cancelled` + `palette.lean_mark(status)`，与盒子模式结果框标题同一套），tui 侧只负责「按执行结果选 status」。改 lean 渲染后跑 `tests/test_tui.py`（`test_lean_tool_lines_are_padded` 含「框选复制不带留白」断言）。
- **配置**：只从 `~/.pie/config.toml` 读取（保留 `PIE_DIR` / `PIE_CONFIG_FILE` 路径重定向）；`Config.tools`（dict，按工具名设默认私有参数，如 read: {_max_lines,_max_bytes,_max_image_bytes}）由 dispatch/adispatch 注入下划线私有参数（`_inject_tool_defaults`，只注入下划线且不覆盖显式传参）；SYSTEM.md / AGENTS.md / MEMORY.md / `~/.pie/memory.md` 存在即注入 system prompt。

## 踩坑记录

### 注解与类型

- `from __future__ import annotations` 使注解变成字符串 → `@tool()` 用 `get_type_hints(fn)` 解析真实类型。
- `str | None` 的 `get_origin` 在 Python 3.13 返回 `types.UnionType`、3.14 返回 `typing.Union`（两者合一）→ 判断要写 `origin is Union or origin is types.UnionType`；全局 uv 工具环境（3.13）与项目 .venv（3.14）可能不同，跨版本改动要两环境都实测。

### 输入与终端

- Textual TextArea 在 ansi 主题（App 用 ansi-dark）下选中高亮会多一条 `text-style: reverse`（来自 `&:ansi .text-area--selection`）：`#input .text-area--selection` 这类 ID 规则能覆盖 background/color，但**覆盖不了没声明的 text-style**（未声明 ≠ 清除）→ 选中呈反色，和 #log 鼠标框选（accent 底 + accent_text 字）看着正好相反。要统一必须显式写 `text-style: none`。
- TUI 长流式文本别塞非滚动 Static（超出可视区的行被裁掉，视觉停更）→ 窗口化尾部或换 RichLog auto_scroll。
- **RichLog 里 Markdown 的颜色不受 Textual 主题 / palette 控制**：RichLog 用 `App.console` 渲染，而 Textual 创建的该 console **没设 theme** → Rich 的 `DEFAULT_STYLES` 生效（`markdown.code = bold cyan on black`、`markdown.h2 = underline magenta`、表格/列表青、链接亮蓝）→ 行内代码/代码块是黑底 + 终端 ANSI 青，浅色终端下糊成一片。2026-09-14 曾用 `Theme.rich_styles()` + `App.console.push_theme(...)` 把 Markdown 全量接管到 palette（含 `code_bg`/`code_fg`/`code_theme`，浅色变体配 `friendly`、深色配 `monokai`），**后按用户要求回退**（字段/方法已删）。如今只留**最小覆盖**：palette 字段 `markdown_code`（**完整样式串**，空串 = 不覆盖）+ `Theme.markdown_styles()`，**只有浅色变体 latte 有值**（现为 `"bold cyan"`——去底色只留青色粗体字；`markdown_styles()` **原样使用**该值，不再自动拼 `bold`），`on_mount` 非空时 `push_theme` 只改 `markdown.code` / `markdown.code_block` 两键；mocha 不注入，继续走 Rich 默认。fence（带语言标注的代码块）底色来自高亮主题的 token（`#272822`），`markdown.code_block` 治不到——另用 palette 字段 **`code_theme`**（pygments 主题名）经 `Markdown(code_theme=...)` 传入：mocha = `monokai`（= Rich 默认，等于不动）、latte = `solarized-light`（暖白底 `#fdf6e3`，用户手改自早期的 `friendly`）。回归：`tests/test_tui.py::test_markdown_code_styles` + `tests/test_theme.py::test_markdown_styles_are_minimal` / `test_code_theme_follows_variant`。
- **工具输出进 RichLog 前必须清洗控制符/ANSI 转义**：Rich 排版把不可见转义字节计成宽度（`cell_len("\x1b[01;32mA\x1b[0m")` = 10 对可见 1）→ Panel 量出的内容宽比实际大，盒子的顶/底边框与内容行右边框不在一列（`ls --color` 必现）。清洗走 textkit.py 的 `strip_escapes()` / `rich_text()`（SGR 交 `Text.from_ansi` 解成样式，其余转义 + C0 剔除，`\r\n`→`\n`、孤立 `\r` 删）。顺序要紧：**先剔转义再剔控制符**，否则 OSC 的结束符 BEL(`\x07`) 先没，`\x1b]...` 的匹配会吞掉后面全部文本。
- `input()` 在部分终端（WSL/mintty）退格按字节删中文会截断 → 改用 prompt_toolkit 行编辑。
- **框选复制要按「源文本」切，不能按显示行拼**：`RichLog.lines` 是软换行后的显示行，且 Rich 在盒内换行点会直接吃掉那个空格（`'aaaaaa bbbb cccc' + 'dddd…'`）→ 按显示行拼接既多出换行又丢空格。`tui.py` 的 `SelectableRichLog` 每次 write 记 `_CopyEntry(row, count, src, spans)`，复制时按字符区间切 src（同一 src 的相邻显示行合并 → 空格/换行都回来），无映射才回退。src 取法：Panel→盒内正文、Text→`.plain`（先 `expand_tabs()`，与显示一致，含 tab 的输出才对得上）、Markdown→宽渲染(4096)纯文本（`.markup` 与显示行对不上）。对齐失败即回退，不猜。两个必须容忍的显示装饰（实测踩坑）：Rich 给 Markdown **列表续行加悬挂缩进**（源文本里没有），给**引用每行重复加 `▌ `**（源文本只首个有）——故行首空白一律不计内容（只计入 x0），整行匹配失败时再试去掉行首 `▌` 并把忽略字符数记进 span `(start, end, off)`。否则一个列表项就能让整条消息回退。分隔线 `---` 会被渲染成随宽度铺满的规则线 → 无宽度无关源文本 → 回退（可接受）。
- 显示层换行：已改成 **CJK 友好断行**——`textkit.py` 的 `install_cjk_wrap()`（tui.py 导入时调用）把 `rich.text.divide_line` 换成 `cjk_divide_line`（全角字 `cell_len == 2` 各自成一个 token，非全角串仍按词，其余逻辑同 Rich；纯 ASCII 交回原实现逐字节不变）。否则 Rich 只按空白断行：中文长句没空格 → 整段挪到下一行、上一行留白（宽 74 实测只用 42 格）。零宽断点（U+200B）无效：Python `\s` 不匹它、Rich 分词认不出。
- 命令行参数有长度上限，长任务走 stdin 或文件指针，不要塞进 argv。

### 会话与文件

- 会话文件名秒级时间戳同秒碰撞会互相覆盖 → 文件名加微秒 `%f`。
- `/save` 自定义路径时 manifest 按文件名 stem 查找可能错位（已知边缘问题，待修）。

### 子进程与取消（/stop）

- **agent shell 工具必须 `create_subprocess_shell(..., start_new_session=True)` + killpg 杀进程组**：只 `proc.kill()` 杀 `/bin/sh` 时子孙进程（真正干活的）变孤儿并持有 stdout 管道写端，Python 3.12+ asyncio 的 `wait()` 要等 stdio 管道 EOF 才返回 → 取消/超时被卡到子孙自然退出（实测 sleep 300 卡 299s）。终止统一走 `_terminate_proc_group`（killpg SIGKILL 整个进程组，pipe 立即 EOF）；kill 后 `wait()` 限时 3s，防 D-state（不可中断内核态）进程拖死取消/超时路径。TUI !shell 的 `_exec_shell_async` 同款（start_new_session + killpg + wait 限时）。
- **loop._wait_cancellable 取消后不无限等清理**：`await asyncio.wait({task}, timeout=CANCEL_GRACE=3.0)`，超时则清理后台继续、先终止回合；别用 `asyncio.gather(task, ..., return_exceptions=True)` 无限等 task 结束。

### 上下文压缩

- `keep` 类参数要 clamp `max(1, keep)`（0 等价 1，当前轮必须保留），否则 `user_idx[m]` 越界崩。
- 轮次进行中不做 pair 提取、pair 仅当轮次以 assistant 结尾时提取，否则产生 user→纯文本 assistant→tool_calls→tool 非法序列被 DeepSeek 400。
- 工具返回全文，落盘统一在 harness 边界做（eager spill），工具内部不截断（截断会丢信息）。—— 例外：read/shell 支持配置容量上限（_max_lines/_max_bytes），截断时用 `write_raw` 落盘全文并返回独立指针标记，信息不丢。指针格式必须是独立的 `[工具输出全文已保存: path]`（`[` 紧挨可选前缀 + `全文已保存:`），不能嵌在其它文字里，否则 `extract_spill_path`（`_SPILL_RE`）匹配不到。

### API / 模型（DeepSeek thinking）

- **Vision / Files API 实测（2026-09）**：上传 `POST /files`（purpose=user_data）后，用 `{"type":"file","file_id":…}` 引用，`deepseek-flash` 确实看得到图；**prompt_tokens 与内联 base64 完全一致**（计费按尺寸，单图 ≤1024，与编码无关）——换 Files API 省的是**请求体/重复传输/上限**，不是钱（3 MB 图：内联 body 4 MiB ↔ file_id 187 B）。
- **`expires_after` 要进 `extra_body`**（openai SDK 不认这个 DeepSeek 扩展字段），带上后响应才有 `expires_at`；**服务端不做内容去重**（同图传两次 = 两个 file_id），所以“不重传”完全靠本地记录。
- **失效 file_id 的报错可识别**：`400 … the following file_ids do not exist or are not created under your account`（`files.is_stale_file_error` 认它）→ 触发“把历史里的 file 块降级成内联 + 重试一次”。
- **上下文窗口与输出预留是两笔账**：服务端按「输入 tokens + `max_tokens` ≤ 窗口」**预检**，超了直接 400（`BadRequestError`）且 `loop` 不兜底 → 整个回合被打断。所以压缩软阈值必须相对 `context_window - reserved_tokens` 算（2026-09 已改），按整个窗口 × 比例会在输入还没到水位时就把请求发超限（实例：793,513 输入 + 256,000 预留 = 1,049,513 > 1,048,576）。
- **窗口大小服务端只在超限报错里告诉你**：`GET /models` 只返回 id（无 context_length）；本部署 deepseek-flash 实测窗口 1,048,576、`max_tokens` 合法区间 `[1, 393216]`。

- OpenAILLM 懒创建连接池：首次真实请求在事件循环内同步建连（WSL2 实测 ~6s）阻塞 UI → `on_mount` 后台 prewarm 极小请求。
- thinking 模式要求 assistant tool_calls 消息回传时必须带 `reasoning_content`（`is not None` 判断、空串兜底），缺失报 400。

### 配置清理

- `save_history`、`resolve_config` 的 OPENAI_* env 覆盖、OpenAILLM 构造器 env 回退均为死配置/遗留，已移除；配置只从 `~/.pie/config.toml` 读取。

### 用量统计

- resume 后 `/stat` 累计显示 0（UsageTracker 不持久化）→ 会话 JSONL 首行 `__meta__` 保存用量，load 时恢复。

## 已知问题 / 待办

- 仓库级回归测试在 `tests/test_tui.py`（`uv run python tests/test_tui.py`，无需 pytest）：覆盖框选复制保真、CJK 断行、工具/命令渲染冒烟；改 TUI / 复制 / 断行后必跑。
- `PieApp._command` 仍是 86 行 if/elif（12 个分支）——可拆 `_cmd_*` 分组方法（可选，收益以可读性为主）；两个流式面板（#stream / #assistant-stream）各维护一份 tail 窗口状态机，可合并（风险中等）。

- 上下文估算用 chars/4 对中文严重低估（实例：估算 1,792 vs provider 上报 30,609）→ `/stat` 主行应优先展示 provider 上报值（`last_prompt_tokens` 已采集，主行待改）。
- `write(content=全文)` 参数 + thinking reasoning 体积大是上下文主要消耗源，且不在 spill 统计口径内；决策：留给自动压缩处理。
- `resume` 只能恢复最近一次会话，不支持选择历史会话。
- 上下文窗口 `context_window` 仍是手填，没与真实窗口校验/自动探测：400 里的 `maximum context length is N` 是最省的探测源，可做「报错 → 写回配置 → 强制压缩 → 重试」自愈（图片那条 file_id 自愈已做）。
- `maybe_compact(..., windows=)` 与 `compact(session=..., windows=)` 这条链路**没有调用方传值**（`loop.py` 只传到 `manifest`）→ 自动触发的会话级压缩会把窗口块写进 `context/` 但**不登记到 `session.windows`**（只有 `/clear` 会登记）；影响 resume 后的窗口摘要重建。

