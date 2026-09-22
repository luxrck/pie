//! Markdown 渲染：把助手正文转成 ratatui 的 `Text`，并**按单元格缓存**。
//!
//! 为什么要有缓存：正文是**增量流式**进来的，如果每帧都重解析整段，长回复会白烧 CPU。
//! 做法与 codex 的 `markdown_stream` / `markdown_text_merge` 同一个思路（那边是自己写渲染，
//! 要更细的流式控制）：这里先用 `tui-markdown` 转换，只在文本变化时重解析一次。
//!
//! ⚠ 关掉了 `tui-markdown` 的默认特性 `highlight-code`——它会拉 `syntect`，而 syntect 默认
//! 后端是 oniguruma（C 库）。代码高亮等真需要时再上（换 `default-fancy` 后端即可）。

use ratatui::text::{Line, Span, Text};

/// 一段 markdown 的渲染缓存：`key` 是源文本的廉价指纹，变了才重解析。
#[derive(Default)]
pub struct MarkdownCache {
    key: u64,
    text: Text<'static>,
}

impl MarkdownCache {
    /// 取渲染结果（源文本没变就复用上一次的 `Text`）。
    pub fn get(&mut self, src: &str) -> &Text<'static> {
        let key = fingerprint(src);
        if key != self.key {
            self.text = render(src);
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

fn render(src: &str) -> Text<'static> {
    // 流式中途可能出现未闭合的 ``` fence，tui-markdown 会照常当代码块处理，不用特判。
    // `from_str` 返回借用 `src` 的 `Text`，这里转成 owned（缓存要 `'static`）。
    let text = tui_markdown::from_str(src);
    Text {
        lines: text
            .lines
            .into_iter()
            .map(|line| Line {
                spans: line
                    .spans
                    .into_iter()
                    .map(|span| Span::styled(span.content.into_owned(), span.style))
                    .collect(),
                style: line.style,
                alignment: line.alignment,
            })
            .collect(),
        style: text.style,
        alignment: text.alignment,
    }
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

    #[test]
    fn renders_markdown_and_caches_by_content() {
        let mut cache = MarkdownCache::default();
        let first = plain(cache.get("**粗** 和 `code`"));
        assert!(first.contains("粗"), "{first}");
        assert!(first.contains("code"), "{first}");
        // 同一内容：key 不变 → 复用（这里只能验证结果一致）
        let again = plain(cache.get("**粗** 和 `code`"));
        assert_eq!(first, again);
        // 内容变了 → 重新解析
        let grown = plain(cache.get("**粗** 和 `code`\n\n- 一项\n- 两项"));
        assert!(grown.contains("一项") && grown.contains("两项"), "{grown}");
    }

    #[test]
    fn tolerates_unclosed_code_fence_while_streaming() {
        let mut cache = MarkdownCache::default();
        let text = plain(cache.get("看这段：\n```rust\nfn main() {"));
        assert!(
            text.contains("fn main()"),
            "未闭合 fence 也要能渲染：{text}"
        );
    }
}
