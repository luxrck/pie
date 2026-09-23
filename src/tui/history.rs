//! 消息流：每条消息一个「单元格」，各自负责渲染成 `Line`。
//!
//! 参考 codex 的 `history_cell`：单元格外只保留顺序与滚动逻辑，视图细节都在这里。

use std::collections::HashMap;
use std::time::Duration;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::markdown::MarkdownCache;
use super::theme::{Palette, Status};
use crate::cancel::CANCEL_TEXT;
use crate::context;
use crate::llm::Message;
use unicode_width::UnicodeWidthChar;

/// 工具正文最多显示多少行（codex 那种紧凑风格；超出的按 §省略）。
const TOOL_BODY_LINES: usize = 12;

pub enum Cell {
    /// 用户输入（`› ` 前缀 + 亮色）
    User(String),
    /// 助手正文（markdown 渲染，带增量缓存）
    Assistant {
        text: String,
        md: MarkdownCache,
    },
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
    pub fn assistant() -> Self {
        Cell::Assistant {
            text: String::new(),
            md: MarkdownCache::default(),
        }
    }

    /// 往助手正文追加增量（没有就新建）。
    pub fn push_assistant_text(cells: &mut Vec<Cell>, delta: &str) {
        match cells.last_mut() {
            Some(Cell::Assistant { text, .. }) => text.push_str(delta),
            _ => {
                let mut cell = Cell::assistant();
                if let Cell::Assistant { text, .. } = &mut cell {
                    text.push_str(delta);
                }
                cells.push(cell);
            }
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

/// 工具调用的一行摘要：从参数 JSON 里抠出最有信息量的那个字段（`read`/`write`/`edit` 取
/// `path`、`shell` 取 `command`）。
///
/// 只有这两个键可达 —— 内置工具的字符串参数就 `path` / `command` / `content` / `edits`
/// （`content` 是正文、`edits` 是数组，都不适合当摘要），没有哪些工具用 `file_path` /
/// `pattern` / `query` 这类键（Python 那版是 `LEAN_SUMMARY_KEYS = {read/write/edit: path,
/// shell: command}`，同效）。以后加新工具再往这里补它的键。
pub fn tool_summary(arguments: &str) -> String {
    const KEYS: [&str; 2] = ["path", "command"];
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) {
        for key in KEYS {
            if let Some(v) = value.get(key).and_then(|v| v.as_str()) {
                return one_line(v, 180);
            }
        }
    }
    one_line(arguments, 180)
}

/// 工具结果里能不能看出失败（`[exit=N]`；非 shell 工具没有退出码 → 都算成功）。
///
/// 现在 `bash` **只在非 0 时**给这行头，所以「没头 = 成功」；`[exit=0]` 只可能来自旧会话
/// （改之前录的）或 Python 版，所以还得认。
pub fn tool_result_ok(content: &str) -> bool {
    match content.lines().next().unwrap_or("") {
        line if line.starts_with("[exit=") => line.starts_with("[exit=0]"),
        line if line.starts_with("[工具错误]") => false,
        _ => true,
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

/// 工具结果的状态：取消哨兵文本 → `⏹`，否则看 `[exit=N]` 头（非 shell 工具没有退出码 → 成功）。
pub fn tool_status(content: &str) -> Status {
    if content == CANCEL_TEXT {
        Status::Cancelled
    } else if tool_result_ok(content) {
        Status::Ok
    } else {
        Status::Fail
    }
}

/// 历史回放（resume）：把会话消息转成消息流单元格。
///
/// 输入应当是 `Session::full_history()`——压缩指针已展开；直接给 `messages` 也能跑，只是压缩过的
/// 回合只剩摘要 + 指针。规则与实时渲染对齐（对齐 Python `PieApp._render_history`）：
///
///   - `system`（system prompt / 窗口摘要）**不显示**；
///   - user → `› ` 一行（`synthetic` 的注入消息跳过：那是图片，不是用户输入）；
///   - assistant → markdown 正文（空正文不占位），带 `tool_calls` 的再各自起一条工具行；
///   - tool → 按 `tool_call_id` 合并进**配对的那条**工具行（同一批可能有同名工具，按名字回溯会错配），
///     配上工具名、参数摘要与最终状态。
pub fn cells_from_history(messages: &[Message]) -> Vec<Cell> {
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
                let text = context::content_text(m.content.as_ref());
                if !text.trim().is_empty() {
                    cells.push(Cell::User(text));
                }
            }
            "assistant" => {
                let text = context::content_text(m.content.as_ref());
                if !text.trim().is_empty() {
                    let mut cell = Cell::assistant();
                    if let Cell::Assistant { text: body, .. } = &mut cell {
                        *body = text;
                    }
                    cells.push(cell);
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
                let content = context::content_text(m.content.as_ref());
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

/// 一条显示行：渲染用的 `Line` + 它在**逻辑行**里的编号。
///
/// `src_line` 只用来判断「软换行的续行」：同一个逻辑行折出来的相邻显示行，复制时直接拼接
/// （源文本本来就连续），不同逻辑行之间才换行。
pub struct Row {
    pub line: Line<'static>,
    pub src_line: usize,
}

/// 消息流的排版结果：`rows` 是**已按宽度折好**的显示行。
///
/// 折行自己算（而非交给 `Paragraph::wrap`）只为了一件事：知道每个显示行对应源文本的哪一段
/// —— 复制按源文本切，软换行的长行复制成一行、不断成多行（对齐 Python `SelectableRichLog`
/// 的目标）。折点处**不丢字符**（断点空白留在上一行行尾），所以同一个逻辑行的相邻显示行
/// 拼起来就是原文。
#[derive(Default)]
pub struct Layout {
    pub rows: Vec<Row>,
}

impl Layout {
    /// 选区文本：显示行区间 `[r1, r2]`、单元格列区间 `[c1, c2]`（**含两端**——
    /// 光标压住的那个单元格也算进来，与终端选择一致）。
    pub fn slice_text(&self, r1: usize, c1: usize, r2: usize, c2: usize) -> String {
        let mut out = String::new();
        let mut prev: Option<usize> = None; // 上一行的逻辑行号（同号 = 续行，直接接上）
        let end_col = c2.saturating_add(1); // 含光标所在格 → 切到它右边
        for r in r1..=r2 {
            let Some(row) = self.rows.get(r) else { continue };
            let (from, to) = if r1 == r2 {
                (c1, end_col)
            } else if r == r1 {
                (c1, usize::MAX)
            } else if r == r2 {
                (0, end_col)
            } else {
                (0, usize::MAX)
            };
            let piece = crop_cells(&row_text(row), from, to);
            if prev == Some(row.src_line) {
                out.push_str(&piece);
            } else {
                if prev.is_some() {
                    out.push('\n');
                }
                out.push_str(&piece);
            }
            prev = Some(row.src_line);
        }
        out
    }
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

/// 字符占几个单元格（控制符算 0，与 ratatui 的渲染口径一致）。
fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// 把一条逻辑行按显示宽度折成显示行，追加到 `out`（`src_line` = 逻辑行号）。
fn wrap_line(line: &Line<'static>, src_line: usize, width: usize, out: &mut Vec<Row>) {
    for wrapped in wrap_line_into(line, width) {
        out.push(Row {
            line: wrapped,
            src_line,
        });
    }
}

/// 把一条逻辑行按显示宽度折成若干条显示行（`markdown::fit_tables` 重排单元格时也用它）。
///
/// 断行策略：优先在**空白之后**断（空白留在上一行行尾，一个字符不丢）；一个词自己就超过
/// 整行宽时硬断。空行也占一行（消息之间的空行就是这么来的）。
pub(crate) fn wrap_line_into(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    // 逐字符带样式展开：spans 会被折行切开，样式得跟着字符走
    let chars: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(|c| (c, s.style)))
        .collect();
    if chars.is_empty() {
        out.push(Line::default());
        return out;
    }
    let mut start = 0usize;
    while start < chars.len() {
        let mut used = 0usize;
        let mut soft = None; // 本行最后一个可断点（空白之后的下标）
        let mut full = false; // 是不是「装不下才停的」（装得下就别在空白处断）
        let mut end = start;
        while end < chars.len() {
            let w = char_width(chars[end].0);
            if used + w > width && end > start {
                full = true;
                break; // 放不下了（至少得放一个字符，否则死循环）
            }
            used += w;
            end += 1;
            if chars[end - 1].0 == ' ' {
                soft = Some(end);
            }
        }
        let end = if full { soft.unwrap_or(end) } else { end };
        out.push(rebuild_line(&chars[start..end], line));
        start = end;
    }
    out
}

/// 用一段「字符 + 样式」重建一条 `Line`（相邻同样式合并成一个 span；行级样式照搛）。
fn rebuild_line(chars: &[(char, Style)], template: &Line<'static>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (c, style) in chars {
        match spans.last_mut() {
            Some(last) if last.style == *style => last.content.to_mut().push(*c),
            _ => spans.push(Span::styled(c.to_string(), *style)),
        }
    }
    Line::from(spans).style(template.style)
}

/// 排版整个消息流（`width` = 消息流区域的宽度）。
///
/// `lean`（简洁模式，来自 `[tui] lean`）：**成功的工具结果不留正文**——一行说清就够；
/// 失败/取消才在下方跟正文块（统一缩进 2 空格）。盒子式全展开（正文一律截断到前
/// `TOOL_BODY_LINES` 行，末尾一行省略提示）。
///
/// 例外：手动 `!cmd`（`Cell::Tool { manual: true }`）**任何模式都展开**——那是用户主动
/// 执行的命令，输出本身就是要看的东西（对齐 Python：`!cmd` 的两个盒子都 `lean=False`）。
pub fn layout(cells: &mut [Cell], palette: &Palette, width: u16, lean: bool) -> Layout {
    let width = (width as usize).max(1);
    // `out` 先装**逻辑行**（不折），最后统一折成显示行
    let mut out: Vec<Line<'static>> = Vec::new();
    for cell in cells.iter_mut() {
        match cell {
            Cell::User(text) => {
                for (i, line) in text.lines().enumerate() {
                    let prefix = if i == 0 { "› " } else { "  " };
                    out.push(Line::from(vec![
                        Span::styled(
                            prefix,
                            Style::default()
                                .fg(palette.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(line.to_string(), palette.style_user()),
                    ]));
                }
            }
            Cell::Assistant { text, md } => {
                for line in md.get(text, width).lines.iter() {
                    out.push(line.clone());
                }
            }
            Cell::Thought(d) => out.push(Line::from(Span::styled(
                format!("• Thought for {}", fmt_duration(*d)),
                palette.style_faint(),
            ))),
            Cell::Tool {
                name,
                summary,
                status,
                body,
                manual,
            } => {
                let (mark, color) = palette.mark(*status);
                out.push(Line::from(vec![
                    Span::styled(format!("{mark} "), Style::default().fg(color)),
                    Span::styled(name.clone(), palette.style_tool()),
                    Span::styled(format!("({summary})"), palette.style_muted()),
                ]));
                if let Some(body) = body {
                    // 简洁模式：成功的工具结果只留一行（正文丢弃）；失败/取消、以及手动 `!cmd`
                    // （用户主动执行）才展示。
                    if *manual || !lean || *status != Status::Ok {
                        let total = body.lines().count();
                        for line in body.lines().take(TOOL_BODY_LINES) {
                            // 正文块统一缩进两格（不再在首行放 `↳`）
                            let prefix = "  ";
                            out.push(Line::from(Span::styled(
                                format!("{prefix}{line}"),
                                palette.style_muted(),
                            )));
                        }
                        if total > TOOL_BODY_LINES {
                            out.push(Line::from(Span::styled(
                                format!("    …（已省略 {} 行）", total - TOOL_BODY_LINES),
                                palette.style_faint(),
                            )));
                        }
                    }
                }
            }
            Cell::Notice(text) => {
                // 多行提示（`/help` 文案、`/status` 报告）要真的分行——富文本里的 `\n` 不是换行
                for (i, line) in text.lines().enumerate() {
                    let prefix = if i == 0 { "· " } else { "  " };
                    out.push(Line::from(Span::styled(
                        format!("{prefix}{line}"),
                        palette.style_muted(),
                    )));
                }
            }
            Cell::Retry { text, .. } => {
                // 重试进度：单个块，每次重试就地改写（不再一行一条）
                for (i, line) in text.lines().enumerate() {
                    let prefix = if i == 0 { "⟳ " } else { "  " };
                    out.push(Line::from(Span::styled(
                        format!("{prefix}{line}"),
                        palette.style_faint(),
                    )));
                }
            }
            Cell::Error(text) => {
                for (i, line) in text.lines().enumerate() {
                    let prefix = if i == 0 { "✗ " } else { "  " };
                    out.push(Line::from(Span::styled(
                        format!("{prefix}{line}"),
                        palette.style_error(),
                    )));
                }
            }
        }
        out.push(Line::default());
    }
    let mut rows = Vec::new();
    for (i, line) in out.iter().enumerate() {
        wrap_line(line, i, width, &mut rows);
    }
    Layout { rows }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Content, FunctionCall, ToolCall};

    /// 排版结果的纯文本（显示行按行拼；行内 spans 直接连起来）。
    /// 排版结果的纯文本（显示行按行拼；行内 spans 直接连起来）。
    fn plain(layout: &Layout) -> String {
        layout
            .rows
            .iter()
            .map(|r| {
                r.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 排版（默认深色色板、盒子式）。
    fn laid_out(cells: &mut [Cell], width: u16) -> Layout {
        layout(cells, &Palette::mocha(), width, false)
    }

    #[test]
    fn wrap_never_exceeds_the_width_and_keeps_every_char() {
        // 中英混排 + 超长单词：每一行都不能超宽，且拼起来必须是原文
        let samples = [
            "alpha beta gamma delta epsilon zeta",
            "中文混排 english words 一起折行的测试",
            "一个超级长的单词withoutanyspacesatall还有中文",
        ];
        for text in samples {
            let mut cells = vec![Cell::User(text.into())];
            let l = laid_out(&mut cells, 16);
            assert!(l.rows.len() > 1, "{text} 在宽 16 下应该折行");
            for row in &l.rows {
                assert!(row.line.width() <= 16, "超宽：{:?}", row_text(row));
            }
            let joined: String = l.rows.iter().map(row_text).collect();
            assert_eq!(joined, format!("› {text}"), "折行不能丢字符");
        }
    }

    #[test]
    fn copy_of_a_soft_wrapped_line_stays_one_line() {
        let mut cells = vec![Cell::User("alpha beta gamma delta epsilon".into())];
        let l = laid_out(&mut cells, 20);
        assert!(l.rows.len() > 1, "先得真的折了行");
        let last = l.rows.len() - 1;
        // 全选（末尾列给够）：同一条逻辑行折出来的显示行要拼回一行
        let text = l.slice_text(0, 0, last, 19).trim_end().to_string();
        assert_eq!(text, "› alpha beta gamma delta epsilon");
        assert!(!text.contains('\n'), "软换行不算换行：{text:?}");
    }

    #[test]
    fn copy_breaks_between_logical_lines() {
        let mut cells = vec![
            Cell::User("第一行".into()),
            Cell::Notice("第二行".into()),
        ];
        let l = laid_out(&mut cells, 40);
        let last = l.rows.len() - 1;
        let text = l.slice_text(0, 0, last, 39);
        assert!(text.contains("第一行\n"), "不同逻辑行之间才换行：{text:?}");
        assert!(text.contains("第二行"), "{text:?}");
    }

    #[test]
    fn selection_crops_by_cells_without_splitting_wide_chars() {
        let mut cells = vec![Cell::User("中文测试".into())];
        let l = laid_out(&mut cells, 80);
        assert_eq!(row_text(&l.rows[0]), "› 中文测试", "一行放得下");
        // “› ” 占 2 格 → “中文” 在格 2..6
        assert_eq!(l.slice_text(0, 2, 0, 5), "中文");
        // 只选到“中”的右半格，整个字也要（不能劈成半个）
        assert_eq!(l.slice_text(0, 2, 0, 2), "中");
        // 起点落在“中”的右半格（第 3 格）→ 从**下一个**字开始（同 Python `_cell_to_char` 口径）
        assert_eq!(l.slice_text(0, 3, 0, 6), "文测");
        // 末尾列给大也只会取到行尾
        assert_eq!(l.slice_text(0, 0, 0, 999), "› 中文测试");
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
        let mut cells = cells_from_history(&messages);
        let text = plain(&layout(&mut cells, &palette, 80, false));
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

    #[test]
    fn history_replay_keeps_unpaired_tool_result_and_cancelled_status() {
        let palette = Palette::mocha();
        let messages = vec![
            // 配对信息缺失（老会话没写 tool_call_id）→ 至少把结果行显示出来
            Message::tool_result("", "bash", "[exit=0]\n\nhi"),
            Message::tool_result("call_x", "bash", crate::cancel::CANCEL_TEXT),
        ];
        let mut cells = cells_from_history(&messages);
        let text = plain(&layout(&mut cells, &palette, 80, false));
        assert!(text.contains("✓ bash"), "{text}");
        assert!(text.contains("hi"), "{text}");
        assert!(text.contains("⏹"), "取消哨兵 → ⏹：{text}");
    }

    #[test]
    fn renders_user_assistant_tool_and_thought() {
        let palette = Palette::mocha();
        let mut cells = vec![
            Cell::User("你好\n第二行".into()),
            Cell::assistant(),
            Cell::Thought(Duration::from_millis(3400)),
            Cell::Tool {
                name: "bash".into(),
                summary: "pwd".into(),
                status: Status::Ok,
                body: Some("/tmp\n".into()),
                manual: false,
            },
        ];
        Cell::push_assistant_text(&mut cells, "**在**的");
        let text = plain(&layout(&mut cells, &palette, 80, false));
        assert!(text.contains("› 你好"), "{text}");
        assert!(text.contains("  第二行"), "续行缩进：{text}");
        assert!(text.contains("在"), "{text}");
        assert!(text.contains("• Thought for 3.4s"), "{text}");
        assert!(text.contains("✓ bash(pwd)"), "{text}");
        assert!(text.contains("  /tmp"), "盒子式：正文展示：{text}");
    }

    #[test]
    fn lean_hides_successful_tool_body_but_keeps_failed_one() {
        let palette = Palette::mocha();
        let mut cells = vec![
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
        let text = plain(&layout(&mut cells, &palette, 80, true));
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
        assert_eq!(tool_summary("不是 JSON"), "不是 JSON");
        assert!(tool_result_ok("[exit=0]\n\nhi"));
        assert!(!tool_result_ok("[exit=3]\n\nboom"));
        assert!(!tool_result_ok("[工具错误] 文件不存在: x"));
        assert!(tool_result_ok("plain text"), "没有退出码就当成功");
        assert_eq!(fmt_duration(Duration::from_millis(63_400)), "1m03s");
    }
}
