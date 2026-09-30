# `repl` 工具：调研与设计报告

> 状态：**路线 B 已实现**（2026-10-01）。落地记录见 `docs/CHANGELOG.md`；本文档转为「设计依据」保留。
> 实现即本档的 §4（B）：`src/repl.rs` + `src/repl_driver.py`；§4A（路线 A）作为未选方案/将来逃生舱保留。
> （文中「未实现/待拍板」的措辞是调研时的原文，保留作为决策脉络。）
>
> 一句话需求：给 agent 一个工具 `repl`，调用它 = 往一个 IPython 里敲代码；
> **同一 session 内多次调用共享同一份解释器状态**（变量、import、定义）。

---

## 0. TL;DR（结论先行）

1. **「状态在 session 内保持」的正解**：把「一个长活解释器子进程」挂在 **`Session`** 上，
   经 `ToolCtx` 下发给工具。**不能**放 `ToolRegistry`（会被多个 session 复用 → 状态串台），
   **不能**放全局 map（生命周期/清理/并发都是坑）。见 §2、§4.5。
2. **默认推荐路线 B**：「长活子进程 + `IPython.core.interactiveshell.InteractiveShell.run_cell`」，
   Rust 侧用「先 4 字节长度前缀、再 payload」的极简二进制协议与它说话。Rust 零新依赖。见 §3、§4。
3. **备选路线 A（真 kernel / ZMQ）也可行，细节已实测**（§4A）：kernel 是长活进程 → 状态保持、
   中断（`interrupt_request`）、`input()`（stdin 通道）全是现成的；但 **Rust 直连要 ZMQ 依赖**
   （本机无 libzmq/cmake），所以**务实做法是 A2 = Python 侧用 `jupyter_client` 当代理**——
   Rust 零依赖，用的仍是真 kernel。
4. **路线 A / B 的「状态保持」落法完全一样**（都是「长活子进程挂 Session」）：见 §4.5。
5. **代价**：给 `ToolCtx` 加一个「会话级状态槽」字段、给 `Session` 加一个字段、（B 还要）一个约 80 行的
   Python 驱动脚本（`include_str!` 进二进制，不进构建）。改动小、边界清晰。见 §4.5、附录 B。
6. **状态的生命周期 = 进程的生命周期**：`pie -r` 恢复会话**不会**恢复解释器内存
   （活进程无法跨进程重启携带），恢复后是干净的新 REPL。这是有意的取舍，需写进文档。见 §6。
7. **需要你拍板**：默认开/关、状态槽是「通用」还是「repl 专用」、解释器从哪来、A1 还是 A2。见 §10。

---

## 1. 需求与关键约束

- **工具语义**：调用 = 执行一段 Python 源码（可以多行、可以带 IPython 魔法 `%timeit` / `!ls` / `%run`），
  返回「结果 + stdout/stderr」（异常以 traceback 形式返回，**不算工具失败**）。
- **状态保持**：`x = 1` 之后，下一次 `repl` 调用里 `print(x)` 要能看见。
  范围限定为**同一 session 的同一运行进程内**。
- **对齐仓约定**：输出走 `Headers\n\nBody`；判成败只看第一行；不可再生的输出超限要落盘 + 指针；
  必须响应取消（Esc）；必须能被超时兜住；YOLO/无沙箱（与 `bash` 同等信任级别）。
- **宿主是纯 Rust**：核心层**不 embed Python**（那是绑定层的事）。所以 IPython 只能是一个
  **外部子进程**，且必须容忍「这台机器没装 IPython」。

---

## 2. 现状调研：这层架构给了我们什么

### 2.1 工具是「无状态函数指针」

`src/tools.rs`：

```rust
pub type CallFn = Arc<dyn Fn(Value, ToolCtx) -> BoxFuture<ToolResult> + Send + Sync>;
pub struct Entry { pub name: String, pub description: String, pub parameters: Value, pub call: CallFn }
pub trait Tool { fn call(self, ctx: ToolCtx) -> impl Future<Output = ToolResult> + Send; }
```

`call` 的接收者是 `self`（**参数结构体，按值**），第二个参数永远是 `ToolCtx`。
工具**没有任何「自己的持久内存」**——这正是「无状态」的体现。想让 `repl` 记住东西，
就必须把状态塞进它每次都能拿到的东西里 —— 也就是 `ToolCtx`。

### 2.2 `ToolCtx` 是每次调用现造的

```rust
#[derive(Clone, Default)]
pub struct ToolCtx {
    pub cancel: Option<crate::cancel::Cancel>,
    pub storage: crate::config::Storage,
}
```

构造点只在 `Session::tool_call` 一处（`src/session.rs`）：

```rust
let ctx = tools::ToolCtx::with_cancel(cancel.clone(), self.config.storage.clone());
let out = registry.dispatch(&call.function.name, &args, ctx).await;
```

**每次工具调用都新建一个 `ToolCtx`**。所以「什么东西放进去能跨调用存活」，取决于它是不是
一个**长活的 `Arc`**——`cancel` / `storage` 都只是按值 clone（`Cancel` 内部 `Arc`，`Storage` 是路径+配置的廉价副本）。
要放「活进程」，就得放一个**每次 clone 都指向同一份所有权**的 `Arc`。

### 2.3 `Session` 持有 registry，而 registry 会被多个 session 复用

```rust
pub struct Session { pub tools: ToolRegistry, pub config: Config, ... }
```

`ToolRegistry` 是 `Clone` 的（绑定要拿副本建会话）。**关键坑**：Python 绑定里典型写法是

```python
tools = pie.ToolRegistry.builtins(cfg)     # 建一次
s1 = pie.Session.new(cfg, llm, tools)
s2 = pie.Session.new(cfg, llm, tools)      # 同一个 registry 被复用！
```

所以**状态绝不能放 `ToolRegistry` 或 `Entry` 的闭包捕获里**——那样两个 session 会共享同一个
Jupyter 解释器（一个 session 里 `x=1`，另一个 session 里 `print(x)` 也看得见）。同理，一个进程内
多个 session 并发跑时，全局 `HashMap<session_id, Repl>` 也会带来生命周期与并发的复杂度。

### 2.4 结论

> **状态必须挂在 `Session` 实例上，并经由 `ToolCtx` 下发给工具调用。**
> 这是唯一能同时满足「跨调用保持」+「不跨 session 串台」+「并发安全」的落点。

---

## 3. 「基于 IPython」的四条路线

| # | 路线 | 忠实度 | 新增依赖 | 可控性(取消/超时/结构) | 复杂度 | 结论 |
|---|------|--------|----------|------------------------|--------|------|
| A | jupyter_client + ipykernel（真 kernel，ZMQ 协议） | 最高（= Jupyter 本体） | Rust 直连需 **ZMQ**（`zmq` 绑 libzmq / 纯 Rust `zeromq`）；**走 A2 则 Rust 零新增依赖** | 高（stdin/interrupt 通道规范） | 高（HMAC 签名 + 多路复用 multipart 帧 + iopub 路由） | ⚠ **可行**（协议细节已实测）→ 见 **§4A**；务实走 A2（Python 代理） |
| **B** | **长活 `python` 进程内起 `InteractiveShell`，自定义长度前缀协议** | 高（就是 IPython 的执行引擎：魔法、`!`、`In/Out`、`_`） | 仅运行期 Python 侧需 IPython（Rust 零新依赖） | **最高**（协议由我们定，取消/超时/结构化输出都好做） | 中（一个 ~80 行驱动脚本 + Rust owner task） | ✅ **推荐** |
| C | pexpect/PTY 驱动真的 `ipython` 终端 | 最高（连提示符都一样） | 需 PTY 库 / 手搓 PTY | 低（要解析 `In [n]:` 提示符、续行、ANSI；取消=发 Ctrl-C 但输出边界模糊） | 中高但脆 | ✗ 只是「看起来像」，实际最难维护 |
| D | 自写 `exec` 循环（不用 IPython） | 低（只有纯 Python，无魔法/`Out`/`%run`） | 无 | 高 | 低 | ✗ 不符「基于 IPython」 |

**选 B 的理由**：它用的就是 IPython 真正的执行路径（`InteractiveShell.run_cell`），所以魔法命令、
`!shell`、`In[n]/Out[n]`、`_`/`__`、`%run`/`%timeit`/`%pip` 全都天然可用；同时因为进程由我们起、
协议由我们定，**取消、超时、输出截断、落盘指针**这些仓约定都能干净地实现——这是 A/C 都做不到的
性价比。Rust 侧**不新增任何依赖**（`tokio::process` 已有）。

> ⚠ 实现时以本机 IPython 8.x 为准对拍 API：`InteractiveShell.instance()`、
> `shell.run_cell(code, store_history=True)` → `ExecutionResult(result=…, error_before_exec=…, error_in_exec=…)`、
> `IPython.utils.capture.capture_output()`。

---

## 4. 推荐架构（路线 B）

### 4.1 组件图

```
┌─────────────── pie (Rust) ───────────────┐        ┌──────── python -u -c <driver> ────────┐
│ Session                                   │        │  InteractiveShell.instance()          │
│   └─ tool_state: Arc<SessionState> ──┐    │        │  loop:                                │
│ tool_call() → ToolCtx{ …, state } ───┤    │  stdin │    len(4B) + code  ──read──►          │
│ Repl::call:                           │    │ ──────►│    run_cell(code)                     │
│   state.get_or_init(Repl)  ───────────┘    │        │    capture stdout/stderr/result/error │
│   写 code → 读 reply → 格式化输出           │◄───────│    len(4B) + json     ◄──write──      │
└────────────────────────────────────────────┘ stdout └───────────────────────────────────────┘
                     ▲ 子进程活在 Session 里；session 没了 → 通道关闭 → 驱动收 EOF → 自杀
```

### 4.2 驱动脚本（IPython 侧，~80 行，`include_str!` 进二进制）

职责：起一个 `InteractiveShell`，循环读「定长前缀 + 源码」→ `run_cell` → 把结果写成
「定长前缀 + JSON」。要点：

- **单例**：`InteractiveShell.instance()`（进程内单例）→ `x=1` 自然跨 `run_cell` 保留。
- **别污染用户家目录**：关掉历史库（`HistoryManager.enabled=False`）、尽量不读 `~/.ipython` 的 profile。
- **捕获三路输出**：用 `capture_output()` 拿 `stdout`/`stderr`；结果的 `repr` 从
  `ExecutionResult.result` 取（不回放 `Out[n]:` 前缀，格式化交给 Rust 侧，好对齐仓风格）。
- **异常**：`ExecutionResult.error_in_exec` / `error_before_exec` → 序列化成 traceback 文本，
  **标记为 `error` 而不是进程失败**（REPL 里 traceback 是常态）。
- **`input()` 冲突**（重要）：协议占用 stdin，用户在 cell 里调 `input()` 会读到协议字节。
  驱动先把真 stdin 的 **fd 单独复制一份**给协议用，再把 `sys.stdin` 换成空的 dummy /
  覆盖 `builtins.input` 抛清晰错误 → 单元格里 `input()` 得到「本 REPL 不支持交互式输入」。
- **中断**：不捕信号，让 `KeyboardInterrupt` 照常从 `run_cell` 里冒出来（= IPython 里的 Ctrl-C），
  由外层 `try/except` 转成一次 `error` 回复 —— **状态与解释器都保住**。

### 4.3 协议

为了不跟「任意源码 / 任意输出」里的字符打架，**不用行分隔哨兵**（`bash` 用的是行读，因为 shell 输出
按行读够用；这里源码与 traceback 都是任意字节），改用**长度前缀**：

```
Rust → 驱动:  u32 big-endian(len)  ++  code(utf-8, len 字节)
驱动 → Rust:  u32 big-endian(len)  ++  json(utf-8, len 字节)
```

`json` 形如 `{"stdout":…, "stderr":…, "result_repr":…, "error":… }`。
Rust 侧用 `AsyncReadExt::read_exact` 读定长帧。

### 4.4 Rust 侧：owner task + 请求/应答通道

**不要**在 `Repl::call` 里直接对 `ChildStdout` 做可取消的 `read_exact`
（`read_exact` **不是 cancel-safe**：`select!` 里被丢掉会丢半帧，把后续帧全部错位）。推荐：

- `Repl` 状态里养一个 **owner task**（`tokio::spawn`）：独占 `child.stdin/stdout`，
  循环 `recv 请求 → 写帧 → 读整帧 → oneshot 回发`。它**永不进 `select!`**，所以读写永远完整。
- `Repl::call` 只做：`send(请求)` → `select! { reply = oneshot => …, _ = cancel.cancelled() => 中断 }`。
- 取消：拿到 `child.id()`（存成 `u32`，跨平台）→ `SIGINT` 到**子进程组**（起进程时 `process_group(0)`，
  与 `bash` 同款）→ 驱动把 `KeyboardInterrupt` 变成一次 `error` 回复 → 我们带宽限期继续等这一帧
  （保住状态，不丢帧）。宽限期内没回 → `killpg(SIGKILL)` + 把 REPL 标记为 dead（下次调用重建）。
- 超时 `timeout`：同款——先 `SIGINT`，宽限期后 `SIGKILL`，返回 `[repl] 超时…`。

### 4.5 状态保持怎么落地（三选一，推荐 R1）

**R1（推荐）：给 `ToolCtx` 加「会话级状态槽」，`Session` 持有它。**

```rust
// tools.rs
pub struct ToolCtx {
    pub cancel: Option<crate::cancel::Cancel>,
    pub storage: crate::config::Storage,
    /// 会话级工具私有状态：同一 Session 的所有工具调用共享；不同 Session 各一份。
    pub state: Arc<SessionState>,     // SessionState = 内部 Mutex<HashMap<TypeId, Box<dyn Any+Send+Sync>>>
}
impl SessionState {  // 泛型 get_or_init：谁都能往这个槽里放自己的长活对象
    pub fn get_or_init<T: Send + Sync + 'static>(&self, f: impl FnOnce() -> T) -> Arc<T> { … }
}
```

- `Session::at` 里 `tool_state: Arc::new(SessionState::default())`；`tool_call` 里塞进 `ToolCtx`。
- `Repl::call` 用 `ctx.state.get_or_init(Repl::spawn…)` 拿到本会话的 REPL（首次调用**懒启动**）。
- 为什么是**泛型槽**而不是 `ToolCtx.repl: Arc<Repl>` 专用字段：代码量几乎一样，但泛型槽
  顺手给「将来任何有状态的工具」留了口子，且 `ToolCtx` 不必认识任何具体工具。
  （若嫌 `Any` 略重，可用**专用字段**作为最小替代——见 §10 待拍板。）

**R2：全局 `HashMap<会话路径, Repl>`。**
否决：会话路径可重复、`ephemeral` 路径无意义、多 session 并发要额外锁、**谁负责清理**没有答案（内存泄漏）。

**R3：塞进 `ToolRegistry` / `Entry` 闭包。**
否决（见 §2.3）：registry 被多 session 复用 → 状态串台；且 `builtins()` 被复用是绑定的常见用法。

**清理**：`SessionState` 里那份 `Repl` 被 drop 时，owner task 的请求通道关闭 → task 退出 → 杀子进程。
无需显式 `Drop`，也无需全局注册表。**进程崩溃**时子进程会因 stdin 管道 EOF（父 fd 关闭）自行退出——

这是免费拿到的孤儿进程防护。

---

## 4A. 路线 A（真 kernel / ZMQ 协议）的具体实现

> 本节是「假设要基于 A」的落地说明。**协议事实全部在本机实测**（Python 3.9.6 / ipykernel 6.31.0 /
> jupyter_client 8.6.3 / pyzmq 27.2.0，macOS）；复现方法见 §4A.4。
> 与路线 B 的关系：**「状态挂 Session」的做法完全一样（§4.5）**，区别只在「子进程是谁、协议谁来解」。

### 4A.1 事实基础（实测）

**(1) 进程启动**：kernel = `python -m ipykernel_launcher -f <connection_file>`。
连接文件由**客户端**写（kernel 只读它）；实测形状：

```json
{
  "shell_port": 53118, "iopub_port": 53119, "stdin_port": 53120,
  "control_port": 53122, "hb_port": 53121,
  "ip": "127.0.0.1", "transport": "tcp",
  "key": "aaf65ab2-7603f59b2d3da6429009cda5",
  "signature_scheme": "hmac-sha256", "kernel_name": "python3"
}
```

- 端口：客户端预挑 5 个空闲端口（`bind 127.0.0.1:0` 取端口再关；jupyter_client 也这么干，
  有 TOCTOU 但不影响使用）。
- `key` 是个**字符串**（实测形如 `6e48ee56-f4c65bc075f3399ed8895945`，33 字符、含 `-`，**不是 hex**）；
  签名密钥 = 它的 **UTF-8 字节**（实测：只有用 raw bytes 签名 kernel 才认，`bytes.fromhex` 直接报错）。
  **空串 = 不签名**（`signature` 段为空）。

**(2) 五个通道（ZMQ socket）**：

| 通道 | 类型 | 用途 |
|---|---|---|
| shell | DEALER | 发 `execute_request` / `kernel_info_request`，收 `*_reply` |
| iopub | SUB（subscribe `""`） | 广播：`status` / `execute_input` / `stream` / `execute_result` / `display_data` / `error` |
| stdin | DEALER | kernel 反向要输入：`input_request` → `input_reply` |
| control | DEALER | `interrupt_request` / `shutdown_request`（高优先级，不被 shell 上的执行阻塞） |
| hb | REQ | 心跳（存活探测） |

**(3) 帧格式（multipart）**——实测：

```
shell / control 收： [ <IDS|MSG> , signature , header , parent_header , metadata , content , (extra buffers…) ]
iopub 收：           [ topic , <IDS|MSG> , signature , header , parent_header , metadata , content , (extra buffers…) ]
```

- **shell/control 的 reply 没有 topic 帧；iopub 有且只有 1 个 topic 帧**
  （实测形如 `b'stream.stdout'` / `b'kernel.<kernel-id>.status'` / `b'kernel.<kernel-id>.execute_result'`）。
  解析时统一「**找 `<IDS|MSG>` 分隔符，之前的都当路由前缀跳过**」。
- `signature` = `hex(HMAC-SHA256(key_bytes, header ‖ parent_header ‖ metadata ‖ content))`
  —— 四段 JSON 字节按序拼接、**无分隔符**。**实测逐字验证通过**。
- 四段 JSON 的最小形状：`header = {msg_id, username, session, date(ISO8601), msg_type, version:"5.3"}`；
  `parent_header`（请求时 `{}`，reply 里回填请求 header）；`metadata = {}`；`content` 因消息类型而异。
- **可选 extra buffers**：图像等二进制负载可能作为 content 之后的额外帧传（实测纯文本时 extra=0）。
  只取文本的话可忽略这层。

**(4) 一次执行的消息序列**——实测 `print("hi")\n2+3`：

```
iopub: status(busy) → execute_input → stream(stdout,"hi\n") → execute_result({"text/plain":"5"}) → status(idle)
shell: execute_reply {status:"ok", execution_count:1, payload:[], user_expressions:{}}
```

- 判「本次执行结束」：iopub 上 **parent == 本次 msg_id 的 `status/idle`**（权威信号）。
- `stream`：`{name:"stdout"|"stderr", text}`（**可能分多条**，按序拼）。
- `execute_result`：`{data:{"text/plain":…}, metadata, execution_count}`（末表达式的值）。
- `display_data`：`{data, metadata}`（`display()` 的输出）。
- `error`：`{ename, evalue, traceback:[…]}`（**traceback 是行数组**）。
- ⚠ 异常时 iopub 给 `error`、`execute_reply.status == "error"`——这**不算工具失败**，是 REPL 的正常输出
  （与 §5 的判成败口径一致）。

**(5) 状态保持**：kernel 是**长活进程**，`execution_count` 单调递增、变量跨 `execute_request` 保留（实测）。
→ 状态保持天然成立，和路线 B 一样，问题只剩「谁持有这个进程」（答：`Session`，见 §4.5）。

**(6) 中断**——实测：control 通道发 `interrupt_request` →

```
iopub: error {ename:"KeyboardInterrupt", evalue:""}
shell: execute_reply {status:"error"}
（随后再 execute 仍正常，状态保留）
```

这正是想要的「Ctrl-C 语义」：**保状态、可恢复**；且是**协议里正规的一步**（不像 B 要自己给子进程发 SIGINT）。

**(7) stdin 通道**——实测 `input()`：kernel 在 stdin 通道发 `input_request {prompt:"who? ", password:false}`，
客户端回 `input_reply {value:…}` 后 kernel 继续。→ 路线 A **原生支持 `input()`**（B 里这是要绕开的坑）。
不想要就发 `allow_stdin:false`。

**(8) 握手 / 就绪**：`wait_for_ready` ≈ 在 shell 上反复发 `kernel_info_request` 直到收到 `kernel_info_reply`
（实测 `protocol_version:"5.3"`、带 `language_info.version`）。⚠ **iopub SUB 有 slow-joiner**：
订阅后要等订阅生效再执行，否则漏掉开头的 `busy`/`execute_input`（实测确实会漏）——用 kernel_info 往返 + 短 sleep 兜底。

### 4A.2 两个子方案

| | **A1：Rust 直连 kernel** | **A2：Python 侧 `jupyter_client` 代理** |
|---|---|---|
| 谁讲 ZMQ | Rust（`zeromq` 或 `zmq` crate） | Python 的 `jupyter_client`（pyzmq 是现成 wheel） |
| Rust 依赖 | ZMQ 客户端 + hmac（见下） | **零新增**（Rust 只讲 §4.3 那个定长协议） |
| 忠实度 | 完全（自己实现协议） | 完全（`jupyter_client` 就是官方客户端） |
| 工作量 | 大（帧/签名/多通道/慢加入/多路复用全自己写） | 小（代理脚本 ~60 行） |
| 风险 | 协议细节多、需专门测试 | 多一层进程；代理要装 `jupyter_client` |
| 建议 | 只在「Rust 必须脱离 Python 客户端」时选 | **务实首选** |

> 两者与 §4.5 的「状态挂 Session」完全一样：kernel 子进程是长活的，挂法不变。

#### A1 的 Rust 依赖现实（本机实测结论）

- **`zmq`（rust-zmq 0.10，绑 libzmq）**：需要 **libzmq 与 pkg-config**；`vendored` feature 会用 **cmake** 现场编 libzmq。
  本机 **既无 libzmq、也无 pkg-config、也没装 cmake**（`brew install zeromq` + `cmake` 可解）
  —— 与本仓「零 C 依赖、能不引就不引」直接冲突。
- **`zeromq`（纯 Rust，0.6.0，tokio）**：支持 TCP + `DEALER`/`ROUTER`/`SUB`/`PUB`/`XSUB`/`XPUB`，
  README 自述 **Basic ZMTP 已对参考实现做过互操作测试**；**无 C 依赖**。代价：成熟度不如 libzmq（未覆盖全部 ZMQ 特性）。
  实测 API 形状：`DealerSocket::new().connect("tcp://…")` / `SubSocket::new().subscribe("")` /
  `socket.send(ZmqMessage)` / `socket.recv() -> ZmqMessage`，`ZmqMessage` 用 `push_back`/`get`/`into_vec` 拼拆帧。
- **签名**：本仓**已有 `sha2` + `hex`**；HMAC 要么加 `hmac`（纯 Rust、极小），要么用 `sha2` 手写 HMAC（~30 行）。
- **结论**：A1 要在本仓不引 C 依赖，应选 **纯 Rust 的 `zeromq`**；但**实现前必须先写最小往返原型**
  验证它与 ipykernel 的互操作（这是 A1 最大的未验证风险）。

#### A2（推荐）的代理脚本形状

Python 代理用 `jupyter_client.KernelManager` 起 kernel、`BlockingKernelClient` 收发，把 §4.3 的
「定长前缀 + JSON」协议映射到「`kc.execute(code)` + 收 iopub 直到 idle + 回一帧 JSON」：

- 输出收集：照 §4A.1(4) 的序列拼 `stdout`/`stderr`/`execute_result`/`display_data`/`error`。
- 中断：Rust 发一个「中断」控制帧（或直接对代理进程转 SIGINT）→ 代理调 `km.interrupt_kernel()`。
- `input()`：把 `allow_stdin` 设为 `False`（或代理统一回空），避免 stdin 通道把回合挂死。
- 代理**和 kernel 是两个进程**（`KernelManager` 会 fork 出 kernel）；代理退出时记得 `km.shutdown_kernel()`，
  否则 kernel 会变孤儿（B 那条「父死 → stdin EOF → 自杀」的免费防护在这儿不成立，要显式关）。

### 4A.3 落地计划（若选 A）

| 阶段 | 内容 |
|---|---|
| **A-M0** | 选定 A1/A2；**A1 先做最小往返原型**（连真 kernel、跑一个 cell、验证签名与 iopub 解析） |
| **A-M1** | 接进 `repl`：连接文件生成 + 起 kernel + 握手就绪 + 执行 + 输出格式化（截断/落盘）+ 状态挂 Session（§4.5） |
| **A-M2** | 中断（control `interrupt_request`，保状态）、超时、`shutdown_request` 清理、hb 存活探测、子进程自死重启 |
| **A-M3** | 降级文案（没装 ipykernel/jupyter_client）、`_python` 配置、文档 |

### 4A.4 复现（本节「实测」的验证方法）

本节结论来自一次性脚本：起 `KernelManager` → 打印连接文件 → 用 `jupyter_client.Session.serialize`
核验帧结构与 HMAC 签名 → 收 iopub 序列；另用**裸 pyzmq** 手搓 `execute_request`/`interrupt_request`
验证「Rust 侧要实现的每个字节」（含 iopub 的 topic 帧、shell 无 topic 帧）。
复现依赖：`pip install ipykernel jupyter_client`（本机已实测可用）。

---

## 5. 与现有契约对齐

| 约定 | `repl` 的做法 |
|------|----------------|
| `Headers\n\nBody` | 成功：直接给正文（`stdout` + `result_repr`）。异常：正文里就是 traceback。 |
| **判成败只看第一行** | **普通异常不算失败**（否则 TUI 会把每次 traceback 标红，而 REPL 里 traceback 是常态）→ 不打 `[exit=…]`。只有**基础设施失败**（起不了解释器 / 缺 IPython / 协议错 / 被杀）才 `err(...)` → 上层 `[工具错误] {e}`。 |
| 不可再生 → 落盘 | 单元格输出一旦被消费就没了 → 超 `_max_lines`/`_max_bytes` 时**头部截断 + 全文落盘**（`StoreType::Raw { prefix: "repl" }`），指针走 `ToolOutput.spill`——与 `bash` **逐字同款**。 |
| 取消 | `ctx.cancel`：`SIGINT`（保状态）→ 宽限 → `SIGKILL`；返回 `CANCEL_TEXT` 让上层收尾。 |
| 超时 | 同款，`timeout` 为 `Option<i64>` 秒（与 `bash` 对齐，默认不设）。 |
| 并行 | 状态槽内的 REPL 用 `tokio::sync::Mutex` 串行化；同批 `parallel_tools` 里有多个 `repl` 调用会**排队**（顺序不保证）→ 文档写明「状态型工具建议一次一个」。 |
| 日志 | 起/杀子进程、降级告警一律 `log::warn`（TUI 期间不能写 stderr）。 |
| 工作目录 | 子进程 `current_dir = std::env::current_dir()`（`/cd`、`--cwd` 已改过进程 cwd）→ 与 `bash` 语义一致。 |
| 隐私/API 边界 | 与其它工具一样：输出进历史、可被压缩。无本地专有字段问题。 |

---

## 6. 生命周期与边界

- **懒启动**：Session 刚建时**不**起子进程；第一次 `repl` 调用才 `spawn`（没用到就不付代价，
  也让「这机器没装 IPython」只在真用时才暴露）。
- **同一 session 内保持一致**：后续调用复用同一子进程 → 变量/import/定义/`%pip` 装的包都在。
- **`pie -r` / `-s` 恢复**：**不恢复**解释器内存（活进程带不过去）。恢复后是干净的新 REPL。
  小字提示写进 `repl` 的工具描述或首次返回里，避免模型误以为 `x` 还在。
  （可选：把「上次 REPL 里定义过什么」记一行到会话文件做提示，但**不建议持久化命名空间**——pickle 不可靠。）
- **`/clear`、压缩**：不影响 REPL；它跟历史解耦。
- **`ephemeral` 会话**：照样有 REPL（挂在 Session 上），随 Python 的 `Session` 对象 GC 而清理。
- **多 session 并发**（绑定场景）：各持各的子进程，互不干扰。
- **子进程自己死了**（用户 `exit()` / `%run` 里 `sys.exit()`）：下次调用检测到 owner task 已退出 → 重启一个
  干净 REPL，并在返回里说明「解释器已重启，之前的变量没了」。

---

## 7. 安全

与 `bash` **同级**：都是「执行任意代码、无权限确认、无沙箱」。这不是新增攻击面——`bash` 已经能
`python -c '…'`。要收紧只能靠 `--tools` 里**不启用** `repl`（见 §8）。文档里明说这一点即可。

---

## 8. 配置与开关

- **工具白名单**：`tools_from_spec` 的 `BUILTINS` 加 `"repl"`；`ToolRegistry::new` 加
  `.with_tool::<Repl>("repl")`。同步更新 `builtin_tool_names` 等固化名字的用例（§11）。
- **参数形状（实现定稿）**：公开参数只有 `code`（必填）与 `timeout`（秒，缺省不设；与 `bash` 一致）；
  其余是私有参数（`[tools.repl]` 下划线注入，与 `[tools.bash]` 同款）：

  ```toml
  [tools.repl]
  _python = "python3"     # 解释器；开头 `~` 会展开成 $HOME（可指向 venv 里的 python）
  _max_lines = 200       # 输出行上限（超限只留开头 + 落盘全文）
  _max_bytes = 65536
  ```

  （`_driver` 也是私有参数：覆盖驱动脚本正文，只给测试用假驱动。）

- **IPython 从哪来**：优先用 `_python` 指定的解释器里 `import IPython`；缺失 → 工具**返回一句清晰错误**
  （「该解释器没装 IPython，`pip install ipython` 或把 `[tools.repl] _python` 指向装了 IPython 的 venv」），
  **不 panic、不影响其它工具**。
- **默认开/关**：建议**默认开启**（与 `bash` 一致，符合「打开 ipython 就写」的直觉），
  因为缺 IPython 时它只是「一个会报错但无害的工具」。想关就 `--tools read,edit,writ,bash`。

---

## 9. 分阶段落地

| 阶段 | 内容 | 验收 |
|------|------|------|
| **M0 骨架** | 驱动脚本 + 协议 + owner task；`Repl::call` 单次执行；输出格式化（含截断落盘）。**先把状态槽加好**（R1）。 | 一次 `repl "1+1"` 返回 `2`；`repl` 不在 `--tools` 时不存在 |
| **M1 状态保持** | `SessionState` + 懒启动 + 复用；「跨两次调用变量还在」端到端用例 | `x=1` 后 `print(x)` 得 1；两个 Session 互不可见 |
| **M2 中断与超时** | SIGINT→宽限→SIGKILL；`timeout`；会话结束/进程退出的清理与孤儿自愈 | Esc 停住长 cell 且状态存活；kill -9 pie 后子进程自行退出 |
| **M3 打磨** | `_python`/降级文案/子进程自死重启/文档/`AGENTS.md`+`MEMORY.md` 更新 | 全绿 + 一条真机手测记录进 CHANGELOG |

---

## 10. 风险与待拍板

**风险**

1. **异步读帧的 cancel-safety**：直接 `select!` + `read_exact` 会丢半帧 → 必须 owner task（§4.4）。
2. **IPython API 漂移**：`InteractiveShell` 的行为随版本变；把「驱动脚本」当**契约**钉住，用一条
   真机冒烟用例（本机当前 **未装 IPython**，CI/开发机需先 `pip install ipython`）。
3. **`input()` / `%debug` / `%matplotlib` 等交互式特性**：无终端 → 不支持，需在驱动里**优雅报错**而非挂死。
4. **`KeyboardInterrupt` 被用户代码吞掉**（`except BaseException` / C 扩展不听 SIGINT）：宽限期后 `SIGKILL`，
   代价是丢状态（文档写明）。
5. **输出体积**：`matplotlib`/`pandas` 的富文本输出可能巨大 → 靠 `_max_lines/_max_bytes` + 落盘兜底。
6. **（路线 A）ZMQ 互操作与慢加入**：纯 Rust `zeromq` 与 ipykernel 的互操作**未在本仓验证**；
   iopub SUB 有 slow-joiner（漏收开头帧）。→ A2 一次绕开这两条（jupyter_client 已处理）。

**待你拍板**

- **P1 默认开/关**：建议默认开（缺 IPython 时无害化报错）。要不要更保守地默认关、靠 `--tools` 开？
- **P2 状态槽形态**：通用 `Any` 槽（§4.5 R1，略抽象、面向未来）还是 `repl` 专用字段（更直白、更少代码）？
- **P3 解释器来源**：默认 `python3`，还是跟随 `PIE_REPL_PYTHON` / 当前 venv（`VIRTUAL_ENV`）？
  本机 `python3` = 3.9.6 且**没装 IPython**——默认值选不好会「开箱即报错」。
- **P4（若走路线 A）A1 还是 A2**：A1 = Rust 直连 kernel（要 ZMQ 依赖、协议全自写、与 ipykernel 的
  互操作待验证）；**A2 = Python 侧 `jupyter_client` 代理（Rust 零新增依赖，推荐）**。**P5（若选 A2）**
  代理进程与 kernel 的清理顺序、以及要不要把 stdin 通道接给用户（还是直接 `allow_stdin:false`）。

---

## 11. 测试策略（不联网）

- **驱动脚本契约测试**（纯 Python，可在 Rust 之外先跑）：起驱动进程，喂几帧，断言回复 JSON；
  含多行、异常、stdout、`%` 魔法、`!` shell。
- **Rust 侧**：`Repl::call` 用**假驱动**（一个回放固定帧的小进程 / 脚本）测格式化、截断、落盘指针、
  取消、超时；不需要真 IPython。
- **状态用例**：同一 `Session` 两次调用共享变量；两个 `Session` 不共享；`ephemeral` 亦然。
- **固化名字用例**：更新 `builtin_tool_names` 与 `tools_from_spec` 的期望（现在全是
  `["read","edit","writ","bash"]`）。
- 参考 `bash` 的既有测试（进程组、取消、超时、落盘）逐条对照补齐。
- **（若走路线 A）**：A1 需一条「纯 Rust ZMQ ↔ ipykernel」的互操作用例（最小往返：发 `kernel_info_request`
  收 reply、发 `execute_request` 收满 iopub 序列）；A2 则测「代理 → Rust」的定长协议，kernel 部分可复用
  §4A.1 的实测序列当 golden。

---

## 附录 A：驱动脚本骨架（伪代码，实现时对拍 IPython 版本）

```python
# pie_repl_driver.py —— 由 `repl` 工具以 `python -u -c <本文件内容>` 启动
import io, json, os, struct, sys, traceback

# 1) 协议走真 stdin/stdout 的**文件描述符副本**，把 sys.stdin 让给用户代码的 input()
proto_in = os.fdopen(os.dup(sys.stdin.fileno()), "rb", buffering=0)
proto_out = os.fdopen(os.dup(sys.stdout.fileno()), "wb", buffering=0)
sys.stdin = io.StringIO("")           # 单元格里 input() 拿到 EOF（驱动再把内置 input 换成清晰报错）

from IPython.core.interactiveshell import InteractiveShell
from IPython.utils.capture import capture_output

shell = InteractiveShell.instance()
shell.history_manager.enabled = False  # 不往 ~/.ipython 写历史

def read_exact(n):
    buf = b""
    while len(buf) < n:
        chunk = proto_in.read(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return buf

while True:
    try:
        (n,) = struct.unpack(">I", read_exact(4))
        code = read_exact(n).decode("utf-8")
    except EOFError:
        break                                     # 父进程没了 → 退出（孤儿自愈）
    with capture_output() as cap:
        res = shell.run_cell(code, store_history=True)
    err = None
    if res.error_before_exec is not None:
        err = "".join(traceback.format_exception(res.error_before_exec))
    elif res.error_in_exec is not None:
        err = "".join(traceback.format_exception(res.error_in_exec))
    payload = {
        "stdout": cap.stdout,
        "stderr": cap.stderr,
        "result_repr": None if res.result is None else repr(res.result),
        "error": err,
    }
    body = json.dumps(payload).encode("utf-8")
    proto_out.write(struct.pack(">I", len(body)) + body)
```

## 附录 B：Rust 改动清单（预估）

| 文件 | 改动 |
|------|------|
| `src/tools.rs` | 新增 `Repl` 结构体 + `impl Tool`（参数 `code` / `timeout` / `_python` / `_max_*`）；`ToolCtx` 加 `state: Arc<SessionState>`；`SessionState`（泛型槽）；`ToolRegistry::new` 加 `.with_tool::<Repl>("repl")`；`tools_from_spec` 的 `BUILTINS` 加 `"repl"`；驱动脚本 `include_str!("repl_driver.py")` |
| `src/session.rs` | `Session` 加 `tool_state: Arc<SessionState>`；`Session::at` 初始化；`tool_call` 构造 `ToolCtx` 时带上 |
| `src/tools/repl_driver.py` | 新增（数据文件，不进构建） |
| `AGENTS.md` / `MEMORY.md` | 记录 `repl` 工具与「会话级状态槽」这个新概念 |
| 测试 | `builtin_tool_names`、`tools_from_spec` 期望更新；新增 repl 用例（状态/取消/超时/落盘/降级） |

> 若走**路线 A**：`ToolCtx` / `Session` 的改动**一字不变**（状态仍挂 Session）；
> 变的只是 `Repl` 内部（换成 kernel 客户端 / 起代理）；A1 额外加 ZMQ 依赖，
> `src/tools/repl_driver.py` 换成 `src/tools/repl_proxy.py`（A2）或去掉（A1）。

---

## 附录 C：A1（Rust 直连 kernel）代码骨架

> 伪代码，只为把「具体要实现什么」落到代码上；真实现要在 §4A.3 的 A-M0 先验证 `zeromq` 与 ipykernel 的互操作。

```toml
# Cargo.toml（A1 新增；sha2 / hex / serde_json / tokio 本仓已有）
zeromq = "0.6"                 # 纯 Rust、tokio。不想用它就换 libzmq 版：
                               # zmq = { version = "0.10", features = ["vendored"] }  # ⚠ 要 cmake
hmac = "0.12"                  # 纯 Rust；或省掉，用已有 sha2 手写 HMAC（~30 行）
uuid = { version = "1", features = ["v4"] }   # msg_id / session；或自己发计数器+pid
```

```rust
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeromq::{DealerSocket, SubSocket, ZmqMessage, Socket, SocketRecv, SocketSend};
use serde_json::{json, Value};

// §4A.1(1) 连接文件
#[derive(serde::Deserialize)]
struct Conn { shell_port: u16, iopub_port: u16, stdin_port: u16, control_port: u16,
              hb_port: u16, ip: String, transport: String, key: String, signature_scheme: String }
impl Conn { fn addr(&self, port: u16) -> String { format!("{}://{}:{}", self.transport, self.ip, port) } }

// 签名 + 四段 JSON，组一帧（shell/control/stdin 发的时候没有 topic 前缀，§4A.1(3)）
// 注意 key 传的是连接文件里那串的 **UTF-8 字节**（conn.key.as_bytes()），不是 hex 解码（§4A.1(1)）
fn encode(key: &[u8], header: &Value, parent: &Value, meta: &Value, content: &Value) -> ZmqMessage {
    let blobs: Vec<Vec<u8>> = [header, parent, meta, content]
        .iter().map(|v| serde_json::to_vec(v).unwrap()).collect();
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    for b in &blobs { mac.update(b); }                       // 无分隔符、按序拼接
    let sig = hex::encode(mac.finalize().into_bytes());
    let mut msg = ZmqMessage::from(b"<IDS|MSG>".to_vec());
    msg.push_back(sig.into_bytes().into());
    for b in blobs { msg.push_back(b.into()); }
    msg
}

// 解析回包：跳过 topic/identity，在 <IDS|MSG> 处切开
fn decode(key: &[u8], msg: &ZmqMessage) -> (Value /*header*/, Value /*parent*/, Value /*content*/) {
    let frames: Vec<_> = msg.iter().collect();
    let i = frames.iter().position(|f| &f[..] == b"<IDS|MSG>").expect("no delimiter");
    let (sig, blobs) = (&frames[i + 1], &frames[i + 2..i + 6]);
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    for b in blobs { mac.update(b); }
    assert_eq!(hex::encode(mac.finalize().into_bytes()), String::from_utf8_lossy(sig)); // 校验
    (serde_json::from_slice(blobs[0]).unwrap(), serde_json::from_slice(blobs[1]).unwrap(),
     serde_json::from_slice(blobs[3]).unwrap())
}

// ⚠ `date` 是 ISO8601；本仓的时间工具（`config::now` / `fmt_local`）是本地展示向的，
//    这里要自己拼一个 UTC ISO8601（或直接用任意可解析字符串；kernel 对它不严格校验）。
fn header(msg_type: &str, session: &str) -> Value {
    json!({ "msg_id": uuid::Uuid::new_v4().simple().to_string(), "username": "pie",
            "session": session, "date": now_iso8601(), "msg_type": msg_type, "version": "5.3" })
}

async fn run_cell(conn: &Conn, key: &[u8], session: &str, code: &str) -> String {
    let mut shell = DealerSocket::new(); shell.connect(&conn.addr(conn.shell_port)).await.unwrap();
    let mut iopub = SubSocket::new(); iopub.subscribe("").await.unwrap();
    iopub.connect(&conn.addr(conn.iopub_port)).await.unwrap();

    // §4A.1(8) 就绪 + 绕开 slow-joiner：kernel_info 往返，再短 sleep
    let h = header("kernel_info_request", session);
    shell.send(encode(key, &h, &json!({}), &json!({}), &json!({}))).await.unwrap();
    let _ = shell.recv().await.unwrap();                       // kernel_info_reply
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // 发 execute_request
    let h = header("execute_request", session);
    let mid = h["msg_id"].as_str().unwrap().to_string();
    let content = json!({ "code": code, "silent": false, "store_history": true,
                          "user_expressions": {}, "allow_stdin": false, "stop_on_error": true });
    shell.send(encode(key, &h, &json!({}), &json!({}), &content)).await.unwrap();

    // 收 iopub 直到「parent==mid 的 status/idle」（§4A.1(4)）；普通异常不算工具失败（§5）
    let mut out = String::new();
    loop {
        let (hdr, parent, c) = decode(key, &iopub.recv().await.unwrap());
        if parent.get("msg_id").and_then(Value::as_str) != Some(mid.as_str()) { continue; }
        match hdr["msg_type"].as_str().unwrap() {
            "stream" => out.push_str(c["text"].as_str().unwrap_or("")),
            "execute_result" | "display_data" =>
                if let Some(t) = c["data"].get("text/plain") { out.push_str(t.as_str().unwrap_or("")); },
            "error" => out.push_str(&c["traceback"].as_array().map(|a|
                a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n")).unwrap_or_default()),
            "status" if c["execution_state"] == "idle" => break,
            _ => {}
        }
    }
    // 可选：再收 shell 上的 execute_reply 拿 execution_count / status
    // 收尾：control 通道发 shutdown_request；中断：发 interrupt_request（§4A.1(6)）
    out
}
```
