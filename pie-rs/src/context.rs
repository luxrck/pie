//! 上下文管理层：三级压缩（工具级 / 轮次级 / 会话级）+ 原文落盘指针 + GC。
//!
//! 对齐 Python 版 `src/pie/context.py` 的语义。消息模型不同：那边是类层级
//! （`UserMessage` / `AssistantMessage` / `ToolMessage` / …），这边是扁平 `Vec<Message>`，
//! 靠 `role` + `compress_level` + `synthetic` 判定，三种压缩都以「就地改写消息列表」实现：
//!   - **工具级**（level 1）：`keep_last_steps` 个 step 批次**之外**的 tool 输出，行数超过
//!     `head+tail` 就全文落盘 + 头部/尾部留预览，内容换成 `[工具输出全文已保存: <path>]` 指针；
//!   - **轮次级**（level 2）：已完成的轮次（最后一个 user 之前的）压成一条摘要 assistant
//!     （user 保留），原文落盘成 `[轮次原文已保存: <path>]`，摘要只留模型最终输出；
//!   - **会话级**（level 3）：当前轮之前的整段历史落盘成**窗口块**
//!     （`~/.pie/context/session-*.txt`，与 Python `write_raw` 同目录；被 manifest / 会话 meta 引用，
//!     GC 不碰——Python 那个 `~/.pie/windows/` 是 `/clear` 归档用的，Rust 还没实现 `/clear`），
//!     插一条 `[历史窗口: <path>]` 摘要 system 消息，旧窗口摘要继续留在上下文里；
//!   - 压缩级别**只升不降**；落盘按内容 hash 寻址（同内容只存一份）。
//!
//! 两个驱动入口：`maybe_compact`（自动，看软阈值：相对「可用输入预算」的 `soft_ratio`，
//! 压到 `target_ratio` 以下）与 `compact`（手动 `/compact`，不看水位）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{pie_dir, Config};
use crate::llm::{Content, Message};

// ---------------------------------------------------------------- 目录与常量

/// 落盘正文目录（`PIE_DIR` 可重定向）。
pub fn context_dir() -> PathBuf {
    pie_dir().join("context")
}

/// 窗口摘要消息的开头标记（Python 同款，便于识别）。
pub const WINDOW_SUMMARY_MARKER: &str = "[历史窗口:";

/// 工具级压缩时中间省略的标记。
const TOOL_GAP: &str = "...[中间省略]...";

/// 服务端规则：图片按约 1300×1300 折成 token 后**单图上限 1024**（内联与 Files API 同一个上界）。
const IMAGE_TOKENS_MAX: i64 = 1024;

// ---------------------------------------------------------------- content 归一与估算

/// 把 content 归一为可读文本：纯文本原样；多模态 parts 拼 text 片段、图片用 `[图片]` 占位
/// （data URI 不进人读文本）。摘要 / 标题 / 日志都走这里。
pub fn content_text(content: Option<&Content>) -> String {
    match content {
        Some(Content::Text(t)) => t.clone(),
        Some(Content::Parts(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p.get("text").and_then(Value::as_str).map(str::to_string),
                Some("image_url") | Some("file") => Some("[图片]".to_string()),
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
}

/// content 的 token 估算：文本按字符/4；图片按单图上界（data URI 粗估后封顶）。
fn content_tokens(content: Option<&Content>) -> i64 {
    match content {
        Some(Content::Text(t)) => chars_div4(t),
        Some(Content::Parts(parts)) => parts
            .iter()
            .map(|p| match p.get("type").and_then(Value::as_str) {
                Some("image_url") => {
                    let url = p
                        .get("image_url")
                        .and_then(|v| v.get("url"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if url.starts_with("data:") {
                        IMAGE_TOKENS_MAX.min(800 + url.chars().count() as i64 / 256)
                    } else {
                        IMAGE_TOKENS_MAX
                    }
                }
                Some("file") => IMAGE_TOKENS_MAX,
                Some("text") => chars_div4(p.get("text").and_then(Value::as_str).unwrap_or("")),
                _ => 0,
            })
            .sum(),
        None => 0,
    }
}

fn chars_div4(s: &str) -> i64 {
    s.chars().count() as i64 / 4
}

/// 单条消息的 token 估算（对齐 Python `Message.tokens()`）：
/// 压缩过的消息按「当前长度 / 原始长度 × 原始 token」比例复原，其余按 content + 开销估算。
pub fn message_tokens(m: &Message) -> i64 {
    if m.compress_level >= 1 {
        if let (Some(len), Some(raw_tokens)) = (m.raw_len, m.raw_tokens) {
            let cur = content_tokens(m.content.as_ref());
            return 1.max((cur as f64 / len.max(1) as f64 * raw_tokens as f64).round() as i64);
        }
    }
    let mut n = content_tokens(m.content.as_ref()) + 12;
    if let Some(calls) = &m.tool_calls {
        n += chars_div4(&serde_json::to_string(calls).unwrap_or_default());
    }
    if let Some(r) = &m.reasoning_content {
        n += chars_div4(r);
    }
    n
}

/// 整个历史的 token 估算。
pub fn messages_tokens(messages: &[Message]) -> i64 {
    messages.iter().map(message_tokens).sum()
}

// ---------------------------------------------------------------- 落盘

/// 内容寻址：sha256 前 16 位（同内容只落一份）。
pub fn content_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())[..16].to_string()
}

/// 原文落盘：`<PIE_DIR>/context/<prefix>-<hash>.txt`（已存在则不动）。
pub fn write_raw(blob: &str, prefix: &str) -> std::io::Result<PathBuf> {
    let dir = context_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{prefix}-{}.txt", content_hash(blob)));
    if !path.exists() {
        std::fs::write(&path, blob)?;
    }
    Ok(path)
}

/// 追加一条压缩事件到 manifest（jsonl）。
pub fn write_manifest(manifest: &Path, entry: &Value) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = manifest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(manifest)?;
    writeln!(f, "{entry}")
}

/// 压缩事件（写进 manifest，也交给 `on_compact` 回调）。
///
/// `raw_hash` 取自**文件名**里的 hash（Python 同款）：它总是内容寻址得到的那段，
/// 而 shell 自带落盘的内容是 stdout 原文——若拿结果文本重算就对不上了。
fn compact_event(level: u8, kind: &str, path: &Path, summary: &str) -> Value {
    let mut entry = serde_json::json!({
        "ts": now_iso_utc(),
        "level": level,
        "kind": kind,
        "raw_path": path.display().to_string(),
        "raw_hash": raw_hash_of(path),
    });
    if !summary.is_empty() {
        entry["summary"] = Value::String(summary.chars().take(200).collect());
    }
    entry
}

/// `<prefix>-<hash>.txt` 里的 hash 段。
fn raw_hash_of(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.rsplit('-').next())
        .unwrap_or_default()
        .to_string()
}

/// 压缩事件的时间戳（`YYYY-MM-DDTHH:MM:SS` UTC）——时间换算统一在 `config`。
fn now_iso_utc() -> String {
    crate::config::iso_utc(crate::config::now_unix())
}

/// JSON 美化（缩进 1 空格，对齐 Python `json.dumps(..., indent=1)`）——落盘原文用。
fn pretty_indent1(value: &Value) -> String {
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    if value.serialize(&mut ser).is_err() {
        return String::new();
    }
    String::from_utf8(buf).unwrap_or_default()
}

// ---------------------------------------------------------------- 指针解析

/// 在文本里找 `[<前缀>已保存: <path>]` 形式的指针（不引 regex：格式是固定的）。
/// `prefixes` 传前缀名（不含 `[`），空前缀表示 `[…已保存:` 直接跟 `[`。
fn pointer_path(text: &str, prefixes: &[&str]) -> Option<PathBuf> {
    const SUFFIX: &str = "已保存: ";
    for (pos, _) in text.match_indices(SUFFIX) {
        let before = &text[..pos];
        let after = &text[pos + SUFFIX.len()..];
        let Some(end) = after.find(']') else { continue };
        let hit = before.ends_with('[')
            || prefixes
                .iter()
                .any(|p| before.ends_with(p) && before[..before.len() - p.len()].ends_with('['));
        if !hit {
            continue;
        }
        let path = after[..end].trim();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    None
}

/// 工具输出落盘指针（`[工具输出全文已保存: <path>]`）→ 路径。
///
/// 双保险（对齐 Python）：指针必然指向刚落盘的真实文件，文件不存在就当假指针——
/// 免得 `read` 回来的源码里恰好含这个格式的字符串被误判成压缩事件。
/// 调用方：`mark_tool_spill`（loop 拿到 shell 结果后同步压缩元数据）。
pub fn extract_spill_path(text: &str) -> Option<PathBuf> {
    let path = pointer_path(text, &["工具输出全文", "shell 输出全文"])?;
    path.exists().then_some(path)
}

// ---------------------------------------------------------------- 窗口块读取与摘要

/// 读落盘的窗口块 / 压缩原文：JSON 数组或 JSONL 都支持；纯文本（工具输出）→ 空。
pub fn load_window_dicts(path: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    if text.trim_start().starts_with('[') {
        return serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
    }
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.is_object())
        .collect()
}

/// 规则式会话摘要：保留开头 `head` 轮 + 末尾 `tail` 轮，每轮只留「用户输入 + 模型最终回复」，
/// 中间被省略的轮次显式标注，编号保留原始轮次序号（对齐 Python `summarize_turns`）。
pub fn summarize_turns(dicts: &[Value], head: usize, tail: usize) -> String {
    // 先切轮次：一个非 synthetic 的 user 开启一轮，其后的无 tool_calls 的 assistant 文本是「最终输出」
    let mut turns: Vec<(String, String)> = Vec::new();
    let mut q: Option<String> = None;
    let mut final_text = String::new();
    for d in dicts {
        let role = d.get("role").and_then(Value::as_str).unwrap_or("");
        let synthetic = d.get("synthetic").and_then(Value::as_bool).unwrap_or(false);
        let quote = |d: &Value| -> String {
            let content = d.get("content").cloned().unwrap_or(Value::Null);
            let content = content_from_value(content);
            let text = content_text(content.as_ref());
            if text.is_empty() {
                "[图片输入]".to_string()
            } else {
                text
            }
        };
        if role == "user" && !synthetic {
            if let Some(q0) = q.take() {
                turns.push((q0, std::mem::take(&mut final_text)));
            }
            q = Some(quote(d));
        } else if role == "assistant" && q.is_some() && d.get("tool_calls").is_none() {
            let content = d.get("content").cloned().unwrap_or(Value::Null);
            if !content.is_null() {
                final_text = content_text(content_from_value(content).as_ref());
            }
        }
    }
    if let Some(q0) = q.take() {
        turns.push((q0, final_text));
    }

    let total = turns.len();
    if total == 0 {
        return String::new();
    }
    let omitted = if tail > 0 && head + tail < total {
        total - head - tail
    } else {
        0
    };
    let parts: Vec<(Vec<(String, String)>, usize)> = if omitted > 0 {
        vec![
            (turns[..head].to_vec(), 1),
            (turns[total - tail..].to_vec(), total - tail + 1),
        ]
    } else {
        vec![(turns.clone(), 1)]
    };

    let mut lines: Vec<String> = Vec::new();
    for (i, (part, base)) in parts.iter().enumerate() {
        let mut prev: Option<(String, String)> = None;
        let mut idx = *base;
        for turn in part {
            if prev.as_ref() == Some(turn) {
                idx += 1; // 相邻重复合并，序号照旧推进（表示原始位置）
                continue;
            }
            prev = Some(turn.clone());
            lines.push(format!("{idx}. 用户: {}", turn.0));
            if !turn.1.is_empty() {
                lines.push(format!("   pie: {}", turn.1));
            }
            idx += 1;
        }
        if omitted > 0 && i == 0 {
            lines.push(format!("...[中间省略 {omitted} 轮]..."));
        }
    }
    lines.join("\n")
}

/// 把 JSON 形态的 content 还原成 `Content`（摘要/展示用）。
fn content_from_value(v: Value) -> Option<Content> {
    match v {
        Value::String(s) => Some(Content::Text(s)),
        Value::Array(parts) => Some(Content::Parts(parts)),
        _ => None,
    }
}

/// 历史窗口的摘要文本：`[历史窗口: <path>]` + 规则式轮次摘要。
pub fn build_window_summary(path: &Path, head: usize, tail: usize) -> String {
    let body = summarize_turns(&load_window_dicts(path), head, tail);
    let pointer = format!("{WINDOW_SUMMARY_MARKER} {}]", path.display());
    if body.is_empty() {
        format!("{pointer}\n\n（无可摘要内容）")
    } else {
        format!("{pointer}\n\n{body}")
    }
}

// ---------------------------------------------------------------- 消息构造辅助

fn text_message(role: &str, text: String) -> Message {
    Message {
        role: role.to_string(),
        content: Some(Content::Text(text)),
        ..Default::default()
    }
}

fn is_tool(m: &Message) -> bool {
    m.role == "tool"
}

fn is_user(m: &Message) -> bool {
    m.role == "user" && !m.synthetic
}

/// 压缩落盘后给消息打上「原文指针」元数据（对齐 Python `Message._set_raw`）。
fn set_raw(m: &mut Message, path: &Path, blob: &str) {
    m.raw_path = Some(path.display().to_string());
    m.raw_hash = Some(raw_hash_of(path));
    m.raw_len = Some(blob.chars().count() as i64);
    m.raw_tokens = Some(chars_div4(blob));
}

/// **工具自带落盘**（shell 超 `_max_lines`/`_max_bytes` 时写下的「全文已保存」指针）：
/// 把这条 tool 消息标成「已工具级压缩」（`compress_level=1` + `raw_*`），并返回给 manifest 的压缩事件。
/// 结果里没有真指针（或文件已不在）→ `None`（不动消息）。
///
/// ⚠ **只对 `shell` 调用**：read/edit/write 的结果文本里可能恰好含同样格式的字符串
/// （比如刚读进来的源码字面量），全局搜会误判成落盘事件（Python 同款 gate）。
/// 不标这一下会出真问题：那份 `shell-*.txt` 不会被 manifest 与 `raw_path` 引用，
/// `context gc` 会把它当垃圾删掉，历史里的指针就成了死链。
pub fn mark_tool_spill(msg: &mut Message, tool_name: &str, text: &str) -> Option<Value> {
    let path = extract_spill_path(text)?;
    msg.compress_level = 1;
    set_raw(msg, &path, text);
    let mut entry = compact_event(1, "tool", &path, "");
    entry["tool"] = Value::String(tool_name.to_string());
    Some(entry)
}

// ---------------------------------------------------------------- 三级压缩

/// step 批次（半开区间）：一次 `assistant(tool_calls)` + 其后的连续 tool 结果。
fn step_batches(flat: &[Message]) -> Vec<(usize, usize)> {
    let mut batches = Vec::new();
    let mut i = 0usize;
    while i < flat.len() {
        let m = &flat[i];
        if m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|c| !c.is_empty()) {
            let mut j = i + 1;
            while j < flat.len() && is_tool(&flat[j]) {
                j += 1;
            }
            batches.push((i, j));
            i = j;
        } else {
            i += 1;
        }
    }
    batches
}

/// 最近 `keep` 个 step 批次内 tool 消息的索引（跨轮次滚动保护）。
fn protected_step_tool_indices(flat: &[Message], keep: usize) -> HashSet<usize> {
    let batches = step_batches(flat);
    let keep = keep.max(1);
    let start = batches.len().saturating_sub(keep);
    let mut protected = HashSet::new();
    for (s, e) in &batches[start..] {
        protected.extend((*s..*e).filter(|&i| is_tool(&flat[i])));
    }
    protected
}

/// 工具级压缩：保护窗口之外、还没压过的 tool 消息（从最老开始）落盘成指针 + head/tail 预览。
/// 返回**真正落盘压缩了几条**（短输出不值得压 → 不计）。
fn compact_tools(
    messages: &mut [Message],
    cfg: &crate::config::ToolCompaction,
    keep: usize,
    cb: &mut dyn FnMut(Value),
) -> usize {
    let protected = protected_step_tool_indices(messages, keep);
    let mut count = 0usize;
    for (i, m) in messages.iter_mut().enumerate() {
        if !is_tool(m) || m.compress_level >= 1 || protected.contains(&i) {
            continue;
        }
        let Some(Content::Text(content)) = &m.content else {
            continue;
        };
        let content = content.clone();
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() <= cfg.head + cfg.tail {
            continue;
        }
        let prefix = m.tool_name.clone().unwrap_or_else(|| "tool".to_string());
        let Ok(path) = write_raw(&content, &prefix) else {
            continue;
        };
        let mut preview: Vec<&str> = lines[..cfg.head.min(lines.len())].to_vec();
        preview.push(TOOL_GAP);
        if cfg.tail > 0 {
            preview.extend_from_slice(&lines[lines.len().saturating_sub(cfg.tail)..]);
        }
        m.content = Some(Content::Text(format!(
            "[工具输出全文已保存: {}]\n\n{}",
            path.display(),
            preview.join("\n")
        )));
        m.compress_level = 1;
        set_raw(m, &path, &content);
        let tool = m.tool_name.clone().unwrap_or_default();
        let mut entry = compact_event(1, "tool", &path, "");
        entry["tool"] = Value::String(tool);
        cb(entry);
        count += 1;
    }
    count
}

/// 轮次级压缩：从最老开始压**已完成**的轮次（最后一个 user 之后是进行中，不压），
/// 直到降到 `target` 以下或没有可压的。返回压掉的轮数。
fn compact_turns(
    messages: &mut Vec<Message>,
    target: Option<i64>,
    cb: &mut dyn FnMut(Value),
) -> usize {
    let completed = messages
        .iter()
        .filter(|m| is_user(m))
        .count()
        .saturating_sub(1);
    let mut count = 0usize;
    for _ in 0..=completed {
        if let Some(t) = target {
            if messages_tokens(messages) <= t {
                break;
            }
        }
        // 每次重扫索引：切片替换会让后面的位置漂移，预算索引会误压进行中的轮次
        let user_idxs: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|&(_, m)| is_user(m))
            .map(|(i, _)| i)
            .collect();
        let mut victim: Option<(usize, usize)> = None;
        for k in 0..user_idxs.len().saturating_sub(1) {
            let (u, end) = (user_idxs[k], user_idxs[k + 1]);
            let span = &messages[u + 1..end];
            if span.is_empty() {
                continue; // 连续 user，没有内容
            }
            if span
                .iter()
                .all(|m| m.role == "assistant" && m.compress_level >= 2)
            {
                continue; // 已经轮次级压过
            }
            victim = Some((u, end));
            break;
        }
        let Some((u, end)) = victim else { break };
        let span: Vec<Message> = messages[u + 1..end].to_vec();
        let new_msg = compact_turn_span(&span, cb);
        messages.splice(u + 1..end, std::iter::once(new_msg));
        count += 1;
    }
    count
}

/// 把一轮的叶子压成摘要 assistant（user 保留在外层）：原文落盘 + `[轮次原文已保存: …]` + 最终输出。
fn compact_turn_span(span: &[Message], cb: &mut dyn FnMut(Value)) -> Message {
    let raw = pretty_indent1(&Value::Array(
        span.iter()
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect(),
    ));
    let path = write_raw(&raw, "turn").ok();
    let final_text = span
        .iter()
        .filter(|m| m.role == "assistant")
        .filter_map(|m| match &m.content {
            Some(Content::Text(t)) if !t.is_empty() => Some(t.clone()),
            _ => None,
        })
        .next_back()
        .unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    if span.len() > 1 {
        lines.push("...[中间过程省略]...".to_string());
    }
    lines.push(if final_text.is_empty() {
        "[该轮次无最终文本，原文已保存]".to_string()
    } else {
        final_text
    });
    let body = lines.join("\n");
    if let Some(path) = &path {
        cb(compact_event(2, "turn", path, &body));
    }
    // 落盘失败（极端情况）：只放摘要，不写假指针
    let content = match &path {
        Some(p) => format!("[轮次原文已保存: {}]\n\n{body}", p.display()),
        None => body,
    };
    let mut msg = text_message("assistant", content);
    msg.compress_level = 2;
    if let Some(p) = &path {
        set_raw(&mut msg, p, &raw);
    }
    msg
}

/// 会话级压缩：当前轮之前的整段历史落盘成新窗口块，插一条「摘要 + 指针」system 消息；
/// 旧窗口摘要（level 3）继续保留在上下文里，不重复归档。
fn compact_session(
    messages: &mut Vec<Message>,
    cfg: &crate::config::SessionCompaction,
    cb: &mut dyn FnMut(Value),
) -> bool {
    let Some(current_start) = messages.iter().rposition(is_user) else {
        return false;
    };
    if current_start <= 1 {
        return false; // 只有当前轮（或只有 system + 当前轮）
    }
    let head = &messages[1..current_start];
    let (summaries, old): (Vec<Message>, Vec<Message>) = head
        .iter()
        .cloned()
        .partition(|m| m.role == "system" && m.compress_level == 3);
    if old.is_empty() {
        return false;
    }
    let raw = pretty_indent1(&Value::Array(
        old.iter()
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect(),
    ));
    let Ok(path) = write_raw(&raw, "session") else {
        return false;
    };
    let summary = build_window_summary(&path, cfg.head, cfg.tail);
    let digest = summarize_turns(&load_window_dicts(&path), cfg.head, cfg.tail);
    cb(compact_event(3, "session", &path, &digest));
    let mut ptr = text_message("system", summary);
    ptr.compress_level = 3;
    set_raw(&mut ptr, &path, &raw);

    let tail = messages[current_start..].to_vec();
    let mut rebuilt = vec![messages[0].clone()];
    rebuilt.extend(summaries);
    rebuilt.push(ptr);
    rebuilt.extend(tail);
    *messages = rebuilt;
    true
}

// ---------------------------------------------------------------- 驱动

/// 压缩统计（`maybe_compact` / `compact` 共用同形状）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactStats {
    pub saved_tokens: i64,
    pub turns: usize,
    pub tools: usize,
    pub session: bool,
    /// 只有手动压缩在「未配置 compaction」时会带这个原因。
    pub skipped: Option<String>,
}

/// 按需压缩（自动）：**软阈值触发**，tools → turns → session，各级受对应子配置门控。
///
/// 会话级压缩产出的**窗口块路径**通过 `on_compact` 事件（`level=3, kind=session, raw_path`）
/// 回报——调用方（如会话层）据此登记；这里不再单开一个 `windows` 出参（Python 那条
/// `maybe_compact(..., windows=)` 链路实际没有调用方传值）。
pub fn maybe_compact(
    messages: &mut Vec<Message>,
    cfg: &Config,
    current_tokens: Option<i64>,
    on_compact: Option<&mut dyn FnMut(Value)>,
) -> CompactStats {
    let mut stats = CompactStats::default();
    let Some(compaction) = &cfg.compaction else {
        return stats;
    };
    if cfg.context_window <= 0 {
        return stats;
    }
    let tokens = current_tokens.unwrap_or_else(|| messages_tokens(messages));
    if tokens < cfg.soft_limit() {
        return stats;
    }
    let target = cfg.target_limit();
    let before = messages_tokens(messages);
    // 回调可选：None 时给个空实现，内部各级只管调（不再层层判 Option，也就没有重复可变借用）
    let mut noop = |_: Value| {};
    let cb: &mut dyn FnMut(Value) = match on_compact {
        Some(c) => c,
        None => &mut noop,
    };
    if let Some(tool_cfg) = &compaction.tool {
        stats.tools = compact_tools(messages, tool_cfg, cfg.keep_last_steps, &mut *cb);
    }
    if compaction.turn && messages_tokens(messages) > target {
        stats.turns = compact_turns(messages, Some(target), &mut *cb);
    }
    if let Some(session_cfg) = &compaction.session {
        if messages_tokens(messages) > target {
            stats.session = compact_session(messages, session_cfg, &mut *cb);
        }
    }
    stats.saved_tokens = (before - messages_tokens(messages)).max(0);
    if cfg.verbose && (stats.tools > 0 || stats.turns > 0 || stats.session) {
        crate::log::warn(format!(
            "[context] 压缩节省约 {} tokens（turns={}, tools={}, session={}）",
            stats.saved_tokens, stats.turns, stats.tools, stats.session
        ));
    }
    stats
}

/// 手动压缩模式（`/compact`）。
/// 手动压缩模式（`/compact`）。
///
/// ⚠ `Tools` / `Turns` 暂时没人构造：`/compact` 是交互式命令，等 TUI / REPL 落地时接线
/// （`compact()` 本身也是如此）；保留是为了与 Python 公共面（`context.compact` 的 mode）一致。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactMode {
    /// 工具级 + 轮次级
    Auto,
    Tools,
    Turns,
}

/// **手动**压缩（`/compact`）：不看水位，按 `mode` 压；轮次级一路压到不能再压。
/// 会话级（整窗口归档）不在手动范围内——那是 `/clear` 的事（与 Python 一致）。
#[allow(dead_code)]
pub fn compact(
    messages: &mut Vec<Message>,
    cfg: &Config,
    mode: CompactMode,
    on_compact: Option<&mut dyn FnMut(Value)>,
) -> CompactStats {
    let mut stats = CompactStats::default();
    let Some(compaction) = &cfg.compaction else {
        stats.skipped = Some("compaction disabled (未配置 [compaction])".to_string());
        return stats;
    };
    let before = messages_tokens(messages);
    let mut noop = |_: Value| {};
    let cb: &mut dyn FnMut(Value) = match on_compact {
        Some(c) => c,
        None => &mut noop,
    };
    if matches!(mode, CompactMode::Auto | CompactMode::Tools) {
        if let Some(tool_cfg) = &compaction.tool {
            stats.tools = compact_tools(messages, tool_cfg, cfg.keep_last_steps, &mut *cb);
        }
    }
    if matches!(mode, CompactMode::Auto | CompactMode::Turns) && compaction.turn {
        stats.turns = compact_turns(messages, None, &mut *cb);
    }
    stats.saved_tokens = (before - messages_tokens(messages)).max(0);
    stats
}

// ---------------------------------------------------------------- GC

/// 被引用的原文文件：所有 manifest 条目 + 所有会话消息里的 `raw_path`。
pub fn referenced_raw_paths() -> HashSet<PathBuf> {
    let mut refs: HashSet<PathBuf> = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(context_dir()) {
        for e in entries.flatten() {
            let p = e.path();
            if !p.to_string_lossy().ends_with(".manifest.jsonl") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p) {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    if let Ok(v) = serde_json::from_str::<Value>(line) {
                        if let Some(raw) = v.get("raw_path").and_then(Value::as_str) {
                            if let Some(abs) = absolutize(raw) {
                                refs.insert(abs);
                            }
                        }
                    }
                }
            }
        }
    }
    for session in std::fs::read_dir(pie_dir().join("sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
    {
        if let Ok(text) = std::fs::read_to_string(&session) {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    if v.get("__meta__").is_some() {
                        continue;
                    }
                    if let Some(raw) = v.get("raw_path").and_then(Value::as_str) {
                        if let Some(abs) = absolutize(raw) {
                            refs.insert(abs);
                        }
                    }
                }
            }
        }
    }
    refs
}

/// 相对路径按「当前目录」补全（与 Python `Path.resolve()` 同义，但不要求文件存在）。
fn absolutize(path: &str) -> Option<PathBuf> {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        return Some(p);
    }
    std::env::current_dir().ok().map(|cwd| cwd.join(p))
}

/// `context/` 下没有被任何会话引用的 `.txt`（可安全删除）。窗口块在 `windows/`，不受影响。
pub fn collect_context_garbage() -> Vec<PathBuf> {
    let referenced = referenced_raw_paths();
    let mut garbage: Vec<PathBuf> = std::fs::read_dir(context_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .filter(|p| !referenced.contains(p))
        .collect();
    garbage.sort();
    garbage
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CompactionConfig, SessionCompaction, ToolCompaction};
    use crate::llm::{FunctionCall, ToolCall};
    use serde_json::json;

    /// 测试会改进程级 `PIE_DIR` → 用全局锁把它们串行化（与 `llm` 的测试共用同一把锁）。
    /// 某个测试 panic 后锁会中毒（这不是错误，只是个测试挂了）→ 继续拿锁，别连锁失败。
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::config::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn pie_dir_tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pie-rs-ctx-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("PIE_DIR", &dir);
        dir
    }

    fn long_body(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    fn assistant_calls(id: &str, name: &str) -> Message {
        Message {
            role: "assistant".into(),
            tool_calls: Some(vec![ToolCall {
                id: id.into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: name.into(),
                    arguments: "{}".into(),
                },
            }]),
            ..Default::default()
        }
    }

    fn assistant_text(t: &str) -> Message {
        Message {
            role: "assistant".into(),
            content: Some(Content::Text(t.into())),
            ..Default::default()
        }
    }

    fn text_of(m: &Message) -> String {
        content_text(m.content.as_ref())
    }

    #[test]
    fn content_hash_and_pointer_round_trip() {
        let _g = env_lock();
        let dir = pie_dir_tmp("hash");
        let a = write_raw("same", "shell").unwrap();
        let b = write_raw("same", "shell").unwrap();
        assert_eq!(a, b, "同内容只落一份");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "same");
        // 真指针：文件在 → 认；把文件删了（或凭空写的字符串）→ 假指针，不算
        let text = format!("看这个 [工具输出全文已保存: {}]", a.display());
        assert_eq!(extract_spill_path(&text), Some(a.clone()));
        std::fs::remove_file(&a).unwrap();
        assert_eq!(extract_spill_path(&text), None);
        assert_eq!(extract_spill_path("没有指针"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 工具级：保护窗口外的长输出落盘成指针 + head/tail 预览，原文可完整取回。
    #[test]
    fn tool_level_spills_outside_keep_window_only() {
        let _g = env_lock();
        let dir = pie_dir_tmp("tool-level");
        let body = long_body(200);
        let mut msgs = vec![
            Message::system("s"),
            assistant_calls("c1", "shell"),
            Message::tool_result("c1", "shell", body.clone()),
            assistant_calls("c2", "shell"),
            Message::tool_result("c2", "shell", body.clone()),
        ];
        let mut events: Vec<Value> = Vec::new();
        let n = compact_tools(
            &mut msgs,
            &ToolCompaction { head: 2, tail: 2 },
            1,
            &mut |e| events.push(e),
        );
        assert_eq!(n, 1, "只压保护窗口（最近 1 个 step 批次）之外的那条");
        assert_eq!(msgs[4].compress_level, 0, "最近一批受保护");
        let compressed = &msgs[2];
        assert_eq!(compressed.compress_level, 1);
        let text = text_of(compressed);
        assert!(text.starts_with("[工具输出全文已保存: "), "{text}");
        assert!(
            text.contains("line 1") && text.contains("line 200"),
            "{text}"
        );
        assert!(text.contains(TOOL_GAP), "{text}");
        let raw = PathBuf::from(compressed.raw_path.clone().unwrap());
        assert!(raw.exists());
        assert_eq!(
            std::fs::read_to_string(&raw).unwrap(),
            body,
            "原文完整可回取"
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["level"], 1);
        assert_eq!(events[0]["kind"], "tool");
        assert_eq!(events[0]["tool"], "shell");
        // 短输出不值得压
        let mut short = vec![
            assistant_calls("c3", "shell"),
            Message::tool_result("c3", "shell", "ok"),
        ];
        assert_eq!(
            compact_tools(
                &mut short,
                &ToolCompaction { head: 2, tail: 2 },
                0,
                &mut |_| {}
            ),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 轮次级：user 保留、叶子压成一条摘要 assistant（带指针 + 最终输出）。
    #[test]
    fn turn_level_keeps_user_and_final_text() {
        let _g = env_lock();
        let dir = pie_dir_tmp("turn-level");
        let mut msgs = vec![
            Message::system("s"),
            Message::user("第一轮问题"),
            assistant_calls("c1", "shell"),
            Message::tool_result("c1", "shell", "out1"),
            assistant_text("第一轮答复"),
            Message::user("第二轮问题"),
            assistant_text("第二轮答复"),
        ];
        let mut events: Vec<Value> = Vec::new();
        let n = compact_turns(&mut msgs, None, &mut |e| events.push(e));
        assert_eq!(n, 1);
        assert_eq!(msgs.len(), 5, "3 条叶子 → 1 条摘要");
        assert_eq!(msgs[1].role, "user", "user 保留");
        assert_eq!(msgs[2].compress_level, 2);
        let text = text_of(&msgs[2]);
        assert!(text.starts_with("[轮次原文已保存: "), "{text}");
        assert!(text.contains("...[中间过程省略]..."), "{text}");
        assert!(text.contains("第一轮答复"), "{text}");
        assert!(PathBuf::from(msgs[2].raw_path.clone().unwrap()).exists());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["level"], 2);
        assert!(events[0]["summary"]
            .as_str()
            .unwrap()
            .contains("第一轮答复"));
        // 已经压过 → 没有可再压的
        assert_eq!(compact_turns(&mut msgs, None, &mut |_| {}), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 会话级：当前轮之前的历史落盘成窗口块，插一条 `[历史窗口: …]` 摘要 system 消息。
    #[test]
    fn session_level_archives_history_into_window() {
        let _g = env_lock();
        let dir = pie_dir_tmp("session-level");
        let mut msgs = vec![
            Message::system("当前提示词"),
            Message::user("旧问题"),
            assistant_text("旧答复"),
            Message::user("当前问题"),
        ];
        let mut events: Vec<Value> = Vec::new();
        let cfg = SessionCompaction { head: 1, tail: 1 };
        assert!(compact_session(&mut msgs, &cfg, &mut |e| events.push(e)));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["level"], 3);
        // 窗口块路径从事件里回报（不再单开出参）——调用方据此登记
        let window = PathBuf::from(events[0]["raw_path"].as_str().unwrap());
        assert!(window.exists());
        assert_eq!(msgs.len(), 3, "system + 窗口摘要 + 当前轮");
        assert_eq!(msgs[0].role, "system");
        assert_eq!(text_of(&msgs[0]), "当前提示词", "system 不动");
        assert_eq!(msgs[1].compress_level, 3);
        let text = text_of(&msgs[1]);
        assert!(text.starts_with(WINDOW_SUMMARY_MARKER), "{text}");
        assert!(text.contains("旧问题") && text.contains("旧答复"), "{text}");
        assert_eq!(msgs[2].role, "user");
        // 只剩窗口摘要 + 当前轮 → 没有再可归档的
        assert!(!compact_session(&mut msgs, &cfg, &mut |_| {}));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 自动压缩只在超过软阈值时触发（并水到目标水位以下）。
    #[test]
    fn maybe_compact_triggers_above_soft_limit_only() {
        let _g = env_lock();
        let dir = pie_dir_tmp("maybe");
        let cfg = Config {
            context_window: 400,
            reserved_tokens: None,
            verbose: false,
            keep_last_steps: 1,
            compaction: Some(CompactionConfig {
                tool: Some(ToolCompaction { head: 1, tail: 1 }),
                ..Default::default()
            }),
            ..Default::default()
        };

        // 短历史 → 不动
        let mut short = vec![Message::system("s"), Message::user("hi")];
        assert_eq!(
            maybe_compact(&mut short, &cfg, None, None),
            CompactStats::default()
        );

        // 一条巨长 tool 输出（≈ 5000 字符 → 远超 400×0.8）→ 触发工具级压缩
        let mut long = vec![
            Message::system("s"),
            assistant_calls("c1", "shell"),
            Message::tool_result("c1", "shell", long_body(2000)),
            assistant_calls("c2", "shell"),
            Message::tool_result("c2", "shell", "ok"),
        ];
        let stats = maybe_compact(&mut long, &cfg, Some(9999), None);
        assert_eq!(stats.tools, 1);
        assert!(stats.saved_tokens > 0, "{stats:?}");
        assert_eq!(long[2].compress_level, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 手动压缩：轮到不能再压；未配置 `[compaction]` 时给出 skipped 原因。
    #[test]
    fn manual_compact_reports_skipped_when_disabled() {
        let _g = env_lock();
        let dir = pie_dir_tmp("manual");
        let mut cfg = Config {
            compaction: None,
            ..Default::default()
        };
        let mut msgs = vec![Message::system("s"), Message::user("hi")];
        let stats = compact(&mut msgs, &cfg, CompactMode::Auto, None);
        assert!(stats.skipped.is_some());
        assert_eq!(stats.turns, 0);

        cfg.compaction = Some(CompactionConfig::default());
        cfg.keep_last_steps = 1; // 保护窗口只罩住最近 1 个 step 批次，好让更早的那批能被压
        let mut msgs = vec![
            Message::system("s"),
            Message::user("q1"),
            assistant_calls("c1", "shell"),
            Message::tool_result("c1", "shell", long_body(200)),
            assistant_text("a1"),
            Message::user("q2"),
            assistant_calls("c2", "shell"),
            Message::tool_result("c2", "shell", "ok"), // 最近一批：受 keep 保护
        ];
        let stats = compact(&mut msgs, &cfg, CompactMode::Auto, None);
        assert_eq!(stats.tools, 1, "压掉保护窗口之外那条长输出");
        assert_eq!(stats.turns, 1, "第一轮（已完成）压成摘要");
        assert!(stats.saved_tokens > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// shell 自带落盘（结果里带 spill 指针）→ 消息标成 `compress_level=1` + 落盘元数据 + 事件；
    /// 而且这份文件被 `raw_path`/manifest 引用 → `context gc` 不能把它当垃圾删。
    #[test]
    fn marks_shell_spill_so_gc_keeps_it() {
        let _g = env_lock();
        let dir = pie_dir_tmp("spill");
        let full = write_raw(&long_body(200), "shell").unwrap();
        let text = format!(
            "[exit=0]\n\n[工具输出全文已保存: {}]\n\nline 1\n...[中间省略]...\nline 200\n",
            full.display()
        );
        let mut msg = Message::tool_result("c1", "shell", text.clone());
        let entry = mark_tool_spill(&mut msg, "shell", &text).expect("有真指针 → 有事件");
        assert_eq!(msg.compress_level, 1);
        assert_eq!(entry["level"], 1);
        assert_eq!(entry["kind"], "tool");
        assert_eq!(entry["tool"], "shell");
        let raw = PathBuf::from(msg.raw_path.clone().unwrap());
        assert!(raw.exists());
        // 关键：manifest + raw_path 都引用了它 → GC 不能删
        write_manifest(&context_dir().join("s.manifest.jsonl"), &entry).unwrap();
        assert!(referenced_raw_paths().contains(&raw));
        assert!(!collect_context_garbage().contains(&raw));

        // 指针指向的文件不存在（如 read 回来的源码里恰好含这种字符串）→ 假指针，不动消息
        let fake = "[工具输出全文已保存: /tmp/pie-rs-definitely-missing.txt]";
        let mut m = Message::tool_result("c2", "shell", fake);
        assert!(mark_tool_spill(&mut m, "shell", fake).is_none());
        assert_eq!(m.compress_level, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn summarize_turns_keeps_head_tail_and_marks_gap() {
        let dicts: Vec<Value> = (1..=5)
            .flat_map(|i| {
                vec![
                    json!({"role": "user", "content": format!("问{i}")}),
                    json!({"role": "assistant", "content": format!("答{i}")}),
                ]
            })
            .collect();
        let s = summarize_turns(&dicts, 1, 1);
        assert!(s.contains("1. 用户: 问1"), "{s}");
        assert!(s.contains("   pie: 答1"), "{s}");
        assert!(s.contains("...[中间省略 3 轮]..."), "{s}");
        assert!(s.contains("5. 用户: 问5"), "{s}");
        assert!(!s.contains("问3"), "{s}");
        // 轮数不够时全留
        assert!(summarize_turns(&dicts, 10, 10).contains("问3"));
        assert_eq!(summarize_turns(&[], 1, 1), "");
    }

    #[test]
    fn gc_keeps_referenced_files_only() {
        let _g = env_lock();
        let dir = pie_dir_tmp("gc");
        let referenced = write_raw("referenced", "shell").unwrap();
        let orphan = write_raw("orphan", "turn").unwrap();
        let manifest = context_dir().join("s.manifest.jsonl");
        write_manifest(
            &manifest,
            &json!({"level": 1, "raw_path": referenced.display().to_string()}),
        )
        .unwrap();
        let garbage = collect_context_garbage();
        assert!(garbage.contains(&orphan), "{garbage:?}");
        assert!(!garbage.contains(&referenced), "{garbage:?}");
        assert!(referenced_raw_paths().contains(&referenced));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn iso_utc_matches_known_instants() {
        assert_eq!(crate::config::iso_utc(0), "1970-01-01T00:00:00");
        assert_eq!(crate::config::iso_utc(1_700_000_000), "2023-11-14T22:13:20");
    }
}
