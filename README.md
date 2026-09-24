# pie-rs

`pie` 是一个极简的 agent harness，**纯 Rust 实现**。它最初是一套纯 Python 实现（`python -m pie`），
2026-09-23 整体重构为 Rust、Python 代码已从本仓库删除；那一段历史、当年的 oracle 迁移方式，
以及**现行实现与旧版的逐条差异**，都收在 [`docs/python-legacy.md`](docs/python-legacy.md)。

## 当前进度

| 层 | 状态 | 说明 |
|---|---|---|
| `config` | ✅ | `~/.pie/config.toml` 加载（同一份配置文件）+ 分层 system prompt + 首跑写全局记忆种子（`~/.pie/memory.md`） |
| `llm` | ✅ | **reqwest + 手写 SSE**，不含任何 OpenAI SDK；自研重试（`Retry-After` 优先）；`GET /user/balance` 查余额（`fetch_balance`） |
| `tools` | ✅ | read / edit / **writ** / **bash**（后两个名字 2026-09-23 用户点名改，旧 Python 版叫 `write`/`shell`），统一 `Headers\n\nBody` 输出 |
| `loop` | ✅ | 问模型 → 执行工具 → 再问；工具失败文本化回传（**已并入 `session`**：回合循环现在是 `Session::aturn`） |
| `session` | ✅ | JSONL 持久化 + resume + 一回合的 agent 循环；`set_model`/`set_reasoning_effort`/`compact`/`usage_report` 等 API 齐 |
| CLI | ✅ | 一次性模式 `pie-rs "任务"`（支持 stdin）；`-m/-t/--reserved-tokens/--mode/--max-steps/--no-stream/--cwd/--stat/--tools/--system-prompt` 等覆盖项；`-r/--resume`、`-s/--session`；子命令 `setup`/`sessions`/`context`/`files` |
| `Tool` trait | ✅ | **结构体即参数**（serde + schemars 派生 schema），注册 `.with_tool::<T>("名字")` |
| context | ✅ | 三级压缩（工具/轮次/会话级）+ 落盘指针 + manifest + `context info\|verify\|gc` |
| 图片 | ✅ | read 标记 → 本地副本 → Files API 上传（`file` 块注入）；`files list\|gc [--all]`；**无 base64 回退** |
| TUI | ✅ | ratatui 重写，`src/tui/`（app / render 在 app 内 / status / input / history / markdown / theme / clipboard）：流式渲染、思考计时、滚动、`/` 命令、`Esc` 取消 |
| **Python 绑定** | 🚧 | M0–M3 + M5 已落地（PyO3 + maturin，`bindings/pie-py`）：`Config` / `LlmClient` / `ToolRegistry` / `Session` / `Cancel` / `run()` / `list_sessions()` + 事件回调 + 异常层级 + 类型存根 + **`@pie.tool` 注册 Python 工具** + **`aturn_async` / `events()`**（M4 的 wheel 分发未做）；**`import pie`**；规划与进度见 [`docs/python-bindings.md`](docs/python-bindings.md)（**TUI 不进绑定**） |

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

工具定义与旧 Python 版 `@tool` 的对应（历史与差异见 [`docs/python-legacy.md`](docs/python-legacy.md)）：

| 行为 | 旧 Python `@tool` | Rust |
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
不同**（旧 Python 版也如此）：`read` 的行不含换行 → 每行 `len+1`；`bash` 的行含 `\n` → 按实际字节（各自就地实现）。

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
3. 嵌套结构体（`edits` 的元素）**不能加 doc 注释**，否则 `items` 会多出一个旧 Python 版没有的 description。
4. 归一化四件事：去 `$schema` / `title` / `format`，`Option<T>` 不要 `["integer","null"]`，
   嵌套必须 `inline_subschemas = true`（否则出 `$ref` + `definitions`）。

### 工具名

`builtin_tool_names`：注册名与顺序固定为 `read` / `edit` / `writ` / `bash`（后两个是用户点名改的）。
原先还有一条与旧 Python 版逐字对拍的**契约测试**（基准 `fixtures/python-tools.json`），已于 2026-09-23
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
输入框上边框与光标**一起跟随窗口焦点**：聚焦 = 边框强调色 + 光标强调色块（**空输入也是亮的**），
失焦 = 两者都降成 muted 暗色（以 `!` 开头时边框转工具色 = 直接执行 shell；失焦仍按 muted 显示），
**状态栏也跟着一起变灰**（名字与 spinner/耗时换成 muted——窗口在不在前台，一眼就看得出），
**键位提示就显示在输入框的 placeholder 上**（空输入时可见：`⏎ 发送 · ⇧⏎ 换行 · …`，
回合进行中换成 `Esc 停止 …`）——不再单独占一行底栏，省下的那行给了消息流、输入框直接贴屏幕底；
代价是有字时看不到提示（`/help` 有全表）。
**宽字符不留残影**：ratatui 的 diff 不会重画宽字形后面那格（在它模型里就是个普通空格，与真空格全等）
→ 删字后会留下“半个汉字”/ 底色方块（placeholder 里的 `⏎`/`⇧` 同样会留碎片）。`Input::render` 每帧用
小控件 `StaleTail` 把「已写区间右边、上一帧写过的那一段」标 `AlwaysUpdate` 强制重画（只在行变短那帧；
正文里那格不碰——写了真终端会把汉字擦掉半个）。
选中文本用 `Shift+方向键` / `Ctrl+A`，或**鼠标在输入框里拖**（accent 底 + 深色字），
鼠标**左键拖动框选消息流，松开即复制**到剪贴板（`Esc` 先把高亮收掉）、滚轮滚动，
复制完**右下角浮一条** `已复制 N 字符到剪贴板`（`TOAST_TTL = 3s` 后自消，不占消息流）
`Esc` 停止本回合（退出时会先取消再落盘）、`PgUp`/`PgDn` 滚动、`/` 开头的命令（`/clear` = 把当前窗口
归档到 `~/.pie/windows/` 后开新窗口，历史仍可经指针回查）、
**`@` 开文件路径补全**（候选面板在输入框上方：以 cwd 为根扫一遍、按 `.gitignore` 排除，
`@app` 这种按文件名模糊找、`@src/tu` 这种按目录前缀找；`Tab` 接受、`↑/↓` 选、`Esc` 收起；
接受时 `@` 一起吃掉，插进去的是干净路径；**Tab 掉目录后面板不倒**，接着列这个目录的下一层、
可以逐层钻（`src/` → `src/tui/` → `src/tui/app.rs`）；回车只在候选是**文件**时先补全再发，
目录候选不动——免得正文里的 `@词` 被改成 `词/`）；
**索引之外的目录也能列**：`@..` 列上级目录、`@/` 列根目录、`@~/` 列主目录（＝ `$HOME`，
插进输入框时展开成绝对路径）——这三档走实时列目录，**不用等 cwd 扫盘**，也能接着逐层钻；
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
（旧 Python 版用 Pillow 干这件事）。

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
还没做：abi3 wheel 分发（M4）。

三条约定（错了会挂或者不生效，理由在规划稿里）：

- **同步外观**：`aturn` 阻塞到回合结束，但**期间释放 GIL**（别的 Python 线程照常跑）；要并发就用
  `asyncio.to_thread`。
- **一个 Session 同时只跑一个回合**：回合进行中再调 `aturn` / 读属性会抛 `RuntimeError("session 正忙")`
  —— **事件回调里不要碰同一个 Session**（想中途停：别的线程调 `s.stop()` 或传入 `Cancel`）。
- **`messages` / 事件都是 dict**：字段名与 JSONL / 旧 Python 版 `to_dict()` 一致 → 两边写下的会话可以互读。

## 与旧 Python 版的差异（历史）

pie 在 2026-09-23 之前是一套**纯 Python 实现**（`python -m pie`，内置 `read` / `edit` / `write` /
`shell`，用 Textual 做 TUI），此后整体重构为 Rust，Python 代码已从本仓库删除。旧版长什么样、
当年怎么用 oracle 方式「迁一块对一块」、以及**现行实现与旧版的逐条差异**，都收在
[`docs/python-legacy.md`](docs/python-legacy.md)；本仓库当前行为一律以 `AGENTS.md` / `MEMORY.md`
与源码为准。

## 还没做（TODO，按优先级）

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
  ⚠️ **有意不同于旧 Python 版**：`tool_result` 事件的 `text` **不再截 500 字**（旧版在
  `loop._run_tool_call` 里 `clip_output(text, 500)`）——Session 只搬真话，少显示是展示层的事
  （TUI 按 `TOOL_BODY_LINES` 截、CLI 只取首行）、少回传是工具自己配容量上限的事。
- **工具执行进度**：旧 Python 版 shell 有 `_on_progress`，TUI 能边跑边显示；Rust 版**先移除了**，
  但现在 `Tool::call` 已经有 `ToolCtx`（取消信号就走它）→ 加进度时在同一处挂 `mpsc::Sender` 即可。
  注意丢的只是**实时性**：输出本身仍然完整地作为工具结果返回。
- **工具 panic 文本化**：旧 Python 版把工具抛的异常兜成 `[工具异常] 类型: 消息`，Rust 这边只捕
  `Err(ToolError)` —— 工具里 panic 会带崩整个回合。
- TUI 细化：`Config.theme` 已接线（`catppuccin` / `catppuccin-<flavor>` / 裸 flavor 名都认，
  2026-09-24），**只差明暗自适应**（无 OSC 11 背景探测 → 族名按深色）与运行中切主题的
  `/theme` 命令。（`!cmd` 手动 shell、resume
  历史回放、消息流框选复制、**输入框内鼠标拖选**、**超宽 Markdown 表格按宽度重排**、
  **重试进度单个块**已于 2026-09-23 接上；**代码块语法高亮**已于 2026-09-24 接上。）

## 环境备忘（本机特有）

- 公司代理会用自签 MITM CA 拦 crates.io → `~/.cargo/config.toml` 配了
  `http.cainfo = ~/.cargo/certs/bundle.pem`（系统 CA + 代理链）与 `http.proxy`。
- `api.deepseek.com` **没有**被 MITM（真实证书），程序运行时用系统根证书即可 —— 所以
  `rustls-tls-native-roots` 够用；若将来某端点也被网关拦，需把网关 CA 装进系统库或走 `SSL_CERT_FILE`。
- 本机 nightly（1.100.0）的格式串**不接受 `f` 类型**：`{x:.1f}` 会报 `unknown format trait f`，
  用 `{x:.1}` 代替。
