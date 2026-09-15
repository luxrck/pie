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
│   └── cli.py         # CLI：pie（新对话）/ pie resume（恢复当前目录最近的会话）
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
| `read(path, offset?, limit?)` | 读取文件内容（不截断；大文件按 1 起的 offset + limit 分页读取）；图片（PNG/JPEG/GIF/WebP/BMP）返回图像引用并作为多模态消息发给模型，offset/limit 不适用 |
| `edit(path, edits)` | 一次做多个精确替换：oldText 在原文中必须唯一且互不重叠，按原文一次性应用 |
| `write(path, content)` | 写入 / 覆盖文件，自动创建父目录 |
| `shell(command, timeout=None)` | 执行 shell 命令（YOLO）；timeout 不设则无超时；超长输出交回 harness 工具级压缩处理 |

没有权限审计：`shell` 直接执行，文件工具不做路径限制，风险自负。

## 快速开始

```bash
uv sync
uv tool install . --editable   # 可选：装成全局命令
pie                            # 首次运行会先引导写入模型配置
```

首次运行会交互式询问模型名、API 地址和 API key，写入 `~/.pie/config.toml` 后从文件读取并启动对话。以后直接 `pie` 进入新对话，`pie resume` 恢复当前目录最近一次会话。

## 全局安装（任意目录运行）

```bash
uv tool install . --editable   # 把 pie 装成全局命令
pie                            # 任意目录下直接运行
```

提示词文件（`SYSTEM.md` / `AGENTS.md` / `MEMORY.md`）会从当前目录向上寻找最近的“项目根”（含任一提示词文件或 `.git` 的目录）。在别的项目里运行时会自动加载那个项目的上下文；找不到就退回内置 prompt + 全局记忆（`~/.pie/memory.md`）。

## 交互式多轮对话

```bash
pie "帮我看看这个项目"      # 新对话，并把该消息作为第一条输入
pie -r                      # 恢复当前目录下最近的会话（等价 pie resume）
pie --session 20260831-103224   # 恢复指定会话（id / 文件名 / 路径）
```

直接输入消息即可，每条消息都会走完整的工具循环（read / edit / write / shell），历史上下文在会话内持续保留。
每轮对话自动保存到 `~/.pie/sessions/<时间戳>.jsonl`，所以 `pie resume` 能恢复当前目录最近的会话。
真实终端下使用 **Textual TUI**（pi / tau 风格：消息流、工具调用日志、状态栏、底部输入；`/stop` 或 `Esc` 取消当前任务）；非 TTY（管道/脚本）自动回退 readline。
对话内命令：`/exit` 退出、`/reset` 清空历史、`/clear` 当前窗口写入 windows 归档并开新窗口（新窗口带旧窗口的摘要 + 文件指针）、`/paste` 把剪贴板里的图片存成文件并把路径插进输入框（同 `Ctrl+V` / `Ctrl+G`）、`/compact [tools|turns]` 手动压缩、`/save [文件]` 保存为 JSONL、`/thinking <none|low|high|max>` 设置思考深度（立即生效并写入配置）、`/model <id>` 切换模型（启动时自动拉取可用列表，重启仍生效；`/model` 查看列表、`/model refresh` 重新拉取）、`/stat` 查看 token 使用情况与当前会话文件、`/help` 查看帮助。
输入使用 prompt_toolkit 做 Unicode 安全行编辑：中文退格按字符删除，不会出现半个字符导致的 UTF-8 错误；管道/脚本输入时自动回退 `input()`。
以 `/` 开头、但**不是已知命令**的输入按普通消息发出（粘进来的绝对路径 `/home/.../x.png` 不会被当成未知命令吞掉）。

## 一次性执行（子 agent / 摘要）

```bash
pie -p "把 /tmp/out.txt 总结成结构化摘要"  # 非交互一次性执行，打印最终答案
cat 任务.txt | pie -p                      # 任务从 stdin 读取
echo "任务" | pie                          # 非 TTY 下自动进入一次性模式
pie -p "任务" --mode json                  # 输出 JSON（answer/usage/session）
pie -p "任务" --mode transcript            # 输出完整消息转录
pie -r -p "继续刚才的任务"                 # 在当前目录最近会话上追加执行一条消息
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
-r, --resume                  恢复当前目录下最近的会话
    --session ID              恢复指定会话（id / 文件名 / 路径）
    --session-id ID           为新会话指定精确 id
    --cwd PATH                内置工具的工作目录
    --system-prompt TEXT_OR_PATH     替换 SYSTEM.md 基础提示
    --append-system-prompt TEXT_OR_PATH  追加到 system prompt（可重复）
    --reserved-tokens N       每次请求为输出预留的 token（即 API 的 max_tokens；别名 --max-tokens）
    --auto-compact-threshold TOKENS   上下文超过该 token 估算即自动压缩
    --timeout-seconds / --max-retries / --max-retry-delay-seconds  HTTP 与重试
-v, --version                 显示版本
```

子命令：`pie resume`（等价 `-r`）、`pie sessions`（列出会话）、`pie files`（图片上传件维护）、`pie setup`（配置向导）、`pie context`（压缩维护）。

## 配置持久化

配置全部保存在 `~/.pie/config.toml`，CLI 不带任何配置参数，需要修改时直接编辑该文件：

```toml
model = "deepseek-v4-flash"
base_url = "https://api.deepseek.com/"
api_key = "sk-..."   # 默认 key 已写入代码与配置
reasoning_effort = "high"
verbose = true
context_window = 1048576       # 模型上下文窗口（输入 + 输出一起算）
reserved_tokens = 128000       # 每次请求为输出预留的 token（= API 的 max_tokens）；"auto" = 不发送、用服务端默认
files_api = true               # 图片走 Files API（上传一次拿 file_id）；失败自动回退内联 base64
files_ttl_days = 30            # 上传件在服务端的保留天数（1~30；0 = 永久保留）
keep_last_steps = 5
timeout_seconds = 60.0
max_retries = 2
max_retry_delay_seconds = 1.0
[compaction]
# turn 为 bool：true 开启轮次级（摘要只保留用户输入 + 模型最终输出），false 关闭
# turn = false
# 两个水位比例相对「可用输入预算 = context_window - reserved_tokens」
soft_ratio = 0.8
# target_ratio = 0.55

[compaction.tool]
head = 10
tail = 10

[compaction.session]
head = 3
tail = 2
```

> 旧键在加载时自动迁移（不重写你的文件）：`max_tokens` → `reserved_tokens`、`max_seq_len` → `context_window`、`context_soft_ratio`/`context_target_ratio` → `[compaction]` 的 `soft_ratio`/`target_ratio`；同名新旧键并存时新键优先。

> **可用输入预算 = `context_window` - `reserved_tokens`**：服务端按「输入 tokens + max_tokens ≤ 窗口」判超限（超了直接 400，回合被打断），所以真正能装历史的只有窗口减去输出预留。两个水位比例就是相对这份预算算的。

> `[compaction]` 不写或整段移除 = 不做任何上下文压缩；写了 `[compaction]` 即开启，**默认三级全开**，`[compaction.tool]` / `[compaction.session]` 子表只用于调整 head/tail 参数，`turn = false` / `tool = false` / `session = false` 显式关闭对应级别（`turn` 为 bool，旧 `[compaction.turn]` 子表写法自动迁移为开启）。旧配置的 `enabled` 键已移除：`enabled = false` 迁移为整段关闭（None），`enabled = true` 无效果。

旧的 `~/.pie/config.json` 会在首次加载时自动迁移为 toml。
`pie -c FILE` 可以指定其他配置文件（等价于设置 `PIE_CONFIG_FILE` 环境变量），适合给子 agent 单独准备一套轻量配置。

## 上下文与记忆

每次运行组装 system prompt 时按分层加载：

- `SYSTEM.md` — 角色与工作原则（运行时提示词主体）；
- `AGENTS.md` — 项目说明（架构、命令、约定）；
- `MEMORY.md` — 项目持久记忆；agent 可以用 edit/write 更新它，跨会话保留；
- `~/.pie/memory.md` — 可选的全局记忆（跨项目），存在即注入。

## 图片（Vision / Files API）

**粘贴剪贴板里的截图**：输入框按 `Ctrl+V`（或 `Ctrl+G`，或输入 `/paste`）会把剪贴板里的图片落成
`~/.pie/files/img-<sha256[:16]>.png` —— **就是 `read` 用的那份内容寻址副本**（同一个
`files.store_blob()`，所以回车后 read 这张图时拿到的是同一个文件，不会多出一份重复字节；
之后它被登记进会话 `__meta__.files`，与普通图片副本同命运），并把**路径**插进输入框。
`Ctrl+G` 是给「终端把 `Ctrl+V` 截给自己」的场合准备的（Windows Terminal 默认如此，按键到不了应用）。
剪贴板里没有图片时 `Ctrl+V` 照旧是文本粘贴；Windows 从资源管理器复制的图片文件
（CF_HDROP）直接返回原路径，不复制。代价是「未被引用 = 垃圾」对刚粘贴还没 read 的图不成立
→ `pie files gc` 另有 24 小时 mtime 保护窗口挡误删。
读剪贴板走 Pillow `ImageGrab.grabclipboard()`：**Windows / macOS 开箱可用**；Linux 需要装
`wl-clipboard`（Wayland）或 `xclip`（X11），**两个都没有时才**回退到 WSL 的 PowerShell 后端
（`Clipboard::GetImage()`，~0.5s）；都没有就静默当作「没有图片」。WSLg 下装了 `wl-clipboard`
就够（实测它的剪贴板桥会把 Windows 剪贴板里的图以 `image/png` 转发过来，0.035s）。

`read` 读到图片时不把文本塞进历史，而是注入一条**多模态 user 消息**（图片只能出现在 user 消息里）。默认走 Files API：

1. 图片先**复制**一份到 `~/.pie/files/<hash_id>`（`hash_id = img-<sha256[:16]>`，内容寻址、幂等）；
2. **从这个副本上传**（保证「服务端那份 == 本地这份」），拿到 `file_id`；
3. 历史里只留 `{"type":"file","file_id":"file-api-…"}`（~200 字节）：不需要每轮重发几 MB 的 base64，
   单图上限也从内联的 32 MiB 提到 64 MiB。

记录**按会话**存在 `__meta__.files`（不做全局缓存，所以没有需要 gc 的全量索引）：

```json
{"img-85fbd97740797558": {"sha256": "…", "size": 3290, "mime": "image/png", "filename": "half.png",
  "src": "/tmp/poc/half.png", "local": "~/.pie/files/img-85fbd97740797558.png",
  "file_id": "file-api-…", "base_url": "https://api.deepseek.com", "key_fp": "31d7a4c2",
  "uploaded_at": "…", "expires_at": 1791973701}}
```

不重传（命中）的条件：同一内容 hash + 同一 `base_url`/`key_fp` + 未过期。以下情况会自动重传：
新会话（记录不跨会话）、过期（默认 30 天）、换了 API key 或端点（`-c FILE` 多套配置）、服务端把文件删了。
回退链：上传失败 / 模型不支持 `file` 块（非 `deepseek-flash`）/ `files_api = false` → **静默退回内联 base64**，行为与从前一致；
请求报 `400 … file_ids do not exist or are not created under your account` → 把历史里的 `file` 块就地降级成内联（从本地副本取字节）并重试一次。

维护命令：

```bash
pie files list                # 各会话记了哪些图（hash_id / file_id / 过期时间 / 本地副本）
pie files gc                  # 列出可回收的本地副本（未被任何会话引用、且已放置超过 24 小时）
pie files gc --delete         # 删掉它们（服务端那份由 expires_after 自行过期，本命令不动服务端）
```

> 「超过 24 小时」是给**刚粘贴进 files/ 还没来得及 read** 的图留的保护窗口（`files.GC_PROTECT_HOURS`）：
> 那种文件还没有任何会话引用它，但路径可能正躺在输入框里，删了就是死链接。

> 隐私：图片本来就要发给服务端（内联 base64 也一样），区别是 Files API 会在服务端**留存一份** —— 所以上传时默认带 30 天过期（`files_ttl_days = 0` 则永久保留）。
> token 计费**按尺寸**算，与哪种编码无关（单图 ≤1024）；换 Files API 省的是请求体、重复传输与上限，不是钱。

## 上下文压缩

三级压缩，信息不删除只换表示：全文落盘到 `~/.pie/context/`（内容 sha256 前 16 位命名，同内容只落一份），上下文里只留“摘要 + 文件路径指针”。摘要全部为**规则式**（免费、确定、不调模型）。压缩默认关闭（`compaction = None`），在配置里写上 `[compaction]` 即开启（默认三级全开，子表调参、`xxx = false` 关闭对应级）：

- 工具级：`read` 用 `offset/limit` 分页；`shell` 不做内部截断（全文返回），超长输出由 harness 工具级压缩按行 `head+tail` 落盘指针；保护窗口 = 最近 `keep_last_steps` 个 step 批次（跨轮次滚动）verbatim，窗口外未压缩工具结果从最老开始落盘。
- 轮次级：把最老的已完成轮次压成摘要 assistant——只保留该轮次模型最终输出（`pie: ...`，用户输入由紧随其前的用户消息承担、不重复写入），中间过程用 `...[中间过程省略]...` 显式标注（附原文文件指针）。
- 会话级：旧窗口落盘为 `[历史窗口: 路径]` 规则式摘要，只保留开头 `head` 轮 + 末尾 `tail` 轮的“用户提问 + 最终回复”，中间 `...[中间省略 N 轮]...` 标注。
- 保护区域：进行中轮次（最后一个用户消息之后）不参与轮次级压缩；已完成轮次可被轮次级/会话级压缩；单条消息压缩级别只升不降（0→1→2→3）。

触发：软阈值 `soft_ratio × (context_window - reserved_tokens)`，压缩到 `target_ratio` 水位（迟滞防抖）；token 计数优先使用 API 返回的 `usage.prompt_tokens`，无上报时用字符估算（chars/4，对中文低估）。单条消息的压缩级别只升不降（0=原始 → 2=轮次级/step → 3=会话级；旧文件的 1=工具级 仍兼容）。

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



SYSTEM_PROMPT = """\
你是 pie，一个运行在用户机器上的自动化 agent，通过反复调用工具完成用户任务。

## 上下文压缩

长对话中，pie 会把旧内容压缩成「文件指针 + 摘要」，信息不会删除，只是换了一种表示。具体如下：
- 工具输出截断：`[工具输出全文已保存: 路径]`（或 `[shell 输出全文已保存: 路径]`）后只附首尾若干行，中间被省略。
- 轮次压缩：`[轮次原文已保存: 路径]` 代表一轮完整的「用户 → 思考 → 工具调用 → 结果 → 回复」，摘要只保留该轮次的模型最终输出，中间过程被省略时用 `...[中间过程省略]...` 显式标注（无中间过程则不标注）。
- 历史窗口摘要：`[历史窗口: 路径]` 后是规则式摘要，只保留最早 N 轮和最晚 M 轮的“用户提问 + 最终回复”，中间被省略的轮次用 `...[中间省略 N 轮]...` 显式标注；其中的 `pie:` 行是当时的最终回答，不是工具结果。

## 核心原则

- YOLO 模式：所有工具直接执行，无权限确认。这要求你更谨慎、更准确，避免破坏性操作。
- 先理解再动手：修改文件前，先用 read 或 shell 了解现状。
- 最小改动：小修改用 edit，整体重写才用 write。
- 用 shell 验证：写完代码就运行；状态不明就看目录、查日志、跑测试。
- 不假装完成：工具报错就读错误信息并修正；多种方法都失败时如实汇报，不编造成功。
- 当你遇到看起来上下文信息不足的提问时，先观察当前会话是否经过压缩，优先查找分析当前会话历史文件以补全上下文信息。

## 可用工具

- `read`：读取文件内容（UTF-8，不截断；二进制返回大小提示）。offset 为 1 起的起始行，limit 为最大读取行数，用于大文件分页读取。读取图片（PNG/JPEG/GIF/WebP/BMP）时返回图像引用，图片内容会作为多模态图像消息随对话发送给模型，offset/limit 不适用。
- `edit`：一次做多个精确替换。每个 edits 项的 oldText 在原文中必须唯一且互不重叠，按原文一次性应用，相邻改动请合并成一个 edit。
- `write`：写入文件，自动创建父目录，覆盖已有内容。
- `shell`：执行 shell 命令，返回 stdout/stderr 和退出码。超长输出不做内部截断，交回 harness 由工具级压缩（head+tail 落盘指针）处理。

## 工作流程

1. 拆解任务，先用 read / shell 了解现状（大文件先用 `wc -l` 或 `rg` 定位，再用 read 按行范围分段读）。
2. 按需调用工具，一次只做必要的事。
3. 每步确认结果，出错即修正。
4. 全部完成后，用简洁的中文总结：做了什么、结果如何、有无遗留问题。

## 记忆管理

你有两层记忆文件：
- **全局记忆**：`~/.pie/memory.md` — 跨项目的用户偏好、通用编码风格、工具链路径、个人习惯。
- **项目记忆**：`<项目根>/MEMORY.md` — 当前项目的架构决策、业务逻辑、技术选型、待办事项。

**当记忆冲突时，以项目记忆优先**，若冲突则在回答开头提醒：`⚠️ 记忆冲突：全局 X，项目 Y，我将遵循 Y`。

### 记忆更新

学到跨会话仍然有效的经验、用户偏好或项目决策时，用 `edit` 或 `write` 更新对应记忆文件。不要记录临时状态（如临时文件路径），只记长期事实。

| 写入内容 | 目标文件 | 触发条件 |
|---------|---------|---------|
| 跨项目的通用经验（编码风格、工具习惯、个人设置等） | `~/.pie/memory.md` | 用户显式要求记住；或从多次修正反馈中提炼出的新习惯 |
| 项目专属决策经验（架构选型、业务规则、技术栈版本等） | `<项目根>/MEMORY.md` | 完成重大功能、修复复杂 Bug、重构后；或用户说“记住这个决策” |
| 待办事项 / 路线图 | `<项目根>/MEMORY.md` | 规划新阶段、完成里程碑后更新进度 |

### 禁止写入

- 临时性报错堆栈、中间输出、调试日志
- 明文密钥或敏感凭证（应使用 `.env`）
- 已被项目记忆或全局记忆覆盖的临时性偏好
"""

GLOBAL_MEMORY_TEMPLATE = """\
# 全局记忆（~/.pie/memory.md）

跨项目的持久记忆：用户偏好、关键约定、踩过的坑。每次会话自动注入 system prompt，保持简洁。

只记跨会话仍然有效的事实。不记临时状态（临时文件路径、报错堆栈、调试日志）；密钥放 `.env`；项目专属内容写到项目根 `MEMORY.md`。

## 写入时机

- 用户显式要求记住；或从多次修正反馈中提炼出的新习惯。
- 完成重大功能、修复复杂 Bug、重构后沉淀的关键决策。

## 内容分类

- **编码风格／工具链**：跨项目的代码风格、命名习惯、工具路径、个人设置。
- **关键约定与踩坑**：容易再踩的规则，如「xxx 必须先 yyy，否则报 zzz」。

说明段落不要删除，追加内容写到对应分类下。
"""

分类与禁忌（临时报错堆栈、调试日志不记；密钥放 `.env`）见注入的 `~/.pie/memory.md` 开头说明，不在此重复。

我正在做一个手机 systemAgent 项目，主要功能是更好地操控手机完成用户指令。目前注册了 200+ 工具和 10+ skills。我现在要做 RL，但是目前感到比较难以准确判断执行轨迹是否正确。请问该如何优化？

可以给 Config 增加一个字段`tools`，表示可以对每个tool单独设置默认参数吗？类似如下：
{
    ...
    "tools": {
        "read": {"max_lines": 500, "max_bytes": 16384},
        "bash": {"max_lines": 200, "max_bytes":  8192}
    }
    ...
}

以 read 为例，max_lines / max_bytes 是 read 函数的参数。read 定义如下：
```python
def read(
    path: str,
    offset: int | None = None,
    limit: int | None = None,
    _max_lines: int | None = None,
    _max_bytes: int | None = None,
    _max_image_bytes: int | None = 32 * 1024 * 1024,
) -> str:
    """
    path, offset, limit 是模型可见的三个公开的参数。schema 里面也只写了这三个。
    _max_lines, _max_bytes 是私有参数，我们可以在配置文件里面设置。
    """
    ...
```

你觉得这样改动大吗？

把 READ_IMAGE_MAX_BYTES 也改成这种吧。
```python
async def read(
    path: str,
    offset: int | None = None,
    limit: int | None = None,
    _max_lines: int | None = None,
    _max_bytes: int | None = None,
    _max_image_bytes: int | None = 32 * 1024 * 1024,
) -> str:
    """
    对于文本：读取限制为 min(_max_lines, _max_lines_from_bytes)，其中有 f_lines[offset-1:offset-1+_max_lines_from_bytes] 尽可能接近 _max_bytes。
    对于图像：忽略 _max_lines 和 _max_bytes，只看 _max_image_bytes 。
    """
    ...

async def shell(
    command: str,
    timeout: int | None = None,
    _max_lines: int | None = None,
    _max_bytes: int | None = None,
    _on_progress: Callable[[dict[str, Any]], None] | None = None,
) -> str:
    """
    这里处理  _max_lines 和 _max_bytes 的方式同 read 。
    shell 的输入如果超出限制则只输出尾部。
    """
    ...
```

