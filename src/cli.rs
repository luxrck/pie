//! 会话 / 图片文件的**列表与维护**：`pie sessions`、`pie files list|gc`、`pie context info|verify|gc`
//! 这些子命令的数据源。
//!
//! 为什么单独一处：`session.rs` 那边是**会话本身**（加载 / 落盘 / 回合循环 / 图片上传），
//! 这里则是「扫 `sessions/`、`files/`，按 id 找文件，算谁可以被回收」这类**只读磁盘的批量逻辑**——
//! 不改任何会话状态。
//!
//! ⚠ 模块名 `cli` 是**按用途**取的（命令行子命令的数据源），不是「只在 CLI 编译进来」：它一直在 lib 里，
//! TUI（同一个二进制）与 Python 绑定（`pie.list_sessions`）也都用它。

use std::path::PathBuf;

use serde_json::Value;

use crate::config::{self, name_of};

/// 历史会话概览（`pie sessions` 用）。
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub path: PathBuf,
    /// 文件 mtime（unix 秒）——列表按它降序。
    pub mtime: i64,
    pub size: u64,
    /// 真实用户消息数（`synthetic` 的图片消息不算）。
    pub turns: usize,
    /// `__meta__.usage.calls`（累计 API 请求数）。
    pub api_calls: i64,
    /// 标题（`__meta__.title`），没有就用首个用户消息。
    pub first_query: String,
}

/// 读 `<dir>/meta.json` 里的 `parent`（`None` = 主会话 / 读不出来）。
fn meta_parent(dir: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("meta.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("parent")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// 列出历史会话（按 `messages.jsonl` 的 mtime 降序；`limit = None` = 全部）。
///
/// ⚠ **mtime 取 `messages.jsonl`**，不能取会话目录：目录只在增删条目时变，改文件内容不动它。
/// 没有 `messages.jsonl` 的目录跳过（`save` 两次 rename 之间崩掉的残留）。
pub fn list_sessions(
    storage: &config::Storage,
    limit: Option<usize>,
    include_children: bool,
) -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir(storage.sessions()) else {
        return Vec::new();
    };
    // 排序用 `SystemTime`（**不是秒**：同一秒里连开两个会话，秒级排序会任意选）
    let mut dirs: Vec<(std::time::SystemTime, PathBuf, u64)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        // 子会话（`meta.parent` 非空 = 某个工具开的）默认不列：它们不是"跟用户的对话"
        .filter(|p| include_children || meta_parent(p).is_none())
        .filter_map(|dir| {
            let meta = dir.join("messages.jsonl").metadata().ok()?;
            let mtime = meta.modified().ok()?;
            Some((mtime, dir, meta.len()))
        })
        .collect();
    dirs.sort_by_key(|f| std::cmp::Reverse(f.0));
    if let Some(limit) = limit {
        dirs.truncate(limit);
    }
    dirs.into_iter()
        .map(|(mtime, dir, size)| {
            let mtime = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let mut turns = 0usize;
            let mut api_calls = 0i64;
            let mut first_query = String::new();
            // 元数据在 `meta.json`（`sessions -l` 只需要它 + 消息里的用户条数）
            if let Ok(text) = std::fs::read_to_string(dir.join("meta.json"))
                && let Ok(meta) = serde_json::from_str::<Value>(&text)
            {
                api_calls = meta
                    .get("usage")
                    .and_then(|u| u.get("calls"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if let Some(t) = meta.get("title").and_then(Value::as_str) {
                    first_query = t.to_string();
                }
            }
            if let Ok(text) = std::fs::read_to_string(dir.join("messages.jsonl")) {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    let Ok(v) = serde_json::from_str::<Value>(line) else {
                        continue;
                    };
                    if v.get("role").and_then(Value::as_str) == Some("user")
                        && !v.get("synthetic").and_then(Value::as_bool).unwrap_or(false)
                    {
                        turns += 1;
                        if first_query.is_empty()
                            && let Some(c) = v.get("content").and_then(Value::as_str)
                        {
                            first_query = c.trim().to_string();
                        }
                    }
                }
            }
            SessionInfo {
                id: name_of(&dir),
                path: dir,
                mtime,
                size,
                turns,
                api_calls,
                first_query,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Storage;

    /// `sessions -l` 的数据源：一个会话 = 一个目录，`id` = 目录名；列表按 **`messages.jsonl`
    /// 的 mtime** 排（目录 mtime 只在增删条目时变，改内容不动它）。
    #[test]
    fn sessions_are_directories_sorted_by_messages_mtime() {
        let dir = std::env::temp_dir().join(format!("pie-cli-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::at(&dir);
        let write = |id: &str, title: &str| {
            let session = storage.session_dir(id);
            std::fs::create_dir_all(&session).unwrap();
            std::fs::write(
                session.join("meta.json"),
                serde_json::json!({"usage": {"calls": 2}, "title": title}).to_string(),
            )
            .unwrap();
            std::fs::write(
                session.join("messages.jsonl"),
                "{\"role\":\"user\",\"content\":\"hi\"}\n",
            )
            .unwrap();
            session
        };
        write("a", "先的");
        std::thread::sleep(std::time::Duration::from_millis(20));
        write("b", "后的");
        // 没有 messages.jsonl 的目录 = 崩溃残留 → 跳过
        std::fs::create_dir_all(storage.session_dir("half-saved")).unwrap();

        let rows = list_sessions(&storage, None, false);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["b", "a"], "按时间倒序、跳过没有 messages 的目录");
        assert_eq!(rows[0].first_query, "后的");
        assert_eq!(rows[0].api_calls, 2);
        assert_eq!(rows[0].turns, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
