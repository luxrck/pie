# pie — 极简 agent harness

四个内置工具、零权限审计（YOLO 模式）。模型走 OpenAI 兼容接口，可对接任意兼容服务（DeepSeek、Qwen、vLLM、Ollama 等）。

## 目录结构

```text
pie/
├── src/pie/
│   ├── __init__.py    # 公开 API
│   ├── __main__.py    # python -m pie 入口
│   ├── chat.py        # 会话层：Session 多轮对话 + JSONL 持久化
│   ├── config.py        # 配置持久化（~/.pie/config.toml）+ system prompt 组装
│   ├── context.py     # 上下文管理：三级压缩 + 全文落盘 + 摘要指针
│   ├── tools.py       # 工具层：内置工具 + 注册表 + @tool 装饰器
│   ├── llm.py         # 模型层：LLM 协议 + OpenAI 兼容实现
│   ├── loop.py        # 循环层：run_agent + Config
│   └── cli.py         # CLI：pie（新对话）/ pie resume（恢复最近会话）
├── AGENTS.md          # 项目说明（面向 agent/协作者）
├── MEMORY.md          # 项目持久记忆
├── SYSTEM.md          # agent 运行时 system prompt
├── pyproject.toml
└── README.md
```

设计吸收了主流 harness 的理念：分层提示词（SYSTEM / AGENTS / MEMORY）、持久记忆、配置持久化、对话历史落盘、可注入的模型后端与工具集。

## 内置工具

| 工具 | 作用 |
| --- | --- |
| `read(path, offset?, limit?)` | 读取文件内容（不截断；大文件按 1 起的 offset + limit 分页读取） |
| `edit(path, edits)` | 一次做多个精确替换：oldText 在原文中必须唯一且互不重叠，按原文一次性应用 |
| `write(path, content)` | 写入 / 覆盖文件，自动创建父目录 |
| `shell(cmd, timeout=120, cwd=None, limit=200)` | 执行 shell 命令；输出超过 limit 行时全文落盘，只返回文件指针 + 最后 limit 行 |

没有权限审计：`shell` 直接执行，文件工具不做路径限制，风险自负。

## 快速开始

```bash
uv sync
uv tool install . --editable   # 可选：装成全局命令
pie                            # 首次运行会先引导写入模型配置
```

首次运行会交互式询问模型名、API 地址和 API key，写入 `~/.pie/config.toml` 后从文件读取并启动对话。以后直接 `pie` 进入新对话，`pie resume` 恢复最近一次会话。

## 全局安装（任意目录运行）

```bash
uv tool install . --editable   # 把 pie 装成全局命令
pie                            # 任意目录下直接运行
```

提示词文件（`SYSTEM.md` / `AGENTS.md` / `MEMORY.md`）会从当前目录向上寻找最近的“项目根”（含任一提示词文件或 `.git` 的目录）。在别的项目里运行时会自动加载那个项目的上下文；找不到就退回内置 prompt + 全局记忆（`~/.pie/memory.md`）。

## 交互式多轮对话

```bash
pie "帮我看看这个项目"      # 新对话，并把该消息作为第一条输入
pie -r                      # 恢复最近的会话（等价 pie resume）
pie --session 20260831-103224   # 恢复指定会话（id / 文件名 / 路径）
```

直接输入消息即可，每条消息都会走完整的工具循环（read / edit / write / shell），历史上下文在会话内持续保留。
每轮对话自动保存到 `~/.pie/sessions/<时间戳>.jsonl`，所以 `pie resume` 能恢复最近会话。
真实终端下使用 **Textual TUI**（pi / tau 风格：消息流、工具调用日志、状态栏、底部输入）；非 TTY（管道/脚本）自动回退 readline。
对话内命令：`/exit` 退出、`/reset` 清空历史、`/clear` 当前窗口写入 fs 归档并开新窗口（新窗口带旧窗口的摘要 + 文件指针）、`/compact [tools|turns]` 手动压缩、`/save [文件]` 保存为 JSONL、`/stat` 查看 token 使用情况与当前会话文件、`/help` 查看帮助。
输入使用 prompt_toolkit 做 Unicode 安全行编辑：中文退格按字符删除，不会出现半个字符导致的 UTF-8 错误；管道/脚本输入时自动回退 `input()`。

## 一次性执行（子 agent / 摘要）

```bash
pie -p "把 /tmp/out.txt 总结成结构化摘要"  # 非交互一次性执行，打印最终答案
cat 任务.txt | pie -p                      # 任务从 stdin 读取
echo "任务" | pie                          # 非 TTY 下自动进入一次性模式
pie -p "任务" --mode json                  # 输出 JSON（answer/usage/session）
pie -p "任务" --mode transcript            # 输出完整消息转录
pie -r -p "继续刚才的任务"                 # 在最近会话上追加执行一条消息
```

一次性模式不写入 sessions（不会污染 `pie resume`），stdout 只输出最终答案，适合被父 agent 通过 shell 调用——这就是本项目 subagent 的实现形式：父 agent 用 `shell("pie ...")` 派一个独立上下文的子 agent。

注意命令行参数有长度上限（Linux 单个参数约 128KiB，macOS 总参数约 256KiB，Windows 更小），长任务请走 stdin，或先落盘成文件、再让子 agent 用 `read` 读取。

## 命令行参数（参照 tau 设计）

```text
pie [OPTIONS] [PROMPT]

-p, --print                   非交互一次性执行（子 agent 模式）
    --mode text|json|transcript   print 输出格式（默认 text）
-m, --model NAME              本次运行的模型（覆盖配置，不持久化）
-t, --thinking LEVEL          本次思考强度（off..max，覆盖配置，不持久化）
-c, --config FILE             指定配置文件（默认 ~/.pie/config.toml）
-r, --resume                  恢复最近的会话
    --session ID              恢复指定会话（id / 文件名 / 路径）
    --session-id ID           为新会话指定精确 id
    --cwd PATH                内置工具的工作目录
    --system-prompt TEXT_OR_PATH     替换 SYSTEM.md 基础提示
    --append-system-prompt TEXT_OR_PATH  追加到 system prompt（可重复）
    --auto-compact-threshold TOKENS   上下文超过该 token 估算即自动压缩
    --timeout-seconds / --max-retries / --max-retry-delay-seconds  HTTP 与重试
-v, --version                 显示版本
```

子命令：`pie resume`（等价 `-r`）、`pie sessions`（列出会话）、`pie setup`（配置向导）、`pie context`（压缩维护）。

## 配置持久化

配置全部保存在 `~/.pie/config.toml`，CLI 不带任何配置参数，需要修改时直接编辑该文件：

```toml
model = "deepseek-v4-flash"
base_url = "https://api.deepseek.com/"
api_key = "sk-..."   # 默认 key 已写入代码与配置
reasoning_effort = "high"
verbose = true
max_seq_len = 128000
keep_last_steps = 5
context_soft_ratio = 0.8
context_target_ratio = 0.55
timeout_seconds = 60.0
max_retries = 2
max_retry_delay_seconds = 1.0
[compaction]
enabled = true

[compaction.tool]
head = 10
tail = 10

[compaction.session]
enabled = true
head = 3
tail = 2
```

旧的 `~/.pie/config.json` 会在首次加载时自动迁移为 toml。
`pie -c FILE` 可以指定其他配置文件（等价于设置 `PIE_CONFIG_FILE` 环境变量），适合给子 agent 单独准备一套轻量配置。

## 上下文与记忆

每次运行组装 system prompt 时按分层加载：

- `SYSTEM.md` — 角色与工作原则（运行时提示词主体）；
- `AGENTS.md` — 项目说明（架构、命令、约定）；
- `MEMORY.md` — 项目持久记忆；agent 可以用 edit/write 更新它，跨会话保留；
- `~/.pie/memory.md` — 可选的全局记忆（跨项目），存在即注入。

## 上下文压缩

三级压缩，信息不删除只换表示：全文落盘到 `~/.pie/context/`（内容 sha256 前 16 位命名，同内容只落一份），上下文里只留“摘要 + 文件路径指针”。摘要全部为**规则式**（免费、确定、不调模型）：

- 工具级由工具自身承担：`shell` 带 `limit` 参数（默认 200 行），超限时全文落盘（`shell-<hash>.txt`）并返回“指针 + 最后 limit 行”；`read` 用 `offset/limit` 分页。harness 不再设阈值。
- 轮次级：把最老的未压缩轮次压成 `user + 最终模型输出 assistant`（附原文文件指针）。
- 当前轮次 step 级：进行中的轮次若仍超限，只把窗口之外批次的 **tool 结果文本**落盘为“指针 + 预览”（消息全部保留），最近 `keep_last_steps` 批保持完整。
- 保护区域 = 当前（进行中）轮次整段 verbatim；已完成轮次不受保护，可被轮次级/会话级压缩；`keep_last_steps` 只决定当前轮内最近 N 批完整保留、更早批次的工具文本落盘。

触发：软阈值 `context_soft_ratio × max_seq_len`，压缩到 `context_target_ratio` 水位（迟滞防抖）；token 计数优先使用 API 返回的 `usage.prompt_tokens`，无上报时用字符估算。单条消息的压缩级别只升不降（0=原始 → 2=轮次级/step → 3=会话级；旧文件的 1=工具级 仍兼容）。

每次压缩事件都会记录统计（节省 token、各级数量）并写入会话的压缩索引：`~/.pie/context/<session>-manifest.jsonl`，包含时间戳、级别、类型（tool/turn/session）、是否滚动、原文文件路径与 hash、摘要。程序可通过 `Session.compression_history()` / `verify_context()` / `raw_history()` 读取与校验。

压缩维护命令：

```bash
pie context info            # 列出所有压缩事件
pie context verify          # 校验 manifest 引用的原文文件是否存在
pie context gc              # 列出未被引用的 context 文件（GC 预览）
pie context gc --delete     # 删除未引用文件
```

## 自测

不调用模型，验证四个工具与自动生成的 schema（开发用）：

```bash
uv run python -c "from pie.cli import self_check; self_check()"
```

## 扩展：自定义工具

`@tool()` 装饰器会根据函数签名自动生成参数 schema，注册进 `ToolRegistry` 即可：

```python
from pie import default_tools, run_agent, tool

@tool()
def add(a: int, b: int) -> str:
    """把两个数加起来。"""
    return str(a + b)

tools = default_tools()
tools.register(add)
run_agent("1 + 1 = ?", tools=tools)
```

也支持 `name=`、`description=`、`parameters=` 覆盖自动生成的元数据。

## 扩展：自定义模型后端

实现 `complete(messages, tools, model=None) -> LLMResult` 即可，协议见 `pie/llm.py`：

```python
from pie import Config, run_agent

class MyLLM:
    def complete(self, messages, tools, model=None):
        ...

cfg = Config(model="my-model")  # 或编辑 ~/.pie/config.toml
run_agent("...", llm=MyLLM(), config=cfg)
```

