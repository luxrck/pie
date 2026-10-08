# pie

`pie` 是 [`pi`](https://github.com/earendil-works/pi)（TypeScript 写的 agent harness）的 **Rust 重实现**：一个极简的
agent harness —— 内置 read / edit / **writ** / **bash** / **repl** 五个工具，另可加载**用 Python 写的工具**
（见「用 Python 写工具」）。对外提供 cli 可执行文件、Rust API 库和相应的 Python 绑定。

五个工具里，`repl` 不是「跑一段 Python 就完」的沙箱，而是**本会话的一台长活解释器**（见下面那节）。

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
| `pie setup` | 补齐 `~/.pie/` 下缺的默认件：配置 + 全局记忆 + **默认 Python 环境**（`uv` 建 `~/.pie/envs/base`，并把 `repl` 要的 IPython 装进去）；已存在不动，没装 uv 只告警 |
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
| `--auto-compact-threshold <N>` | 服务端上报的上下文超过该值即自动压缩 |
| `--timeout-seconds` / `--max-retries` / `--max-retry-delay-seconds` | HTTP 超时 / 重试次数 / 重试等待上限 |
| `--cwd <目录>` | 工具的工作目录（默认当前目录） |
| `--tools <名单>` | 限制可用工具，如 `read,ls,grep`（内置名 `read/edit/writ/bash/repl` 启用对应工具；其余名字先跟用 Python 写的工具对，对不上当 shell 子命令白名单） |
| `--system-prompt <文本或路径>` / `--append-system-prompt <…>` | 替换基础提示 / 追加到 system prompt（可重复） |
| `--mode <text\|json\|transcript>` | 一次性模式的输出格式（默认 `text`） |
| `--stat` | 跑完把 `/stat` 报告（上下文占用 / 压缩事件 / API 用量）打到 stderr |
| `--models` | 列出端点可用模型 id |

`pie <子命令> --help` 看细节。

## repl：会话内持久的 IPython

`repl` 是**同一个进程**里的一台 IPython：变量 / import / 定义跨调用、**跨轮次**都留着。
所以「加载一次、反复掏」这类活儿（大表、模型、连接、子进程）只付一次代价，而不是用 `bash` 每轮重跑一遍。

```python
# 第一次调用
import pandas as pd
df = pd.read_csv("sales.csv")          # 贵的活只做一次

# 下一个 cell、下一轮对话都还在同一个解释器里
df.groupby("month")["amount"].sum()
df.plot()                              # 图直接回到界面（见下）
```

- **状态可见**：结果头区那行 `[解释器] 共 12 个：df, model, pd …（本次新增：df）` 每次都报当前命名空间。
  上下文压缩可能已经把你写过的代码块卷走了，所以「解释器里现在有什么」由**每次执行自己带回来**，而不是靠模型回忆。
- **`history()`**：解释器里能拿到**本会话的完整转录**（压缩指针已展开，逐条含角色 / 内容 / 代码 / 落盘全文的路径）
  —— 记不清早先写过什么、跑过什么就查它。
- **图能活着回来**：matplotlib 的 figure 走结构化通道回传，TUI 里 `←` / `→` 切到 **REPL 画布**（或 `/repl`）直接看；
  图会转存成内容寻址副本并登记进会话，`-r` 回来还在。
- **程序不会因压缩消失**：被压缩带走的 repl 代码块会集中列在 system prompt 末尾那节
  （`__meta__.repl_blocks`）—— 丢的是**体积**（工具输出），留的是**结构**（代码 + 结论）。
- **中断不丢状态**：`Esc` 取消给解释器发 `SIGINT`（命名空间保住），停不住才 `SIGKILL`；输出超限与 `bash` 同款（落盘 + 指针）。
- **解释器从哪来**：`[tools.repl] _python` → `[python] interpreter` → `~/.pie/envs/base/bin/python`（`pie setup` 用 uv 建好、
  连 IPython 一起装上）→ `python3`。所以只跑过 `pie setup` 的话，`repl` 开箱就能用（系统 python3 通常没有 IPython）。

什么时候该用它：分几段写、边看边改（探查数据 / 调参数 / 试算法）；任务里有**贵的准备**；要**画图**。
反过来，一次性命令（grep / 看目录 / 构建 / 测试）用 `bash` 更直接 —— `bash` 每次都是新进程。

⚠ `pie -r` 恢复会话**不带回**解释器内存（新进程里是空的解释器：只有历史，没有变量）；`history()` 照旧能查到之前跑过的代码。

## 用 Python 写工具

`pie` 二进制可以直接加载 **Python 写的工具**（不嵌 CPython、不用装 Python 绑定）：文件里用与绑定同款的
`@pie.tool`，启动时起一个长活宿主进程读清单，把工具注册进**同一张**工具表 —— 模型看不出它和内置工具有什么区别。

```python
# ~/.pie/tools/word_count.py
from pie import tool

@tool(description="统计文本字符数")      # 描述可省（退到 docstring 首行 → 函数名）
def word_count(text: str) -> str:
    return str(len(text))
```

```toml
# ~/.pie/config.toml
[python]
interpreter = "~/.pie/envs/base/bin/python"   # 缺省就是这样（`pie setup` 建的环境，存在即用）
tools = ["~/.pie/tools"]                     # 文件或目录（目录取其下 *.py，不递归；`~` 会展开）
```

不用写 `interpreter` —— **只要 `envs/base` 存在就用它**（`pie setup` 给你建），`repl` 与 Python 工具共用同一个环境，
所以要装依赖只装一处：`uv pip install --python ~/.pie/envs/base/bin/python pandas`。

工具文件放 `[python] tools` 列出的路径，或直接丢进约定目录 `~/.pie/tools/`（存在即自动加载，不必写配置）。

- **并发**：同一批的多个 `tool_calls` 一起进宿主、并发跑（`parallel_tools = false` 时按顺序），应答靠 `id` 配对。
- **取消**：`Esc` 杀掉宿主进程组（在飞的调用一起失败），**下次调用自动重启**。
- **失败不阻断**：起不来 / 导入报错 / 与内置工具重名 / 名字不合法（只收 `[A-Za-z0-9_-]{1,64}`）→ 只告警，那一条不注册，会话照跑。
- 工具里的 `print` 与 traceback 走 stderr → pie 的告警通道，**不会污染工具结果**（协议走单独的 fd）。
- 参数 schema 从**类型注解**生成（`str/int/float/bool/list/dict/Optional`）；下划线开头的参数是注入项、不进 schema；
  handler 必须**同步**、必须返回 `str`。超限与内置工具同款：`[tools.<名字>] _max_lines` / `_max_bytes`
  （这两个是**宿主**的旋钮，不会传给 handler；超限→ 全文落盘 + `[工具输出全文已保存: …]` 指针）。
- 与 `repl` 的关系：**共用解释器（同一个 venv），不共用进程**——工具里 `import` 得到的包，repl 里也 import 得到；
  但两边状态不互通（repl 的命名空间是模型可写的，工具不该和它共享）。解释器解析顺序：
  `[python] interpreter` → `~/.pie/envs/base`（存在即用，`pie setup` 建的） → `python3`。
- 两条边界：工具进程的 **cwd 固定在启动时**（TUI 里 `/cd` 不改它，要跟 cwd 走请用绝对路径）；**会话中途改工具文件不生效**，重启 `pie`。

## 上下文压缩

长会话不靠「丢老消息」续命：pie 自己做**三级压缩**，把旧内容换成**指针 + 落盘原文**——模型看到的是摘要/预览，原文都在盘上，
**可逆**（`-r` 恢复会话时会把指针展开成完整转录）。

| 级别 | 压的是什么 | 压成什么 |
| --- | --- | --- |
| 工具级 | 最近 `keep_last_steps`（默认 7）个 step 批次**之外**、行数超过 `head+tail`（默认 30+50）的工具输出 | 头尾预览 + `[工具输出全文已保存: <path>]`，全文落盘 |
| 轮次级 | 已完成的回合（最后一个 user 之前的） | 一条摘要 assistant（只留模型最终输出）+ `[轮次原文已保存: <path>]`，原文落盘 |
| 会话级 | 当前窗口的整段历史（system 与旧窗口摘要除外） | 一个窗口块（`~/.pie/windows/window-<hash>`）+ 一条 `[历史窗口: <path>]` 摘要 system 消息（摘要里留头 3 / 尾 5 轮原文）；**只由用户手动 `/clear` 触发** |

- 自动压缩**只看服务端上报的水位**（最近一次 `usage.prompt_tokens`，相对可用输入预算 `context_window - reserved_tokens`，后者就是发给 API 的 `max_tokens`）：
  超过 `soft_ratio`（默认 0.8）就把**工具级 / 轮次级**压到不能再压。**不做 token 估算**——
  本次会话还没发过请求（没有上报）时不压。**会话级（整窗口归档）只由用户手动 `/clear` 触发**：
  换窗口会让当前轮的工作记忆只剩摘要（模型「失忆」），自动做不合适；服务端 400 被认成「上下文超限」时
  只会再强压一次工具级/轮次级，压不动就报错（`log::warn` 提示 `/clear`）。
  TUI 里可以 `/compact [tools|turns|auto]` 手动压一次（不看水位）；`pie --stat "任务"` 跑完会把上下文占用 / 压缩事件 / API 用量打到 stderr。
- 压缩级别**只升不降**；落盘按内容 sha256 寻址（同一份内容只存一次）。维护用 `pie context info` / `verify` / `gc`（见上面的子命令表）。
- 压掉的是**体积**，不是**结构**：被压走的 repl 代码块会集中保留在 system prompt 末尾那节（见上面 repl 那节）；
  `[轮次原文已保存: …]` / `[历史窗口: …]` 这些指针都是**真路径**，模型可以 `read` 回去。
- 目录分工：压缩落盘在 `~/.pie/context/`（`context gc` 的地盘）；`/clear` 归档的窗口块在 `~/.pie/windows/`（用户主动归档，GC 不碰）。

```toml
[compaction]                 # 写了即开启；整段 `compaction = false` = 完全不压，某级 `false` = 只关那一级
turn         = true          # 轮次级（level 2）：已完成的回合 → 一条摘要（只留 user + 最终输出）；没参数，只能开关
soft_ratio   = 0.8           # 上报的占用超过可用输入预算的这个比例就自动压（各级压到不能再压）

[compaction.tool]            # 工具级（level 1）：工具输出超过 head+tail 行就全文落盘，头/尾留预览
head = 30
tail = 50
keep_last_steps = 7          # 保护窗口：最近 7 个 step 批次不碰（只有工具级有这个概念）

[compaction.session]         # 会话级（level 3）：把历史落成窗口块；摘要里保留头 head / 尾 tail 轮原文
                             # （只影响 `/clear` 与超限兜底；自动压缩不做这一级）
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
