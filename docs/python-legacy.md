# Python 版（已删除）：历史与对照

pie 最初是一套**纯 Python 实现**；2026-09-23 用户把整套代码重构为 Rust，Python 实现已从本仓库
整体删除。本文件把那一段历史与旧版的关键信息收在一处，供了解来龙去脉、查旧实现、以及看
「现行实现与旧版的逐条差异」用。**当前行为一律以 Rust 源码 + `AGENTS.md` / `MEMORY.md` 为准**
——本文件只是历史与对照，不再随代码更新（除非有人刻意回来补）。

> ⚠ 别和 [`docs/python-bindings.md`](python-bindings.md) 搞混：那份讲的是 `bindings/pie-py`
> ——Rust 核心的 **Python 原生绑定**（`import pie`，PyO3 + maturin），**仍然存在、仍在维护**；
> 本文件讲的「Python 版」是被删掉的**旧纯 Python 实现**。

## 1. 旧版是什么样

| | 旧 Python 版（已删除） | 现在的 Rust 版 |
|---|---|---|
| 入口 | `python -m pie`（装好后即 `pie`） | `pie`（Cargo bin） |
| 内置工具 | `read` / `edit` / `write` / `shell` | `read` / `edit` / **`writ`** / **`bash`**（2026-09-23 用户点名改名） |
| 模块 | `src/pie/{config,llm,tools,session,context,tui,input,…}.py` | `src/{config,llm,tools,session,context,cancel,log}.rs` + `src/tui/*` |
| LLM 层 | 官方 OpenAI SDK（`AsyncOpenAI`） | `reqwest` + 手写 SSE（**不含任何 SDK**） |
| TUI | Textual（`src/pie/tui.py`） | ratatui（`src/tui/`） |
| 测试 / 打包 | `tests/*.py`（pytest） / `pyproject.toml` + `uv.lock`（uv） | `cargo test` / `Cargo.toml` |
| 事件 | `on_event` 回调传 dict | 同一形状的 `TurnEvent`（绑定侧逐字对齐） |

两边**共用同一批磁盘资产**（迁移时有意对齐，所以旧文件现在还能读）：`~/.pie/config.toml`、
`~/.pie/sessions/*.jsonl`、`~/.pie/context/`、`~/.pie/windows/`、全局记忆 `~/.pie/memory.md`、
项目记忆 `MEMORY.md` / `AGENTS.md` / `SYSTEM.md`。配置键、记忆文件、会话格式都同名同义。

## 2. 当年怎么迁的（oracle 方法）

原则是「迁一块对一块」：把旧 Python 实现当**行为 oracle**，每迁一个模块就拿同一批输入在新旧两边
各跑一遍、逐字比对，而不是凭感觉重写。迁移已于 2026-09-23 结束，oracle 现在只存在于 git 历史里：

- 查旧实现（**需要带完整 git 历史的树**）：`git show b188058^:src/pie/<模块>.py`——`b188058` 是
  **删除 Python 代码**的那次提交，`^` 指向它之前（也就是代码还在）的状态。⚠ 手上的工作树若是
  「单提交快照」（`git rev-list --count HEAD` == 1）就没有这段历史，得回上游仓库查。
- 当时有一条「工具 schema 与 Python 逐字对拍」的**契约测试**（基准 `fixtures/python-tools.json`），
  随 `fixtures/` 一起在 2026-09-23 删除，现在只留 `builtin_tool_names` 钉住注册名与顺序
  → 工具描述/参数再分叉就没有自动拦网了。

## 3. 与旧版的有意差异（现行实现仍成立）

下面这些是当初为了 Rust 的形态**有意**与旧 Python 版不同的地方，按层分组。多数是「换实现形态」，
少数是行为上的明确取舍。

### 工具层

- **工具名 `writ` / `bash`**：旧版叫 `write` / `shell`，2026-09-23 用户点名改（详见根目录 `MEMORY.md`）。
- **统一输出协议 `Headers\n\nBody`**：headers 一行一个 `[key=value]`，body 为空时连空行都不给。
- **bash 退出码头「失败才给头，成功只给结果」**（2026-09-23 定稿，2026-09-24 并成一行）：
  - `exit != 0` → **一行**头 `[exit=N, os=linux, shell=bash]`，再接正文（有输出时空行隔开）；
  - `exit == 0` → **只有正文**（`true` → 空串），连 `[exit=0]` 都没有。
  - 判成败只看第一行（`[exit=` 开头且不是 `[exit=0…` = 失败）；旧 Python 版留下的
    `[exit=0]…` 也认成成功，所以那行**不再**是纯值，别按 `^\[exit=(\d+)\]$` 解析。
- **shell 只有一个来源**：`impl Bash` 的 `const SHELL`（`bash` / `cmd`）+ `const SHELL_FLAG`
  （`-c` / `/C`）——⚠ 旧 Python 版用 `sh -c`。
- **shell 超限只保留开头**（旧 Python 版保留**尾部**）：丢的只是可见性，全文照旧落盘成
  `[工具输出全文已保存: …]` 指针。理由：`cat 大文件` 这类命令开头才是要看的。字节口径与旧版
  一致（行含 `\n` 按实际字节；`read` 那边不含换行、每行 `+1`）。
- **`edit` 四类诊断齐**（2026-09-22）：级联引用 / 出现多次 / 重叠 / 只差空白之外，还多给
  「原文里最接近的位置 + 简版 diff」；超长回显用 `{:?}`（单行 + 转义，对齐旧版的 `!r`）。
- **`--tools` 工具集裁剪**：`--tools read,ls,grep` = read + 只允许 ls/grep 的 shell。受限 shell
  复用 Bash 的私有参数 `_allow_cmds`（不另起工具类型），description 追加一行白名单。
- **`max_steps` / `stream` 是 `aturn` 的形参**而不是配置项：`Session::aturn(input, on_event,
  cancel, max_steps, stream)`，与旧版 `loop.aturn(max_steps=…, stream=…)` 同形。

### 会话 / 上下文

- **会话文件与旧版互通**：同目录（`~/.pie/sessions/`）、同 JSONL 形状（首行 `__meta__` + 每行一条
  消息），旧版写的会话 Rust 能 resume（未知字段如 `synthetic` / `compress_level` 忽略），反之亦然。
  唯一差异是文件名：Rust 用 `chat-<unix 秒>-<微秒>.jsonl`（没有日期库），旧版用本地时间
  `%Y%m%d-%H%M%S-%f` —— 两边都按 mtime 选最新，命名不同不影响互相恢复。
- **窗口摘要在 resume 时重建**：`load` 丢掉文件里的旧 system（提示词会变）后，按
  `__meta__.windows` 用 `context::build_window_summary` 重新生成 `[历史窗口: …]` 摘要——不重建
  模型就看不到被归档的历史（旧版同款做法）。
- **JSONL 落盘走 `session::json_line`**：`serde_json` 只转义 C0，工具输出里的裸 C1
  （U+0080–U+009F）与 U+2028/U+2029 会原样落盘 —— `splitlines()` 那类读者会把 U+0085 当换行、
  一条消息被劈成两半。`escape_control_chars` 把它们换成 `\uXXXX`（读回来同字符），session 文件
  与 `/clear` 的窗口块都走它。
- **三级上下文压缩**（工具级 / 轮次级 / 会话级）：语义照旧版，但**消息模型不同**——旧版是类层级，
  Rust 是扁平 `Vec<Message>`（靠 `role` + `compress_level` + `synthetic` 判定）→ 三种压缩都是
  「就地改写消息列表」。压缩元数据字段名与旧版 `to_dict()` 逐字对齐（`compress_level` /
  `raw_path` / `raw_hash` / `raw_len` / `raw_tokens` / `synthetic`），但**绝不能进 API 请求体**
  （发模型前统一过 `Message::to_api()`）。
- **落盘时间统一 unix 秒数字**：manifest 的 `ts`、`__meta__.files[].uploaded_at` 从 ISO 串改成
  数字；旧 manifest 里的 ISO 串**不解析**，`pie context info` 原样打印。落盘原文用 `indent=1`
  的 JSON（对齐旧版 `json.dumps(..., indent=1)`）。
- **目录分工**：压缩落盘在 `~/.pie/context/`（`context gc` 的地盘），`/clear` 主动归档的窗口块在
  `~/.pie/windows/`（gc 不碰）——与旧版同一分工。

### 模型层

- **`reqwest` + 手写 SSE 取代 openai SDK**：错误是自己定义的类型，`retryable()` 不再靠
  `type(exc).__module__` 嗅探。重试语义与旧版一致（408/409/429/5xx + 传输层异常；`Retry-After`
  优先并夹在 `[1.0, 60.0]`；流式**只在没吐过增量时**才重试；400 拒 `stream_options` → 摘参数重来
  且不占重试额度），但实现收敛成**唯一驱动器 `LlmClient::with_retry` + 判定枚举
  `Retry::{Backoff, Now, Give}`**；旧版的三个 `*_once`（`complete_once` / `stream_once` /
  `list_models_once`）随之退役。
- **读到 SSE `[DONE]` 直接 break**：Rust 侧 body drop 即关连接，**没有**旧版那边的
  `generator didn't stop after athrow()` 收尾噪音，所以旧版的 `aio.py`（`close_asyncgens()`）不需要。
- **配置只保留当前形状**：不再背旧 Python 里那条旧键迁移链（`max_tokens→reserved_tokens`、
  `compress_tools→compaction` 等）。
- **取消用自建 `Cancel`**（不引 `tokio-util`）：`AtomicBool` + `Notify` 两件套，`Clone` 是共享语义；
  三层取消点（`aturn` 每步开头、模型请求 `select!` race、shell 等进程时 race + `killpg`）。收尾语义
  与旧版一致：被取消的 `tool_call` 补 `CANCEL_TEXT` 的 tool 消息、历史里写一条 `CANCEL_TEXT` 的
  assistant 消息并作为本轮答复返回。
- **回合失败也留一条 assistant**（`push_error_turn`）：模型请求出错时先往历史补一条
  `[请求失败] <错误>` 的 assistant 再抛 `Err`——否则 `push_user` 那条 user 永远没人应答（历史里连续
  两条 user）。旧版同毛病，这边补上了。

### 图片

- **只走 Files API，不回退 base64**：read 读到图 → 本地内容寻址副本（`~/.pie/files/`，0o600）→
  上传拿 `file_id` → 注入一条 `synthetic` user 消息（`{"type":"file","file_id":…}`）。旧版在
  上传失败/模型不支持时**回退内联 base64**，这边不回退：拿不到 `file_id` 就不注入图；`file_id`
  失效时也不降级 base64，而是把历史里的 `file` 块换成文本占位重试一次
  （`Session::downgrade_file_blocks`），标记失效 → 下次同图重传。
- **⚠ `expires_after` 只能用方括号展开的表单字段**发：`expires_after[anchor]=created_at` +
  `expires_after[seconds]=N`（发 JSON 串 → 响应 `expires_at` 为 `null` = 被当永久件收下，TTL 静默失效）。
  本地副本文件名与 `__meta__.files` 条目字段与旧版逐字一致（两边记录互认）。
- **图片识别改用 `image` crate**（`guess_format` + `into_dimensions`）顶掉手写的 PNG/GIF/BMP 头解析
  + JPEG SOF 扫描 + WEBP 三变体。`guess_format` 的魔数表比旧版宽（TIFF/ICO 也认得出来），所以**必须
  过格式白名单**，否则 TIFF 会被当图片而旧版把它当二进制。

### TUI

- **告警不写 stderr，走进程级出口**（`src/log.rs`）：TUI 期间终端是 raw mode + 交替屏，直接写 stderr
  的字节会落在当前光标处，而 ratatui 只重画变化格子 → 砸花的行永不恢复。所以重试提示 / 压缩统计 /
  上传告警都走 `log::warn`（`App::run` 装了出口就是消息流里一条 `· …`，没装就 `eprintln!`）。
- **消息流自己折行**：`history::layout(cells, palette, width, lean)` 自己按宽度折好，滚动偏移按
  折行后的显示行算（旧版 Textual 是「渲染后再对齐 + 对不上就回退」的启发式）。自己折的唯一动机是
  **复制**：每个 `Row` 带逻辑行号，选区文本用 `Layout::slice_text` 从源文本切（软换行不产生换行、
  宽字符不劈开）。
- **框选复制**：鼠标捕获为滚轮常开 → 终端原生选择用不了，自己做（`App::on_mouse` 把屏幕坐标 →
  「绝对显示行 + 单元格列」，松开即写剪贴板，用长活 `Copier`）。复制完清选区 + 右下角弹一条 Toast
  （`TOAST_TTL = 3s`，不占消息流——旧版就是 `App.notify`，不写 `#log`）。
- **TUI resume 回放**（`Session::full_history()`）：按顺序把压缩指针展开成完整转录。与旧版不同：
  展开工具级时**把落盘全文和原消息的头区拼回去**（`[exit=N]` 在头区、不在落盘件里；不拼的话回放里
  失败的命令会显示成 ✓）；**不**把 manifest 里「已不在消息中」的原文追加到末尾（旧版这么做，会让
  顺序错乱）。
- **`Config.theme` 认 `catppuccin` / `catppuccin-<flavor>` / 裸 flavor 名**：`catppuccin-<flavor>`
  这种带族名的写法是照顾旧版命名；认不出回 mocha 并在消息流提一句。

### 提示词 / 记忆

- **内置提示词正文在 `prompts/`（小写文件名）**：`prompts/system.md`（`include_str!` 就地嵌，与旧版
  `config.SYSTEM_PROMPT` 常量逐字一致）、`prompts/memory.md`（全局记忆种子，同旧版
  `GLOBAL_MEMORY_TEMPLATE` 逐字一致）。⚠ 与**运行时**按名找的 `SYSTEM.md` / `AGENTS.md` /
  `MEMORY.md` 不是一回事（后者从 cwd 往上找）。
- **记忆文件共用同一批路径**：`~/.pie/memory.md`（全局）+ 项目根的 `MEMORY.md` / `AGENTS.md` /
  `SYSTEM.md`；首跑写种子（已存在不覆盖），system prompt 里按 `## 全局记忆（<绝对路径>）` +
  `## 项目记忆（MEMORY.md）` 拼（对齐旧版 `build_system_prompt`，没文件就不拼那块）。

## 4. 旧格式现在还被认的地方（兼容点）

- **旧会话文件**：旧版写的 `~/.pie/sessions/*.jsonl` 能直接 resume；`[exit=0]…` 的三行旧头也认成成功。
- **旧 manifest**：`ts` 是 ISO 串的老 manifest 不解析，原样打印。
- **未知字段**：`synthetic` / `compress_level` 等旧字段读会话时忽略，不报错。
- **字段名逐字对齐**：`to_dict()` / `__meta__.files` 条目 / 压缩元数据字段名两边一致 → 记录互认。
- **主题名**：`catppuccin-<flavor>`（带族名）这种旧命名能认。
- **内存/配置文件路径**：`~/.pie/config.toml` / `~/.pie/memory.md` 完全共用。

## 5. 旧版有、Rust 版仍未做 / 有意不做

- **工具执行进度**（旧版 shell 有 `_on_progress`，TUI 能边跑边显示）：Rust 版先移除了；`Tool::call`
  已有 `ToolCtx`，加进度就在那里挂 `mpsc::Sender`。丢的只是**实时性**，输出本身仍完整返回。
- **工具 panic 文本化**（旧版把工具抛的异常兜成 `[工具异常] 类型: 消息`）：Rust 版只捕
  `Err(ToolError)`，工具里 panic 会带崩整个回合。
- **`tool_call` 事件的 `turn` / `step`**：旧版事件 dict 里有，2026-09-24 删除（仓库内没有活着的读者，
  绑定侧同步去掉）——这是对外事件形状的破坏性变更。
- **CLI 形态差异**：TTY 下带任务这边走一次性（旧版进 TUI）、没有 `-p/--print`、`-V` 大小写不同、
  `--tools` 内置名是 `read/edit/writ/bash`。
- **`setup` 交互式向导**：旧版是逐个问答模型/地址/key；这边 `pie setup` 非交互，只补缺的默认文件。
- **`tool_result` 事件不再截 500 字**：旧版在 `loop._run_tool_call` 里 `clip_output(text, 500)`，
  这边「Session 只搬真话」（少显示是展示层的事、少回传是工具自己配容量的事）。
- **TUI 主题仍缺明暗自适应 / `/theme` 热切**：无 OSC 11 探测 → 族名按深色（沿用旧版习惯）。
