//! 消息流面板（`pane`）：每条消息一个「单元格」（[`Cell`]，纯数据），排出来的显示行（[`Row`]）
//! 与缓存存在**与它一一对应**的 [`CellBlock`] 里。
//!
//! 折行**只有一份实现**：`Cell` 只交代「我要显示哪些**逻辑行**、行首装饰几格」，
//! 折行、`indent` 的宽度记账与「续行标记」（[`Row::continues`]）都由 [`CellBlock`] 兜住
//! （唯一出口 [`CellBlock::push_line`] → [`wrap_segments`]，档位见 [`WrapMode`]）。
//!
//! 参考 codex 的 `history_cell`：单元格外只保留顺序与滚动逻辑，视图细节都在这里。

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::markdown::{floor_char_boundary, MarkdownCache};
use super::theme::{Palette, Status};
use crate::cancel::CANCEL_TEXT;
use crate::llm::Message;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// 用户消息的行首装饰宽度：首行 `› `、续行 `  `（悬挂缩进）。
///
/// 消息流按它给用户消息留悬挂缩进（正文折行宽度 = 整宽 - 它，由 [`CellBlock::push_line`] 扣）。
/// （输入框**不**跟它对齐了：那是个普通编辑器，左边不留 gutter —— 见 `docs/CHANGELOG.md` 2026-09-28。）
pub(crate) const PREFIX_CELLS: u16 = 2;

/// 工具正文最多显示多少行（codex 那种紧凑风格；超出的按 §省略）。
const TOOL_BODY_LINES: usize = 24;

pub enum Cell {
    /// 用户输入（`› ` 前缀 + 亮色）
    User(String),
    /// 助手正文（markdown 渲染；渲染缓存在 `Pane` 里按 cell 下标存，不在 cell 里）
    Assistant { text: String },
    /// 思考耗时（一轮结束留痕：`• Thought for 3.4s`）
    Thought(Duration),
    /// 工具活动：一行摘要 + 可选正文
    Tool {
        name: String,
        summary: String,
        status: Status,
        body: Option<String>,
        /// 手动 `!cmd`（用户自己执行的）：**不吃简洁模式**——输出本身就是要看的东西
        manual: bool,
    },
    /// 系统提示（回合中不可用的命令、粘贴结果…）
    Notice(String),
    /// 重试进度（**就地更新**的单个块：`log::progress` 来的，同一个 `key` 只占一行）
    Retry { key: String, text: String },
    Error(String),
}

impl Cell {
    /// 一条助手正文单元格。
    pub fn assistant(text: impl Into<String>) -> Self {
        Cell::Assistant { text: text.into() }
    }

    /// 往助手正文追加增量（没有就新建）。
    pub fn push_assistant_text(cells: &mut Vec<Cell>, delta: &str) {
        match cells.last_mut() {
            Some(Cell::Assistant { text, .. }) => text.push_str(delta),
            _ => cells.push(Cell::assistant(delta)),
        }
    }

    /// 更新最近一个还在「运行中」的工具单元格（工具结果是成对回来的）。
    pub fn finish_tool(cells: &mut [Cell], name: &str, outcome: Status, body: &str) {
        for cell in cells.iter_mut().rev() {
            if let Cell::Tool {
                name: n,
                status,
                body: b,
                ..
            } = cell
            {
                if n == name && *status == Status::Running {
                    *status = outcome;
                    *b = Some(body.trim().to_string());
                    return;
                }
            }
        }
    }

    /// 取消收尾：还挂在「运行中」的工具行结算成 `⏹`——没轮到的 `tool_calls` 不会再有结果事件。
    pub fn cancel_running(cells: &mut [Cell]) {
        for cell in cells.iter_mut() {
            if let Cell::Tool { status, .. } = cell {
                if *status == Status::Running {
                    *status = Status::Cancelled;
                }
            }
        }
    }

    /// 历史回放（resume）：把会话消息转成消息流单元格。
    ///
    /// 输入应当是 `Session::full_history()`——压缩指针已展开；直接给 `messages` 也能跑，只是压缩过的
    /// 回合只剩摘要 + 指针。规则与实时渲染对齐：
    ///
    ///   - `system`（system prompt / 窗口摘要）**不显示**；
    ///   - user → `› ` 一行（`synthetic` 的注入消息跳过：那是图片，不是用户输入）；
    ///   - assistant → markdown 正文（空正文不占位），带 `tool_calls` 的再各自起一条工具行；
    ///     带 `thought_ms` 的先补一行 `• Thought for …`（实时视图现算的那行，靠它落盘才能还原）；
    ///     正文以 `[请求失败]` 开头的（`session::ERROR_TURN_PREFIX`）走 `Cell::Error`（红 `✗`），
    ///     否则回放时失败回合看起来像一次正常回答；
    ///   - tool → 按 `tool_call_id` 合并进**配对的那条**工具行（同一批可能有同名工具，按名字回溯会错配），
    ///     配上工具名、参数摘要与最终状态。
    pub fn from_messages(messages: &[Message]) -> Vec<Cell> {
        let mut cells: Vec<Cell> = Vec::new();
        // tool_call_id → 那条工具行在 cells 里的下标（工具结果消息只带 id，名字/参数在调用里）
        let mut pending: HashMap<String, usize> = HashMap::new();
        for m in messages {
            match m.role.as_str() {
                "system" => {}
                "user" => {
                    if m.synthetic {
                        continue;
                    }
                    let text = m.content_text();
                    if !text.trim().is_empty() {
                        cells.push(Cell::User(text));
                    }
                }
                "assistant" => {
                    // 实时视图里「思考」是 TUI 现算的一行；回放靠会话里的 `thought_ms` 还原
                    if let Some(ms) = m.thought_ms {
                        cells.push(Cell::Thought(Duration::from_millis(ms)));
                    }
                    let text = m.content_text();
                    if !text.trim().is_empty() {
                        // 请求失败的回合（`[请求失败] …`）在实时视图里是红色的 `✗` 行，不是普通正文
                        if text.starts_with(crate::session::ERROR_TURN_PREFIX) {
                            cells.push(Cell::Error(text.trim().to_string()));
                        } else {
                            cells.push(Cell::assistant(text));
                        }
                    }
                    for call in m.tool_calls.iter().flatten() {
                        let arguments = call.function.arguments.clone();
                        pending.insert(call.id.clone(), cells.len());
                        cells.push(Cell::Tool {
                            name: call.function.name.clone(),
                            summary: tool_summary(&arguments),
                            status: Status::Running,
                            body: None,
                            manual: false,
                        });
                    }
                }
                "tool" => {
                    let content = m.content_text();
                    let status = tool_status(&content);
                    let body = Some(content.trim().to_string());
                    match pending.remove(&m.tool_call_id.clone().unwrap_or_default()) {
                        Some(idx) => {
                            if let Cell::Tool {
                                status: s, body: b, ..
                            } = &mut cells[idx]
                            {
                                *s = status;
                                *b = body;
                            }
                        }
                        // 配不上（老会话没写 id / 调用那条被压缩干掉）：至少把结果行显示出来
                        None => cells.push(Cell::Tool {
                            name: m.tool_name.clone().unwrap_or_else(|| "工具".into()),
                            summary: String::new(),
                            status,
                            body,
                            manual: false,
                        }),
                    }
                }
                _ => {}
            }
        }
        cells
    }

}

/// 把 `duration` 格式化成人读的思考耗时（`3.4s` / `1m03s`）。
pub fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let m = d.as_secs() / 60;
        let s = d.as_secs() % 60;
        format!("{m}m{s:02}s")
    }
}

/// 工具调用的一行摘要：从参数 JSON 里抠出最有信息量的那个字段（`read`/`writ`/`edit` 取
/// `path`、`bash` 取 `command`、`repl` 取 `code`）。
///
/// 只有这三个键可达 —— 内置工具的字符串参数就 `path` / `command` / `code` / `content` /
/// `edits`（`content` 是正文、`edits` 是数组，都不适合当摘要），没有哪些工具用 `file_path` /
/// `pattern` / `query` 这类键。以后加新工具再往这里补它的键。
pub fn tool_summary(arguments: &str) -> String {
    const KEYS: [&str; 3] = ["path", "command", "code"];
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) {
        for key in KEYS {
            if let Some(v) = value.get(key).and_then(|v| v.as_str()) {
                return one_line(v, 240);
            }
        }
    }
    one_line(arguments, 240)
}

/// 工具结果的状态：取消哨兵文本 → `⏹`，否则看**第一行**的 `[exit=N]` 头（非 shell 工具没有
/// 退出码 → 成功）。
///
/// 现在 `bash` **只在非 0 时**给这行头（一行里带 `exit` / `os` / `shell` 三个字段，2026-09-24 起），
/// 所以「没头 = 成功」；判据只用到前缀，**不解析退出码**。`[exit=0]` 只可能来自旧会话
/// （改之前录的），所以还得认。取消哨兵比对的是**整段等于** `CANCEL_TEXT`——手动 `!cmd` 那条路
/// 是 `app` 显式给 `Status::Cancelled`（内容带 `[exit=cancelled]` 头，不走这个比对）。
pub fn tool_status(content: &str) -> Status {
    if content == CANCEL_TEXT {
        return Status::Cancelled;
    }
    match content.lines().next().unwrap_or("") {
        line if line.starts_with("[exit=") => {
            if line.starts_with("[exit=0]") {
                Status::Ok
            } else {
                Status::Fail
            }
        }
        line if line.starts_with("[工具错误]") => Status::Fail,
        _ => Status::Ok,
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flattened: String = text
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    if flattened.chars().count() > max {
        format!("{}…", flattened.chars().take(max).collect::<String>())
    } else {
        flattened
    }
}

/// 一条显示行：渲染用的 `Line` + 「是不是软换行的续行」+ 行首装饰格数。
pub struct Row {
    pub line: Line<'static>,
    /// 这一行是**上一条显示行的软换行续行**（同一个逻辑行折出来的）：复制时直接拼接
    /// （源文本本来就连续），不同逻辑行之间才换行。
    ///
    /// 自描述：cell 各自产出自己的行也成立（不必再靠「跨 cell 唯一的逻辑行号」表达续行）。
    pub continues: bool,
    /// 行首有多少格是**装饰**（用户消息的 `› ` / `  ` 前缀）→ 复制时跳过，不进内容。
    pub indent: u16,
}

/// 消息流面板的**跨帧状态**：上一帧的显示行 + 每条助手 cell 的 markdown 渲染缓存。
///
/// 消息流面板的**跨帧状态**：每一条 cell 的排版结果（显示行 + 指纹 + 它自己的 markdown 缓存）。
///
/// 排版旋钮（`width` / `lean` / `palette`）**不住这儿**——它们在 `frame_key` 里（用来判失效），
/// 并按参数传给 `CellBlock::rebuild`；`Cell` 是纯数据，所以 `layout` 只读 `&[Cell]`。
/// 反过来说：块必须**跨帧存活**（每帧现造的临时对象里放缓存，命中率会是 0）。
#[derive(Default)]
pub struct Pane {
    /// 上一帧的 (宽度, 简洁模式, 配色) 指纹：变了 → **每一条** cell 都要重排
    frame_key: u64,
    /// 上一次 `layout` 里**真正重排**的 cell 条数（只有用例关心：证明「没变的块被跳过」）
    #[cfg(test)]
    relaid: usize,
    /// **与 cell 下标一一对应**的排版结果（per-cell 缓存：不变的那条整份复用）
    blocks: Vec<CellBlock>,
}

/// 一条 cell 的排版结果：显示行 + 指纹 + **它自己的** markdown 渲染缓存。
///
/// 三样捆一起（而不是 `rows` / `keys` / `md` 三份并行数组）是为了让「第 i 条」不可能错位；
/// 写入方法也挂在这儿——拿到 `&mut CellBlock` 就**物理上只能写这一条**，不需要靠约定。
#[derive(Default)]
struct CellBlock {
    /// 这条 cell 的显示行（末尾含它后面那条空行）
    rows: Vec<Row>,
    /// 生成上面这份 rows 时的 `cell_key`；与当前指纹不同 ⇒ 需要重排这一条
    key: u64,
    /// 这条 cell 的 markdown 渲染缓存（内容或宽度变了就重解析）
    md: MarkdownCache,
}

impl CellBlock {
    /// 这条 cell 的指纹变了吗（变了 ⇒ 这一块得重建）。
    fn is_stale(&self, cell: &Cell) -> bool {
        self.key != cell_key(cell)
    }

    /// 按这条 cell **重建**这一块：清空旧行 → 逐行重折（折进 `self`）→ 更新指纹 → 补尾行。
    ///
    /// 这四步以前散在 `Pane::layout` 的循环里；收进来之后 `layout` 只负责「判要不要重建」。
    fn rebuild(&mut self, cell: &Cell, palette: &Palette, width: usize, lean: bool) {
        self.rows.clear(); // 复用这一条的容量
        self.key = cell_key(cell);
        self.render(cell, palette, width, lean);
        self.end_cell();
    }

    /// 把**这条 cell** 按本帧的旋钮（配色 / 宽度 / 简洁模式）写进 `self`。
    ///
    /// 只回答「这条消息长什么样」——折行与 `indent` / `continues` 的记账都在 `self` 的
    /// `push_line` 里。`cell` 是只读的（渲染不改消息本身）。
    fn render(&mut self, cell: &Cell, palette: &Palette, width: usize, lean: bool) {
        match cell {
            Cell::User(text) => {
                // `› `（首行）/ `  `（其余逻辑行）是**悬挂缩进**：每条显示行都带（续行由
                // `Self::push_line` 补等宽空白），正文的折行宽度自动少 `PREFIX_CELLS` 格。
                for (i, line) in text.lines().enumerate() {
                    let mark = if i == 0 {
                        Span::styled(
                            "› ",
                            Style::default()
                                .fg(palette.accent)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        Span::raw("  ")
                    };
                    self.push_line(
                        Line::from(Span::styled(line.to_string(), palette.style_user())),
                        PREFIX_CELLS,
                        width,
                        Some(mark),
                    );
                }
            }
            Cell::Assistant { text } => {
                for line in self.markdown_lines(text, width) {
                    self.push_line(line, 0, width, None);
                }
            }
            Cell::Thought(d) => self.push_line(
                Line::from(Span::styled(
                    format!("• Thought for {}", fmt_duration(*d)),
                    palette.style_faint(),
                )),
                0,
                width,
                None,
            ),
            Cell::Tool {
                name,
                summary,
                status,
                body,
                manual,
            } => {
                let (mark, color) = palette.mark(*status);
                self.push_line(
                    Line::from(vec![
                        Span::styled(format!("{mark} "), Style::default().fg(color)),
                        Span::styled(name.clone(), palette.style_tool()),
                        Span::styled(format!("({summary})"), palette.style_muted()),
                    ]),
                    0,
                    width,
                    None,
                );
                if let Some(body) = body {
                    // 简洁模式：成功的工具结果只留一行（正文丢弃）；失败/取消、以及手动 `!cmd`
                    // （用户主动执行）才展示。
                    if *manual || !lean || *status != Status::Ok {
                        let total = body.lines().count();
                        for line in body.lines().take(TOOL_BODY_LINES) {
                            // 正文块统一缩进两格（不再是行首装饰，而是内容的一部分）
                            self.push_line(
                                Line::from(Span::styled(
                                    format!("  {line}"),
                                    palette.style_muted(),
                                )),
                                0,
                                width,
                                None,
                            );
                        }
                        if total > TOOL_BODY_LINES {
                            self.push_line(
                                Line::from(Span::styled(
                                    format!("    …（已省略 {} 行）", total - TOOL_BODY_LINES),
                                    palette.style_faint(),
                                )),
                                0,
                                width,
                                None,
                            );
                        }
                    }
                }
            }
            Cell::Notice(text) => {
                // 多行提示（`/help` 文案、`/status` 报告）要真的分行——富文本里的 `\n` 不是换行
                for (i, line) in text.lines().enumerate() {
                    let prefix = if i == 0 { "· " } else { "  " };
                    self.push_line(
                        Line::from(Span::styled(
                            format!("{prefix}{line}"),
                            palette.style_muted(),
                        )),
                        0,
                        width,
                        None,
                    );
                }
            }
            Cell::Retry { text, .. } => {
                // 重试进度：单个块，每次重试就地改写（不再一行一条）
                for (i, line) in text.lines().enumerate() {
                    let prefix = if i == 0 { "⟳ " } else { "  " };
                    self.push_line(
                        Line::from(Span::styled(
                            format!("{prefix}{line}"),
                            palette.style_faint(),
                        )),
                        0,
                        width,
                        None,
                    );
                }
            }
            Cell::Error(text) => {
                for (i, line) in text.lines().enumerate() {
                    let prefix = if i == 0 { "✗ " } else { "  " };
                    self.push_line(
                        Line::from(Span::styled(
                            format!("{prefix}{line}"),
                            palette.style_error(),
                        )),
                        0,
                        width,
                        None,
                    );
                }
            }
        }
    }

    /// 这条 cell 的 markdown 渲染结果（结果按内容 + 宽度缓存）。
    ///
    /// **拷出**行来返回（不借 `self`）：调用方紧接着要 `push_line(&mut self, …)`，借用得先结束。
    /// 每条 `Line` 本来也要 clone 一次（进 `Row`），所以没多花钱。
    fn markdown_lines(&mut self, src: &str, width: usize) -> Vec<Line<'static>> {
        self.md.get(src, width).lines.clone()
    }

    /// 一条**逻辑行** → 若干显示行（cell 一律走这里：折行只有这一份出口）。
    ///
    /// `indent` = 行首装饰格数（占屏幕宽度、复制时跳过）；`mark` = 首行的装饰文本，续行与其余
    /// 逻辑行补等宽的空白（否则折行后悬挂缩进会丢）。`width` = 消息流整宽，折行按 `width - indent`
    /// （悬挂缩进占的格数从这里扣）。折行**不丢字符**：断点空白留在上一行行尾，所以同一逻辑行的
    /// 相邻显示行拼起来就是原文。
    fn push_line(
        &mut self,
        line: Line<'static>,
        indent: u16,
        width: usize,
        mark: Option<Span<'static>>,
    ) {
        let budget = width.saturating_sub(indent as usize).max(1);
        for (i, wrapped) in wrap_line_into(&line, budget).into_iter().enumerate() {
            let line = match (i, mark.as_ref()) {
                (0, Some(m)) if indent > 0 => {
                    let mut spans = vec![m.clone()];
                    spans.extend(wrapped.spans);
                    Line::from(spans).style(wrapped.style)
                }
                (_, Some(_)) if indent > 0 => {
                    let mut spans = vec![Span::raw(" ".repeat(indent as usize))];
                    spans.extend(wrapped.spans);
                    Line::from(spans).style(wrapped.style)
                }
                _ => wrapped,
            };
            self.rows.push(Row {
                line,
                continues: i > 0,
                indent,
            });
        }
    }

    /// 两条消息之间恰好一行空白（流的结构，不属于任何一条 cell）。
    fn end_cell(&mut self) {
        self.rows.push(Row {
            line: Line::default(),
            continues: false,
            indent: 0,
        });
    }
}

impl Pane {
    /// 上一次 `layout` 里真正重排的 cell 条数（用例用：证明「没变的块被跳过」）。
    #[cfg(test)]
    pub(crate) fn relaid(&self) -> usize {
        self.relaid
    }

    /// 显示行总数（滚动夹取 / 滚动条 / `max_scroll` 用）。
    ///
    /// 逐块累加（O(#cell) 次整数加法）——不必维护「运行总和」那份额外不变量。
    pub fn total(&self) -> usize {
        self.blocks.iter().map(|b| b.rows.len()).sum()
    }

    /// 按显示行顺序迭代（`render` 取行用）。
    pub fn iter(&self) -> impl Iterator<Item = &Row> {
        self.blocks.iter().flat_map(|b| b.rows.iter())
    }

    /// 重排消息流（`width` = 消息流文本区的宽度）。
    ///
    /// **增量**：`(宽度, 简洁模式, 配色)` 没变、且某条 cell 的指纹没变，那一条的显示行整份复用
    /// （折行是这里最贵的一步）；变的那些才重折。这就是 per-cell 缓存——流式只动尾部那一条。
    ///
    /// `lean`（简洁模式，来自 `[tui] lean`）：**成功的工具结果不留正文**——一行说清就够；
    /// 失败/取消才在下方跟正文块（统一缩进 2 空格）。盒子式全展开（正文一律截断到前
    /// `TOOL_BODY_LINES` 行，末尾一行省略提示）。
    ///
    /// 例外：手动 `!cmd`（`Cell::Tool { manual: true }`）**任何模式都展开**——那是用户主动
    /// 执行的命令，输出本身就是要看的东西（`!cmd` 的两个盒子都 `lean=False`）。
    pub fn layout(&mut self, cells: &[Cell], palette: &Palette, width: u16, lean: bool) {
        let width = (width as usize).max(1);
        // 一帧的输入指纹里「与 cells 无关」的那半：宽度 / 简洁模式 / 配色
        let frame_key = {
            let mut h = DefaultHasher::new();
            (width, lean).hash(&mut h);
            palette.hash(&mut h);
            h.finish()
        };
        let all = frame_key != self.frame_key; // 宽度 / 简洁模式 / 配色变了 → 全部重排
        self.frame_key = frame_key;
        #[cfg(test)]
        {
            self.relaid = 0;
        }
        self.blocks.resize_with(cells.len(), CellBlock::default); // `/clear` 后多余的块在这里被丢掉

        for (i, cell) in cells.iter().enumerate() {
            if !all && !self.blocks[i].is_stale(cell) {
                continue; // ← 这一条一字未动：显示行整份复用
            }
            #[cfg(test)]
            {
                self.relaid += 1;
            }
            self.blocks[i].rebuild(cell, palette, width, lean);
        }
    }

    /// 选区文本：显示行区间 `[r1, r2]`、单元格列区间 `[c1, c2]`（**含两端**——
    /// 光标压住的那个单元格也算进来，与终端选择一致）。
    ///
    /// 折行自己算（而非交给 `Paragraph::wrap`）只为了一件事：知道每个显示行对应源文本的哪一段
    /// —— 软换行的相邻显示行（[`Row::continues`]）复制时拼成一行、不产生换行；行首装饰按
    /// [`Row::indent`] 跳过。折点处**不丢字符**，所以同一个逻辑行的相邻显示行拼起来就是原文。
    pub fn slice_text(&self, r1: usize, c1: usize, r2: usize, c2: usize) -> String {
        let mut out = String::new();
        let mut seen = false; // 已经处理过至少一行（第一行之前不插换行）
        let mut base = 0usize; // 当前块的第一行对应的绝对行号
        for block in &self.blocks {
            let end = base + block.rows.len();
            if end <= r1 {
                base = end; // 整块都在选区之上 → 跳过（不用逐行看）
                continue;
            }
            if base > r2 {
                break;
            }
            for (k, row) in block.rows.iter().enumerate() {
                let r = base + k;
                if r < r1 {
                    continue;
                }
                if r > r2 {
                    return out;
                }
                // 逐行规则：按「行是不是选区首/末行」决定切到哪一列；行首装饰（`Row::indent`）
                // 不进内容；软换行的续行（`Row::continues`）**不插换行**（粘回去就是原文）。
                let end_col = c2.saturating_add(1); // 含光标所在格 → 切到它右边
                let (from, to) = if r1 == r2 {
                    (c1, end_col)
                } else if r == r1 {
                    (c1, usize::MAX)
                } else if r == r2 {
                    (0, end_col)
                } else {
                    (0, usize::MAX)
                };
                let piece = crop_cells(&row_text(row), from.max(row.indent as usize), to);
                if !row.continues && seen {
                    out.push('\n');
                }
                out.push_str(&piece);
                seen = true;
            }
            base = end;
        }
        out
    }
}

/// 一条 cell 的排版指纹（**只看影响它自己显示行的东西**，O(1)）。
///
/// 依据与 `markdown::fingerprint` 同一套路：本仓对 cell 的改动都会动到这几样之一——
///   · 流式追加正文 → 尾部 text 的 len / 尾部字节变；
///   · 工具结算（`finish_tool`）/ 取消 → `status` 必然从 `Running` 变掉；
///   · 新消息 / `/clear` / `retain` → 由 `blocks.resize_with` 与 `cells.len()` 兜住。
fn cell_key(cell: &Cell) -> u64 {
    fn text_sig(s: &str, h: &mut DefaultHasher) {
        s.len().hash(h);
        // ⚠ 必须落在字符边界上（中文按字节切会 panic）——用 markdown 里那一个实现
        s[..floor_char_boundary(s, 64)].hash(h);
        s[floor_char_boundary(s, s.len().saturating_sub(64))..].hash(h);
    }
    let mut h = DefaultHasher::new();
    match cell {
        Cell::User(t) | Cell::Notice(t) | Cell::Error(t) => text_sig(t, &mut h),
        Cell::Retry { text, .. } => text_sig(text, &mut h),
        Cell::Assistant { text } => text_sig(text, &mut h),
        Cell::Thought(d) => d.hash(&mut h),
        Cell::Tool {
            summary,
            status,
            body,
            manual,
            ..
        } => {
            (summary.len(), *status, *manual).hash(&mut h);
            body.as_ref().map_or(0, String::len).hash(&mut h);
        }
    }
    h.finish()
}


/// 一条显示行的纯文本（复制用）。
fn row_text(row: &Row) -> String {
    row.line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

/// 按**单元格**区间 `[from, to)` 裁文本：宽字符占两格，只有**起点**落在区间里就整个要
/// （不会把汉字 / emoji 劈成两半）。`to = usize::MAX` = 一直到行尾。
fn crop_cells(text: &str, from: usize, to: usize) -> String {
    let mut out = String::new();
    let mut cell = 0usize;
    for c in text.chars() {
        let start = cell;
        cell += char_width(c);
        if start >= from && start < to {
            out.push(c);
        }
    }
    out
}

/// 字符占几个单元格（控制符算 0）：ratatui 的渲染口径与输入框控件内部都是这把尺子。
pub(crate) fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// 消息流的折行档位（`Glyph` = 逐字硬断；`WordOrGlyph` = 词级断 + 超长词退回逐字断）。
///
/// 规则**对齐 `ratatui-textarea` 的 `WrapMode`**（那份的 `wrap_word_chunks`）：词级档按 UAX#29
/// 词边界切块再装块，块自己一个字符不切；只有「单个块就比整行宽」（超长英文词 / 长 URL）时才退回
/// 逐字硬断。共同点：**任何显示行都不超过宽度**（超了消息流的 `Paragraph` 会二次折行，`Row` ↔
/// 源文本的映射就错了）。
///
/// 曾经考虑过、**不打算收**的档位与原因：
///   - 不软换行（水平滚动）：另一套几何（`Row` ↔ 源文本的映射、滚动偏移、复制切片都不成立）；
///   - `Word`（只在词边界断、**不**做超长词回退）：比整行宽的块会原样留着 → 行超宽 → 二次折行。
///
/// ⚠ 与控件的一处细节差异：控件的逐字断走**字素簇**（`grapheme_indices`），pie 这边走**字符**
/// （`chars()`）——emoji 组合序列 / 组合记号在控件那边不会被劈开，在这儿可能被劈。要完全对齐得
/// 给 `grapheme_indices` 也留一条路（目前不值当）。
#[allow(dead_code)] // 生产只走 `WordOrGlyph`；`Glyph` 留着「换档位」与用例（改调用点那个字面量即可）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WrapMode {
    /// 逐字硬断：CJK 逐字天然友好（长句能填满整行），代价是英文词 / URL / 路径会从中间切开。
    Glyph,
    /// 词级断行 + 超长词按字硬断（英文词不断、中日韩字各自成块）——与 `ratatui-textarea` 同规则。
    WordOrGlyph,
}

/// **消息流**的折行：把 `text` 按显示宽度折成若干段，返回每段的 `(起始字符下标, 字符数)`。
///
/// `mode` 由调用点给（`CellBlock::push_line` 走 [`WrapMode::WordOrGlyph`]）。宽度用
/// `UnicodeWidthChar`（与 ratatui 内部同一把尺子；制表符计 0，与 `Paragraph` 的渲染口径一致）；
/// 每行至少放一个字符（免得死循环）。空行也占一行。
///
/// 输入框**不共用这个函数**（它固定逐字断、自己复刻控件那份，见 `input::screen_rows`）——所以
/// 两边**可能折在不同位置**（那是有意为之：输入框是"普通编辑器"）。
fn wrap_segments(text: &str, width: usize, mode: WrapMode) -> Vec<(usize, usize)> {
    /// 单个块自己就比整行宽：按字硬断（每行至少放一个字符，免得死循环）。
    fn hard_split(chars: &[char], width: usize, base: usize, out: &mut Vec<(usize, usize)>) {
        let mut start = 0usize;
        while start < chars.len() {
            let mut end = start;
            let mut used = 0usize;
            while end < chars.len() {
                let w = char_width(chars[end]);
                if used + w > width && end > start {
                    break;
                }
                used += w;
                end += 1;
            }
            out.push((base + start, end - start));
            start = end;
        }
    }

    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![(0, 0)]; // 空行也占一行
    }
    match mode {
        // 逐字硬断：整行就是一块
        WrapMode::Glyph => {
            let mut out = Vec::new();
            hard_split(&chars, width, 0, &mut out);
            out
        }
        // 词级：先按 UAX#29 词边界切成块 `(起始字符下标, 结束字符下标, 显示宽度)`，再贪心装块
        WrapMode::WordOrGlyph => {
            let mut chunks: Vec<(usize, usize, usize)> = Vec::new();
            let mut at = 0usize;
            for (_, chunk) in text.split_word_bound_indices() {
                let len = chunk.chars().count();
                chunks.push((at, at + len, chunk.chars().map(char_width).sum()));
                at += len;
            }
            let mut out: Vec<(usize, usize)> = Vec::new();
            let mut i = 0usize;
            let mut start = chunks[0].0;
            let mut end = start;
            let mut used = 0usize;
            while i < chunks.len() {
                let (chunk_start, chunk_end, chunk_width) = chunks[i];
                if end == start {
                    start = chunk_start;
                }
                if used + chunk_width <= width {
                    end = chunk_end;
                    used += chunk_width;
                    i += 1;
                    continue;
                }
                if end > start {
                    // 这一行装不下了：断在块边界上（块自己一个字符不切）
                    out.push((start, end - start));
                    start = end;
                    used = 0;
                    continue;
                }
                // 单个块自己就比整行宽（超长英文词 / 长 URL）→ 按字硬断
                hard_split(&chars[chunk_start..chunk_end], width, chunk_start, &mut out);
                i += 1;
                start = chunk_end;
                end = chunk_end;
                used = 0;
            }
            if end > start {
                out.push((start, end - start));
            }
            out
        }
    }
}

/// 把一条逻辑行按显示宽度折成若干条显示行（只有 [`CellBlock::push_line`] 用它：消息流自己折行）。
///
/// 断行规则见 [`wrap_segments`]（当前那档：逐字硬断、CJK 友好；断点处**不丢字符**）。
/// 空行也占一行（消息之间的空行就是这么来的）。
fn wrap_line_into(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    // 逐字符带样式展开：spans 会被折行切开，样式得跟着字符走
    let chars: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(|c| (c, s.style)))
        .collect();
    if chars.is_empty() {
        return vec![Line::default()];
    }
    let text: String = chars.iter().map(|(c, _)| *c).collect();
    wrap_segments(&text, width, WrapMode::WordOrGlyph)
        .into_iter()
        .map(|(start, len)| {
            // 相邻同样式的字符合成一个 span（折行会把原来的 span 切成两半），行级样式照搬
            let spans: Vec<Span<'static>> = chars[start..start + len]
                .chunk_by(|a, b| a.1 == b.1)
                .map(|run| Span::styled(run.iter().map(|(c, _)| *c).collect::<String>(), run[0].1))
                .collect();
            Line::from(spans).style(line.style)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Content, FunctionCall, ToolCall};

    /// 排版结果的纯文本（显示行按行拼；行内 spans 直接连起来）。
    fn plain<'a>(rows: impl IntoIterator<Item = &'a Row>) -> String {
        rows.into_iter().map(row_text).collect::<Vec<_>>().join("\n")
    }

    /// 排版一帧（`Pane` 是跨帧状态：这里每次新建，只当「跑一遍」用）。
    fn rendered(cells: &[Cell], palette: &Palette, width: u16, lean: bool) -> Pane {
        let mut pane = Pane::default();
        pane.layout(cells, palette, width, lean);
        pane
    }

    /// 排版（默认深色色板、盒子式）。
    fn laid_out(cells: &[Cell], width: u16) -> Pane {
        rendered(cells, &Palette::mocha(), width, false)
    }

    /// 拍平成一个扁平视图（用例断言用；`Pane` 里现在是 per-cell 的块）。
    fn flat(pane: &Pane) -> Vec<&Row> {
        pane.iter().collect()
    }

    /// [`wrap_segments`] 是**消息流**自己的断行实现，两档都在这儿钉住（规则对齐 `ratatui-textarea`）。
    ///
    /// 共同的不变量：任何显示行都不超宽、不丢字符、不产生空段。差别只在断点位置——
    /// 词级档英文词整块（除非它自己比整行还宽），逐字档会把词从中间切开。
    #[test]
    fn wrap_segments_pins_both_modes() {
        let word = |text: &str, width: usize| wrap_segments(text, width, WrapMode::WordOrGlyph);
        let glyph = |text: &str, width: usize| wrap_segments(text, width, WrapMode::Glyph);

        // —— 词级档：ASCII 词整块不切开（哪怕就差一格）→ 断在不下的那个词之前
        assert_eq!(word("alpha beta", 9), vec![(0, 6), (6, 4)]);
        // 中文逐字可断（汉字本来就各自成块 → 长句能填满整行）
        assert_eq!(word("中文中文", 4), vec![(0, 2), (2, 2)]);
        // 中英混排：按块装；空白块**装得下就留在上一行行尾**，否则挤到下一行
        assert_eq!(
            word("中文 ab 中文", 6),
            vec![(0, 3), (3, 4), (7, 1)],
            "`中文 ` / `ab 中` / `文`"
        );
        assert_eq!(
            word("中文 ab 中文", 4),
            vec![(0, 2), (2, 4), (6, 2)],
            "`中文` / ` ab ` / `中文`（空白挤到下一行）"
        );
        // 单个块自己就比整行宽（超长英文词 / 长 URL）→ 退回按字硬断，每行至少一个字符
        assert_eq!(word("abcdefghij", 4), vec![(0, 4), (4, 4), (8, 2)]);
        assert_eq!(word("中", 1), vec![(0, 1)], "宽字放不下也得放一个");
        // 空行也占一行；宽 0 当 1 用
        assert_eq!(word("", 5), vec![(0, 0)]);
        assert_eq!(word("ab", 0), vec![(0, 1), (1, 1)]);

        // —— 逐字档：英文词也会从中间切开（`alpha bet` / `a`）
        assert_eq!(glyph("alpha beta", 9), vec![(0, 9), (9, 1)]);
        assert_eq!(glyph("中文中文", 4), vec![(0, 2), (2, 2)], "CJK 两档结果一样");
        assert_eq!(glyph("中文 ab 中文", 6), vec![(0, 4), (4, 4)]);

        // —— 两档共同的不变量：拼起来就是原文（不丢不重）、不超宽、无空段
        let text = "中文混排 english words 一起折行测试 /tmp/一个超级长的单词withoutspaces还有中文 https://example.com/a/very/long/url";
        let chars: Vec<char> = text.chars().collect();
        for mode in [WrapMode::Glyph, WrapMode::WordOrGlyph] {
            for width in 1..40 {
                let segs = wrap_segments(text, width, mode);
                let joined: String = segs
                    .iter()
                    .flat_map(|(start, len)| chars[*start..start + len].iter())
                    .collect();
                assert_eq!(joined, text, "{mode:?} 宽 {width}：折行丢了/重了字符");
                for (start, len) in &segs {
                    assert!(*len > 0, "{mode:?} 宽 {width}：不能有空段");
                    let w: usize = chars[*start..start + len].iter().map(|c| char_width(*c)).sum();
                    // 宽字放不下也得放一个（`max(2)` 就是这个余量）
                    assert!(w <= width.max(2), "{mode:?} 宽 {width}：段超宽（{w}）");
                }
            }
        }
    }

    #[test]
    fn wrap_never_exceeds_the_width_and_keeps_every_char() {
        // 中英混排 + 超长单词：每一行都不能超宽，拼回来（跳过行首装饰）必须是原文
        let samples = [
            "alpha beta gamma delta epsilon zeta",
            "中文混排 english words 一起折行的测试",
            "一个超级长的单词withoutanyspacesatall还有中文",
        ];
        for text in samples {
            let cells = vec![Cell::User(text.into())];
            let l = laid_out(&cells, 16);
            let rows = flat(&l);
            assert!(rows.len() > 1, "{text} 在宽 16 下应该折行");
            for row in &rows {
                assert!(row.line.width() <= 16, "超宽：{:?}", row_text(row));
            }
            // 首行 `› `、续行 `  `（悬挂缩进）：每条显示行都占 2 格装饰
            assert!(row_text(rows[0]).starts_with("› "));
            assert!(row_text(rows[1]).starts_with("  "));
            // 复制（装饰不进内容）= 原文，一个字符不丢
            let last = rows.len() - 1;
            let joined = l.slice_text(0, 0, last, 15);
            assert_eq!(joined.trim_end(), text, "折行不能丢字符");
        }
    }

    /// 折行会把 span 切开：**每个字的样式得跟着自己走**，行级样式照搬（以前由 `rebuild_line`
    /// 负责，现在就地合并在 `wrap_line_into` 里——顺手补上这条以前没人钉的用例）。
    #[test]
    fn wrapping_keeps_per_char_styles_and_the_line_style() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let italic = Style::default().add_modifier(Modifier::ITALIC);
        let line_style = Style::default().add_modifier(Modifier::UNDERLINED);
        let line = Line::from(vec![
            Span::styled("aaa", bold),
            Span::styled("bbb", italic),
        ])
        .style(line_style);

        // 宽 2、无空格 → 按字硬断：`aa` / `ab` / `bb`
        let rows = wrap_line_into(&line, 2);
        let texts: Vec<String> = rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(texts, vec!["aa", "ab", "bb"]);
        // 第二行被切成两个 span：粗体的 `a` + 斜体的 `b`
        assert_eq!(rows[1].spans.len(), 2, "跨样式的断点要分成两个 span");
        assert_eq!(rows[1].spans[0].style, bold);
        assert_eq!(rows[1].spans[1].style, italic);
        // 每条显示行照搬行级样式
        assert!(rows.iter().all(|l| l.style == line_style));
        // 空行也占一行
        assert_eq!(wrap_line_into(&Line::default(), 10), vec![Line::default()]);
    }

    #[test]
    fn copy_of_a_soft_wrapped_line_stays_one_line() {
        let cells = vec![Cell::User("alpha beta gamma delta epsilon".into())];
        let l = laid_out(&cells, 20);
        let last = flat(&l).len() - 1;
        assert!(last > 0, "先得真的折了行");
        // 全选（末尾列给够）：同一条逻辑行折出来的显示行要拼回一行
        let text = l.slice_text(0, 0, last, 19).trim_end().to_string();
        assert_eq!(text, "alpha beta gamma delta epsilon");
        assert!(!text.contains('\n'), "软换行不算换行：{text:?}");
    }

    #[test]
    fn copy_breaks_between_logical_lines() {
        let cells = vec![Cell::User("第一行".into()), Cell::Notice("第二行".into())];
        let l = laid_out(&cells, 40);
        let last = flat(&l).len() - 1;
        let text = l.slice_text(0, 0, last, 39);
        assert!(text.contains("第一行\n"), "不同逻辑行之间才换行：{text:?}");
        assert!(text.contains("第二行"), "{text:?}");
    }

    #[test]
    fn selection_crops_by_cells_without_splitting_wide_chars() {
        let cells = vec![Cell::User("中文测试".into())];
        let l = laid_out(&cells, 80);
        assert_eq!(row_text(flat(&l)[0]), "› 中文测试", "一行放得下");
        // “› ” 占 2 格 → “中文” 在格 2..6
        assert_eq!(l.slice_text(0, 2, 0, 5), "中文");
        // 只选到“中”的右半格，整个字也要（不能劈成半个）
        assert_eq!(l.slice_text(0, 2, 0, 2), "中");
        // 起点落在“中”的右半格（第 3 格）→ 从**下一个**字开始
        assert_eq!(l.slice_text(0, 3, 0, 6), "文测");
        // 末尾列给大也只会取到行尾（`› ` 装饰不进内容）
        assert_eq!(l.slice_text(0, 0, 0, 999), "中文测试");
    }

    #[test]
    fn history_replay_pairs_calls_with_results() {
        let palette = Palette::mocha();
        let call = |id: &str, name: &str, args: &str| ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        };
        let assistant = Message {
            role: "assistant".into(),
            tool_calls: Some(vec![
                call("c1", "read", r#"{"path":"a.rs"}"#),
                call("c2", "bash", r#"{"command":"false"}"#),
            ]),
            ..Default::default()
        };
        let mut synthetic = Message::user("[图片]");
        synthetic.synthetic = true;
        let messages = vec![
            Message::system("SENTINEL-SYSTEM"),
            synthetic,
            Message::user("看看 a.rs"),
            assistant,
            Message::tool_result("c1", "read", "fn main() {}\n"),
            Message::tool_result("c2", "bash", "[exit=1]\n\nboom"),
            Message {
                role: "assistant".into(),
                content: Some(Content::Text("看完了".into())),
                ..Default::default()
            },
        ];
        let cells = Cell::from_messages(&messages);
        let pane = rendered(&cells, &palette, 80, false);
        let text = plain(pane.iter());
        assert!(!text.contains("SENTINEL-SYSTEM"), "system 不回放：{text}");
        assert!(!text.contains("[图片]"), "注入的图片消息不是用户输入：{text}");
        assert!(text.contains("› 看看 a.rs"), "{text}");
        assert!(text.contains("✓ read(a.rs)"), "调用与结果要合并：{text}");
        assert!(text.contains("  fn main() {}"), "{text}");
        assert!(text.contains("✗ bash(false)"), "退出码决定状态：{text}");
        assert!(text.contains("  boom"), "{text}");
        assert!(text.contains("看完了"), "最终正文：{text}");
        // 两个调用各自配对（按 `tool_call_id`，不是按工具名回溯）
        assert_eq!(cells.iter().filter(|c| matches!(c, Cell::Tool { .. })).count(), 2);
        assert!(cells.iter().any(|c| matches!(c, Cell::User(_))));
    }

    /// 用例诊断用：cell 的种类名（`Cell` 本身没实现 `Debug`）。
    fn kind_of(cell: &Cell) -> &'static str {
        match cell {
            Cell::User(_) => "user",
            Cell::Assistant { .. } => "assistant",
            Cell::Thought(_) => "thought",
            Cell::Tool { .. } => "tool",
            Cell::Notice(_) => "notice",
            Cell::Retry { .. } => "retry",
            Cell::Error(_) => "error",
        }
    }

    /// 回放要还原**只活在实时视图里**的两样东西（用户报的两个 bug）：
    ///   · `• Thought for …`（TUI 现算的时长，靠 `thought_ms` 落盘才能还原）；
    ///   · 失败回合的红色 `✗`（会话里只是一条内容为 `[请求失败] …` 的 assistant 消息）。
    #[test]
    fn history_replay_restores_thought_line_and_error_turn() {
        let palette = Palette::mocha();
        let messages = vec![
            Message::user("问题一"),
            Message {
                role: "assistant".into(),
                content: Some(Content::Text("回答一".into())),
                reasoning_content: Some("想了很久".into()),
                thought_ms: Some(3400),
                ..Default::default()
            },
            Message::user("继续"),
            Message {
                role: "assistant".into(),
                content: Some(Content::Text(format!(
                    "{}连接失败: timeout",
                    crate::session::ERROR_TURN_PREFIX
                ))),
                ..Default::default()
            },
        ];
        let cells = Cell::from_messages(&messages);
        // 失败回合必须是 Error（红 ✗），不能是普通正文
        assert!(
            cells.iter().any(|c| matches!(c, Cell::Error(t) if t.starts_with("[请求失败]"))),
            "失败回合要回放成 Cell::Error（cells 里的 kinds：{:?}）",
            cells.iter().map(kind_of).collect::<Vec<_>>()
        );
        // 「思考」那行要排在正文之前
        let thought = cells
            .iter()
            .position(|c| matches!(c, Cell::Thought(_)))
            .expect("有 thought_ms 就该有一行 Thought");
        let answer = cells
            .iter()
            .position(|c| matches!(c, Cell::Assistant { .. }))
            .expect("正文");
        assert!(thought < answer, "Thought 行要在正文之前");
        // 没有 thought_ms 的消息不该凭空冒出思考行（这里只有一条带）
        assert_eq!(
            cells.iter().filter(|c| matches!(c, Cell::Thought(_))).count(),
            1
        );

        let pane = rendered(&cells, &palette, 80, false);
        let text = plain(pane.iter());
        assert!(text.contains("• Thought for 3.4s"), "{text}");
        assert!(text.contains("✗ [请求失败] 连接失败: timeout"), "{text}");
        assert!(text.contains("回答一"), "{text}");
    }

    #[test]
    fn history_replay_keeps_unpaired_tool_result_and_cancelled_status() {
        let palette = Palette::mocha();
        let messages = vec![
            // 配对信息缺失（老会话没写 tool_call_id）→ 至少把结果行显示出来
            Message::tool_result("", "bash", "[exit=0]\n\nhi"),
            Message::tool_result("call_x", "bash", crate::cancel::CANCEL_TEXT),
        ];
        let cells = Cell::from_messages(&messages);
        let pane = rendered(&cells, &palette, 80, false);
        let text = plain(pane.iter());
        assert!(text.contains("✓ bash"), "{text}");
        assert!(text.contains("hi"), "{text}");
        assert!(text.contains("⏹"), "取消哨兵 → ⏹：{text}");
    }

    #[test]
    fn renders_user_assistant_tool_and_thought() {
        let palette = Palette::mocha();
        let mut cells = vec![
            Cell::User("你好\n第二行".into()),
            Cell::assistant("前"),
            Cell::Thought(Duration::from_millis(3400)),
            Cell::Tool {
                name: "bash".into(),
                summary: "pwd".into(),
                status: Status::Ok,
                body: Some("/tmp\n".into()),
                manual: false,
            },
        ];
        // 末尾是 Tool → 新建一条助手格；紧接着再来一条则**就地追加**到同一格
        Cell::push_assistant_text(&mut cells, "**在**的");
        Cell::push_assistant_text(&mut cells, "！");
        let pane = rendered(&cells, &palette, 80, false);
        let text = plain(pane.iter());
        assert!(text.contains("› 你好"), "{text}");
        assert!(text.contains("  第二行"), "续行缩进：{text}");
        assert!(text.contains("前"), "{text}");
        assert!(text.contains("在的！"), "增量追加回同一格：{text}");
        assert!(text.contains("• Thought for 3.4s"), "{text}");
        assert!(text.contains("✓ bash(pwd)"), "{text}");
        assert!(text.contains("  /tmp"), "盒子式：正文展示：{text}");
    }

    #[test]
    fn lean_hides_successful_tool_body_but_keeps_failed_one() {
        let palette = Palette::mocha();
        let cells = vec![
            Cell::Tool {
                name: "read".into(),
                summary: "a.rs".into(),
                status: Status::Ok,
                body: Some("fn main() {}".into()),
                manual: false,
            },
            Cell::Tool {
                name: "bash".into(),
                summary: "false".into(),
                status: Status::Fail,
                body: Some("[exit=1]\n\nboom".into()),
                manual: false,
            },
            // 手动 `!cmd`：成功也带正文
            Cell::Tool {
                name: "bash".into(),
                summary: "$ pwd".into(),
                status: Status::Ok,
                body: Some("[exit=0]\n\n/tmp".into()),
                manual: true,
            },
        ];
        let pane = rendered(&cells, &palette, 80, true);
        let text = plain(pane.iter());
        assert!(text.contains("✓ read(a.rs)"), "{text}");
        assert!(!text.contains("fn main()"), "成功不带正文：{text}");
        assert!(text.contains("✗ bash(false)"), "{text}");
        assert!(text.contains("  [exit=1]"), "失败带正文：{text}");
        assert!(
            text.contains("✓ bash($ pwd)") && text.contains("  [exit=0]"),
            "手动 !cmd 不吃简洁模式：{text}"
        );
    }

    #[test]
    fn finish_and_cancel_only_touch_running_matching_cell() {
        let mut cells = vec![Cell::Tool {
            name: "bash".into(),
            summary: "false".into(),
            status: Status::Running,
            body: None,
            manual: false,
        }];
        Cell::finish_tool(&mut cells, "bash", Status::Fail, "[exit=1]\n\nboom");
        match &cells[0] {
            Cell::Tool { status, body, .. } => {
                assert_eq!(*status, Status::Fail);
                assert!(body.as_deref().unwrap().contains("boom"));
            }
            _ => panic!("还是 Tool"),
        }
        // 已经结束的不再被改
        Cell::finish_tool(&mut cells, "bash", Status::Ok, "[exit=0]");
        match &cells[0] {
            Cell::Tool { status, .. } => assert_eq!(*status, Status::Fail),
            _ => panic!("还是 Tool"),
        }

        // 取消收尾：还挂在「运行中」的结算成 ⏹（没跑到的 tool_calls 不会再有结果事件）
        let mut cells = vec![
            Cell::Tool {
                name: "bash".into(),
                summary: "sleep 100".into(),
                status: Status::Running,
                body: None,
                manual: false,
            },
            Cell::Tool {
                name: "read".into(),
                summary: "a.rs".into(),
                status: Status::Ok,
                body: None,
                manual: false,
            },
        ];
        Cell::cancel_running(&mut cells);
        assert!(matches!(
            &cells[0],
            Cell::Tool {
                status: Status::Cancelled,
                ..
            }
        ));
        assert!(
            matches!(&cells[1], Cell::Tool { status: Status::Ok, .. }),
            "已经结束的行不动"
        );
    }

    #[test]
    fn summaries_and_exit_codes() {
        assert_eq!(
            tool_summary(r#"{"command":"seq 1 3","timeout":null}"#),
            "seq 1 3"
        );
        assert_eq!(tool_summary(r#"{"path":"/tmp/a b.png"}"#), "/tmp/a b.png");
        assert_eq!(tool_summary(r#"{"code":"print(1)","timeout":null}"#), "print(1)");
        assert_eq!(tool_summary("不是 JSON"), "不是 JSON");
        // 判成败只看第一行：`[exit=` 开头且不是 `[exit=0]` = 失败；没有头 = 成功
        assert_eq!(tool_status("[exit=0]\n\nhi"), Status::Ok);
        assert_eq!(tool_status("[exit=3]\n\nboom"), Status::Fail);
        // 新格式（2026-09-24 起三字段同挤一行）：只看第一行就能判成败
        assert_eq!(tool_status("[exit=3, os=linux, shell=bash]\n\nboom"), Status::Fail);
        assert_eq!(tool_status("[exit=3, os=linux, shell=bash]"), Status::Fail);
        assert_eq!(tool_status("[工具错误] 文件不存在: x"), Status::Fail);
        assert_eq!(tool_status("plain text"), Status::Ok, "没有退出码就当成功");
        // 取消哨兵是**整段等于** `CANCEL_TEXT`（优先于退出码头）
        assert_eq!(tool_status(CANCEL_TEXT), Status::Cancelled);
        assert_eq!(fmt_duration(Duration::from_millis(63_400)), "1m03s");
    }

    /// 单条 cell 的排版可以直接单测：给它一个 `CellBlock` 就够（不必构造整个 `Pane`）。
    #[test]
    fn a_single_cell_renders_on_its_own() {
        let palette = Palette::mocha();
        let mut b = CellBlock::default();
        b.rebuild(&Cell::User("你好".into()), &palette, 40, false);
        assert_eq!(plain(b.rows.as_slice()).trim_end(), "› 你好");
        assert_eq!(b.rows[0].indent, PREFIX_CELLS);
        assert!(!b.rows[0].continues);
        assert_eq!(b.rows.len(), 2, "`rebuild` 末尾补一条空行（消息之间那条）");

        // 悬挂缩进：续行的等宽空白由 `CellBlock::push_line` 补（cell 只交代 mark）
        b.rebuild(&Cell::User("alpha beta gamma delta".into()), &palette, 14, false);
        assert!(b.rows.len() > 1);
        assert!(plain(b.rows.as_slice())
            .lines()
            .all(|l| l.starts_with("› ") || l.starts_with("  ")));
        let body = &b.rows[..b.rows.len() - 1]; // 末条是 `rebuild` 补的空行，不参与
        for (i, row) in body.iter().enumerate() {
            assert_eq!(row.continues, i > 0);
            assert!(row.line.width() <= 14, "超宽：{:?}", row_text(row));
        }

        // 同样的内容走一整遍 `Pane::layout` → 折行、行号、复制切片都对得上
        let pane = rendered(&[Cell::User("alpha beta gamma delta".into())], &palette, 14, false);
        let rows = flat(&pane);
        assert_eq!(rows.len(), b.rows.len(), "与直接重建这一条完全一致（含尾部空行）");
        // 续行拼回一行（不含装饰）；最后一行是那条空白，所以正文到 `len-2`
        assert_eq!(
            pane.slice_text(0, 0, rows.len() - 2, 13).trim_end(),
            "alpha beta gamma delta"
        );
    }

    /// 增量重排的**核心不变量**：只重排「真的变了」的 cell；宽度 / 简洁模式 / 配色变了才全排。
    #[test]
    fn only_changed_cells_are_relaid_out() {
        let palette = Palette::mocha();
        let mut pane = Pane::default();
        let mut cells = vec![
            Cell::User("第一行".into()),
            Cell::assistant("回答一"),
            Cell::User("第二行".into()),
        ];
        pane.layout(&cells, &palette, 40, false);
        assert_eq!(pane.relaid, cells.len(), "第一帧全排");
        assert_eq!(pane.total(), flat(&pane).len(), "total 就是每块行数之和");

        // 输入没变 → 一条都不排（这就是 tick / 打字 / 滚轮那些帧）
        pane.layout(&cells, &palette, 40, false);
        assert_eq!(pane.relaid, 0, "输入没变：一条都不该重排");

        // 尾部追加（流式）→ 只排尾巴那一条（末尾是 User，所以这一步会新起一条 Assistant 格）
        Cell::push_assistant_text(&mut cells, "（补一句）");
        pane.layout(&cells, &palette, 40, false);
        assert_eq!(pane.relaid, 1, "流式只该重排被追加的那一条");

        // 中间那条改了（工具结算、命令回显这类）→ 也只排它
        cells[1] = Cell::assistant("换个回答");
        pane.layout(&cells, &palette, 40, false);
        assert_eq!(pane.relaid, 1, "中间的 cell 也只排它自己");

        // 宽度 / 简洁模式 / 配色变了 → 全部重排（折行与取色都变了）
        pane.layout(&cells, &palette, 24, false);
        assert_eq!(pane.relaid, cells.len(), "宽度变了要全排");
        pane.layout(&cells, &Palette::latte(), 24, false);
        assert_eq!(pane.relaid, cells.len(), "配色变了要全排");
        pane.layout(&cells, &Palette::latte(), 24, true);
        assert_eq!(pane.relaid, cells.len(), "简洁模式变了要全排");
    }

    /// 增量的结果必须与「从头全量排一遍」逐行一致——包括 cells 被插入/删除导致下标整体移位
    /// 的情况（此时指纹必然不匹配 → 重排，正确性靠这个）。
    #[test]
    fn incremental_layout_matches_a_full_one() {
        let palette = Palette::mocha();
        let mut cells = vec![
            Cell::User("甲".into()),
            Cell::assistant("乙"),
            Cell::User("丙".into()),
        ];
        let mut pane = Pane::default();
        pane.layout(&cells, &palette, 30, false);

        Cell::push_assistant_text(&mut cells, "的补充");
        cells.insert(0, Cell::User("新插到最前面的".into()));
        pane.layout(&cells, &palette, 30, false);

        let mut fresh = Pane::default();
        fresh.layout(&cells, &palette, 30, false);
        assert_eq!(
            plain(pane.iter()),
            plain(fresh.iter()),
            "增量排出来的行必须与全量一致"
        );
        assert_eq!(pane.total(), fresh.total());
    }

    /// 工具状态从「运行中」变掉必须被抓到——漏判的后果是界面一直显示 `•`（旧状态）。
    #[test]
    fn a_tool_status_change_invalidates_that_cell() {
        let palette = Palette::mocha();
        let mut cells = vec![Cell::Tool {
            name: "bash".into(),
            summary: "false".into(),
            status: Status::Running,
            body: None,
            manual: false,
        }];
        let mut pane = Pane::default();
        pane.layout(&cells, &palette, 40, false);
        assert!(plain(pane.iter()).contains('•'), "运行中：•");

        Cell::finish_tool(&mut cells, "bash", Status::Fail, "[exit=1]\n\nboom");
        pane.layout(&cells, &palette, 40, false);
        assert_eq!(pane.relaid, 1, "status 变了必须重排这一条");
        let text = plain(pane.iter());
        assert!(text.contains("✗ bash(false)"), "{text}");
        assert!(text.contains("  [exit=1]"), "{text}");
    }

    /// cells 缩到 0 条（`/clear`）：块、行数、markdown 缓存一起清干净。
    #[test]
    fn clearing_cells_drops_blocks_and_cache() {
        let palette = Palette::mocha();
        let mut pane = Pane::default();
        let cells = vec![Cell::User("问题".into()), Cell::assistant("**答**")];
        pane.layout(&cells, &palette, 40, false);
        assert!(pane.total() > 0 && !pane.blocks.is_empty());

        pane.layout(&[], &palette, 40, false);
        assert_eq!(pane.total(), 0, "没有 cell 就没有行");
        assert!(pane.blocks.is_empty(), "块没了，它带的 markdown 缓存也一并没了");
        assert!(pane.slice_text(0, 0, 5, 5).is_empty(), "空流里切不出东西");
    }

    /// markdown 缓存在 `Pane` 里**按 cell 下标**存：同一批 cells 再排一遍 → 命中；
    /// 内容或宽度变了 → 重算；cells 缩到 0 条（`/clear` 一类）→ 越界项被清掉。
    #[test]
    fn markdown_cache_lives_in_the_block_and_follows_content() {
        let palette = Palette::mocha();
        let mut pane = Pane::default();
        let cells = vec![Cell::assistant("**粗**"), Cell::User("问题".into())];
        pane.layout(&cells, &palette, 80, false);
        assert_eq!(pane.blocks[1].md.key(), 0, "User 那条不用 markdown 缓存");
        let key = pane.blocks[0].md.key();

        // 同一批 cells 再排一遍 → 命中（键不变 = 没重解析）
        pane.layout(&cells, &palette, 80, false);
        assert_eq!(pane.blocks[0].md.key(), key, "内容没变不该重算");

        // 内容变了（流式追加）→ 重算
        let cells = vec![Cell::assistant("**粗**体"), Cell::User("问题".into())];
        pane.layout(&cells, &palette, 80, false);
        assert_ne!(pane.blocks[0].md.key(), key, "内容变了要重算");

        // 宽度变了 → 也重算（缓存键含宽度：超宽表格的重排按宽度算）
        let narrow_key = pane.blocks[0].md.key();
        pane.layout(&cells, &palette, 40, false);
        assert_ne!(pane.blocks[0].md.key(), narrow_key, "宽度变了要重算");

        // 下标整体移位（往头部插一条）→ 指纹不符 ⇒ 该重排的照样重排（缓存不会张冠李戴）
        let shifted = vec![
            Cell::User("插到最前面".into()),
            Cell::assistant("**粗**体"),
            Cell::User("问题".into()),
        ];
        pane.layout(&shifted, &palette, 40, false);
        let mut fresh = Pane::default();
        fresh.layout(&shifted, &palette, 40, false);
        assert_eq!(pane.blocks.len(), 3);
        assert_eq!(
            pane.blocks[1].md.key(),
            fresh.blocks[1].md.key(),
            "换到下标 1 的那条要按它自己的内容重建缓存"
        );
    }
}
