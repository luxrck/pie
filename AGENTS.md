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
│   ├── config.py      # 配置持久化（~/.pie/config.toml）+ system prompt 组装
│   ├── session.py     # 会话层：多轮对话 + JSONL 持久化 + 用量累计
│   ├── loop.py        # 循环层：run_agent / aturn（工具调用编排 + 取消）
│   ├── context.py     # 上下文管理：三级压缩 + 全文落盘 + 摘要指针
│   ├── tools.py       # 工具层：@tool 装饰器 + ToolRegistry + 内置工具
│   ├── llm.py         # 模型层：LLM 协议 + OpenAI 兼容实现
│   ├── input.py       # 非 TTY 行编辑（prompt_toolkit，中文退格安全）
│   ├── theme.py       # TUI 展示数据：配色 + 图标 + build_css（纯字符串，不依赖 Textual）
│   ├── textkit.py     # 显示层文本处理：CJK 友好断行 + 控制符/ANSI 转义清洗（纯函数）
│   └── tui.py         # TUI 应用：布局/命令/回合 worker/流式渲染 + 日志区控件与框选复制
├── tests/test_tui.py  # TUI 回归测试（无 pytest 依赖）
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
- 新配置项：在 `src/pie/config.py` 的 `Config` dataclass 中加字段，配置文件为 ~/.pie/config.toml。
- 配置只从 ~/.pie/config.toml 读取；首次运行由 `ensure_config()` 交互式写入，旧 config.json 自动迁移。
- 会话每轮自动保存到 ~/.pie/sessions/，`pie resume` 恢复最新文件。
- 上下文压缩在 src/pie/context.py：摘要全部规则式（轮次 user+最终输出 / 会话级指针 + 保护区域 verbatim）；工具级保护窗口由 keep_last_steps 决定——最近 N 个 step 批次（跨轮次滚动）内的工具结果 verbatim 保留，窗口外（含历史轮次）未压缩工具结果落盘成指针；轮次级只保护当前轮，已完成轮次可被轮次级/会话级压缩。
- 每次压缩事件写会话 manifest（~/.pie/context/<session>-manifest.jsonl）并返回统计；`Session.compression_history()` 可查压缩历史。
- system prompt 分层：SYSTEM.md（角色/原则）+ AGENTS.md（项目）+ MEMORY.md（记忆），由 `build_system_prompt(config)` 组装。
- 提示词文件按“当前目录向上找项目根”解析，支持在任意目录运行 `pie`。
- 用户偏好与重要决策写入 MEMORY.md，而不是散落在代码注释里。
- TUI 分层：显示层文本处理（CJK 断行 / 转义清洗）在 `textkit.py`（纯函数、无 Textual 依赖），配色与 CSS 在 `theme.py`；其余全在 `tui.py`（应用编排 + 控件 + 日志区框选复制 `SelectableRichLog`，文件内用 `# ---- xxx ----` 分区）。`tui.py` 导入时调用 `textkit.install_cjk_wrap()` 替换 `rich.text.divide_line`（全角字逐字可断、英文词不断，纯 ASCII 走 Rich 原实现）。
- 渲染 live 事件与 resume 历史共用同一套盒子 helper（`PieApp._render_tool_call/_render_tool_result/_notify`）与 `_format_tool_args`——要改工具盒样式改一处即可，别在事件分支里另写一份。

## 已知限制

- resume 只能恢复最近一次会话，不支持选择历史会话。
- 记忆文件靠 agent 主动更新，没有自动摘要。

