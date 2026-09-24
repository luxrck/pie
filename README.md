# pie

`pie` 是一个极简的 agent harness，**纯 Rust 实现**。内置 read / edit / **writ** / **bash** 四个工具、
YOLO 模式（无权限确认、不做沙箱）、模型走 OpenAI 兼容接口（DeepSeek / Qwen / vLLM / Ollama…）。

## 当前进度

| 层 | 状态 | 说明 |
|---|---|---|
| `config` | ✅ | `~/.pie/config.toml` 加载 + 分层 system prompt + 首跑写全局记忆种子 |
| `llm` | ✅ | **reqwest + 手写 SSE**（不含任何 OpenAI SDK）+ 自研重试（`Retry-After` 优先）+ Files API + `GET /user/balance` 查余额 |
| `tools` | ✅ | read / edit / **writ** / **bash**（后两名 2026-09-23 用户点名改），统一 `Headers\n\nBody` 输出 |
| `session` | ✅ | JSONL 持久化 + resume + 回合循环（`Session::aturn`） |
| context | ✅ | 三级压缩（工具/轮次/会话级）+ 落盘指针 + manifest + `context info\|verify\|gc` |
| 图片 | ✅ | read 标记 → 本地副本 → Files API 上传（`file` 块注入）；`files list\|gc [--all]`；**无 base64 回退** |
| CLI | ✅ | 一次性 `pie "任务"`（支持 stdin）；`-m/-t/--reserved-tokens/--mode/--max-steps/--no-stream/--cwd/--stat/--tools/--system-prompt` 等覆盖项；`-r/-s`；子命令 `setup`/`sessions`/`context`/`files` |
| TUI | ✅ | ratatui 重写（`src/tui/`）：流式渲染、思考计时、滚动、`/` 命令、`@` 补全、`Esc` 取消 |
| **Python 绑定** | 🚧 | `bindings/pie-py`（PyO3 + maturin）：M0–M3 + M5 已落地（`import pie`），**M4 wheel 分发未做**；见 [`docs/python-bindings.md`](docs/python-bindings.md)（**TUI 不进绑定**） |

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

两处**不能省**，都是编译器定的：trait 里写 `-> impl Future + Send` 而非 `async fn`（`async fn` 表达不出 `Send`，
而注册表要把工具 future 装箱成 `dyn Future + Send`；**impl 里仍写 `async fn`**）；注册传**类型**
`.with_tool::<Read>("read")`（带字段的结构体在 Rust 里不是值表达式）。分发用泛型函数单态化成**函数指针**（不经 `dyn Tool`）。

**实现自包含**：只服务单个工具的辅助逻辑（图片嗅探、字节预算、头部截断、全文落盘、命令构造、edit 诊断）
都就地写在 `impl Tool for X` 的 `call` 里。模块级只留 `format_output` 这一件**被所有工具共用**的小事。

加一个工具 = 写一个结构体 + `impl Tool` + 在 `ToolRegistry::new()` 里加一行 `.with_tool::<T>("名字")`。
注册名与顺序固定为 `read` / `edit` / `writ` / `bash`（由 `builtin_tool_names` 钉住）。

### schema 的 4 个坑（都实测过）

1. **结构体 doc 首行 = 工具描述**，多行说明必须写成普通注释——schemars 会把 doc 里的**单换行合并成空格**。
   真要多行描述得用 `#[schemars(description = "...")]` 显式给（`edit` 就是）。
2. `#[schemars(...)]` 必须写在 `#[derive(JsonSchema)]` **之后**（derive helper 属性的顺序，新版是硬错误）。
3. 嵌套结构体（`edits` 的元素）**不能加 doc 注释**，否则 `items` 会多出一个多余的 description。
4. 归一化四件事：去 `$schema` / `title` / `format`，`Option<T>` 不要 `["integer","null"]`，
   嵌套必须 `inline_subschemas = true`（否则出 `$ref` + `definitions`）。

## 构建与运行

```bash
export PATH="$HOME/.cargo/bin:$PATH"          # 本机 cargo 不在默认 PATH
export CARGO_TARGET_DIR=$HOME/.cache/pie-target      # 源码在 /mnt/d(9p) 时，构建产物放到原生盘
cargo build && cargo test
cargo run -- --models
cargo run -- "用 shell 跑 date，把结果写进 /tmp/x.txt 再 read 验证"
cargo run -- -s demo "记住：我在改 pie"      # 新建/载入会话，跑完落盘
cargo run -- -r "最近那条会话继续"            # 恢复最近的会话（同工作目录优先）
cargo run -- --tools read,ls,grep "看看目录"   # 限制工具：read + 只允许 ls/grep 的 shell
cargo run -- -m deepseek-v4-pro -t high "任务"   # 一次性覆盖模型 / 思考深度（不写回配置）
cargo run -- --reserved-tokens 64k --stat "任务"  # 覆盖输出预留；/stat 报告走 stderr
cargo run -- setup                            # 补齐默认配置文件 + 全局记忆（缺什么写什么，已存在不动）
cargo run -- sessions -l 5                    # 列出最近 5 个会话
cargo run -- context info | context verify | context gc --delete   # 压缩事件 / 死链校验 / 垃圾回收
cargo run -- files list [--all] | files gc --delete [--all]        # 本地图片副本 / 云端上传件
cargo run -- --mode json "任务"                # 一次性模式输出：text（默认）/ json / transcript
cargo run                                     # 无任务 → 进 TUI（真实终端）；-r 则恢复最近会话
```

依赖要求：`build-essential`（`cc` + `crt1.o`）。**不需要** `libssl-dev` / `pkg-config` / `cmake`。
`image` crate（只为图片识别）用 `default-features = false` + `png/jpeg/gif/webp/bmp`：
纯 Rust crate、**零 C 编译**。

### TUI 按键

`Enter` 发送、`Shift+Enter`/`Ctrl+J` 换行（多行编辑器，长行**软换行**）、`Ctrl+A` 全选、`Ctrl+C`/`Ctrl+D`
（空输入）退出、`Ctrl+G` 粘贴图片。`Esc` 停止本回合（先把选区/补全面板收起）、`PgUp`/`PgDn` 滚动。
鼠标：滚轮滚动、左键拖动**框选消息流**（松开即复制到剪贴板，右下角浮一条 toast）、在输入框里拖选。

`!` 开头的输入**直接跑 shell**（不进 LLM、不进上下文）；`/` 开命令（`/help` 全表，`/clear` 把当前窗口归档到
`~/.pie/windows/` 后开新窗口，历史仍可经指针回查）；**`@` 开文件路径补全**（候选面板在输入框上方，
`Tab` 接受、`↑/↓` 选、`Esc` 收起；接受目录后可逐层钻；`@..`/`@/`/`@~/` 分别实时列上级/根/主目录）。

状态栏在**最下方**：左边常驻 `<模型> · <思考深度> · <目录> │ 上下文用量 │ 余额`，右边只有活动指示
（转圈 + 计时）贴右缘。输入框上边框与光标**跟随窗口焦点**（聚焦亮、失焦灰，**状态栏也一起变灰**）；
**键位提示住在输入框 placeholder**（空输入时可见，回合进行中换成 `Esc 停止 …`）。

## Python 绑定（`import pie`）

把核心层（config / llm / tools / session / context）以**原生扩展**给 Python 用；**TUI 不进绑定**。
规划稿（含「为什么这样设计」）见 [`docs/python-bindings.md`](docs/python-bindings.md)。

```bash
cd bindings/pie-py
export PATH="$HOME/.cargo/bin:$PATH"
uv venv --python 3.12 .venv && uv pip install --python .venv/bin/python maturin pytest
VIRTUAL_ENV=$PWD/.venv .venv/bin/maturin develop && .venv/bin/python -m pytest tests -q   # 不联网，本地假 SSE
```

```python
import pie

cfg = pie.Config.load()                            # ~/.pie/config.toml（只改内存，不写盘）
session = pie.Session.ephemeral(cfg, pie.LlmClient(cfg), pie.ToolRegistry.builtins(cfg))
answer = session.aturn("看看当前目录", on_event=lambda ev: print(ev["type"]))
```

三条约定（错了会挂或不生效）：**同步外观但释放 GIL**（要并发用 `asyncio.to_thread`）；
**一个 Session 同时只跑一个回合**（事件回调里别碰同一个 Session，要停就另线程 `stop()`）；
**`messages` / 事件都是 dict**，字段名与 JSONL 一致。

## 还没做（TODO，按优先级）

- **工具执行进度**：TUI 现在看不到 bash 跑到一半的输出；`Tool::call` 已有 `ToolCtx`（取消信号就走它）
  → 加进度时在同一处挂回调即可。丢的只是**实时性**，输出本身仍完整返回。
- **工具 panic 文本化**：目前只捕 `Err(ToolError)`——工具里 panic 会带崩整个回合。
- **TUI**：`Config.theme` 已接线，**只差明暗自适应**（无 OSC 11 背景探测）与运行中切主题的 `/theme` 命令。
- **Python 绑定 M4**（abi3 wheel 分发）未做。

## 环境备忘（本机特有）

- 公司代理用自签 MITM CA 拦 crates.io → `~/.cargo/config.toml` 配了 `http.cainfo`（系统 CA + 代理链）与
  `http.proxy`；`api.deepseek.com` **没被** MITM → `rustls-tls-native-roots` 够用。
- 本机 nightly（1.100.0）的格式串**不接受 `f` 类型**：`{x:.1f}` 报错，用 `{x:.1}`。
- 更全的本机构建细节见 `MEMORY.md`。
