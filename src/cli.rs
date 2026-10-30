//! 会话 / 图片文件的**列表与维护**：`pie sessions`、`pie files list|gc`、`pie context info|verify|gc`
//! 这些子命令的数据源。
//!
//! 为什么单独一处：`session.rs` 那边是**会话本身**（加载 / 落盘 / 回合循环 / 图片上传），
//! 这里则是「扫 `sessions/`、`files/`，按 id 找文件，算谁可以被回收」这类**只读磁盘的批量逻辑**——
//! 不改任何会话状态。
//!
//! ⚠ 模块名 `cli` 是**按用途**取的（命令行子命令的数据源），不是「只在 CLI 编译进来」：它一直在 lib 里，
//! TUI（同一个二进制）与 Python 绑定（`pie.list_sessions`）也都用它。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

/// 列出历史会话（按 `messages.jsonl` 的 mtime 降序；`limit = None` = 全部）。
///
/// ⚠ **mtime 取 `messages.jsonl`**，不能取会话目录：目录只在增删条目时变，改文件内容不动它。
/// 没有 `messages.jsonl` 的目录跳过（`save` 两次 rename 之间崩掉的残留）。
pub fn list_sessions(storage: &config::Storage, limit: Option<usize>) -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir(storage.sessions()) else {
        return Vec::new();
    };
    let mut dirs: Vec<(i64, PathBuf, u64)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|dir| {
            let meta = dir.join("messages.jsonl").metadata().ok()?;
            let mtime = meta
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs() as i64;
            Some((mtime, dir, meta.len()))
        })
        .collect();
    dirs.sort_by_key(|f| std::cmp::Reverse(f.0));
    if let Some(limit) = limit {
        dirs.truncate(limit);
    }
    dirs.into_iter()
        .map(|(mtime, dir, size)| {
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

/// 本地副本的 GC 保护窗口（小时）：比这新的未引用副本一概先留着。
///
/// 理由：“未被引用”不等于“没人用”—— 刚粘进 `files/` 的图在它被某次 `read` 登记进
/// `__meta__.files` 之前没有任何引用，但路径可能正躺在输入框 / 某条命令里。
pub const GC_PROTECT_HOURS: u64 = 24;

/// 遍历所有会话的图片条目：`(会话目录, hash_id, 条目)`。
///
/// 数据源是各会话的 `meta.json`（`files` 表）——**只读那一个文件**，不碰 `messages.jsonl`
/// （后者可能几十 MB：历史里把图内联成 base64 的那批）。
pub fn iter_session_files(sessions_dir: &Path) -> Vec<(PathBuf, String, Value)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return out;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let Ok(text) = std::fs::read_to_string(dir.join("meta.json")) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(map) = meta.get("files").and_then(Value::as_object) else {
            continue;
        };
        for (hash, entry) in map {
            if entry.is_object() {
                out.push((dir.clone(), hash.clone(), entry.clone()));
            }
        }
    }
    out
}

/// `file_id` → 记着它的会话名（`files list --all` 标“谁传的”用）。
pub fn file_id_index(storage: &config::Storage) -> HashMap<String, Vec<String>> {
    let mut index: HashMap<String, Vec<String>> = HashMap::new();
    for (file, _, entry) in iter_session_files(&storage.sessions()) {
        if let Some(id) = entry.get("file_id").and_then(Value::as_str) {
            index
                .entry(id.to_string())
                .or_default()
                .push(name_of(&file));
        }
    }
    index
}

/// `~/.pie/files/` 下没有被任何会话引用、且已经放了 `protect_hours` 小时的副本。
///
/// 副本是**跳会话共享**的（同一内容一个文件），所以“删会话”不会自动删副本 —— 回收靠这次
/// 无状态扫描；服务端那份由上传时的 `expires_after`（默认 30 天）自行过期。
pub fn collect_file_garbage(storage: &config::Storage, protect_hours: u64) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(storage.files()) else {
        return Vec::new();
    };
    let referenced: std::collections::HashSet<PathBuf> = iter_session_files(&storage.sessions())
        .into_iter()
        .filter_map(|(_, _, e)| e.get("local").and_then(Value::as_str).map(PathBuf::from))
        .collect();
    let now = config::now();
    let mut garbage: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            !p.file_name()
                .map(|n| n.to_string_lossy().starts_with('.'))
                .unwrap_or(false)
        })
        .filter(|p| !referenced.contains(p))
        .filter(|p| {
            // 保护窗口内 → 留着
            p.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|mtime| now.saturating_sub(mtime).as_secs() >= protect_hours * 3600)
        })
        .collect();
    garbage.sort();
    garbage
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Storage;

    /// 图片条目从 `meta.json` 读：**不碰 `messages.jsonl`**（后者可能几十 MB）。
    /// 消息文件坏了也不该让这个会话的图片记录消失。
    #[test]
    fn listing_reads_only_the_meta_so_a_broken_messages_file_is_harmless() {
        let dir = std::env::temp_dir().join(format!("pie-cli-first-line-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::at(&dir);
        let session = storage.session_dir("s");
        std::fs::create_dir_all(&session).unwrap();
        let meta = serde_json::json!({
            "files": {"img-x": {"file_id": "f1", "local": "/tmp/x.png"}}
        });
        std::fs::write(session.join("meta.json"), meta.to_string()).unwrap();
        // 消息文件：尾部不是合法 UTF-8
        let mut bytes = b"{\"role\":\"user\",\"content\":\"hi\"}\n".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe, 0x00, b'\n']);
        std::fs::write(session.join("messages.jsonl"), bytes).unwrap();

        let rows = iter_session_files(&storage.sessions());
        assert_eq!(rows.len(), 1, "meta 有效就够：{rows:?}");
        assert_eq!(rows[0].1, "img-x");
    }

    /// `files gc` 的判据：**既没人引用、又过了保护窗**才回收。
    ///
    /// 用 `Storage::at(临时目录)` 注入（不动进程级 `PIE_HOME`、也不用环境锁）。
    #[test]
    fn garbage_needs_no_reference_and_past_protect_window() {
        let dir = std::env::temp_dir().join(format!("pie-cli-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::at(&dir);
        let blob = |data: &[u8]| {
            storage
                .store(
                    None,
                    config::StoreType::Blob {
                        data,
                        mime: "image/png",
                    },
                )
                .expect("落副本")
        };
        std::fs::create_dir_all(storage.sessions()).unwrap();
        let referenced = blob(b"referenced");
        let orphan_old = blob(b"orphan-old");
        let orphan_fresh = blob(b"orphan-fresh");
        // 把孤儿副本的 mtime 拨到 2 天前（超出保护窗）
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&orphan_old)
            .unwrap()
            .set_modified(old)
            .unwrap();
        // 一个会话的 meta 引用 referenced
        let session = storage.session_dir("s");
        std::fs::create_dir_all(&session).unwrap();
        let meta = serde_json::json!({
            "files": {"img-x": {"file_id": "f1", "local": referenced.display().to_string()}}
        });
        std::fs::write(session.join("meta.json"), meta.to_string()).unwrap();
        std::fs::write(session.join("messages.jsonl"), "").unwrap();

        // 会话记录（`files list` 的数据源）读得出来，file_id 索引也建得出
        let rows = iter_session_files(&storage.sessions());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "img-x");
        assert!(file_id_index(&storage).contains_key("f1"));

        let garbage = collect_file_garbage(&storage, GC_PROTECT_HOURS);
        assert!(garbage.contains(&orphan_old), "{garbage:?}");
        assert!(!garbage.contains(&referenced), "被会话引用 → 不能收");
        assert!(!garbage.contains(&orphan_fresh), "保护窗口内 → 不能收");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
