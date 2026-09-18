# MEMORY.md — 持久记忆

本文件是项目的持久记忆：记录用户偏好、当前系统概览与踩坑记录。agent 在每次会话开始时读取，并可在运行中用 edit/write 更新。

保持简洁：只记跨会话仍然有效的事实，不要记临时状态。
历史关键决策与变更记录已移至 `docs/CHANGELOG.md`（本文件只保留当前状态）。

## 用户偏好

- 信奉 YOLO：不要权限确认，工具直接执行。
- 偏好极简、可扩展的实现。
- TUI 背景透明：露出终端窗口背景色（Ubuntu 终端默认深红色），不画自己的背景色。
- **本机有两份 pie 源码树，别搞混**（2026-09-16 查清）：① 本次会话跑的是 `/mnt/d/pie-master` 的项目 venv（`/mnt/d/pie-master/.venv/bin/pie`，PATH 首位，editable → `/mnt/d/pie-master/src`）；② 全局 `pie` 命令（`~/.local/bin/pie` → `~/.local/share/uv/tools/pie/bin/pie`，uv tool 装）指向**另一个 checkout `/home/cc/src/pie`**（editable → `/home/cc/src/pie/src`），两者 git 历史不同、cli/clipboard/files/llm/textkit/tui 六个文件已分叉。在 `/mnt/d/pie-master` 里跑 `uv tool install . --editable` 会把全局命令**改指向本树**，不是“更新”——想同步 tool 环境要在 `/home/cc/src/pie` 里操作。

## 当前系统概览

- **架构**：分层包（tools / llm / loop / config / chat / context / cli / aio），src 布局，可全局安装（`uv tool install . --editable`）。内置工具仅 read / edit / write / shell，`@tool()` 按签名生成 schema，LLM 协议可注入。
  `aio.py` = 事件循环收尾（`run()` 代替 `asyncio.run`、`close_asyncgens()`、`event_loop()`；见「踩坑记录 → 事件循环与异步生成器」）。
  **Session 的三个路径/集合字段**（名字要一眼分清）：`path` = 会话 JSONL 自身的文件路径；`windows` = `/clear` 归档的历史窗口块（落在 `~/.pie/windows/`）；`files` = 图片 id 表（hash_id → file_id/本地副本/过期时间，见 `files.py`，存进 `__meta__`）。旧名 `file` / `fs` 已于 2026-09 改名（`__meta__` 里的 `fs` 键仍兼容读）。
- **CLI 与交互**：`/` 开头**但不是已知命令**的输入按普通消息发出（粘贴进来的绝对路径 `/home/.../x.png` 常被误伤，判据 = 首词在 `PALETTE_COMMANDS` 里，见 `PieApp.is_known_command`）。`pie`（新对话）/ `pie resume`（恢复最近）/ `pie` 带参数（一次性子 agent，不写 sessions）/ `pie context info|verify|gc`；`-c/--config` 指定配置。真实终端走 Textual TUI，非 TTY 回退 readline/prompt_toolkit；输入以 `!` 开头走 shell 模式（不进上下文）。运行时命令：`/stat`（用量）、`/thinking`（切换思考深度）、`/model <id>`（切换模型，启动自动拉取可用列表）、`/compact [tools|turns]` 手动压缩（无参 = 工具级+轮次级）、`/stop` 或 `Esc`（取消当前任务：补全面板开着时 Esc 只收起面板）、`/clear`（切换窗口）。
- **工具**：所有内置工具输出统一为 `Headers\n\nBody`（headers 一行一个 `[...]`，body 空则省略空行；状态/元数据放 header 区，如 shell 首行 `[exit=..]`、截断时 `[工具输出全文已保存: path]`、行号 `[行 a-b，共 N 行]`）。read 支持 offset/limit 分页 + 图片多模态 + 私有容量上限（_max_lines/_max_bytes/_max_image_bytes）；edit 为 edits 数组（oldText 唯一、防重叠、按原文非增量）；shell 支持私有容量上限（超限只保留尾部，全文落盘指针）。
- **上下文压缩**：三级——工具级（按行保留 head+tail 落盘指针）/ 轮次级 / 会话级，摘要全部规则式（零模型调用）。工具级保护窗口由 `keep_last_steps` 决定（最近 N 个 step 批次，跨轮次滚动）；轮次级只保护当前轮、已完成轮次可压。内容 hash 落盘 + 指针链 + 压缩级别只升不降 + 软阈值 / 目标水位迟滞（比例在 `[compaction]` 的 `soft_ratio` / `target_ratio`，**相对可用输入预算 = `context_window` - `reserved_tokens`**，默认 80% / 55%）；manifest 索引 + verify/gc。`compaction` 为显式配置：默认 None 不做任何压缩，写了则三级全开（子表调 head/tail，`tool/turn/session = false` 关闭对应级）。
- **模型层**：OpenAI 兼容接口，`reasoning_effort` 透传；`reasoning_content` 捕获/持久化/原样回传（thinking 模式 400 已修）。采集 provider usage（prompt/completion）+ UsageTracker 跨 resume 累计。**重试自己实现**（`llm.py`：`_retryable` / `_retry_delay` / `_retry`，SDK 自带那套用 `max_retries=0` 关掉）——次数 = `1 + Config.max_retries`，等待 = `max(1.0, random(0, Config.max_retry_delay_seconds))`（默认值在 config.py：`Config.max_retries=2` / `Config.max_retry_delay_seconds=DEFAULT_MAX_RETRY_DELAY_SECONDS`；`OpenAILLM` 的这两个参数**不传就是 0**，组件层面默认不重试）；**上下文账**：`context_window`（模型窗口，输入 + 输出一起算）+ `reserved_tokens`（每次请求为输出预留，就是发给 API 的 `max_tokens`，`None`/`"auto"` = 不发、用服务端默认）→ `Config.context_budget() = 二者相减`；`/stat` 与 system prompt 都按这份预算展示。
- **图片（`read` → 多模态注入）**：read 返回标记文本，harness 把图片作为**多模态 user 消息**注入（图片只能在 user 消息里）。默认走 **Files API**：先落本地内容寻址副本 `~/.pie/files/<hash_id>`（`hash_id = img-<sha256[:16]>`）→ **从副本上传**拿 `file_id` → 历史里只留 `{"type":"file","file_id":…}`。记录按会话存 `__meta__.files`（无全局缓存）；未命中/过期/换 key/模型不支持/上传失败 → 静默回退内联 base64。维护：`pie files list [--all]` / `pie files gc [--delete] [--all]`（**副本目录也是剪贴板粘贴的落点** → 本地 gc 另有 24h mtime 保护窗口，见 `clipboard.py`）；两个 `--all` 都直接调 Files API 看/清**云端**（`files.list_remote_files` / `purge_remote_files`，云端是**账号全局**的：list 用本地会话记录标注 `会话=未记录`，gc 删本账号**全部**上传件），本地副本/会话记录不动、靠 loop 降级自愈。⚠️ `Config().api_key` 默认值是本部署真实 key —— 写测试时「配置没写 api_key」不等于空 key，漏 stub 会真的删线上文件。
- **剪贴板图片 → 路径（`clipboard.py`）**：输入框 `Ctrl+V`（或 `/paste`）把剪贴板里的图片编码成 PNG 后交给 **`files.store_blob()`**，返回 `~/.pie/files/img-<sha256[:16]>.png`（**就是 `read` 用的那份副本** → 回车后 read 命中同一文件、**零复制**）并把**路径**插进输入框（回车即普通 `read`）。后端是 Pillow `ImageGrab.grabclipboard()`（同步阻塞 → TUI 侧 `asyncio.to_thread`）：**Windows/macOS 开箱可用**，**Linux 需 `wl-clipboard` 或 `xclip`，两者都缺时 Pillow 抛 `NotImplementedError`** → 静默当作「没有图片」。没图时 Ctrl+V 回退原本的文本粘贴（覆盖的是 `TextArea.action_paste`，绑定不变）；Windows 剪贴板里是图片**文件**（CF_HDROP）→ 返回原路径、不复制。共目录的代价：`files.gc` 的「未被引用 = 垃圾」对刚粘贴还没 read 的图不成立 → **mtime 保护窗口 `GC_PROTECT_HOURS`（24h）**。按键：Ctrl+V（Textual TextArea 自带绑定）+ **Ctrl+G**（App 级绑定，终端截走 Ctrl+V 时的兜底）+ `/paste` 命令，三者同一实现；WSL 另有一条 **PowerShell 后备**（`Clipboard::GetImage()`，`-sta` 必加，路径走 `wslpath -w`），**只在 Pillow 抛 NotImplementedError（没装 wl-paste/xclip）时**才跑——否则「剪贴板里没有图片」也要白等一次 0.4s PowerShell 冷启动。
- **一套主题适配深色/浅色终端（主题族 + 终端背景探测）**：`termbg.py` 的 `detect_dark_background()`（进程内缓存）按 OSC 11 查询 → `COLORFGBG` → None 的顺序判断终端背景明暗（OSC 11 直接读写 `/dev/tty`，临时关规范模式/回显、0.2s 超时、读完恢复；纯函数 `parse_osc11` 认 `rgb:`/`rgba:` 的 2/4 位分量，按 sRGB 加权亮度 < 0.5 判深色；`parse_colorfgbg` 认背景索引 < 8 为深色）。`theme.py` 引入**主题族**：`THEME_FAMILIES = {"catppuccin": (CATPPUCCIN_MOCHA, CATPPUCCIN_LATTE)}`，`get_theme(name, dark=None)` 对族名按明暗选变体（探测不到按深色），具体变体名（`catppuccin-mocha`）固定不受影响；`Config.theme` 默认 `"catppuccin"`，`PieApp.__init__` 探测一次并据此选 palette + Textual 主题（`ansi-dark`/`ansi-light`，两者 background 都是 ansi_default → 仍透明）。用户配置已由 `catppuccin-mocha` 改为 `catppuccin`（否则不会自适应）。
- **TUI 主题**：`theme.py` 集中所有展示数据（配色 + 图标，不依赖 Textual/Rich），四类取用：`Theme.role_border(role)`（未知 role 回退 system；`cancelled` 与 system 同色）、`Theme.role_icon(role)`、`Theme.tool_icon(name)`（**调用**框图标：`tool_icons` 按工具名覆盖，未配置回退 `icon_tool_call`，值 `""` = 无图标）、`Theme.result_icon(role, name)`（**结果**框图标：按 role 取状态字形，可被 `tool_result_icons` 覆盖）、`Theme.lean_mark(status)`（简洁模式行首标记，status ∈ ok/fail/cancelled，未知回退 ok）。
  **「执行结果」字形只有一套**：`icon_ok(✅)` / `icon_error(❌)` / `icon_cancelled(⏹)`——盒子模式作 role=tool_result/error/cancelled 的标题图标（`↳` 那套「这是上一个框的产出」已废），简洁模式同一套放行首；取哪个由 `_box` 按执行结果判定（退出码/失败前缀/取消）。角色轴与结果轴分开：**调用**框图标回答「调的是哪个工具」（`tool_icons` + `tool_result_icons` 是对结果字形的 opt-in 覆盖），结果框图标回答「结果如何」。默认主题：`tool_icons={"read": "✧", "edit": "⟱", "write": "✦", "shell": "❯"}`，`tool_result_icons={}`。原则：**视图层不再出现图标字面量**。渲染分层：**`_box` 只做分派** → 工具活动且 lean 走 `_lean`（单行 `list[Padding]`）、否则 `_panel`（盒子 `list[Panel]`）；两者参数一致、body/role 语义一致，但**各自自包含、不调中间小函数**（`_raw_panel` / `_tool_result_box` 已删）。边框色一律 `border_role` > `role`，**只有手动 `!cmd` 指定 `border_role`**（成功框用默认灰 `MANUAL_SHELL_ROLE`，同时保住 role 带来的 ✓/✗ 状态图标）。
- **TUI 显示**：断行是 CJK 友好的——tui.py 导入时调 `textkit.install_cjk_wrap()`（全角字逐字可断、英文词不断，纯 ASCII 交回 Rich 原实现）；`tui.py` 的 `SelectableRichLog` 框选复制按「源文本 + 显示行对齐」切来源文本（不是拼显示行），长行复制为一行。tui.py 只做应用编排（布局/命令/回合 worker/流式），渲染**只有一个出口 `_notify(body, role="system", **kw)`**（= 把 `_box` 的产物写进 #log，唯一调用 `log.write` 的地方），而 **`_box(palette, body, *, role="system", border_role="", lean=False)` 只做分派**：lean 且 role 是 `tool_call` / `tool_result` → `_lean`（单行，`list[Padding]`），否则 `_panel`（盒子，`list[Panel]`）。**`_panel` / `_lean` 参数一致、body/role 语义一致，且各自自包含**（参数→文本、参数→摘要、状态判定、`[exit=]` 解析、标题图标推导、截断、清洗都 inline 在函数里，不调任何中间小函数；代价是前两项在两者各一份）。`role` 只有三类取值，**工具名写在 body 里**：消息类 → 正文；`tool_call` → `{"name", "arguments"}`；`tool_result` → `{"name", "arguments", "content"}`（arguments = 配对那次调用的参数，供 `_lean` 在内部按 `LEAN_SUMMARY_KEYS` 推摘要）。盒子模式的**工具正文**超过 `BOX_BODY_LINES`（24）行时只显示前 N 行；消息正文不截。**异常显示**：`_error_body(exc)`（`_fail_turn` / `!shell` 兜底共用）= 首行 `类名: 消息` + `↳ 异常链`（`_exc_chain` 走 `__cause__`，SDK 包装层的真原因——DNS/连接/TLS——都在这里；不再按类型另加提示）。
- **TUI 简洁模式**（`[tui] lean = true`）：只给 user / assistant 套盒子，工具活动交给 `_lean` 压成单行 `Text`（`[状态标记|工具图标] 工具名 摘要`——结果行 ✅/❌/⏹ 在**行首**，失败/取消时下方 `↳ 正文` 续行），外面套 `Padding(text, (0, BOX_INSET))` 与盒内正文对齐。**失败正文块的截断与 `_panel` 同一条规则**：超过 `BOX_BODY_LINES` 行只显示前 N 行 + 同一句 `...[已省略 K 行，共 M 行]...`（`LEAN_DETAIL_HEAD/TAIL` 已删，别再另搞一套；改渲染后跑 `tests/test_tui.py`）。`BOX_INSET` 由盒子几何推导（`_BOX_BORDER + _BOX_PADDING[1]`，与 `_panel` 共用 `_BOX_PADDING`）——别另写 2；`#log` 的 CSS padding 与这件事无关（同一内容区，改它不破坏对齐）。标记放行首是关键：`Text(no_wrap=True, overflow="ellipsis")` 的右截断天然保住它，所以没有自定义 renderable（`_LeanLine` 已删；摘要要先压成单行：真换行在 `no_wrap` 下照样断行、tab 会撑破宽度——这段 inline 在 `_lean` 里）。状态标记字形在主题里（`icon_ok/icon_error/icon_cancelled` + `palette.lean_mark(status)`，**只认 ok/fail/cancelled 三个词，传 role 名会静默退回 ok**；`_lean` 里有一张 status→它的小映射表）；单行摘要从 `arguments` 推（`LEAN_SUMMARY_KEYS`，见上条）。改动 lean 渲染后跑 `tests/test_tui.py`（`test_lean_tool_lines_are_padded` 含「框选复制不带留白」断言）。**唯一例外：手动 `!cmd`**（`_run_shell` / `_show_shell_result`）不吃简洁模式——它是用户主动执行、输出本身就是要看的东西，任何模式都套盒子：命令行回显 = `tool_call` 形状（`arguments` 位置放 `$ cmd`）+ `border_role=MANUAL_SHELL_ROLE` + `lean=False`；结果框 = `role="shell"` + 自行补 `[exit=N]` 头 + `border_role` 只在成功时给默认灰（失败红/取消灰由状态推），`_panel` 的 `border_role` 参数实际上只给这条路用。
- **依赖（direct，5 个）**：`openai`（核心）/ `textual` + `rich`（TUI）/ `pillow`（剪贴板图片）/ `prompt_toolkit`（非 TTY 行编辑）。`numpy` 于 2026-09 移除：声明了但全仓库零引用、也不是任何传递依赖（`uv tree` 确认），删后 `uv lock` 29→27 包、`.venv` 123M→68M，全部 73 例测试通过。⚠️ **`pillow` / `prompt_toolkit` 是刻意的软依赖、别误判为可删**：代码里都是 try-import 降级（`clipboard._engine()` 缺 Pillow → 剪贴板图片功能静默不可用；`input.py` 缺 prompt_toolkit → 回退内置 `input()`，WSL/mintty 中文退格会截断），静态扫描看起来“只在一处用到”就删会让功能悄悄失效。
- **配置**：只从 `~/.pie/config.toml` 读取（保留 `PIE_DIR` / `PIE_CONFIG_FILE` 路径重定向）；`Config.tools`（dict，按工具名设默认私有参数，如 read: {_max_lines,_max_bytes,_max_image_bytes}）由 dispatch/adispatch 注入下划线私有参数（`_inject_tool_defaults`，只注入下划线且不覆盖显式传参）；SYSTEM.md / AGENTS.md / MEMORY.md / `~/.pie/memory.md` 存在即注入 system prompt。

## 踩坑记录

### 注解与类型

- `from __future__ import annotations` 使注解变成字符串 → `@tool()` 用 `get_type_hints(fn)` 解析真实类型。
- `str | None` 的 `get_origin` 在 Python 3.13 返回 `types.UnionType`、3.14 返回 `typing.Union`（两者合一）→ 判断要写 `origin is Union or origin is types.UnionType`；全局 uv 工具环境（3.13）与项目 .venv（3.14）可能不同，跨版本改动要两环境都实测。

### 输入与终端

- **终端可能截走 Ctrl+V，应用的键盘绑定根本收不到**：Windows Terminal 把 `ctrl+v` 绑给终端侧粘贴（剪贴板是图片时什么也不发、也不产生 key 事件）→ pie 里 Ctrl+V 读剪贴板这条路在 WT 下无效，靠 `/paste` 命令兜底；GNOME Terminal / macOS Terminal / kitty 等把 Ctrl+V 交给应用的才走按键路径（终端自己的粘贴快捷键多是 Ctrl+Shift+V / Cmd+V）。
- **终端粘贴链路天生拿不到图片**：与终端交换剪贴板只有两条路——OSC 52（Textual `action_paste` 用的，仅文本）和 bracketed paste（`events.Paste`，终端只把文本发过来）。剪贴板里只有位图时（Win+Shift+S 截图就是：CF_DIB，无文本、无文件路径），粘贴事件是空的 → 想要图片只能应用自己去问系统剪贴板（`clipboard.py`）。
- **回退后端要用「能力缺失」信号，不能用「这次没拿到」信号**：Pillow 在 Linux 上没有 wl-paste/xclip 时抛 `NotImplementedError` —— 这才是「Pillow 看不到剪贴板」；它返回 None 只是「剪贴板里没有图」。拿 None 当触发条件会让每次纯文本粘贴都白等一次 PowerShell 冷启动（0.4s）。
- **WSLg 的剪贴板桥会转发图片**：Windows 剪贴板里的位图能以 `image/png` 被 `wl-paste` 读到（实测 0.035s、223x258 原样），所以 WSL 里装了 `wl-clipboard` 就够，不必绕 PowerShell。
- **Pillow `ImageGrab.grabclipboard()` 各平台语义（Pillow 12.3 实测源码）**：Windows 返回 `Image`（png/DIB）**或 `list[str]`**（剪贴板里是文件 = CF_HDROP）；macOS 走 `osascript -e "get the clipboard as «class PNGf»"`（**只认 PNG**，截图到剪贴板虽是 TIFF，但 pasteboard 读取时会自动转 PNG；读剪贴板不需要自动化权限，写才要）；Linux 转调 `wl-paste -t image` / `xclip -t image/png -o`，**缺工具直接 raise NotImplementedError**，剪贴板空/无图时按 stderr 关键字识别后返回 None（其它子进程错误抛 ChildProcessError）。→ 封装必须 catch `(NotImplementedError, OSError, ...)`，并按「Image / list / None」三分支处理。
- **macOS「截图粘不进来」首先是快捷键的坑，不是代码**：`⇧⌘4` 是**存成文件**（默认桌面），要直接进剪贴板得按 **`⌃⇧⌘4`**。另有一个真缺口已补：Finder 里 `⌘C` 复制图片文件时剪贴板里是**文件引用**（`«class furl»`），而 Pillow 的 macOS 分支只请求 `«class PNGf»`（位图）→ 返回 None，被当成「没有图片」。现在 `clipboard._mac_file_path()` 在 Pillow 拿不到位图时用 osascript 读 furl（`POSIX path of (the clipboard as «class furl»)`，脚本自带 `try/on error` 返回空串）→ 直接用那个文件（不复制，只认图片后缀）。无图时的提示按平台给（`tui._NO_IMAGE_HINT`：macOS 写快捷键、Linux 写 wl-clipboard/xclip）。实测：真剪贴板放位图 → 0.78s 落盘；Finder 式 furl → 返回原路径。回归：`tests/test_clipboard.py::test_mac_file_reference_returns_path`（新增 `_mac_env`/`_mac_off` 两个替身；**“没有图片”类用例都要套 `_mac_off()`**，否则在 macOS 上会真去读用户剪贴板）。
- Textual TextArea 在 ansi 主题（App 用 ansi-dark）下选中高亮会多一条 `text-style: reverse`（来自 `&:ansi .text-area--selection`）：`#input .text-area--selection` 这类 ID 规则能覆盖 background/color，但**覆盖不了没声明的 text-style**（未声明 ≠ 清除）→ 选中呈反色，和 #log 鼠标框选（accent 底 + accent_text 字）看着正好相反。要统一必须显式写 `text-style: none`。
- TUI 长流式文本别塞非滚动 Static（超出可视区的行被裁掉，视觉停更）→ 窗口化尾部或换 RichLog auto_scroll。
- **RichLog 里 Markdown 的颜色不受 Textual 主题 / palette 控制**：RichLog 用 `App.console` 渲染，而 Textual 创建的该 console **没设 theme** → Rich 的 `DEFAULT_STYLES` 生效（`markdown.code = bold cyan on black`、`markdown.h2 = underline magenta`、表格/列表青、链接亮蓝）→ 行内代码/代码块是黑底 + 终端 ANSI 青，浅色终端下糊成一片。2026-09-14 曾用 `Theme.rich_styles()` + `App.console.push_theme(...)` 把 Markdown 全量接管到 palette（含 `code_bg`/`code_fg`/`code_theme`，浅色变体配 `friendly`、深色配 `monokai`），**后按用户要求回退**（字段/方法已删）。如今只留**最小覆盖**：palette 字段 `markdown_code`（**完整样式串**，空串 = 不覆盖）+ `Theme.markdown_styles()`，**只有浅色变体 latte 有值**（现为 `"bold cyan"`——去底色只留青色粗体字；`markdown_styles()` **原样使用**该值，不再自动拼 `bold`），`on_mount` 非空时 `push_theme` 只改 `markdown.code` / `markdown.code_block` 两键；mocha 不注入，继续走 Rich 默认。fence（带语言标注的代码块）底色来自高亮主题的 token（`#272822`），`markdown.code_block` 治不到——另用 palette 字段 **`code_theme`**（pygments 主题名）经 `Markdown(code_theme=...)` 传入：mocha = `monokai`（= Rich 默认，等于不动）、latte = `solarized-light`（暖白底 `#fdf6e3`，用户手改自早期的 `friendly`）。回归：`tests/test_tui.py::test_markdown_code_styles` + `tests/test_theme.py::test_markdown_styles_are_minimal` / `test_code_theme_follows_variant`。
- **工具输出进 RichLog 前必须清洗控制符/ANSI 转义**：Rich 排版把不可见转义字节计成宽度（`cell_len("\x1b[01;32mA\x1b[0m")` = 10 对可见 1）→ Panel 量出的内容宽比实际大，盒子的顶/底边框与内容行右边框不在一列（`ls --color` 必现）。清洗走 textkit.py 的 `strip_escapes()` / `rich_text()`（SGR 交 `Text.from_ansi` 解成样式，其余转义 + C0 剔除，`\r\n`→`\n`、孤立 `\r` 删）。顺序要紧：**先剔转义再剔控制符**，否则 OSC 的结束符 BEL(`\x07`) 先没，`\x1b]...` 的匹配会吞掉后面全部文本。
- `input()` 在部分终端（WSL/mintty）退格按字节删中文会截断 → 改用 prompt_toolkit 行编辑。
- **框选复制要按「源文本」切，不能按显示行拼**：`RichLog.lines` 是软换行后的显示行，且 Rich 在盒内换行点会直接吃掉那个空格（`'aaaaaa bbbb cccc' + 'dddd…'`）→ 按显示行拼接既多出换行又丢空格。`tui.py` 的 `SelectableRichLog` 每次 write 记 `_CopyEntry(row, count, src, spans)`，复制时按字符区间切 src（同一 src 的相邻显示行合并 → 空格/换行都回来），无映射才回退。src 取法：Panel→盒内正文、Text→`.plain`（先 `expand_tabs()`，与显示一致，含 tab 的输出才对得上）、Markdown→宽渲染(4096)纯文本（`.markup` 与显示行对不上）。对齐失败即回退，不猜。两个必须容忍的显示装饰（实测踩坑）：Rich 给 Markdown **列表续行加悬挂缩进**（源文本里没有），给**引用每行重复加 `▌ `**（源文本只首个有）——故行首空白一律不计内容（只计入 x0），整行匹配失败时再试去掉行首 `▌` 并把忽略字符数记进 span `(start, end, off)`。否则一个列表项就能让整条消息回退。分隔线 `---` 会被渲染成随宽度铺满的规则线 → 无宽度无关源文本 → 回退（可接受）。
- 显示层换行：已改成 **CJK 友好断行**——`textkit.py` 的 `install_cjk_wrap()`（tui.py 导入时调用）把 `rich.text.divide_line` 换成 `cjk_divide_line`（全角字 `cell_len == 2` 各自成一个 token，非全角串仍按词，其余逻辑同 Rich；纯 ASCII 交回原实现逐字节不变）。否则 Rich 只按空白断行：中文长句没空格 → 整段挪到下一行、上一行留白（宽 74 实测只用 42 格）。零宽断点（U+200B）无效：Python `\s` 不匹它、Rich 分词认不出。
- **输入框（`#input`/TextArea）的断行是另一条链路**：`WrappedDocument` 调 `textual._wrap.compute_wrap_offsets`，`install_cjk_wrap()` 也替换了它（`textkit.cjk_compute_wrap_offsets`）。两点约定：
  - **逐字符断**（用户要求）：`cjk_divide_line(..., char_level=True)` —— 非全角字也逐个成 token，英文单词/长路径会在当前行剩余空间里接着填（`... mixed with eng|lish words ...`），不再整块挪到下一行留一大片空白。Rich 日志区仍是词级（代码/命令整词更好读）——**两者行为不同是有意的**（2026-09 用户确认：日志区暂不改）。含 tab 的行仍交回 Textual 原实现（tab 宽度依赖列位置，用它预计算的 `precomputed_tab_sections`）。
  - ⚠️ **「尾空白算不算进词宽」两家不同**：Rich 的 `divide_line` 按 `word.rstrip()` 判宽，Textual 的 `compute_wrap_offsets` 把 `chunk`（**含尾空白**）整段算进 `remaining_space`。曾让两者共用 `cjk_divide_line` → 含中文的行遇到**行尾空格**时会产出 `width + 1` 格的显示行（400 条随机混排实测 3 条超宽，Textual 原版 0 条），输入框里光标/滚动位置随之偏 1 格。现在 `cjk_divide_line(..., count_trailing_space=)` 参数化：Rich 链路默认 `False`，Textual 链路传 `True`。
  - 回归：`tests/test_tui.py::test_textarea_wrap_never_exceeds_width`（fuzz：行宽 ≤ width）、`test_textarea_wrap_breaks_ascii_at_char`（英文也逐字符断 + 行被填满）。
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

### 事件循环与异步生成器

- **收尾的 `RuntimeError: generator didn't stop after athrow()` 是 httpx2/httpcore2 的收尾顺序问题，不是我们的 bug**（`pie -p 你好` 即可复现，长驻循环不会）：openai 流式响应读到 SSE `[DONE]` 就地 break → 「响应字节流」那串异步生成器（`AsyncStream.__stream__` → … → `PoolByteStream.__aiter__` → `HTTP11ConnectionByteStream.__aiter__` → `safe_async_iterate`）一直挂起在 yield 上；`asyncio.run` 收尾的 `shutdown_asyncgens()` 按 `loop._asyncgens`（WeakSet，顺序随对象地址漂移，所以时好时坏）一次性 aclose，**内层先关**时 httpcore2 就抛 RuntimeError。连接此时已被正确释放（`HTTP11ConnectionByteStream.aclose()` 在抛错前已调用），纯噪音。
- 对策：同步入口统一走 **`aio.run`**（不是 `asyncio.run`）——它收尾前先 `close_asyncgens()`：分多轮、每轮先摘空 `loop._asyncgens` 再逐个 `aclose()`、单个失败留给下一轮（与顺序无关，实测 2 轮清空）。**别在长驻循环（TUI）里按回合调它**：那会把同循环里无关的在用生成器一并关掉；TUI 只在 **`run_tui` 退出时**用 `aio.event_loop()`（自建循环交给 `App.run(loop=…)`）收一次尾。
- 经验：凡是「收尾才出现、还时有时无」的 asyncio 报错，先怀疑**关闭顺序 / GC 时机**；`loop._asyncgens`（CPython 内部结构，3.6+ 稳定，拿不到就降级为原生行为）就是 `shutdown_asyncgens()` 用的那个集合。

### API / 模型（DeepSeek thinking）


- **Vision / Files API 实测（2026-09）**：上传 `POST /files`（purpose=user_data）后，用 `{"type":"file","file_id":…}` 引用，`deepseek-flash` 确实看得到图；**prompt_tokens 与内联 base64 完全一致**（计费按尺寸，单图 ≤1024，与编码无关）——换 Files API 省的是**请求体/重复传输/上限**，不是钱（3 MB 图：内联 body 4 MiB ↔ file_id 187 B）。
- **`expires_after` 要进 `extra_body`**（openai SDK 不认这个 DeepSeek 扩展字段），带上后响应才有 `expires_at`；**服务端不做内容去重**（同图传两次 = 两个 file_id），所以“不重传”完全靠本地记录。
- **重试自己实现，SDK 那套已关（2026-09-17）**：SDK 的 `max_retries` 只在「发请求 + 拿响应头」阶段生效（读 body 中途断了不重试、也不包成 `APIConnectionError`），而且 pie 里还叠着一层 `stream_options` 兼容回退（catch 一切、只要还没吐过块）→ `max_retries=2` 实测最坏发 **6** 次请求（`x-stainless-retry-count` 会归零，说明进了第二轮）。现在：`OpenAILLM` 把 `client_kwargs["max_retries"]=0`，改为 `1 + Config.max_retries` 次尝试，单次等待 = `max(1.0, random(0, Config.max_retry_delay_seconds))`（默认 1.0 → 恒 1s），`_retryable` 只认 408/409/429/5xx 与 `_TRANSIENT_MODULES`（openai/httpx/httpcore/ssl）的异常；**流式只在还没 yield 过任何增量时**才重试（吐过再重来会重复内容）；`stream_options` 回退收紧成「只认 400」才摘参数重来（不占重试额度）。默认值常量 `DEFAULT_MAX_RETRY_DELAY_SECONDS` 在 **config.py**，`OpenAILLM` 同名参数可覆盖；**`OpenAILLM` 的 `max_retries` / `max_retry_delay_seconds` 不传就是 0**（组件默认不重试，无 `DEFAULT_MAX_RETRIES` 这类常量；应用里三个构造点都显式传 cfg）。
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

- 剪贴板粘贴的 **WSL 后端已实现**（`clipboard._wsl_png_bytes`：`powershell.exe -sta` + `Clipboard::GetImage()`，~0.5s）。实测本机更省：装了 `wl-clipboard` 后走 WSLg 剪贴板桥（`wl-paste` 拿 `image/png`，0.035s），PowerShell 那条根本不会被触发。
- 仓库级回归测试在 `tests/test_tui.py`（`uv run python tests/test_tui.py`，无需 pytest）：覆盖框选复制保真、CJK 断行、工具/命令渲染冒烟；改 TUI / 复制 / 断行后必跑。
- `PieApp._command` 仍是 86 行 if/elif（12 个分支）——可拆 `_cmd_*` 分组方法（可选，收益以可读性为主）；两个流式面板（#stream / #assistant-stream）各维护一份 tail 窗口状态机，可合并（风险中等）。

- 上下文估算用 chars/4 对中文严重低估（实例：估算 1,792 vs provider 上报 30,609）→ `/stat` 主行应优先展示 provider 上报值（`last_prompt_tokens` 已采集，主行待改）。
- `write(content=全文)` 参数 + thinking reasoning 体积大是上下文主要消耗源，且不在 spill 统计口径内；决策：留给自动压缩处理。
- `resume` 只能恢复最近一次会话，不支持选择历史会话。
- 上下文窗口 `context_window` 仍是手填，没与真实窗口校验/自动探测：400 里的 `maximum context length is N` 是最省的探测源，可做「报错 → 写回配置 → 强制压缩 → 重试」自愈（图片那条 file_id 自愈已做）。
- `maybe_compact(..., windows=)` 与 `compact(session=..., windows=)` 这条链路**没有调用方传值**（`loop.py` 只传到 `manifest`）→ 自动触发的会话级压缩会把窗口块写进 `context/` 但**不登记到 `session.windows`**（只有 `/clear` 会登记）；影响 resume 后的窗口摘要重建。

