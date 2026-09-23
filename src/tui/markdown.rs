//! Markdown 渲染：把助手正文转成 ratatui 的 `Text`，并**按单元格缓存**。
//!
//! 为什么要有缓存：正文是**增量流式**进来的，如果每帧都重解析整段，长回复会白烧 CPU。
//! 做法与 codex 的 `markdown_stream` / `markdown_text_merge` 同一个思路（那边是自己写渲染，
//! 要更细的流式控制）：这里先用 `tui-markdown` 转换，只在文本（或宽度）变化时重解析一次。
//!
//! ⚠ 关掉了 `tui-markdown` 的默认特性 `highlight-code`——它会拉 `syntect`，而 syntect 默认
//! 后端是 oniguruma（C 库）。代码高亮等真需要时再上（换 `default-fancy` 后端即可）。
//!
//! **表格按可用宽度重排**（`fit_tables`）：`tui-markdown` 的列宽 = 内容自然宽度，它根本
//! 不知道终端有多宽；而消息流是 `history::layout` 自己逐行折行的 → 超宽表格会被拦腰折断、
//! 边框画歪（一张 3 列中文表轻松到 116 列，窗口只有 100 就花屏）。所以渲染完再过一遍：
//! → 表格比可用宽度宽时，收缩列宽 + 单元格内折行，重画边框。为此**缓存键必须带上宽度**
//! （窗口一变就得重排）。

use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};

use super::history::wrap_line_into;

/// 一段 markdown 的渲染缓存：`key` 是「源文本 + 可用宽度」的廉价指纹，变了才重解析。
#[derive(Default)]
pub struct MarkdownCache {
    key: u64,
    text: Text<'static>,
}

impl MarkdownCache {
    /// 取渲染结果（源文本与宽度都没变就复用上一次的 `Text`）。
    ///
    /// `width` = 消息流区域宽度：只有表格重排它会用到，但不传就画不出正确的表宽。
    pub fn get(&mut self, src: &str, width: usize) -> &Text<'static> {
        let key = fingerprint(src) ^ (width as u64).rotate_left(17);
        if key != self.key {
            self.text = render(src, width);
            self.key = key;
        }
        &self.text
    }
}

/// 源文本指纹：长度 + 首尾各取一小段（比整段 hash 便宜，够用——流式追加必然改变尾部）。
fn fingerprint(src: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    src.len().hash(&mut hasher);
    // ⚠️ 必须落在字符边界上：中文按字节切会 panic（`byte index N is not a char boundary`）
    src[..floor_char_boundary(src, 64)].hash(&mut hasher);
    let tail_start = floor_char_boundary(src, src.len().saturating_sub(64));
    src[tail_start..].hash(&mut hasher);
    hasher.finish()
}

/// 把字节位置向左退到最近的字符边界（`i` 越界时返回 `len`）。
fn floor_char_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn render(src: &str, width: usize) -> Text<'static> {
    // 流式中途可能出现未闭合的 ``` fence，tui-markdown 会照常当代码块处理，不用特判。
    // `from_str` 返回借用 `src` 的 `Text`，这里转成 owned（缓存要 `'static`）。
    let text = tui_markdown::from_str(src);
    let mut lines: Vec<Line<'static>> = text.lines.into_iter().map(own_line).collect();
    fit_tables(&mut lines, width.max(1));
    Text {
        lines,
        style: text.style,
        alignment: text.alignment,
    }
}

fn own_line(line: Line<'_>) -> Line<'static> {
    Line {
        spans: line
            .spans
            .into_iter()
            .map(|span| Span::styled(span.content.into_owned(), span.style))
            .collect(),
        style: line.style,
        alignment: line.alignment,
    }
}

// ---------------------------------------------------------------- 表格重排

const TOP_LEFT: char = '┌';
const HEADER_TEE: char = '├';
const BOTTOM_LEFT: char = '└';
const VERTICAL: char = '│';
const TOP_TEE: char = '┬';
const HEADER_CROSS: char = '┼';
const HORIZONTAL: char = '─';

/// 列的对齐（从原表的填充推出来，`tui-markdown` 按 markdown 的 `:---:` 标注排的）。
#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Right,
    Center,
}

/// 把超宽表格重排进 `width` 列；不超宽的表格原样留着。
///
/// 表格块的识别靠 `tui-markdown` 的输出形状：`┌…┐` 开头、`└…┘` 收尾、中间全是边框开头的行
/// （顶框 / 表头 / 分隔 / 正文 / 底框）。**代码块里逐字渲染的方框字符理论上会误判**——
/// 但要求连续三行恰好是「┌…┬…┐ / │…│ / ├…┼…┤」这种形状，实际不可能。
fn fit_tables(lines: &mut Vec<Line<'static>>, width: usize) {
    let mut i = 0;
    while i < lines.len() {
        if border_kind(&lines[i]) != Some(TOP_LEFT) {
            i += 1;
            continue;
        }
        let mut end = None;
        let mut j = i + 1;
        while j < lines.len() {
            match border_kind(&lines[j]) {
                Some(BOTTOM_LEFT) => {
                    end = Some(j);
                    break;
                }
                Some(_) => j += 1,
                None => break, // 没等到 `└` 就不是表格
            }
        }
        let Some(end) = end else {
            i += 1;
            continue;
        };
        let natural = lines[i..=end].iter().map(Line::width).max().unwrap_or(0);
        if natural > width {
            let shrunk = shrink_table(&lines[i..=end], width);
            let len = shrunk.len();
            lines.splice(i..=end, shrunk);
            i += len;
        } else {
            i = end + 1;
        }
    }
}

/// 一行的第一个字符是不是方框字符（表格块的判据）。
fn border_kind(line: &Line<'_>) -> Option<char> {
    for span in &line.spans {
        if let Some(c) = span.content.chars().next() {
            return matches!(c, TOP_LEFT | HEADER_TEE | BOTTOM_LEFT | VERTICAL).then_some(c);
        }
    }
    None
}

/// 把一张表格重排到 `width` 列以内：列宽按需收缩、单元格内容在格内折行、边框按新列宽重画。
fn shrink_table(block: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    // `tui-markdown` 的表格形状固定：顶框 / 表头 / 分隔 / 正文… / 底框
    if block.len() < 4 {
        return block.to_vec();
    }
    let columns = line_text(&block[0]).matches(TOP_TEE).count() + 1;
    let raw_header = split_cells(&block[1], columns);
    let raw_body: Vec<Vec<Vec<Span<'static>>>> = block[3..block.len() - 1]
        .iter()
        .map(|line| split_cells(line, columns))
        .collect();

    let aligns = infer_alignments(&raw_header);
    let header: Vec<Vec<Span<'static>>> = raw_header.iter().map(|c| trim_spans(c)).collect();
    let body: Vec<Vec<Vec<Span<'static>>>> = raw_body
        .iter()
        .map(|row| row.iter().map(|c| trim_spans(c)).collect())
        .collect();

    let natural: Vec<usize> = (0..columns)
        .map(|c| {
            let mut w = cell_width(header.get(c));
            for row in &body {
                w = w.max(cell_width(row.get(c)));
            }
            w.max(1)
        })
        .collect();
    // 每格两端各占 1 空格、列间 1 根竖线：`1 + Σ(w+2) + (n-1)` = `3n + 1 + Σw`
    let budget = width.saturating_sub(3 * columns + 1).max(columns);
    let widths = shrink_widths(&natural, budget);

    let border_style = block[0]
        .spans
        .first()
        .map(|s| s.style)
        .unwrap_or_default();
    let mut out = vec![rule(TOP_LEFT, TOP_TEE, '┐', &widths, border_style)];
    out.extend(render_row(&header, &widths, &aligns, border_style));
    out.push(rule(HEADER_TEE, HEADER_CROSS, '┤', &widths, border_style));
    for row in &body {
        out.extend(render_row(row, &widths, &aligns, border_style));
    }
    out.push(rule(BOTTOM_LEFT, '┴', '┘', &widths, border_style));
    out
}

/// 一条横向边框（顶框 / 表头分隔 / 底框）。
fn rule(left: char, mid: char, right: char, widths: &[usize], style: Style) -> Line<'static> {
    let mut text = String::new();
    text.push(left);
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            text.push(mid);
        }
        for _ in 0..(w + 2) {
            text.push(HORIZONTAL);
        }
    }
    text.push(right);
    Line::from(Span::styled(text, style))
}

/// 一行表格：把每个单元格的内容折进各自的列宽，逐条显示行拼出边框。
fn render_row(
    cells: &[Vec<Span<'static>>],
    widths: &[usize],
    aligns: &[Align],
    border_style: Style,
) -> Vec<Line<'static>> {
    let wrapped: Vec<Vec<Line<'static>>> = (0..widths.len())
        .map(|c| {
            let cell = Line::from(cells.get(c).cloned().unwrap_or_default());
            wrap_line_into(&cell, widths[c].max(1))
                .iter()
                // 折行的续行可能以空白开头（上一条显示行正好塞满、空白挤到了下一行）→ 掐掉，
                // 免得格子里出现看着歪掉的缩进
                .map(|line| Line::from(trim_spans(&line.spans)))
                .collect()
        })
        .collect();
    let height = wrapped.iter().map(|l| l.len()).max().unwrap_or(1).max(1);
    (0..height)
        .map(|row| {
            let mut spans = vec![Span::styled(VERTICAL.to_string(), border_style)];
            for (c, width) in widths.iter().enumerate() {
                let line = wrapped[c].get(row);
                let content = line.map(Line::width).unwrap_or(0);
                let (left, right) = padding(*width, content, aligns[c]);
                spans.push(Span::raw(" ".repeat(left + 1)));
                if let Some(line) = line {
                    spans.extend(line.spans.iter().cloned());
                }
                spans.push(Span::raw(" ".repeat(right + 1)));
                spans.push(Span::styled(VERTICAL.to_string(), border_style));
            }
            Line::from(spans)
        })
        .collect()
}

/// 单元格里的内容宽度（不含两端填充）。
fn cell_width(cell: Option<&Vec<Span<'static>>>) -> usize {
    cell.map(|spans| spans.iter().map(Span::width).sum())
        .unwrap_or(0)
}

fn padding(column_width: usize, content_width: usize, align: Align) -> (usize, usize) {
    if content_width >= column_width {
        return (0, 0);
    }
    let free = column_width - content_width;
    match align {
        Align::Left => (0, free),
        Align::Right => (free, 0),
        Align::Center => (free / 2, free - free / 2),
    }
}

/// 列宽收缩：总是削当前**最宽**的那一列（水填法，保证各列尽量均衡），每列至少留 1 格。
fn shrink_widths(natural: &[usize], budget: usize) -> Vec<usize> {
    let mut widths = natural.to_vec();
    let mut total: usize = widths.iter().sum();
    while total > budget {
        let mut target = None;
        let mut widest = 1;
        for (i, w) in widths.iter().enumerate() {
            if *w > widest {
                widest = *w;
                target = Some(i);
            }
        }
        match target {
            Some(i) => {
                widths[i] -= 1;
                total -= 1;
            }
            None => break, // 每列都到底了（不可能更窄）
        }
    }
    widths
}

/// 一行表格按竖线切成单元格（保留每格的样式），去掉两条外框之外的段，补齐到 `columns` 格。
fn split_cells(line: &Line<'static>, columns: usize) -> Vec<Vec<Span<'static>>> {
    let mut cells: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    for span in &line.spans {
        let mut buf = String::new();
        for ch in span.content.chars() {
            if ch == VERTICAL {
                if !buf.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut buf), span.style));
                }
                cells.push(std::mem::take(&mut current));
            } else {
                buf.push(ch);
            }
        }
        if !buf.is_empty() {
            current.push(Span::styled(buf, span.style));
        }
    }
    cells.push(current);
    // `│ a │ b │` 切出来是 ["", " a ", " b ", ""]：掐掉首段（外框左边）与末段（外框右边）
    if !cells.is_empty() {
        cells.remove(0);
    }
    if cells.len() == columns + 1 && cells.last().is_some_and(|c| c.is_empty()) {
        cells.pop();
    }
    cells.truncate(columns);
    while cells.len() < columns {
        cells.push(Vec::new());
    }
    cells
}

/// 从**未 trim** 的表头格推断列对齐：两端各有一个强制填充空格，多出来的那边就是对不齐的方向。
fn infer_alignments(header: &[Vec<Span<'static>>]) -> Vec<Align> {
    header
        .iter()
        .map(|cell| {
            let (lead, trail) = edge_spaces(cell);
            match (lead.saturating_sub(1) > 0, trail.saturating_sub(1) > 0) {
                (true, true) => Align::Center,
                (true, false) => Align::Right,
                _ => Align::Left,
            }
        })
        .collect()
}

/// 单元格两端各有几个空格（含强制的那 1 个）。
fn edge_spaces(cell: &[Span<'static>]) -> (usize, usize) {
    let text = line_text(&Line::from(cell.to_vec()));
    let lead = text.len() - text.trim_start_matches(' ').len();
    let trail = text.len() - text.trim_end_matches(' ').len();
    (lead, trail)
}

/// 去掉单元格**两端**的填充空白（样式跟着字符走；**中间的空白是内容，一个不能动**）。
fn trim_spans(cell: &[Span<'static>]) -> Vec<Span<'static>> {
    let chars: Vec<(char, Style)> = cell
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();
    // 注意不能逐 span 各 trim 一遍：`与 /reset` 里那个空格自己是单独一个 span，
    // 逐段 trim 会把它吃掉。
    let start = chars
        .iter()
        .position(|(c, _)| *c != ' ')
        .unwrap_or(chars.len());
    let end = chars
        .iter()
        .rposition(|(c, _)| *c != ' ')
        .map(|i| i + 1)
        .unwrap_or(0);
    let chars = if start < end { &chars[start..end] } else { &[][..] };
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (c, style) in chars {
        match spans.last_mut() {
            Some(last) if last.style == *style => last.content.to_mut().push(*c),
            _ => spans.push(Span::styled(c.to_string(), *style)),
        }
    }
    spans
}

fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn widths(text: &Text<'_>) -> Vec<usize> {
        text.lines.iter().map(Line::width).collect()
    }

    /// 一张三列表：自然宽度远超窄窗口。
    const TABLE: &str = r#"| 命令 | Python | Rust |
|---|---|---|
| `/clear` | `session.clear_window()`，归档窗口到 `~/.pie/windows/` | 与 `/reset` 同实现，仅 `reset()`，提示「窗口归档未接」 |
| `/reset` | 清历史 | 清历史（cells 也清） |
"#;

    #[test]
    fn renders_markdown_and_caches_by_content() {
        let mut cache = MarkdownCache::default();
        let first = plain(cache.get("**粗** 和 `code`", 80));
        assert!(first.contains("粗"), "{first}");
        assert!(first.contains("code"), "{first}");
        // 同一内容：key 不变 → 复用（这里只能验证结果一致）
        let again = plain(cache.get("**粗** 和 `code`", 80));
        assert_eq!(first, again);
        // 内容变了 → 重新解析
        let grown = plain(cache.get("**粗** 和 `code`\n\n- 一项\n- 两项", 80));
        assert!(grown.contains("一项") && grown.contains("两项"), "{grown}");
        // **宽度**变了也要重排（缓存键带上宽度）
        let wide = plain(cache.get(TABLE, 200));
        let narrow = plain(cache.get(TABLE, 40));
        assert_ne!(wide, narrow, "换宽度要重排（而不是复用宽表）");
    }

    #[test]
    fn tolerates_unclosed_code_fence_while_streaming() {
        let mut cache = MarkdownCache::default();
        let text = plain(cache.get("看这段：\n```rust\nfn main() {", 80));
        assert!(
            text.contains("fn main()"),
            "未闭合 fence 也要能渲染：{text}"
        );
    }

    #[test]
    fn wide_table_is_reflowed_inside_the_width() {
        let mut cache = MarkdownCache::default();
        // 宽窗口：原样（自然宽度）
        let wide = cache.get(TABLE, 200);
        let natural = widths(wide).into_iter().max().unwrap_or(0);
        assert!(natural > 100, "这张表自然宽度就很宽：{natural}");
        assert!(plain(wide).starts_with("┌"), "是表格：\n{}", plain(wide));

        // 窄窗口：每一行都不超过宽度，且还是完整对齐的一张表
        let narrow = cache.get(TABLE, 50);
        let rows = widths(narrow);
        assert!(
            rows.iter().all(|w| *w <= 50),
            "重排后不能超宽：{rows:?}\n{}",
            plain(narrow)
        );
        let text = plain(narrow);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with('┌') && lines[0].ends_with('┐'), "{text}");
        assert!(
            lines.last().unwrap().starts_with('└') && lines.last().unwrap().ends_with('┘'),
            "{text}"
        );
        // 边框对齐：每一行的首尾字符都是竖线（折行后的续行也是）
        for line in &lines {
            assert!(
                line.starts_with('│') || line.starts_with('┌') || line.starts_with('├')
                    || line.starts_with('└'),
                "每行都要以边框开头：{line:?}"
            );
            assert!(
                line.ends_with('│') || line.ends_with('┐') || line.ends_with('┤')
                    || line.ends_with('┘'),
                "每行都要以边框结尾：{line:?}"
            );
        }
        // 内容一个不丢（去掉边框与填充空白后，字符集要完全一致）
        let mut want: Vec<char> = TABLE
            .chars()
            .filter(|c| !c.is_whitespace() && !"|`-:".contains(*c))
            .collect();
        want.sort_unstable();
        let mut got: Vec<char> = text
            .chars()
            .filter(|c| !c.is_whitespace() && !"│─┌┬┐├┼┤└┴┘".contains(*c))
            .collect();
        got.sort_unstable();
        assert_eq!(got, want, "重排不能丢内容：\n{text}");
    }

    #[test]
    fn narrow_table_is_left_alone() {
        let mut cache = MarkdownCache::default();
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let before = plain(cache.get(src, 200)).to_string();
        let after = plain(cache.get(src, 200)).to_string();
        assert_eq!(before, after);
        assert!(before.starts_with('┌'), "{before}");
    }

    #[test]
    fn cell_content_keeps_its_style() {
        let mut cache = MarkdownCache::default();
        let text = cache.get(TABLE, 40);
        // 行内代码 `session.clear_window()` 在重排后仍有独立样式（不是全篇一个样式）
        let styled = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.content.contains("session"))
            .any(|s| s.style != Style::default());
        assert!(styled, "单元格里的行内代码样式要留着");
    }

    #[test]
    fn degenerate_tables_do_not_panic() {
        let mut cache = MarkdownCache::default();
        // 只有表头（没有正文行）
        let src = "| 一个很长很长的表头单元格内容 | 另一个很长的表头 |\n|---|---|\n";
        let text = plain(cache.get(src, 30));
        assert!(text.starts_with('┌'), "{text}");
        assert!(widths(cache.get(src, 30)).iter().all(|w| *w <= 30), "{text}");
        // 单列表
        let src = "| 唯一一列很长很长很长很长的内容 |\n|---|\n| 值也很长很长很长很长很长很长 |\n";
        let text = plain(cache.get(src, 20));
        assert!(widths(cache.get(src, 20)).iter().all(|w| *w <= 20), "{text}");
        // 窄到装不下三列的边框（预算被夹住）也不能崩
        let _ = plain(cache.get(TABLE, 8));
    }

    #[test]
    fn shrinks_the_widest_column_first() {
        assert_eq!(shrink_widths(&[10, 4, 4], 18), vec![10, 4, 4]);
        assert_eq!(shrink_widths(&[10, 4, 4], 14), vec![6, 4, 4]);
        assert_eq!(shrink_widths(&[10, 10, 10], 12), vec![4, 4, 4]);
        // 每列至少 1 格（预算再小也不为 0）
        assert_eq!(shrink_widths(&[3, 3], 1), vec![1, 1]);
    }

    #[test]
    fn trims_padding_but_keeps_inner_spaces() {
        let cell = vec![Span::raw("  a b  ")];
        let trimmed = trim_spans(&cell);
        let text: String = trimmed.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "a b");
        // 跨 span 的两端空白也要去掉，样式保留
        let cell = vec![
            Span::styled("  ", Style::default()),
            Span::styled("hi", Style::new().bold()),
            Span::raw("   "),
        ];
        let trimmed = trim_spans(&cell);
        let text: String = trimmed.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "hi");
        assert_eq!(trimmed[0].style, Style::new().bold());
        // **单元格中间**的空格（自己占一个 span，如 `与 ` + `/reset`）必须留着
        let cell = vec![
            Span::raw(" 与 "),
            Span::styled("/reset", Style::new().bold()),
            Span::raw(" 同实现 "),
        ];
        let trimmed = trim_spans(&cell);
        let text: String = trimmed.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "与 /reset 同实现");
    }
}
