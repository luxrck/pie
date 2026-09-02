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
│   ├── session.py     # 会话层：Session 多轮对话 + JSONL 持久化
│   ├── config.py        # 配置持久化（~/.pie/config.toml）+ system prompt 组装
│   ├── context.py     # 上下文管理：三级压缩 + 全文落盘 + 摘要指针
│   ├── tools.py       # 工具层：@tool 装饰器 + ToolRegistry + 内置工具
│   ├── llm.py         # 模型层：LLM 协议 + OpenAILLM
│   ├── loop.py        # 循环层：run_agent
│   └── cli.py         # CLI：pie（新对话）/ pie resume（恢复最近会话）
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
```

## 约定

- Python >= 3.11，依赖用 uv 管理。
- 用户明确要求 YOLO：不加权限确认、不做沙箱。
- 新工具：普通函数 + `@tool()` 装饰器 + `registry.register()`。
- 新配置项：在 `src/pie/config.py` 的 `Config` dataclass 中加字段，配置文件为 ~/.pie/config.toml。
- 配置只从 ~/.pie/config.toml 读取；首次运行由 `ensure_config()` 交互式写入，旧 config.json 自动迁移。
- 会话每轮自动保存到 ~/.pie/sessions/，`pie resume` 恢复最新文件。
- 上下文压缩在 src/pie/context.py：摘要全部规则式（轮次 user+最终输出 / 会话级指针 + 保护区域 verbatim）；保护区域 = 当前轮（进行中）verbatim，已完成轮次可被轮次级/会话级压缩；keep_last_steps 只决定当前轮内最近 N 批保留。
- 每次压缩事件写会话 manifest（~/.pie/context/<session>-manifest.jsonl）并返回统计；`Session.compression_history()` 可查压缩历史。
- system prompt 分层：SYSTEM.md（角色/原则）+ AGENTS.md（项目）+ MEMORY.md（记忆），由 `build_system_prompt(config)` 组装。
- 提示词文件按“当前目录向上找项目根”解析，支持在任意目录运行 `pie`。
- 用户偏好与重要决策写入 MEMORY.md，而不是散落在代码注释里。

## 已知限制

- resume 只能恢复最近一次会话，不支持选择历史会话；仍不支持并行工具调用与流式输出。
- 记忆文件靠 agent 主动更新，没有自动摘要。

