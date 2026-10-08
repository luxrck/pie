//! 上下文管理层：三级压缩（工具级 / 轮次级 / 会话级）+ 原文落盘指针。
//! （`context/` 的维护——引用扫描与垃圾判定——在 [`crate::cli`]，与图片副本那份同类。）
//!
//! 消息是扁平 `Vec<Message>`（没有类层级），靠 `role` + `compaction` + `synthetic` 判定，
//! 三种压缩都以「就地改写消息列表」实现：
//!   - **工具级**（level 1）：`keep_last_steps` 个 step 批次**之外**的 tool 输出，行数超过
//!     `head+tail` 就全文落盘 + 头部/尾部留预览，内容换成 `[工具输出全文已保存: <path>]` 指针；
//!   - **轮次级**（level 2）：已完成的轮次（最后一个 user 之前的）压成一条摘要 assistant
//!     （user 保留），原文落盘成 `[轮次原文已保存: <path>]`，摘要只留模型最终输出；
//!   - **会话级**（level 3）：当前轮之前的整段历史落盘成**窗口块**
//!     （`~/.pie/context/session-<hash>`；被会话 meta 的 `compaction_events` 引用，GC 不碰。`/clear` 归档的窗口块
//!     另在 `~/.pie/windows/`，见 [`crate::config::Storage::windows`]），
//!     插一条 `[历史窗口: <path>]` 摘要 system 消息，旧窗口摘要继续留在上下文里；
//!   - 压缩级别**只升不降**；落盘按内容 hash 寻址（同内容只存一份）。
//!
//! 两个驱动入口：`maybe_compact`（自动，**只看 API 上报的水位**：上一次 `usage.prompt_tokens`
//! ≥「可用输入预算」的 `soft_ratio` 就压，各级压到不能再压）与 `compact`（手动 `/compact`，不看水位）。
//!
//! **不做 token 估算**：水位只有两个来源 —— 服务端上报的 `prompt_tokens`（准），以及
//! 「还没有任何上报」（首次请求前）时**不压**。单条消息的有界由工具层负责
//! （`read` / `bash` 的 `_max_lines` / `_max_bytes` + 超限落盘），服务端预检算最后一道。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, Storage, StoreType};
use crate::llm::{Compaction, Content, Message};

// ---------------------------------------------------------------- 目录与常量
//
// 磁盘布局（`context_dir` / `windows_dir`）住 `config`——一处定义，别处只管用。

/// 窗口摘要消息的开头标记（便于识别）。
pub const WINDOW_SUMMARY_MARKER: &str = "[历史窗口:";

/// 工具级压缩时中间省略的标记。
const TOOL_GAP: &str = "...[中间省略]...";

// ---------------------------------------------------------------- 落盘
//
// 落盘本身住 `config::Storage`（`store()`：内容寻址 + 原子写都在那儿）——这里只管
// 「什么时候写、写哪个前缀」，即 `context/`（压缩原文）与 `windows/`（窗口块）两个目录的用法。

/// 一次压缩的记录（`Session.compaction_events` 的一条，随会话落盘）。
///
/// 类型由 variant 决定，落盘写成 `kind` 标签（`#[serde(tag = "kind")]`）——不再另存冗余的
/// `level`（要数字用 [`CompactEvent::level`]）；旧会话里多出来的 `level` 键被 serde 忽略，照读。
///
/// 字段名不带 `raw_` 前缀（`path` / `hash`）：事件本身已经说明这是压缩件。旧会话里叫
/// `raw_path` / `raw_hash` → `#[serde(alias)]` 兜住。
///
/// `hash` 取自**文件名**里的 hash：它总是内容寻址得到的那段，
/// 而 shell 自带落盘的内容是 stdout 原文——若拿结果文本重算就对不上了。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CompactEvent {
    /// 工具级（level 1）：工具输出落盘。`tool` 是触发落盘的工具名。
    Tool {
        ts: i64,
        tool: String,
        #[serde(alias = "raw_path")]
        path: PathBuf,
        #[serde(alias = "raw_hash")]
        hash: String,
    },
    /// 轮次级（level 2）：已完成的轮次压成摘要。
    Turn {
        ts: i64,
        #[serde(alias = "raw_path")]
        path: PathBuf,
        #[serde(alias = "raw_hash")]
        hash: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        summary: String,
    },
    /// 会话级（level 3）：整段历史落盘成窗口块。
    Session {
        ts: i64,
        #[serde(alias = "raw_path")]
        path: PathBuf,
        #[serde(alias = "raw_hash")]
        hash: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        summary: String,
    },
}

impl CompactEvent {
    /// 工具级事件（level 1）。
    pub fn tool(tool: &str, path: &Path) -> Self {
        Self::Tool {
            ts: crate::config::now().as_secs() as i64,
            tool: tool.to_string(),
            path: path.to_path_buf(),
            hash: crate::config::hash_of(path),
        }
    }

    /// 轮次级事件（level 2）。摘要只留前 200 字。
    pub fn turn(path: &Path, summary: &str) -> Self {
        Self::Turn {
            ts: crate::config::now().as_secs() as i64,
            path: path.to_path_buf(),
            hash: crate::config::hash_of(path),
            summary: summary.chars().take(200).collect(),
        }
    }

    /// 会话级事件（level 3）。摘要只留前 200 字。
    pub fn session(path: &Path, summary: &str) -> Self {
        Self::Session {
            ts: crate::config::now().as_secs() as i64,
            path: path.to_path_buf(),
            hash: crate::config::hash_of(path),
            summary: summary.chars().take(200).collect(),
        }
    }

    /// 压缩级别（1 工具级 / 2 轮次级 / 3 会话级，与 `Message.compaction` 的变体同口径）。
    pub fn level(&self) -> u8 {
        match self {
            Self::Tool { .. } => 1,
            Self::Turn { .. } => 2,
            Self::Session { .. } => 3,
        }
    }

    /// 落盘原文的路径。
    pub fn raw_path(&self) -> &Path {
        match self {
            Self::Tool { path, .. } | Self::Turn { path, .. } | Self::Session { path, .. } => path,
        }
    }
}

/// JSON 美化（缩进 1 空格）——落盘原文 / `--mode transcript` 用。
pub fn pretty_indent1(value: &Value) -> String {
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    if value.serialize(&mut ser).is_err() {
        return String::new();
    }
    String::from_utf8(buf).unwrap_or_default()
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
/// 中间被省略的轮次显式标注，编号保留原始轮次序号。
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
            let text = Content::from_value(content)
                .map(|c| c.text())
                .unwrap_or_default();
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
                final_text = Content::from_value(content)
                    .map(|c| c.text())
                    .unwrap_or_default();
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

/// 历史窗口的摘要文本：`[历史窗口: <path>]` + 规则式轮次摘要（自己读盘）。
pub fn build_window_summary(path: &Path, head: usize, tail: usize) -> String {
    let body = summarize_turns(&load_window_dicts(path), head, tail);
    window_summary_text(path, &body)
}

/// 摘要正文就绪时拼「指针 + 正文」——`build_window_summary`（自己读盘算）与归档路径
/// （digest 已经在手上，不必再读一遍盘、算一遍摘要）共用。
fn window_summary_text(path: &Path, body: &str) -> String {
    let pointer = format!("{WINDOW_SUMMARY_MARKER} {}]", path.display());
    if body.is_empty() {
        format!("{pointer}\n\n（无可摘要内容）")
    } else {
        format!("{pointer}\n\n{body}")
    }
}

/// **会话级压缩（第三级，level 3）**：把当前窗口的历史（`messages[1..]`，旧窗口摘要除外）
/// 写进 `~/.pie/windows/` 成一块窗口块，并按「system + 各窗口摘要」重开窗口。
/// 返回（新块路径, 流水事件）；没有可归档的内容 → `Ok(None)`；落盘失败 → `Err`（调用方**不动窗口**）。
///
/// ⚠ **只有用户手动触发**（`Session::clear_window` ← `/clear`）：换窗口会让当前轮的工作记忆
/// （刚读的文件、跑的命令）只剩摘要，自动做会让模型莫名「失忆」—— 所以自动压缩只做
/// 工具级（level 1）/ 轮次级（level 2），这一级留给用户。
///
/// 旧窗口摘要（`system` + level 3）不入档（否则「摘要的摘要」层层嵌套），而是留在窗口里
/// —— 重开后的摘要链是连续的一段。
pub fn compact_session(
    messages: &mut Vec<Message>,
    config: &crate::config::SessionCompaction,
    storage: &Storage,
) -> std::io::Result<Option<(PathBuf, CompactEvent)>> {
    // 先算成 owned（下面要整体改写 messages，不能再借它）
    let (dicts, old_summaries): (Vec<Value>, Vec<Message>) = {
        let span = &messages[1..];
        let archivable: Vec<Value> = span
            .iter()
            .filter(|m| {
                !(m.role == "system" && matches!(m.compaction, Some(Compaction::Session { .. })))
            })
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect();
        let kept: Vec<Message> = span
            .iter()
            .filter(|m| {
                m.role == "system" && matches!(m.compaction, Some(Compaction::Session { .. }))
            })
            .cloned()
            .collect();
        (archivable, kept)
    };
    if dicts.is_empty() {
        return Ok(None);
    }
    // 块一律 pretty JSON 数组（与存量块同格式；`load_window_dicts` 两种都读）
    let raw = pretty_indent1(&Value::Array(dicts.clone()));
    let path = storage.store(StoreType::Window(&raw))?;
    // 摘要直接拿手上的 dicts 算，不再读一遍盘
    let digest = summarize_turns(&dicts, config.head, config.tail);
    let mut summary = Message::system(window_summary_text(&path, &digest));
    summary.compaction = Some(Compaction::session(&path));

    // 重开窗口：system 不动，旧摘要留着（不入档），后面接新摘要
    let mut rebuilt: Vec<Message> = Vec::with_capacity(2 + old_summaries.len());
    rebuilt.push(messages[0].clone());
    rebuilt.extend(old_summaries);
    rebuilt.push(summary);
    *messages = rebuilt;
    let event = CompactEvent::session(&path, &digest);
    Ok(Some((path, event)))
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
            while j < flat.len() && flat[j].role == "tool" {
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
        protected.extend((*s..*e).filter(|&i| flat[i].role == "tool"));
    }
    protected
}

/// 工具级压缩：保护窗口之外、还没压过的 tool 消息（从最老开始）落盘成指针 + head/tail 预览。
/// 返回**真正落盘压缩了几条**（短输出不值得压 → 不计）。
fn compact_tools(
    messages: &mut [Message],
    config: &crate::config::ToolCompaction,
    storage: &Storage,
    cb: &mut dyn FnMut(CompactEvent),
) -> usize {
    let protected = protected_step_tool_indices(messages, config.keep_last_steps);
    let mut count = 0usize;
    for (i, m) in messages.iter_mut().enumerate() {
        if m.role != "tool" || m.compaction.is_some() || protected.contains(&i) {
            continue;
        }
        // ⚠ 只压**纯文本**：`Content::Parts` 的 tool 消息（`read` 读到图、图挂在 content 上那种）
        // 正文就一行标记，本来就短到不值得压 —— 先跳过。哪天出现「长正文 + 图」的 parts 消息，
        // 在这里支持即可（把 text part 拿出来做头尾预览 + 落盘，其余 part 原样接回去）。
        let Some(Content::Text(content)) = &m.content else {
            continue;
        };
        let content = content.clone();
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() <= config.head + config.tail {
            continue;
        }
        let prefix = m.tool_name.clone().unwrap_or_else(|| "tool".to_string());
        let Ok(path) = storage.store(StoreType::Raw {
            prefix: &prefix,
            body: &content,
        }) else {
            continue;
        };
        let mut preview: Vec<&str> = lines[..config.head.min(lines.len())].to_vec();
        preview.push(TOOL_GAP);
        if config.tail > 0 {
            preview.extend_from_slice(&lines[lines.len().saturating_sub(config.tail)..]);
        }
        m.content = Some(Content::Text(format!(
            "[工具输出全文已保存: {}]\n\n{}",
            path.display(),
            preview.join("\n")
        )));
        m.compaction = Some(Compaction::tool(&path));
        let tool = m.tool_name.clone().unwrap_or_default();
        cb(CompactEvent::tool(&tool, &path));
        count += 1;
    }
    count
}

/// 轮次级压缩：从最老开始压**已完成**的轮次（最后一个 user 之后是进行中，不压），
/// 一路压到没有可压的。返回压掉的轮数。
fn compact_turns(
    messages: &mut Vec<Message>,
    storage: &Storage,
    cb: &mut dyn FnMut(CompactEvent),
) -> usize {
    let completed = messages
        .iter()
        .filter(|m| m.role == "user" && !m.synthetic)
        .count()
        .saturating_sub(1);
    let mut count = 0usize;
    for _ in 0..=completed {
        // 每次重扫索引：切片替换会让后面的位置漂移，预算索引会误压进行中的轮次
        let user_idxs: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|&(_, m)| m.role == "user" && !m.synthetic)
            .map(|(i, _)| i)
            .collect();
        let mut victim: Option<(usize, usize)> = None;
        for k in 0..user_idxs.len().saturating_sub(1) {
            let (u, end) = (user_idxs[k], user_idxs[k + 1]);
            let span = &messages[u + 1..end];
            if span.is_empty() {
                continue; // 连续 user，没有内容
            }
            if span.iter().all(|m| {
                m.role == "assistant" && matches!(m.compaction, Some(Compaction::Turn { .. }))
            }) {
                continue; // 已经轮次级压过
            }
            victim = Some((u, end));
            break;
        }
        let Some((u, end)) = victim else { break };
        let span: Vec<Message> = messages[u + 1..end].to_vec();
        let new_msg = compact_turn_span(&span, storage, cb);
        messages.splice(u + 1..end, std::iter::once(new_msg));
        count += 1;
    }
    count
}

/// 把一轮的叶子压成摘要 assistant（user 保留在外层）：原文落盘 + `[轮次原文已保存: …]` + 最终输出。
fn compact_turn_span(
    span: &[Message],
    storage: &Storage,
    cb: &mut dyn FnMut(CompactEvent),
) -> Message {
    let raw = pretty_indent1(&Value::Array(
        span.iter()
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect(),
    ));
    let path = storage
        .store(StoreType::Raw {
            prefix: "turn",
            body: &raw,
        })
        .ok();
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
        cb(CompactEvent::turn(path, &body));
    }
    // 落盘失败（极端情况）：只放摘要，不写假指针
    let content = match &path {
        Some(p) => format!("[轮次原文已保存: {}]\n\n{body}", p.display()),
        None => body,
    };
    let mut msg = Message::assistant(content);
    msg.compaction = Some(Compaction::turn(path.as_deref()));
    msg
}

// ---------------------------------------------------------------- 驱动

/// 压缩统计（`maybe_compact` / `compact` 共用同形状）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactStats {
    pub turns: usize,
    pub tools: usize,
    /// 只有手动压缩在「未配置 compaction」时会带这个原因。
    pub skipped: Option<String>,
}

/// 按需压缩（自动）：**只看 API 上报的水位** —— `reported`（上一次 `usage.prompt_tokens`）
/// 达到软阈值就压：工具级 → 轮次级，各级压到不能再压；没有上报（首次请求前）就不动。
///
/// **第三级（会话级）不在这里**：那一级只由用户手动 `/clear` 触发（见 [`compact_session`]）——
/// 自动压缩只做工具级 / 轮次级（换表示：原文落盘 + 指针/摘要，窗口不动）。
///
/// 产出的事件**随返回值交给调用方**：会话层拿去登记 `windows` / 落进 `__meta__.compaction_events`。
/// 不走 `on_event` —— 事件是内部记账（gc 靠它保护原文），不是展示流。
pub fn maybe_compact(
    messages: &mut Vec<Message>,
    config: &Config,
    reported: Option<i64>,
) -> (CompactStats, Vec<CompactEvent>) {
    let mut stats = CompactStats::default();
    let mut events: Vec<CompactEvent> = Vec::new();
    let Some(compaction) = &config.compaction else {
        return (stats, events);
    };
    if config.context_window == 0 {
        return (stats, events);
    }
    let Some(tokens) = reported else {
        return (stats, events); // 还没有任何 API 上报（本次会话第一次请求前）→ 不压
    };
    if tokens < config.soft_limit() as i64 {
        return (stats, events);
    }
    let mut push = |e| events.push(e);
    if let Some(tool_config) = &compaction.tool {
        stats.tools = compact_tools(messages, tool_config, &config.storage, &mut push);
    }
    if compaction.turn {
        stats.turns = compact_turns(messages, &config.storage, &mut push);
    }
    if stats.tools > 0 || stats.turns > 0 {
        crate::log::warn(format!(
            "[context] 水位 {} 超软阈值 {} → 压缩（tools={}, turns={}）",
            tokens,
            config.soft_limit(),
            stats.tools,
            stats.turns
        ));
    }
    (stats, events)
}

/// 手动压缩模式（`/compact`）：`Auto` = 工具级 + 轮次级；`Tools` / `Turns` 只做对应那一级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactMode {
    /// 工具级 + 轮次级
    Auto,
    Tools,
    Turns,
}

/// **手动**压缩（`/compact`）：不看水位，按 `mode` 压；轮次级一路压到不能再压。
/// 会话级（整窗口归档）不在手动范围内——那是 `/clear` 的事。
pub fn compact(
    messages: &mut Vec<Message>,
    config: &Config,
    mode: CompactMode,
) -> (CompactStats, Vec<CompactEvent>) {
    let mut stats = CompactStats::default();
    let mut events: Vec<CompactEvent> = Vec::new();
    let Some(compaction) = &config.compaction else {
        stats.skipped = Some("compaction disabled (未配置 [compaction])".to_string());
        return (stats, events);
    };
    let mut push = |e| events.push(e);
    if matches!(mode, CompactMode::Auto | CompactMode::Tools)
        && let Some(tool_config) = &compaction.tool
    {
        stats.tools = compact_tools(messages, tool_config, &config.storage, &mut push);
    }
    if matches!(mode, CompactMode::Auto | CompactMode::Turns) && compaction.turn {
        stats.turns = compact_turns(messages, &config.storage, &mut push);
    }
    (stats, events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CompactionConfig, SessionCompaction, ToolCompaction};
    use crate::llm::{FunctionCall, ToolCall};
    use serde_json::json;

    /// 测试用数据目录：跟随进程级 `PIE_DIR`（`pie_dir_tmp` + `env_lock` 那套）。
    fn storage() -> Storage {
        Storage::default()
    }

    /// 造一个会话文件：首行 `__meta__`（带 `compaction_events`）+ 给定消息。
    ///
    /// `referenced_raw_paths` 现在只扫 `sessions/`：流水在首行、指针在消息里，两者同一份文件。
    fn write_session_with_events(
        storage: &Storage,
        events: &[serde_json::Value],
        messages: &[Message],
    ) {
        let dir = storage.sessions();
        std::fs::create_dir_all(&dir).unwrap();
        let mut out = format!(
            "{}\n",
            json!({"__meta__": true, "compaction_events": events})
        );
        for m in messages {
            out.push_str(&format!("{}\n", serde_json::to_string(m).unwrap()));
        }
        std::fs::write(dir.join("s.jsonl"), out).unwrap();
    }

    /// 落一段压缩原文 → 路径（`StoreType::Raw` 的薄包装，只为测试读起来短）。
    fn raw(body: &str, prefix: &str) -> PathBuf {
        storage().store(StoreType::Raw { prefix, body }).unwrap()
    }

    /// 测试会改进程级 `PIE_DIR` → 用全局锁把它们串行化（与 `llm` 的测试共用同一把锁）。
    /// 某个测试 panic 后锁会中毒（这不是错误，只是个测试挂了）→ 继续拿锁，别连锁失败。
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::config::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn pie_dir_tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pie-ctx-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::config::set_env("PIE_DIR", &dir);
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

    #[test]
    fn content_hash_dedupes_identical_blobs() {
        let _g = env_lock();
        let dir = pie_dir_tmp("hash");
        let a = raw("same", "bash");
        let b = raw("same", "bash");
        assert_eq!(a, b, "同内容只落一份");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "same");
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
            assistant_calls("c1", "bash"),
            Message::tool_result("c1", "bash", body.clone()),
            assistant_calls("c2", "bash"),
            Message::tool_result("c2", "bash", body.clone()),
        ];
        let mut events: Vec<CompactEvent> = Vec::new();
        let n = compact_tools(
            &mut msgs,
            &ToolCompaction {
                head: 2,
                tail: 2,
                keep_last_steps: 1,
            },
            &storage(),
            &mut |e| events.push(e),
        );
        assert_eq!(n, 1, "只压保护窗口（最近 1 个 step 批次）之外的那条");
        assert_eq!(msgs[4].compaction, None, "最近一批受保护");
        let compressed = &msgs[2];
        assert!(matches!(
            compressed.compaction,
            Some(Compaction::Tool { .. })
        ));
        let text = compressed.content_text();
        assert!(text.starts_with("[工具输出全文已保存: "), "{text}");
        assert!(
            text.contains("line 1") && text.contains("line 200"),
            "{text}"
        );
        assert!(text.contains(TOOL_GAP), "{text}");
        let raw = PathBuf::from(compressed.compaction.as_ref().unwrap().path().unwrap());
        assert!(raw.exists());
        assert_eq!(
            std::fs::read_to_string(&raw).unwrap(),
            body,
            "原文完整可回取"
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level(), 1);
        match &events[0] {
            CompactEvent::Tool { tool, .. } => assert_eq!(tool, "bash"),
            other => panic!("期望工具级事件：{other:?}"),
        }
        // 短输出不值得压
        let mut short = vec![
            assistant_calls("c3", "bash"),
            Message::tool_result("c3", "bash", "ok"),
        ];
        assert_eq!(
            compact_tools(
                &mut short,
                &ToolCompaction::default(),
                &storage(),
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
            assistant_calls("c1", "bash"),
            Message::tool_result("c1", "bash", "out1"),
            Message::assistant("第一轮答复"),
            Message::user("第二轮问题"),
            Message::assistant("第二轮答复"),
        ];
        let mut events: Vec<CompactEvent> = Vec::new();
        let n = compact_turns(&mut msgs, &storage(), &mut |e| events.push(e));
        assert_eq!(n, 1);
        assert_eq!(msgs.len(), 5, "3 条叶子 → 1 条摘要");
        assert_eq!(msgs[1].role, "user", "user 保留");
        assert!(matches!(msgs[2].compaction, Some(Compaction::Turn { .. })));
        let text = msgs[2].content_text();
        assert!(text.starts_with("[轮次原文已保存: "), "{text}");
        assert!(text.contains("...[中间过程省略]..."), "{text}");
        assert!(text.contains("第一轮答复"), "{text}");
        assert!(PathBuf::from(msgs[2].compaction.as_ref().unwrap().path().unwrap()).exists());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level(), 2);
        match &events[0] {
            CompactEvent::Turn { summary, .. } => {
                assert!(summary.contains("第一轮答复"), "{summary}")
            }
            other => panic!("期望轮次级事件：{other:?}"),
        }
        // 已经压过 → 没有可再压的
        assert_eq!(compact_turns(&mut msgs, &storage(), &mut |_| {}), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 会话级：当前轮之前的历史落盘成窗口块，插一条 `[历史窗口: …]` 摘要 system 消息。
    #[test]
    /// 第三级（会话级，[`compact_session`]）：整窗口落盘，当前轮**也**进归档，窗口重开成 system + 摘要。
    fn compact_session_spills_the_whole_window() {
        let _g = env_lock();
        let dir = pie_dir_tmp("archive-window");
        let mut msgs = vec![
            Message::system("当前提示词"),
            Message::user("旧问题"),
            Message::assistant("旧答复"),
            Message::user("当前问题"),
        ];
        let config = SessionCompaction { head: 1, tail: 1 };
        let (path, event) = compact_session(&mut msgs, &config, &storage())
            .expect("落盘成功")
            .expect("有内容可归档");
        assert!(path.exists(), "{path:?}");
        assert_eq!(event.level(), 3);
        assert_eq!(event.raw_path(), path);
        // 窗口重开：system + 一条摘要（当前轮也被归档 → 摘要里）
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content_text(), "当前提示词", "system 不动");
        assert!(matches!(
            msgs[1].compaction,
            Some(Compaction::Session { .. })
        ));
        let text = msgs[1].content_text();
        assert!(text.starts_with(WINDOW_SUMMARY_MARKER), "{text}");
        assert!(
            text.contains("旧问题") && text.contains("当前问题"),
            "{text}"
        );
        // 原文都在落盘件里（能从指针回取）
        let raw = std::fs::read_to_string(&path).unwrap();
        for needle in ["旧问题", "旧答复", "当前问题"] {
            assert!(raw.contains(needle), "块里丢了 {needle}");
        }
        // 只剩 system + 旧摘要 → 没有再可归档的
        let again = compact_session(&mut msgs, &config, &storage()).unwrap();
        assert!(again.is_none(), "只剩 system + 旧摘要 → 没内容可归档");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 自动压缩只在 API 上报的水位超过软阈值时触发，并且没有上报（首次请求前）时不动。
    #[test]
    fn maybe_compact_triggers_above_soft_limit_only() {
        let _g = env_lock();
        let dir = pie_dir_tmp("maybe");
        let config = Config {
            context_window: 400,
            reserved_tokens: None,
            compaction: Some(CompactionConfig {
                tool: Some(ToolCompaction {
                    head: 1,
                    tail: 1,
                    keep_last_steps: 1,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        // 没有上报（本次会话还没发过请求）→ 不动
        let mut short = vec![Message::system("s"), Message::user("hi")];
        let (stats, events) = maybe_compact(&mut short, &config, None);
        assert_eq!(stats, CompactStats::default());
        assert!(events.is_empty());

        // 有上报但没到软阈值（400×0.8 = 320）→ 也不动
        let (stats, _) = maybe_compact(&mut short, &config, Some(10));
        assert_eq!(stats, CompactStats::default());

        // 一条巨长 tool 输出（≈ 5000 字符 → 远超 400×0.8）→ 触发工具级压缩
        let mut long = vec![
            Message::system("s"),
            assistant_calls("c1", "bash"),
            Message::tool_result("c1", "bash", long_body(2000)),
            assistant_calls("c2", "bash"),
            Message::tool_result("c2", "bash", "ok"),
        ];
        let (stats, events) = maybe_compact(&mut long, &config, Some(9999));
        assert_eq!(stats.tools, 1);
        assert_eq!(events.len(), 1, "工具级事件也从这里回报");
        assert!(matches!(long[2].compaction, Some(Compaction::Tool { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A+：自动压缩只做工具级 / 轮次级；第三级（整窗口归档）不在这里
    /// （只由用户 `/clear` 触发）—— 即使配了 `[compaction.session]` 也不归档。
    #[test]
    fn auto_compaction_never_archives_the_window() {
        let _g = env_lock();
        let dir = pie_dir_tmp("auto-no-session");
        let config = Config {
            context_window: 400,
            reserved_tokens: None,
            compaction: Some(CompactionConfig::default()),
            ..Default::default()
        };
        let mut msgs = vec![
            Message::system("s"),
            Message::user("q1"),
            Message::assistant("a1"),
            Message::user("q2"),
        ];
        let (_, events) = maybe_compact(&mut msgs, &config, Some(9999));
        assert!(events.iter().all(|e| e.level() < 3), "{events:?}");
        assert!(
            msgs.iter()
                .all(|m| !matches!(m.compaction, Some(Compaction::Session { .. }))),
            "{msgs:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 手动压缩：轮到不能再压；未配置 `[compaction]` 时给出 skipped 原因。
    #[test]
    fn manual_compact_reports_skipped_when_disabled() {
        let _g = env_lock();
        let dir = pie_dir_tmp("manual");
        let mut config = Config {
            compaction: None,
            ..Default::default()
        };
        let mut msgs = vec![Message::system("s"), Message::user("hi")];
        let (stats, events) = compact(&mut msgs, &config, CompactMode::Auto);
        assert!(stats.skipped.is_some());
        assert_eq!(stats.turns, 0);
        assert!(events.is_empty());

        config.compaction = Some(CompactionConfig {
            // 保护窗口只罩住最近 1 个 step 批次，好让更早的那批能被压
            tool: Some(ToolCompaction {
                keep_last_steps: 1,
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut msgs = vec![
            Message::system("s"),
            Message::user("q1"),
            assistant_calls("c1", "bash"),
            Message::tool_result("c1", "bash", long_body(200)),
            Message::assistant("a1"),
            Message::user("q2"),
            assistant_calls("c2", "bash"),
            Message::tool_result("c2", "bash", "ok"), // 最近一批：受 keep 保护
        ];
        let (stats, events) = compact(&mut msgs, &config, CompactMode::Auto);
        assert_eq!(stats.tools, 1, "压掉保护窗口之外那条长输出");
        assert_eq!(stats.turns, 1, "第一轮（已完成）压成摘要");
        assert_eq!(events.len(), 2, "工具级 + 轮次级各一条");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 工具自带落盘（`ToolOutput.spill`）→ 消息**构造时**就带上 `compaction` 元数据，
    /// 并由会话层记一条压缩流水；两者都引用那份文件 → `context gc` 不能把它当垃圾删。
    #[test]
    fn shell_spill_is_set_on_the_message_and_gc_keeps_it() {
        let _g = env_lock();
        let dir = pie_dir_tmp("spill");
        let full = raw(&long_body(200), "bash");
        // 这正是 `aturn` 回填工具结果时做的事：标工具级元数据，顺手产一条事件
        let mut msg = Message::tool_result("c1", "bash", "line 1\n...\nline 200\n");
        msg.compaction = Some(Compaction::tool(&full));
        assert_eq!(
            msg.compaction,
            Some(Compaction::tool(&full)),
            "落盘即工具级"
        );
        assert_eq!(
            msg.compaction.as_ref().and_then(Compaction::path),
            Some(full.to_str().unwrap())
        );
        // 没落盘 → 不标
        let plain = Message::tool_result("c2", "read", "x");
        assert!(plain.compaction.is_none());

        let entry = CompactEvent::tool("bash", &full);
        assert_eq!(entry.level(), 1);
        match &entry {
            CompactEvent::Tool { tool, .. } => assert_eq!(tool, "bash"),
            other => panic!("期望工具级事件：{other:?}"),
        }
        // 关键：压缩流水 + raw_path 都引用了它 → GC 不能删
        // （流水住在**会话文件首行**：`__meta__.compaction_events`，跟消息同一趟车落盘）
        let entry_json = serde_json::to_value(&entry).unwrap();
        write_session_with_events(&storage(), std::slice::from_ref(&entry_json), &[]);
        assert!(crate::cli::referenced_raw_paths(&storage()).contains(&full));
        assert!(!crate::cli::collect_context_garbage(&storage()).contains(&full));
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
}
