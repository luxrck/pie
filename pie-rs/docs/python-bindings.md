# pie-rs：Python 绑定规划（PyO3 / maturin）

> 状态：**规划稿**（尚未动代码）。目标是把 `pie-rs` 的 harness 能力以原生扩展的形式给 Python 程序调用，
> 与已有的纯 Python 包 `pie`（Textual TUI 那套）**并存**，不是替换它。
>
> 相关：`README.md`（迁移进度）、`../MEMORY.md`（决策记录）。

---

## 1. 目标与非目标

### 目标

- Python 侧能完成「配一个模型 → 建工具集 → 开会话 → 丢一个任务 → 收事件/收答复」的完整闭环，
  能力对齐纯 Python 版的 `run` / `aturn` / `Session` / `Config` / `ToolRegistry`。
- 会话文件、上下文压缩、图片 Files API 这些**跨语言共享**的资产继续共用 `~/.pie/`（两边已同格式）。
- 用 Python 写自定义工具注册进 harness（对齐 Python 版 `@tool()` 的体验）。
- 可分发的 wheel（maturin 构建），至少覆盖本机 macOS，其次 Linux / Windows。

### 非目标

- **TUI 不进绑定**：`src/tui/`（ratatui / crossterm / tui-markdown / arboard）不暴露、不依赖。
  交互界面继续走 `pie-rs` 二进制。
- 不追求 1:1 覆盖 Python 版 `__all__`（31 项）；先覆盖「跑 agent」这条主链，其余按需补。
- 不保证与 Python 版**行为逐字一致**——Rust 版已有几处有意差异（不回退内联 base64、shell 超限保留头部、
  Toast 时长等），绑定继承 Rust 行为，文档需明示（对齐 `README.md` 的「已知差异」一节）。

---

## 2. 现状盘点：四个必须先解决的障碍

| # | 现状 | 影响 |
|---|---|---|
| 1 | **纯 bin crate**：没有 `src/lib.rs`，`Cargo.toml` 只有 `[[bin]]`，`mod config;` 等全是私有 | 外部 crate 拿不到任何 API → 第一步必须提出库 |
| 2 | `mod tui;` 与核心模块**在同一个 crate**，TUI 依赖（ratatui/crossterm/arboard…）是必选依赖 | 绑定会连带编译 TUI（慢、平台依赖）→ 需要 feature gate 或拆 crate |
| 3 | **async-first**：`Session::aturn` / `LlmClient::complete` 都是 `async fn`，需要 tokio runtime | Python 侧要处理 runtime 与 GIL 的关系（见 §5.2） |
| 4 | 工具是**静态泛型**：`with_tool::<T>(name)` 单态化成函数指针 `fn(Value, ToolCtx) -> BoxFuture`，`Entry.name` 还是 `&'static str` | 运行期从 Python 注册工具没有通道 → 需要给 `Entry` 加一个动态变体（见 §5.5） |

另外两个既成事实要利用好：

- `Message` / `Config` 等核心类型**已经全部 serde**（`Serialize + Deserialize`，字段名与 Python 版 `to_dict()` 逐字对齐）
  → 跨语言桥接优先走 JSON/dict，不必写逐字段转换。
- `Session::aturn(&mut self, input, on_event: &mut (dyn FnMut(TurnEvent) + Send), cancel: &Cancel)` 的形状
  天然适合「事件推 channel + 主线程分发」的绑定方式（见 §5.3）。

---

## 3. 目标 API 草案（Python 侧长什么样）

```python
import pie_rs

cfg = pie_rs.Config.load()                 # ~/.pie/config.toml（等价于 CLI 的 -c）
cfg.model = "deepseek-flash"               # 内存覆盖，不写盘（与 CLI 覆盖项同语义）
cfg.reasoning_effort = "high"

llm   = pie_rs.LlmClient(cfg)
tools = pie_rs.ToolRegistry.builtins(cfg)  # read / edit / write / shell

# —— 会话 ——
s = pie_rs.Session.new(cfg, llm, tools)          # 或 Session.load(path, ...) / Session.resume(...)
# s = pie_rs.Session.ephemeral(cfg, llm, tools)  # 不落盘、不写 manifest（服务 / notebook 用）

# ① 回调式（阻塞到回合结束，返回最终答复）
s.aturn("读一下 README 的前 20 行", on_event=lambda ev: print(ev.type, ev))

# ② 迭代器式（生成器，边跑边取事件；底层同一个 channel）
for ev in s.aturn_stream("同样的问题"):
    if ev.type == "assistant_text":
        print(ev.text, end="")

print(s.messages[-1]["content"])     # 历史是 dict 列表（字段名 = JSONL 字段名）
print(s.usage_report())              # /stat 那段文本
s.compact("tools"); s.reset(); s.save()

# —— 取消 ——
tok = pie_rs.Cancel()
threading.Thread(target=lambda: s.aturn("写一篇长文", on_event=..., cancel=tok)).start()
tok.cancel()

# —— 一次性 ——
print(pie_rs.run("总结这个仓库", cfg=cfg))      # 无会话、不落盘

# —— 自定义工具（M3） ——
@pie_rs.tool(name="fetch", description="抓一个 URL 的正文")
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
| `TurnEvent` | dict（`{"type": "tool_call", "name": …, "arguments": …}`） | 与 Python 版 `on_event` 的 dict 形状**一致**，两边代码可移植 |
| `Usage` / `CompactStats` | dict | 同上 |
| `Config` 的字段 | `to_dict()` / `update(dict)` + 少量 `#[getter]`/`#[setter]`（model / base_url / reasoning_effort / context_window / reserved_tokens / stream / max_steps / compaction / tools / tui） | 全字段属性太脆（Rust 结构体会变）；常用项给属性，其余走 dict |

配套 `py.typed` + `pie_rs/_pie_rs.pyi` 类型存根，把 dict 写成 `TypedDict`。

---

## 4. 结构方案

### 方案 A：最小侵入（推荐先做）

```
pie-rs/
├── Cargo.toml          # 仍是 package；新增 [lib] + feature gate
├── src/lib.rs          # 新增：pub mod config/llm/tools/session/context/cancel/log
│                       #        #[cfg(feature = "tui")] pub mod tui;
├── src/main.rs         # 变薄：use pie_rs::{...}（bin 与 lib 同名同目录，cargo 允许）
├── bindings/pie-py/    # 独立 crate：cdylib，path 依赖 pie-rs，default-features = false
└── python/             # pie_rs 外壳包（__init__.py / .pyi / pyproject.toml / tests）
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
- 缺点：一次大搬家（所有 `crate::` 路径、`include_str!` 相对路径、`fixtures/`、测试分布全要动），
  且 `pie-rs/` 目前**还没进 git**（`git status` 显示 `?? pie-rs/`）——大重构前先提交，否则不可回退。

**建议：先 A，等 API 稳定、真要发 crates.io 时再做 B。** 两者对绑定代码的写法没有区别
（都是 `pie_rs::session::Session` 这种路径），所以先 A 不会白干。

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

### 5.2 异步模型：**Phase 1 只做同步**（`block_on`），asyncio 放 M5

- `LlmClient` 内部持一个**进程级、常驻的 tokio runtime**（`OnceLock<Runtime>`，`rt-multi-thread`），
  入口统一 `py.allow_threads(|| RUNTIME.block_on(fut))`。
- 好处：不用把 Python 事件循环与 tokio 绑一起（`pyo3-async-runtimes` 那套：每个 asyncio loop 要配一个
  runtime、跨 loop 复用会炸、取消语义要对齐）。这一层最容易出难查的 bug，先不引。
- asyncio 用户的第一版出路：`await asyncio.to_thread(s.aturn, ...)`（文档里给示例）。
- M5 若真要原生 async：`aturn_async()` → `future_into_py`，**feature gate**，不破坏同步路径。

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
  类型注解 + docstring 推导（对齐纯 Python 版 `@tool()` 的 `parameters` 推导）。
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
  用 `PIE_DIR` / `PIE_CONFIG_FILE` 重定向（与 Python 版、CLI 完全一致）。
- 这意味着**Python（纯 Python 版）/ pie-rs CLI / 绑定**三方共享同一批会话文件 —— 已经在做的同格式契约，
  绑定侧只是多一个消费者；回归里要有一条「Rust 写的会话 Python 读得到」（已有先例可扩）。

---

## 6. 打包与分发

- 构建后端 **maturin**（`pyproject.toml` + `[tool.maturin] module-name = "pie_rs._pie_rs", features = ["pyo3/extension-module"]`）。
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
- 版本：`pie-rs` Cargo version = wheel version = `pie_rs.__version__`（单一来源）。

---

## 7. 测试策略

| 层 | 做法 |
|---|---|
| Rust 单测 | 现有的 118 项保持不变（`cargo test`）——M0 的验收就是「一个不挂」 |
| 绑定单测（不联网） | `pie_rs` 侧 `Config.base_url` 指向**本地假 SSE 服务器**：pytest 里用 `http.server` 回放固定 chunk（tool_calls → 文本 → `[DONE]`）→ 端到端跑 aturn |
| 工具/事件桥接 | 断言 `TurnEvent` 序列、Python 自定义工具被调用、异常 → `ToolError` 文本 |
| 契约测试 | 与纯 Python `pie` 包对拍：同一台假服务器、同一任务 → 相同工具调用与文件布局（已有 `fixtures/python-tools.json` 先例） |
| 共享资产 | 「Rust 写会话 → Python 读」/「Python 写 → Rust 读」双向（已有 session 契约测试，扩到绑定） |
| GIL 行为 | 起两条 Python 线程：一条跑 `aturn`（慢假服务器），另一条 `time.sleep` 计数 → 验证没被冻住 |

不需要 pty（无 TUI）。

---

## 8. 里程碑

| 里程碑 | 内容 | 验收 | 估时 |
|---|---|---|---|
| **M0 结构准备** | `src/lib.rs` + `tui` feature；`main.rs` 改薄（`use pie_rs::…`）；`bindings/pie-py` 骨架 + `#[pymodule]` 空模块 | `cargo test` 118 全绿、CLI 行为不变、`import pie_rs` 成功 | 0.5d |
| **M1 最小闭环** | `Config` + `LlmClient` + `ToolRegistry::builtins` + `Session`（`new`/`load`/`resume`/`ephemeral`/`aturn`/`save`）+ 事件回调 + 错误层级 | 一段 Python 脚本跑通「提问 → 模型调 `read` → 打印答复」；pytest 走假服务器 | 2–3d |
| **M2 API 补齐** | `run()`、`Cancel`、`compact`/`compression_history`/`full_history`/`usage_report`/`reset`、`list_sessions`、`Config` 属性 + `to_dict`/`update`、`PIE_DIR` 支持 | 能力对齐 CLI；上述对象都有回归用例 | 1–2d |
| **M3 Python 工具** | `Entry` 动态变体 + `register()` + `@tool`（签名→schema）+ 返回值/异常桥接 + 线程语义文档 | Python 工具被模型调用、结果回传、异常文本化；与内置工具同列表 | 2d |
| **M4 打包** | maturin + abi3 + `py.typed` / `.pyi` + README 示例 + `__version__` | 干净 venv 里 `pip install` wheel 可用 | 1–2d |
| **M5 可选** | `aturn_async`（`pyo3-async-runtimes`，feature gate）；free-threading wheel；PyPI/CI | `await` 版本可用且不破坏同步路径 | 2–3d |

依赖顺序：M1 之后（M2 与 M3 可并行）；M4 最早在 M1 后可先做（便于分发试用）。

---

## 9. 风险清单

| 风险 | 影响 | 对策 |
|---|---|---|
| GIL 死锁 / 回调重入 | 卡死或 panic 穿过 FFI（进程崩） | §5.4 纪律 + `Mutex::try_lock` + 回调里禁碰同一 Session + 有专门回归 |
| 阻塞式 Python 工具占住 tokio worker | 并发下降、看似卡住 | 多线程 runtime + 文档；必要时 `spawn_blocking` |
| 类型面漂移（Rust 字段改了、Python 没跟上） | 静默错误 | dict-first（JSON 自动同步）+ `.pyi` 由 Rust 测试对拍字段名 |
| `pie-rs/` 未进 git | 大重构（方案 B）不可回退 | **M0 前先 `git add pie-rs/`** |
| 与纯 Python 版行为分叉 | 用户预期落差 | 文档明确「有意差异」清单（README 已有），绑定的 docstring 里复述 |
| PyO3 版本 API 变动（如 `Python::with_gil` → `Python::attach`） | 编译不过 | 动手时以锁定版本的文档为准，别照抄过时示例 |
| abi3 + free-threading 目标不同 | wheel 矩阵变大 | 先只出 abi3 常规 wheel，3.13t 按需 |

---

## 10. 待拍板的点

1. **包名 / 模块名**：`pie_rs`（扩展模块 `pie_rs._pie_rs`）？还是叫 `pie.rust` / 别的？
2. **异步**：先只做同步（推荐）+ `to_thread` 示例，还是一上来就做 asyncio？
3. **事件形态**：回调 + 迭代器都给（草案），还是只给回调？
4. **Config 形态**：dict-first（`to_dict`/`update` + 少量属性）还是逐字段属性？
5. **M3（Python 自定义工具）是否进第一版**？不进的话 M1+M2+M4 ≈ 一周内可用。
6. **结构**：方案 A（lib.rs + feature，最小侵入）还是直接上方案 B（三分 workspace）？
7. **分发范围**：本机 `maturin develop` 自用，还是要 wheel 分发 / 进 CI？
