# pie

`pie` 是一个极简的 agent harness，**纯 Rust 实现**。内置 read / edit / **writ** / **bash** 四个工具、
YOLO 模式（无权限确认、不做沙箱）、模型走 OpenAI 兼容接口（DeepSeek / Qwen / vLLM / Ollama…）。
对外三样东西：可执行文件 `pie`（CLI + TUI）、库 `pie`、Python 绑定（`bindings/pie-py`，`import pie`）。

## 安装

```bash
cargo install --path .     # 装成 ~/.cargo/bin/pie（更新已装的加 --force）
```

cargo 不在默认 PATH 时先 `export PATH="$HOME/.cargo/bin:$PATH"`。构建依赖只要 `cc` —— TLS 走 rustls、
图片用纯 Rust 的 `image`，**不需要** libssl / pkg-config / cmake。

## 命令行

```
pie [OPTIONS] [TASK]... [COMMAND]
```

**模式**（看有没有给 `TASK`）：

- `pie` —— 真终端 + 无任务 → 进 TUI。（`-r` / `-s` 则接着那个会话聊。）
- `pie "任务"` —— **一次性**（子 agent）：答案走 stdout，不落盘、不写会话。
- `echo "任务" | pie` —— 非终端下从 stdin 读任务（同一次性）。
- `pie -s <id|路径> "任务"` —— 在指定会话上跑一轮（不存在则新建），跑完落盘。
- `pie -r "任务"` —— 同上，会话取**最近**那个（同工作目录优先）。

**子命令**：

| 命令 | 作用 |
| --- | --- |
| `pie setup` | 补齐 `~/.pie/` 下缺的默认件（配置 + 全局记忆），已存在不动 |
| `pie sessions [-l N] [--all] [--json]` | 列出历史会话（默认 20 个） |
| `pie context info` / `verify` / `gc [--delete]` | 列出压缩事件 / 校验落盘原文是否还在 / 列出或删除未引用的 `~/.pie/context/` 文件 |
| `pie files list [--all]` / `gc [--delete] [--all]` | 列出图片记录 / 回收本地副本（`--all` 作用于云端上传件） |

**选项**（都只作用于本次运行，不写回配置）：

| 选项 | 说明 |
| --- | --- |
| `-c, --config <路径>` | 指定配置文件（默认 `~/.pie/config.toml`） |
| `-m, --model <名字>` | 覆盖模型 |
| `-t, --thinking <档>` | 思考强度：`off` / `none` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max`（`off` = `none`） |
| `--reserved-tokens <N>` | 为输出预留的 token（= API 的 `max_tokens`；如 `128k` / `auto`） |
| `--max-steps <N>` | 单回合最多问几次模型 |
| `--no-stream` | 强制非流式（一次性 `complete`） |
| `--auto-compact-threshold <N>` | 上下文估算超过该值即自动压缩 |
| `--timeout-seconds` / `--max-retries` / `--max-retry-delay-seconds` | HTTP 超时 / 重试次数 / 重试等待上限 |
| `--cwd <目录>` | 工具的工作目录（默认当前目录） |
| `--tools <名单>` | 限制可用工具，如 `read,ls,grep`（自带名 `read/edit/writ/bash` 启用对应工具，其余当 shell 子命令白名单） |
| `--system-prompt <文本或路径>` / `--append-system-prompt <…>` | 替换基础提示 / 追加到 system prompt（可重复） |
| `--mode <text\|json\|transcript>` | 一次性模式的输出格式（默认 `text`） |
| `--stat` | 跑完把 `/stat` 报告（上下文占用 / 压缩事件 / API 用量）打到 stderr |
| `--models` | 列出端点可用模型 id |

`pie <子命令> --help` 看细节。

## 写一个工具

**一个工具 = 一个结构体**：字段即参数、doc 首行即描述、非 `Option` 即必填，schema 由
`#[derive(Deserialize, JsonSchema)]`（serde + schemars）派生。加工具 = 写结构体 + `impl Tool` +
在 `ToolRegistry::new()` 里加一行 `.with_tool::<T>("名字")`。其余约定（trait 签名、schemars 的坑）
见 `AGENTS.md`。

## Python 绑定（`import pie`）

核心层（config / llm / tools / session / context）的原生扩展；**TUI 不进绑定**。规划见
[`docs/python-bindings.md`](docs/python-bindings.md)。

```bash
cd bindings/pie-py
export PATH="$HOME/.cargo/bin:$PATH"
uv venv --python 3.12 .venv && uv pip install --python .venv/bin/python maturin pytest
VIRTUAL_ENV=$PWD/.venv .venv/bin/maturin develop && .venv/bin/python -m pytest tests -q   # 不联网，本地假 SSE
```

```python
import pie
cfg = pie.Config.load()                          # ~/.pie/config.toml（只改内存，不写盘）
session = pie.Session.ephemeral(cfg, pie.LlmClient(cfg), pie.ToolRegistry.builtins(cfg))
answer = session.aturn("看看当前目录", on_event=lambda ev: print(ev["type"]))
```

三条约定：**同步外观但释放 GIL**（并发用 `asyncio.to_thread`）；**一个 Session 同时只跑一个回合**；
**`messages` / 事件都是 dict**，字段名与 JSONL 一致。

## 更多

- `AGENTS.md` —— 工作约定 / 目录结构 / 常用命令（运行时被拼进 system prompt）
- `MEMORY.md` —— 项目持久记忆（同上；含本机构建细节）
- `docs/CHANGELOG.md` —— 历史决策与变更
