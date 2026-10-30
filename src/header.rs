//! 工具结果的**头区**：`header! { … }` 块 + 空行 + 正文（契约见 `docs/header-protocol.md`）。
//!
//! ```text
//! header! { "lines": "1-10", "total": 100 }
//!
//! <正文>
//! ```
//!
//! 形状规则：
//!   - 块必须在输出**最开头**，块内是严格 JSON object（**不允许尾逗号**）；
//!   - 块后：没有正文时文本即结束；有正文时必须是 `\n\n` 再接正文；
//!   - 渲染：0 键不输出、1 键单行、≥2 键多行（4 空格缩进、中间行带逗号、末行不带）；
//!   - 解析失败 → **整段当正文**（头区是元数据，坏了不该让工具失败）。
//!
//! 消费者：`ToolOutput::to_text`（渲染）、TUI/CLI（[`strip_headers`] 展示时剥掉）、
//! 压缩/回放（[`split_header_body`] 切分）。算法与 `shplugin::schema_block` 同源
//! （配平扫描 + `serde_json`），但输入形态不同（注释流 vs 原文），不强行共用。

use serde_json::{Map, Value};

/// 头区：**有序**键值（`Vec` 而不是 `Map`，渲染时按插入序输出，diff 不因重排而爆）。
pub type Headers = Vec<(String, Value)>;

/// 头区 → `header!` 块文本（**0 键 = 空串**）。末尾不带空行（拼正文是调用方的事）。
pub fn render(headers: &Headers) -> String {
    let pairs: Vec<String> = headers
        .iter()
        .map(|(key, value)| {
            format!(
                "{}: {}",
                serde_json::to_string(key).expect("键一定是合法字符串"),
                serde_json::to_string(value).expect("Value 一定可序列化")
            )
        })
        .collect();
    match pairs.len() {
        0 => String::new(),
        1 => format!("header! {{ {} }}", pairs[0]),
        _ => {
            let mut out = String::from("header! {\n");
            for (i, pair) in pairs.iter().enumerate() {
                out.push_str("    ");
                out.push_str(pair);
                if i + 1 < pairs.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push('}');
            out
        }
    }
}

/// 切分：开头有 `header!` 块就返回 `(头区, 正文)`，否则 `(空, 原文)`。
pub fn split_header_body(text: &str) -> (Headers, &str) {
    parse_block(text).unwrap_or_else(|| (Headers::new(), text))
}

/// 展示用：剥掉开头的 `header!` 块（没有就原样返回）。
pub fn strip_headers(text: &str) -> &str {
    split_header_body(text).1
}

fn parse_block(text: &str) -> Option<(Headers, &str)> {
    let after = text.strip_prefix("header!")?;
    let after = after.trim_start_matches([' ', '\t']);
    if !after.starts_with('{') {
        return None;
    }
    let open = text.len() - after.len();
    let close = scan_braces(text, open)?;
    let map: Map<String, Value> = serde_json::from_str(&text[open..close]).ok()?;
    let headers: Headers = map.into_iter().collect();

    let rest = &text[close..];
    if rest.is_empty() {
        return Some((headers, "")); // 块后没有换行 = 文本就到这里
    }
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))?;
    if rest.is_empty() {
        return Some((headers, "")); // 块后只有一个换行
    }
    // 有正文 → 必须隔一个空行（否则正文首行就被误当正文之前的行）
    let body = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))?;
    Some((headers, body))
}

/// 从 `open`（必须指向 `{`）开始配平括号，返回闭合 `}` **之后**的字节位置。
///
/// 跳过字符串与转义（字符串里的 `}`/`{`/`"` 不参与配平）。按字节扫是安全的：
/// UTF-8 的续字节都 ≥ 0x80，不会撞上这些 ASCII 字符。
fn scan_braces(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let (mut in_string, mut escaped) = (false, false);
    for (i, &byte) in bytes.iter().enumerate().skip(open) {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers(pairs: &[(&str, Value)]) -> Headers {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn empty_headers_render_nothing() {
        assert_eq!(render(&Headers::new()), "");
    }

    #[test]
    fn one_key_is_a_single_line() {
        let h = headers(&[("exit", json!(1))]);
        assert_eq!(render(&h), r#"header! { "exit": 1 }"#);
    }

    #[test]
    fn many_keys_are_a_block_in_insertion_order() {
        let h = headers(&[("lines", json!("1-10")), ("total", json!(100))]);
        assert_eq!(
            render(&h),
            "header! {\n    \"lines\": \"1-10\",\n    \"total\": 100\n}"
        );
    }

    #[test]
    fn render_and_parse_round_trip() {
        let h = headers(&[
            ("lines", json!("1-10")),
            ("total", json!(100)),
            ("image", json!("/tmp/a b,c.png")),
        ]);
        let text = format!("{}\n\n正文", render(&h));
        let (mut parsed, body) = split_header_body(&text);
        assert_eq!(body, "正文");
        // 解析侧用 serde 的 `Map`（BTreeMap）→ 键按字母序回来；渲染侧的插入序只用于展示
        parsed.sort_by(|a, b| a.0.cmp(&b.0));
        let mut want = h.clone();
        want.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(parsed, want, "键与值都要原样回来");
    }

    #[test]
    fn braces_inside_strings_do_not_end_the_block() {
        let h = headers(&[("saved", json!("/tmp/{weird}].txt")), ("n", json!(1))]);
        let text = format!("{}\n\nbody", render(&h));
        let (mut parsed, body) = split_header_body(&text);
        parsed.sort_by(|a, b| a.0.cmp(&b.0));
        let mut want = h.clone();
        want.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(parsed, want);
        assert_eq!(body, "body");
    }

    #[test]
    fn a_json_first_line_without_the_marker_is_plain_body() {
        let text = "{\"a\":1}\n\n不是头区";
        let (parsed, body) = split_header_body(text);
        assert!(parsed.is_empty());
        assert_eq!(body, text, "没标记就整段是正文");
    }

    #[test]
    fn body_without_a_blank_line_is_not_a_header() {
        let text = "header! { \"exit\": 1 }\n同一行的正文";
        let (parsed, body) = split_header_body(text);
        assert!(parsed.is_empty(), "块后必须空行才认");
        assert_eq!(body, text);
    }

    #[test]
    fn block_without_body_is_fine() {
        let (parsed, body) = split_header_body(r#"header! { "status": "failed" }"#);
        assert_eq!(parsed, headers(&[("status", json!("failed"))]));
        assert_eq!(body, "");

        let text = format!("{}\n", r#"header! { "status": "failed" }"#);
        let (parsed, body) = split_header_body(&text);
        assert_eq!(parsed.len(), 1);
        assert_eq!(body, "");
    }

    #[test]
    fn broken_json_is_plain_text() {
        let text = "header! { 不是 JSON }\n\nbody";
        let (parsed, body) = split_header_body(text);
        assert!(parsed.is_empty());
        assert_eq!(body, text);
    }

    #[test]
    fn strip_headers_leaves_plain_text_alone() {
        assert_eq!(strip_headers("line1\nline2"), "line1\nline2");
        assert_eq!(
            strip_headers("header! { \"lines\": \"1-10\" }\n\nline1\nline2"),
            "line1\nline2"
        );
    }
}
