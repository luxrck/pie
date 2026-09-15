# AGENTS.md

面向进入本仓库的 AI agent（以及人类协作者）的项目说明。

## 项目概述

pie 是一个极简的 agent harness：内置 read / edit / write / shell 四个工具，YOLO 模式（无权限审计），模型走 OpenAI 兼容接口，可对接 DeepSeek、Qwen、vLLM、Ollama 等。

## 目录结构

```text
pie/
├── src/pie/
│   ├── __init__.py    # 公开 API
│   ├── __main__.py    # python -m pie 入口
│   ├── cli.py         # CLI：新对话 / resume / sessions / setup / context + 一次性（子 agent）模式
│   ├── clipboard.py   # 剪贴板图片 → ~/.pie/paste/<hash>.png（Pillow 后端，TUI 的 Ctrl+V / /paste 用）
│   ├── config.py      # 配置持久化（~/.pie/config.toml）+ system prompt 组装
│   ├── session.py     # 会话层：多轮对话 + JSONL 持久化 + 用量累计
│   ├── loop.py        # 循环层：run_agent / aturn（工具调用编排 + 取消）
│   ├── context.py     # 上下文管理：三级压缩 + 全文落盘 + 摘要指针
│   ├── tools.py       # 工具层：@tool 装饰器 + ToolRegistry + 内置工具
│   ├── llm.py         # 模型层：LLM 协议 + OpenAI 兼容实现
│   ├── input.py       # 非 TTY 行编辑（prompt_toolkit，中文退格安全）
│   ├── files.py       # 图片 Files API：本地内容寻址副本 + 上传/复用/失效回退
│   ├── theme.py       # TUI 展示数据：配色 + 图标 + 主题族/变体 + build_css（纯字符串，不依赖 Textual）
│   ├── termbg.py      # 终端背景明暗探测（OSC 11 → COLORFGBG）：供主题族自动选深/浅变体
│   ├── textkit.py     # 显示层文本处理：CJK 友好断行 + 控制符/ANSI 转义清洗（纯函数）
│   └── tui.py         # TUI 应用：布局/命令/回合 worker/流式渲染 + 日志区控件与框选复制
├── tests/             # 回归测试（无 pytest 依赖）：test_theme / test_tui / test_config / test_session / test_files
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
```

## 约定

- Python >= 3.11，依赖用 uv 管理。
- 用户明确要求 YOLO：不加权限确认、不做沙箱。
- 新工具：普通函数 + `@tool()` 装饰器 + `registry.register()`。
- 工具输出格式：统一为 `Headers\n\nBody`（仿 http）——headers 为一行一个 `[...]` 方括号行；body 非空时用空行分隔，body 为空（或仅 headers）则省略空行；状态/元数据放 header 区（如 shell 首行 `[exit=..]`、截断时 `[工具输出全文已保存: path]`、图片标记 `[图片已读取: ...]`、行号 `[行 a-b，共 N 行]`）。Header 值内不要含 `]`。
- read 图片多模态：read 读图片（PNG/JPEG/GIF/WebP/BMP，魔数嗅探 + 头解析宽高）返回机器可读标记文本（`[图片已读取: path=..., mime=..., size=..., dim=...]`），offset/limit 对图片无意义被忽略；loop 层 `_inject_read_images` 解析标记（`tools.parse_image_marker`）后把图片 base64 成 data URI，作为 `ImageMessage`（context.py，多模态 user content parts）紧随工具结果注入——OpenAI 兼容 API 要求图片只能出现在 user 消息 content 里。ImageMessage 不是 UserMessage（synthetic 标记）→ 不构成轮次边界，不影响压缩轮次认定 / 轮数 / 标题 / 摘要，可随轮次级/会话级压缩一起落盘；from_dict 按 to_dict 的 cls 字段还原。图片字节上限为 `_max_image_bytes`（默认 32MB，可由配置注入覆盖），超限拒绝内联。
- 粘贴图片：`clipboard.grab_image_path()`（Pillow `ImageGrab.grabclipboard()`，**同步阻塞**→ TUI 侧必须 `asyncio.to_thread`）把剪贴板里的图片编码成 PNG 后**直接交给 `files.store_blob(mime="image/png")`**，返回 `~/.pie/files/img-<sha256[:16]>.png`（= `read` 用的那份副本 → 回车后 read 命中同一文件，**零复制**，详见 clipboard.py 模块注释）；没有图片/未装 Pillow/Linux 缺 wl-paste+xclip → None。TUI 三个入口共用同一实现：`PieTextArea.action_paste`（覆盖 TextArea 的 ctrl+v/super+v 绑定，None 时回退 `super().action_paste()` 文本粘贴）、App 级 `ctrl+g`（`PieApp.action_paste_image`，终端截走 Ctrl+V 时的兜底）与 `/paste` 命令。剪贴板里是文件列表（Windows CF_HDROP）时只认图片扩展名、返回原路径不复制。回退判据：**只**在 Pillow 抛 NotImplementedError（根本看不到剪贴板）时才走 WSL 的 PowerShell 后端 `_wsl_png_bytes`；Pillow 返回 None（工具在、没图）不回退，否则每次文本粘贴白等 0.4s。
- gc 与粘贴共用 `~/.pie/files/` 的代价：`collect_file_garbage` 的「未被引用」判据对**刚粘贴、还没 read** 的图不成立（它还是活的，只是没人引用）→ 加了 mtime 保护窗口 `files.GC_PROTECT_HOURS`（默认 24h，比这新的未引用副本一概不回收）；`store_blob` 落盘统一 0o600（图是用户数据）。
- 新配置项：在 `src/pie/config.py` 的 `Config` dataclass 中加字段，配置文件为 ~/.pie/config.toml。
- 配置只从 ~/.pie/config.toml 读取；首次运行由 `ensure_config()` 交互式写入，旧 config.json 自动迁移。
- 会话每轮自动保存到 ~/.pie/sessions/，`pie resume` 恢复最新文件。
- 上下文压缩在 src/pie/context.py：摘要全部规则式（轮次 user+最终输出 / 会话级指针 + 保护区域 verbatim）；工具级保护窗口由 keep_last_steps 决定——最近 N 个 step 批次（跨轮次滚动）内的工具结果 verbatim 保留，窗口外（含历史轮次）未压缩工具结果落盘成指针；轮次级只保护当前轮，已完成轮次可被轮次级/会话级压缩。
- 每次压缩事件写会话 manifest（~/.pie/context/<session>-manifest.jsonl）并返回统计；`Session.compression_history()` 可查压缩历史。
- system prompt 分层：SYSTEM.md（角色/原则）+ AGENTS.md（项目）+ MEMORY.md（记忆），由 `build_system_prompt(config)` 组装。
- 提示词文件按“当前目录向上找项目根”解析，支持在任意目录运行 `pie`。
- 用户偏好与重要决策写入 MEMORY.md，而不是散落在代码注释里。
- 主题（theme.py）分两层：**族**（`THEME_FAMILIES`，如 `catppuccin` = 深色 `catppuccin-mocha` + 浅色 `catppuccin-latte`）与**具体变体**（`THEMES`）。`get_theme(name, dark=None)` 对族名按终端背景明暗自适应（`termbg.detect_dark_background()`：OSC 11 → COLORFGBG → None 按深色）；`Config.theme` 默认是族名。Markdown **不做全量接管**（走 Rich 默认）；只有浅色变体通过 `Theme.markdown_styles()`（字段 `markdown_code`，完整样式串）覆盖代码两键、并通过 `Theme.code_theme`（pygments 主题名）给 `Markdown(code_theme=...)` 换代码块高亮，`tui.on_mount` 非空时才 `console.push_theme(...)`。
- TUI 分层：显示层文本处理（CJK 断行 / 转义清洗）在 `textkit.py`（纯函数、无 Textual 依赖），配色与 CSS 在 `theme.py`；其余全在 `tui.py`（应用编排 + 控件 + 日志区框选复制 `SelectableRichLog`，文件内用 `# ---- xxx ----` 分区）。`tui.py` 导入时调用 `textkit.install_cjk_wrap()` 替换 `rich.text.divide_line`（全角字逐字可断、英文词不断，纯 ASCII 走 Rich 原实现）。
- 渲染 live 事件与 resume 历史共用同一套盒子 helper（`PieApp._render_tool_call/_render_tool_result/_notify`）与 `_format_tool_args`——要改工具盒样式改一处即可，别在事件分支里另写一份。

## 已知限制

- 剪贴板图片粘贴（Ctrl+V / Ctrl+G / `/paste`）依赖 Pillow + 平台剪贴板：Windows/macOS 开箱可用；Linux 要装 `wl-clipboard` 或 `xclip`（WSLg 下 `wl-clipboard` 即可，剪贴板桥会转发图片）；两者都缺时回退 WSL 的 PowerShell 后端。Windows Terminal 会截走 Ctrl+V → 用 Ctrl+G 或 `/paste`。
- resume 只能恢复最近一次会话，不支持选择历史会话。
- 记忆文件靠 agent 主动更新，没有自动摘要。

