# pie-rs：Python 绑定规划（PyO3 / maturin）

> **进度**：M0–M3 + M5 ✅（`import pie` 可用：`Config`/`LlmClient`/`ToolRegistry`/`Session`/`Cancel`
> /`run()`/`list_sessions()` + 事件回调 + 异常层级 + 类型存根 + `@pie.tool`）。**M4（abi3 wheel 分发）⬜**。
> 实现时定的东西见 §11。
>
> 目标：把 `pie-rs` 的 harness 能力以原生扩展的形式给 Python 程序调用。包名 `pie`（`import pie`）
> ——它曾与**旧纯 Python 实现**（`python -m pie`，Textual TUI 那套）同名，但那个实现已于 2026-09-23
> 删除（历史与差异见 [`python-legacy.md`](python-legacy.md)），本绑定就是它现在的对应物。
>
> 相关：`README.md`（进度与用法）、`../MEMORY.md`（决策记录）、`python-legacy.md`（旧 Python 版历史）。

---

## 1. 目标与非目标

### 目标

- Python 侧能完成「配一个模型 → 建工具集 → 开会话 → 丢一个任务 → 收事件/收答复」的完整闭环，
  能力对齐（已删除的）旧纯 Python 版的 `run` / `aturn` / `Session` / `Config` / `ToolRegistry`。
- 会话文件、上下文压缩、图片 Files API 这些**跨语言共享**的资产继续共用 `~/.pie/`（两边已同格式）。
- 用 Python 写自定义工具注册进 harness（对齐旧 Python 版 `@tool()` 的体验）。
- 可分发的 wheel（maturin 构建），至少覆盖本机 macOS，其次 Linux / Windows。

### 非目标

- **TUI 不进绑定**：`src/tui/`（ratatui / crossterm / tui-markdown / arboard）不暴露、不依赖。
  交互界面继续走 `pie-rs` 二进制。
- 不追求 1:1 覆盖旧 Python 版 `__all__`（31 项）；先覆盖「跑 agent」这条主链，其余按需补。
- 不保证与旧 Python 版**行为逐字一致**——Rust 版已有几处有意差异（不回退内联 base64、shell 超限保留头部、
  Toast 时长等），绑定继承 Rust 行为，文档需明示（完整差异清单见 [`python-legacy.md`](python-legacy.md)）。

---

## 2. 现状盘点：四个必须先解决的障碍

| # | 现状 | 影响 |
|---|---|---|
| 1 | **纯 bin crate**：没有 `src/lib.rs`，`Cargo.toml` 只有 `[[bin]]`，`mod config;` 等全是私有 | 外部 crate 拿不到任何 API → 第一步必须提出库 |
| 2 | `mod tui;` 与核心模块**在同一个 crate**，TUI 依赖（ratatui/crossterm/arboard…）是必选依赖 | 绑定会连带编译 TUI（慢、平台依赖）→ 需要 feature gate 或拆 crate |
| 3 | **async-first**：`Session::aturn` / `LlmClient::complete` 都是 `async fn`，需要 tokio runtime | Python 侧要处理 runtime 与 GIL 的关系（见 §5.2） |
| 4 | 工具是**静态泛型**：`with_tool::<T>(name)` 单态化成函数指针 `fn(Value, ToolCtx) -> BoxFuture`，`Entry.name` 还是 `&'static str` | 运行期从 Python 注册工具没有通道 → 需要给 `Entry` 加一个动态变体（见 §5.5） |

另外两个既成事实要利用好：

- `Message` / `Config` 等核心类型**已经全部 serde**（`Serialize + Deserialize`，字段名与旧 Python 版 `to_dict()` 逐字对齐）
  → 跨语言桥接优先走 JSON/dict，不必写逐字段转换。
- `Session::aturn(&mut self, input, on_event: &mut (dyn FnMut(TurnEvent) + Send), cancel: &Cancel)` 的形状
  天然适合「事件推 channel + 主线程分发」的绑定方式（见 §5.3）。

---

## 3. 目标 API 草案（Python 侧长什么样）

```python
import pie

cfg = pie.Config.load()                 # ~/.pie/config.toml（等价于 CLI 的 -c）
cfg.model = "deepseek-flash"               # 内存覆盖，不写盘（与 CLI 覆盖项同语义）
cfg.reasoning_effort = "high"

llm   = pie.LlmClient(cfg)
tools = pie.ToolRegistry.builtins(cfg)  # read / edit / writ / bash（Rust 侧工具名）

# —— 会话 ——
s = pie.Session.new(cfg, llm, tools)          # 或 Session.load(path, ...) / Session.resume(...)
# s = pie.Session.ephemeral(cfg, llm, tools)  # 不落盘、不写 manifest（服务 / notebook 用）

# ① 回调式（阻塞到回合结束，返回最终答复）
s.aturn("读一下 README 的前 20 行", on_event=lambda ev: print(ev["type"], ev))

# ② 迭代器式（生成器，边跑边取事件；底层同一个 channel）
for ev in s.aturn_stream("同样的问题"):
    if ev["type"] == "assistant_text":
        print(ev["text"], end="")

# ③ 原生 async（M5 起，feature gate；见 §5.2）
async with pie.Session.new(cfg, llm, tools) as s:
    answer = await s.aturn_async("同样的问题", on_event=print)
    async for ev in s.events():
        ...

print(s.messages[-1]["content"])     # 历史是 dict 列表（字段名 = JSONL 字段名）
print(s.usage_report())              # /stat 那段文本
s.compact("tools"); s.reset(); s.save()

# —— 取消 ——
tok = pie.Cancel()
threading.Thread(target=lambda: s.aturn("写一篇长文", on_event=..., cancel=tok)).start()
tok.cancel()

# —— 一次性 ——
print(pie.run("总结这个仓库", cfg=cfg))      # 无会话、不落盘

# —— 自定义工具（M3） ——
@pie.tool(name="fetch", description="抓一个 URL 的正文")
def fetch(url: str) -> str:
    """url: 要抓的地址"""
    return requests.get(url).text     # 返回值 str → 工具结果文本

tools.register(fetch)                 # 或 tools.register(name="x", schema={...}, fn=f)
```

### 曝光面（分档）

| 档 | 对象 | 说明 |
|---|---|---|
| A 必做 | `Config`、`LlmClient`、`ToolRegistry`、`Session`、`Cancel`、`run()`、`TurnEvent` | 跑 agent 的主链 |
| B 顺带 | `list_sessions()`、`CompactMode`、`usage`、`compression_history()`、`full_history()`、`Message`（dict） | 与 CLI 子命令同能力 |
| C 可选 | `upload_file`/`list_files`/`delete_file`、`parse_reserved_tokens`、`build_system_prompt`、常量（`DEFAULT_MODEL` …） | 少数人会用 |
| ✗ | TUI 全部、`main.rs` 的 clap 结构 | 不进绑定 |

### 数据形态：**dict-first + 少量 pyclass**

| Rust 类型 | Python 形态 | 理由 |
|---|---|---|
| `Session` / `Config` / `LlmClient` / `ToolRegistry` / `Cancel` | `#[pyclass]`（有方法、有身份） | 需要持有状态 |
| `Message` | **dict**（`serde_json` → Python；可选 `pythonize` crate 零样板） | 字段多、含压缩元数据，逐字段 pyclass 维护成本高 |
| `TurnEvent` | dict（`{"type": "tool_call", "name": …, "arguments": …}`） | 与旧 Python 版 `on_event` 的 dict 形状**一致**，两边代码可移植 |
| `Usage` / `CompactStats` | dict | 同上 |
| `Config` 的字段 | `to_dict()` / `update(dict)` + 少量 `#[getter]`/`#[setter]`（model / base_url / reasoning_effort / context_window / reserved_tokens / compaction / tools / tui） | 全字段属性太脆（Rust 结构体会变）；常用项给属性，其余走 dict |
| 每回合的执行旋钮 | `aturn(input, on_event=None, cancel=None, max_steps=None, stream=None, parallel_tools=None)`（**形参**，不在 Config 里） | 同旧纯 Python 版 `loop.aturn`；`max_steps=None` = 不限，`stream=None` = 默认流式，`parallel_tools=None` = 跟随 `Config.parallel_tools` |

配套 `py.typed` + `pie/_pie_rs.pyi` 类型存根，把 dict 写成 `TypedDict`。

---

## 4. 结构方案

### 方案 A：最小侵入（推荐先做）

```
pie-rs/
├── Cargo.toml          # 仍是 package；新增 [lib] + feature gate
├── src/lib.rs          # 新增：pub mod config/llm/tools/session/context/cancel/log
│                       #        #[cfg(feature = "tui")] pub mod tui;
├── src/main.rs         # 变薄：use pie::{...}（lib 名 `pie`，bin 名仍是 `pie-rs`）
├── bindings/pie-py/    # 独立 crate：cdylib，path 依赖 pie-rs，default-features = false
└── python/             # pie 外壳包（__init__.py / .pyi / pyproject.toml / tests）
```

- `Cargo.toml`：`default = ["tui"]`，`tui = []`；`bindings/pie-py` 用 `default-features = false`。
- 优点：改动小（半天），`cargo test` / CLI 行为零变化；TUI 只在绑定时不编译。
- 缺点：TUI 那几个依赖（ratatui / crossterm / ratatui-textarea / tui-markdown / arboard）必须一并改成**可选依赖**（`tui = [...]`，`default = ["tui"]`），否则 `default-features = false` 也照样会把它们解析进依赖图。

### 方案 B：正式三分 workspace（长期）

```
crates/pie-core   # lib：config/llm/tools/session/context/cancel/log（无 TUI 依赖）
crates/pie-cli    # bin：main.rs + tui/
crates/pie-py     # cdylib：PyO3 绑定
```

- 优点：依赖边界硬隔离；将来 `pie-core` 可单独发 crates.io。
- 缺点：一次大搬家（所有 `crate::` 路径、`include_str!` 相对路径、测试分布全要动），
  且 `pie-rs/` 目前**还没进 git**（`git status` 显示 `?? pie-rs/`）——大重构前先提交，否则不可回退。

**建议：先 A，等 API 稳定、真要发 crates.io 时再做 B。** 两者对绑定代码的写法没有区别
（都是 `pie::session::Session` 这种路径），所以先 A 不会白干。

---

## 5. 关键技术决策

### 5.1 事件分发：**channel + 「持 GIL 的消费线程」**，而不是「tokio 线程里回调 Python」

`aturn` 要在大任务的中间推事件。两种做法：

| 做法 | 问题 |
|---|---|
| 在 tokio worker 里 `Python::attach` 直接调 Python 回调 | 需要跨线程 attach；回调若碰 `Session` → **死锁/借用冲突**；回调抛异常要跨 FFI 边界处理 |
| **（推荐）** Rust 侧把事件推进 `mpsc`（`TurnEvent` 已是 `Clone + Send`），Python 侧在 `aturn` 内**同步 recv 循环**，在持 GIL 的状态下调回调 | 只需一处锁；回调重入可控（见 §5.4）；同一个 channel 天然支持「生成器式」API |

代价：Rust 侧要 `allow_threads` 包住 recv（否则阻塞 Python 线程的同时还持 GIL → 别的线程全停）。
即：`py.allow_threads(|| rx.recv())` → 拿到事件 → 回 GIL → 调回调 → 再 `allow_threads`。循环体小、直白。

### 5.2 异步模型：**同步是默认入口，原生 async 是第二个入口**（不二选一，asyncio 放 M5）

**结论：能导出异步方法，而且两套入口不冲突**——核心本来就是 `async fn`，同步入口只是套一层 `block_on`。
现成方案是 `pyo3-async-runtimes`（`pyo3-asyncio` 的维护续作；当前 **0.29.0**，与 **pyo3 0.29.2** 配套）：
`future_into_py(py, async { … })` 把 tokio future 变成 Python 侧可 `await` 的对象。

```rust
#[pyfunction]
fn aturn_async(
    py: Python<'_>,
    session: PyRef<'_, Session>,
    input: String,
) -> PyResult<Bound<'_, PyAny>> {
    let sess = session.shared();                       // 内部 Arc<Mutex<Session>>
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        sess.lock().await.aturn(&input, &mut |_| {}, &Cancel::new()).await.map_err(to_py_err)
    })
}
```

**但 async 入口会新引入三个同步版没有的问题**——这才是把它放 M5 的真正原因：

| # | 问题 | 说明 | 做法 |
|---|---|---|---|
| ① | 事件流怎么给 | `on_event` 回调会在 tokio 线程上跑 → 又回到 §5.1 里被否掉的「跨线程 attach GIL 调 Python」，且回调抛异常要跨 FFI 边界处理 | 事件推给一个 Python `asyncio.Queue`（从 tokio 线程 `loop.call_soon_threadsafe(queue.put_nowait, ev)`），Python 侧 `async for ev in s.events()` —— **channel 的 async 版**，用户代码仍不在 tokio 线程里跑 |
| ② | 取消语义 | **`task.cancel()` 不会停 Rust 侧的 future**（tokio 任务照跑到结束）→ 用户以为停了、模型还在烧 token | 显式桥接：`tokio::select!` 监听 `Cancel`；另给 `s.stop()`，并让 `aturn_async` 的 awaitable 在收到 `CancelledError` 时触发同一个 `Cancel`（需要一层自定义 awaitable / `__del__`，是这块最容易错的地方） |
| ③ | loop / runtime 生命周期 | `future_into_py` 要求调用时处于运行中的 asyncio loop；同一进程两个 loop、或 `asyncio.run()` 跑两次 → runtime 与 loop 配对、退出时残留任务（`coroutine was never awaited` / loop closed 后才 resolve）都是常见故障 | 用 `pyo3_async_runtimes::tokio::get_runtime()`（**进程级** runtime，不随 loop 生死），而不是每个 loop 建一个；并文档化「一个进程一个 loop 最稳」 |

**为什么示例默认写同步**（不是因为做不到）：

1. 跑 agent 是「发起 → 等结果 → 期间没别的活」的形状，同步 + 回调就够；要并发就 `await asyncio.to_thread(s.aturn, …)`（一行），照样不冻解释器（§5.4 的 `allow_threads` 已放 GIL）。
2. 上面三条每条做错都是**挂死或静默失效**（取消不生效、事件丢），比同步版的 bug 难查一个量级。先把主链跑通、API 定形，再上 async。
3. 同步入口的实现（channel pump）在 async 版里**仍然是基础**（`asyncio.Queue` 就是它换了个出口）—— 先做不白干。

**M5 的目标形态**（feature gate `asyncio`，不影响同步路径）：

```python
async with pie.Session.new(cfg, llm, tools) as s:
    task = asyncio.create_task(s.aturn_async("写一篇长文"))   # 事件走 asyncio.Queue
    async for ev in s.events():
        print(ev["type"])
    answer = await task
    # task.cancel() / s.stop() 都能真的停住 shell 子进程与模型请求

### 5.3 借用与重入：`#[pyclass]` 内用 `Mutex`，锁不上就报错

- `Session`（Rust）的 `aturn` 要 `&mut self`，而回调期间 Python 可能回头调 `s.usage_report()`。
- 用 `Mutex<session::Session>`（**不是 `RefCell`**：`RefCell` 不是 `Sync`，且回调可能来自另一线程）
  + `try_lock()` 失败 → 抛 `RuntimeError("session 正忙（回合进行中）")`。
- 文档明写：**回调里不要碰同一个 Session**（其它 Session/纯函数随便用）。
- free-threading（3.13t+）：`allow_threads` 在 no-GIL 构建下是 no-op，互斥只能靠 `Mutex` —— 上面的设计恰好兼容。

### 5.4 GIL 纪律（正确性红线）

1. 任何「可能跑几十秒」的 Rust 调用（模型请求、工具执行、压缩）**必须** `allow_threads`，否则会冻住整个解释器。
2. 回调只做「把事件转成 Python 对象 + 调用户函数」，不在 Rust 侧持锁进回调。
3. 不用「持 GIL」当同步手段（那在 3.13t 下不成立）。

### 5.5 Python 定义的工具（M3）

`Entry` 现在长这样：

```rust
pub struct Entry {
    pub name: &'static str,
    pub description: String,
    pub parameters: Value,
    pub call: fn(Value, ToolCtx) -> BoxFuture<ToolResult>,
}
```

改造（对现有内置工具零行为变化）：

```rust
enum EntryImpl {
    // 原样：Rust 结构体工具（单态化函数指针）
    Static { call: fn(Value, ToolCtx) -> BoxFuture<ToolResult> },
    // 新增：Python 回调
    Python { callback: Py<PyAny>, arg_types: ..., /* 供返回/异常桥接 */ },
}
pub struct Entry { pub name: String, pub description: String, pub parameters: Value, pub imp: EntryImpl }
```

- `name: &'static str` → `String`（`names()` 返回值跟着变成 `Vec<&str>`，调用点小改）。
- 调用：`Python::attach` → 把 args（JSON → Python dict）传进去 → 结果 `str` 直接用 / `dict` 序列化成 JSON /
  其它类型 `str()`；Python 异常 → `ToolError(格式化文本)`（工具失败本来就文本化回传，不中断回合）。
- **schema 来源**两选一（先做①）：① 显式给 JSON schema；② `@tool` 装饰器从 `inspect.signature` +
  类型注解 + docstring 推导（对齐旧纯 Python 版 `@tool()` 的 `parameters` 推导）。
- 线程语义要写清：Python 函数在 **tokio worker 线程**上调用（持 GIL），阻塞式 I/O 会占住一个 worker；
  默认多线程 runtime 下可接受，重 I/O 的建议自己 await/线程池。

### 5.6 类型映射表（要点）

| Rust | Python | 备注 |
|---|---|---|
| `Option<T>` | `T \| None` | `reserved_tokens: Option<i64>`：`None` ↔ Python `None`；配置侧 `"auto"` 映射为 `None` |
| `CompactMode`（enum） | `str`（`"auto" \| "tools" \| "turns"`） | 解析失败抛 `ValueError` |
| `LlmError` / `ConfigError` | 异常层级：`PieError` → `LlmError`（带 `.status`）/ `ConfigError` / `ToolError` | `status()` / `retryable()` 暴露成属性 |
| `PathBuf` | `str`（`os.fspath` 兼容） | 读写都走 `os.PathLike` |
| `Message` / `Usage` / `TurnEvent` | `dict` | 字段名保持 JSONL 原名（`compress_level` 而非 `compressLevel`） |
| `Config.api_key` | ⚠️ `repr` 打码；默认值是**本部署真实 key** | 文档与 `Config.load()` 都要提示 |

### 5.7 路径与共享资产

- 继续用 `~/.pie/`（`sessions/` `context/` `files/` `memory.md` `config.toml`），
  用 `PIE_DIR` / `PIE_CONFIG_FILE` 重定向（与 CLI 完全一致）。
- 这意味着**旧 Python 版 / pie-rs CLI / 绑定**共享同一批会话文件 —— 已经在做的同格式契约，
  绑定侧只是多一个消费者；旧版写的会话文件现在仍能读（见 `python-legacy.md`）。

---

## 6. 打包与分发

- 构建后端 **maturin**（`pyproject.toml` + `[tool.maturin] module-name = "pie._pie_rs", features = ["pyo3/extension-module"]`）。
- **abi3**（`pyo3/abi3-py39`）→ 一个 wheel 覆盖 3.9+；若要 free-threading 支持需另出 `cp313t` wheel。
- 平台：macOS arm64/x86_64（本机）、Linux manylinux（rustls + ring，**无 C 依赖**，天然友好）、Windows msvc。
- 开发环：
  ```bash
  uv venv && uv pip install maturin
  cd pie-rs/bindings/pie-py && maturin develop --release      # 装进当前 venv
  uvx maturin build --release                                  # 出 wheel 到 target/wheels
  ```
- 本机注意（沿用 `README.md` 的环境备忘）：cargo 不在 PATH（用 `~/.cargo/bin`）；
  crates.io 走公司代理 MITM → `~/.cargo/config.toml` 的 `http.cainfo` / `http.proxy` 已配好，构建时置
  `CARGO_TARGET_DIR=~/.cache/pie-rs-target`（源码在 /mnt/d 时尤其必要）。
- 版本：`pie-rs` Cargo version = wheel version = `pie.__version__`（单一来源）。

---

## 7. 测试策略

| 层 | 做法 |
|---|---|
| Rust 单测 | 现有的 118 项保持不变（`cargo test`）——M0 的验收就是「一个不挂」 |
| 绑定单测（不联网） | `pie` 侧 `Config.base_url` 指向**本地假 SSE 服务器**：pytest 里用 `http.server` 回放固定 chunk（tool_calls → 文本 → `[DONE]`）→ 端到端跑 aturn |
| 工具/事件桥接 | 断言 `TurnEvent` 序列、Python 自定义工具被调用、异常 → `ToolError` 文本 |
| 契约测试 | 与旧纯 Python `pie` 包对拍：同一台假服务器、同一任务 → 相同工具调用与文件布局（**该包已于 2026-09-23 删除，此路不再可行**；工具 schema 逐字对拍那条先例也随 `fixtures/` 移除） |
| 共享资产 | 「Rust 写会话 → Python 读」/「Python 写 → Rust 读」双向（已有 session 契约测试，扩到绑定） |
| GIL 行为 | 起两条 Python 线程：一条跑 `aturn`（慢假服务器），另一条 `time.sleep` 计数 → 验证没被冻住 |

不需要 pty（无 TUI）。

---

## 8. 里程碑

| 里程碑 | 内容 | 验收 | 估时 |
|---|---|---|---|
| **M0 结构准备** | `src/lib.rs` + `tui` feature；`main.rs` 改薄（`use pie::…`）；`bindings/pie-py` 骨架 + `#[pymodule]` 空模块 | `cargo test` 全绿、CLI 行为不变、`import pie` 成功 | 0.5d |
| **M1 最小闭环** | `Config` + `LlmClient` + `ToolRegistry::builtins` + `Session`（`new`/`load`/`resume`/`ephemeral`/`aturn`/`save`）+ 事件回调 + 错误层级 | 一段 Python 脚本跑通「提问 → 模型调 `read` → 打印答复」；pytest 走假服务器 | 2–3d |
| **M2 API 补齐** | `run()`、`Cancel`、`compact`/`compression_history`/`full_history`/`usage_report`/`reset`、`list_sessions`、`Config` 属性 + `to_dict`/`update`、`PIE_DIR` 支持 | 能力对齐 CLI；上述对象都有回归用例 | 1–2d |
| **M3 Python 工具** ✅ | `Entry` 动态变体 + `register()` + `@tool`（签名→schema）+ 返回值/异常桥接 + 线程语义文档 | Python 工具被模型调用、结果回传、异常文本化；与内置工具同列表 | 2d |
| **M4 打包** | maturin + abi3 + `py.typed` / `.pyi` + README 示例 + `__version__` | 干净 venv 里 `pip install` wheel 可用 | 1–2d |
| **M5** ✅ | `aturn_async` + `events()`（`pyo3-async-runtimes 0.29`，feature gate `asyncio`；三个坑见 §5.2） | `await` 版本可用、**`task.cancel()` 真能停住**、不破坏同步路径 | 2–4d |
| M5 剩余 | free-threading wheel；PyPI/CI | — | 按需 |

依赖顺序：M1 之后（M2 与 M3 可并行）；M4 最早在 M1 后可先做（便于分发试用）。

---

## 9. 风险清单

| 风险 | 影响 | 对策 |
|---|---|---|
| GIL 死锁 / 回调重入 | 卡死或 panic 穿过 FFI（进程崩） | §5.4 纪律 + `Mutex::try_lock` + 回调里禁碰同一 Session + 有专门回归 |
| 阻塞式 Python 工具占住 tokio worker | 并发下降、看似卡住 | 多线程 runtime + 文档；必要时 `spawn_blocking` |
| 类型面漂移（Rust 字段改了、Python 没跟上） | 静默错误 | dict-first（JSON 自动同步）+ `.pyi` 由 Rust 测试对拍字段名 |
| `pie-rs/` 未进 git | 大重构（方案 B）不可回退 | **M0 前先 `git add pie-rs/`** |
| 与旧 Python 版行为分叉 | 用户预期落差 | 文档明确「有意差异」清单（现收在 `docs/python-legacy.md`） |
| PyO3 版本 API 变动（如 `Python::with_gil` → `Python::attach`） | 编译不过 | 动手时以锁定版本的文档为准，别照抄过时示例 |
| abi3 + free-threading 目标不同 | wheel 矩阵变大 | 先只出 abi3 常规 wheel，3.13t 按需 |

---

## 10. 待拍板的点

**M1 已按推荐默认开工**（括号里是实际采取的选项，要改现在改最便宜）：

1. **包名 / 模块名**：`pie_rs`（扩展模块 `pie._pie_rs`）？还是叫 `pie.rust` / 别的？
   → 先用了 `pie_rs`，**2026-09-23 用户点名改成 `pie`**（外壳包 `pie/`、扩展模块 `pie._pie_rs`、
   发行名也改成 `pie`）—— 曾与旧纯 Python 版同名（那个实现已删除，现无冲突）。
2. **异步**：同步为主 + M5 补原生 async（推荐，见 §5.2；两者共用同一个内部实现，不冲突），
   还是 M1 就直接上 asyncio（少一轮 API 定形，但前面三周的 bug 面更大）？ → 已按 **同步为主**。
3. **事件形态**：回调 + 迭代器都给（草案），还是只给回调？ → 已只给 **`on_event` 回调**（迭代器 / `async for` 留给 M5）。
4. **Config 形态**：dict-first（`to_dict`/`update` + 少量属性）还是逐字段属性？
   → 已用 **常用字段属性**（`to_dict`/`update` 放 M2；核心 `Config` 没实现 Serialize，做它要先给核心加）。
5. **M3（Python 自定义工具）是否进第一版**？ → 暂不进（M1 只内置四件套）。
6. **结构**：方案 A（lib.rs + feature，最小侵入）还是直接上方案 B（三分 workspace）？ → 已用 **方案 A**。
7. **分发范围**：本机 `maturin develop` 自用，还是要 wheel 分发 / 进 CI？ → 暂本机自用（abi3 wheel 是 M4）。

---

## 11. 实现时定的东西（M0 / M1，写完才知道的）

### 结构（方案 A 落地）

- `Cargo.toml`：`[lib] name = "pie"`（2026-09-23 由 `pie_rs` 改名）；`[[bin]] required-features = ["cli", "tui"]`；
  `default = ["cli", "tui"]`。`cli` = `dep:clap`（只有 bin 用）；`tui` = ratatui / crossterm /
  ratatui-textarea / tui-markdown / arboard / unicode-width 这 6 个 **optional** 依赖。
- **TUI 依赖必须一起转 optional**（光 gate 模块不够：依赖仍会进解析图）→ 验收方式：
  `cargo build --no-default-features` 后 `cargo tree -e normal` 里搜不到 ratatui / crossterm / arboard / tui-markdown。
- `main.rs` 改成 `use pie::{…}`（**别再写 `mod xxx;`** —— 那会变成第二份独立的编译单元，
  两边的类型不兼容）。
- `bindings/pie-py`：自己的 workspace，被父级 `exclude = ["bindings"]` 排除；
  cdylib、`[lib] name = "_pie_rs"`（maturin 的 `module-name = "pie._pie_rs"`），
  `pie-rs = { path = "../..", default-features = false }`。
- 核心为绑定加的两处：`tools::Entry` / `tools::ToolRegistry` 加 `#[derive(Clone)]`
  （绑定要拿副本建会话；`LlmClient` / `Config` 本来就 Clone）。

### PyO3 0.29 的 API 坑（都是编译期踩出来的）

1. `Python::with_gil` / `allow_threads` 现在叫 **`Python::attach` / `py.detach`**。
2. `detach` 的闭包要 `Ungil`。stable 下 `Ungil` 就是 `Send`，但**按引用捕获**的闭包还要被捕获类型
   是 `Sync` —— 而 `mpsc::Receiver` 是 `Send + !Sync` → `py.detach(|| rx.recv())` **编译不过**。
   写法：把 receiver **按值**进闭包再带出来：
   `let mut rx = rx; loop { let (next, ev) = py.detach(move || { let e = rx.recv(); (rx, e) }); rx = next; … }`。
3. `bool::into_pyobject` 给的是 `Borrowed<PyBool>`（Python 里 bool 是单例）→ 要 `.to_owned()`
   再 `.into_any().unbind()`；`i64` / `f64` / `&str` 直接给 `Bound`。
4. 给异常实例挂属性（如 `.status`）：`PyErr::new_err` **拿不到实例** → 用
   `py.get_type::<LlmError>().call1((msg,))` 造实例 → `setattr` → `PyErr::from_value(inst)`。
5. **`cargo build` 直接编 cdylib 在 macOS 会报一堆 `_PyBaseObject_Type` undefined**（扩展模块本来就该
   留着符号不解析）→ 用 `maturin develop` / `maturin build`，别手搓 cargo 产物去当扩展模块。

### 回合 pump 的形状（§5.1 的落地）

- 会话锁 = `Arc<tokio::sync::Mutex<Session>>`；`aturn` 开始时 `try_lock_owned()`（拿到就移进
  `spawn` 的任务里跨 await 持有，所以必须 tokio 的锁，不是 `std::sync::Mutex`）。
- 忙 → 直接 `RuntimeError("session 正忙")`（**不排队**：排队会在「回调里碰同一个 session」时死锁）。
- 事件：`mpsc` + 调用线程 pump（`py.detach` 等事件、回 GIL 调用户回调）——**用户代码永远不在 tokio 线程上跑**。
- 回调抛异常：记下来，**让回合跑完**再抛回去（半路撤会把历史写坏）；这条要有用例。
- `s.stop()`：把当前回合的 `Cancel` 存在 `current: Mutex<Option<Cancel>>` 里供别的线程触发。

### 与旧 Python 版不一致的地方（照实暴露，不偷偷改）

- 核心 **`Config::default()` 里 compaction 是开着的**（`Some(CompactionConfig::default())`）
  → 绑定的 `cfg.compaction` 布尔属性默认 **True**；旧 Python 版是「不写 `[compaction]` 就不压」。
  绑定照实反映默认值（要一致就让核心默认改 `None`，但那是另一件事）。
- `CompactStats` **没有实现 Serialize**（核心只在内部用）→ 绑定里手工拼 dict；**加字段要两处都改**。
- `Config` **没实现 Serialize** → `to_dict()` 要等给核心加上（或手写每个字段），所以 M1 只给了常用属性。

### 测试与环境

- 绑定回归 = pytest + **本地假 SSE 端点**（`http.server`，按 `messages[-1].role` 决定回工具调用还是最终答复）；
  一条不联网。`PIE_DIR` 指到 tmp 目录 → 不碰真实 `~/.pie`（`ephemeral` 不落盘另有断言）。
- `.gitignore` 加了 `pie-rs/bindings/*/target/`（这个 crate 有自己的 target/）。
- 绑定侧 dev 环境：`bindings/pie-py/.venv`（maturin + pytest）。

### M5：asyncio 入口（2026-09-23）

API 形状（§5.2 的目标形态）：

```python
task = asyncio.create_task(s.aturn_async("写一篇长文"))   # 事件走 asyncio.Queue
async for ev in s.events():
    print(ev["type"])
answer = await task
```

- **Rust 侧只留三个小口**（`#[cfg(feature = "asyncio")]`，默认开）：
  `Session.aturn_async()`（建队列 + 登记 + 转交给 Python 胶水）、`Session.turn_future(input, sink, …)`
  （`pyo3_async_runtimes::tokio::future_into_py`，回合跑在**进程级** runtime 上）、`Session.events()`
  （返回 `pie._async._Events`）。队列在 `aturn_async` 里**同步**建好并登记 → 返回后 `s.events()`
  立刻可用（不用先 await 一下让协程跑起来）。
- **胶水在 Python 侧**（`pie/_async.py`）——三条坑逐条对着 §5.2 办：
  1. **事件怎么给**：`sink` 从 tokio 线程被调用，只做 `loop.call_soon_threadsafe(queue.put_nowait, ev)`
     （`asyncio.Queue` 不是线程安全的）；回合结束时推一个 `None` → 胶水换成哨兵 → `async for` 自然结束。
     用户代码**仍然不在 tokio 线程上跑**。
  2. **取消**：`task.cancel()` 不会停 tokio 任务 → 胶水捕 `CancelledError` 后调 `session.stop()`。
     实测：模型请求挂着 3s，`task.cancel()` 后 **0.30s** 返回，且会话立刻还能用（取消没弄坏历史）。
  3. **loop / runtime 生命周期**：runtime 是进程级的，不随 loop 生死；**一个进程一个 loop 最稳**
     （写进模块 docstring）。
- 依赖：`pyo3-async-runtimes 0.29`（feature 名是 **`tokio-runtime`**，不是 `tokio`）+ optional，
  `default = ["asyncio"]`。
- 回归：`events()` 早调报错、`aturn_async` 的完整事件序列 + 答复、取消（含取消后会话仍可用）。

### M3：Python 工具（2026-09-23）

- **核心一侧**：`Entry` 的名字改 `String`、调用体换成 `CallFn = Arc<dyn Fn(Value, ToolCtx) -> BoxFuture<ToolResult>>`
  （内置工具包 `erased::<T>` 的 fn 指针，Python 工具包绑定给的回调）→ **两条路共用同一条分发链**，
  模型看不出区别；`Debug` 手写（`dyn Fn` 不是 Debug）。
- **schema 在 Python 侧算**（`pie/_tool.py` 的 `tool()` / `_type_to_schema`，与旧纯 Python 版**逐字同款**）：
  实测同一批签名两边产出的 `definition()` **逐字相同**（`str/int/float/bool/list/dict/Optional`；
  下划线参数不进 schema；描述取 `description` → docstring 首行 → 函数名）。
- **注册口是 `ToolRegistry.register(...)`**（写在 Rust 侧，因为原生类不能从 Python 加方法）：
  两种用法 `register(pie.tool()(fn))` / `register(name=…, handler=…, description=…, parameters=…)`。
  注册期就拦三类错：**名字不合法**（API 要求 `[A-Za-z0-9_-]{1,64}`）、**重名**、**async handler**（要等 M5）。
- **调用体**：tokio worker 上 `Python::attach` 回 GIL → 参数按**关键字**传给 handler → 返回值
  必须 `str` → 抛异常变成 `ToolError`（核心的 `dispatch_tool` 文本化成 `[工具错误] …`，回合照跑）。
  三条纪律（**写进 docstring**）：同步函数、别在 handler 里等别的线程、**别碰同一个 Session**。
- 回归：schema 对拍 + 端到端（模型调起来它、参数解析、结果回传）+ 异常文本化 + 三类注册期校验。

### M2 补齐 + 类型存根（2026-09-23）

- **补齐的 API**：模块级 `run(task, config=None, llm=None, tools=None, max_steps=…, stream=…,
  parallel_tools=…)`（内部就是「建临时会话 → `aturn`」，**无会话不落盘**）、`list_sessions(limit=None)`
  （键名与 CLI `sessions --json` 一致）、`Config.to_dict()` / `update(dict)`、
  `Session.clear_window()` / `set_model()` / `set_reasoning_effort()` / `config`（快照）。
- **`to_dict` / `update` 复用核心的 `Config::to_toml()`**（为此把它从 private 改成 `pub`）：
  字段列表只有那一份，绑定侧不再抄一遍。`update` 的语义是「当前值 → 打补丁 → 再 deserialize 一遍」
  → 归一/校验与读配置文件同一条路；**未知键直接报错**（打错字不会静默失效）；运行时字段
  （`config_file` / `system_prompt` / …）原样保留。
- **类型存根** `python/pie/_pie_rs.pyi`：dict 形状写成 `TypedDict`（`TurnEvent` 是判别联合 →
  `ev["type"] == "tool_call"` 能窄化出 `arguments`）。三条对拍用例防漂移：`__all__` ⊆ 存根、
  类上的公开属性双向对拍、事件 dict 的键 ⊆ 对应 TypedDict。
  - ⚠ `__init__.py` 里再导出**类型名**要用 `if TYPE_CHECKING:` + **`X as X`** 写法：
    `--strict` 下 mypy 不认隐式再导出（普通 `from … import X` 在外面会「未定义」）；
    同时**别放进 `__all__`**（那会让运行时的 `import *` 炸，这些名字只在 `.pyi` 里存在）。
- 验收：`mypy --strict` 干净（含判别联合的 `reveal_type` 窄化），pytest 20 例全绿。

