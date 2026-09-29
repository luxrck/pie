# pie

`pie` 是 [`pi`](https://github.com/earendil-works/pi)（TypeScript 写的 agent harness）的 **Rust 重实现**：一个极简的
agent harness —— 内置 read / edit / **writ** / **bash** 四个工具。对外提供 cli 可执行文件、Rust API 库和相应的 Python 绑定。

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

## 上下文压缩

长会话不靠「丢老消息」续命：pie 自己做**三级压缩**，把旧内容换成**指针 + 落盘原文**——模型看到的是摘要/预览，原文都在盘上，
**可逆**（`-r` 恢复会话时会把指针展开成完整转录）。

| 级别 | 压的是什么 | 压成什么 |
| --- | --- | --- |
| 工具级 | 最近 `keep_last_steps`（默认 7）个 step 批次**之外**、行数超过 `head+tail`（默认 30+50）的工具输出 | 头尾预览 + `[工具输出全文已保存: <path>]`，全文落盘 |
| 轮次级 | 已完成的回合（最后一个 user 之前的） | 一条摘要 assistant（只留模型最终输出）+ `[轮次原文已保存: <path>]`，原文落盘 |
| 会话级 | 当前轮之前的整段历史 | 一个窗口块（`~/.pie/context/session-*.txt`）+ 一条 `[历史窗口: <path>]` 摘要 system 消息（摘要里留头 3 / 尾 5 轮原文） |

- 自动压缩看**可用输入预算**（`context_window - reserved_tokens`，后者就是发给 API 的 `max_tokens`）：超过 `soft_ratio`
  （默认 0.8）就压到 `target_ratio`（默认 0.55）以下——软阈值 + 目标水位构成**迟滞**，不会压完又弹回去。
  TUI 里可以 `/compact [tools|turns|auto]` 手动压一次（不看水位）；`pie --stat "任务"` 跑完会把上下文占用 / 压缩事件 / API 用量打到 stderr。
- 压缩级别**只升不降**；落盘按内容 sha256 寻址（同一份内容只存一次）。维护用 `pie context info` / `verify` / `gc`（见上面的子命令表）。
- 目录分工：压缩落盘在 `~/.pie/context/`（`context gc` 的地盘）；`/clear` 归档的窗口块在 `~/.pie/windows/`（用户主动归档，GC 不碰）。

```toml
keep_last_steps = 7          # 顶层：最近 7 个 step 批次不动（工具级不碰它们）

[compaction]                 # 写了即开启；整段 `compaction = false` = 完全不压，某级 `false` = 只关那一级
turn         = true          # 轮次级（level 2）：已完成的回合 → 一条摘要（只留 user + 最终输出）；没参数，只能开关
soft_ratio   = 0.8           # 超过可用输入预算的这个比例就自动压
target_ratio = 0.55          # 压到这个水位以下（软阈值 + 目标水位 = 迟滞，不来回抖）

[compaction.tool]            # 工具级（level 1）：工具输出超过 head+tail 行就全文落盘，头/尾留预览
head = 30
tail = 50

[compaction.session]         # 会话级（level 3）：整段历史落成窗口块；摘要里保留头 head / 尾 tail 轮原文
head = 3
tail = 5
```

## Python 绑定

构建后 `import pie`（扩展模块是 `pie._pie_rs`，TUI 那堆依赖不进绑定）：

```python
import pie
cfg = pie.Config.load()                          # ~/.pie/config.toml（只改内存，不写盘）
session = pie.Session.ephemeral(cfg, pie.LlmClient(cfg), pie.ToolRegistry.builtins(cfg))
answer = session.aturn("看看当前目录", on_event=lambda ev: print(ev["type"]))
```

## 更多

- `AGENTS.md` —— 工作约定 / 目录结构 / 常用命令（运行时被拼进 system prompt）
- `MEMORY.md` —— 项目持久记忆（同上；含本机构建细节）
- `docs/CHANGELOG.md` —— 历史决策与变更
