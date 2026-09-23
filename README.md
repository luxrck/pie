# pie-rs

`pie` 的 Rust 重构。与 Python 版**同仓库并存**（`/mnt/d/pie-master`）：Python 版是**行为 oracle**，
每迁移一块就对着它跑同一批输入比对，而不是凭感觉重写。

## 当前进度

| 层 | 状态 | 说明 |
|---|---|---|
| `config` | ✅ | `~/.pie/config.toml` 加载（同一份配置文件）+ 分层 system prompt + 首跑写全局记忆种子（`~/.pie/memory.md`） |
| `llm` | ✅ | **reqwest + 手写 SSE**，不含任何 OpenAI SDK；自研重试（`Retry-After` 优先）；`GET /user/balance` 查余额（`fetch_balance`） |
| `tools` | ✅ | read / edit / **writ** / **bash**（工具名与 Python 有意不同，2026-09-23），统一 `Headers\n\nBody` 输出 |
| `loop` | ✅ | 问模型 → 执行工具 → 再问；工具失败文本化回传（**已并入 `session`**：回合循环现在是 `Session::aturn`） |
| `session` | ✅ | JSONL 持久化 + resume + 一回合的 agent 循环；`set_model`/`set_reasoning_effort`/`compact`/`usage_report` 等 API 齐 |
| CLI | ✅ | 一次性模式 `pie-rs "任务"`（支持 stdin）；`-m/-t/--reserved-tokens/--mode/--max-steps/--no-stream/--cwd/--stat/--tools/--system-prompt` 等覆盖项；`-r/--resume`、`-s/--session`；子命令 `setup`/`sessions`/`context`/`files` |
| `Tool` trait | ✅ | **结构体即参数**（serde + schemars 派生 schema），注册 `.with_tool::<T>("名字")` |
| context | ✅ | 三级压缩（工具/轮次/会话级）+ 落盘指针 + manifest + `context info\|verify\|gc` |
| 图片 | ✅ | read 标记 → 本地副本 → Files API 上传（`file` 块注入）；`files list\|gc [--all]`；**无 base64 回退** |
| TUI | ✅ | ratatui 重写，`src/tui/`（app / render 在 app 内 / status / input / history / markdown / theme / clipboard）：流式渲染、思考计时、滚动、`/` 命令、`Esc` 取消 |
| **Python 绑定** | 🚧 | M0–M3 + M5 已落地（PyO3 + maturin，`bindings/pie-py`）：`Config` / `LlmClient` / `ToolRegistry` / `Session` / `Cancel` / `run()` / `list_sessions()` + 事件回调 + 异常层级 + 类型存根 + **`@pie.tool` 注册 Python 工具** + **`aturn_async` / `events()`**（M4 的 wheel 分发未做）；**`import pie`**（与纯 Python 版同名，别装进同一个环境）；规划与进度见 [`docs/python-bindings.md`](docs/python-bindings.md)（**TUI 不进绑定**） |

## 怎么定义一个工具

**一个工具 = 一个结构体**：字段就是参数，doc 注释就是参数描述，必填由「非 `Option` 即必填」推导。
这些全由 `#[derive(Deserialize, JsonSchema)]`（serde + schemars）包办——**没有自己写的宏**。

```rust
/// 读取文件内容：文本按 UTF-8 全文或 offset（1 起）/ limit 分页读取。   ← 这就是 function.description
#[derive(Deserialize, JsonSchema)]
pub struct Read {
    /// Path to the file to read (relative or absolute)               ← 参数描述
    path: String,
    /// Line number to start reading from (1-indexed); ...
    offset: Option<i64>,
    /// 私有参数：配置注入，不进 schema
    #[schemars(skip)]
    _max_lines: Option<i64>,
}

impl Tool for Read {
    async fn call(self) -> ToolResult {
        let Self { path, offset, .. } = self;   // 实现直接写在 call 里，没有额外的 `_impl` 一层
        // …
    }
}
```

协议只有 4 行：

```rust
pub trait Tool: DeserializeOwned + JsonSchema + Send + Sync + 'static {
    fn call(self) -> impl Future<Output = ToolResult> + Send;
}
```

只有两处**不能省**，都是编译器定的：

| 为什么 | 说明 |
|---|---|
| `-> impl Future + Send` 而不是 `async fn` | trait 里的 `async fn` **表达不出 `Send`**，而注册表要把工具 future 装箱成 `dyn Future + Send`。**impl 里仍写 `async fn`**，`Send` 只在 trait 声明一次 |
| 注册传**类型**：`.with_tool::<Read>("read")` | 「结构体即参数」⇒ 带字段的结构体在 Rust 里不是值表达式（`Read` 这三个字写不出来） |

与 Python 版一一对应：

| 行为 | Python `@tool` | Rust |
|---|---|---|
| 描述 | docstring 首行 | 结构体 doc 首行 |
| 参数 schema | 类型注解推导 / `parameters=` | `#[derive(JsonSchema)]` |
| 哪些必填 | 「无默认值即必填」 | **非 `Option<T>` 即必填** |
| 私有参数 | `_` 开头 → 不进 schema | 同左（`#[schemars(skip)]`） |
| 注册 | `register_builtins` 显式列一遍 | `ToolRegistry::new()` 里显式列一遍 |

加一个工具 = 写一个结构体 + `impl Tool` + 在 `ToolRegistry::new()` 里加一行 `.with_tool::<T>("名字")`。

**实现自包含**：只服务单个工具的辅助逻辑（图片嗅探、字节预算、头部截断、全文落盘、命令构造、edit 诊断）
都就地写在 `impl Tool for X` 的 `call` 里，不往模块级抛小函数——读一个工具不用在文件里跳来跳去。模块级只留
`format_output` 这一件**被所有工具共用**的小事，其余是 trait / schema / 注册表基建。字节预算两边口径**本就
不同**（Python 也如此）：`read` 的行不含换行 → 每行 `len+1`；`bash` 的行含 `\n` → 按实际字节（各自就地实现）。

分发用泛型函数单态化成**函数指针**（**不经过 `dyn Tool`**，所以也不碰 object safety）。
`call` 没有借用参数（参数是 `Value`、`self` 按值传），所以 future 是 `'static`：

```rust
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

fn erased<T: Tool>(args: Value) -> BoxFuture<ToolResult> {
    Box::pin(async move {
        let args: T = serde_json::from_value(args)
            .map_err(|e| ToolError(format!("参数解析失败: {e}")))?;
        args.call().await
    })
}
```

### schema 的 4 个坑（都实测过）

1. **结构体 doc 首行 = 工具描述**，多行说明必须写成普通注释——schemars 会把 doc 里的**单换行合并成空格**。
   真要多行描述得用 `#[schemars(description = "...")]` 显式给（`edit` 就是）。
2. `#[schemars(...)]` 必须写在 `#[derive(JsonSchema)]` **之后**（derive helper 属性的顺序，新版是硬错误）。
3. 嵌套结构体（`edits` 的元素）**不能加 doc 注释**，否则 `items` 会多出一个 Python 版没有的 description。
4. 归一化四件事：去 `$schema` / `title` / `format`，`Option<T>` 不要 `["integer","null"]`，
   嵌套必须 `inline_subschemas = true`（否则出 `$ref` + `definitions`）。

### 工具名

`builtin_tool_names`：注册名与顺序固定为 `read` / `edit` / `writ` / `bash`（后两个是用户点名改的）。
原先还有一条与 Python 版逐字对拍的**契约测试**（基准 `fixtures/python-tools.json`），已于 2026-09-23
连同 `fixtures/` 一起移除。

## 构建与运行

```bash
export PATH="$HOME/.cargo/bin:$PATH"          # 本机 cargo 不在默认 PATH
export CARGO_TARGET_DIR=$HOME/.cache/pie-rs-target   # 源码在 /mnt/d(9p)，构建产物放到 Linux 原生盘
cargo build
cargo test
cargo run -- --models
cargo run -- "用 shell 跑 date，把结果写进 /tmp/x.txt 再 read 验证"
cargo run -- -s demo "记住：我在改 pie-rs"      # 新建/载入会话，跑完落盘
cargo run -- -s demo "接着上面那件事"          # 载入同一会话继续（历史可恢复）
cargo run -- -r "最近那条会话继续"               # 恢复最近的（同工作目录优先）
cargo run -- --tools read,ls,grep "看看目录"      # 限制工具：read + 只允许 ls/grep 的 shell
cargo run -- -m deepseek-v4-pro -t high "任务"   # 一次性覆盖模型 / 思考深度（不写回配置）
cargo run -- --reserved-tokens 64k --stat "任务"  # 覆盖输出预留；/stat 报告走 stderr，stdout 只留答案
cargo run -- setup                              # 补齐默认配置文件 + 全局记忆（缺什么写什么，已存在不动）
cargo run -- sessions -l 5                       # 列出最近 5 个会话
cargo run -- context info                     # 压缩事件 / 死链校验 / 垃圾回收
cargo run -- context verify                   # 校验 manifest 引用的原文还在
cargo run -- context gc --delete              # 列出并删除 context/ 里没人引用的文件
cargo run -- files list                       # 各会话记的图片（副本 / file_id / 过期）
cargo run -- files list --all                 # 云端本账号的全部上传件（只读）
cargo run -- files gc --delete                # 回收本地副本（未被引用且过了 24h 保护窗）
cargo run -- --stat "任务"                     # 跑完把 /stat 报告打到 stderr（上下文占用 / 水位 / 压缩事件 / API 用量）
cargo run -- --mode json "任务"                 # 一次性模式的输出：text（默认）/ json / transcript
cargo run -- --mode transcript "任务"           # 完整历史（含压缩原文展开）的 JSON，缩进 1
cargo run                                    # 无任务 → 进 TUI（真实终端）
cargo run -- -r                               # 进 TUI 并恢复最近会话
```

TUI 按键：`Enter` 发送、`Shift+Enter`/`Ctrl+J` 换行（输入框是多行编辑器，**长行软换行**，内容超过 9 行才有滚动）、
输入框上边框**常亮强调色**（聚焦高亮，默认 3 行高；以 `!` 开头时转工具色 = 直接执行 shell），
**键位提示就显示在输入框的 placeholder 上**（空输入时可见：`⏎ 发送 · ⇧⏎ 换行 · …`，
回合进行中换成 `Esc 停止 …`）——不再单独占一行底栏，省下的那行给了消息流、输入框直接贴屏幕底；
代价是有字时看不到提示（`/help` 有全表）。
选中文本用 `Shift+方向键` / `Ctrl+A`，或**鼠标在输入框里拖**（accent 底 + 深色字），
鼠标**左键拖动框选消息流，松开即复制**到剪贴板（`Esc` 先把高亮收掉）、滚轮滚动，
复制完**右下角浮一条** `已复制 N 字符到剪贴板`（`TOAST_TTL = 3s` 后自消，不占消息流）
`Esc` 停止本回合（退出时会先取消再落盘）、`PgUp`/`PgDn` 滚动、`/` 开头的命令（`/clear` = 把当前窗口
归档到 `~/.pie/windows/` 后开新窗口，历史仍可经指针回查）、
**`@` 开文件路径补全**（候选面板在输入框上方：以 cwd 为根扫一遍、按 `.gitignore` 排除，
`@app` 这种按文件名模糊找、`@src/tu` 这种按目录前缀找；`Tab` 接受、`↑/↓` 选、`Esc` 收起；
接受时 `@` 一起吃掉，插进去的是干净相对路径；**Tab 掉目录后面板不倒**，接着列这个目录的下一层、
可以逐层钻（`src/` → `src/tui/` → `src/tui/app.rs`）；回车只在候选是**文件**时先补全再发，
目录候选不动——免得正文里的 `@词` 被改成 `词/`）、
`Ctrl+G` 粘贴（文本 / 图片路径）、`Ctrl+C` 退出。回合进行中界面**流式**刷新：
思考中的耗时显示在助手块标题行，正文增量追加，工具调用/结果按状态着色（`✓`/`✗`/`⏹`）。
多行粘贴靠**bracketed paste**（`tui::run` 里自己开 `EnableBracketedPaste`——`ratatui::init()` 不开）：
不开的话终端不会用 `\x1b[200~` 包住粘贴内容，多行文本会被拆成一个个按键（换行 = 回车）→ 在第一行就发出去。

状态栏（最下方那一行）**左边**是常驻信息堆在一起的 `<模型> · <思考深度> · <目录> │ 上下文用量 │ 余额`
（不带 `pie-rs` 前缀；窗口窄就从右往左截名字，`/thinking` 改了立刻变）；**右边**只有**活动指示**
（转圈 + 计时）贴屏幕右边缘——它一直变，单独占一位，不会把左边的用量/余额推来推去。
余额来自 `GET /user/balance`（启动时查一次、每回合结束后自动刷新，`/balance` 看明细）；
拿不到（非 DeepSeek 端点没这个接口）就不显示这一块，也不会每回合白试。

依赖要求：`build-essential`（`cc` + `crt1.o`）。**不需要** `libssl-dev` / `pkg-config` / `cmake`。

`image` crate（只为图片识别）用 `default-features = false` + `png/jpeg/gif/webp/bmp`：
23 个纯 Rust crate、**零 C 编译**、冷构建 +7.5s。将来 `clipboard` 模块还靠它把 raw RGBA 编码成 PNG
（Python 那边是 Pillow 干的）。

## Python 绑定（`import pie`）

把核心层（config / llm / tools / session / context）以**原生扩展**的形式给 Python 用；**TUI 不进绑定**。
规划稿（含「为什么这样设计」与不要踩的坑）见 [`docs/python-bindings.md`](docs/python-bindings.md)。

```bash
cd pie-rs/bindings/pie-py
export PATH="$HOME/.cargo/bin:$PATH"                       # maturin 要调 cargo
uv venv --python 3.12 .venv && uv pip install --python .venv/bin/python maturin pytest
VIRTUAL_ENV=$PWD/.venv .venv/bin/maturin develop          # 构建 + 装进 .venv（editable）
.venv/bin/python -m pytest tests -q                        # 不联网：本地假 SSE 端点回放
```

```python
import pie

cfg = pie.Config.load()                          # ~/.pie/config.toml
cfg.model = "deepseek-flash"                        # 只改内存，不写盘
llm = pie.LlmClient(cfg)
tools = pie.ToolRegistry.builtins(cfg)
session = pie.Session.ephemeral(cfg, llm, tools)  # 不落盘；要留档就用 Session.new(...) + save()

answer = session.aturn("看看当前目录", on_event=lambda ev: print(ev["type"]))
print(answer)
```

已实现（M1）：`Config`（load/save + 常用字段 + `context_budget`）、`LlmClient`（`list_models`）、
`ToolRegistry`（`builtins` / `from_spec`）、`Session`（`new` / `ephemeral` / `load` / `resume` / `aturn` /
`save` / `messages` / `full_history` / `usage` / `usage_report` / `compact` / `compression_history` /
`reset` / `stop`）、`Cancel`、异常层级（`PieError` → `ConfigError` / `LlmError`（带 `.status`）/ `ToolError`）。
还没做：从 Python 注册工具（M3）、`run()` 一次性入口、原生 `await`（M5）、abi3 wheel + 类型存根（M4）。

三条约定（错了会挂或者不生效，理由在规划稿里）：

- **同步外观**：`aturn` 阻塞到回合结束，但**期间释放 GIL**（别的 Python 线程照常跑）；要并发就用
  `asyncio.to_thread`。
- **一个 Session 同时只跑一个回合**：回合进行中再调 `aturn` / 读属性会抛 `RuntimeError("session 正忙")`
  —— **事件回调里不要碰同一个 Session**（想中途停：别的线程调 `s.stop()` 或传入 `Cancel`）。
- **`messages` / 事件都是 dict**：字段名与 JSONL / Python 版 `to_dict()` 一致 → 两边写下的会话可以互读。

## 与 Python 版的已知差异

按「有意为之」与「还没做」分开记，避免后面误判。

**有意为之**
- **内置提示词正文在 `prompts/`（小写文件名）**：`prompts/system.md`（`build_system_prompt` 里
  `include_str!` 就地嵌，与 Python 版 `config.SYSTEM_PROMPT` 常量逐字一致）、
  `prompts/memory.md`（`ensure_global_memory` 的种子，同上）。
  两个都用 `include_str!` 编译期嵌入。⚠ 与**运行时找的文件名不是一回事**：运行时仍是 `SYSTEM_FILE = "SYSTEM.md"`
  / `AGENTS.md` / `MEMORY.md`（从 cwd 往上找）——仓库根没有 `SYSTEM.md`，所以实际走的就是内置的这份。
- **记忆文件与 Python 共用同一批路径**：`~/.pie/memory.md`（全局）+ 项目根的 `MEMORY.md`/`AGENTS.md`/`SYSTEM.md`。
  首跑写种子（`config::ensure_global_memory()`，正文在 `prompts/memory.md`、`include_str!` 编进来，
  与 Python 的 `GLOBAL_MEMORY_TEMPLATE` **逐字一致**；已存在则**不覆盖**），之后由 agent 自己用
  `edit`/`writ` 维护；system prompt 里按 `## 全局记忆（<绝对路径>）` + `## 项目记忆（MEMORY.md）` 拼
  （对齐 Python `build_system_prompt`，没文件就不拼那块）。
- `reqwest` + 手写 SSE 取代 openai SDK：错误是自己定义的类型，`retryable()` 不再靠 `type(exc).__module__` 嗅探；
  重试语义与 Python 版一致（408/409/429/5xx + 传输层异常；`Retry-After` 优先并夹在 `[1.0, 60.0]`；
  流式**只在没吐过增量时**才重试；400 拒 `stream_options` → 摘参数重来一次且不占重试额度）。
  实现上重试收敛成**唯一驱动器 `LlmClient::with_retry` + 判定枚举 `Retry`**（`Backoff` / `Now` / `Give`）：
  `complete` / `list_models` 用默认判定 `default_retry`，`stream` 那两条特例写在它自己的 `decide` 里
  （Python 版同理：`_retry` 只包非流式，流式单独一段）。三个 `*_once`（`complete_once` / `stream_once` /
  `list_models_once`）随之退役——一次尝试的完整流程就在闭包里（对应 Python 把请求写在 lambda 里）。
- **取消用自建 `Cancel`（`src/cancel.rs`）**，不引 `tokio-util`：`AtomicBool` + `Notify` 两件套，
  `Clone` 是共享语义。三层的取消点：`aturn` 每步开头查一次、模型请求用 `tokio::select!` race、
  shell 在等进程时 race 并 `killpg` 整个进程组（与超时同款）。收尾语义与 Python 一致：
  被取消的 `tool_call` 补 `CANCEL_TEXT` 的 tool 消息、历史里写一条 `CANCEL_TEXT` 的 assistant 消息、
  把它作为本轮答复返回。Python 用 `asyncio.Event` + `_wait_cancellable`，语义等价。
- 历史里读到 SSE `[DONE]` 直接 break：Rust 侧 body drop 即关连接，**没有** Python 那边的
  `generator didn't stop after athrow()` 收尾噪音，所以 `aio.py` 那套 `close_asyncgens()` 不需要。
- 配置只保留**当前**形状 + 少量容错，不再背 Python 里那条旧键迁移链（`max_tokens→reserved_tokens`、
  `compress_tools→compaction` 等）。
- **会话文件与 Python 版互通**（`session.rs`）：同一目录（`~/.pie/sessions/`）、同一 JSONL 形状
  （首行 `__meta__` + 每行一条消息）——Python 写的会话 Rust 能 resume（未知字段如 `synthetic` /
  `compress_level` 忽略，历史里的旧 system 丢弃后按当前提示词重建），反之亦然。唯一差异是文件名：
  Rust 用 `chat-<unix 秒>-<微秒>.jsonl`（没有日期库），Python 用本地时间 `%Y%m%d-%H%M%S-%f`——
  两边都按 mtime 排序选最新，命名不同不影响互相恢复。
  **窗口摘要在 resume 时重建**：`load` 丢掉文件里的旧 system（提示词会变）后，按 `__meta__.windows`
  用 `context::build_window_summary` 重新生成 `[历史窗口: …]` 摘要——不重建的话 resume 后模型就
  看不到被归档的历史了（Python 同款做法）。
- **上下文三级压缩**（`context.rs`，2026-09-22）：语义照 Python——工具级（head/tail 预览 + `[工具输出全文已保存: …]`
  指针）、轮次级（user 保留 + 摘要 assistant，只留最终输出）、会话级（历史落盘成窗口块 + `[历史窗口: …]`
  摘要 system 消息）；压缩级别只升不降、内容 hash 寻址、软阈值 / 目标水位迟滞、`keep_last_steps`
  保护最近 N 个 step 批次。**消息模型不同**：Python 是类层级，Rust 是扁平 `Vec<Message>`（靠 `role` +
  `compress_level` + `synthetic` 判定）→ 三种压缩都是「就地改写消息列表」。
  压缩元数据字段名与 Python `to_dict()` 逐字对齐（`compress_level` / `raw_path` / `raw_hash` / `raw_len` /
  `raw_tokens` / `synthetic`），但**绝不能进 API 请求体**——发给模型前统一过 `Message::to_api()`
  （与 Python 同规则：tool 的 content 兜空串、assistant 带 tool_calls 必须带 `reasoning_content`）。
  另外：manifest 的 `ts` 是 **unix 秒（数字）**（落盘时间统一数字、不带时区口径）、落盘原文用 `indent=1` 的 JSON（对齐 Python
  `json.dumps(..., indent=1)`）、**自动**会话级压缩的窗口块落在 `~/.pie/context/session-*.txt`
  （而 `/clear` 主动归档的块落在 `~/.pie/windows/`，与 Python 同一分工：`context gc` 只扫前者）。
- **bash 的退出码头**（2026-09-23 用户定稿）：**失败才给头，成功只给结果**。
  - `exit != 0` → 三行头 `[exit=N]` / `[os=linux]` / `[shell=bash]`（`os` = `std::env::consts::OS`，
    `shell` = `impl Bash` 的 `const SHELL`），再接正文（有输出时用空行隔开）；
  - `exit == 0` → **只有正文**（`echo aaa` → `aaa\n`；`true` → 空串），连 `[exit=0]` 都没有。
  判成败只看第一行：Rust TUI `history::tool_result_ok`、Python 版 TUI 的 `text[6:].split("]", 1)[0]`，
  所以 `[exit=N]` 必须是**第一行的纯值**（没头 = 成功）。`format_output` 遇空头区直接返回正文，
  不会多出空行；成功且无输出的命令结果是空串——本部署端点实测收（HTTP 200）。
  `[exit_hint=…]`（退出码含义表）试过又按用户要求删了：没有「退出码 → 说明」的标准 API，编出来只是猜。
- **shell 只有一个来源**：`impl Bash` 的 `const SHELL`（`bash` / `cmd`）+ `const SHELL_FLAG`（`-c` / `/C`），
  `call` 起进程读它们（两个平台分支因此合成一段）——⚠ 与 Python 版的 `sh -c` 不同。
- **shell 超限只保留开头**（Python 版保留**尾部**）。全文照旧落盘成
  `[工具输出全文已保存: …]` 指针——丢的只是可见性，不是信息。理由：`cat 大文件` 这类命令
  开头才是要看的那部分。字节口径与 Python `_tail_output` 一致（行含 `\n` 按实际字节；`read` 那边是不含换行 +1）。
- **`edit` 的四类诊断已齐**（2026-09-22）：找不到时除「级联引用」「只差空白」外，还给「原文里最接近的位置 + 简版 diff」
  （锚行 = oldText 最长行、字符级 LCS 当相似度、窗口与 oldText 等长逐行对齐）——自写，不引 difflib 替代依赖；
  超长回显也改成 `{:?}`（单行 + 转义，与 Python 的 `!r` 对齐）。
- **`max_steps` / `stream` 是 `aturn` 的形参**（2026-09-23 改回）：它们曾在 `Config` 里放了一阵（`Config.max_steps` / `Config.stream`），现在**退回形参**——`Session::aturn(input, on_event, cancel, max_steps, stream)`，与 Python `loop.aturn(max_steps=…, stream=…)` 同形（`None` = 不限 / 跟随后端能力）；
  嵌入方（CLI `--max-steps` / `--no-stream`、TUI、绑定 `aturn(max_steps=…, stream=…)`）按次传。
  连带：`Config` 删了这两个字段（`to_toml` / 往返测试同步）；main.rs 把 CLI 覆盖项直接传给每个回合，
  TUI 经 `tui::run(session, max_steps, stream)` → `App` 带下来（否则 `--no-stream` 在 TUI 里会被静默丢弃）。
  `TurnEvent::Answer` 的规则不变：**只在非流式时推**（流式下正文已经走增量，再推会重复）。
- **告警不写 stderr，走进程级出口**（`src/log.rs`，2026-09-23）：TUI 期间终端是 raw mode +
  alternate screen，**直接写 stderr 的字节会落在当前光标处**，而 ratatui 每帧只重写有变化的单元格
  （双缓冲 diff）→ 砸花的那几行再也不会被重画（表现：屏幕残留旧文本、最新输出看不见）。
  所以重试提示 / 压缩统计 / 上传告警 / manifest 失败都走 `log::warn`：`App::run` 装了出口就变成
  消息流里的一条 `· …` 提示，没装（一次性模式、测试）就照旧 `eprintln!`。退出时的保存提示由
  `App::run` 返回、`tui::run` 在 `restore()` **之后**打印。
- **滚动偏移按「折行后的显示行」算，折行自己算**（2026-09-23）：`Paragraph::scroll` 跳过的是 wrap 之后
  的行，早前拿逻辑行数算偏移 → 长行/中文一折行就把底部内容顶出视口。现在 `history::layout(cells, palette,
  width, lean)` **自己按宽度折好**（词级优先，空白留在上一行行尾 → 一个字符不丢），显示行数就是
  `rows.len()`（所以不再需要 ratatui 的 `unstable-rendered-line-info`）。自己折的唯一动机是**复制**：
  每个 `Row` 带着「逻辑行号」，选区文本用 `Layout::slice_text` 从**源文本**切 —— 软换行的长行复制成
  一行、不同逻辑行之间才换行（对齐 Python `SelectableRichLog` 的目标，但不用它那套「渲染后再对齐 +
  对不上就回退」的启发式：这里知道每一行是怎么折的）。
- **消息流框选复制**（2026-09-23）：鼠标捕获开着（滚轮要用）→ 终端原生选择用不了，所以自己做：
  `App::on_mouse` 把屏幕坐标 → 「绝对显示行 + 单元格列」（拖动时贴边、区域外按下不算选区），
  高亮在段落渲染**之后直接改 buf 的单元格**（纯几何，不关心怎么折行的），**松开即写剪贴板**
  （`clipboard::Copier`，与 Python 同款：复制完清选区 + 右下角弹一条 Toast），
  **Toast 不占消息流**（Python 版就是 `App.notify`，不写 `#log`）：`App::toast` 记 `(文案, 截止时刻)`，
  `paint_toast` 在**所有控件之后**用 `Clear` 擦掉底下的内容再画一个 `Block::bordered()` 小盒子，
  贴屏幕右侧、输入框上沿（`above = input.y`），`TOAST_TTL`（3s）到点自消——主循环本来 66ms 一 tick，
  所以最多差一帧。失败时边框换错误色。
  `Esc` 先收选区——防鼠标在窗口外松开丢掉 Up 事件导致高亮赖着不走）。选区分行由逻辑行号决定
  （`Layout::slice_text`），宽字符按「起点落在选区里就整个要」裁（不劈汉字，口径同 Python `_cell_to_char`）。
- **`--tools` 工具集裁剪**（2026-09-22，对齐 Python `tools_from_spec` / `_restricted_shell_tool`）：
  `--tools read,ls,grep` = 只给 read + 只允许 ls/grep 的 shell。受限 shell **不另起一个工具类型**，而是复用
  `Shell` 的私有参数 `_allow_cmds`（`#[schemars(skip)]`，经 defaults 注入路径塞入白名单，schema 与全量版
  逐字一致、契约测试不受影响），description 追加一行白名单（模型事先知道边界、不瞎试）。
- **图片走 Files API，且只走 Files API**（2026-09-22）：read 读到图 → 落本地内容寻址副本（`~/.pie/files/`，0o600）
  → 上传拿 `file_id` → 注入一条 `synthetic` 的 user 消息（`parts = [文本说明, {"type":"file","file_id":…}]`）。
  **分层**：协议在 `LlmClient`（`upload_file`/`list_files`/`delete_file` + `FileObject` + 模型能力/ key 指纹），
  本地文件管理（副本、`__meta__.files` 记录、GC 清单、`files list|gc` 的数据源）在 `session.rs`；
  时间统一在 `config.rs`：只有一个时钟出口 `config::now() -> Duration`（秒 / `subsec_micros` / `subsec_nanos` 都从它取），
  其余都是纯函数（`fmt_local` 展示、`civil` 是 Hinnant 的 civil_from_days）；**落盘一律 unix 秒数字**，不写 ISO。
  与 Python 的**有意差异**：那边上传失败/模型不支持时**回退内联 base64**，这边不回退 —— 拿不到 `file_id`
  就不注入图片（read 的标记文本仍在工具结果里）；`file_id` 失效时也不降级 base64，而是把历史里的
  `file` 块换成文本占位重试一次（`Session::downgrade_file_blocks`），记录标失效 → 下次同图重传。
  ⚠ `expires_after` **只能用方括号展开的表单字段**发：`expires_after[anchor]=created_at` +
  `expires_after[seconds]=N`（服务端认得这种；实测把 JSON 串当单个字段发（无论 part 是否是
  `application/json`）响应里 `expires_at` 都是 `null` = 被当永久件收下）。本文件命名 /`__meta__.files`
  条目字段与 Python 版逐字一致，两边的记录能互相认（`files list --all` 会用本地记录标出会话）。
- **TUI resume 回放**（2026-09-23）：`Session::full_history()` 按顺序把压缩指针展开成完整转录
  （工具级 → 落盘全文，轮次级 / 会话级 → 原文消息序列；落盘文件没了就退回压缩形式），
  `tui::history::cells_from_history()` 再把它转成消息流单元格（system 不显示、注入的图片消息不算用户
  输入、`tool_calls` 与结果按 `tool_call_id` 配对成一条工具行）。两处与 Python 不同：展开工具级时
  **把落盘全文和原消息的头区拼回去**（`[exit=N]` 在头区、不在落盘件里；不拼的话回放里失败的命令会
  显示成 ✓ 且不带正文）；**不**照 Python 把 manifest 里「已不在消息中」的原文追加到末尾（那些原文
  要么仍在消息里、要么属于已归档的窗口块，重复追加只会让顺序错乱）。
- **图片识别改用 `image` crate**（`guess_format` + `into_dimensions`）顶掉手写的 PNG/GIF/BMP 头解析 +
  JPEG SOF 扫描 + WEBP 三变体。先用 Pillow 生成真图对照过：结果与手写版**逐字一致**，含 TIFF/ICO/WAV
  这些反例。注意 `guess_format` 的魔数表比 Python 宽（TIFF/ICO 也认得出来），所以**必须过格式白名单**，
  否则 TIFF 会被当图片而 Python 把它当二进制。

**还没做（TODO，按优先级）**
- **一批工具并发/串行**（2026-09-23 接上）：`parallel_tools` 已接线，实现在 **`Session::tool_call`**
  （`model_call` 的姊妹：那边问模型，这边跑工具）——整批一起跑（并发度 = `calls.len()`）或
  按模型返回顺序串行（并发度 = 1，`buffer_unordered(limit)` 只决定「同时 poll 几个」）。
  两条路的**事件形状一致**：先把整批 `tool_call` 发出去，再按「谁先跑完谁先发」推 `tool_result`
  （被取消的推 `CANCEL_TEXT`）；消息**按调用顺序**回填（历史扁平序列与串行一致 → compaction 的
  step 批次认定不受影响）；返回 `Vec<Option<String>>`（`None` = 被取消，调用方补 `CANCEL_TEXT`）。
  ⚠️ `on_event` 不能进 future（`&mut dyn FnMut` 没实现 `Sync` → `aturn` 的 future 会不再是 `Send`，
  TUI 的 `tokio::spawn` 编不过）：future 只算结果，事件由轮询循环在完成当下推。
  ⚠️ future 必须在 `for` 里造，**不能**写成 `map(|(i, c)| async move {…})`：闭包参数的生命周期
  会变成 HRTB，撞上 rustc 已知限制（#100013），报错却在调用方（`tokio::spawn`）一头雾水。
  ⚠️ **有意不同于 Python**：`tool_result` 事件的 `text` **不再截 500 字**（Python 在
  `loop._run_tool_call` 里 `clip_output(text, 500)`）——Session 只搬真话，少显示是展示层的事
  （TUI 按 `TOOL_BODY_LINES` 截、CLI 只取首行）、少回传是工具自己配容量上限的事。
- **工具执行进度**：Python 版 shell 有 `_on_progress`，TUI 能边跑边显示；Rust 版**先移除了**，
  但现在 `Tool::call` 已经有 `ToolCtx`（取消信号就走它）→ 加进度时在同一处挂 `mpsc::Sender` 即可。
  注意丢的只是**实时性**：输出本身仍然完整地作为工具结果返回。
- **工具 panic 文本化**：Python 版把工具抛的异常兜成 `[工具异常] 类型: 消息`，Rust 这边只捕
  `Err(ToolError)` —— 工具里 panic 会带崩整个回合。
- TUI 细化：代码块语法高亮。（`!cmd` 手动 shell、resume
  历史回放、消息流框选复制、**输入框内鼠标拖选**、**超宽 Markdown 表格按宽度重排**、
  **重试进度单个块**已于 2026-09-23 接上。）

## 环境备忘（本机特有）

- 公司代理会用自签 MITM CA 拦 crates.io → `~/.cargo/config.toml` 配了
  `http.cainfo = ~/.cargo/certs/bundle.pem`（系统 CA + 代理链）与 `http.proxy`。
- `api.deepseek.com` **没有**被 MITM（真实证书），程序运行时用系统根证书即可 —— 所以
  `rustls-tls-native-roots` 够用；若将来某端点也被网关拦，需把网关 CA 装进系统库或走 `SSL_CERT_FILE`。
- 本机 nightly（1.100.0）的格式串**不接受 `f` 类型**：`{x:.1f}` 会报 `unknown format trait f`，
  用 `{x:.1}` 代替。
