# CHANGELOG — 关键决策与变更记录

本文件按时间倒序记录 pie 的关键设计决策与实现变更。决策的「当前状态」摘要保留在仓库根目录 `MEMORY.md`。

## 2026-09-24

- **时间子系统坍缩成一个时钟出口 `fn now() -> Duration`**（用户：选 A）。前提：全仓**没有任何一处把时间串解析回时间**（唯一「读」是原样打印 / 转发），所以统一成数字几乎零风险。
  - `config` 里 `now_unix` / `iso_utc` / `iso_local` / `fmt_unix_ts` 四个函数塌缩成：`now() -> Duration`（**唯一碰 `SystemTime` 的地方**，秒 / `subsec_micros` / `subsec_nanos` 都从它取）+ `fmt_local(secs)`（展示，分钟精度）+ 私有的 `civil`（纯函数，Hinnant 的 civil_from_days）与 `local_utc_offset`（只服务展示）。`session::timestamp()` 收编进 `now()`，`llm::retry_delay` 的 jitter、`collect_file_garbage` 的 age 比较也改用它（原先各自裸调 `SystemTime::now()`）。
  - **落盘时间统一 unix 秒数字**（用户上一轮点名要的）：manifest 的 `ts`、`__meta__.files[].uploaded_at` 由 ISO 串改成 `now().as_secs() as i64`，`iso_utc` / `iso_local` 随之删除。旧 manifest 里的 ISO 串**不解析**，`pie context info` 原样打印。
  - 破坏性（对外）：`compression_history()` 及各事件里的 `ts` / `uploaded_at` 从 ISO 串变数字。
  - 测试：原先依赖本机时区的旧断言（`iso_utc_matches_known_instants`，住在 `context.rs`）删掉，改成 `config.rs` 里测纯函数 `civil`（含负值 `-1 → 1969-12-31T23:59:59`）+ `fmt_local` 只断言形状（core 仍 166 例）。

- **`tool_call` 事件删掉 `turn` / `step`**（用户：选 B）：核实下来这两个值在仓库内**没有任何活着的读者**——
  - `turn`（历史里非 synthetic 的 user 消息数）在一次 `aturn` 里恒定，消费方自己数就行；`step` 同理能从事件流里推。
  - Rust 侧唯一的读者是 CLI 一次性模式那条 `[t{turn}s{step}] …` stderr 日志，而它**跑不到**：`main.rs` 在装 printer 之前就 `config.verbose = false` 了（`ToolResult` 的 `[tool] …←…` 同一条命运）。TUI 本来就忽略（`ToolCall { name, arguments, .. }`）。
  - 绑定侧只是把它俩塞进事件 dict + `.pyi` + 一条断言 → 一起去掉。**这是对外事件形状的破坏性变更**（Python 侧 `ev["turn"]` 不再存在）。
  - 顺带：`Session::tool_call(calls, parallel, cancel, on_event)` 少两个形参，`aturn` 里那 6 行派生逻辑删掉。（这是在**反转** 2026-09-15 那次「保留 `turn`，因为嵌入方可能有用」的决定：至今没长出一个消费方。）
- **`Config.verbose` 整个删掉**（用户要求）：它只剩两处门控，删 `verbose` 后都没有意义了——
  - 一次性模式那两条日志（上一条里说的死分支）跟着删，printer 只处理 `AssistantText` / `Reasoning` / `Answer`；
  - `open_session` 的 `[session] 恢复/载入/新建` 横幅删掉（TUI 进交替屏后本来就看不见；会话模式无任务时那条 `[session] <path>（摘要）` 照旧打印）；
  - 「压缩省了多少 token」的提示改成**无条件** `log::warn`（TUI 里仍是消息流一条 `· …`，CLI 里进 stderr——原来 `verbose = true` 时就是这么走的）。
  - 连带：配置文件少一个键（老配置里的 `verbose = true` 会被忽略，不报错）、绑定的 `Config.verbose` 属性与 `.pyi` 声明一起删。

## 2026-09-23

- **仓库转纯 Rust**：Python 实现（`src/pie/*.py` + `tests/*.py` + `pyproject.toml` + `uv.lock`）整体删除，`pie-rs/` 的内容上提到仓库根（`src/`、`prompts/`、`bindings/`、`docs/`）。Python 版从此只是历史参照（`git show b188058^:src/pie/…`）。
- **新增 `pie setup` 子命令**（用户要求）：把 `~/.pie/` 下缺的默认件补齐 —— 默认配置文件 + 全局记忆种子（`prompts/memory.md`）。
  - **非交互**：Python 版那个 `setup` 是逐个问答模型/地址/key 的向导；这边只写默认值（默认值唯一来源就是 `Config::default()`），之后自己改。
  - **幂等、不覆盖**：已存在的文件原样保留（里面可能有用户自己的 key 与记忆），所以可以反复跑。两个助手函数 `config::ensure_config_file` / `config::ensure_global_memory_file` 都返回 `(路径, 是否新建)`，命令据此报「已创建 / 已存在」。
  - 位置：`run()` 里**早于** `Config::load` 与启动时那发记忆种子（配置缺失/坏掉正是它要修的场景；也保证 memory.md 的「已创建」是真的，不会被启动时的隐式种子抢先）；`-c` / `PIE_CONFIG_FILE` / `PIE_DIR` 照旧生效。
  - 回归：`ensure_config_file_writes_defaults_once_and_keeps_existing`（含父目录不存在要先建、写下来的默认值要能读回）与 `ensure_global_memory_reports_whether_it_created_the_file`。
- **删掉与 Python 逐字对拍的契约测试 + `fixtures/`**（用户要求）：`generated_specs_match_python` 及其辅助（`canonical` / `python_name` / `with_python_names`）与基准 `fixtures/python-tools.json` 一起移除，只留 `builtin_tool_names` 钉住注册名与顺序。代价：工具描述/参数再与 Python 分叉就没有自动拦网了。
- **修回 `prompts/system.md` 的大小写**：上次搬家把它改成了 `SYSTEM.md`，而代码是 `include_str!("../prompts/system.md")` —— macOS 大小写不敏感照样编过，**Linux/WSL 上会直接编译失败**。
- **参数命名统一 `cfg` → `config`**（用户点名）：拿 `Config` 当参数/局部变量时一律叫 `config`（含 `context.rs` 的 `ToolCompaction` / `SessionCompaction` 与测试里的 `let cfg = …`）；绑定内部从 `PyConfig` 取出的核心配置叫 `core_config`（免得与 Python 侧参数名 `config` 撞）。纯改名，无行为变化（core 166 例 + 绑定 27 例照旧）。
- **修掉绑定里过期的 `[exit=0]` 断言**：`test_turn_runs_tool_and_streams_events` 还按老协议断言成功命令的结果以 `[exit=0]` 开头，而协议早已改成「失败才给头」→ 现改成断言正文 `hi\n`（这条失败与本次改名无关，是上次改协议后漏改的）。

## 2026-09-22

- **`aturn` / `run` 新增 `stream: bool | None = None`**（用户要求：给外部嵌入方手动控制流式）：
  - **None（默认）= 原行为**（后端实现了 `stream()` 就走流式，否则 `complete()`）；`False` = 强制一次性 `complete()`（此时 `on_event` 不再收到 `reasoning_delta` / `content_delta`，其余事件不变）；`True` = 强制流式。
  - 后端**没有** `stream()` 时 `stream=True` 不报错，仍回退 `complete()`（嵌入方不用先探测后端能力）。
  - 实现只动一处：`_model_call(..., stream=...)` 的判据从 `inspect.isasyncgenfunction(backend.stream)` 改成 `can_stream and stream is not False`（file_id 失效后的那次重试同样带上该参数）。
  - 回归：新增 `tests/test_loop.py`（替身后端记录走的是 `stream` 还是 `complete`，覆盖三态 + 无 stream 后端回退 + `run()` 签名透传；`Config(api_key="", compaction=None, files_api=False)` 保证零网络、不碰 `~/.pie`）。

## 2026-09-21

- **输入框滚动条样式与 `#log` 统一**（用户提出：输入框的滚动条又宽又蓝，和 #log 的不是一个东西）：`build_css` 里那份 `scrollbar`（`scrollbar-size: 0 1` + 半透明灰轨道 + `muted` 滑块）以前只写在 `#log` / `#assistant-stream` 上，现在也写进 `#input`。
  - 机制：`TextArea` 是 `ScrollView`，它的 `ScrollBar` 子控件渲染时读的是**父控件**（即 `#input`）的 `scrollbar-*` 样式（`scrollbar.py` 的 `ScrollBar.render` 取 `self.parent.styles`）→ 不需要给滚动条控件单独写规则，写在哪一层都行。
  - 顺带把宽度从默认 2 cell 收成 1（文本框可用宽度 +1），轨道底色与 #log 完全一致（两边背景都是 `transparent`）。
  - 回归：`tests/test_tui.py::test_input_scrollbar_matches_log`（八个 `scrollbar-*` 属性逐项比 #log，#log 是基准）。

- **换行键多收一个 `Ctrl+Enter`**（用户提问：「输入框现在好像是 ctrl+enter 是换行？」）：换行 = `Shift+Enter` / `Ctrl+J` / `Ctrl+Enter`，`Enter` 一律提交。
  - 原行为：只认 `shift+enter` / `ctrl+j`；`ctrl+enter` 是**意外**能换行的——多数终端把 Ctrl+Enter 编码成 LF（= `Ctrl+J`），而支持修饰键上报的终端（kitty 键盘协议）会送来独立的 `ctrl+enter`，那种终端里它原来是个**死键**（不提交也不换行：`TextArea._on_key` 只管裸 `enter`）。现在两种来源都收，行为不再因终端而异。
  - 回归：`tests/test_tui.py::test_input_newline_keys`。

- **`read` 的容量上限截断不再落盘**（用户提出：「read 为什么自己要落盘？模型还没看到呢就落盘？」）：截断时改成只补一行 `[已截断：可用 offset=N 继续读]`。
  - 判据（已写进 `AGENTS.md`「谁该落盘」）：工具输出**不可再生** → 落盘 + `[工具输出全文已保存: path]` 指针（`shell` 的 stdout：进程结束就没了，副本是唯一取回途径）；**可再生** → 只报进度、不落盘（`read` 的文件还在原地，且自带 offset 分页，续读拿到的是完整内容）。
  - 原实现是「谁截断谁落盘」这条通用规则的无脑套用（截断发生在工具内部 → harness 看不到全文 → 只能自己落）。实测那份副本**没有任何消费者**：`extract_spill_path` 的唯一调用点被 `call.name == "shell"` gate 住（注释里的理由仍成立：read/edit/write 的结果文本可能含该格式的**字面量**）、`full_history()` 只按消息自身 `raw_path` 字段展开（read 从不设它）。
  - 顺带修掉一个隐蔽 bug：该副本不在 `referenced_raw_paths()`（manifest ∪ 消息 `raw_path`）里 → `pie context gc --delete` 把它当垃圾删掉，而消息里的指针还留着 → **死链**（实测 `collect_context_garbage()` 返回 `['tool-d90d26c1e5dd37cf.txt']`）。不落盘后此问题自然消失。
  - 影响面：`tools.read` 的 `omitted > 0` 分支（2 行）+ docstring；`shell` 一字未动（实测仍落盘 + 指针）；`tests/` 对 read 落盘的覆盖为 0。另：`_max_lines` / `_max_bytes` 的**默认值都是 None**（不设上限），要限得在 `[tools.read]` 里配。

## 2026-09-20

- **公共面收敛：每个模块声明 `__all__`（新增）/ 运行时属性改字段形式 / `ToolMessage.compact` 返回 bool** —— 三项都是为了让「对外契约」在类型检查器与 `import *` 两个层面都可见。
  - **`__all__`**：内部模块（`aio` / `cli` / `clipboard` / `files` / `input` / `textkit` / `theme` / `tui` / `__main__`）写 `__all__: list[str] = []`；公共模块（`config` / `context` / `llm` / `loop` / `session` / `tools`）列出真正对外的名字（**按用户要求划定**：TUI、主题、CLI 都不算对外 API——`pie` 对外就是命令行本身；`loop.CANCEL_TEXT` 虽是模块级常量但也不进公共面）。`pie/__init__.py` 的 `__all__`（31 个）事先就有，是唯一入口契约。实例：`from pie.aio import *` 以前会带出 `['Any','Coroutine','Generator','TypeVar','asyncio','close_asyncgens','contextlib','event_loop','gc','run']`（一半是依赖名），现在为空。**边界要知道**：`__all__` 只约束 `import *`，挡不住显式 `from pie.aio import run`（功能不失——console script `pie = pie.cli:main` 与内部显式 import 都不受影响）；实测（pyright 1.1.414 + ruff）`py.typed` 也只拦 `from pie import <未重导出名>`，拦不住 `from pie.internal import x`；`_` 前缀的静态拦截要 ruff 的 `SLF001`（成员访问）/ `PLC2701`（私有名 import，需 `--preview`）。所以内部模块改名 `_xxx`、或实现拆成独立发行包（物理隔离）是后续可选项。
  - **`Config` 的两个运行时属性 `config_file` / `auto_compact_threshold` 改成 dataclass 字段**（`field(default=None, repr=False, compare=False)`），配 `RUNTIME_ONLY_FIELDS`：`save()` 里从 `asdict` 摘掉、`load()` 里跳过——保留「不落盘、也不从配置文件读回」的原语义，同时消掉 Pyright 的 `reportAttributeAccessIssue`（动态属性对类型系统不可见）；`soft_limit()` / `session._persist_note()` 两处 `getattr` 兜底随之去掉。新增回归 `test_runtime_attrs_are_not_persisted`。
  - **`Message.content` 声明成联合类型**（`MultiMediaContent | str | None`，别名在 `context.py`）：`ImageMessage` 原来用带注解的赋值把 `content` 收窄成 `list|None`，触发 Pyright 的 `reportIncompatibleVariableOverride`（可变属性不协变，覆盖类型必须与基类完全一致）→ 基类放宽 + 子类去掉注解；连带把两处下游收窄点补上（`ToolMessage.compact` 里 `isinstance(content, str)`、`Session.full_history` 的兜底）。
  - **`ToolMessage.compact` 返回值 `int(0/1)` → `bool`**，调用点 `_compact_tools` 改成 `n += m.compact(...)`（bool 是 int 子类）：以前返回值被丢弃、计数是无条件 `n += 1` → `tools=N` 统计的是「扫过的条数」而不是「真正落盘的条数」（短输出不值得压也被算进去）。现在与 `_compact_turns` 的口径一致，`/compact` 与 `[context] 压缩节省…（tools=N）` 都准了。
  - 顺手：`textkit.py:157` 文档字符串里的 `` `\S+\s*` `` 是非法转义（每次 import 报 `SyntaxWarning`），写成 `` `\\S+\\s*` ``（`__doc__` 不变，`compileall -W error::SyntaxWarning` 已干净）。

- **公共入口改名（用户要求）：`loop.acomplete_turn` → `aturn`、`loop.run_agent` → `run`**（对外即 `pie.aturn(...)` / `pie.run(task, tools=/llm=/config=)`）。
  - 依据：`AGENTS.md` 目录树早就写着 `loop.py # 循环层：run_agent / aturn`——代码回到文档的名字；`aturn` 与 `Session.aturn` 同名同义（都是「跑一轮」，只是层级不同：模块函数收 `AgentMessage`，方法收用户串）。
  - `session.py` 内部由 `from .loop import acomplete_turn` 改为 `from . import loop` + `loop.aturn(...)`：模块限定调用，避免在 `Session.aturn` 方法体里出现同名调用看着像递归（解析到全局其实没问题，但读起来误导）。
  - `aio.run`（内部件，不在公共面）名字未动，所以 `run()` 体内是 `aio.run(aturn(...))`；`run()` 的 docstring 里注明了二者无关。
  - 验证：替身 LLM 实跑 `pie.run()` 与 `Session.turn()`（都返回最终答复）；`rg 'acomplete_turn|run_agent' src/` 无残留（docs 的历史条目保留旧名）；全套 78 例 + `self_check()` OK。

- **重试日志带上异常摘要；429 尊重 `Retry-After`**（用户报告：synthetic_rl 那边批量跑出一堆 `[retry] 流式请求失败`，无法归因——「是不是 pie 的 bug？」）：
  - `[retry]` 行改成 `[retry] <what>失败（<异常摘要>），Nd 后第 a/b 次重试`，摘要走新的 **`_exc_brief(exc)`**（`类型: 消息`，最多两层 `__cause__` —— SDK 的 `APIConnectionError: Connection error.` 真原因在 `__cause__` 里）；`_sleep_before_retry(attempt, what, exc)` 多收一个异常参数（两个调用点都传）。
  - 新增 **`_retry_after_seconds(exc)`**（读 `exc.response.headers["retry-after"]`，只认秒数形式）；**`_retry_delay(max_delay_seconds, exc=None)`** 优先用它、夹在 `[1.0, 60.0]`，没有才退回原来的随机退避。
  - 背景与结论：实测 53 次真调用（含 8 并发 + 10KB 大 prompt）**0 次重试**，所以那些重试是**间歇性**的（代理抖动 / 429 / 首包超时），不是 pie 的代码错误；`emitted` 那道闸保证重试不会重复内容。这次改动不改行为，只把「原因」打出来，并让限流退避变准。
  - 验证：`_retry_delay` 对 429+`Retry-After` 的四种取值（无头 / 5 / 0 / 100 / date 形式）逐个实测；`_exc_brief` 对嵌套异常输出两段；`_sleep_before_retry` 实打一行；全套 78 例 + `self_check()` OK。
  - 验证：全套 **78 例全过**（config 11 / session 5 / files 24 / clipboard 10 / theme 6 / aio 4 / tui 18）+ `self_check()` OK；另外脚本校验「每个 `__all__` 里的名字都真实存在」。

- **`aturn` 去掉 `user_turn` 参数**（用户指出「感觉没必要」——核实确实冗余）：它唯一的作用是 stderr 日志标签 `[t{user_turn}s{step}]`（仅 `cfg.verbose` 时）与 `tool_call` 事件的 `turn` 字段，而后者**没有任何消费方**（tui 的 `_append_event` 只读 name/arguments；cli 根本不传 `on_event`）。现在 loop 内部按 `sum(isinstance(m, UserMessage)) or 1` 派生（这行本来就在 `user_turn is None` 分支里）。
  - 与 `Session.turn_count` 的差异实测：**只**在同一次运行里 `/clear` 或 `/reset` 之后分叉（`turn_count` 连续、派生值从 1 重数）；而 `turn_count` **不落盘**（`__meta__` 里没有它），`Session.load()` 也是从 UserMessage 数重算（session.py:236）→ 重启后两者本来就一致，所以「连续编号」这点收益跨不过一次重启。
  - 保留：`tool_call` 事件的 `turn` 字段（对外事件 schema，嵌入方有用，值改用派生）、`Session.turn_count`（还喂 `pie -p --mode json` 的 `"turns"`，cli.py:328）。
  - 验证：替身 LLM 实跑——历史里 2 条 `UserMessage` → 事件 `(turn, step)=(2, 1)`、stderr `[t2s1] echo({"text": "hi"})`；单条 → `[t1s1]`；`rg user_turn src/ tests/` 无残留；全套 78 例 + `self_check()` OK。

- **`aturn` 的 `manifest: Path` 改成 `on_compact` 回调**（用户要求）：loop / context 不再认识「manifest 文件路径」这种实现细节，压缩事件（工具级 / 轮次级 / 会话级 + shell spill）统一经 **`on_compact` 回调**（类型就是 `Callable[[dict[str, Any]], None]`，与 `on_event` 同型；一开始起的别名 `CompactHook` 已按用户要求删掉）交给调用方，`None` = 不通知。CLI 侧由新增的 `Session._record_compact(entry)` 实现（内部仍是 `write_manifest`，**行为一字不变**：manifest 文件、`/stat` 计数、`pie context info/verify`、`full_history` 全照旧）；`Session.clear_window` 也改走同一回调。
  - 嵌入方收益：公共签名不再暴露磁盘路径；想观测压缩就直接接回调（RL 侧能知道「什么时候被压了、省了多少」）。与 `on_event` / `on_progress` 一样属于 loop → 调用方的**出站通知**。
  - ⚠️ 澄清一个易踩的点（本轮实测）：**不传 `on_compact` ≠ 不落盘**——三级压缩里的 `write_raw()` 都是无条件调用，正文照样写 `~/.pie/context/`；嵌入方要完全不碰 `~/.pie` 必须 `Config(compaction=None)`。
  - 同时**否决**了「把 `cancel_event` 也改成回调」：方向相反（`on_*` 是 loop → 调用方的通知；`cancel_event` 是调用方 → loop 的控制信号），且调用方需要「可等待 / 可立即唤醒」的语义——现在靠 `await cancel_event.wait()` 与请求 task 一起 `asyncio.wait(FIRST_COMPLETED)` 实现「真打断」；换成 `Callable[[], bool]` 只能轮询（延迟 + 白白调度）。`asyncio.Event` 是标准件，保持不变。
  - 验证：替身 LLM 实跑——触发压缩时 `on_compact` 收到 `{level: 1, kind: 'tool', tool: 'big', …}`；不传也不报错；`Session` 侧 manifest 文件与 `/stat` 段照常；`pie context info` 正常；全套 78 例 + `self_check()` OK。

- **参数改名 `cancel_event` → `cancel`**（用户提议、确认）：它是调用方 → loop 的**入站控制信号**（`asyncio.Event`，类型不变）。用户先提的 `cancelled` 被否——名字像布尔状态，而 `if cancelled:` 对非 None 的 Event **恒真**（能写出 bug 的命名）；且调用方是在它上调 `wait()` / `is_set()`，只有名词读得通（`cancel.wait()` ✅ / `cancelled.wait()` ❌）。改动只碰参数名：`aturn` / `Session.turn` / `Session.aturn` / `_wait_cancellable` / `_model_call` / `_tool_call` / `_run_tool_call` + tui 的调用点；`PieApp._cancel_event`（TUI 私有属性）未动。
  - 同时定下：**`on_event` 名字保留、暂不拆两路**（用户决定）。被否的候选与理由：`on_delta`（名不副实—— 6 种 `type` 里只有 reasoning/content/tool_progress 3 种是增量，`answer` 是回合终止信号）、`on_data`（与 `on_delta` 不成对照，且 IO 语境里 `on_data` 惯例指原始分片，反而更像增量那一路）、`on_recv`（socket 动词、未描述内容、暗示不存在的双向信道）。→ 命名原则记下：**名字要名词化，并且跟它的类型 / 调用方式读得通**。
  - 验证：取消语义实跑——① 飞行途中 `cancel.set()` → 返回 `用户手动终止`、历史末尾一致；② 进回合前已 set → 同样立即终止；③ 不传 `cancel` → 不可取消、照常返回；全套 78 例 + `self_check()` OK。

- **`termbg.py` 整并进 `theme.py`**（用户要求）：`theme.py` 现在既管主题数据、也管「探测终端背景」——`detect_dark_background` / `query_osc11` / `parse_osc11` / `parse_colorfgbg` + `_OSC11_RE`，只用标准库。依据：`theme.py` 的 `get_theme(name, dark=None)` **本来就在运行时调它**（「探测背景 → 选族变体」本属主题这件事），所以合并**零行为变化**。改动：`theme.py` 搬入 109 行、头部 docstring 改成「两类内容（展示数据 / 终端背景探测）」；`tui.py` 的 import 并进 `from .theme import Theme, build_css, detect_dark_background, get_theme`；`tests/test_theme.py` 的 `from pie.termbg import ...` 并进 `from pie.theme import ...`；`src/pie/termbg.py` 删除。
  - 同期评估并**否决**了「`textkit.py` 并进 `tui.py`」：会毁掉「textkit 只依赖 Rich、不 import Textual」这个性质（合并后 import `pie.tui` 就会执行 `install_cjk_wrap()` 的全局 monkeypatch），也推翻 AGENTS.md / MEMORY.md 里已定的「显示层文本处理独立成模块」约定，且 `textkit` 与 `tui` 在两份源码树里都已分叉、合并只会加重收敛成本。
  - 验证：`parse_osc11` / `parse_colorfgbg` / `detect_dark_background` / `get_theme` 行为不变（含族名 `dark=True` → `catppuccin-mocha`、`dark=False` → `catppuccin-latte`）；`rg termbg src/ tests/` 无残留；全套 78 例 + `self_check()` + `pie --help` OK。

- **`input.py` 整并进 `cli.py`；`config._prompt` 改用内置 `input()`**（用户要求）：`read_input`（prompt_toolkit 行编辑 + 非 TTY 回退）搬进 `cli.py`（它唯一的消费者就是交互式聊天循环），放在 `__version__` 之后带一段说明；`src/pie/input.py` 删除（模块 15 → **14**）。
  - `config._prompt`（首次运行向导：模型名 / 端点 URL / API key）不再经 `read_input` —— **答案都是 ASCII**，直接 `input()` 就够，也省得 `config.py`（配置层）去依赖行编辑层；`EOFError → 默认值` 的容错保留。中英退格截断那个坑（WSL/mintty）只影响聊天输入，由 `cli.read_input` 负责。
  - 验证：管道模拟首次运行向导（`printf 'my-model\nhttps://example.com/v1\nsk-test-123\n' | PIE_DIR=<tmp> python -c 'ensure_config()'`）→ 三项正确写入 config.toml；`rg 'pie\.input|from \.input' src/ tests/` 无残留；全套 78 例 + `self_check()` + `pie --help` OK。

- **`Session.compact` 的编排搬进 `context.compact`**（用户要求；函数名就叫 `compact`，不用 `compact_now`）：它本来只碰 `self.config` / `self.messages` / `self._record_compact`，是 `maybe_compact` 的**手动姊妹版**（同样是「agent + cfg + on_compact → 同形状统计 dict」）。`Session.compact` 留下做薄包装（公共 API 不变），`session.py` 从 26 行变 4 行。
  - 顺带消掉一处重复：两个驱动入口的统计空形状原本各写一份字面量 → 抽出 `context._empty_stats()` 共用。
  - 切分原则写进注释/文档：**纯编排 → context.py；改会话状态 → 留 Session**。所以同类的 `Session.clear_window`（会重建 messages、追加 windows、写 `~/.pie/windows/`）**不搬**。
  - mode 校验（`mode not in ("auto","tools","turns")`）**没搬**：它现在在 cli/tui 各写一遍，但那是 UI 层输入校验（tui 那条走红色错误提示，塞进 `skipped` 通道会丢样式）——保持本次为纯搬迁、零行为变化。
  - 调用点：`session.py` 用 `from . import context` + `context.compact(...)`（模块限定，避免在 `Session.compact` 方法体里出现同名调用）；依赖方向不变（session → context）。
  - 验证（压缩编排此前零测试覆盖，所以逐条实跑）：`tools` → `{tools:1, turns:0}` 1 条事件；`turns` → `{turns:1, tools:0}` 1 条；`auto` → 两条都有、2 条事件；`compaction=None` → `skipped`；`Session.compact("auto")` → manifest 落两条（level 1 tool + level 2 turn）、`/stat` 显示 `1 / 1 / 0`；全套 78 例 + `self_check()` OK。

- **`aturn` / `run` 新增 `max_steps` 与 `parallel_tools`（并给 `Config` 加 `parallel_tools = True`）**（用户要求，为 synthetic_rl 的接入补齐两个硬缺口）：
  - **`max_steps: int | None = None`**：限「最多问模型几次」。达到上限时不再调模型，把**历史里最后一段非空 assistant 文本**当最终答复返回，并推一个 `answer` 事件；**不额外追加消息** —— 所以历史末尾可能停在 tool 结果上（`assistant(tool_calls)` + 对应的 tool 消息是合法序列，下一条 user 接上也没问题）。之所以不追加，是为了让 `answer_turns` / `final_answer` 的口径与旧实现（synthetic_rl 的 `for _ in range(max_steps)` + `answers[-1]`）完全一致（追加会多出一条重复文本）。None = 不限（CLI 就是 None，行为零变化）。
  - **`parallel_tools: bool | None = None`**：同批 tool_calls 是否并发。**None（默认）= 跟随 `Config.parallel_tools`**（新字段，默认 `True` = 保持原行为）；`False` = 按模型返回顺序**串行**执行（`await` 逐个），给「工具改同一份可变状态」的嵌入方用（例：synthetic_rl 的工具都在改同一个 `S`，并行会竞态）。两条路径都按模型返回顺序回填 ToolMessage，所以 `_step_batches` / `keep_last_steps` / 压缩认定不受影响；串行时 `_cancel_tools` 的收尾逻辑与并行完全一致（每个后续工具因 cancel 已置位而立即返回 None）。
  - `run()` 同样透传这两个参数（`run(task, max_steps=..., parallel_tools=...)`）。
  - 验证（替身 backend，零网络）：`max_steps=None` + 两步替身 → 2 次调用、返回 `做完了`、历史末条 assistant；`max_steps=3` + 永不收工替身 → **恰好 3 次调用**、返回 `step3`、历史末条 tool、`answer` 事件 = `step3`；`max_steps=1` → 1 次调用（工具不跑）；三个 tool_call 一批（每个 sleep 0.15s）：`parallel_tools=True` → 耗时 0.16s、有重叠；`False` → 0.45s、无重叠、顺序 = 模型返回顺序；`None` + `cfg.parallel_tools=False` → 同样串行。`Config` 落盘/读回 `parallel_tools = false` 正常；全套 78 例 + `self_check()` OK。

## 2026-09-17

- **重试改成 pie 自己实现（不再用 openai SDK 自带的）；`max_retry_delay_seconds` 配置删除 → 常量**（用户要求：「手动实现 llm.py 里面的 retry 相关功能，不要使用 openai 自带的。相关可用参数：`max_retries`。移除 `max_retry_delay_seconds`，作为常量写进 llm.py」）。背景是上一轮查出 `max_retries=2` 实际会发 **6** 次请求（SDK 3 次 × pie 的 `stream_options` 回退又 3 次，`x-stainless-retry-count` 会归零）。
  - `llm.py` 新增模块级常量与纯函数（**当日随后被本条目末尾的「（后续）」改动取代：常量搬进 config.py、指数退避改成随机等待**）：**`RETRY_DELAY_SECONDS=1.0`**（首次重试等待，即原 `max_retry_delay_seconds` 的值）、`RETRY_MAX_DELAY_SECONDS=8.0`（单次封顶）、`DEFAULT_MAX_RETRIES=2`、`_RETRYABLE_STATUS={408,409,429}`、`_TRANSIENT_MODULES=(openai, httpx, httpx2, httpcore, httpcore2, ssl)`；**`_retryable(exc)`**（408/409/429/5xx 或有 status_code 之外、来自网络栈的传输异常才重试；其余 4xx 与 pie 自己的异常立刻抛）、**`_retry_delay(attempt)=1s×2^(n-1)` 封顶**。
  - `OpenAILLM`：`__init__` 存 `self.max_retries`（默认 `DEFAULT_MAX_RETRIES`，负数归一 0）并把 **`client_kwargs["max_retries"]` 硬置 0**（关掉 SDK 重试，跟 pie 那层叠加会翻倍）；新增 `_retry(factory, what)`（最多 `1+max_retries` 次，退避走 `_sleep_before_retry`，失败在 stderr 打一行 `[retry] …`），`complete()` / `list_models()` 改走它；`stream()` 把原来的 `while True` + 宽 catch 换成 **attempt 计数循环**：`emitted` 为真直接抛（吐过的内容不能重来），`with_usage` 且 **400** → 摘 `stream_options` 重来一次（continue，不计数不退避），否则 `_retryable` 才重试（并把上一轮残留的 `usage` 清掉）。
  - 行为变化（实测）：连接类失败 `max_retries=2` → **3 次**请求（1 + 2），退避 1s/2s；`max_retries=0` → 1 次；`stream_options` 摘参数不再被连接错误触发（只 400 触发）；流中途断线只在「还没吐过块」时重试。
  - 清理：`Config.max_retry_delay_seconds` 字段、CLI `--max-retry-delay-seconds`（含 `--help` 里那句「暂不生效」）、README 配置示例里的同名键一起删；`--max-retries` help 改成「只重试连接/超时/408/409/429/5xx」。旧配置里残留该键无害——`Config.load` 只挑 dataclass 认识的键。
  - **（后续，用户要求）退回并重定义等待策略**：恢复 `Config.max_retry_delay_seconds`（CLI `--max-retry-delay-seconds` 一并恢复），`OpenAILLM` 同名参数透传（session / loop / `pie files` 三个构造点都传），`_retry_delay(max_delay_seconds)` 改成 **`max(1.0, random.uniform(0, max_delay_seconds))`**（去掉指数退避；随机是为了避免一批请求同时撞回来，1.0 是下限 → 默认上限 1.0 时恒等 1s）；`DEFAULT_MAX_RETRIES` 与 `RETRY_MAX_DELAY_SECONDS`（改名 **`DEFAULT_MAX_RETRY_DELAY_SECONDS`**）两个常量从 llm.py **搬进 config.py**（`RETRY_DELAY_SECONDS` 删）。测试跟着改：`_FastRetry` 替身改成按 `llm._retry_delay`（不再是模块常量），新增「默认上限 → 恒 1s / 上限 8 → 落在 [1,8] 且确实在随机」断言。
  - **（后续 2，用户要求）`OpenAILLM` 不再带默认重试值**：`max_retries` / `max_retry_delay_seconds` 不传就是 **0**（组件层面默认不重试；应用里 session / loop / cli 三个构造点都显式传 `Config` 侧的值），并把 `DEFAULT_MAX_RETRIES` 常量整个去掉——`Config.max_retries` 回到字面量 `2`，config.py 只留 `DEFAULT_MAX_RETRY_DELAY_SECONDS = 1.0`（字段默认值）。测试：`test_sdk_retries_are_disabled` → `test_retry_defaults_and_sdk_disabled`（断言裸 `OpenAILLM` 两个字段都是 0 + `client_kwargs["max_retries"]==0`，`Config` 侧才是 2 / 1.0）。
  - 验证：新增 **`tests/test_llm.py`（11 例，零网络）**：判据分类、等待随机+1s 下限、`client_kwargs["max_retries"]==0`、`complete()` 重试/放弃/不重试硬错/`max_retries=0`、`stream()` 首块前重试、**吐过块不重试**、400 摘 `stream_options`（且无退避）、重试上限；跑真配置的流式冒烟（`deepseek-flash`，7 prompt + 1 completion tokens）正常；全套测试 **84/84** + `self_check()` OK。

- **出错盒显示异常链；`BOX_BODY_LINES` 100 → 24**（用户要求：先去问「遇到异常信息的时候，在 tui 里面可以显示多一点吗？」，看完第一版后说「移除 `_exc_hint`。BOX_BODY_LINES 设为 24」）。
  - 新增模块级纯函数 **`_error_body(exc)`** = 首行 `类名: 消息` + `↳` 异常链，接在 `_fail_turn`（回合失败）与 `!cmd` worker 兜底异常两处。动机：`APIConnectionError: Connection error.` 这种被 SDK 包过的报错只显示最外层等于没说——真原因（DNS / 连接 / TLS）在 `__cause__` 里（`raise APIConnectionError(request=request) from err`），用户据此看不出到底是哪一层、也无法判断「是不是我连接 API 出问题了」。
  - **`_exc_chain(exc)`**：走 `__cause__` / 未被抑制的 `__context__`（`raise ... from None` 不算真原因），每层带模块前缀（`httpx2.` / `openai.`，`builtins` / `__main__` 不加）以区分是谁抛的，压成单行（`APIStatusError` 的 `str()` 带多行 body），最多 `_EXC_CHAIN_MAX=4` 层，用 `id()` 去重（链成环也不转死）。
  - **按用户要求删掉 `_exc_hint`**：先写过一版按类名 / `status_code` 给「能照做的动作」（`APIConnectionError`→端点+DNS/代理、`APITimeoutError`→`timeout_seconds`、httpx `ReadError`/`RemoteProtocolError`→流中途断开、`401`/`429`/`400` 上下文超窗），用户看过之后要求移除 → 现在只留首行 + 链，**不解释、不提建议**（`_error_body` 也随之去掉 `cfg` 参数；若日后想恢复，判据本就不用 import openai）。
  - **`BOX_BODY_LINES` 100 → 24**（用户指定）：盒子模式的工具正文（read 大文件 / shell 长输出 / resume 回放）与 `_lean` 的失败正文块共用这一条截断规则，随之都变短。测试里两处「`line299` 不在显示里」的断言本来就绑着常量，实际已失效 → 改成 `line{BOX_BODY_LINES}` 那一行必须不在（否则 24 行时旧断言恒真）。
  - 验证：`tests/test_tui.py` **18/18**（新增/改名为 `test_error_box_shows_cause_chain`：首行、链、长度=2、无链时只剩首行、`from None` 不算、成环不转死，并真挂 `PieApp` 走 `_fail_turn` 后框选复制核对），`test_theme/config/session/files` 全过，`self_check()` OK；headless 渲染确认两行盒子形态正常。

## 2026-09-16

- **死代码清理（tui.py）**（用户问「有没有死代码」）：删了最近几轮重构留下的残留——
  - `MESSAGE_ROLES` 常量：上一轮把 `_box` 的分派改成白名单（`tool_call` / `tool_result`）后，全仓库代码零引用（只在 `_panel` 的 docstring 里被当文字提到）。现在 `role` 的合法取值直接写在两处分派里。
  - **手动 `!cmd` 的 120s 超时残留**：`started = time.monotonic()`（赋值未用）+ `timed_out`（恒 False）+ 注释掉的超时块 + `code = ... "timeout(120s)" if timed_out ...` 死分支 + 与代码不符的 docstring（还写着「保留 120s 超时」）全删——**行为不变**（那个分支本就永远走不到）；`import time` 随之成为未使用 import，一并删。
  - `_finish_turn(self, answer: str)` 的 `answer` 参数未使用（回合正文已由 `answer` 事件固化到 #log）→ 改成无参，`_run_turn` 里的 `answer` 局部变量也去掉。
  - 注释掉的旧代码：`# yield Header()` / `# yield Footer()`（compose 里）、`# compact = lines[3]…`（`_update_status`）、`# f" · [{len(self.session.windows)}]"`（`_update_meta`，docstring 里的「归档窗口数」一并修正）。
  - 保留不动的「看起来像死代码」：`on_text_area_changed(self, event)` 的 `event`（Textual 消息处理器签名，注解中还声明了触发消息类型）、`_slice_of_row` 的 `_end` 与几处 `for _, x` 的 `_`（故意的占位）、`_wide_text`（测试入口）、以及 vulture 报的框架钩子（`compose` / `on_mount` / `action_*` / `CSS` / `TITLE` / `run_tui` / `self.theme`）。
  - 验证：`vulture --min-confidence 60` 只剩上述误报；自写的 AST 扫描（模块级零引用 / 未用参数 / 赋值未用的局部）只剩占位符；headless 渲染快照与清理前**逐字节一致**（历史回放 × 盒子/简洁、手动 !cmd 三态、notify 盒）；另冒烟了「回合收尾链路」（`_run_turn` → `_finish_turn`）与其余测试文件 59/59 + `self_check()`。

- **tui 渲染：单一出口 `_notify` + 单一决策点 `_box`**（用户要求：把 boxed/lean 都藏进 `_box`，且值保留 `body / role / border_role / lean` 四个参数——title、icon、tool、summary、manual、code 都去掉）。原先「往 #log 写东西」散在 6 处（各自 `query_one("#log")`、各自决定写盒子还是简洁单行，`log` 还当参数层层传递）。
  - **`_box(palette, body, *, role="system", border_role="", lean=False) -> list[Panel | Padding]`**：`role` 兼做角色与工具。消息类（`MESSAGE_ROLES`）⇒ 文本盒，标题查 `ROLE_TITLES`（user→你、assistant→pie）；`tool_call` ⇒ 调用（body = `{"name", "arguments"}`）；**其余一律当工具名** ⇒ 该工具的结果（body = 正文）。图标/标题/边框/成败全从 role + body 推，不再靠参数传。
  - **`_notify(body, role="system", **kw)` = #log 的唯一写入口**（tui.py 里唯一的 `log.write`），`lean` 默认取 App 的开关；`_render_tool_call/_render_tool_result` 只负责组装 body 转交（不再接 log 参数）。
  - **盒子模式新增行数封顶**：工具正文超 `BOX_BODY_LINES`（100）行只显示前 N 行（标题标总行数 + 末尾一行 `...[已省略 K 行，共 M 行]...`）。原行为是「实时全量、只在 resume 回放时 head50/tail50」→ 统一成一条规则（`truncate` 参数随之删除，`_shell_result_box` 不再自己做 >200 行的 head/tail）。消息正文不截。
  - **（后续）lean 的失败正文块也统一到同一条截断规则**（用户要求）：**删除 `LEAN_DETAIL_HEAD` / `LEAN_DETAIL_TAIL`**，`_lean` 改成**只显示前 `BOX_BODY_LINES` 行** + **与 `_panel` 完全同一句** `...[已省略 K 行，共 M 行]...`（原先自留 head12/tail7、中间省略，与盒子模式两套规则）。
  - **两个参数推不出来的地方**（已向用户标明）：① **结果行摘要**（lean 下 `✓ read a.txt` 里的 `a.txt` 来自配对的 tool_call 参数，结果正文里没有）→ 放进 body：`{"content", "summary"}`，`_render_tool_result` 的 `summary=` 签名不变（测试与行为都保住）；② **手动 !cmd 的豁免**（不吃 lean / 成功框灰边框 / 取消补 `[用户手动终止]`）→ 拆到调用方：`_notify(..., lean=False)` + `border_role=MANUAL_SHELL_ROLE`（仅成功）+ 自行补 `[exit=code]` 头与取消说明。
  - **改成「两个自包含叶子 + 一个分派器」**（用户要求）：**`_lean(palette, body, *, role, border_role)`**（简洁单行）/ **`_panel(palette, body, *, role, border_role)`**（盒子）**参数与 body/role 语义完全一致**，**两者内部各自 inline 所需逻辑，不调任何中间小函数**——删掉 `_raw_panel` / `_tool_result_box` / `_cap_body` / `_tool_call_args` / `_tool_result_parts` / `_split_shell_exit` / `_shell_result_box` / `_tool_failed_role` / `_single_line` / `_oneline` / `_lean_line` / `_lean_detail` / `_lean_tool_result`。`_box` 缩成几行分派（lean 且工具活动 → `_lean`，否则 `[_panel]`），tui.py 净减 ~50 行。
  - **body 形式同步调整为「显示载荷」**：`tool_call` 的 body 从 `{"name", "arguments"}` 改为 `{"name", "text", "summary"}`（text = `_format_tool_args(args)`，App 渲染；摘要仍由 App 的 `_tool_summary` 按 `LEAN_SUMMARY_KEYS` 取）——于是两个叶子都不再需要碰工具参数结构；工具结果 body 不变（正文或 `{"content", "summary"}`）。
  - **代价（已在两处 docstring 里互相注明）**：成败判定 + `[exit=]` 头解析在 `_panel` 与 `_lean` 各有一份；要收敛的话要么提回一个小纯函数（就破坏了「叶子自包含」），要么让 App 先把结果解析成 `{text, status, code}`（那样叶子就只剩画）。
  - **再一次按用户要求收拢**：`_panel` 也返回 `list[Panel]`（以 `_box` 直接 `return _panel(...)`）；**工具名从 role 移到 body**——`role` 只剩消息类（system/user/assistant/error/cancelled）∪ `{tool_call, tool_result}`，`tool_call` body = `{"name", "arguments"}`、`tool_result` body = `{"name", "arguments", "content"}`（arguments = 配对那次调用的参数）；**摘要（read/write/edit→path、shell→command）改在 `_lean` 里从 `arguments` 推**，于是 App 侧的 `_format_tool_args` / `_tool_summary` 两个 helper 也删了——**工具参数的解析/展示全在渲染层**，App 只组装名字+参数+正文；`_render_tool_result(name, content, *, arguments=None)`。
  - 真 bug 一起修了：`_lean` 把 role 名（`error`）当成 lean 状态词传给 `palette.lean_mark`，而它只认 `ok/fail/cancelled`（未知静默退回 ok）——已加 status→词的小映射表。
  - 验证：headless 渲染快照与上一版**逐字节一致**（历史回放 × 盒子/简洁、手动 !cmd 三态、notify 盒），另用独立脚本复刻了原测试的 lean/盒子断言（单行、失败正文块、超长摘要压单行、首尾行截断、复制不带留白）全过；另抽查了摘要推导：dict 参数、JSON 串参数（历史）、断 JSON、空参数、消息盒全部符合预期；test_aio/config/files/session/theme **59/59** + `self_check()` OK。（后续：**test_tui / test_clipboard 已按新 API 更新完毕**——`_lean_line` / `_tool_result_box` / `title=` / `tool=` / `summary=` / `manual=` / `code=` 全部换成 `_lean` / `_panel` 的 body 形式，全部测试 **87/87 通过**。另：lean 失败正文块的首行前缀 **`→` → `↳` 是有意改动**（用户改的），上文“逐字节一致”仅指盒子/单行布局；测试与 MEMORY 已按新契约同步。）

- **手动 `!cmd` 改为「始终套盒子 + 默认灰边框」（不再吃简洁模式）**（用户要求，附运行截图）：简洁模式下 `!cmd` 原先跟 agent 工具活动一样压成单行（`✛ shell ls` / `✓ shell ls` + `→` 缩进输出块），用户希望它回到 box 且边框用默认灰。
  - `tui.py`：`_run_shell` / `_show_shell_result` 删掉 `self.lean` 分支，改走既有盒子 helper（命令行框 `✛ shell` / `$ cmd`，结果框 `✓ shell [0]` + 输出）；新增模块常量 **`MANUAL_SHELL_ROLE = "system"`**（= `role_border` 的兑底色、与命令反馈盒同色的「默认灰」）——!cmd 不是 agent 的工具调用，不占 `tool_call` 的橙棕身份色（输入框的 shell 模式边框已经是橙棕，两者分开）；执行失败仍染红（`error`）、被 /stop 终止仍低调灰（`cancelled`），标题里的状态图标 ✓/✗/■ 因而不受影响。
  - `_box()` / `_tool_result_box()` 新增可选 **`border_role`**（默认空 = 按 `role`）——唯一用途是把**边框色与状态图标解耦**：结果框的 role 仍按执行结果取（决定 ✓/✗/■），只把边框换成默认灰；没有它就只能改 role，而 `icon_system=""` 会把 ✓ 一起弄丢。顺带把 `!cmd` 输出末尾的换行 rstrip 掉（lean 那条路径本来就是，盒子底部不再多一行空白）。
  - 删除随之失效的 `_lean_shell_result()` / `LEAN_SHELL_HEAD` / `LEAN_SHELL_TAIL`（lean 侧只剩 `_lean_tool_result` 给 agent 工具用；!cmd 的截断额度改用 `_shell_result_box` 的 200 行 head/tail，比原来更宽）。
  - 验证：`tests/test_tui.py` 新增 `test_manual_shell_is_boxed_even_in_lean_mode`（lean 下命令行/结果框都是盒子、灰/红色按渲染后的 Strip segment 取色断言、✓/✗/■ 保留、agent 回合的 shell 结果仍是棕框）；真机 headless 跑 `!ls`（含失败用例 `ls /nope/nope` → `✗ shell [2]` 红框）；全套 74 例（test_tui 15/15）+ `self_check()` OK。

- **移除未使用的 `numpy` 依赖**（依赖精简审查）。`numpy` 声明在 `[project].dependencies` 里但**全仓库零引用**（src / tests / docs / README 全 grep 无 `import numpy` / `np.`；唯一匹配是 pyproject 自身与 `inp.text` 之类误报），也**不是任何包的传递依赖**（`uv tree` 里只有 pie 直接依赖它）。
  - `pyproject.toml` 删掉该行 → `uv lock` 29 → 27 包 → `uv sync` 卸载 numpy，`.venv` 体积 123M → 68M（numpy 29M + numpy.libs 27M）。
  - 验证：先做「numpy 被 `sys.meta_path` 阻断」下的导入/自测（`import pie` + `cli.self_check()` + test_session/files/theme/config 全通过），确认无隐藏动态导入；删后全套 73 例通过（test_aio 4 / test_session 5 / test_files 24 / test_theme 6 / test_config 10 / test_clipboard 10 / test_tui 14）。
  - 保留但**不要**再当成可精简项的两个「看起来没用到」的依赖：`pillow`（仅 `clipboard.py` 里 try-import，缺了就静默禁用剪贴板图片功能）与 `prompt_toolkit`（仅 `input.py` 里 try-import，缺失回退内置 `input()`）。两者都是**刻意的软依赖**，删了会让功能悄悄失效。（另：测试用到 `pygments`，靠 rich/textual 传递引入，未显式声明。）

## 2026-09-15

- **`pie files list --all`：列出云端上传件**（用户要求，接上条）。`pie files list` 只看本地（各会话 `__meta__.files`），云端那份没有任何可见手段（`gc --all` 只能删）。现在 `list --all` 反向：调 Files API `GET /files`（自动翻页）列出**本账号下全部**上传件，每条打印 id / 文件名 / 大小 / 上传时间 / 过期时间，并用本地会话记录标出「这条是哪个会话记的」（`会话=未记录` = 本地没有引用它，多半是别的工具留下的或本地记录已随会话删掉）；云端为空时说「服务端没有上传件（云端为空）」。同样支持 `-c/--config`。
  - `files.py`：抽出 `list_remote_files(client)`（`FileObject` → `{id, filename, bytes, created_at, expires_at}` 普通 dict，字段缺失给 None）+ `_remote_file_info()`；`purge_remote_files()` 改为复用它（先全部列出来、再逐个删，语义不变）。
  - `cli.py`：两个 `--all` 共用一个 `_files_api_call(config_file, note, work)`（建客户端 → `aio.run(work(client))` → 用完 close → 网络/鉴权异常转成一句人话），省掉两边各写一份；新增 `_local_file_index()`（`file_id` → 会话名）与 `_fmt_ts()`（空值/坏值都给 default，服务端字段可能是 null）。
  - 验证：`tests/test_files.py` 新增 5 例（`list_remote_files` 字段归一与缺字段不炸、`_fmt_ts` 容错、`list --all` 云端列表 + 会话标注 + 空云端、空 key 不建客户端）；真机：上传一个探针 → `pie files list --all` 打出 `file-api-… 76 B list-probe.png / 上传=… 过期=永久 会话=未记录` → `pie files gc --all` 删掉它；另实测带 `expires_after` 上传的文件在 `GET /files` 里确实带 `expires_at`（所以 pie 自己的上传会显示真过期日期）。全套 24/24。

- **`pie files gc --all`：调 Files API 清空云端上传件**（用户要求）。此前 `pie files gc` 只管本地副本，服务端那份只能等上传时带的 `expires_after`（默认 30 天）自行过期 —— 过期前想立刻收回（误传了敏感图、想换账号重传）没有手段。
  - `files.py` 新增 `purge_remote_files(client)`：`async for f in client.files.list()`（SDK 的 AsyncPaginator，自动翻页）→ 逐个 `await client.files.delete(f.id)`；**单个删除失败不中断**（记进返回的 `{file_id: 错误}` 表继续删下一个），返回 `(删除成功的文件信息列表, 失败表)`。
  - `cli.py`：`gc` 子命令加 `--all`（与 `--delete` 正交，可同时用：一个清本地、一个清云端）与 `-c/--config`（file_id 属于 API key，多套配置时要能指定用哪份）；`_purge_remote_files()` 用 `OpenAILLM(api_key/base_url/timeout/max_retries 取自配置)` 建客户端（`llm.py` 新增 `files_client()` 公开访问器，就是按事件循环缓存的 `_client()`），跑在 `aio.run` 里，用完 `client.close()`；**无 api_key 直接报错返回 1**，列不出文件（网络/鉴权）也返回 1 —— 清空失败要能被脚本发现，不能假装成功；有删除失败同样退出码 1。
  - 本地副本与 `__meta__.files` **不做修改**：旧 file_id 下次请求 400 → `loop._downgrade_file_blocks` 降级内联 + 重传，自愈链路本就存在（沙箱里没有可清理的会话记录，硬改会话文件风险更大）。
  - 验证：`tests/test_files.py` 新增 5 例（`purge_remote_files` 全删 / 单个失败继续；CLI 接线：走 `-c` 的 key、用完 close、空 key 不建客户端、有失败退出码 1、列出失败退出码 1 —— 全部 stub 掉 LLM 与 purge，**不碰网络**）；真机 `pie files gc --all` 在远端为空时输出「已删除 0 个」退出 0；全套 19/19、`for t in tests/test_*.py` 全绿。
  - ⚠️ 踩坑（值得记）：`Config()` 的 `api_key` 默认值是**本部署的真实 key**（`config.DEFAULT_API_KEY`），所以「配置里没写 api_key」≠ 空 key；写测试时若只 stub 一半，`--all` 会**真的**去删线上文件 —— 测试必须把 `cli.OpenAILLM` 与 `cli.purge_remote_files` 一起换掉，并用显式 `api_key = ""` 构造「无 key」场景。

- **修 `pie -p 你好` 收尾时那段 `RuntimeError: generator didn't stop after athrow()` 噪音**（用户报告）：新模块 **`aio.py`**（`run()` / `close_asyncgens()` / `event_loop()`），把 CLI 所有同步入口的 `asyncio.run` 换成 `aio.run`（`session.turn` / `loop.run_agent` / `tools.dispatch` / cli 的两处 `fetch_models`），`run_tui` 也改为自建循环（`App.run(loop=…)`）以便退出前收尾。
  根因：openai 的流式响应读到 SSE `[DONE]` 就**就地 break**，httpx2/httpcore2 那串「响应字节流」异步生成器（`AsyncStream.__stream__` → `SSEDecoder.aiter_bytes` → `Response.aiter_bytes`/`aiter_raw` → `PoolByteStream.__aiter__` → `HTTP11ConnectionByteStream.__aiter__` → `safe_async_iterate` → `_receive_response_body`）会一直挂起在 yield 上；`asyncio.run` 收尾的 `loop.shutdown_asyncgens()` 按 `loop._asyncgens`（WeakSet，顺序随地址漂移）**一次性** aclose 它们，一旦「内层先关」httpcore2 的 `safe_async_iterate` 就抛这个 RuntimeError，被默认异常处理器打成一大段 Traceback（连接其实早已正确释放，纯噪音）。
  修法：收尾前自己关一遍——分多轮、每轮先摘空集合再逐个 `aclose()`、单个失败留给下一轮（实测 2 轮清空），与顺序无关；长驻循环（TUI）里这些生成器本来是 GC 逐个回收关闭的，所以只有一次性 `asyncio.run` 会犯。
  验证：`pie -p 你好` ×6 次 stderr 全空（修前必现）；带工具的多步任务、TUI（pty 驱动）一整轮 + `/stop` 取消再退出，均无 Traceback / 无 asyncgen 噪音；`tests/` 44/44。

- **latte 代码块高亮主题改为 `solarized-light`**（接上条；**用户手改** `CATPPUCCIN_LATTE.code_theme`，由 `friendly`（底 `#f0f0f0`）换成 `solarized-light`（底 `#fdf6e3`））：同步更新 `theme.py` 注释、`tests/test_tui.py::test_markdown_code_styles`（浅色变体的 fence 底色期望改为**从 `palette.code_theme` 派生**，不再写死 `#f0f0f0`——以后再换高亮主题不必改测试）以及本文件 / `MEMORY.md` 里已过时的 `friendly` 描述。全套 44/44。

- **浅色变体的代码块高亮主题也换掉**（接上条；用户问「Rich 的 code_block 默认 theme 是啥」（答：pygments `monokai`，`#272822` 底）→ 同意换）：palette 新增字段 `code_theme`（pygments 主题名），`tui._box()` 渲染 assistant Markdown 时传 `Markdown(code_theme=palette.code_theme)`：mocha = `monokai`（= Rich 默认，等于不动）、latte = **`friendly`**（底 `#f0f0f0`，替掉 `#272822` 黑块）。顺带把 `Theme.markdown_styles()` 改成**原样使用** `markdown_code`（不再自动拼 `bold`，否则用户写的 `"bold cyan"` 会变成 `"bold bold cyan"`）——latte 的 `markdown_code` 由**用户手改**为 `"bold cyan"`（去底色、只留青色粗体字）。
  验证：`tests/test_theme.py::test_code_theme_follows_variant`（深变体高亮背景亮度 < 0.3、浅变体 > 0.7）+ `tests/test_tui.py::test_markdown_code_styles`（真机：mocha 代码块底 `#272822` / latte `#f0f0f0`，且 latte 的 `markdown.code` 无背景）；全套 44/44。

- **浅色变体最小 Markdown 覆盖**（接上条回退；用户：「对 CATPPUCCIN_LATTE 使用最小改法」）：只给 `catppuccin-latte` 覆盖代码样式两键（`markdown.code` / `markdown.code_block` → `#4c4f69 on #e6e9ef`），把 Rich 默认的硬编码黑底换成浅灰；其余 markdown.* 保持 Rich 默认（ANSI 具名色，由终端自行映射），**深色变体不注入**。实现：palette 字段 `markdown_code`（空串 = 不覆盖）+ `Theme.markdown_styles()`；`PieApp.on_mount` 非空时 `console.push_theme(RichTheme(..., inherit=True))`。注意：**带语言标注的代码块（fence）底色来自 `Syntax(..., theme="monokai")` 的 token 自带背景（`#272822`），本覆盖治不到它**（要治得另设 `Markdown(code_theme=...)`）。验证：`tests/test_theme.py::test_markdown_styles_are_minimal` + `tests/test_tui.py::test_markdown_code_styles`（mocha → `black`；latte → `#e6e9ef`）；全套 43/43。

- **按用户要求回退 Markdown 主题化**（接上面两条，用户指令「恢复一下 CATPPUCCIN_MOCHA 之前的配色」→ 选「只恢复 Markdown 渲染」）：把 Markdown 渲染交回 Rich 默认（黑底青行内代码、品红标题、青表格线、亮蓝链接），删除 `Theme.rich_styles()` 与 `code_bg`/`code_fg`/`code_theme` 字段、`PieApp.on_mount` 的 `console.push_theme(...)`、`_box()` 的 `code_theme=` 参数。**保留**本轮新增的主题族 + 终端背景探测（`catppuccin` 族 / `termbg` / latte 变体）；用户配置固定为 `theme = "catppuccin-mocha"`。测试：删 `test_rich_styles_follow_palette` / `test_code_block_theme_follows_variant`，`test_markdown_styles_come_from_theme` 改为 `test_markdown_uses_rich_defaults`（断言 Rich 默认未被覆盖）；全套 42/42。

- **代码块（围栏）高亮主题随变体（抹掉浅色模式下的黑色代码块）**（用户截图：「浅色模式下这些块的背景还是太深」）：fence 不走 `rich_styles`，而是 Rich 的 `Syntax(..., theme=...)`，高亮 token **自带背景色**、会盖过 `markdown.code_block` 样式；`Markdown` 默认 `code_theme="monokai"`（背景 `#272822` 近黑）。新增 palette 字段 **`code_theme`**（mocha = `monokai`、latte = `friendly`，背景 `#f0f0f0`），`tui._box()` 渲染 assistant Markdown 时传 `Markdown(..., code_theme=palette.code_theme)`。验证：探针 latte 下 fence 背景 `#f0f0f0`、mocha 下 `#272822`；新增 `tests/test_theme.py::test_code_block_theme_follows_variant`（高亮主题背景亮度：深变体 < 0.3、浅变体 > 0.7）+ `tests/test_tui.py` 的 fence 渲染断言（浅色变体下背景 RGB 均 > 180）；全套 44/44。
- **一套主题适配深色/浅色终端：主题族 + 终端背景探测**（用户提议「可以一套主题适应深色终端和浅色终端吗」）：palette 原本只有深色设计，浅色终端下要么无色可用、要么颜色不可读（前面两轮一直在为此做妥协）。现在改为**主题族**：
  - 新模块 `termbg.py`：`detect_dark_background()`（lru_cache，进程内只探一次）= **OSC 11 查询 → COLORFGBG → None**。OSC 11 直接读写 `/dev/tty`（不碰 stdin/stdout，临时关规范模式/回显、0.2s 超时、读完恢复），所以管道的 shell 里也能用；纯函数 `parse_osc11`（认 `rgb:`/`rgba:` 的 2/4 位分量，按 sRGB 加权亮度 < 0.5 判深色）与 `parse_colorfgbg`（背景索引 < 8 为深色）。
  - `theme.py`：新增浅色变体 **`CATPPUCCIN_LATTE`**（catppuccin 官方 latte 色；图标提为 `_SHARED_ICONS` 两个变体共用）；`THEME_FAMILIES = {"catppuccin": (MOCHA, LATTE)}`；`get_theme(name, dark=None)` —— 族名按 `dark`/探测选变体（探测不到按深色），具体变体名固定；`DEFAULT_THEME_NAME = "catppuccin"`。
  - `Theme.rich_styles()` 相应改为**用 palette 的前景色**（标题/链接 = accent、引用/列表/表格线 = muted、代码 = `code_fg on code_bg`）——palette 现在与终端匹配，可以放心上色，之前「非代码元素一律无色」的妥协不再需要。
  - `tui.py`：`PieApp.__init__` 探测一次（`self.dark_bg`）→ 选 palette；`on_mount` 据此选 Textual 主题 `ansi-dark`/`ansi-light`（两者 background 都是 ansi_default → 仍透明）。`Config.theme` 默认改为族名；**用户配置也已由 `catppuccin-mocha` 改为 `catppuccin`**（否则仍是固定深色）。
  - 验证：新增 `tests/test_theme.py`（5 例：OSC 11 解析 / COLORFGBG 解析 / 族选择 / 变体差异 / rich_styles 跟随 palette）；`tests/test_tui.py` 的 markdown 用例改为断言样式来自 palette 且不含 ANSI 具名色（`_dummy_session` 固定 `catppuccin-mocha`，不让测试依赖环境探测）；全套 43/43；探针：`theme="catppuccin"` 在本机（COLORFGBG=0;15）→ palette `catppuccin-latte` + Textual `ansi-light`。

- **Markdown 样式全面收进主题（终结 Rich 默认的 ANSI 具名色）**（同日后续，接上一条）：上一条只接管了**自带底色**的行内代码/代码块，标题/引用/列表/表格/链接仍走 Rich 默认：`markdown.h2 = underline magenta`、`h3 = bold magenta`、`h4 = italic magenta`、`block_quote = magenta`、`list / item.number / table.border = cyan`、`table.header = not bold cyan`、`link = bright_blue`——用户截图里 `## 验证` 呈品红下划线即此。
  - 原则：**只有自带底色的元素指定颜色**（用 palette 的 `code_fg on code_bg`）；其余元素**只给 text-style、不指定前景色**（h1/h2 = bold underline、h3-h6 = bold、link = underline、table.header = bold、引用/列表/表格线 = none）——随终端默认前景走。理由：palette 的前景色是给深色背景设计的，直接搬到浅色终端上会不可读（比 ANSI 具名色更糟）；不加色则深浅终端都稳。
  - 验证：`tests/test_tui.py::test_markdown_styles_come_from_theme`（改名并扩展：断言除 code/code_block 外的样式串不含 cyan/magenta/blue，h2 == "bold underline"，真机渲染后 `markdown.h2` 无背景色），`uv run python tests/test_tui.py` 11/11；探针逆推：h2/引用/列表/表格均 `color=None`，仅行内代码 `#cdd6f4 on #313244`。

- **Markdown 行内代码/代码块配色修复**（用户截图报告）：根因不在 Textual 主题，而在 **RichLog 用 `App.console` 渲染，而该 console 从未设过 theme** → Rich 的 `DEFAULT_STYLES` 生效（`markdown.code = bold cyan on black`、`markdown.code_block = cyan on black`），行内代码/代码块被画成**黑底 + 终端 ANSI 青**。深色终端下尚可，浅色终端把 ANSI 青映射成暗青后对比度极低。推论：此前 `theme.py` 的 palette 对正文 Markdown 颜色**一直没有影响**（RichLog 内容不走 Textual CSS）。
  - 修法：`Theme.rich_styles()` 返回 `{"markdown.code": "bold <fg> on <bg>", "markdown.code_block": "<fg> on <bg>"}`（新增 palette 字段 `code_bg` / `code_fg`，catppuccin-mocha 取 surface1 `#313244` / text `#cdd6f4`），`PieApp.on_mount` 用 `self.console.push_theme(RichTheme(..., inherit=True))` 注入。只覆盖这两个**自带底色**的元素——标题/引用/链接/表格没有自设背景，保留 Rich 默认的 ANSI 具名色（交给终端按明暗自行适配最稳）；若给它们填 palette 的浅色 hex，反而会在浅色终端上不可读。
  - 验证：新增 `tests/test_tui.py::test_markdown_inline_code_uses_palette`（console 主题 + 真机 RichLog 渲染出的行内代码 segment 的 bg/fg == palette 值），`uv run python tests/test_tui.py` 11/11，全套 38/38。
  - 备注：代码块（fence）本体走 Rich 的 `Syntax(code, theme="monokai")`，span 自带 monokai 底色（`#272822`）+ 亮色字（深底浅字，浅色终端下也可读），故未动；`markdown.code_block` 的覆盖只在语法高亮失效时兜底。

- **图片改走 Files API：上传一次拿 `file_id`，历史里只留 `file` 块**（用户提议，依据 [Files API 文档](https://api-docs.deepseek.com/guides/files_api/)）。起因：`read` 到的图原先一律内联 base64，而那条 ImageMessage 会留在历史里 → **同一张图每轮请求重发**（3 MB 图 ≈ 4 MiB body/轮），且受 inline 的「单图 32 MiB / body 48 MiB」限制。
  - **实测先确认了三件事**：① `{"type":"file","file_id":…}` 确实让 deepseek-flash 看到图；② **prompt_tokens 与内联完全一致**（计费按尺寸、单图 ≤1024）→ 换法省的是**请求体/重复传输/上限**，不是钱；③ 同一张图上传两次得到**两个不同 file_id**（服务端不去重）→ “不重传”只能靠本地记录，这也给下面“按会话记”补了硬理由。
  - 新模块 `files.py`：`hash_id = img-<sha256[:16]>`（形状对齐 context 的 `turn-<hash>`）；本地内容寻址副本 `~/.pie/files/<hash_id><ext>`；**先落副本、再从副本上传**（不变量：服务端那份 == 本地这份）；`ImageStore.ensure()` 命中（同 hash + 同 `base_url`/`key_fp` + 未过期）就用旧 id，否则上传并**就地写入 `Session.files`**。
  - **记录放每会话的 `__meta__.files`，不建全局缓存**（用户提议）：`Session.save()` 本来就是全量重写，加一个字段零成本，于是 last-wins/墓碑/跨进程追加/去重压缩**全部消失**——“重传、换 key、失效”只是内存 dict 就地覆盖。代价：跨会话不复用（会重传一次）、`pie -p` 一次性模式没有 `__meta__`（同一次运行内仍去重）。
  - 回退与自愈：上传失败 / 模型不支持 `file` 块 / `files_api=false` → **静默回退内联 base64**（行为与从前一致）；请求报 `400 … file_ids do not exist or are not created under your account` → `_downgrade_file_blocks()` 把历史里的 file 块**就地降级成内联**（字节从本地副本取）并重试一次（`is_stale_file_error` 认这个错）。
  - 配置：`files_api = true`、`files_ttl_days = 30`（上传时带 `expires_after`，走 `extra_body`；0 = 永久）；`Config.tool_defaults()` 派生 `read._max_image_bytes`（内联 32 MiB ↔ Files API 64 MiB），loop 改用它而不是裸 `cfg.tools`。
  - 顺手修一个独立问题：**图片 token 估算原先是 `min(12000, 800 + base64长度/256)`**——服务端规则是**单图 ≤1024**，大图被估成 1.2 万（上下文虚高、提前触发压缩）→ 现在封顶 1024（`context.IMAGE_TOKENS_MAX`），file 块也按同一上界估。
  - CLI：`pie files list [--all]`（各会话记的图 / `--all` = 调 Files API 列云端全部上传件）/ `pie files gc [--delete] [--all]`（本地副本是无状态扫描回收：副本**跨会话共享**，所以删会话不会自动删副本；服务端那份默认由 `expires_after` 过期，`--all` 才主动清空）。
  - 验证：新增 `tests/test_files.py`（12 例：hash/幂等落盘/上传一次再复用/换 key·端点·过期·主动失效→重传/失败返回 None 且不写记录/关闭时不落副本/`is_stale_file_error`/parts 优先 file 块与回退 inline/降级重写历史/`gc` 只删未引用），`pytest tests/` 37/37；真机端到端：`pie -p "看 half.png 说颜色"` 后服务端多一个 `img-85fbd97740797558.png`（证明是**从本地副本上传**）、`~/.pie/files/` 出现副本、同一会话 resume 后再读同一张图**服务端文件数不变**（命中记录不重传）；另用裸 API 对拍 file 块与内联 base64 两种编码，回答一致（排除“传图变形”）。

- **Session 字段改名：`.file` → `.path`、`.fs` → `.windows`**（用户提出这三个名字容易混）。起因是准备加第三个字段 `.files`（图片 id 表），三个名字挤在一起时 `.file` / `.fs` / `.files` 完全分不清。改成按语义命名：
  - `Session.path` = 会话 JSONL 文件自身的路径（原先叫 `file`，最容易和新增的 `files` 撞）；
  - `Session.windows` = `/clear` 归档的历史窗口块列表（原先叫 `fs`，而这个缩写同时被用作压缩管道里的形参名）；顺带把 `context.py` 里 `compact(session=..., fs=)` / `maybe_compact(fs=)` / `_compact_session(..., fs)` 的形参一并改名 `windows`。现在属性名与目录名（`context.WINDOWS_DIR = ~/.pie/windows`）一致。
  - **持久化也改名但不迁移用户文件**：`__meta__` 现在写 `"windows"`，读时 `data.get("windows") or data.get("fs")` → 旧会话直接可用，不重写、不报错。
  - 顺手把两处提示文案里的黑话去掉：「已切换新窗口（归档 N 个历史窗口块，文件在 ~/.pie/windows/）」。
  - 验证：新增 `tests/test_session.py`（4 例：save 写 `windows` 键 + 置 `path` / load 还原 `path`+`windows` / **旧 `fs` 键兼容读** / `/stat` 的「会话文件」行走 `path`），`uv run python tests/test_session.py` 4/4、`pytest tests/` 24/24；`pie -p "..." --mode json` 端到端吐出的 `session` 路径正常。
  - 陷阱记录：`Session` 是 dataclass，机械替换 `self.file`/`session.file` **漏掉了字段声明 `file: Path | None = None` 与 `Session.new` 里的 `file=file` 构造参数** → 表现为「`save()` 后才凭空出现 `self.path`」（写路径时不再是 dataclass 字段）。改名这类事必须把“字段声明 + 构造调用 + 形参”一起扫，不能只替 `self.x`/`obj.x`。

- **配置改名 + 上下文预算算对：`max_seq_len` → `context_window`、`max_tokens` → `reserved_tokens`、压缩水位比例移进 `[compaction]`**。起因是用户会话撞了 400：`This model's maximum context length is 1048576 tokens. However, you requested 1049513 tokens (793513 in the messages, 256000 in the completion)`——**只超 937 个 token**，但整个回合被打断。查下去发现 pie 对「上下文」的理解和服务端不一致：
  - **服务端预检是 `输入 tokens + max_tokens ≤ 窗口`**：`max_tokens` 是「最坏情况下给输出留的位置」而不是已生成量，所以**可用输入预算 = 窗口 − max_tokens**。而 pie 的软阈值是 `max_seq_len × 0.8 = 1,024,000`（还建立在 `max_seq_len = 1,280,000` 这个比真实窗口 1,048,576 更大的假设上）→ 793,513 的输入判定为「还早，不压」→ 带着 256,000 的预留发出去 → 400。
  - 改名（用户指定）：`Config.max_tokens` → **`reserved_tokens`**（语义从“单次生成上限”纠正为“为输出预留”）；`Config.max_seq_len` → **`context_window`**；`context_soft_ratio` / `context_target_ratio` 从 Config 顶层移入 `CompactionConfig`，改名 **`soft_ratio` / `target_ratio`**。
  - **语义修正**：两个比例现在相对 `context_window - reserved_tokens`（新增 `Config.context_budget()`），不是整个窗口。`target_limit()` 也随之简化——以前是 `soft × target/soft` 的间接换算，现在同一份预算上各自乘比例。
  - `Session.usage_report()` 的分母改成 `context_budget()`（并多一行写明「输入预算 = 窗口 − 输出预留」）；system prompt 的「当前模型最大上下文长度」也改成「窗口 / 预留 / 可用输入预算」三件套（`config._context_line()`）。
  - 迁移：`Config.load` 把旧键 `max_tokens`/`max_seq_len` 接到新键上，旧顶层 `context_*_ratio` 接进 `[compaction]`（新键优先，同名并存时不被覆盖）；CLI `--max-tokens` 保留为 `--reserved-tokens` 的别名（`dest=reserved_tokens`）。顺手修一个往返 bug：TOML 没 null，`reserved_tokens = None`（不发送 max_tokens）以前会被 `_toml_dump` 跳过、重启后静默回落到 256000 → 现在 `save()` 写成 `"auto"`（`load` 认它）。
  - 用户配置已重写为新键名，并把 `context_window` 从 1,280,000 校正为**实测值 1,048,576**（备份在 `/tmp/config.toml.bak`）。实测值来源：`GET /models` 不返回 context_length，只有超限报错里带。
  - 验证：新增 `tests/test_config.py`（10 例：预算公式 / 比例相对预算 / 阈值覆盖 / auto 语义 / 旧键迁移（含无 `[compaction]` 段的旧比例）/ 新键优先 / save-load 往返 / **真实窗口回归（793,513 ≥ 软阈值）** / usage_report 分母），`uv run python tests/test_config.py` 10/10、`pytest tests/` 20/20；真实配置实测：窗口 1,048,576 − 预留 256,000 = 预算 792,576，软阈值 634,060（80%）、目标 435,916（55%）——上次那枪 793,513 现在会触发压缩。

- **简洁模式（`[tui] lean = true`）的工具行：状态标记挪到行首、删掉自定义 renderable `_LeanLine`**。起因是用户反映工具调用/结果单行一路顶到日志区左右边缘，与下面带盒子的回复（正文内缩「边框 1 + 内边距 1」= 2 列）不齐。推演后发现**留白和「标记放哪」是同一个问题**：
  - `_LeanLine`（83 行的类：自定义 `__rich_measure__`/`__rich_console__`，按可用宽度先截摘要、再把 ✅/❌ 贴到行尾，还用 `set_cell_size` 手算省略号）的全部存在理由，就是 docstring 里那句「Text 只能整行截断，超长命令会把**行尾**的 ✅/❌ 一并截掉」。把状态标记放到**行首**后，`Text` 的右截断天然保住它 → **整个类可以删**（连带 `Measurement`/`set_cell_size` 两个 import 与 `LEAN_RIGHT_MARGIN`）。实测：`Text(no_wrap=True, overflow="ellipsis")` 在任意宽度下恒 1 行、行首标记恒保留、`…` 也是白送的。
  - 但 **`_single_line()` 必须保留**，只是服务对象从 `_LeanLine` 变成 `Text`：`no_wrap` 只管「不按空白回绕」，真换行（heredoc / 换行串联的 `&&`）仍强制断行；tab 会被 Rich 按制表位展开、而 `cell_len` 只算 1 格 → 撑破宽度。实测两者都复现。
  - **留白**：不用 Panel（实测 `Panel(_LeanLine, box=SIMPLE, padding=(0,1))` 恒产 **3 行**，上下各多一行空白；用 `Box("")` 造无边框 Box 直接抛 `ValueError`；且 `child_width = width - 2` 写死按边框算，语义是「边框里的内边距」）——用 `rich.padding.Padding`（就是「无边框 Panel」，`Padding.indent()` 本来就干这个）。
  - **不再写魔法 2**：`#log` 的 CSS padding（=1）与这件事无关（工具行与盒子共享同一内容区，改它不破坏对齐）；真正要对齐的是**盒内正文**的列偏移 = Panel 边框 + 内边距。所以把盒子几何抽成 `_BOX_BORDER = 1` / `_BOX_PADDING = (0, 1)`，`BOX_INSET = _BOX_BORDER + _BOX_PADDING[1]`，`_box()` 与 `_render_assistant_stream()` 都改用 `_BOX_PADDING`，工具行/续行块用 `BOX_INSET`——以后改盒子内边距，工具行自动跟着走。
  - 于是 `_lean_line() -> Padding(Text, (0, BOX_INSET))`（约 12 行）；`_lean_detail` 也改成「块内缩进留在文本里 + 外面套同一个 Padding」，首行 `→` 正好落在工具行图标的下一列。附带修好一个真 bug：`_copy_source` 现在会剥掉 `Padding`（源文本 = 里面真正的内容），lean 行从「按显示行拼接」的回退升级成「按源文本切」——之前工具行带 2 列留白而回退只去 1 个前导空格，**框选复制每行会多 1 个空格**，现在逐字节干净。
  - 代价（有意为之）：结果行的图标改成「执行结果」本身（✅/❌/⏹），不再用工具身份图标（`↳`/`$`）——见下一条（两处由此合并成一套字形）。
  - 验证：`tests/test_tui.py` 的 `test_lean_tool_lines_are_padded` 重写为断言「标记在行首 / 正文列 == 盒内正文列（`BOX_INSET`）/ 无边框字符 / 超长行单行且行首标记保留 / 窄宽（24 列）下标记不丢 / 框选复制不带留白」，10/10 通过；`self_check()` OK。
- **「执行结果」字形统一：`icon_ok` / `icon_error` / `icon_cancelled`（合并 lean 标记与结果框图标）**：上一步把 ✅/❌/⏹ 收进 Theme 时先落地为 `icon_lean_*`，但随即发现它们与既有的 `icon_tool_result`(↳) / `icon_error`(✗) 是**同一件事**——都在回答「这次执行结果如何」——于是合并成三个字段：
  - `icon_lean_ok` + `icon_tool_result` → **`icon_ok`**（✅）；`icon_lean_fail` + `icon_error` → **`icon_error`**（❌，原来是 ✗）；`icon_lean_cancelled` → **`icon_cancelled`**（⏹）。取舍：`icon_error` 保名换字形（✅/❌/⏹ 成一套，盒子模式下 `✅ read` 与 `❌ read` 也成对；想回 ✗ 改 theme 一行）。
  - `role_icon`：`tool_result → icon_ok`、`error → icon_error`，新增 **`cancelled → icon_cancelled`**（`role_border("cancelled")` = system 灰）。于是盒子里 `↳ read` 变成 `✅ read`、失败的变成 `❌ read`，而 `_tool_result_box` **必须按 role 取图标**（原先是 `tool_icon(tool, result=True)`，那条路在合并后会把失败的也画成 ✅）。
  - `tool_icon(name)` 去掉 `result=`（只剩**调用**形态）；结果框走新 accessor **`result_icon(role, name)`**（按 role 取状态字形，`tool_result_icons` 仍可 opt-in 覆盖）。`lean_mark(status)` 改成映射到同三个字段。
  - 顺手补一个真缺口：**`_tool_failed_role` 现在识别 `/stop` 取消**（内容 = `CANCEL_TEXT`）→ role `cancelled`（以前落回 `tool_result`，合并后会画成 ✅；shell 侧的 `[exit=cancelled]` 也从 `system` 改为 `cancelled`，两模式对取消的处理终于一致：低调灰 + ⏹）。
  - 另：接手时 `theme.py` 里 `icon_tool_result="✓"` 少了个逗号（手改到一半）→ 本轮重写顺手修掉。
  - 验证：`tests/test_tui.py` 10/10 + `self_check()` OK；headless 双模式对拍（call/success/fail/cancel/error 五种）→ 盒子标题 `✧ read / ✅ read / ❌ read / ⏹ read / ❌ shell [1] / ⏹ shell`、简洁单行 `✧ read a.txt / ✅ read a.txt / ❌ read a.txt / ⏹ read a.txt / ❌ shell ls / ⏹ shell sleep 100` 与 `❌ 出错啦` 一致。

## 2026-09-10

- **tui.py 分层重构（1658 → 989 行）+ 回归测试落地**：起因是「一个文件装了 5 个子系统」——ast 统计显示 `PieApp` 772 行（47%）、复制机制 353 行（21%）、`build_css` 135 行、断行/清洗 119 行、其余是控件与常量。分两步做：
  - **纯搬家**：`textkit.py`（CJK 断行 + 转义清洗，纯函数、不依赖 Textual；`install_cjk_wrap()` 改由 tui.py 显式调用）、`logcopy.py`（`SelectableRichLog` + 「显示行 → 源文本」对齐/切片）、`build_css` 并入 `theme.py`（与配色数据同处）；顺带删掉 tui.py 中已死的 import（`re` / `Strip` / `VerticalGroup` / `Header` / `Footer`）。
  - **去重**：工具结果渲染（shell exit code 解析 + 失败判定）此前在实时事件与 resume 历史里各写一份（且只有历史版截断超长）→ 合 `_render_tool_result(..., truncate=)`；工具调用渲染（dict ↔ JSON 串）→ `_render_tool_call` + 纯函数 `_format_tool_args`；「先固化流式正文再写下一个盒子」→ `_flush_assistant_text`；`/help` 改由 `PALETTE_COMMANDS` 生成（此前两处维护，文案已开始漂移）；22 处提示盒改走 `self._notify(text, role=...)`；CSS 滚动条 5 行块（#log / #assistant-stream）→ `build_css` 里一个 `scrollbar` 变量；选中区间排序（`_selected_text` / `render_line`）→ `logcopy._sel_range`（当日后续随 logcopy 并回改为 `SelectableRichLog._sel_range`）。`_append_event` 65→35 行、`_command` 102→86 行，PieApp 772→729 行。
  - 顺手修一个真 bug：`/model refresh` 此前不传 `notify=True`，拉取失败/成功都没有任何反馈（静默）——现会给反馈。
  - **后续（同日）**：按用户偏好把 `logcopy.py` 又并回 `tui.py`——它只服务这个 App，单开模块多一跳；`tui.py` 现 1388 行，内部用 `# ---- xxx ----` 分区（应用编排 / 日志区控件）。`textkit.py`（纯函数、无 Textual）与 `theme.py` 的 `build_css` 保留在外。tests/test_tui.py 9/9。
- **补 TUI 回归测试**（`tests/test_tui.py`，无 pytest 依赖，`uv run python tests/test_tui.py`）：断行（纯 ASCII 与 Rich 原实现逐字节一致 / 中文填满宽度 / 英文词不切开 / 3000 例随机混排断点合法）、复制（12 种 Markdown + 3 种纯文本 × 3 种终端宽整盒复制 = 源文本、部分行选择无换行、真机鼠标拖拽走剪贴板）、渲染（工具参数 dict/JSON 串归一、shell exit code 进标题、超长只历史回放截断）、命令冒烟（/help /status 未知命令与参数）。重构全程以它为安全网：9/9 通过。

- **CJK 友好断行：全角字之间也可断**（TUI 显示层）：Rich 只在空白处断行（`rich.text.divide_line` → `rich._wrap.divide_line` 用 `\s*\S+\s*` 分词）——中文长句没有空格 → 整段被挪到下一行、上一行大片留白（实测宽 74 下只用 42 格，用户截图“第一行很短”即此）。做法：在 `tui.py` 把 `rich.text.divide_line` 换成 `_cjk_divide_line`（模块导入时安装），只改**分词单位**——全角字（`cell_len == 2`）各自成一个 token、非全角串仍按词，其余逻辑（放不下就换行、比整行宽则 `chop_cells` 硬折、`fold=False` 时整体挪行）与 Rich 逐行对齐；于是“英文词尽量不断、中文可逐字折”。纯 ASCII 文本直接交回 Rich 原实现（逐字节不变），Rich 接口不在时静默跳过。踩坑：零宽断点（U+200B）行不通——Python `\s` 不匹配它，Rich 的分词认不出来（实测过）；`chop_cells` 在宽度极小时会产出空块 → 断点需去重（fuzz 发现）。验证：用户那句中文长句宽 74 下首行 42 格 → 73 格；纯 ASCII 与 Rich 原实现一致（4000 例随机串 × 4 宽 × fold 两种）；30000 例随机混排（中文/全角标点/emoji/韩文/日文/ASCII）× 9 种宽度无重复/越界断点；`keep-intact-token` 这类英文词不被切开；真机 `PieApp` 挂载（空会话）+ 盒子渲染/复制、窄表格内长 CJK 单元格逐字折行均正常；复制探针 6 组（46/74/104 宽）全绿。

- **框选复制长行不再断行/丢空格**（`#log` 的 `SelectableRichLog`）：根因是复制按 `RichLog.lines`（**软换行后的显示行**）逐行拼接——长行在盒内被 Rich 折成多行，拼出来就是多行；而且 Rich 在盒内换行点会**直接吃掉那个空格**（实测 `Panel(Text('aaaaaa bbbb cccc dddd…'), padding=(0,1))` 在宽 24 下折成 `'aaaaaa bbbb cccc'` + `'dddd…'`，空格没了），所以光按显示行拼接既多换行又丢空格。改法：每次 `log.write()` 记下「源文本 + 显示行 → 源文本字符区间」的对齐表（`_CopyEntry`），复制时按字符区间**切源文本**（同一源文本的相邻显示行合并成一个切片 → 被吃掉的空格与真实换行随切片一并还原），拿不到映射才回退老的按行拼接。源文本取法：`Panel` → 盒内正文；`Text` → `.plain`（先 `expand_tabs()`，与 Rich 显示时 tab_size=8 的展开一致，否则含 tab 的输出对不上）；`Markdown` → **宽渲染（4096）后的纯文本**（`.markup` 不行：渲染会去围栏/加缩进/合并段落，与显示行对不上；宽渲染拿到的既是「屏幕上的文字」又保持每条逻辑行完整，长代码行/段落复制就是一整行）。对齐失败（逐行匹配不上、或源文本尾部有残留）返回 None 走回退，不猜。顺带修回退路径：整行是盒边框时（哪怕只选到半截 `╰───`）统一丢弃。验证：`run_test()` 下 4 组探针——长英文行/长中文行/超长单词/多行含空行/工具结果盒/JSON 参数框/跨两条写入/含标题边框的选择/短行，复制结果逐字节等于源文本；assistant 的 Markdown 六态（段落、围栏代码块、加粗与行内代码、列表、标题、表格）对齐成功且整盒复制 = 渲染后的完整逻辑行（长命令保持一行）；真机 `Pilot.mouse_down/mouse_up` 拖选一整行长行，剪贴板内容 = 原文一行；`self_check()` OK。

  **后续：两处对齐失效（用户实测反馈后修）**。① **Rich 给 Markdown 列表续行加悬挂缩进**（`• ` 项折行后续行多 2 格、`1. ` 多 3 格），源文本里没这 2 格 → 逐行匹配直接对不上，**整个答案框**回退成按显示行拼接（用户截图“第一行很短”就是这个）。② **Rich 给引用（blockquote）的每条显示行重复加 `▌ ` 装饰**（源文本里只有首个逻辑行有）→ 同样对不上。改法：`_split_log_row` 把**行首空白一律不计内容**（只计入 x0，供单元格→字符换算），`_align_spans` 在整行匹配失败时再试「去掉行首 `▌` 装饰」，并把被忽略的字符数记进 spans（`(start, end, off)`）——切片仍按源文本精确切，装饰不会进复制内容；两层都不命中才回退。教训：对齐不能只靠“源文本选择得对”+“对不上就回退”兵底，**必须容忍渲染产生的显示装饰**，否则一个列表项就能让整条消息回退。已知仍回退的一种：Markdown 分隔线 `---` 被渲染成**随宽度铺满**的规则线，不存在与宽度无关的源文本，回退后复制到与显示等宽的那一行（可接受）。验证：`run_test()` 下 3 种终端宽（46/74/104）× 12 种 Markdown 形态（段落/无序列表/有序列表/嵌套列表/块引用/围栏代码/缩进代码/标题/表格/加粗行内代码/分隔线）+ 3 种纯文本形态（长行/含 tab 缩进/shell 结果盒），除分隔线（INFO）外全部逐字节等于源文本；用户截图里那个列表项 case 三种宽下均复制为单行。

- **各 role 图标集中到 Theme**：原先盒子标题前缀（▎/⚙/↳/✗）散落在 tui.py 的 `_box(..., icon=...)` 调用点上，每处重复且只靠调用方自己保证一致。现 Theme 新增 `icon_user/icon_assistant/icon_tool_call/icon_tool_result/icon_error/icon_system` 六个字段 + `role_icon(role)` 取用方法（与既有 `role_border(role)` 同构，未知 role 回退 system），`_box(icon=None)` 默认用该 role 的主题图标，调用点不再传字面量；`_render_assistant_stream()` 的流式面板标题（原硬编码 `"▎ pie"`）改走 `palette.role_icon("assistant")`。图标与边框色语义解耦：工具结果失败时边框染红（role="error"）但图标仍是 ↳，故新增 `_tool_result_box(palette, body, title, role)` helper 显式指定图标，供实时 / resume / !shell 三处共用。顺带统一：`role="error"` 的普通错误框（/compact 参数错、未知命令、回合异常）此前有的带 ✗ 有的不带，现统一 ✗。验证：headless `run_test()` 跑 `_submit` / `_append_event`（tool_call/tool_result/answer）、`_show_shell_result`、`_fail_turn`、`_render_history`，日志盒子标题依次为 ▎ 你 / ⚙ read / ↳ read / ↳ shell [1] / ▎ pie / ↳ shell [0] / ↳ shell [1]（无输出）/ ✗ 出错；流式面板标题为 ▎ pie。

- **图标支持按工具名覆盖**：同一工具会以「调用框」「结果框」两种形态出现（标题都是工具名），所以没有把 `icon_tool_call` 直接改成 dict（role 默认值仍需保留、与 `role_border` 对称），而是新增两张可选覆盖表 `Theme.tool_icons` / `Theme.tool_result_icons`（工具名 → 图标）+ `Theme.tool_icon(name, *, result=False)`：命中就用配置值，未命中回退对应 role 默认图标（⚙ / ↳）；命中且值为 "" = 该工具刻意不显示图标（与未命中相区别）。两张表对应两种形态、互不覆盖，保留 ⚙ vs ↳ 的形态区分。`_box` 新增 `tool=` 参数（优先级：显式 icon > 工具图标 > role 图标）；`_tool_result_box` 改用该工具的 result 图标（失败染红框时依旧用结果图标而非 ✗）。默认主题两张表为空 → 外观与之前完全一致。dict 字段声明为 `field(hash=False)`（dict 不可哈希，不让它们参与 `__hash__`）。验证：默认主题输出与改前逐字节一致；自定义 `tool_icons={"shell": "$", "read": "R", "edit": "E"}, tool_result_icons={"shell": "$", "read": ""}` 下，实时事件与 resume 历史两条渲染路径标题分别为 `▎ 你 / R read / read（结果图标置空）/ E edit / ↳ edit（未配置回退 ↳，失败框仍 ↳）/ $ shell / $ shell [1] / ▎ pie / $ shell [0]`。

- **4 个内置工具配上默认图标**：`tool_icons={"read": "R", "edit": "E", "write": "W", "shell": "$"}`（默认主题）。选型：不用 emoji（双宽、字体不一，容易把 Panel 边框撞歪）；改用单宽、跨字体稳定的 ASCII 字母/符号，一看就懂；`$` 顺带与 !shell 里 `$ {cmd}` 的惯例一致。`tool_result_icons` 保持空 → 结果框仍是 ↳：图标在不同盒子位上语义不同：调用框的图标回答「调的是哪个工具」（标题里的工具名反而次要），结果框的图标回答「这是上一个框的产出」（工具名已在标题里），两者不混。效果：`R read {参数} / ↳ read 输出 / E edit {...} / ↳ edit 输出 / W write {...} / ↳ write 输出 / $ shell {command} / ↳ shell [0]`（失败仍 `↳ shell [1]` 红框）。若想结果框也带工具图标，把同一张表再赋给 `tool_result_icons` 即可。

- **修复 shell 输出含 ANSI 控制符时 #log 盒子画歪**（`ls --color`）：根因是 Rich 排版把不可见的转义字节也算进文本宽度（`cell_len("\x1b[01;32mAGENTS.md\x1b[0m")` = 19 对可见 9）→ Panel 量出的内容宽比实际大，顶/底边框落在 78 列而内容行右边框落在 65-68 列（RichLog 的 `min_width=78` 下不被 crop，终端直接呈现错位：内容右框在中间、外框在最右）。修复：tui.py 新增显示层清洗 `_strip_escapes()` / `_rich_text()`——先剔 ANSI 转义（SGR 保留交 `Text.from_ansi` 解成 Rich 样式，故 `ls --color` 配色保留；OSC / 光标移动 / 字符集选择 / 其余单字符转义剔除）、再剔 C0 控制符（保留 `\n` `\t`，`\r\n` 归一成 `\n`、孤立 `\r` 剔除），`_box()`（「markdown 走 `keep_sgr=False`）、`_render_stream()`、`_render_assistant_stream()` 统一走它。踩坑：**必须先剔转义再剔控制符**——OSC 以 BEL(`\x07`) 结尾，先删 BEL 会让 `\x1b]...` 的匹配吞掉其后全部文本（实测丢内容）。验证：Textual `run_test()` 下 `ls --color` 的 tool_result 盒子各行可见宽全为 78（修复前 65/68/78 混杂），且无残留 ESC；`\t` 由 Rich 自己按 tab stop 展开、宽度量得准，不必手工替换。

- **TUI 选中高亮统一（输入框 vs 日志区）**：`#input .text-area--selection` 补 `text-style: none`。根因：App 用 ansi-dark 主题 → TextArea 的 `:ansi` 规则给选中加 `text-style: reverse`（并 `background: transparent`）；`#input` 的 ID 规则虽更具体、能覆盖 background/color，但没声明 text-style，低优先级的 reverse 仍叠加 → 输入框选中呈反色，与 `#log` 鼠标框选（accent 底 + accent_text 字）视觉相反。修复后两者 style 完全一致（实测两边选中 Segment.style 均为 `#06121f on #89b4fa`）。
- **TUI 新增 Esc 手动终止（等价 /stop）**：`PieApp.action_escape()`——补全面板开着先收起（保留原有 Esc 语义），否则若 `_busy()`（回合 / !shell 在跑）则等价 `/stop` 置位 `_cancel_event`；空闲时无副作用。绑定两处：`PieTextArea` 的 escape 绑定（输入框有焦点时）与 `PieApp.BINDINGS`（`*App.BINDINGS` + escape，焦点在别处如日志区时兜底；Ctrl+Q/Ctrl+C 保留）。取消逻辑抽成 `PieApp._stop()`（`/stop` 与 Esc 共用），重复触发（已在取消中）静默忽略，避免连按 Esc 刷屏。文案同步：placeholder、忙时提示、`/help`、PALETTE 的 /stop 描述。测试：Textual `run_test()` 驱动 Esc——空闲无副作用、面板显示时仅收起面板、忙时置位取消、`set_focus(None)` 时 App 级绑定兜底。

## 2026-09-09

- **/reasoning 改为 /thinking，新增 /model 命令（运行时切换模型）**：
  - 命令改名：`/reasoning` → `/thinking`（TUI 补全面板、readline 模式、帮助文案同步）；内部配置项 `reasoning_effort`、CLI `-t/--thinking` 不变。`/thinking` 无参数时显示当前深度。
  - `/model`：无参数列出当前模型 + 可用列表；`/model <id>` 切换并持久化到 config.toml（下次启动/请求即生效）；`/model refresh` 重新拉取。切换语义 = 更新 `session.config.model` + `llm.model`（loop 每请求读 `cfg.model`，故下个请求即用新模型，无需重建 client）。非法 id（不在列表内）拒绝并提示。
  - 启动拉取模型列表：`OpenAILLM.list_models()`（GET /models，OpenAI 兼容/DeepSeek 均支持）→ `Session.fetch_models()` 缓存到 `Session.available_models`（不持久化）。TUI 在 on_mount 用后台 worker 拉取（不阻塞 UI，成功/失败各提示一行，失败可 /model refresh 重试）；readline 回退模式在 banner 后同步拉取（8s 超时，失败仅 warn）。自定义 LLM 后端无 list_models → 静默降级，/model <id> 仍可手动切换。
  - Session 新增复用方法 `set_model` / `set_reasoning_effort`（更新 config + llm 实例 + 持久化），TUI 与 readline 共用。
  - 交互拉取会真实请求端点一次（慢网络下启动延迟：TUI 后台无感；readline 最多 8s）。
- **/compact 全部还原为原始实现（auto|tools|turns）**：按用户要求撤回本轮对 /compact 的全部迭代（“配置式查看+compact_mode+all/tool/turn 词汇+← 当前 标注”），`Session.compact(mode="auto")`、TUI/readline 的 `/compact` 命令、PALETTE 静态三条候选均与 HEAD 一致：`/compact`（无参 = auto，工具级 + 轮次级）、`/compact tools`（工具级）、`/compact turns`（轮次级）。
- **修复 /stop 与超时对 agent shell 工具不生效（卡到命令自然结束）**：根因是 `create_subprocess_shell` 未建独立进程组，取消/超时路径的 `proc.kill()` 只杀 `/bin/sh`，真正干活的孙进程（如 `sleep 300`）变成孤儿并**继续持有 stdout 管道写端**；Python 3.12+ 的 asyncio 子进程 `wait()` 要等 stdio 管道 EOF 才返回 → `finally` 里 `await proc.wait()` 被卡到孙进程自然退出（实测 `sleep 300` 取消耗时 299s）。修复：`create_subprocess_shell(..., start_new_session=True)`（独立进程组，对齐 TUI !shell 的既有做法）+ 终止时 `_terminate_proc_group` 用 `os.killpg(os.getpgid(pid), SIGKILL)` 杀整个进程组（pipe 立即 EOF）→ 取消耗时 0.003s、超时路径也一并杀干净；kill 后 `wait()` 限时 3s（进程处不可中断 D-state、SIGKILL 排队时不阻塞取消/超时路径）。
- **loop._wait_cancellable 取消清理不再无限等**：取消后 `await asyncio.wait({task}, timeout=CANCEL_GRACE=3.0)` 等工具清理（asyncio.wait 不打断清理、不二次 cancel），超时则让清理后台继续、先终止回合返回 None；对 done 且非 cancelled 的 task 消费 exception 避免告警。避免个别工具清理自身真卡（如不可杀进程）时 /stop 本身不返回。
- **TUI !shell 收尾 wait 加 3s 限时**：killpg 后 `await proc.wait()` 若遇进程组内 D-state 进程会卡住 worker（UI 保持 busy），改为 `asyncio.wait_for(proc.wait(), 3)`，超时 code 标 `uninterruptible`（SIGKILL 已排队，不阻塞 UI）。

## 2026-09-02

- **read 支持图片（多模态）**：read 读图片返回机器可读标记 `[图片已读取: path=..., mime=..., size=..., dim=...]`（offset/limit 对图片无意义忽略；魔数嗅探 PNG/JPEG/GIF/WebP/BMP + 文件头解析宽高，超 READ_IMAGE_MAX_BYTES=12MB 拒绝内联）；loop 层 `_inject_read_images` 用 `tools.parse_image_marker` 解析标记 → base64 data URI → 注入 `ImageMessage`（context.py，role=user 的多模态 content parts，synthetic=True）。设计关键：OpenAI 兼容 API 图片只能放 user 消息 content parts（tool content 必须 string）；ImageMessage 不是 UserMessage 子类 → 不构成轮次边界，轮次级/会话级压缩的轮次认定、turn_count、标题、摘要提取（synthetic 排除）全部不受影响，可随所在轮/窗口一起落盘；Message.content 类型放宽为 str | list[dict]（to_api 原样透传 parts），tokens() 对图片 part 按 data URI 长度折算（封顶 12k，真实值以 provider 上报为准）；to_dict 新增 cls 字段（from_dict 还原类），synthetic 仅 True 时写出。纯文本模型收图会 400——换多模态模型即可，未加开关。

- **出厂默认常量归位 config.py**：DEFAULT_MODEL + REASONING_LEVELS/REASONING_NONE 统一由 config 定义（原在 llm.py）；llm.py 改 `from .config import DEFAULT_MODEL, REASONING_NONE`（仅构造回退与 none 归一/请求过滤用），tui.py 改从 .config import（/reasoning 校验与补全）；依赖方向 llm→config（config.py 不再 import llm，环消除，llm→config→input 为叶子链）。公开 API 不变（pie.DEFAULT_MODEL 改从 config 导出，REASONING_* 非公开 API）。修正同日旧条目「config 依赖 llm，放这里避免循环 import」——config 已不依赖 llm，该理由仅历史有效。

- **TUI 工具结果失败红框**：工具结果渲染（实时 _append_event / resume _write_tool_result / !shell _show_shell_result）按文本前缀判失败：shell 返回 `[exit=N]` 且 N≠0（命令执行失败）与 `[shell] 超时` / `[工具错误]` / `[工具异常]` / `[参数解析失败]`（工具调用层失败）用 `role="error"` 红框（#f38ba8），其余 `tool_result` 棕框；!shell 按 code 判（0→tool_result，cancelled→system 低调灰，其余→error）。判定逻辑收敛到 tui.py `_tool_failed_role(text)`，tools.py/loop.py 不改（成败语义仍在 harness 层保持为纯文本事实）。

- **工具执行并行化（loop.py）**：同一批 tool_calls 用 asyncio.gather 并行执行（_run_tool_call），全部收尾后按模型返回顺序回填 ToolMessage（_finalize_tool_message），历史扁平序列与串行逐字节一致 → _step_batches / keep_last_steps / 轮次级 / 会话级认定零影响，compaction 代码不改。单工具失败（ToolError/异常）文本化照常返回、不拖累同批；/stop 取消（_cancel_tools）按 call 粒度收尾：真实完成保留结果、被取消补 CANCEL_TEXT，保证每条 tool_calls 恰好对应一条 tool 消息。tool_result 事件按真实完成顺序实时推（与历史回填顺序解耦）。AGENTS.md 已知限制移除“不支持并行工具调用”。

- **TUI 透明背景**：PieApp.on_mount 设 `self.theme = "ansi-dark"`（background=ansi_default + ansi=True），背景输出 `49`（终端默认背景）透出终端窗口色。踩坑：Textual 8.x 默认主题 ansi=False 会启用 ANSIToTruecolor 过滤器，把 ANSI/default 色映射成 MONOKAI 主题背景 rgb(12,12,12)；CSS `background: transparent` 解析为 alpha=0 黑，rich_color 丢弃 alpha 变纯黑——两者都不透明。ansi-dark 主题 background=ansi_default 且 ansi=True → native ANSI 色直通。SCREEN_BG/LOG_BG 保持 "transparent" 配合叠加（子组件 transparent 叠加到 App 的 ansi_default 不变）。

- **WSL /mnt/d 跨盘文件 IO 慢**：openai 3.5.0 import 需 7.7s（3628 次 posix.stat，每次 ~1ms）；import pie.config 8.7s。验证/测试要留足超时（pty 测试至少等 10s+）。

- **TextArea 文字选中高亮统一为 #log 鼠标框选色**：`#input`（PieTextArea）的 `.text-area--selection` 在 ansi-dark 主题下被 Textual 内置 `&:ansi` 规则覆盖成 `background: transparent + text-style: reverse`（反转色），与 #log 自定义框选（蓝底深字）不一致。修复：颜色提为共享常量 SELECTION_BG（#89b4fa）/ SELECTION_FG（#06121f），SELECTION_STYLE 引用之，并在 PieApp CSS 的 `#input` 块内加 `& .text-area--selection {{ background: {SELECTION_BG}; color: {SELECTION_FG}; }}`（#input 是 ID 选择器，优先级压过内置 class 规则）。headless 验证 get_component_rich_style("text-area--selection") = #06121f on #89b4fa。

- **TextArea placeholder 颜色在 ansi-dark 主题下偏亮**：Textual 8.x 默认 `.text-area--placeholder { color: $text 40%; }`，而 ansi-dark 下 `$text` 是终端默认前景色（偏白），叠透明背景显得亮。覆盖规则加在 PieApp CSS 的 `#input` 块内：`& .text-area--placeholder { color: #585b70; }`（catppuccin mocha surface2 暗灰）。注意 CSS 是 f-string，嵌套规则里的花括号要写成 `{{ }}`。

- **shell 工具移除 cwd / limit 参数**：始终在当前工作目录执行；超长输出不做内部截断，全文返回后交回 harness 工具级压缩（按行 head+tail 落盘指针，keep_last_steps 保护窗口）；tools.py 不再用 write_raw（import 移除）。TUI 的 `!` shell 模式与 CLI `--cwd`/`pie sessions -l` 不受影响。

- **轮次级压缩摘要不再保留 user_input**：UserMessage 本身保留在 AgentMessage 中（压缩只替换其后的叶子），摘要重复写入用户输入是冗余，且会污染 summarize_turns 的 (q, final) 提取（final 混入重复 q）；_compact_turn_span 签名去掉 user_input 参数，摘要只保留模型最终输出（pie: ...）+ 中间过程省略标注。修正 09-01「只保留用户输入 + 模型最终输出」的表述。

- **移除 ModelMessage 容器**：AgentMessage 直接持有扁平叶子消息列表（[System, User, Assistant, Tool, ...]），轮次边界由 UserMessage 隐式表达（一个 User 及其后的 assistant/tool 叶子构成一轮）；ModelMessage 职责并入 AgentMessage：add 直存叶子、to_api 直接遍历、轮次级压缩由 _compact_turns/_compact_turn_span 承担；flatten_messages 删除（chat/loop 改用 messages.messages 直遍历）。轮次级压缩每次循环重扫 user 索引（切片替换会漂移后续索引，预计算索引会误压进行中轮次），从最老开始压、最后一个 user 之后（进行中轮次）不压、已压缩轮（span 全 level>=2）跳过。

- **新增 TUI 命令 /reasoning <none|low|high|max> 运行时切换思考深度**：更新 session.config.reasoning_effort + 当前 llm 实例属性（OpenAILLM.reasoning_effort 每次请求由 _request_kwargs 读取，下个请求即生效）+ cfg.save() 持久化（重启仍生效，写回来源 config_file）。级别常量 REASONING_LEVELS/REASONING_NONE 放 llm.py；none=关闭思考：归一为 None → 请求不发 reasoning_effort 参数（__init__ 归一 + _request_kwargs 过滤双保险，运行时赋 "none" 也不发）。补全面板给 4 条具体候选。

## 2026-09-01

- **轮次级压缩摘要改为「只保留用户输入 + 模型最终输出」**：中间被省略的过程用 `...[中间过程省略]...` 显式标注（无中间过程则不标注）；CompactionConfig.turn 由 TurnCompaction(head/tail) 收敛为 bool（true 开启 / false 关闭），旧 [compaction.turn] 子表 dict 写法自动迁移为开启；用户输入从 AgentMessage 层（前一个 UserMessage）传入 ModelMessage._compact_turn。

- **write / shell 工具 schema 显式化**（对齐 TypeBox 风格）：write 参数带 description；shell 参数名 cmd → command（旧会话历史中的 cmd 只回传不重新 dispatch，无需兼容映射）、timeout 默认 None（不设则无超时，subprocess.run 不再有默认 120s）、保留 cwd/limit 并补充 description；cli.py self_check 同步改 {"command": ...}。

- **内置 SYSTEM_PROMPT 承担分层说明职责**：「分层提示与记忆」章节解释 SYSTEM.md / AGENTS.md / MEMORY.md / ~/.pie/memory.md 的注入与维护，build_system_prompt 只做纯内容拼接、不再硬编码引导语；首次运行（ensure_config 写配置）同时创建全局记忆种子 ~/.pie/memory.md（GLOBAL_MEMORY_TEMPLATE，已存在则不覆盖）——解决“无 SYSTEM.md 且无记忆文件时模型完全不知道记忆机制”的种子缺失问题。

- **compaction 改为“显式配置”语义**：Config.compaction 默认为 None（不写 [compaction] = 不做任何压缩）；写了 [compaction] 则默认三级全开（tool/turn/session 默认非 None），子表只调 head/tail 参数，`tool/turn/session = false` 显式关闭对应级；移除所有 enabled 键（旧 enabled=false 迁移为整体 None，enabled=true 无效果）；各级 compact() 改传子配置对象（tool_cfg/turn_cfg/session_cfg）而非整个 cfg，消除空指针依赖；_toml_dump 跳过 None 值（TOML 无 null）。

- **keep_last_steps 恢复跨轮次滚动语义**（仅工具级压缩）：保护最近 N 个 step 批次（每批 = assistant(tool_calls) + 后续 tool 结果），窗口跨轮次滚动、当前轮最近的批次恒在窗口内；窗口外未压缩 ToolMessage（含历史轮次）从最老开始落盘成指针。轮次级/会话级保护规则不变（只保护当前轮 / 最后一个 user 之前），避免 8-31 饿死问题回归。实现：_step_batches + _protected_step_tool_indices（保护粒度 = step 批次，跨轮次）。

## 2026-08-31

- **重构——摘要只保留规则式**：移除子 agent 摘要器（_make_subagent_summarizer）与 LLM 摘要模式（parse_summary_output / remember_facts / 滚式 / compress_summarizer / subagent_timeout / remember_facts / subagent_config_file / ensure_subagent_config）。

- **/usage 改名 /stat**：usage_report 新增“会话文件：<path>”行（仅当会话文件已保存存在时显示），TUI 状态栏过滤该行保持首/尾摘要。

- **语义收敛——只保留 keep_last_steps（默认 5）决定保护窗口**（最近 N 个 step 批次所在轮次，跨轮次滚动，当前轮恒在窗口内）；移除 keep_last_turns 与 compress_current_turn；纯文本会话（无 step 批次）保护全部、不压缩。

- **三级压缩开关合并为 compaction（bool）**：true 时工具/轮次/会话三级全部启用；旧键 compress_tools/compress_turns/compress_session 自动迁移（取三者 AND，新键优先）。

- **compaction 改为嵌套结构 [compaction] enabled + [compaction.tool] head/tail**（工具级压缩按行保留 head+tail，行数不足则不压）；旧扁平 compaction=true 与更早三键自动迁移。

- **修复轮次级/会话级被饿死**：保护窗口收窄为“只保护当前轮”，已完成轮次不再受 keep_last_steps 窗口保护（跨轮次滚动语义取消），轮次级/会话级恢复工作；keep_last_steps 只负责当前轮内最近 N 批 verbatim。

- **上下文管理重构为容器模型**：AgentMessage（整场对话）/ ModelMessage（一轮内 assistant+tool）/ 叶子 System/User/Assistant/ToolMessage；compact(tools|turns|session) 为容器方法，支持切片与拼接；tokens() 用 provider 基线 + 压缩比例估算；compaction.session 默认 false（/clear 切换窗口）；fs 为历史窗口块列表（~/.pie/windows/，GC 不碰）；会话文件新格式，不兼容旧文件。

- **会话 meta 不再记录 manifest 路径**：消息自带 raw_path 自描述，manifest 降级为按文件名推导的可选审计日志；verify_context 以消息字段 + fs 为准。

- **新增 Textual TUI（src/pie/tui.py，pi/tau 风格）**：真实终端下 chat/resume 走图形界面，工具日志经 complete_turn 的 on_event 回调实时展示；非 TTY 回退 readline。

- **移除 compress_max_turns_per_event**（LLM 摘要时代遗留的每事件封顶）；规则式下轮次级一次压到目标水位或无可压轮次，压不完再升级会话级。

- **移除 spill_threshold_chars / read_spill_threshold_chars 与 harness 级工具级压缩**：shell 新增 limit 参数（默认 200 行，超限全文落盘 + 指针 + 最后 limit 行），read 用 offset/limit 分页；shell 落盘指针在 loop 里写入 manifest（kind=tool）。

- **移除 use_memory 配置**：SYSTEM.md / AGENTS.md / MEMORY.md 存在即加载，不再有跳过开关。

- **修复 /save 自定义路径的 manifest 关联**：__meta__ 记录 manifest 路径，load 优先使用；新增 Session.full_history() 按 manifest 展开压缩内容重建完整转录（压缩视图 vs 完整历史的差异是设计，原始数据始终在 step/turn/session-*.txt）。

- **TUI 新增 shell 模式**：输入以 `!` 开头时输入框边框变 tool_call 橙色（#9c4916，CSS 类 shell-mode 切换），提交后 `!` 后内容直接 subprocess 执行（shell=True，120s 超时，超 200 行截断显示前 100 后 50），结果输出到 log 但不经过 LLM、不进会话上下文（不写 messages、不 save）。

- **修正工具级/step 级语义**：工具级压缩只压缩 tool 返回文本（内容落盘成指针，消息保留，绝不删除）；当前轮 step 压缩改为内容级（spill_turn_tool_results），整批删除的 compress_step_batches 已移除；stats 字段 spilled 改名为 tools。

- **压缩指针写入消息自身字段（Message.raw_path / raw_hash）**：消息自描述，full_history() 按消息顺序精确重建；referenced_raw_paths() 同时扫 manifest 与会话消息字段，GC/verify 不再依赖文件名 stem 关联。

## 2026-08-29

- **上下文压缩文件用内容 sha256 前 16 位命名**（不做 turn-range）；摘要子 agent 实现为 shell 调 pie 一次性模式；CLI 新增 -c/--config。

- **三级上下文压缩**（工具级 eager spill / 轮次级 / 会话级），token 计数用 API usage.prompt_tokens；单条消息压缩级别只升不降（0→1→2→3）。

- **read 返回全文不截断、支持 offset（1 起）/ limit 分页**；edit 为 edits 数组（oldText 唯一、互不重叠、按原文非增量应用），对齐 pi-agent。

- **shell 工具不内部截断**，全文交回 harness 统一做 1 级压缩；spill 按工具区分：read 默认不落盘（>100K），shell 等按 8K。

- **压缩事件写会话 manifest（~/.pie/context/<session>-manifest.jsonl）**，maybe_compact 返回节省 token 统计，Session 提供 compression_history / verify_context / raw_history；`pie context info/verify/gc` 维护命令。

- **摘要为规则式**：工具=head+tail，轮次=user+最终输出，会话级=指针+保护区域 verbatim。

- **/usage 显示当前上下文占用**（估算+百分比）、压缩次数与落盘原文量、API 上报与累计；UsageTracker 经会话文件 __meta__ 跨 resume 恢复。

- **DeepSeek thinking 400 真根因 = 会话级压缩在轮次进行中 pair 提取**：产生 user→纯文本 assistant→tool_calls→tool 非法序列；修复：进行中轮次不 pair 提取（turn_in_progress）、pair 仅当轮次以 assistant 结尾时提取、Session.load 自动修复已损坏序列。

- **tool_call 参数膨胀**（write 大 content + thinking reasoning 大）是上下文主要消耗源，决策：留给自动压缩处理，不单独改工具。

- **新增 keep_last_steps（当前轮次内必须完整保留的最近 step 批次数，默认 3）与 compress_current_turn（默认 true）**：进行中的轮次超限时压缩较早 step 批次（整批落盘 + manifest kind=step），解决长工具循环单轮撑爆上下文的问题。

## 2026-08-28

- **harness 采用 OpenAI 兼容接口**：默认模型 deepseek-v4-flash（reasoning_effort=high），默认 API https://api.deepseek.com/，配置只从 ~/.pie/config.toml 读取。

- **包名为 pie，入口 python -m pie**：内置工具仅 read / edit / write / shell，扩展用 @tool() + ToolRegistry。

- **配置持久化到 ~/.pie/config.toml**（旧 config.json 自动迁移），记忆文件为 MEMORY.md 与 ~/.pie/memory.md。

- **CLI 为 pie（新对话）/ pie resume（恢复最近会话）/ pie [PROMPT]（一次性子 agent，不写 sessions）**；Session 支持多轮对话与 JSONL 会话持久化；支持全局安装（uv tool install . --editable），任意目录可运行。

