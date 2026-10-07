//! Markdown 渲染：把助手正文转成 ratatui 的 `Text`，并**按单元格缓存**。
//!
//! 为什么要有缓存：正文是**增量流式**进来的，如果每帧都重解析整段，长回复会白烧 CPU。
//! 做法与 codex 的 `markdown_stream` / `markdown_text_merge` 同一个思路（那边是自己写渲染，
//! 要更细的流式控制）：这里先用 `tui-markdown` 转换，只在文本（或宽度）变化时重解析一次。
//!
//! ⚠ 开了 `tui-markdown` 的默认特性 `highlight-code`（2026-09-24）：代码块用 syntect 做语法
//! 高亮，主题是它内置的 Base16 Ocean Dark。代价是 syntect 默认后端 oniguruma（C 库，靠 `cc` 编译）
//! + 几 MB 语法/主题数据——**有意接受**（想零 C 依赖就换自写高亮，见 docs/CHANGELOG.md）。
//! 另注意：流式期间每个 delta 都会让缓存失效 → 整段重渲染（含 syntect），长代码块有 CPU 成本
//! （release 下 300 行代码块约 8ms/次）。
//!
//! **表格宽度交给上游**（2026-09-29）：`tui-markdown 0.3.10` 起有 `Options::table_width(w)`，
//! 会把单元格折进预算（含边框 / 内边距 / 外层列表与引用前缀），**默认 `None` = 自然宽度**。
//! 在这之前 pie 自己写过一套 `fit_tables`（检测 `┌…┘` 块 → 收缩列宽 → 重画边框），现已删掉。
//! 仍要**把宽度算进缓存键**：表的折行结果依赖宽度，窗口一变就得重渲染。

use ratatui::text::{Line, Span, Text};

/// 一条消息的 markdown 渲染缓存：`key` 是「源文本 + 可用宽度」的廉价指纹，变了才重解析。
///
/// 它**不住在 `Cell` 里**（那样 `Cell` 就不是纯数据了）：一条 cell 一个，住在 `pane::CellBlock`
/// 里（与那条 cell 一一对应；`Cell` 只管数据，排版结果与缓存都归块）。
#[derive(Default)]
pub struct MarkdownCache {
    key: u64,
    text: Text<'static>,
}

impl MarkdownCache {
    /// 取渲染结果（源文本与宽度都没变就复用上一次的 `Text`）。
    ///
    /// `width` = 消息流区域宽度：只有**表格折行**会用到（`Options::table_width`），
    /// 但不传就画不出正确的表宽。
    pub fn get(&mut self, src: &str, width: usize) -> &Text<'static> {
        let key = fingerprint(src) ^ (width as u64).rotate_left(17);
        if key != self.key {
            self.text = render(src, width);
            self.key = key;
        }
        &self.text
    }

    /// 缓存键（用例用：判「这一帧是命中还是重算」）。
    #[cfg(test)]
    pub(crate) fn key(&self) -> u64 {
        self.key
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
pub(crate) fn floor_char_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn render(src: &str, width: usize) -> Text<'static> {
    // 流式中途可能出现未闭合的 ``` fence，tui-markdown 会照常当代码块处理，不用特判。
    // `from_str_with_options` 返回借用 `src` 的 `Text`，这里转成 owned（缓存要 `'static`）。
    // `table_width` = 表格的列宽预算（含边框 / 内边距 / 外层前缀）：放得下就是自然宽度，
    // 放不下就把单元格折进预算——这条以前是 pie 的 `fit_tables` 自己做的。
    let options = tui_markdown::Options::default().table_width(width.max(1) as u16);
    let text = tui_markdown::from_str_with_options(src, &options);
    let lines: Vec<Line<'static>> = text.lines.into_iter().map(own_line).collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

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
        assert!(
            lines[0].starts_with('┌') && lines[0].ends_with('┐'),
            "{text}"
        );
        assert!(
            lines.last().unwrap().starts_with('└') && lines.last().unwrap().ends_with('┘'),
            "{text}"
        );
        // 边框对齐：每一行的首尾字符都是竖线（折行后的续行也是）
        for line in &lines {
            assert!(
                line.starts_with('│')
                    || line.starts_with('┌')
                    || line.starts_with('├')
                    || line.starts_with('└'),
                "每行都要以边框开头：{line:?}"
            );
            assert!(
                line.ends_with('│')
                    || line.ends_with('┐')
                    || line.ends_with('┤')
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
        assert!(
            widths(cache.get(src, 30)).iter().all(|w| *w <= 30),
            "{text}"
        );
        // 单列表
        let src = "| 唯一一列很长很长很长很长的内容 |\n|---|\n| 值也很长很长很长很长很长很长 |\n";
        let text = plain(cache.get(src, 20));
        assert!(
            widths(cache.get(src, 20)).iter().all(|w| *w <= 20),
            "{text}"
        );
        // 窄到装不下三列的边框（预算被夹住）也不能崩
        let _ = plain(cache.get(TABLE, 8));
    }
}
