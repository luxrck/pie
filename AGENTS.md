# AGENTS.md

面向进入本仓库的 AI agent（以及人类协作者）的项目说明。

## 项目概述

pie 是一个极简的 agent harness：内置 read / edit / write / shell 四个工具（**Rust 版注册名为 `writ` / `bash`**，2026-09-23 用户点名改的），YOLO 模式（无权限审计），模型走 OpenAI 兼容接口，可对接 DeepSeek、Qwen、vLLM、Ollama 等。

## 目录结构

```text
pie/
├── src/pie/
│   ├── __init__.py    # 公开 API
│   ├── __main__.py    # python -m pie 入口
│   ├── cli.py         # CLI：新对话 / resume / sessions / setup / context + 一次性（子 agent）模式；含非 TTY 行编辑 read_input
│   ├── clipboard.py   # 剪贴板图片 → ~/.pie/files/img-<hash>.png（Pillow 后端，TUI 的 Ctrl+V / Ctrl+G / /paste 用）
│   ├── config.py      # 配置持久化（~/.pie/config.toml）+ system prompt 组装
│   ├── session.py     # 会话层：多轮对话 + JSONL 持久化 + 用量累计
│   ├── loop.py        # 循环层：run / aturn（工具调用编排 + 取消）
│   ├── context.py     # 上下文管理：三级压缩 + 全文落盘 + 摘要指针
│   ├── tools.py       # 工具层：@tool 装饰器 + ToolRegistry + 内置工具
│   ├── llm.py         # 模型层：LLM 协议 + OpenAI 兼容实现
│   ├── files.py       # 图片 Files API：本地内容寻址副本 + 上传/复用/失效回退
│   ├── theme.py       # 主题展示数据：配色 + 图标 + 主题族/变体 + build_css + 终端背景探测（OSC 11）
│   ├── textkit.py     # 显示层文本处理：CJK 友好断行 + 控制符/ANSI 转义清洗（纯函数）
│   └── tui.py         # TUI 应用：布局/命令/回合 worker/流式渲染 + 日志区控件与框选复制
├── tests/             # 回归测试（无 pytest 依赖）：test_theme / test_tui / test_config / test_session / test_files / test_clipboard / test_loop / test_aio
├── AGENTS.md          # 项目说明（本文件）
├── MEMORY.md          # 项目持久记忆
├── SYSTEM.md          # agent 运行时 system prompt
├── pyproject.toml
└── README.md
```

## 常用命令

```bash
uv sync
uv tool install . --editable                # 全局安装 pie 命令
pie                                         # 新对话（首次运行会引导写配置）
pie resume                                  # 恢复最近的会话
pie "任务"                                  # 一次性执行（子 agent 模式）
pie -c FILE "任务"                          # 指定配置文件
pie context gc                              # 压缩维护：info / verify / gc
uv run python -c "from pie.cli import self_check; self_check()"  # 工具自测
uv run python tests/test_tui.py            # TUI 回归：框选复制保真 + CJK 断行（约 15s，不调模型）
uv run python tests/test_llm.py            # 重试回归：判据/等待/次数/流式不重复（替身客户端，不联网）
```

## 约定

- Python >= 3.11，依赖用 uv 管理。
- 用户明确要求 YOLO：不加权限确认、不做沙箱。
- 新工具：普通函数 + `@tool()` 装饰器 + `registry.register()`。
- **公共面 = `pie/__init__.py` 的 `__all__`，且每个模块顶部也声明自己的 `__all__`**（2026-09-20 加）：内部模块（`aio` / `cli` / `clipboard` / `files` / `input` / `textkit` / `theme` / `tui` / `__main__`）写 `__all__: list[str] = []` → `from pie.aio import *` 不暴露任何东西（否则会带出 `asyncio` / `gc` / `Any` 这类依赖名）；公共模块（`config` / `context` / `llm` / `loop` / `session` / `tools`）列出真正对外的名字，实现细节不进（如 `config.RUNTIME_ONLY_FIELDS`、`context` 的压缩/GC 函数、`loop.CANCEL_TEXT`）。**TUI / 主题 / CLI 都不算对外 API**：`pie` 对外就是命令行本身。⚠️ `__all__` **只约束 `import *`**，挡不住显式 `from pie.aio import run`（Python 无访问控制），也不影响 console script `pie = pie.cli:main`；再拦一层要靠 `py.typed` + ruff `PLC2701`/`SLF001`（未做）。
- 工具输出格式：统一为 `Headers\n\nBody`（仿 http）——headers 为一行一个 `[...]` 方括号行；body 非空时用空行分隔，body 为空（或仅 headers）则省略空行；状态/元数据放 header 区（如 shell 首行 `[exit=..]`、shell 截断时 `[工具输出全文已保存: path]`、read 截断时 `[已截断：可用 offset=N 继续读]`、图片标记 `[图片已读取: ...]`、行号 `[行 a-b，共 N 行]`）。Header 值内不要含 `]`。
- **谁该落盘**（2026-09-21）：工具输出**不可再生** → 落盘 + `[工具输出全文已保存: path]` 指针（`shell` 的 stdout：进程结束就没了，副本是唯一取回途径）；**可再生** → 只报进度、不落盘（`read` 的文件还在原地，且自带 offset 分页 → 只补 `[已截断：可用 offset=N 继续读]`）。`read` 原先也落盘是「谁截断谁落盘」这条通用规则的无脑套用：那份副本**没有任何消费者**——`extract_spill_path` 的唯一调用点被 `call.name == "shell"`（Rust 版：`"bash"`）gate 住、`full_history()` 只按消息自身 `raw_path` 字段展开（read 从不设它）、`collect_context_garbage()` 反倒把它当垃圾 → 白占一份大文件副本（读 10MB 文件 = `~/.pie/context/` 里 10MB），指针还会被 `pie context gc` 删成**死链**。
- read 图片多模态：read 读图片（PNG/JPEG/GIF/WebP/BMP，魔数嗅探 + 头解析宽高）返回机器可读标记文本（`[图片已读取: path=..., mime=..., size=..., dim=...]`），offset/limit 对图片无意义被忽略；loop 层 `_inject_read_images` 解析标记（`tools.parse_image_marker`）后把图片 base64 成 data URI，作为 `ImageMessage`（context.py，多模态 user content parts）紧随工具结果注入——OpenAI 兼容 API 要求图片只能出现在 user 消息 content 里。ImageMessage 不是 UserMessage（synthetic 标记）→ 不构成轮次边界，不影响压缩轮次认定 / 轮数 / 标题 / 摘要，可随轮次级/会话级压缩一起落盘；from_dict 按 to_dict 的 cls 字段还原。图片字节上限为 `_max_image_bytes`（默认 32MB，可由配置注入覆盖），超限拒绝内联。
- 粘贴图片：`clipboard.grab_image_path()`（Pillow `ImageGrab.grabclipboard()`，**同步阻塞**→ TUI 侧必须 `asyncio.to_thread`）把剪贴板里的图片编码成 PNG 后**直接交给 `files.store_blob(mime="image/png")`**，返回 `~/.pie/files/img-<sha256[:16]>.png`（= `read` 用的那份副本 → 回车后 read 命中同一文件，**零复制**，详见 clipboard.py 模块注释）；没有图片/未装 Pillow/Linux 缺 wl-paste+xclip → None。TUI 三个入口共用同一实现：`PieTextArea.action_paste`（覆盖 TextArea 的 ctrl+v/super+v 绑定，None 时回退 `super().action_paste()` 文本粘贴）、App 级 `ctrl+g`（`PieApp.action_paste_image`，终端截走 Ctrl+V 时的兜底）与 `/paste` 命令。剪贴板里是文件列表（Windows CF_HDROP）时只认图片扩展名、返回原路径不复制。回退判据：**只**在 Pillow 抛 NotImplementedError（根本看不到剪贴板）时才走 WSL 的 PowerShell 后端 `_wsl_png_bytes`；Pillow 返回 None（工具在、没图）不回退，否则每次文本粘贴白等 0.4s。
- gc 与粘贴共用 `~/.pie/files/` 的代价：`collect_file_garbage` 的「未被引用」判据对**刚粘贴、还没 read** 的图不成立（它还是活的，只是没人引用）→ 加了 mtime 保护窗口 `files.GC_PROTECT_HOURS`（默认 24h，比这新的未引用副本一概不回收）；`store_blob` 落盘统一 0o600（图是用户数据）。
- 云端那份默认不动（交给上传时的 `expires_after`，默认 30 天）；两个 `--all` 都直接调 Files API、共用 `cli._files_api_call`（建客户端 → `aio.run` → close；网络/鉴权异常转成一句人话 + 退出码 1）：`pie files list --all` = `files.list_remote_files()` 列出云端全部上传件（`FileObject` → `{id, filename, bytes, created_at, expires_at}`，并用 `_local_file_index()` 按 `file_id` 标出是哪个会话记的），`pie files gc --all` = `files.purge_remote_files()` 把列出的逐个 `DELETE`（单个失败不中断、有失败退出码 1）。本地副本与 `__meta__.files` 都不改（旧 file_id 靠 `loop._downgrade_file_blocks` 自愈）。
- **测试里别只 stub 一半**：`Config()` 的 `api_key` 默认值是本部署的真实 key → 「配置没写 api_key」不是「没有 key」，漏 stub 会让 `--all` 真的删线上文件（2026-09-15 踩过）；模拟无 key 必须显式 `api_key = ""`，并同时替换 `cli.OpenAILLM` 与 `cli.purge_remote_files`。
- 新配置项：在 `src/pie/config.py` 的 `Config` dataclass 中加字段，配置文件为 ~/.pie/config.toml。
- 配置只从 ~/.pie/config.toml 读取；首次运行由 `ensure_config()` 交互式写入，旧 config.json 自动迁移。
- 会话每轮自动保存到 ~/.pie/sessions/，`pie resume` 恢复最新文件。
- 上下文压缩在 src/pie/context.py：**两个驱动入口都在那里**——`maybe_compact()`（自动，看软阈值）与 `compact()`（手动 `/compact [auto|tools|turns]`，不看水位，轮次级 `target=None`）；`Session.compact` 只是把会话历史 / 配置 / manifest 回调接上去的薄包装。两者共用 `_empty_stats()` 的统计形状。摘要全部规则式（轮次 user+最终输出 / 会话级指针 + 保护区域 verbatim）；工具级保护窗口由 keep_last_steps 决定——最近 N 个 step 批次（跨轮次滚动）内的工具结果 verbatim 保留，窗口外（含历史轮次）未压缩工具结果落盘成指针；轮次级只保护当前轮，已完成轮次可被轮次级/会话级压缩。
- **`aturn` 的三个执行旋钮**：`max_steps`（`int | None`，None = 不限；达到上限就不再问模型，把历史里最后一段 assistant 文本当最终答复返回并推一个 `answer` 事件——**不额外追加消息**，所以历史可能停在 tool 结果上，这是合法的 API 序列）、`parallel_tools`（`bool | None`，None = 跟随 `Config.parallel_tools`；True = `asyncio.gather` 并发，False = 按模型返回顺序**串行**——工具共享可变状态时必须 False）、`stream`（`bool | None`，None = 后端实现了 `stream()` 就流式（原行为），False = 强制一次性 `complete()`（on_event 不再有 reasoning/content 增量），True = 强制流式（后端没 stream() 仍回退 complete）——判断在 `_model_call` 一处：`can_stream and stream is not False`）。两条执行路径回填 ToolMessage 都是模型返回顺序，`run()` 同样透传这三个参数。
- 每次压缩事件通过 **`on_compact` 回调**（类型就是 `Callable[[dict[str, Any]], None]`——与 `on_event` 同型，不另起别名）交给调用方——`aturn` / `maybe_compact`（自动，看水位）/ `compact`（手动 `/compact`，不看水位）/ `AgentMessage.compact` / `ToolMessage.compact` 都只收回调、**不认识 manifest 文件路径**（默认 `None` = 不通知）。CLI 侧的回调是 `Session._record_compact`，它把事件追加到会话 manifest（~/.pie/context/<session>-manifest.jsonl）。`Session.compression_history()` 可查压缩历史。⚠️ **不传 on_compact ≠ 不落盘**：三级压缩里的 `write_raw()` 都是无条件调用，正文照样写 `~/.pie/context/`；嵌入方要完全不碰 `~/.pie` 必须 `Config(compaction=None)`。
- system prompt 分层：SYSTEM.md（角色/原则）+ AGENTS.md（项目）+ MEMORY.md（记忆），由 `build_system_prompt(config)` 组装。
- 提示词文件按“当前目录向上找项目根”解析，支持在任意目录运行 `pie`。
- 用户偏好与重要决策写入 MEMORY.md，而不是散落在代码注释里。
- 请求重试由 **pie 自己实现**（`llm.py`），不用 SDK 自带那套：`_retryable` 只认 408/409/429/5xx 与网络栈异常（连接/超时/流中断），其余 4xx 与我们自己的异常立刻抛；次数 = `1 + Config.max_retries`，单次等待 = **优先服务端 `Retry-After`**（429/503；`_retry_after_seconds`，夹在 `[1.0, 60.0]` —— 太短的限流等待没意义、太长会把回合卡死），没有该头才退回 `max(1.0, random(0, Config.max_retry_delay_seconds))`（`_retry_delay(max_delay, exc=None)` 是纯函数）。**命中重试时 stderr 那行带异常摘要**：`[retry] <what>失败（<类型: 消息>），Nd 后第 a/b 次重试`（`_exc_brief`：最多两层 `__cause__` —— SDK 包装异常的真原因（代理/连接/TLS）就在那里，不然只看到一句「Connection error.」无法归因）。**`OpenAILLM` 的 `max_retries` / `max_retry_delay_seconds` 不传就是 0（组件层面默认不重试）**，应用里由 session / loop / cli 三个构造点显式传 `Config` 侧的值（`Config.max_retries=2`、`Config.max_retry_delay_seconds=DEFAULT_MAX_RETRY_DELAY_SECONDS=1.0`，在 config.py）。**SDK 自带重试必须关掉**（`client_kwargs["max_retries"] == 0`），否则两层叠加、实际请求数不可预期。流式**只在还没吐过任何增量时**重试（吐过再重来会重复内容）；端点以 400 拒 `stream_options` → 摘掉该参数重来一次，不占重试额度、不退避。
- 主题（theme.py）分两层：**族**（`THEME_FAMILIES`，如 `catppuccin` = 深色 `catppuccin-mocha` + 浅色 `catppuccin-latte`）与**具体变体**（`THEMES`）。`get_theme(name, dark=None)` 对族名按终端背景明暗自适应（theme.py 自己的 `detect_dark_background()`：OSC 11 → COLORFGBG → None 按深色）；`Config.theme` 默认是族名。Markdown **不做全量接管**（走 Rich 默认）；只有浅色变体通过 `Theme.markdown_styles()`（字段 `markdown_code`，完整样式串）覆盖代码两键、并通过 `Theme.code_theme`（pygments 主题名）给 `Markdown(code_theme=...)` 换代码块高亮，`tui.on_mount` 非空时才 `console.push_theme(...)`。**滚动条样式只有一份**（`build_css` 里的 `scrollbar` 片段：`scrollbar-size: 0 1` + 半透明灰轨道 + `muted` 滑块），**每个可滚动控件都要自己带上**（现为 `#log` / `#assistant-stream` / `#input`）——`ScrollBar` 子控件读的是**父控件**的 `scrollbar-*` 样式（`scrollbar.py` 的 `ScrollBar.render` → `self.parent.styles`），漏写就退回 Textual 默认的 2 cell 黑底蓝条。
- TUI 分层：显示层文本处理（CJK 断行 / 转义清洗）在 `textkit.py`（纯函数、无 Textual 依赖），配色与 CSS 在 `theme.py`；其余全在 `tui.py`（应用编排 + 控件 + 日志区框选复制 `SelectableRichLog`，文件内用 `# ---- xxx ----` 分区）。`tui.py` 导入时调用 `textkit.install_cjk_wrap()` 替换 `rich.text.divide_line`（全角字逐字可断、英文词不断，纯 ASCII 走 Rich 原实现）。
- 渲染分层：**唯一入口是模块级纯函数 `_box(palette, body, *, role="system", border_role="", lean=False)`**（不碰 App，返回 `list[Panel | Padding]`），它只做一件事：**分派**——`lean=True` 且 role 是工具活动（`tool_call` / `tool_result`）→ **`_lean(...)`**（单行，返回 `list[Padding]`），否则 **`_panel(...)`**（盒子，返回 `list[Panel]`）。
  - **`_panel` / `_lean` 参数完全一致**（`palette, body, *, role, border_role`），**且都是自包含叶子**：所需逻辑（参数→文本 / 参数→摘要 / 状态判定 / `[exit=N]` 头解析 / 标题与图标推导 / 截断 / 转义清洗）全部 **inline 在函数体内，不调任何中间小函数**（`_raw_panel` / `_cap_body` / `_split_shell_exit` / `_shell_result_box` / `_tool_failed_role` / `_lean_line` / `_lean_tool_result` / App 侧的 `_format_tool_args`·`_tool_summary` 一整串都删了）。代价：**「参数→文本」「状态判定 + `[exit=]` 解析」在 `_panel` 与 `_lean` 各有一份**（两函数 docstring 里互相注明了）。
  - `role` 只有三类取值，**工具名一律写在 body 里**：**消息类**（system / user / assistant / error / cancelled）→ body 就是正文，标题查 `ROLE_TITLES`（user→「你」、assistant→「pie」）；**`tool_call`** → body = `{"name": <工具名>, "arguments": <参数>}`（实时给 dict、历史给 JSON 串，渲染层自己归一）；**`tool_result`** → body = `{"name": <工具名>, "arguments": <参数>, "content": <结果正文>}`（`arguments` 是配对的那次调用参数）。
  - **摘要（lean 单行的「一句话」）在 `_lean` 里从 `arguments` 推**：`LEAN_SUMMARY_KEYS`（read/write/edit→path、shell→command）命中就用那个值，没命中退回参数文本；`_lean` 还负责把它压成单行可打印文本。
  - `border_role` 只用于**显式覆盖边框色**（唯一用途：手动 `!cmd` **成功**框用默认灰 `MANUAL_SHELL_ROLE`，同时保住 ✓/✗/⏹ 图标）；工具正文超过 `BOX_BODY_LINES`（24）行时盒子只显示前 N 行，消息正文不截。
- **出错盒的正文由 `_error_body(exc)` 组装**（纯函数；`_fail_turn` 与 `!shell` worker 兜底共用）：首行 `类名: 消息` + `↳` 异常链。链由 `_exc_chain` 走 `__cause__` / 未被抑制的 `__context__`（`raise ... from None` 不算），带模块前缀、压单行、最多 4 层——SDK 的包装异常真原因都在这里（`APIConnectionError` 裹着 httpx 的 DNS / 连接 / TLS 错误）。不按异常类型另加提示（试过 `_exc_hint`，已按用户要求删）。
- `PieApp` 侧**只有一个渲染出口 `_notify(body, role="system", **kw)`**（把 `_box` 的产物写进 #log，也是 tui.py 里唯一的 `log.write(`；`lean` 默认取 App 的开关，可显式传 `lean=False`）。`_render_tool_call(name, args)` / `_render_tool_result(name, content, *, arguments=None)` 只负责把名字/参数/正文装成 body（摘要、参数文本、成败都归渲染层），live 事件、resume 历史回放、命令反馈全走同一条路。
- **手动 `!cmd`**（`PieApp._run_shell/_show_shell_result`）不吃简洁模式（用户主动执行、输出本身就是要看的），两个盒子都靠现有机制表达、不占用 `_box` 的额外参数：命令行回显 = `tool_call` 形状（`arguments` 位置放 `$ cmd` 原文）+ `border_role=MANUAL_SHELL_ROLE` + `lean=False`；结果框 = `role="tool_result"` + 自行把退出码补成 `[exit=N]` 正文 + `border_role=""`（失败红 / 取消灰，由状态推；只有成功用灰）。

## 已知限制

- 剪贴板图片粘贴（Ctrl+V / Ctrl+G / `/paste`）依赖 Pillow + 平台剪贴板：Windows/macOS 开箱可用；Linux 要装 `wl-clipboard` 或 `xclip`（WSLg 下 `wl-clipboard` 即可，剪贴板桥会转发图片）；两者都缺时回退 WSL 的 PowerShell 后端。Windows Terminal 会截走 Ctrl+V → 用 Ctrl+G 或 `/paste`。
- resume 只能恢复最近一次会话，不支持选择历史会话。
- 记忆文件靠 agent 主动更新，没有自动摘要。

