# CHANGELOG — 关键决策与变更记录

本文件按时间倒序记录 pie 的关键设计决策与实现变更。决策的「当前状态」摘要保留在仓库根目录 `MEMORY.md`。

## 2026-10-01（REPL 画布也有滚动条）

用户：「tui Repl 也加个 scrollbar 吧」。

- **画布右缘固定留 1 列**（与消息流同口径：画布也折行、也吃 `width`，不能「有溢出才留」），只有溢出才画 thumb；
  画法抽成 `App::render_scrollbar`（消息流与画布各调一次，样式只有一处）。
- 点/拖：`App::scrollbar_jump` 按 tab 分派 —— 消息流改 `App::scroll_from_bottom`，画布交给 `Repl::jump_to_row`
  （同一套线性映射）。为此 `Repl` 在 `render` 里记下上一帧的 `total` / `top` / `viewport`，并给出
  `total()` / `top()`（`App` 画 thumb 与换算偏移都要用）；画布视图下 `App.body` 仍是空的 → 画布本身不成
  「文本面」（上面按下不起框选），`surface_at` 只多认右缘那 1 列；消息流那份偏移与输入框/状态栏完全不受影响。
- 测试：`jumping_by_row_maps_the_scrollbar_onto_offsets` / `jumping_does_nothing_without_overflow`（`Repl` 层）
  + `the_repl_canvas_has_its_own_scrollbar`（`App` 层：几何、`surface_at`、点/拖只动画布）。

## 2026-10-01（改名：`src/tui/repl_view.rs` → `src/tui/repl.rs`，`ReplView` → `Repl`）

用户：「repl_view.rs -> repl.rs, ReplView -> Repl」。

- 模块声明 `pub mod repl;`（`src/tui/mod.rs`），类型/方法/测试全改 `Repl`；两处 `use`：
  `app.rs` 的 `use super::repl::{Repl, Tab};`、`status.rs` 的 `use super::repl::Tab;`。
- 与顶层 `src/repl.rs`（**repl 工具**）同名不同模块（`crate::tui::repl` vs `crate::repl`）——两边都没跨引，
  不存在歧义；`Tab::Repl` 变体与 `Repl` 类型不同命名空间，也不冲突。
- **视图指示是小写 `cr`**：接手这棵树时 `Tab::label()` 的返回值是 `"c"` / `"r"`，我先按上一节的 `CR`
  改回了大写 → 用户确认**小写是他改的** → 翻回 `"c"` / `"r"`，状态栏文本断言（`status.rs` / `app.rs`）
  与 `AGENTS.md` / `MEMORY.md` 一并同步（上一节那条写 `CR` 的已被这条取代）。

## 2026-10-01（状态栏视图指示：`Chat │ REPL` → `CR`）

用户：「我说的是 status_line 里面的 view_tabe 的显示从"Chat | REPL"变成"CR"两个字符。」

- `Tab::label()`（`repl_view.rs`）从 `Chat` / `REPL` 变成**一个字符**（`C` / `R`），`status::view_tabs`
  去掉中间那根 `│`：两档挨着显示成 ` CR `（4 格，原来 13 格），当前那档仍是 accent + 粗体、另一档 muted
  （宽度仍参与 `status_line` 的填空计算：`view_len` 4）。新增用例 `view_tabs_marks_the_current_one` 钉住
  「哪一档 accent / 哪一档 muted / 失焦全 muted」；pty 实测确认 `C`→`R` 切过去后是 `R` 亮。
- 顺带修了三条**原本就红**的断言（与本次改动无关：期望串里的空格位置一直和真实渲染不符，我用临时
  回插旧串验证过）：名字 span 自带尾空格 + tail 前缀 ` │ ` → **cwd 后是两个空格**、用量与余额之间**一个**
  空格；`status_line_shows_zero_usage_before_any_report` 里 `│` 的计数 1 → 0（视图指示里那根也一起没了）。

## 2026-10-01（修：REPL 画布的图小了一半、还浮在画布中间）

用户贴了张 TUI 截图：「这是你现在的显示效果，很明显不对吧」——图小小一张压在代码中间，右下角那块浮层里空空的。

**根因**：`Picker` 的 `FontSize` 是**猜**的（`halfblocks()` 自带 10×20）。iTerm2 协议是按**像素**发图的
（`1337;File=…;width=Npx;height=Mpx`），终端就照这个像素数 **1:1** 画 —— 所以 `FontSize` 与终端真实字符格
差多少，图就按那个比例缩错多少。本机 Kaku 的字符格是 **20×58**（`TIOCGWINSZ`：2580×1914 ÷ 129×33）：
拿 10×20 算出来的 1210×440 图只有 **51×6 格**（应为 61×8 格，正好差一倍），而 `Image` 是**顶在**右下角
浮层的左上角画的 —— 看上去就是「一张小图浮在画布中间、压着代码」。

- `tui::pick_terminal_protocol`：字体尺寸改为**问终端**（`crossterm::terminal::window_size()`，只做
  `TIOCGWINSZ`、不读 stdin），问不到才退回 10×20；构造走 `Picker::from_fontsize`（唯一能自定 `FontSize`
  的入口，已 deprecated → `#[allow(deprecated)]`）。
- 契约测试 `the_image_is_encoded_in_the_cells_of_the_real_terminal_font`：20×58 的格子里，1210×440
  必须编成 **61×8 格** / `width=1220px;height=464px`（= 格数 × 每格像素，向上取整到整格）。
- 图仍然按**原像素 1:1** 出屏（`Fit` 只缩不放）：129 列画布上图占 61 列、落右下角，正好避开 60 来列宽的代码正文。

## 2026-10-01（REPL 画布的图：**浮**在右下角 + 放大）

用户：「repl_view 的图片放在右下角，而且现在图太小了，放大点。」→「图片现在是浮在右下角那种，文本
还是要占据全部 repl_view 画布的。图还是不够大。」

- `ReplView::split`：文本**永远拿到整块画布**；有图时额外在**右下角浮**一块**固定 Rect**（不随滚动重传）。
  浮层取画布 **4/5 宽 × 4/5 高**（`IMAGE_MAX_ROWS` 24 → 60）。图**压在文本上**（overlay），不再把文本挤上去。
- `ReplView::render_image`：编码拿整个浮层当上限（`Fit` 只缩不放），再把编出来的 `Protocol`（`Protocol::size()`）
  **贴到浮层右下角**——图比浮层小时靠右下沉、左上露出底下的文本，像浮在字上。
- 效果（120×40 画布、10×20 字体）：宽图 77×16 → 96×20 格；方图 32×16 → 63×32 格。新增几何单测
  `the_image_sits_in_the_bottom_right_corner`（断言图贴右下角、文本区 == 整块画布）。

## 2026-10-01（REPL 画布贴图：`repl` 里的 matplotlib 图直接显示在 TUI 里）

用户：「现在可以轻松实现 repl 渲染图片了吧」→「使用 Kitty graphics protocol 呢？它和 iterm 图片协议有啥区别？」
→ 讨论后定案：**ratatui-image 11.1.0 + iTerm2 优先**。

- **数据通道**（结构化，**不嗅探正文**）：`repl_driver.py` 在 `post_execute` 扫「还开着的 matplotlib figure」
  → `savefig` 到临时目录（`PIE_REPL_IMAGE_DIR` 可覆盖，缺省系统临时目录；顺带清 1 小时前的旧图）→
  `Reply.images` → `ToolOutput.images`（新字段，与 `spill` 同级）→ `TurnEvent::ToolResult.images` → `ReplView`。
  **为什么在 `post_execute` 抓、而不 patch `plt.show`**：Agg 下 `show()` 是 no-op，而用户往往在
  **同一个 cell** 里 `import` + `plot` + `show` —— 执行前 patch 来不及，执行后扫「还没关掉的 figure」一定抓得到。
- **渲染**：画布下方固定区（约 2/5 高，最多 24 行）显示**跟随滚动位置**的那张图（用户：「图片应该也要可以
  滚动吧？」）——`ReplView::Entry` 自己带 `images`，滚动时取「可见范围里最后一条记录」再往前找最近一张
  （那条没图就沿用上面那张，比闪成空白/一直挂最新那张都自然）。布局用「画布里有**没有**图」判断，
  不用「当前选中哪张」——否则滚到没图的区域时图区会忽现忽没，文本区高度跟着抖。
  为什么不做「图像混在文本流里滚」（jupyter 那种）：iTerm2 无状态，位置一变就得重传整段 base64；
  而且半张可见时没法安全裁剪（会贴到别处去）。固定 Rect + 换图时重编，代价最小。
  `Protocol` 按区域编码一次；用无状态 `Image` widget——`StatefulImage` 在 render 时 resize + 编码 = **阻塞渲染线程**。
- ⚠ **协议档位不用 IO 探测**：`Picker::from_query_stdio()` 把「写查询 + 读回应」放在**独立线程**，
  超时（终端不回应很常见）后那个线程**杀不掉**，会继续跟 crossterm 抢 stdin —— 实测接线后 `←`/`→`
  **完全失效**（普通字符侥幸能进）。改成 `tui::pick_terminal_protocol()` 按环境变量定：
  kitty / ghostty → Kitty；iTerm2 / WezTerm / **Kaku**（WezTerm 的 fork，`TERM_PROGRAM` 不叫 WezTerm）/ rio / vscode → Iterm2；
  其余 → Halfblocks。字体像素尺寸用 `halfblocks()` 自带的 10×20 估值（只影响缩放比例）。
- **为什么 iTerm2 而不是 Kitty**（ratatui-image README 的兼容性矩阵）：WezTerm 系**只有 iTerm2 无 bug**
  （kitty 的 unicode placeholder 实现不完整，作者在源码里主动拉黑）；Kitty/Ghostty 才用 Kitty 协议。
  而我们的场景（固定 Rect、静态图、不滚动）本来也用不上 kitty 的杀手锏（placeholder / image id /
  共享内存 / 压缩）——它真正值钱的地方是「图像随文本滚动」，而那正是我们绕开的。
- **resume**：`ReplView::from_messages` 按历史重建 code/output（`Cell::from_messages` 的 repl 版）；
  图**不回放**（运行时附件，不进会话文件）。
- 依赖：`ratatui-image 11.1`（`default-features = false` + 只 `crossterm`；默认的 `chafa-dyn` 要
  pkg-config + libchafa C 库，`image-defaults` 实测也不必）。
- **验证**：单测 10 例（halfblocks 出块字符 / iTerm2 序列进 buffer / 图读不出来降级提示 / 无图不切图像区 /
  只有 repl 进画布 / resume 回放 / 滚动选图 / 没图的记录往前找 / 图区不抖）；端到端跑 driver（同 cell
  import+plot+show 也能抓到、无图的 cell 不误报、跨 cell 也行）；**pty 真机**：切到画布时终端里出现
  1 次 iTerm2 序列；再用两张不同的图滚上滚下——滚动过程中确实出现了**两种**不同的图像数据
  （固定 Rect + diff 生效：图没变就不重发）。

## 2026-10-01（TUI：REPL 画布 tab —— `←`/`→` 切视图，`/repl` 进、`Esc` 回）

用户：「感觉不是很好嵌入当前消息流；是不是可以新建一个屏幕（画布），在那里面单独更新 repl 的
code/output。然后我们可以通过键盘的 pageLeft/pageRight 来切换，这样可行嘛？」→ 确认可行（比嵌消息流
正确），键位改成 `←`/`→`，落地。

- **两个视图（tab）**：`Chat`（消息流）/ `Repl`（REPL 画布）。新文件 `src/tui/repl_view.rs`：
  `Tab` 枚举 + `ReplView`（记录 + 折行 + 滚动）。**不嵌消息流**的核心理由：独立画布 Rect 固定，
  将来贴 matplotlib 图（ratatui-image）不会因为滚动被反复重传。
- **数据零新通道**：`TurnEvent::ToolCall/ToolResult` 里 `name == "repl"` 的事件顺手喂给 `ReplView`
  （工具层 / session 层一行未改）；消息流里那行工具摘要**照旧保留**。
- **键位**：`←`/`→` **只在输入框为空时**切视图（非空时它们是输入框的光标键——必须让位，否则没法移光标）；
  `/repl` 进、`Esc` 回（`Esc` 在画布里优先于其它 Esc 语义）；`PgUp`/`PgDn` / 滚轮作用于**当前**视图。
  ⚠`PageLeft`/`PageRight` 在终端里**不存在**（crossterm 只有 `PageUp`/`PageDown`：`ESC[5~`/`ESC[6~`）。
- **折行复用** `pane::wrap_segments` / `WrapMode`（两者改成 `pub(crate)`）——不引第二个折行实现
  （`wrap_segments` 返回的是**字符下标**，调用方按 `chars()[start..start+len]` 切）。
- 状态栏最左加视图指示 `Chat │ REPL`（当前那档 accent + 粗体，其余 muted）；`status::hint_text(busy, tab)`
  按视图换 placeholder 文案。
- **测试**：`repl_view` 单测 6 例 + `app` 集成 4 例（←/→ 只在空输入时切、`/repl`+`Esc`、
  只有 repl 事件进画布、空画布有提示）。改状态栏布局后需同步 `status_line` 的断言（`name_span` 取模型名那个
  span，不能拿 `spans[0]`）。

## 2026-10-01（新增 `repl` 工具：**会话内持久**的 IPython）

用户：「增加一个工具 `repl`（python REPL，为了简便基于 IPython）……状态在当前 session 里可以保持。
具体怎么实现我还没想好，调研分析一下给报告」→ 报告 `docs/repl-tool.md`；用户选**路线 B**、要求落地。

- **路线选型**：长活子进程 + `IPython.core.interactiveshell.InteractiveShell.run_cell`（**路线 B**）。
  对比过路线 A（jupyter_client + ipykernel，真 kernel）并**实测了整套 ZMQ/消息协议**（连接文件、
  `<IDS|MSG>` 帧、HMAC 签名、iopub 序列、`interrupt_request`、stdin 通道）。结论：
  **A2（Python 侧 `jupyter_client` 代理）的 Rust 侧与 B 一模一样**（同一套定长协议），只是多了
  依赖/进程/端口——所以先上 B，`Repl` 接口不变，将来要 kernel 语义只换 Python 脚本。
- **状态落点**：`ToolCtx` 新增 `state: Arc<tools::SessionState>`（`TypeId → Arc<T>` 泛型槽），
  `Session` 建一份、`tool_call` 每轮 clone 进去。**不放 `ToolRegistry`**（绑定里 `builtins(cfg)` 会被
  多个 session 复用 → 状态串台）、**不放全局 map**（生命周期/清理没答案）。
- **`src/repl.rs` + `src/repl_driver.py`**：驱动用 `InteractiveShell.instance()` + `capture_output`；
  实测后关掉两处“污染”：`displayhook.write_output_prompt`（去掉 `Out[n]: ` 前缀，避免与 `res.result`
  重复）与 `showtraceback`（IPython 自带的带色 traceback 直接写 stdout，改由驱动给一份纯文本
  traceback）。输出 = stdout + stderr + traceback。
- **进程/并发**：子进程归一个 **owner task** 独占（读帧**永不进 `select!`**——`read_exact` 非 cancel-safe，
  半帧会错位）；工具只发 `mpsc` 请求 + 等 `oneshot`；同一会话用 `tokio::sync::Mutex` 串行。
  取消/超时给**进程组**发 `SIGINT`（保状态），停不住再 `SIGKILL`（丢状态、下次重启）。`kill_on_drop(true)`。
- **对齐现有契约**：异常**不算工具失败**（是 REPL 正常输出，不打 `[exit=…]`）；超限走新抽出的
  `tools::head_prefix` + `Storage::store(Raw{prefix:"repl"})` 落盘 + 指针（与 bash 同款，bash 也改用 `head_prefix`）。
  缺 IPython / 解释器起不来 → 把子进程 stderr 文本化回给模型（`[工具错误] …`），不崩。
- **`_python` 支持 `~`**：开头的 `~` / `~/…` 展开成 $HOME（`Command::new` 不自己展开；不展开就会
  「启动解释器失败: No such file or directory」）。
- **测试**：`repl.rs` 内 9 例用**假驱动**（不依赖 IPython）盖状态保持/不串台/错误合并/截断落盘/缺解释器/
  取消/超时；另手动用真 IPython 跑通端到端（状态、`math.sqrt`、`%timeit`、traceback）。`builtin_tool_names`、
  `tools_from_spec_*` 与绑定的 `builtins()` 用例同步更新。

## 2026-09-30（`compact_session` / `clear_window` 改用 `std::io::Result`：错误上下文在底层只写一次）

用户：「`compact_session` 返回 `Result` 这 ok，但是它可以返回 `std::io::Result` 吗？我不想手写 error message。」
—— 可以，而且早该如此：`Storage::store` 本来就是 `std::io::Result<PathBuf>`，那句
`map_err(|e| format!("写窗口块失败: {e}"))` 是纯手写包装。

- `context::compact_session` → `std::io::Result<Option<(PathBuf, CompactEvent)>>`；
  `Session::clear_window` → `std::io::Result<usize>`（沿途零手写 message）。TUI 的
  `format!("切换窗口失败：{e}")` 与绑定的 `Ok(self.lock()?.clear_window()?)`（pyo3 自带
  `From<io::Error> for PyErr`）都不用改。
- **错误上下文下沉到唯一落盘出口**：`Storage::write_atomic` 给每个 io 错误补上路径
  （`Error::new(e.kind(), "<path>: <e>")`）—— std 的 `io::Error` 不带路径，写在底层一处，
  于是 `store()` 的所有调用者（bash 落盘、图片副本、压缩原文、窗口块）都受益，上层不必各自拼 message。
- 验证：`cargo test` 209（lib）+ 4（bin）全绿；clippy 14；绑定 27。

## 2026-09-30（`compact_session(messages, config, storage)`：与 `compact_tools` 同形）

用户点名了签名。原来 `head` / `tail` 是**单传的两个 usize**（调用方从 `config.compaction.session` 里
取出来），而它们本来就是 `SessionCompaction` 的字段 —— 直接把 `&SessionCompaction` 传进来，
参数就回到与 `compact_tools(messages, &ToolCompaction, storage, cb)` 同形（`messages → config → storage`）。

- 调用方 `Session::clear_window` 反而更短：私有的 `window_sizes()` **删掉**（它只为“取 head/tail”存在，
  单一消费者 → 内联），换成三行取配置（`[compaction.session]` 没配就用 `Default`）。
- 验证：`cargo test` 209（lib）+ 4（bin）全绿；clippy 14；绑定 27。

## 2026-09-30（`keep_last_steps` 挪进 `[compaction.tool]`）

用户：「`Config.keep_last_steps` 是不是应该放入 `ToolCompaction`？」—— 是，它**只被工具级压缩用**
（`compact_tools` 的跨批次保护窗口），住在顶层却只服务一级坐标不明。

- `ToolCompaction` 加 `keep_last_steps: usize`（默认 7，值不变）；`Config.keep_last_steps` 删除
  （含 `to_toml`、默认模板、绑定 getter/setter、`.pyi`）。
- `compact_tools(messages, tool_config, storage, cb)` 少一个参数（keep 从 `tool_config` 里读）。
- ⚠ **配置迁移**：顶层 `keep_last_steps = N` 现在会被忽略（未知键静默跳过）→ 要保留自定义值
  就写进 `[compaction.tool]` 段。
- 验证：`cargo test` 209（lib）+ 4（bin）全绿；clippy 14；绑定 27。

## 2026-09-30（名字改回：`archive_window` → `compact_session`）

用户：「第三级（`compact_session`）存在，只是只能由用户手动触发！」

—— 对。level 3 从未消失：`compress_level = 3`、`CompactEvent::Session`、`[历史窗口: …]` 摘要、
`[compaction.session]` 的 head/tail 都还在，变的只是**触发者**（水位 → 用户）。所以名字该描述
“它是哪一级压缩”（`compact_session`），而不是“它物理上做了什么”（`archive_window`）。

- `context::archive_window` 改名回 `pub fn compact_session(messages, storage, head, tail)`
  （它现在**只服务于第三级**：唯一调用者是 `Session::clear_window`）。
- 名字的“三级”含义在文档里写明：自动压缩只做工具级（level 1）/ 轮次级（level 2），
  **第三级（会话级，level 3）只由用户手动 `/clear` 触发**。
- 验证：`cargo test` 209（含 `compact_session_spills_the_whole_window`）+ 4 全绿；clippy 14；绑定 27。

## 2026-09-30（归档只留给用户：`/clear` 是唯一入口，`archive_now` 删除）

用户：「`compact_session` 只能由用户手动触发 —— 假设可以自动触发，那岂不是模型输出着突然就 clear window 了？」

- 观察对，但真正的毛病比他说的更具体：**归档会打断「当前轮的工作记忆」**（刚读的文件、跑的命令只剩摘要），
  而 400 兜底恰好发生在回合中途。另有一个决定性论据：兜底其实**救不了场** ——
  已完成的历史太大时，轮次级本来就能压下去，用不着归档；真需要归档的是「当前轮自己就撑爆」，
  而那正是「只归档当前轮之前」无能为力的情形（上一轮把归档改成整窗口才有效，代价就是打断工作记忆）。
- **动手**：`context::archive_now` 删除；`WindowStore` enum 删除；`archive_window` 只剩 `/clear` 一个调用者，
  签名简化为 `archive_window(messages, storage, head, tail)`（直接落 `windows/`）。
- `aturn` 的 400 分支：只再强压一次工具级/轮次级；**压不动就报错收场**，并用 `log::warn` 给出出路
  （「可以 `/clear` 开新窗口，或调小单条工具输出上限」）。
- 验证：`cargo test` 209（lib，删了 `archive_now_spills_the_whole_window`）+ 4（bin）全绿；clippy 14；绑定 27。

## 2026-09-30（统一语义：`/clear` 与超限兜底都是「整窗口落盘」）

用户：「应该统一 `clear_window` 和 `compact_session` 的语义，都是把当前 window 内容全部落盘。」
—— 把「归档哪一段」这个差异消掉：

- `context::archive_window(messages, storage, store, head, tail)` 现在**就地**做完整件事：
  把 `messages[1..]`（旧窗口摘要除外）落盘成一块 + 按「system + 摘要链」重开窗口，返回 `(路径, 事件)`；
  没内容可归档 → `Ok(None)`。`WindowStore::{Compaction, Archive}` 定去处（`context/` vs `windows/`）。
- `compact_session` **删掉**（它原来只归档「当前轮之前」，正是要消掉的那个差异）；`archive_now` 变薄，
  成为 `archive_window` 在 `context/` 上的包装。
- `Session::clear_window` 调同一个函数（`Archive` → `windows/`），之后把 `messages[0]` 换成重建的
  system prompt —— 这是它与兜底唯一的差别（`/clear` 要反映 cwd / 记忆的变化）。顺带：`/clear` 不再
  从盘重算所有旧摘要（`messages` 里的摘要文本原地留着），`window_summary_messages()` 只留给 `load` 的 resume 重建。
- 验证：`cargo test` 210（含改名后的 `archive_window_spills_the_whole_window` /
  `archive_now_spills_the_whole_window`）+ 4 全绿；clippy 14；绑定 27 全绿。

## 2026-09-30（A+：自动压缩只做工具级/轮次级；`/clear` 与超限兜底共用 `archive_window`）

用户发现：「`compact_session` 了那就是把当前窗口归档了，那岂不是 `compact_turns` / `compact_tools` 都没必要了？」
—— 确实，而且是**删 `target` 判据那一步的直接后果**：原来有 target 时是逐级递进（压完工具级还超目标水位
才压轮次级、还超才归档），删掉后变成「一触发就三级全上」，前两级必然白做且直接吃到损失最大的一级。

- **自动压缩（`maybe_compact`）只做工具级 + 轮次级**；**会话级（整窗口归档）退出自动路径**。
- **`archive_window`（新，`context.rs`）**：把一段消息归档成窗口块 —— 落盘 + 摘要消息 + 流水事件；
  去处由 `WindowStore`（`Compaction(prefix)` → `context/`；`Archive` → `windows/`）定。
  **`Session::clear_window` 与 `compact_session` 共用这一条**（用户点名要求），差异只剩「归档哪一段」
  （调用方给切片）与去处。
- **`compact_session` 降级为超限兜底**：`archive_now(messages, config)` 是它的 pub 包装，只在 `aturn`
  的「400 上下文超限」分支里调（先 `compact(Auto)`，压不动才归档）；`[compaction.session]` 关掉 → 兜底也不归档。
- 顺带：块格式统一成 pretty JSON 数组（`/clear` 原来是 JSONL；`load_window_dicts` 两种都读，存量块不受影响）；
  `CompactStats.session` 字段、「会话级」的 TUI/绑定展示与 `.pyi` 一并删。
- 验证：`cargo test` 210（lib，含新增 `auto_compaction_never_archives_the_window` /
  `archive_now_archives_before_the_current_turn`）+ 4（bin）全绿；clippy 14；绑定 27 全绿。

## 2026-09-30（`clear_window` vs `compact_session`：不合并，但少读一遍盘）

用户问「`Session::clear_window` 实际上是不是做 `compact_session` 的事？能利用 `compact_session` 完成吗？」
→ 结论：**机制同源、策略不同** —— 两个都是「归档一段历史 → 窗口块 + `[历史窗口: …]` 摘要 system 消息」，
但四项策略不一样：归档范围（全部 vs 当前轮之前）/ 落盘去处（`windows/` 用户主动归档、gc 不碰
vs `context/` 压缩产物、gc 的地盘）/ system prompt（重建 vs 保留）/ 重建布局（`windows` 字段动态重建
vs 旧摘要留在上下文）；失败语义也不同（`/clear` 宁可不整理也不丢历史 → `Result`）。
这些**不是重复代码而是两种语义**；上参数化只会换回四个开关。两者**已经**共享了所有该共享的小件
（`summarize_turns` / `build_window_summary` / `CompactEvent::session` / `Storage::store` / `window_sizes`）。

- 顺手清掉唯一一处真重复：`compact_session` 原本对同一个窗口块**读两遍盘、算两遍摘要**
  （`build_window_summary` 内部一趟 + 紧接着 `summarize_turns(&load_window_dicts(&path))` 又一趟）
  → 抽 `window_summary_text(path, body)`（拼「指针 + 正文」）：
  `build_window_summary` = 读盘 + summarize + `window_summary_text`；归档路径算一次 `digest`，
  同时喂消息与事件。
- 验证：`cargo test` 208（lib）+ 4（bin）全绿；clippy 14（与基线一致）。

## 2026-09-30（`content_text` 变成 `Message` / `Content` 的方法）

用户：「content_text 功能是不是应该变为 Message 的方法？」——对，而且更好：它的参数本来就是
`Option<&Content>`，而**每个调用点都在传 `m.content.as_ref()`**（session 6 处 / TUI 3 处 / context 内部）
—— 说明它要的其实是「一条消息的可读文本」，不是「一个 `Option<&Content>`」。

- 底层拆两半，都住 `llm.rs`（纯数据操作，不依赖 context）：
  `Content::text()`（纯文本原样 / 多模态 parts 拼 text 片段 + 图片占位）+
  `Message::content_text()`（`content.as_ref().map(Content::text).unwrap_or_default()`）。
  名字**不叫** `content()`：`content` 是公开字段（`Option<Content>`），同名方法会让 `m.content` /
  `m.content()` 一读就错。
- `context::content_text` **删掉**；`summarize_turns`（处理的是会话 JSON 的 dict，不是 Message）
  改走 `content_from_value(...).map(|c| c.text())`；测试里那个零逻辑的 `text_of` 包装也删了。
- 验证：`cargo test` 208（lib）+ 4（bin）全绿；clippy 14（与基线一致）。
- 顺带：`context::content_from_value` 也变成 **`Content::from_value`**（手写解析：`"text"` → `Text`、
  `[...]` → `Parts`，其余 `None`；与 serde 的 `untagged` 等价，但更直白也更快）—— 于是
  「content 的形状 / 可读文本」都住在 `llm.rs` 的 `impl Content` 里，`context.rs` 不再有 content 层的东西。

## 2026-09-30（`context.rs` 的 GC 搬进 `cli.rs`）

用户：「context.rs 里面和 cli 有关的移入 cli.rs 吧」。`referenced_raw_paths` / `collect_context_garbage`
（+ 私有 `absolutize`）正是 `pie context verify|gc` 的数据源，与 `cli.rs` 里已有的图片副本那份
（`iter_session_files` / `collect_file_garbage`）同类 → 搬过去；测试 `gc_keeps_referenced_files_only`
跟着搬并改名 `context_gc_keeps_referenced_files_only`（与图片那份命名对齐），`write_session_with_events`
helper 在 `cli.rs` 的 tests 里也备了一份。

- `context.rs` 只剩压缩本身（三级压缩 + 落盘指针），模块 doc 指向 [`crate::cli`]；
  `pretty_indent1` **留在 context**（它服务 `compact_*` 的落盘，`--mode transcript` 只是搭车用）。
- `main.rs` 的 `context verify|gc` 改调 `cli::…`；`context.rs` 里那个跨模块断言（工具落盘 → gc 不删）
  改走 `crate::cli::…`。
- 验证：`cargo test` 208（lib）+ 4（bin）全绿；clippy 14（与基线一致）；绑定 27 全绿。

## 2026-09-30（`Message::assistant` 补齐，删掉 `context::text_message`）

- 构造器一族（`system` / `user` / `tool_result`）缺了 `assistant` → 补上 `Message::assistant(text)`
  （纯文本；带 `tool_calls` / `reasoning_content` 的那条仍在 `aturn` 里手设字段）。顺手把三处手搓
  （`Session::push_assistant` / `push_error_turn` / 取消收尾）与 `context` 测试里的 `assistant_text` helper 换掉。
- `context::text_message(role, text)` 随之**删掉**：两个调用点一个变 `Message::assistant`、
  一个变 `Message::system`（本来就存在）—— 又一个「单一用途的泛化参数」消失。
- `context::set_raw` 退役：它那一行（写 `raw_path`）升成 `Message::mark_compressed(level, Option<&Path>)`
  —— `compress_level` 与 `raw_path` **成对设置**只此一处（工具级 / 轮次级 / 会话级三条压缩路径 +
  resume 重建窗口摘要共用；`None` 表示落盘失败，只标级别）。随后 `Message::with_spill` 也**删了**
  （它与 `mark_compressed` 完全重复，且只有一个调用点）—— 四条落盘路径现在都走 `mark_compressed`。
- 验证：`cargo test` 208（lib）+ 4（bin）全绿；clippy 14（与基线一致）。

## 2026-09-30（落盘路径结构化：`ToolOutput { text, spill }` + `Message::with_spill`）

用户问「`mark_tool_spill` 是不是应该内联进 `Message::tool_result`」→ 澄清后他给出了真正的意图：
**消息构造出来就该是最终形态**（工具输出过长已落盘时，`raw_path` / `compress_level` 当场就设好），
而不是「先造好、再拿正文里的指针去嗅探补一刀」。

- **`Tool::call` 的返回从 `Result<String, ToolError>` 改成 `Result<ToolOutput, ToolError>`**
  （`ToolOutput { text, spill: Option<PathBuf> }`）：只有 `bash` 超 `_max_lines` / `_max_bytes` 时给 `spill`
  （落盘失败 → `None`，正文里的 `(落盘失败: …)` 照旧）。`ToolOutput` 实现 `Deref<Target = str>` + `Display`
  → 测试与调用点的 `out.starts_with(…)` / `format!("{out}")` 几乎不用改。
- **`Message::mark_compressed(1, spill)`**（`llm.rs`）：纯数据的一步——`Some(path)` →
  `compress_level = 1` + `raw_path`；`None` 直通。所以 `llm.rs` **不需要依赖 context**（没有反向依赖、也没有 IO）。
  （当天稍后 `Message::with_spill` 被删：它与 `mark_compressed` 完全重复。）
- **删掉文本嗅探那一整套**：`context::mark_tool_spill` / `extract_spill_path` / `pointer_path`
  全部删除（连带「read 回来的源码字面量会被误判 → 按工具名 gate → `path.exists()` 双保险」那串补丁）。
  `aturn` 回填工具结果时改成一眼睛能看完的三行：`spill` 有值 → 记一条 `CompactEvent::tool` + `mark_compressed`。
  正文里那行 `[工具输出全文已保存: <path>]` **保留**（那是给模型看、让它能读回的指针）。
- **绑定**：Python 工具的调用体从 `Ok(text)` 改成 `Ok(ToolOutput::text(text))`（Python 工具没有落盘通道）。
- 验证：`cargo test` 208（lib）+ 4（bin）全绿；clippy 14（与基线一致）；绑定 `maturin develop` + `pytest` 27 全绿。

## 2026-09-30（压缩水位只认 API 上报：删掉 token 估算那一整条链）

用户从「`raw_tokens` 能不能直接拿 `usage` 填」一路问到「我们真的需要 `message_tokens` 吗」，
读了一圈 DeepSeek 的 Chat Completions 文档（`usage` 的粒度是**整个请求**，没有逐消息分项）后拍板：
**不做估算**，水位只看服务端上报 —— 并点名「单条 tool 消息几十万 token 不正常，我给 `read` 加
`_max_lines`/`_max_bytes`、给 bash 输出做 clip 就是为了这个」。

- **`maybe_compact(messages, config, reported)`**：只看上一次 `usage.prompt_tokens`（≥ `soft_limit()` 就压）；
  `reported = None`（本次会话还没发过请求）→ **不压**。`aturn` 的请求前 / 请求后两次都传
  `self.usage.prompt_tokens`（请求后那次拿到的是刚回来的最准值）。
- **各级压到不能再压**：`compact_turns` 的 `target` 参数与「压到目标水位」判据删除（反事实无法用实测判定），
  `target_ratio` 配置项、`Config::target_limit()` / `target_ratio()`、绑定同名方法与 README 的「迟滞」说明一起删。
- **删掉整条估算链**：`message_tokens` / `messages_tokens` / `content_tokens` / `chars_div4` / `IMAGE_TOKENS_MAX`
  + `Message.raw_len` / `raw_tokens`（`set_raw` 只记 `raw_path`）；`CompactStats.saved_tokens`（收益本身就是估算值）
  与 TUI / 绑定的对应展示一起去掉；`/stat` 的「各角色占用（估算）」改成**条数**、没有上报时明说「尚无 API 上报」、
  落盘原文量改用**字节**（不再 `/4` 折 token）。
- **新增安全网**（不是估算的替代品，是判据失手的兜底）：`LlmError::is_context_overflow`（400 + 关键词，
  与 `is_stale_file_error` 同款）→ `aturn` 命中就**无视水位**强制 `compact(Auto)` 一次再发（没压动就照常失败）。
  理由：水位只来自上报，首次请求前没有任何值；单条输入本身把窗口撑满时判据必然失手。
- 验证：`cargo test` 208（lib，含新增 `api_400_error_kinds_are_classified`）+ 4（bin）全绿；
  clippy 仍 14（与基线一致）；绑定 `maturin develop` + `pytest` 27 全绿。

## 2026-09-30（压缩事件类型化：`CompactEvent` 取代裸 `Value`，判别看 `kind`）

用户从「`compact_event` 能不能加进 `TurnEvent`」一路追问（「`on_event` 加 `Session` 参数呢」「新增一个 `CompactEvent` enum 呢」）。讨论结论：

- **压缩事件不进 `TurnEvent`**。它是**内部记账**——`context gc` 靠它保护原文、`/stat` 靠它计数、resume 靠它登记窗口块——而 `TurnEvent` 是**展示流**（可丢、可限流、`Session::compact` 那条路压根没有 `on_event`）。给 `on_event` 加 `&mut Session` 也走不通：调用点全在「已借了 self」的上下文里（`self.llm.stream(&self.messages, …, |c| on_event(…))`）、`model_call` 是 `&self`、工具那批 future 并发跑（`&mut Session` 独占），实测 E0500 / E0524 两处编不过。
- **但「用 `Value` 传」确实太松**：键名在四个地方裸摸（`entry.get("level")==Some(3)` 探窗口、`/stat` 分桶、`/clear` 归档**手搓同形状 JSON**、测试断言），形状一改就静默失效。于是定义 `context::CompactEvent`（`#[serde(tag = "kind", rename_all = "lowercase")]`，variant `Tool` / `Turn` / `Session` ↔ 现有 `kind` 值）+ 三个构造器（`tool` / `turn` / `session`，summary 的 200 字截断收在这里）+ `level()` / `raw_path()` 访问器。
- **去掉冗余 `level`**：类型由 variant 决定，落盘只留 `kind` 标签（`{kind, ts, path, hash, summary?}`）；旧会话里多出来的 `level` 被 serde 忽略（逐条 `from_value`，单条坏只丢自己）→ 新增回归用例 `legacy_and_current_event_keys_both_load` + 落盘形状断言（`clear_window` 用例里钉住 `meta` 行不含 `"level"`）。
- **事件里的键名去掉 `raw_` 前缀**（用户问「`raw_path`/`raw_hash` 是不是可以变更为 `path`/`hash`」，并追问当初为什么叫 `raw_*` → 翻 init commit 的 `src/pie/context.py`：`raw_` 的本义是「**压缩前**的原文」，注释原文就是「原始文本长度（压缩前），tokens() 比例估算用」）：`CompactEvent` 的字段直接用短名 `path` / `hash`，`#[serde(alias = "raw_path")]` / `alias = "raw_hash"` 保旧会话可读。⚠ `referenced_raw_paths` 是**裸读 `Value`**（不走 serde）—— 那里改成 `path` 优先、`raw_path` 回退，否则旧会话的引用查不到，`gc --delete` 会删掉还在被引用的原文。`Message` 那族的 `raw_*` **不动**（`raw_len` / `raw_tokens` 是「原文尺寸」，token 比例估算必需）。（`spill` 与 `raw` 是两套词：`extract_spill_path` 描述「溢出落盘」这个动作，`raw_*` 描述「那份件是原文」。）
- **顺带消掉回调参数**：`maybe_compact` / `compact` 改成返回 `(CompactStats, Vec<CompactEvent>)`，「`on_compact: Option<&mut dyn FnMut(Value)>` + `noop` 空实现 + `drop(on_compact)` 借用收尾」三处别扭一起消失；`aturn` 里改成攒本地 `Vec<CompactEvent>`、回合末登记 `windows` + 并入字段。
- **删过时标记**：`compact` / `CompactMode` 上的 `#[allow(dead_code)]` 与「⚠ `Tools`/`Turns` 暂时没人构造……等 TUI 落地时接线」注释（TUI `/compact` 与 Python 绑定都在用）、`Session::compact` 上同款 attr、重复的 doc 行。
- 验证：`cargo test` 207（lib）+ 4（bin）全绿；绑定 `maturin develop` + `pytest tests` 27 全绿（`compression_history()` 输出形状不变——仍是 dict 列表，只是少了 `level` 键、`raw_path`/`raw_hash` 改名 `path`/`hash`）。

## 2026-09-29（`--cwd` / `/cd`：数据目录跟着工作目录走，并在会话里留一条 cwd 说明）

- **`--cwd` 之后重解析数据目录**（用户点名要）：`Storage` 是在 `Config::load` 时定型的，而 `--cwd` 的 `set_current_dir` 在更晚的 `run()` 里 —— 原来切了工作目录、数据目录却不变。现在 `set_current_dir` 后紧跟 `config.storage = Storage::from_env()`：`PIE_DIR` 仍优先，其次新 cwd 下的 `.pie`（存在才算），最后 `~/.pie`。
- **TUI 新增 `/cd <路径>`**（手动切工作目录，与 `--cwd` 同一口径）：`set_current_dir` + 重解析 + 刷新 `App.storage` 与 `session.config.storage` + 重抓状态栏快照 + 作废 `@` 补全索引；`~` 会展开；数据目录真的换了就顺带写进说明里。
  切换后**往会话历史与消息流各插一条 system 说明**（`已切换工作目录：<绝对路径>`）——历史那条是给模型的 cwd 上下文（模型得知道目录变了），流里那条是给人看的（`· ` 开头的 Notice）。
- 验证：`cargo test` 206（lib，含 `tui::app::tests::cd_switches_working_dir_and_announces_it`）+ 4（bin）全绿；`cargo clippy --all-targets` 仍 14（与基线一致）。CLI 实测：`--cwd <含 .pie 的目录> sessions` 读的是那个 `.pie/sessions`，`--cwd <无 .pie 的目录> sessions` 回到 `~/.pie`。

## 2026-09-29（落盘收敛成 `config::Storage::store`；文件名不换 uuid）

用户问了两件事：`session::store_blob` / `context::write_raw` 的文件名是 hash、`session::save` 是 timestamp，**能不能统一成 uuid**？四个目录函数（`sessions_dir` / `context_dir` / `files_dir` / `windows_dir`）**能不能都放进 `config.rs`**？后来又问：**能不能合成一个 `store(StoreType)` 入口**？

- **文件名不换 uuid**。三处看似「三种 id 生成法」，实际是**两种语义**：会话文件是「事件 id」（`chat-<unix 秒>-<微秒>`，唯一 + 天然可排序），而 `files/` 图片副本与 `context/` 原文是**内容寻址**（`img-<sha256[:16]>` / `<prefix>-<hash>.txt`）。后者不是「拿个哈希当文件名」，而是功能：同内容只存一份（图片重复粘贴幂等，`blob_is_content_addressed_and_0600` 钉住）、跨会话共享同一份副本、`context gc` 的引用计数才有意义。换 uuid 会把这些换成「同内容存 N 份」 + 引 `uuid` 依赖，而时间戳已到微秒、单机根本撞不上。**收尾：不动**（要统一就统一「命名形状」，不是 id 生成算法）。
- **目录 + 落盘合进 `config::Storage`**（就是先叫 `Paths` 的那个类型，按用户要求扩充）：一个 root + `sessions()` / `context()` / `files()` / `windows()` + **`store(StoreType) -> Stored`**。三种形态在 enum 里、策略**不合并**：`Blob`（图片副本，0o600 + 原子写）/ `Raw`（压缩原文）内容寻址 + 幂等（已存在则不动），`Window`（`/clear` 窗口块）名字带 unix 秒、要留住每一次归档；内核统一成 `write_atomic`（先写同目录临时件再 rename）。它挂在 `Config.storage`（`#[serde(skip)]`）与 `ToolCtx.storage`（bash 全文落盘那条链）上。
- **参数是 `&Storage`，不是 `&Config`、也不是函数内部取默认值**：`ToolCtx` 只装 storage（没有 Config）；落盘也只需要「写哪儿」。内部取 `Storage::default()` 会让测试隔离退回进程级 `PIE_DIR` + `env_lock`（不能并行）——上一轮专门消除了这个。
- **三个薄壳已删**：`context::write_raw` / `context::write_window_block` / `session::store_blob` 不再存在，调用点直接 `storage.store(StoreType::{Raw,Window,Blob})`；`content_hash` / `image_hash_id` 搬进 `config`（`context.rs` 不再需要 sha2）。四个 `config::*_dir()` 自由函数更早就删了（它们每次都去读环境变量，而 `context.rs` 里还手拼过一个 `pie_dir().join("sessions")`——布局散落的证据）。
- **`pie_dir()` 搜索顺序**：`PIE_DIR` → 当前目录下的 `.pie`（**存在**才算，不凭空造）→ `~/.pie`；抽成纯函数 `pick_root(env, cwd, home)` 直接测顺序，不必改进程 cwd。
- **顺带删 `Message.raw_hash`**：全仓库没有任何读者（只在 `llm.rs` 声明 + 三处赋值），是纯元数据噪音。旧会话 JSONL 里的 `raw_hash` 被 `#[serde(default)]` 静默忽略，反序列化不受影响。⚠ manifest 里的 `raw_hash` **保留**：`pie context info` 会原样打印整行给用户看，那是审计信息。
- 验证：`cargo test` 205（lib，含新增的 `pie_dir_prefers_env_then_local_then_home`）+ 4（bin）全绿；`cargo clippy --all-targets` 的 warning 数与 `HEAD` 基线一致（14 → 14）。
- 还没做：存量测试从 `PIE_DIR` + `env_lock` 迁到 `Storage::at(临时目录)`（只迁了 `context::tests::gc_keeps_referenced_files_only` 作示范），迁完才能解锁测试并行。

## 2026-09-29（把「pie 是 pi 的 Rust 实现」写进描述）

- **三处描述统一交代 pie ↔ pi 的关系**（用户：「我想强调 pie 和 pi 的关系，pie 是一个类似 pi 的极简 agent，用 Rust 实现」，
  并给了出处 `https://github.com/earendil-works/pi`）：
  - 先**去查了 pi 的真实情况**（`api.github.com` + 它的 README）：*Pi Agent Harness*，TypeScript，拆成
    `pi-ai`（统一多 provider LLM API）/ `pi-agent-core`（工具调用 + 状态）/ `pi-coding-agent`（交互式 CLI）/ `pi-tui`（差分渲染 TUI）
    等包；**明确声明不内置权限系统**（"runs with the permissions of the user and process that launched it"，要隔离自己上容器）。
    这条正好对得上 pie 的 YOLO 取舍，所以描述里可以点明「同样不内置权限系统」而不是泛泛说"类似"。
  - `README.md` 首段重写：`pi` 的 **Rust 重实现** + 四条定位（OpenAI 兼容的统一 LLM 接口 / agent 循环 / 差分渲染 TUI / coding agent CLI）
    + 「同样极简、同样不内置权限系统」+ **名字是 π 的谐音（π → pie）**。⚠ 没写"多 provider"（pie 只走 OpenAI 兼容端点）、
    没写"可扩展"（pie 没有 pi 那套扩展/插件），只写了两边真正一致的东西。
  - `Cargo.toml` 的 `description`：顺手修了两个**事实错误**——它写的是「**pie** 的 Rust 重构」（自指，应为 pi），工具名写的是
    `write`/`shell`（本仓实际是 `writ`/`bash`）。
  - `AGENTS.md` 首段 + `MEMORY.md` 的「项目定位与仓库」各加一句来历（未来的 agent 看到命名不用猜）。

## 2026-09-29（`file_stem` → `config::name_of`：落盘命名的三个函数归位）

- **`session::file_stem` 改名为 `config::name_of` 并搬进 `config.rs`**（用户：「那把 file_stem 改名为 name_of 移入 config.rs 吧」）：
  它本来就是「从落盘路径取回**文件名主干 = id**」，跟 `config::hash_of`（路径 → hash 段）是一对——搬过去后
  `config` 里凑齐**三个一队**（`Hash` 段那一节的文档写明了分工）：
  * `hash_id(data)`：内容 → hash（`Storage::store` 起名用）；
  * `name_of(path)`：路径 → 文件名主干（= id，`img-<hash>` / `chat-<秒>-<微秒>`）——**别拿它当 `hash_of` 用**（主干带前缀）；
  * `hash_of(path)`：路径 → hash 段（metadata 只要 hash 时用）。
- 消费点：`session.rs` 取图片副本 id（与 `__meta__.files` 的键同形）、`cli.rs` 取会话 id（`list_sessions` / `file_id_index`）。
  顺带把这条"别用 `hash_of` 顶替"的坑写进两侧的注释（上一轮用户问的正是这个）。
- 验证：`cargo test` 206 + 4 全绿（改名没有行为变化）、clippy 14 不变；`pie sessions -l 3` 实跑（走的是 `name_of`）✓。

## 2026-09-29（新建 `cli.rs`：把 CLI 用到的会话/文件查询搬出 `session.rs`）

- **新模块 `src/cli.rs`**（用户：「新建 cli.rs，把 session.rs 里面会被 cli 用到的函数都放进去」）：装了 6 项
  `SessionInfo` / `list_sessions` / `GC_PROTECT_HOURS` / `iter_session_files` / `file_id_index` / `collect_file_garbage`
  （共 185 行）+ 它们的那条用例（`garbage_needs_no_reference_and_past_protect_window` 顺手改成 `Storage::at(临时目录)` 注入，
  不再动进程级 `PIE_DIR`）。留 `session.rs` 的是会话本身（加载 / 落盘 / 回合循环 / 图片上传）与 `Session` 的方法
  （`Session::new/load/resume/aturn` 这些**搬不动**——它们是类型的 API）；`file_stem` 变成 `pub(crate)` 给两处共用。
- **命名说明**（写进模块头与 AGENTS）：`cli` 是**按用途**取的（CLI 子命令的数据源），不是"只在 CLI 编译进来"——
  它一直在 lib 里，TUI（同一二进制）与 Python 绑定（`pie.list_sessions`）也用它。
- 调用点：`main.rs` 5 处 `session::…` → `cli::…`；绑定 1 处 `pie::session::list_sessions` → `pie::cli::list_sessions`。
- **顺手修了绑定侧两处早就编不过的调用**（HEAD 里就坏：`pie::session::list_sessions(limit)` 少个 `storage` 参数、
  `guard.compression_history()` 这个方法上午已被删）→ 现在传 `Storage::default()`（与 CLI 同口径）并用 `compaction_events`；
  `bindings/pie-py/.venv` 不在，重建了（`uv venv --python 3.12` + `maturin develop`）→ **27 个 pytest 全过** ✓。
- 验证：`cargo test` 206 + 4 全绿、clippy 14（搬完先多出 1 条 `empty line after doc comment`——内联 `dispatch_tool` 时留下的孤儿 doc，已清）；
  真二进制跑 `pie sessions -l 3` / `files list` / `files gc`（三条都打到了搬走的函数）✓；绑定 pytest 27 ✓。

## 2026-09-29（压缩流水并入 `__meta__`：manifest 文件消失）

- **压缩事件不再单独写 `~/.pie/context/<会话名>.manifest.jsonl`，改住会话文件首行 `__meta__.compaction_events`**
  （用户：「既然每次 compact 都会修改 session 文件，为啥不直接把 manifest 放入 `__meta__`？」）：
  - 先纠了前提：**compaction 本来不改会话文件**（它只 append manifest + 写 `context/` 原文；会话文件只在 `save()` 时整份重写）。
    但用户的方案在"磁盘自洽"上是对的：把流水放进 `__meta__` 后，**流水与消息里的 `raw_path` 指针同一趟车落盘**，
    不存在"账记了、指针没记"的半吊子状态；崩溃场景也只是"两边都没落盘 → 原文成孤儿 → 被回收是正确行为"。
  - 落地：`Session.manifest: Option<PathBuf>` → `Session.compaction_events: Vec<Value>`（`ephemeral` 不再需要"关记账"的开关——
    不 `save` 自然不落盘）；`save()` 写 `meta["compaction_events"]`（空则不写），`load()` 读回来；
    `aturn` / `compact()` 用**本地 Vec 收集**（`self` 那时被借出去跑回合）再 `append` 进字段，`clear_window` 直接 push；
    `compression_history()`（读文件）删掉，`/stat` 直接用字段。
  - 删掉：`context::write_manifest`、`session::record_compact`、`session::manifest_path_of`、`Session.manifest` 字段。
  - `referenced_raw_paths()` 改成**只扫 `sessions/`**（首行流水 + 消息指针都在同一个文件里）→ `context info` 也改成遍历会话首行；
    `collect_context_garbage` 不动（它用的是同一个引用集）。
  - ⚠ **不做兼容**（用户明确）：老会话的 `context/*.manifest.jsonl` 不再被读 → 那些老原文失去保护（下次 `gc --delete` 会回收）。
  - ⚠ **一个我引入的回退，已发现并把范围做成可见**：老设计里 manifest **固定落在 `context/`**，所以哪怕会话文件在 `sessions/` 之外
    （`pie -s /tmp/x.jsonl`）也顺带保护了它引用的原文；新设计里"记录跟着会话文件走"，**外部路径的会话不在 gc/verify 的统计里** ✗。
    现在 `context info` / `gc` 的输出与 `--help` 都写明"只统计 `<sessions>` 里的会话"（`gc` 的那种情况会真的被回收 → 指针落空，
    优雅降级但原文没了）。要彻底修的话得在 `save()` 时给外部会话另落一份引用索引（待定，用户未拍）。
  - 验证：`cargo test` **206 + 4 全绿**、clippy 14；**端到端两轮真实回合**（临时 `PIE_DIR` + 临时 config 把 `[tools.bash] _max_bytes`
    压到 20000 逼出落盘）：① 会话在 `sessions/` 里 → `__meta__` 里有 `compaction_events`、`context info` 打出它、
    `gc` 报 0 个未引用、`verify` OK ✓；② 会话在 `/tmp/xxx/s.jsonl`（默认目录之外）→ 复现了上面那条回退 ✓。

## 2026-09-29（`pie_dir` 内联 `pick_root`；「不为测试拆代码」写进规矩）

- **`config::pick_root` 内联进 `pie_dir`**（用户：「pick_root 内联进入 pie_dir。不要为了方便测试把功能拆那么碎。代码也要给我读的。」）：
  搜索顺序（`PIE_DIR` 非空 → `<cwd>/.pie` 存在才算 → `~/.pie`）本来就是 12 行三步，之前拆成「纯函数 + 问环境」两半，
  唯一理由是能注入 `env`/`cwd`/`home` 直接测顺序。现在合成一个 `pie_dir()`，文档里写明**别再为了好测拆开**。
  * 用例从 4 档缩到能测的 2 档（`PIE_DIR` 非空 / 空串回落到 `$HOME/.pie`，`HOME` 用环境变量注入），
    `<cwd>/.pie` 那档要改进程 cwd（会污染并行跑的用例）→ **放弃覆盖**，用例里明写了原因 + 一个 `cwd 有 .pie 就跳过` 的守卫。
  * 顺带把记忆改了：**判据只有「有没有第二个消费者」，不含「值不值得独立测试」**——之前那条把测试算进判据被用户否掉了
    （全局 `~/.pie/memory.md` 与项目 `MEMORY.md` 都改成「要测就注入环境变量那类能测的，测不到的几行让读者看代码」）。
- 验证：`cargo test` 206 + 4 全绿、clippy 14。

## 2026-09-29（落盘命名与 store 接口收口）

一次提了四件事，逐条落地并核过代码：

- **`Storage::store` 只返回 `PathBuf`**（原来返回 `Stored { id, path }`）：三个变体里只有 `Blob` 的调用方用得到 id，
  而「id」本来就等于文件名主干（`session.rs` 扫 `files/` 时也是用 `file_stem` 反推的）→ `Stored` 一并删掉。
  要 hash 段的元数据改走新的 **`config::hash_of(path)`**（命名形状的逆运算；`context.rs` 的私有同名函数搬进 `config` 共用，
  消费方：manifest 事件、窗口块元数据）；后来用户又把 `hash_id` 也移出 `store`，两个现在贴在一起当一对（`hash_id`：内容 → id，`hash_of`：路径 → id）。。
- **`content_hash` + `image_hash_id` 合并成 `store` 里的嵌套 `fn hash_id(data: &[u8]) -> String`**（sha256 前 16 位）：
  本来就是同一件事（一个收 `&str`、一个收 `&[u8]`，后者只多拼个 `img-` 前缀）。顺带修掉一处重复计算：
  `session.rs` 写窗口块的 manifest 时又算了一遍 `content_hash(&raw)`，现在从落盘路径反推。
- **`Window` 前缀对齐**：`window-<unix 秒>-<hash>.jsonl` → **`window-<hash>`**（与 `Raw` 的 `<前缀>-<hash>`、`Blob` 的
  `img-<hash>` 同一套形状）。⚠ 这不只是改名：`Window` 从「每次新建」变成**内容寻址**（同内容 ⇒ 同名 ⇒ 已存在则不动），
  「什么时候清的」由 manifest 的 `ts` 记着；`StoreType` 上面那段「两种策略别合并」的注释也一并改了。
- **扩展名**：`Raw` / `Window` 的 `.txt` / `.jsonl` 去掉（纯装饰，`.txt` 对"美化过的 JSON 原文"还是误导）——
  文件名主干因此严格等于 id。**`Blob` 的 `.png`/`.jpg` 保留**（我的建议，理由：那是用户数据，可能被双击 / 拖出去打开，
  扩展名是「格式」的唯一载体，`image_ext(mime)` 就是为它存在的）；要一并去掉是一行的事。
- **连带改到的地方**（好几处是测试当场抓出来的）：
  * `collect_context_garbage` 原来按 `extension == "txt"` 筛 → 去掉扩展名后它会**一个都收不到**（`gc_keeps_referenced_files_only`
    当场红了）；现在按**命名形状**筛（嵌套 `fn is_stored`：`<前缀>-<16 位十六进制>`）。
  * 8 个调用点（`context.rs` × 3 + 测试助手、`session.rs` × 2、`tools.rs`、`clipboard.rs`）改收 `PathBuf`；
    `blob()` / `raw()` 两个测试助手跟着改签名；窗口用例加强成「manifest 的 `raw_hash` == 文件名里那段」。
- 验证：`cargo test` **206 + 4 全绿**、clippy 14；另外用真二进制 + 临时 `PIE_DIR` 手造 `context/bash-<hash>` + `turn-<hash>` +
  `s.manifest.jsonl` 跑了 `context info` / `gc` / `gc --delete`：只列出**未被引用**的那个、manifest 自己不误删 ✓。
  （小坑：`PIE_DIR` 连**配置**一起重定向 → 临时目录里没配置，真跑回合得加 `-c ~/.pie/config.toml`，否则 401。）
- 留个观察：`files/` 的 gc 判据仍是「不以 `.` 开头 + 未被引用」，没有像 `context/` 那样查命名形状——要不要对齐待定。

## 2026-09-29（TUI 更新不及时：一轮吃整队事件）

- **修「工具已经在跑，状态栏还显示思考中、消息流里那条工具行也还没出现」**（用户报的现象：思考显示很久、明明已开始调工具、Pane 也没更新）：
  - **根因不在渲染，在事件消化速度**：`App::run` 的主循环是「一轮 = 一帧」，而 `select!` 里 `rx.recv()` **一轮只取一个回合事件**。
    模型一次长思考能吐几百上千个增量，工具调用排在这些增量**后面** → 队列越积越长，界面落后现实几秒到几十秒。
    实测（19,200 行、接近用户会话规模）：**debug 一帧 ≈ 13ms**（layout 跳过 2.6ms + 全量克隆行 10.5ms，
    用户跑的就是 dev 构建）→ 每秒只吃几十个事件；release 一帧 ≈ 5ms（克隆行占 5.0ms / 其余 0.2ms）。
  - **修法**：新增 `App::drain_turn_events`（`try_recv` 把积压的整队一次消化），主循环收到一个事件后立刻调它。
    滞后上界因此从「队列长度 ÷ 帧率」变成**一帧**；事件的状态更新本身很便宜（`String::push_str` / 压一个 cell），
    贵的是渲染，而一队只画一帧还顺带省掉了中间的 markdown 重解析。
  - 用例 `app::queued_turn_events_all_land_in_one_pass`：四个事件（reasoning / 两个正文增量 / 工具调用）一轮吃完，
    状态落在**最后一个**事件上（状态栏 = 工具、消息流里已有那条工具行、正文增量攒在同一条 cell 里）。
  - **仍未做**（这次量出来是每帧的大头）：`render` 里那句「把全部行克隆成 `Vec<Line>` 喂 `Paragraph`」在 19k 行时
    是 10.5ms(debug) / 5.0ms(release)——治法 B（`Pane::window(offset, height)` 只借出可见那几十行）能把它降到 ~O(视口)，
    用户此前说过 `Pane::window` 先不做，所以留着（这次有了数字，随时可以捡起来）。

## 2026-09-29（README 补压缩一节）

- **README 加了「上下文压缩」一节**（用户：「可以把我们的特色功能【上下文压缩】这块也写进 README.md 里面去」；
  README 本身刚被用户手动精简过——删了「写一个工具」、构建依赖说明、绑定的构建步骤等，本节按精简后的风格写）：
  三级（工具级 / 轮次级 / 会话级）各自「压什么 → 压成什么」一张表 + 自动/手动触发口径（可用输入预算 = `context_window - reserved_tokens`，
  `soft_ratio 0.8` → `target_ratio 0.55` 迟滞）+ 只升不降 / 内容寻址 / 两个目录的分工 + 一段 `[compaction]` 的 TOML 示例。
  事实逐条对着 `context.rs` 的模块文档与 `config.rs` 的默认值核过（`keep_last_steps=7`、`ToolCompaction{head:30,tail:50}`、
  `CompactionConfig{soft_ratio:0.8,target_ratio:0.55}`、`compaction=false` 整段关、某级 `false` 只关那一级、
  `/compact [tools|turns|auto]` 且**不含会话级**——会话级整窗归档是 `/clear` 的事）。
  ⚠ `auto` 这条只有 `palette::COMMANDS` 的**描述**里有、候选表里没有对应条目（handler 落到 `_ => Auto`，所以能用）——顺手记下这个不一致。
- **TOML 示例补全三级**（用户：「把 compaction 的 toml 写详细些，把 turn/session 级也加上」）：示例里现在顶层 `keep_last_steps`、
  `[compaction]`（`turn` / `soft_ratio` / `target_ratio`）、`[compaction.tool]`（head/tail）、`[compaction.session]`（head/tail）都在，
  每条带注释说明含义与默认值；表格里会话级那行也补了「摘要里留头 3 / 尾 5 轮」。
  顺手**用真二进制验证**了这段 TOML：把它原样抠出来 `pie -c <tmp> context info` → exit 0（配置被接受）；
  注释里承诺的几种写法也逐个试过——`compaction = false`、`compaction = true`、`[compaction] tool = false`、
  `session = false` + `turn = false` 都能解析。

## 2026-09-29（词级折行加回）

- **`WrapMode::WordOrGlyph` 加回，生产档位回到词级**（用户：「现在，扩展 `wrap_segments` 函数，让它支持 `WrapMode::WordOrGlyph` 吧（类似
  ratatui-textarea 里面的那样）」）：
  - 算法**对齐 `ratatui-textarea 0.9.2` 的 `wrap_word_chunks`**：按 UAX#29 词边界（`split_word_bound_indices`）切块 → 贪心装块
    （块自己一个字符不切）→ 只有「单个块就比整行宽」（超长英文词 / 长 URL）才退回按字硬断。这正是收口前那版的算法，所以 `WrapMode`
    两档的用例期望值也是原来那批（`中文 ab 中文` 那两条、`abcdefghij` 那条都在）。
  - `unicode-segmentation` 依赖随之加回（只服务这一档；注释里写明了）。
  - **生产档位从 `Glyph` 换回 `WordOrGlyph`**（调用点那个字面量）：实测同一段内容，URL 不再从 token 中间被切开
    （`https://` / `api.deepseek.com/chat/completions`，之前是 `https://api.de` / `epseek.com/...`）。想让生产走逐字断就改那一处字面量。
  - 一处**已知差异**（写进 `WrapMode` 的 doc）：控件的逐字断走**字素簇**（`grapheme_indices`），pie 这边走**字符**（`chars()`）→
    emoji 组合序列 / 组合记号在控件那边不会被劈开，在这儿可能被劈。要完全对齐得给 `grapheme_indices` 也留一条路（暂时不值当）。
  - 副作用（恢复旧状态）：**消息流（词级）与输入框（`Glyph`）又折在不同位置了**——上一节「两边规则终于一致」那条收益随之取消；
    AGENTS/MEMORY 已改回「不跟消息流对齐」的措辞。
  - 用例：`wrap_segments_pins_both_modes`（两档各自的具体期望 + 两档共同的不变量：不超宽 / 不丢字符 / 无空段 / 空行占一行 / 宽 0 当 1）。
    测试 203 + 4 全绿，clippy 14。

## 2026-09-29（折行收口到逐字）

- **消息流折行收口成只剩「逐字硬断」**（用户：「`wrap_segments` 收口成只支持 `WrapMode::Glyph` 吧。`WrapMode` 只保留 `Glyph`，
  `wrap_line_into` 里面调用 `wrap_segments` 直接用 `WrapMode::Glyph`，移除 `WRAP_MODE` 常量」）：
  - ⚠ 动手前 `WRAP_MODE` 已经是 `WrapMode::Glyph` —— **不是本轮改的**（早先它写的是 `WordOrGlyph`，是用户在工作区里手改的），
    这次等于把那个手改**固化**下来。
  - 落地时比字面请求更进一步：既然只剩一个档位，`WrapMode` 枚举 + `WRAP_MODE` 常量都是**单值仪式**，而「逐字硬断」的逻辑
    **恰好就是原来那个 `hard_split`** → 于是三者一起消失，`wrap_segments(text, width)` 就是这个循环本身
    （`wrap_line_into` 直接 `wrap_segments(&text, width)`）。`unicode-segmentation` 也不再被本仓使用 → 从 `Cargo.toml` 移除
    （它仍在树里，是 `ratatui-textarea` 等的传递依赖）。
  - **行为变化**（这是取舍，不是 bug）：CJK **完全不变**（汉字本来就各自成块，长句照样填满整行 —— 2026-09-28 修「中文长句第一行很短」
    的那个收益保住了）；**英文词 / URL / 路径会从中间切开**（例：宽 9 下 `alpha beta` → `alpha bet` / `a`）。
    另一个副作用是**消息流与输入框的规则终于一致了**（输入框一直固定 `WrapMode::Glyph`），2026-09-28 那条「两边折在不同地方」的
    已知代价消失（几何仍不共用：输入框是控件自己的折行）。
  - **跟进**（用户：「想留 `WrapMode` 当『以后再加档位』的钩子」）：把 `WrapMode` 类型留回来了——`enum WrapMode { Glyph }` +
    `wrap_segments(text, width, mode)`，调用点显式传 `WrapMode::Glyph`（**仍不留 `WRAP_MODE` 常量**，那是用户上一轮明确要删的）。
    `wrap_segments` 里那个 `match mode { Glyph => {} }` 不做事，作用是**分叉点**：加档位时编译器在这里报 non-exhaustive，逼着就地处理；
    枚举的 doc 里记下了「曾经实现过的词级档去哪儿找」与「不收的档位及原因」，需要时照它恢复（连 `unicode-segmentation` 一起）。
  - 用例：`wrap_segments_is_word_level_and_cjk_friendly` 改写成 `wrap_segments_breaks_by_glyph_and_is_cjk_friendly`
    （逐档对拍的断言删掉，保留「不超宽 / 不丢字符 / 无空段 / 空行占一行 / 宽 0 当 1」这些不变量，另钉几条逐字断的具体期望）。
    测试仍 203 + 4 全绿，clippy 14。

## 2026-09-29（折行收口）

- **`wrap_segments_with` 并入 `wrap_segments`**（用户：「`wrap_segments_with` 和 `hard_split` 是否可以直接合入 `wrap_segments`？」）：
  - `wrap_segments_with(text, width, mode)` 原本只是「让用例能逐档钉规则」的一层包装（生产入口是 `wrap_segments` 那个 1 行转发）。
    现在合成一个 `fn wrap_segments(text, width, mode)`，**唯一生产调用点**（`wrap_line_into`）显式传 `WRAP_MODE` ——
    档位开关仍然只有那一处，但少一层间接。
  - 顺带**整族降为模块私有**（`wrap_segments` / `wrap_line_into` / `WrapMode` / `WRAP_MODE`）：自 `fit_tables` 删掉后，
    `markdown.rs` 不再借用它们（那只借了 `wrap_line_into`），全仓代码级的引用只剩 `pane.rs` 自己 ✓
    （`input.rs` 里的 `WrapMode` 是 `ratatui_textarea::WrapMode`，另一个类型；本仓 `WRAP_MODE` 只剩注释提及）。
    要换档位仍然是改 `pane.rs` 里的那一行常量。
  - **`hard_split` 变成 `wrap_segments` 里的嵌套函数**（用户：「把 `hard_split` 函数放到 `wrap_segments` 做嵌套函数呢？」）：
    它的两个调用点（`Glyph` 档整行硬断、词级档里「单个块比整行还宽」的回退）**都在 `wrap_segments` 体内**，参数齐全、
    不捕获任何环境（所以是 `fn` 而不是闭包），`mod tests` 也不直接调它 → 嵌套是「作用域自证 + 读的时候不用跳」，
    零成本（嵌套 `fn` 与顶层 `fn` 编出来的东西一样，只是可见范围小）。**没有**内联进主体：内联要复制两份，
    两处逻辑一样但语义不同（整行 vs 退回单块）。
    另一条被否掉的方案：把两档统一成「先切块再装块」（`Glyph` 档 = 每字符一块），那样只剩一个调用点，但那条路径
    要为整行多分配一份逐字符 chunks（现在零额外分配），收益只是少一个 12 行的函数 → 不值。
- **`rebuild_line` 并入 `wrap_line_into`**（用户：「`rebuild_line` 并入 `wrap_line_into` 吧」）：它只有一个调用点（`wrap_line_into`
  末尾那个 `.map`），合并后 `chars[start..start + len].chunk_by(|a, b| a.1 == b.1)` 一组一段、每组拼一个 `Span` ——
  语义与老实现逐字逐段合并完全一致（相邻同样式合一个 span），但少了一个函数、读的时候在一个函数里看全「展开 → 折 → 重装」三步。
  - 顺手**补了一条以前没人钉的用例** `wrapping_keeps_per_char_styles_and_the_line_style`：折行切开 span 时每个字的样式要跟着走
    （`aa`/`ab`/`bb` 逐行断言 span 数量与样式）、行级样式照搬、空行也占一行 —— 这正是 `rebuild_line` 存在的理由，以前只靠"重构时别改错"。
  - 测试 202 → 203，clippy 仍 14。
  - 验证：`cargo test` 202 + 4 全绿（逐档钉规则的那个用例一字未改，改的只是调用名）、clippy 14 不变。



- **`tui-markdown` 0.3.9 → 0.3.10**（用户：「看一下 tui-markdown 是否可以升级到 0.3.10，我看它说修复了一些 markdown table 的 bug」）：
  - 0.3.10 的 CHANGELOG 有 **`wrap tables to the configured width` (#200)**：新增 `Options::table_width(u16)` + `from_str_with_options`；
    **默认 `None` = 表格保持自然宽度**（即 0.3.9 的行为），所以是**纯增量** API。
  - 升级动作：`Cargo.toml` 的版本约束改 `0.3.10` + `cargo update -p tui-markdown`。实测：**204 + 4 测试全绿、clippy 不变**，
    同一张表格在 TUI 里的渲染**逐行一致**（拿升级前后两张截图 diff 过）→ 对 pie 现有代码是行为中立的一跳。
    顺带**少了 3 个依赖**（0.3.10 把测试 helper 改成 dev-dependency：`toml_parser` / `winnow` / `yansi` 从依赖树里掉出去）。
  - **顺手删掉了 pie 自写的表格重排**（用户：「看起来我们的 `fit_tables` 相关可以删除了」）：
    * `markdown.rs` 从 **584 行 → 247 行**：删掉 `fit_tables` / `shrink_table` / `border_kind` / `rule` / `render_row` /
      `cell_width` / `padding` / `shrink_widths` / `split_cells` / `infer_alignments` / `edge_spaces` / `trim_spans` /
      `line_text` 与那批边框常量、`Align`，以及只测这些私有助手的两个用例（`shrinks_the_widest_column_first` /
      `trims_padding_but_keeps_inner_spaces`）。测试 204 → 202，其余**全绿**（含「宽表被收进宽度」「窄表原样」
      「单元格样式保留」「畸形表不崩」四条——它们现在钉的是上游行为，正好当回归网）。
    * `render()` 改成 `from_str_with_options(src, &Options::default().table_width(width.max(1) as u16))`；
      `markdown.rs` 不再借用 `pane::wrap_line_into`（那份折行只剩消息流自己在用）。
    * **收益不止删代码**：上游的预算是「含边框 + 内边距 + **外层引用/列表前缀**」，而老实现靠行首 `┌` 检测表格块、
      预算里也不含 `> ` 前缀 → **引用块里的表格以前根本认不出来**。实测（tmux 46×26）：正文表与 `> ` 引用表都精确收在
      46 列内（引用表 45 = 含 `> ` 前缀）。
    * 差异（接受）：列宽取整与断行位置换成上游口径（同一张表 26/25 vs pie 的 25/26），两者都不超宽。



- **修两个「只活在实时视图里的东西没进会话」的 bug**（用户：退出再 `pie -r` 回来，「• Thought for …」那行消失；`[请求失败]` 的行不再标红/带 `✗`）：
  - 定位（对着那份真实会话核过：**351 个 tool 调用一个不少**，回放都会渲染——所以不是丢消息）：
    | 实时视图 | 会话里有什么 | 回放原来的结果 |
    |---|---|---|
    | `• Thought for 3.4s`（**TUI 自己计时**） | 只有 `reasoning_content`（"思考过"，**没有时长**） | 整行消失 |
    | `✗ [请求失败] …`（`Cell::Error`，红色） | 一条内容为 `[请求失败] …` 的 assistant 消息 | 当普通正文渲染（不红、没 `✗`） |
  - **`Message.thought_ms`（新字段，只进会话文件）** + `session::ThoughtClock`：口径与 TUI 的 `settle_thought` **一致**
    ——**首个 reasoning 增量**起算、**首个正文增量**停下；计时器作为参数传进 `model_call`，`aturn` 在 push 那条
    assistant 消息时 `take_ms()` 写进去。`to_api()` 不带它（有单测钉住，否则就把本地元数据喂给模型了）。
    ⚠ 第一版把计时包在**重试**那条路上（正常成功的回合量不到，真实跑一条才发现 `thought_ms=None`）→ 改成传 `&mut ThoughtClock`，
    两条路都过它。
  - **`Cell::from_messages` 补两条回放规则**：带 `thought_ms` 的 assistant 消息先补一行 `Cell::Thought(...)`（排在正文之前）；
    正文以 `[请求失败]`（`session::ERROR_TURN_PREFIX`，为此外提成 `pub(crate)`）开头的走 `Cell::Error`（红 `✗`）。
  - 实测：真跑一条带思考的回合 → 会话里 `thought_ms: 294` ✓；对那份真实会话的副本 `pie -s` 回放 → 失败行现在是
    `✗ [请求失败] …`（抓 `capture-pane -e` 验证颜色 = RGB `243,139,168` = mocha `red`）✓；新建一条 `thought_ms=294` 的会话回放 →
    `• Thought for 0.3s` 回来了 ✓。
  - 用例：`pane::history_replay_restores_thought_line_and_error_turn`（Thought 行在正文之前 + 失败回合是 `Cell::Error`）、
    `llm::thought_ms_stays_out_of_the_api_payload_and_survives_jsonl`（不进请求体 + 能落盘读回）。
  - ⚠ **旧会话**里的思考行回不来（那时候没写 `thought_ms`）——从今往后的回合才有。另外顺手确认：这套「实时视图 vs 回放」的
    差异只有三处，另两处**是有意如此**——`!cmd` 手动命令不进会话上下文（`palette` 的帮助里就这么写的）、`Notice`/`Retry`
    是纯瞬态（权限告警、重试进度）。



- **`history.rs` → `pane.rs`，并把排版下放到 `Cell`（用户：「history.rs 改名为 pane.rs。其它的按你的来」）**：
  - **改名**：`src/tui/history.rs` → `src/tui/pane.rs`（`pane::` 前缀；`src/tui/mod.rs` / `app.rs` / `input.rs` / `markdown.rs` / `status.rs` /
    `tools.rs` 的 `use` 与注释一并更新）。本文件里的历史条目**不动**（当时就叫 `history.rs`）。
  - **`Row` 改成自描述**：`src_line: usize`（跨 cell 唯一的逻辑行号）→ `continues: bool`（「我是上一条显示行的软换行续行」）。
    `src_line` 全程只被 `slice_text` 拿去比「相等」，而它需要跨 cell 唯一**只是为了表达续行**；换成每行自带的标记后，
    「编号必须全局唯一」这条隐式契约消失——每条 cell 的首行与 cell 之间的空行都是 `continues = false`，跨 cell 复制不再有粘连风险。
  - **排版下放到 `Cell`**：新增 `Cell::render(index, palette, &mut Pane)`——每个变体只回答「我要显示哪些**逻辑行**、行首装饰几格、首行的 mark 是什么」；
    `Pane::push_line` 是**唯一的折行出口**（按 `width - indent` 折 → 给续行补等宽空白 → 写 `indent` / `continues`），`end_cell()` 负责 cell 之间的空行。
    `pub fn layout(cells, palette, width, lean) -> Vec<Row>` 退化成十几行 driver（`cells.iter_mut()` + `render` + `end_cell`）。
    - 顺带删掉 `Cell::User` 的**重复预折**（以前它得先自己按 `width - PREFIX_CELLS` 折一次，因为统一折行那一步传的是整宽）。
    - `Pane` 是累加器（整个消息流一次分配），不是「每 cell 一个 `Vec<Row>`」。
    - 折行仍**只有一份**（所有变体都走 `push_line` → `pane::wrap_segments`，`WRAP_MODE` 仍是一处开关）；
      这条不变量是刻意守的：漏一条逻辑行、或折错宽度，`Paragraph` 就会二次折行、`Row` ↔ 源文本的映射就错了。
  - **去掉 `Layout` 外壳**：`Layout { rows }` + `Layout::slice_text` → `pub fn slice_rows(&[Row], r1, c1, r2, c2) -> String`；
    `App.layout: history::Layout` → `App.rows: Vec<Row>`（顺带解掉与 `ratatui::layout::Layout` 的撞名——以前只能写全路径）。
    `slice_rows` 保持纯函数，单测直接喂 `&[Row]`（不必构造 `App`）。
  - 实测：`cargo test` 198 例全绿（lib 194 + bin 4），含新增 `pane::tests::a_single_cell_renders_on_its_own`
    ——单条 cell 现在可以直接钉（`Cell::render(index, palette, &mut Pane)`），不必经过整个 `layout`。
  - **markdown 缓存从 `Cell` 搬到跨帧的 `Pane`**（用户：「MarkdownCache 是不是应该放在 Stream 里面？」）：
    - **不能放每帧新建的 `Stream`**——那玩意儿 `Pane::layout` 里现造现丢，命中率会是 0；缓存的意义就在**跨帧**（未变的助手格每帧都得复用结果）。
    - 所以新类型 `pub struct Pane { rows: Vec<Row>, md: HashMap<usize, MarkdownCache> }` 当**跨帧状态**（`App.rows` → `App.pane`），
      `Pane::layout(&mut self, cells: &[Cell], palette, width, lean)` 是唯一入口。
  - **`Stream` 并入 `Pane`**（用户：「你都有 Pane 了，那是不是 Stream 可以并入 Pane?」）：可以——`Stream` 原本只是为了把「本帧旋钮 + 借来的 `md`/`rows`」
    打包给 `Cell::render`；`markdown_lines` 已经把行拷出来返回（借用当步结束），所以这些方法直接挂在 `Pane` 上不会撞借用。于是
    `Pane { width, lean, rows, md }`（旋钮也住进去：折行宽度与缓存键本来就是同一个量），`push_line` / `end_cell` / `markdown_lines` 全归它，
    全仓不再有 `Stream` 这个类型；`Cell::render(&self, index, palette, &mut Pane)` 拿到 palette（`Pane` 不能持有 `&Palette`——`Palette` 归 `App`，
    自己存一份会与 `/theme` 热切打架）。
    - 顺带三得：① `Cell` 回归**纯数据**（`Assistant { text }`，与其他变体同形；`assistant(text)` 不用再藏 `md: Default::default()`），
      `Cell::render` 从 `&mut self` 降为 `&self`，`layout` 收 `&[Cell]`；② `rows` 不再每帧新分配（`self.rows.clear()` 复用容量）；
      ③ 缓存键 = **cell 下标**（内容/宽度变了指纹就不符 → 重算；越界项（`/clear`）每次 `layout` 清掉），
      于是 2026-09-28 那条治法①（复用上一帧排版）现在只需在 `Pane` 里比一下——`rows` 与它要用的缓存已经同居。
    - 用例：`pane::markdown_cache_is_keyed_by_cell_index`（命中 / 内容变 / 宽度变 / 清越界项），另真终端（tmux 100×24）
      拉一份带 markdown 助手消息的会话验了渲染（标题 + 加粗 + 表格重排都正常）。
  - **消息流加滚动条**（用户：「pie 的 tui 能显示 scrollbar 吗？」→「可以的」）：
    - ratatui 0.30 自带 `Scrollbar` / `ScrollbarState`（`ratatui::widgets`）；三个数现成：`content_length = pane.rows().len()`、
      `viewport_content_length = body.height`、`position = scroll_top`。
    - **右缘固定留 1 列**（`bar_w = body.width > 2`）：不能做成「有溢出才留」——折行宽度与 markdown 缓存键都吃 `width`，
      一旦跨过溢出阈值就换了宽度 → 整段历史重排 + 全部缓存失效。因此改成“列一直留、只是不画”。
    - 不溢出时**不画**（否则 `ScrollbarState` 会把 thumb 铺满整条轨道）；样式：thumb = `palette.cancelled`（overlay2）、
      `track_symbol(None)`（只画 thumb，codex / claude code 那种极简风）。
    - `Surface` 多了第三种 **`Scrollbar`**（`surface_at`：输入框 > 滚动条 > 消息流；`App.body` 现在已经是**不含**滚动条列的那块，
      因此坐标/选区数学不用改）→ 在滚动条上点/拖不再被当成“在消息流里按下”而起框选，而是 `App::scrollbar_jump(row)`：
      屏幕行 → 偏移的**线性**映射（不按 thumb 尺寸精确抓取；第一版够）。
    - ⚠ 用户确认的语义：**拖动滚动条只改 `scroll_from_bottom`**，输入框（连同它自己的视口/草稿）与状态栏都在垂直布局的下方几行，
      完全不受影响——已用 `app::dragging_the_scrollbar_scrolls_only_the_stream` 钉住（还断言了不产生选区/复制提示）。
    - 用例：`app::scrollbar_appears_only_when_the_stream_overflows`（不溢出→右缘空白；溢出→有 thumb 但不铺满）；
      真终端（tmux 60×16、40 组消息）验过渲染。
  - **per-cell 排版缓存（治法②的骨架；`Pane::window` 按要求先不做）**（用户：「我感觉还是 per-cell cache 设计上好点。要不 `Pane::rows` 换成 `Vec<Vec<Row>>`？」→「暂时先不要 `Pane::window` 了」）：
    - `Pane.rows: Vec<Row>` → `blocks: Vec<CellBlock>`（`CellBlock { rows: Vec<Row>, key: u64 }`，**与 `cells` 下标一一对应**）。用户说的是裸
      `Vec<Vec<Row>>`；这里把「这一条的行」与「生成它的指纹」捆成一个结构体，免得两份并行数组（`rows`/`keys`）错位。
    - **失效规则**：`frame_key`（`width` / `lean` / `palette`，为此给 `Palette` / `Status` 补了 `derive(Hash)`——`Color` 本来就 `Hash`）
      变了 → **全部**重排；否则逐条比 `cell_key(cell)`（variant + `len` + 首尾 64 字节，退到字符边界——把 `markdown::floor_char_boundary`
      提成 `pub(crate)` 复用，不再写第二份切字节的代码；`Tool` 另算 `status`/`manual`/`body.len()`）。
      **指纹每帧现算 ⇒ App 那 ~20 个改 cells 的地方一处都不用碰**（否则得在每处 bump 版本号，漏一处就是"界面显示旧内容"）。
    - `Cell::render` 的入口从 `&mut Pane` 收窄成 `&mut CellSink`（`{ palette, width, lean, md, rows: &mut Vec<Row> }`）：
      per-cell 结构下写入口必须只写"当前这一条"。于是上一轮并进 `Pane` 的 `Stream` 以 `CellSink` 之名回来了（**部分回退是必要的**，
      不是反复横跳）。
    - 对外接口：`Pane::rows()` → `Pane::total()`（Σ 每块行数，O(#cell) 整数加法，不维护"运行总和"那份额外不变量）+ `Pane::iter()`
      + `Pane::slice_text(...)`（原 `slice_rows` 的**逐行逻辑一字未改**，只是"第 n 行"先逐块定位；`Row` / `continues` / `indent` 全保留）。
      `App` 只改 4 处（`max_scroll` / 滚动条 / 复制 / `render` 里那行克隆走 `iter()`）。
    - 实测（release，3200 行 / 600 条 cell）：首次（含 markdown 解析 + 高亮）21ms；**全量重排（md 已缓存）2.6ms**（与 2026-09-28 那条基线对得上）；
      **输入没变 → 24–39µs（≈100×）**；**只改尾巴一条 → 34µs（工具格）/ 96µs（助手格，含它自己的 markdown 重解析）**。
    - 用例：`pane::only_changed_cells_are_relaid_out`（`#[cfg(test)] relaid` 计数钉住"没变的块被跳过"；宽度/简洁/配色变了必须全排）、
      `pane::incremental_layout_matches_a_full_one`（含"往头部插一条 → 下标整体移位"也必须与全量逐行一致）、
      `pane::a_tool_status_change_invalidates_that_cell`（漏判的后果是一直显示 `•`）、`pane::clearing_cells_drops_blocks_and_cache`、
      `app::a_second_identical_frame_relays_out_nothing`。
    - **仍未做**：`Pane::window()`（治法 B：只把可见那几十行借出去，干掉 `render` 里那行全量克隆——用户要求先不做）；窗口定位现在靠
      逐块扫（O(#cell)），没上前缀和。

  - **`CellSink` 并入 `CellBlock`（顺带消掉 `Pane.md`）**（用户：「可不可以把 `CellSink` 合并到 `CellBlock` 里面呢？」）：
    - 合并的关键是**让 markdown 缓存跟着块走**：`CellBlock { rows, key, md: MarkdownCache }`（一条 cell 一个，本来就是这样）。
      于是 `Pane.md: HashMap<usize, MarkdownCache>` 整个消失，「下标 → 缓存」那层映射与它的 `retain` 清理都不需要了。
    - 写入方法（`push_line` / `end_cell` / `markdown_lines`）直接挂在 `CellBlock` 上，只需多收一个 `width`（本帧宽度）；
      `Cell::render(&self, palette, width, lean, block: &mut CellBlock)`——**拿到 `&mut CellBlock` 就物理上只能写这一条**，
      比原来"传 index + 借整个 Pane"更安全（也更少参数：`index` 不必再传）。
    - `Pane` 因此缩到 `{ frame_key, blocks, #[cfg(test)] relaid }`：`width` / `lean` 两字段不必存（它们在 `frame_key` 里，
      每帧按参数传给 `render`）；`CellSink<'a>` 这个带生命周期的类型彻底消失。
    - 单 cell 用例因此更好写：直接 `CellBlock::default()` + `cell.render(&palette, 40, false, &mut b)`。

  - **`Cell::render` 也收进 `CellBlock`**（用户：「`Cell::render` 是不是也应该移入 `CellBlock`？」）：可以，而且这次顺手把 `layout` 里那四步记账
    一起收了——入口变成两个方法，语义各自清楚：
    - `CellBlock::is_stale(&Cell) -> bool`（指纹对不上 ⇒ 这一块得重建）；
    - `CellBlock::rebuild(&Cell, palette, width, lean)` = `rows.clear()` → `key = cell_key(cell)` → `render(...)` → `end_cell()`；
    - `CellBlock::render(&mut self, cell, palette, width, lean)` 就是那个 per-variant 的 match（回答「这条消息长什么样」），
      内部写行一律走 `self.push_line(...)` / `self.markdown_lines(...)`——**一条 cell 的全部排版知识（策略 + 折行 + 缓存）都在块上了**。
    - `Pane::layout` 的循环因此只剩「判失效 → 计数 → 交给块重建」（原来是清空/记指纹/render/补空行四行散在循环里）。
    - 单 cell 用例同步简化：`b.rebuild(&Cell::User("你好".into()), &palette, 40, false)`（`rebuild` 自带尾部空行，断言相应改）。

  - 屏幕模型不变（继续用 alt screen；上一轮讨论过的 inline / 终端 scrollback 路线仍只是 `2026-09-28` 那条里的选项）。
  - 顺手：`cells_from_history(&[Message])` → **`Cell::from_messages(&[Message]) -> Vec<Cell>`**（与 `push_assistant_text` / `finish_tool` /
    `cancel_running` 一致：`Vec<Cell>` 级操作都挂在 `Cell` 上），调用点变成 `Cell::from_messages(&session.full_history())`。
  - 顺手：`Cell::assistant()`（无参，造完还得在外面 `if let` 把正文塞进去）→ **`Cell::assistant(text: impl Into<String>)`**；
    `push_assistant_text` / `from_messages` 里那两处「先造再填字段」的 dance 随之消失。`Cell::Assistant.md` 保持具体类型
    `MarkdownCache`（**不**改成 `Option<MarkdownCache>`：缓存是纯 memoization，`None` 不表达任何状态，只为少写一个 `Default::default()` 不值得）。
  - 顺手：把 `tool_result_ok`（`pub fn -> bool`）**合并进 `tool_status`**——它全仓只被 `tool_status` 自己与单测调用（`app.rs` 只用 `tool_status`），而且那个 `bool` 对取消哨兵返回 `true`（语义是错的）；
    现在「取消 → ⏹ / `[exit=N]` 头 → ✓✗」只有一处判定，输出协议的说明也并到 `tool_status` 的 doc 里。

## 2026-09-28

- **判明「每帧全量重排」的底细**（用户连问：`App::render` 何时调用 / 滚轮会不会触发重排 / 能不能用终端自带的 scroll。**本轮不改行为**，只记录决策与数据）：
  - **触发**：`App::run` 每轮循环开头**无条件** `terminal.draw`，而每轮由三类事件之一唤醒——终端事件（每个按键/鼠标/粘贴/焦点）、
    回合事件（**每个流式增量** / 工具结果 / 重试进度）、tick（`TICK` ≈ 66ms，15fps）。**没有 dirty 标记**：什么都没改的事件也跑一整帧
    `render`。ratatui 双缓冲 diff 只省「**终端写入**」，省不掉我们自己的 `render`。
  - **成本**（`history::layout` 走完**全部** cells + 再把**全部行克隆**成 `Paragraph` 的行；唯一缓存是 markdown 渲染）：release、本机——
    3.5k 行 → layout 2.5ms / 整帧 4.0ms；5k 行 → 4.1 / 6.5ms；20k 行 → 16.3 / 25.9ms（debug ≈ ×10）。**线性**；光 tick 就够触发（5k 行 ≈ 10% 一个核），
    流式时每个 delta 一帧（50 tok/s ≈ 1/3 个核，且与按键同一线程）。
  - **三条可选治法**（收益/复杂度排序）：① 复用 `Layout`（cells / `body.width` / `lean` / 配色没变就不重排——`App` 里本来就存着上一帧的
    `self.layout`，tick 帧因此零成本）；② 流式只重排**尾部那个 cell**（`Vec<Row>` 追加 + per-cell 行数缓存，`total` 用累计和）；③ 喂 `Paragraph`
    时只克隆**可见窗口**那几十行。①+③ 约 30～50 行，② 再 50～80 行。⚠ 首次 / 宽度变化仍得全量测一遍（不知道总行数就没法算滚动范围）。
  - **「把历史交给终端 scrollback」（`pie -r` 时全量打印一次、之后只增量）技术可行**（ratatui 0.30 就有 `Viewport::Inline(h)` +
    `Terminal::insert_before`，文档明说插进去的行直接进终端 scrollback），**但那是换屏幕模型、不是优化**：① 历史必须「定稿才印」
    （我们的流有就地更新：助手回答流式增长并重新折行、`⟳`→`✓`、重试块就地改写、`lean`/主题改历史外观）→ 当前回合得留在底部活区；
    ② 已打印的行是按当时宽度硬折的，resize 后旧行宽不再正确（现在是重排一遍、任何宽度都对）；③ 要放开**鼠标捕获**才能滚终端 scrollback
    → 失去输入框内鼠标点选/拖选、滚轮按位置分派、自绘框选复制（换回终端原生选择），键盘 PgUp/PgDn 也失效；④ 换来每帧零重排 +
    **原生 scrollback（退出后还能翻/搜/选）**，稳态性能与 ①②③ 等价。真要动就分两步：先 inline viewport + 定稿才 `insert_before`
    （**保留**鼠标捕获），再决定要不要放开捕获。

- **输入框回归「普通编辑器」：固定 Glyph、不要 `› `**（用户：「1. 输入框的 WrapModel 固定为 Glyph；2. 输入框其实不需要 ">"，简化一下 input 的代码吧」）：
  **推翻的是上一条的「输入框所见 = 发送后所得」那层对齐**——不再值得为一个预览去维护「两边同档 + 同宽 + 逐字对拍」。
  - `input.rs`：`set_wrap_mode(WrapMode::Glyph)` 写死（不再读 `history::WRAP_MODE`，也不再要 `WrapMode::widget()` 映射）；
    去掉左边 `PREFIX_CELLS` 内边距与 gutter 里的 `› ` 绘制（`available_width` / marker 块 / `Paragraph`/`Span` import 一并删）
    → 内容区 = 整块扣掉上边框那一行（渲染时从块里取一次记进 `Input::inner`，`inner_rect()` 直接读它）；`screen_rows` 回到**逐字断**的简单实现（只服务
    鼠标命中 / 框高 / 残影擦除 / 视口滚动复刻，仍由 `screen_rows_matches_the_widget_wrapping` 钒着）；`desired_height` 直接数行数。
  - `history.rs`：`WrapMode::widget()` 删（不再需要映射）；枚举/`WRAP_MODE`/`wrap_segments` 的文档改成「**消息流**的档位」，
    `PREFIX_CELLS` 不再是「两边共用」；`wrap_segments` 的「输入框里折在哪，发出去就折在哪」全部删掉（历史里保留 `› ` 悬挂缩进不变）。
  - `app.rs`：删掉 `the_input_box_previews_how_the_message_wraps_in_the_flow`（不变量没了）；插入符坐标断言回到 `x = 0` 起。
  - ⚠ 代价（已知并接受）：同一段文字在输入框（逐字断）与消息流（词级断）**折在不同的地方**，宽度也差 `› ` 那 2 格。
    消息流那边不动：词级 + CJK 友好仍是它的默认（`WRAP_MODE = WordOrGlyph`，修「中文长句第一行很短」靠的是它，跟输入框无关）。
  - 顺手：上一条验证开关时把 `WRAP_MODE` 留成了 `Glyph`（与文档说的默认不符、测试也看不出来）→ 改回 `WordOrGlyph`。


- **输入框把那三处「复刻」交回给控件**（用户：「做一下 1、2、3 吧」——即上一条分析里列的三步）：
  1. **gutter 走控件的块内边距**：`render` 里 `self.area.set_block(Self::block(border))`（`Borders::TOP` +
     `Padding::left(PREFIX_CELLS)`），**边框也交给控件画**；我们算几何统一走 `Self::block(Style::default()).inner(rect)`
     —— 与控件内部同一个 `Block::inner`，不用各自再算一遍边框/内边距（以前是我们手算 `x+2`/`width-2`）。
  2. **插入符位置不改我们的复刻**：`caret_position()` 改成读控件的 `screen_cursor() -> { row, col }`（它直接
     给「光标在第几折行、行内第几列」，tab 也算得对）。⚠ 这使它**依赖控件那份屏幕表是当前帧的**（控件只在
     渲染时按区域重建）→ 只能在 `render` 之后调（doc 已写明；两个用例改成先渲染一帧）。
  3. **滚轮按位置分派**（补上缺口：以前无论指到哪都滚消息流）：`App::wheel` —— 指针在输入框里且
     [`Input::overflows`]（内容真的比框高）→ `Input::wheel(mouse)` 转给控件（`MouseScrollUp/Down` → 视口 ±1 行 +
     把光标收进视口）；否则滚消息流（**不留死区**：光标常停在输入框上，那时滚轮该照旧翻对话）。
     ⚠ 控件挪的是视口**基础偏移**（每帧重算时以它为 `prev`）→ 复刻的 `scroll_top` 必须跟着一起挪
     （同一个饱和加减），否则命中/残影擦除会差这么多行 —— `Input::wheel` 里就一并做了。
  测试：`input::inner_rect_matches_where_the_widget_draws_the_text`（真渲染后对 `inner_rect` / 边框 / 文字起点 /
  插入符位置）、`input::overflows_only_when_the_text_is_taller_than_the_box`、
  `app::wheel_over_the_input_scrolls_the_input_unless_it_cannot_scroll`（两种指针位置各验一遍）；
  两个 `caret_position_*` 用例改成「先渲染一帧再断言」（新口径的代价）。


- **折行档位收成一个开关**（用户：「history 能使用 `WrapMode::Glyph` 嘛？目前的代码可以轻松切换不同 WrapMode 嘛」）：
  能，但**不是 history 单方面的事**——输入框的渲染在控件里、消息流的折行在我们手里，两边档位一分叉就回到
  「换行不一致」那个 bug。所以档位做成**一处开关**：
  - `history::WrapMode`（只收两档：`WordOrGlyph` 默认 / `Glyph`）+ `history::WRAP_MODE` 常量：输入框
    `new_area` 里 `set_wrap_mode(WRAP_MODE.widget())`、`wrap_segments` 也按它折 → **改这一行两边一起换**
    （与 `ratatui-textarea` 的映射只有 `widget()` 一处）。
  - 不收的档位与理由（写在枚举 doc 里）：`None`（水平滚动 = 另一套几何：`desired_height` / 命中 /
    滚动复刻都不成立）、`Word`（超长词不切 → 行超出宽度；输入框那边控件裁掉、消息流的 `Paragraph`
    会**二次折行** → `Row` ↔ 源文本的映射就错）。
  - `wrap_segments_with(text, width, mode)` 是带档位的实现，`wrap_segments` 只是「按当前档位」的包一层
    （唯一生产入口）；它 `pub(crate)` 只为让镜像用例逐档对拍 —— `input::screen_rows_matches_the_widget_wrapping`
    现在**两档各跑一遍**（同一批文本 × 3 种宽度，拿真控件渲染出来的逐行对拍）。
  - 验证：把 `WRAP_MODE` 临时改成 `Glyph` → 全套 192+4 全绿（`app::the_input_box_previews_how_the_message_
    wraps_in_the_flow` 也绿 = 两边真的一起换了），再改回去同样全绿。要加用户可见的开关就把 `WRAP_MODE`
    换成 `App` 的字段（从 `config.tui` 读，`[tui] lean` 那条路），并把 `mode` 传进 `history::layout`。

- **折行 / 行首宽度收拢到一处**（用户：「history.rs 和 input.rs 关于 word wrap 这块的实现可以合并一下嘛？……`history::PREFIX_CELLS`
  和 `input::GUTTER` 可以合并嘛？」）：两处的**实现**本来就只有一份（`input::screen_rows` / `desired_height` 直接调
  `history::wrap_segments`），这次把剩下两样也合并：
  - `input::GUTTER` 删掉 → 用 `history::PREFIX_CELLS`（转 `pub(crate) const`、类型改 `u16`）；`input::char_width` 删掉 →
    用 `history::char_width`。于是「用户消息前几格」与「显示宽度尺子」都只有一处定义，`input.rs` 只从 `history.rs` 借
    （与 `markdown.rs` 借 `wrap_line_into` 同一路数）。
  - 给 `wrap_segments` 补直接用例 `history::wrap_segments_is_word_level_and_cjk_friendly`（ASCII 词整块 / 中文逐字 /
    空白块装得下就留在上一行、否则挤到下一行 / 超长词硬断 / 空行 / 宽 0；并对一条长混排文本在宽 1..40 上验「拼回来 =
    原文、无空段、非硬断不超宽」）。
  - **合并不了的那一层**（已写在 `wrap_segments` 的 doc 里）：输入框的**渲染**在 `ratatui-textarea` 里（它按
    `WrapMode::WordOrGlyph` 自己折），而它的 `wrap_word_chunks` 是私有的、也没有「给我屏幕行表」的公开 API
    （只有 `screen_cursor()` 一个点）——所以我们只能**复刻**它来做几何（命中 / 插入符 / 高度 / 视口滚动复刻）。
    这份复刻由 `input::screen_rows_matches_the_widget_wrapping`（拿真控件渲染出来的逐行对拍）钉住，分叉就会红。

- **滚轮不会再「空转」**（用户：滚到最开始之后继续往上滚，得先往下滚同样圈数才真的动）：
  `App::scroll` 原来只夹下界 0（贴底）——`scroll_from_bottom`（离底行数，越大越靠历史开头）滚过顶以后
  还会一直加：画面上不动（`offset` 那边饱和了）、状态里却在掰圈数，得反着滚回来才见到效果。
  现在两端都夹：`scroll` 用 `max_scroll()`（折行后的总显示行数 - 消息流可见高度，取上一帧排版）夹住；
  `render` 里再按当帧的 `bottom` 兜一道（内容变短时把旧偏移夹回去，不然同样会「明明在最上面还得先往下滚几圈」）。
  测试：`app::scrolling_stops_at_the_top_instead_of_banking_extra_ticks`（按终端上报的方向一路滚到最开始 →
  夹在 `max_scroll`，立刻往回滚就见效；`PageUp/PageDown` 同路径）、`app::shrinking_the_history_clamps_the_scroll_offset`。
  顺带把方向约定写在 `on_mouse` 那里：`ScrollUp` → 往**贴底**方向、`ScrollDown` → 往历史**更早**处
  （事件名跟「画面往哪动」不是一回事，别凭直觉改反）。

- **输入框所见 = 发送后所得（折行终于一致）**（用户：附截图「输出框中的文字换行和输入框的换行不一致」）：
  两处不同步，叠起来能把同一段的断点错开 20 多格：
  1. **断行规则**：消息流自己折行（`history`）只认**空白**断点（`soft = 最后一个空格`）——中文长句没有空格 →
     整段被挪到下一行、上一行留一大片白（截图里第一行到 `YOLO` 就断了，还剩 20 多格用不上）；输入框则是
     控件自带的 `WrapMode::Glyph`（按显示宽度逐字硬断，英文词会从中间切开）。
  2. **可用宽度**：消息流里用户消息首行有 `› ` 前缀（吃 2 格），输入框没有它、用满整宽。
  改法（三处一起抳）：
  - 折行收敛成**唯一一份** `history::wrap_segments(text, width)` → `(起始字符下标, 字符数)` 列表，**词级、
    CJK 友好**：分词走 `unicode-segmentation` 的 UAX#29（`split_word_bound_indices`，与
    `ratatui-textarea` 的 `WrapMode::WordOrGlyph` **同一套**）——ASCII 词整块不切开、中日韩字与全角标点
    各自成块，只有单块自己就超过整行宽（长 URL / 超长词）才按字硬断。`wrap_line_into`（消息流 +
    `markdown::fit_tables` 的表格重排）与 `input::screen_rows` / `desired_height` 都调它 → 两边断点逐字一致。
  - 输入框：`WrapMode::WordOrGlyph`；内容区左边让出 `GUTTER = 2` 格（`inner_rect` 统一扣，命中 / 插入符 /
    折行 / 视口滚动全走它），并在那 2 格里画 `› `（颜色跟着上边框：聚焦 accent / 失焦 muted / `!cmd` 工具色；
    视口滚动过头了就不画）——这就是消息流用户消息前缀的位置。
  - 消息流：用户消息改成**悬挂缩进**——`› `（首个逻辑行）/ `  `（续行与其余逻辑行）落在**每条显示行**上，
    正文按 `width - 2` 折。新增 `Row::indent`（行首装饰格数）→ `Layout::slice_text` 复制时跳过装饰，
    于是复制用户消息拿到的是**正文**（那两类前缀都不进剪贴板，也不会多出缩进）。
  - 顺带：`desired_height` 不再用 `ceil(总宽 / 可用宽)` 估算，改按真折行数算（精确，含空行）；
    `unicode-segmentation` 走显式声明（已是 `ratatui-widgets` / `unicode-truncate` 的传递依赖，挂在 `tui` feature 下）。
  测试：`input::wrap_width_keeps_room_for_the_gutter`、`input::screen_rows_matches_the_widget_wrapping`
  （拿真控件渲染出来的每一行逐行对 `screen_rows`：中文长句 / 英文词 / 超长词 / 短句 × 3 种宽度 —— 它是
  `wrap_word_chunks` 的复刻，两边一分叉命中与光标就会偏）、
  `app::the_input_box_previews_how_the_message_wraps_in_the_flow`（同一段文字在消息流与输入框里**逐行逐字**
  相等，含行首那 2 格）；`history::wrap_never_exceeds_the_width_and_keeps_every_char` 改成用 `slice_text`
  验「不丢字符」；另更新受影响的插入符 / 高度 / 复制断言（都只差那 2 格）。

## 2026-09-24

- **`Config::load` 认 `OPENAI_API_KEY` / `OPENAI_BASE_URL` 环境变量**（用户：「Config 在加载 config file 的时候如果存在环境变量 OPENAI_API_KEY，Config.api_key 应用环境变量值。同理 … OPENAI_BASE_URL」）：
  `load()` 读完 TOML（或落回 `Config::default()`）后按环境变量覆盖这两项——空串 / 全空白视为没设（不把配好的值冲掉），只改内存、**不写回配置文件**（`save()` 照旧落盘当前值），`Config::default()` 本身不受影响。新增 `config::env_api_key()` / `env_base_url()`（共用 `non_empty_var`）。
  测试：`config::tests::env_overrides_api_key_and_base_url`（设了就用、空串/空白回落到文件里的值、`config_file` 不受影响；用例自己存取 `ENV_LOCK` 并恢复环境变量）与既有 `ensure_config_file_*` 改按 `env_*()` 算预期（否则有环境变量时那条会假失败）。
  验证：起本地假端点，`OPENAI_BASE_URL=http://127.0.0.1:PORT/v1 OPENAI_API_KEY=sk-from-env pie --models` → 服务端看到 `GET /v1/models auth=Bearer sk-from-env`；不设环境变量时看到的是配置文件里的 `sk-from-file`。

- **文档精简（AGENTS.md / README.md / MEMORY.md）**（用户：「简化、聚焦一下 AGENTS.md README.md MEMORY.md，多余的内容可以放到 CHANGELOG 里面」）：三份文件互相重复（TUI / 环境 / 已知限制在 `AGENTS.md` 与 `MEMORY.md` 各写一遍），且把大量属本文件的「历史决策 / 机制解释」也写进了正文。按分工收敛——`AGENTS.md`（会被拼进 system prompt）只留「怎么在本仓库干活」的规则（TUI 聚焦色板 / 宽字符残影 / IME 锚点等机制故事各压成一条规则）；`README.md` 面向人，TODO 只留简短列表；`MEMORY.md` 只保「当前状态」、约定一律指向 `AGENTS.md`。被删的机制细节此前多已在本文件记录，未记录的一并补上：`README.md` 原 TODO 里那大段 `parallel_tools` 实现细节搬到 2026-09-23（见下）。随后按用户「可以不提及之前的 Python 版本了，反正信息已经保存在 docs 里面」再做一轮：去掉三份文件里对**旧 Python 实现**的提及（工具名、会话 JSONL 格式、`[exit=0]` 兼容、TUI 行为等只保留与当前行为相关的规则；两版差异一律只在 `docs/python-legacy.md`）——**Python 绑定**是现役功能，不算旧版本，照留。同一轮又把**源码注释**也同步了（用户：「同步代码」）：`src/` 与 `bindings/` 里那批 `// 对齐 Python 版 …` 之类的**旧版注脚**一并去掉（保留「为什么这么写」的理由）；顺手修掉几处过时注释——`main.rs` 顶部那段「分阶段迁移进度」表（还写着旧工具名 `write`/`shell`、`⬜ TUI`）、`lib.rs` / `tools.rs` 模块头里的 `write`/`shell`、`context.rs` 里「Rust 还没实现 `/clear`」都按现状改写。`bindings/` 里指自身那层纯 Python 助手（`pie/_tool.py`）与「Python 绑定」本身的话照留。
- **crate 名 `pie-rs` → `pie`**（用户：「我就是要把 pie-rs 改成 pie」）：根 `Cargo.toml` 的
  `package.name` 改成 `pie`（`[lib]` / `[[bin]]` 本来就都叫 `pie`），连带：`bindings/pie-py` 的
  path 依赖键（`pie = { path = "../.." }`）、两份 `Cargo.lock`、CLI 的 `#[command(name = …)]`
  （`--help` / usage 里的程序名）、`llm.rs` 的 UA（`pie/<version>`）、以及 README / AGENTS / MEMORY /
  docs 里所有 `pie-rs` 提法。**不动 `_pie_rs`**（Python 扩展模块名 `pie._pie_rs`，属于绑定 API）。
  本机 `CARGO_TARGET_DIR` 约定目录顺带改名（`~/.cache/pie-rs-target` → `~/.cache/pie-target`）。

- **状态栏也跟着窗口焦点变灰**（用户：把输入框那种 unfocused 变灰的渲染也用到状态栏）：
  `status::status_line` / `activity_line` 多一个 `focused` 形参（与 `Input::render` 同款），
  失焦时那两档“活的”颜色从 accent 降成 muted：**名字**（本来 accent + BOLD，失焦连粗体一起去掉）与
  **活动指示的转圈/耗时**；用量 / 余额本来就用 muted，不动——于是失焦后整条状态栏只剩 muted 一档。
  测试：`status::status_line_dims_when_the_window_loses_focus`（断 accent span 3 → 0、BOLD 去掉、
  整行只剩 muted、**文本一字不变**）与 `app::empty_input_caret_and_border_follow_window_focus`
  扩到三处一起验（真渲染后读 buffer：光标块 / 上边框格 / 状态栏首字的 fg）。
  - 收尾把那条规则收进色板（用户：「emphasis 是不是应该放到 theme.rs？input 是不是也应该用这个？」）：
    `theme.rs` 新增 `Palette::style_emphasis(focused)`（accent / muted）、`style_selection()`
    （accent 底 + accent_text 字）、`style_caret(focused)`（聚焦 = selection、失焦 = muted 底）。
    于是：状态栏不再有本地 `emphasis`，`input.rs` 的上边框 / 选区 / 光标三处、`App::paint_selection`
    都改成调色板的方法——**「失焦就 muted」只在色板里写一遍**（视图里再写就会各自跑偏）。
    新增 `theme::focus_sensitive_styles_live_in_the_palette`（含“不是把 mocha 写死”：换个 flavor 也成立）。

- **`@` 补全支持「索引之外」的三档锚点**（用户：默认只列当前目录，想要 `@..` 上级 / `@/` 根 / `@~/` 主目录）：
  - 设计：`@` 那套本来是**cwd 索引**（`ignore` 扫一遍，按 `.gitignore` 排除），只管 cwd 子树——列不出它之外的目录；
    而 `~/…` 那种树根本建不起索引。所以这三档走**实时 `read_dir`**（[`files::external`] + [`files::list_dir`]）：
    只列一层、名字前缀过滤、字典序、点文件不收（与索引同口径），不算忽略规则。
  - 插入的文本仍是**能直接给 `read` 用的路径**：`..` → `../…`（相对，保留用户写法）；`/` → 绝对；
    `~` → **展开成主目录的绝对路径**（`read` 不认 `~`，原样插进去就是坏的）。
  - 入口收敛成一个自由函数 `files::matches(fragment, index, root, limit)`：先看三档锚点（实时列目录），
    否则才用索引——所以 `@~/` **不依赖索引**，打完就弹，不必等 `@` 那次后台扫盘回来。
    连带的：`App::ensure_index` 遇到这三档直接早退（不为它们扫 cwd）——单测里没 runtime 也敢调它
    （真去 `tokio::spawn` 就会炸），正好当“没扫盘”的证言。
  - 接受目录后的「路径补全会话」照旧生效，所以 `@../src/` → `../src/tui/` → `../src/tui/app.rs` 能逐层钻。
  - 测试：`files::external_anchors_resolve_parent_root_and_home`（纯字符串解析，**不改 `HOME`**——进程级 env 会
    把并行用例拖下水）、`external_anchors_list_directories_without_an_index`（真目录：一层/前缀/点文件/无索引也能列）、
    `index_still_serves_plain_relative_fragments`、`app::at_completion_handles_parent_and_root_without_an_index`（走完
    `@..` 列目录 → Tab 接受 → 逐层钻 → 接受文件收场，并验 `ensure_index` 不建索引）。
  - 顺手：`palette::MAX_SHOWN` 被用户并行改成 12（原 7）后，两个面板用例（样本写死 10 项、断言 `rows.len() == MAX_SHOWN`）
    就对不上了；改成**样本 = `MAX_SHOWN + 8`、断言不写死数字**（验证过 3 / 7 / 12 / 20 都能过）。`files::MATCH_LIMIT`
    的注释里那句「面板只显示 7 行」也改成不写数字（免得下次再过期）。

- **文档按「Python 版已删除」的现状重整**（用户：「已经没有之前的 python 版本了，更新下相关文档描述」）：
  新增 **`docs/python-legacy.md`**——把旧纯 Python 实现那一段收在一处：旧版长什么样（入口 / 工具名 /
  模块 / Textual TUI / uv 打包）、当年「迁一块对一块」的 oracle 方法（查旧代码 `git show b188058^:src/pie/…`）、
  **现行实现与旧版的逐条差异**（按工具 / 会话上下文 / 模型 / 图片 / TUI / 提示词分组）、旧格式现在还被认的
  兼容点、以及旧版有而 Rust 版仍缺/有意不做的特性。
  - README 的 `## 与 Python 版的已知差异` 整节（131 行）搬进该文件，README 只留一段指针 + 原本的 TODO
    （另修了绑定节里过时的「M3/M5 未做」）；`AGENTS.md` / `MEMORY.md` / `docs/python-bindings.md` 里
    「Python 版仍在」的措辞一并改成「旧 Python 版（已删除）」并指向新文件。
  - **源码里的 `// 对齐 Python 版 …` 注释未动**：那是当前行为的来由（历史注脚），不是需要同步的文档。（⚠ 同日的「文档精简」条目已反转此决定：那批注脚现已同步去掉。）

- **输入框的宽字符残影**（用户报：打一文字后按退格，输入行右边留蓝色方块 / 半个汉字；`6.png`）：
  - 机制：**ratatui 的 `BufferDiff` 永远不会重画宽字符的后半格**。`Buffer::set_stringn` 写宽字形时会把后面那格 `reset()`（= 普通空格），而 `Cell::eq` 比 symbol/underline_color/skip/fg/bg/modifier/diff_option → 它与真空格**完全相等** → 那格进不了 diff（`BufferDiff` 自带一处强制重发，但只在「前一格有底色或 REVERSED 之类可见修饰」时触发，普通汉字不满足）。于是那一格只要以前被写过东西——被删汉字的右半边、placeholder 里 `⏎`/`⇧` 的碎片、光标/选区涂过的 accent——就**永久残留**在终端上。实测（抓 App 写出的格子）：退格一次只写了 2 格（新光标 + 旧光标格），被删汉字的后半格一个字都没写；`6.png` 量像素也对上了：cell 13/15/17/19 各一格 accent（4 个宽字符的后半格）+ cell 10-11（光标压在宽字符上的 2 格）。
  - 修法（`src/tui/input.rs`）：新增小控件 `StaleTail`（实现 `Widget`，直接改 frame 的 buffer；`Frame` 这版没公开 `buffer_mut()`）——每帧算出「这一显示行写了多少格」（折行后的文本宽度 + 行尾光标格），**只在这一行比上一帧短**时把 `[新行尾, 旧行尾)` 标 `CellDiffOption::AlwaysUpdate`（跳过相等判断、强制发一格空白）。两个约束：**只标已写区间之外**（正文里宽字符的后半格绝不能写——真终端上写它会把汉字擦掉半个）；只在变短那帧标（`AlwaysUpdate` 只对当帧有效，back buffer 每帧 `reset()`）。空输入时 placeholder 占着第一行 → 那行按整行算，否则提示消失后它的宽字符碎片没人擦。
  - 验证：单测 `input::tests::deleting_a_wide_char_repaints_the_stale_cell_behind_it`（拿 `CompletedFrame::buffer` 两帧比 `diff_iter`：x=3 必须发、x=1（正文里的后半格）不能发、静止帧一个格子都不发）；pty 复验（打字 / 连删 5 个汉字 / 折行两行 / 多行 + `←` 压宽字符 / 拖选后删，输入区再无游离 `·` 残影）；顺手量化静止帧开销：空输入 / 打字后 / 删字后都是 ~500 B/s（与改动前同量级，没有每帧白刷）。
  - 有意保留：**光标压在宽字符上时是 2 格 accent**（控件把光标样式涂在字形那格上，与选区同款）——用户明确说这样很好，不动。

- **bash 失败头三行并成一行**（2026-09-24 用户改的）：`[exit=N]` + `[os=…]` + `[shell=…]` → `[exit=N, os=…, shell=…]`，`tools.rs` 的 `Bash::call` 里那三行 `headers.push` 合一处。
  - 安全性：所有消费者都只看**第一行**、只问「是不是 `[exit=` 开头且不是 `[exit=0`」——`tui::history::tool_result_ok`、`context`/`session` 的头区处理（按 `\n\n` 切、与头里几行无关）、`mark_tool_spill`（找指针行）都不关心头有几行；成功路径本来就是「没有头」。代价是那行**不再是纯值**：要退出码得按 `exit=` 到逗号 / `]` 切（旧 Python TUI 那种 `text[6:].split("]", 1)[0]` 会拿到 `3, os=linux, shell=bash`——Python 实现已不在仓库，只影响用旧版本读新会话）。
  - 测试：`tools::tests::fail_header(code)` 成了期望值的单一口径（`os` / `shell` 按常量拼，不写死 linux/bash），`shell_reports_nonzero_exit_code` / `shell_exit_headers_only_on_failure` / `killed_shell_reports_minus_one` 改用 `assert_eq!`（比以前的 `starts_with` 更紧）；三行头时代的旧会话（`[exit=0]\n[os=…]\n…`）照旧被 `tool_result_ok` 认成成功。

- **回合失败不再留「没人应答的提问」+ JSONL 落盘转义控制符**（用户看 `chat-1790218823-466873.jsonl` 时发现：末尾连着两条 user 后面直接是 cancel 占位，另有 `splitlines()` 读者在那条 397KB 的工具输出上解码失败）：
  - 失败回合：`aturn` 开头就 `push_user`，而 `model_call` 出错是直接 `return Err` → 那条 user 永远没人回答，下次请求的历史里就是连续两条 user（实测 API 收，但语义脏；Python 版同毛病）。现在两处出错退出（首次请求 / file_id 失效后的重试再失败）都先 `push_error_turn` 补一条 `[请求失败] <错误>` 的 assistant 消息再抛 `Err`（前缀常量 `ERROR_TURN_PREFIX`）——调用方照旧拿 `Err`（TUI 的错误行不变），只是历史里 user/assistant 成对；与取消写 `CANCEL_TEXT` 同款。
  - 控制符：`serde_json` 只转义 C0，工具输出里的裸 C1（U+0084/U+0085/U+0088… 转义序列 dump、二进制预览里很常见）与 U+2028/U+2029 会原样落盘——JSON 合法，但 `str.splitlines()` / `bytes.splitlines()` 那类读者把 U+0085 当换行，**一条消息被劈成两半、整份 JSONL 读不出来**（本次那份文件的第 61 行就中招）。新增 `session::json_line()`（= `serde_json::to_string` + `escape_control_chars`：把 C1 与 U+2028/9 换成 `\uXXXX`，读回来仍是同一字符），session 文件与 `/clear` 的窗口块（`~/.pie/windows/*.jsonl`）都改走它。
  - 验证：`session::tests::failed_model_call_leaves_an_error_assistant_message`（故意给个 reqwest 解析不了的 `base_url`：失败后历史末尾是 `[请求失败] …` 的 assistant，不再是悬空 user）与 `save_escapes_control_chars_that_break_line_readers`（全文件无裸 C1/U+2028、行数不变、每行仍是合法 JSON、load 回来字符一致）；另拿出事的那份真会话（197 条）跑一遍 load→save：裸 C1 3 处 → 0、`\u0085` 出现 3 次、198 行全部可解析、除「重建 system + cwd + `reasoning_content` 补空串」外逐条一致。
  - 顺带：**CLI 的会话模式（`-r`/`-s` + 任务）失败也落盘**（`main.rs` 的 `Err(e) => { eprintln!; let _ = session.save(); return 1 }`）。之前这条路径失败就直接 `return 1`，那轮提问连同失败原因一起消失；TUI 本来就是退出时一定 `save()`。实测：拿真会话做副本、故意给死端点跑 `-s … "任务"` → exit 1，且文件里确实多了 `user` + `[请求失败] 连接失败: …`（199 条）；成功路径仍照旧（追加 user+assistant 后 `save()`）。

- **输入法候选框跑到鼠标点击处**（用户报的 bug：输入「你好，」后在消息流里点一下，候选框就不在输入框后面了）：
  - 根因：**TUI 从不告诉终端「文本光标在哪」**。光标块是自己画的，终端光标一直隐藏、位置停在「上一帧 diff 最后写入的单元格」上；而 IME 的锚点正是**终端光标单元格**——VS Code 的 xterm（`@xterm/xterm` 6.1，藏在 `node_modules.asar`）把隐藏的 `.xterm-helper-textarea` 摆在终端光标处（`_syncTextArea`，在光标移动 / resize / `compositionstart` 时调），Windows 就把候选框画在那个 textarea 的插入符上。点消息流时 `App::paint_selection` 只为高亮重画了**被点的那一格** → 那一格成了 diff 最后写入的位置 → 终端光标跑到点击处 → 候选框跟着跑（输入法起手 `compositionstart` 还会再同步一次，所以它「粘」在那里）。
  - 修法：`Input::caret_position()`（折行 + 视口滚动偏移都算上，宽字符按显示列）算出插入符的屏幕单元格，`App` 每帧记下（`caret`），`App::run` 在 `terminal.draw()` **之后** `terminal.set_cursor_position(...)` —— 放 draw 后面是因为挪光标的是 diff 本身，我们负责收尾。光标仍然隐藏（`Hide` 是 ratatui 每帧发的）→ 观感零变化；只有**位置**变了，终端据此摆 IME 的锚点。
  - 验证：单测 `input::caret_position_*`（含软换行那一段）+ `app::caret_follows_the_input_insertion_point`；另用 pty 跑真 TUI：每帧字节流是 `[…]\x1b[?25l\x1b[22;1H`（hide 之后必有一个 CUP），输入 `你好` → `22;5H`、按 ← → `22;3H`，**点击消息流 `(10,5)` 时帧内仍会出现 `\x1b[5;10H`（高亮那一格），但每帧最后一个 CUP 始终是插入符**。
- **输入框光标 + 上边框改成「聚焦亮、失焦暗」**（用户：空输入时光标是暗的；希望窗口被选中时高亮 → 随后又要求“上边框也同步这样”）：
  - 根因（光标）：控件的光标样式是**裸 `REVERSED`（不带颜色）** → 颜色跟着**所在行**跑。空输入时那一行是 placeholder（muted + ITALIC）→ 反色后是一块**暗灰**；有文本时那一行是正文色才变亮。即「高亮与否」跟的是“行”，不是“聚焦”。
  - 修法：光标样式就地写在 `Input::render` 里（`if focused { accent 底 + accent_text 字 } else { muted 底 }`，与选中高亮同款）→ `set_cursor_style`；上边框同理——原先是「恒 accent（`!` 开头工具色）」，现在先判焦点：失焦 → muted（盖过 `!` 的工具色）。
  - 焦点从哪来：`tui::run` 开 `crossterm::event::EnableFocusChange`（退出时关），`Event::FocusGained/FocusLost` → `App::focused`。初值 `true`：**多数终端根本不发焦点事件**，收不到就当聚焦（否则所有终端都会常暗）。
  - 验证：单测 `app::empty_input_caret_and_border_follow_window_focus`（渲染后数 accent 底格子 + 读上边框那格的颜色：聚焦 1/accent → 失焦 0/muted → 回前台 1/accent；`!ls` 时边框为工具色、失焦又回 muted），另用 pty 跑真 TUI：启动带 `38;2;137;180;250`（边框）+ `48;2;137;180;250`（光标块）；发 `\x1b[O`（FocusLost）后两者都消失、变成 `48;2;108;112;134`；发 `\x1b[I` 复原。
  - 注意：终端侧的真实光标一直是隐藏的（ratatui 只在 `set_cursor_position` 后才显示）→ 屏幕上那个“光标”就是这个色块，不是终端自己的闪烁光标。

- **内置色板改用 `catppuccin` crate**（用户：想基于 Catppuccin 官方配色来做）：`cargo add catppuccin`（v2.8，开 `ratatui` feature）→ `src/tui/theme.rs` 里那 11 个大写 RGB 字面量改成从 `catppuccin::PALETTE` 的槽位取。
  - **行为基本不变**：旧值本来就逐个对得上 Mocha 槽位（accent=blue / user=ok=green / assistant=text / tool=teal / fail=red / cancelled=overlay2 / muted=overlay0 / warn=yellow / code_bg=base）——唯一“新颜色”是 `accent_text`：旧的自造值 `#06121f`（不在 Catppuccin 里）→ **`crust`**（accent 底上的深色文字，取官方最深槽）。
  - `ratatui` feature 给的是 `impl From<catppuccin::Color> for ratatui_core::style::Color` → `Color::Rgb`；它锁的 ratatui-core 与我们 ratatui 0.30.2 的**同一份 0.1.2**（`cargo tree -i ratatui-core` 只一份）→ 类型直接对得上，**没有**任何额外新增依赖（`cargo tree --no-default-features -i catppuccin` 为空 → 绑定侧不进依赖图）。
  - 实现形态：没有手写转换助手 —— 字段类型就是 `ratatui::style::Color`，`.into()` 自己就能推出目标类型，所以直接 `m.blue.into()`；“转换从哪来”写在 `from_flavor` 的 doc 里。（最初写过一个 `fn slot()` 转发，用户问「需要吗」→ 删。）
  - 语义层不变：视图仍只认 `Palette` 的字段名，Catppuccin 槽位名只出现在 `theme.rs`。
- **另外三个 flavor 一并搬进来**（用户：「再把 catppuccin 的其它几个 theme 也一并搬过来」）：新增 **`Palette::from_flavor(FlavorName)`**（唯一的槽位映射点；四个 flavor 的槽位名一致所以映射共用）+ 四个快捷构造 `mocha` / `macchiato` / `frappe` / `latte`（由深到浅）。
  - `latte` 是唯一的**浅色**变体；其余三个都是深色。`Default` 仍是 `mocha`。
  - 测试：`palette_uses_catppuccin_mocha_values`（Mocha 五个槽位的 hex + 默认 + 全真彩）与 `all_four_flavors_are_wired_to_catppuccin`（每个 flavor 的 blue/text 取样 vs 官方 `palette.json`，能同时抓「某个 flavor 没接上」与「四个都指向同一个 flavor」；另断言 latte ≠ mocha、`from_flavor` ∥快捷构造）。
- **TUI 开始真的读 `Config.theme`**（用户：「tui 开始接入 Config.theme 吧」）：之前这个字段**只写不读**，现在启动时 `Palette::resolve(&session.config.theme)` 定色板。
  - `Palette::from_name`：认具体 flavor（`mocha` / `macchiato` / `frappe` / `latte`）、Python 那种带族名的 `catppuccin-<flavor>`，以及族名 `catppuccin`；`trim` + `to_lowercase`，重音也容错（`Frappé` / `catppuccin-frappé`）。**族名不带变体 → `mocha`**：没有 OSC 11 背景探测，照 Python 版「探测不到按深色」的习惯；要浅色得写全名 `catppuccin-latte`。
  - 认不出来：`Palette::resolve` 返回 `(mocha, Some("[theme] 认不出的主题 `x`，按 catppuccin-mocha 显示"))`；**告警文案住在 `theme.rs`**（哪些名字有效只有它知道），App 只负责在 `run()` 装好日志出口后再 `log::warn` —— 不能在 `App::new` 里提：那时已经 raw mode + 交替屏，写 stderr 会砸花屏幕（仓库头条踩坑）。
  - 视图层零改动：App 只把 `Palette` 从 `Palette::default()` 换成解析结果；其它模块仍只认 `Palette` 的字段名。
  - **仍缺**：OSC 11 探测（族名/明暗自适应）、`/theme` 运行中热切（色板启动时定下；真要热切还得让 `markdown` 的 `MarkdownCache` 按色板失效重绘）、`--theme` CLI 覆盖。（且**热切不会**改已发的工具结果——它们只是 `Cell`，颜色在渲染时现取。）
  - 测试：`theme::from_name_accepts_flavor_and_family_spellings`、`theme::resolve_falls_back_to_mocha_and_carries_a_warning`、`app::tests::palette_follows_config_theme`（真建 App：五个名字的 `palette.assistant` 对得上；“dracula” → mocha + 告警，且告警经 `push_notice` 渲染成 `· [theme] …`）。全套 **175 例**（lib 171 + bin 4）全过。
  - 实现位置：`App::new` 里多一步解析（`palette` 字段 + `theme_warning: Option<String>`）；app 测试辅助重构成 `app_with_rx_for(config)`（原来的 `app_with_rx()` 调它）。

- **TUI 代码块开语法高亮**（用户：选 A）：恢复 `tui-markdown` 的默认特性 `highlight-code`（此前为躲 C 依赖而 `default-features = false`）。fence 代码块交给 syntect 高亮，主题用内置 Base16 Ocean Dark。
  - **代价（实测，有意接受）**：依赖树 584 → 619 节点（+35）；debug 二进制 50.0 → 53.7 MB。syntect 的 oniguruma 后端是**默认特性**（`default-onig` → `onig_sys`，C，靠 `cc` 编译），且 Cargo 特性是**并集**——即使我们自己再写一行 `default-features = false, features = ["default-fancy"]` 也躲不掉。构建只需 `cc`（本机有）。
  - **流式成本**：`MarkdownCache` 按内容失效 → 每个 delta 都整段重渲染（含 syntect）；release 下 300 行代码块 ≈ 8ms/次（短块可忽略）。
  - 想回到零 C 依赖：要么改成自写极简高亮（`palette.code_bg` 早就在等这条路），要么 fork tui-markdown 把它的 syntect 依赖改成 `default-fancy`（fancy-regex，纯 Rust）。

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

- **一批工具并发/串行（`parallel_tools`）**：`Session::aturn` 的执行旋钮之一，实现在 **`Session::tool_call`**（`model_call` 的姊妹：那边问模型，这边跑工具）——整批一起跑（并发度 = `calls.len()`）或按模型返回顺序串行（并发度 = 1，`buffer_unordered(limit)` 只决定「同时 poll 几个」）。
  - 两条路**事件形状一致**：先把整批 `tool_call` 发出去，再按「谁先跑完谁先发」推 `tool_result`（被取消的推 `CANCEL_TEXT`）；消息**按调用顺序**回填（历史扁平序列与串行一致 → compaction 的 step 批次认定不受影响）；返回 `Vec<Option<String>>`（`None` = 被取消，调用方补 `CANCEL_TEXT`）。
  - ⚠ `on_event` 不能进 future（`&mut dyn FnMut` 没实现 `Sync` → `aturn` 的 future 会不再是 `Send`，TUI 的 `tokio::spawn` 编不过）：future 只算结果，事件由轮询循环在完成当下推。
  - ⚠ future 必须在 `for` 里造，**不能**写成 `map(|(i, c)| async move {…})`：闭包参数的生命周期会变成 HRTB，撞上 rustc 已知限制（#100013），报错却在调用方（`tokio::spawn`）一头雾水。
  - ⚠ **有意不同于旧 Python 版**：`tool_result` 事件的 `text` **不再截 500 字**（旧版在 `loop._run_tool_call` 里 `clip_output(text, 500)`）——Session 只搬真话，少显示是展示层的事（TUI 按 `TOOL_BODY_LINES` 截、CLI 只取首行）、少回传是工具自己配容量上限的事。
- **仓库转纯 Rust**：Python 实现（`src/pie/*.py` + `tests/*.py` + `pyproject.toml` + `uv.lock`）整体删除，`pie/` 的内容上提到仓库根（`src/`、`prompts/`、`bindings/`、`docs/`）。Python 版从此只是历史参照（`git show b188058^:src/pie/…`）。
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

