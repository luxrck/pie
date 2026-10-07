//! 会话 / 图片文件的**列表与维护**：`pie sessions`、`pie files list|gc`、`pie context info|verify|gc`
//! 这些子命令的数据源。
//!
//! 为什么单独一处：`session.rs` 那边是**会话本身**（加载 / 落盘 / 回合循环 / 图片上传），
//! 这里则是「扫 `sessions/`、`files/`，按 id 找文件，算谁可以被回收」这类**只读磁盘的批量逻辑**——
//! 不改任何会话状态。
//!
//! ⚠ 模块名 `cli` 是**按用途**取的（命令行子命令的数据源），不是「只在 CLI 编译进来」：它一直在 lib 里，
//! TUI（同一个二进制）与 Python 绑定（`pie.list_sessions`）也都用它。

use std::collections::{HashMap, HashSet};
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

/// 列出历史会话（按 mtime 降序；`limit = None` = 全部）。
pub fn list_sessions(storage: &config::Storage, limit: Option<usize>) -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir(storage.sessions()) else {
        return Vec::new();
    };
    let mut files: Vec<(i64, PathBuf, u64)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| {
            let meta = p.metadata().ok()?;
            let mtime = meta
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs() as i64;
            Some((mtime, p, meta.len()))
        })
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    if let Some(limit) = limit {
        files.truncate(limit);
    }
    files
        .into_iter()
        .map(|(mtime, path, size)| {
            let mut turns = 0usize;
            let mut api_calls = 0i64;
            let mut first_query = String::new();
            if let Ok(text) = std::fs::read_to_string(&path) {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    let Ok(v) = serde_json::from_str::<Value>(line) else {
                        continue;
                    };
                    if v.get("__meta__").is_some() {
                        api_calls = v
                            .get("usage")
                            .and_then(|u| u.get("calls"))
                            .and_then(Value::as_i64)
                            .unwrap_or(0);
                        if let Some(t) = v.get("title").and_then(Value::as_str) {
                            first_query = t.to_string();
                        }
                    } else if v.get("role").and_then(Value::as_str) == Some("user")
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
                id: name_of(&path),
                path,
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

/// 遍历所有会话记录里的图片条目：`(会话文件, hash_id, 条目)`。
pub fn iter_session_files(sessions_dir: &Path) -> Vec<(PathBuf, String, Value)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return out;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        // meta 是第一行，读完就够
        let Some(first) = text.lines().find(|l| !l.trim().is_empty()) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<Value>(first) else {
            continue;
        };
        if !meta
            .get("__meta__")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let Some(map) = meta.get("files").and_then(Value::as_object) else {
            continue;
        };
        for (hash, entry) in map {
            if entry.is_object() {
                out.push((file.clone(), hash.clone(), entry.clone()));
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

/// 被引用的**压缩原文**：所有会话的 `__meta__.compaction_events` + 所有消息里的 `compaction.path`。
///
/// 两者都在**会话文件**里（同一趟车落盘），所以这里只扫 `sessions/`。
pub fn referenced_raw_paths(storage: &config::Storage) -> HashSet<PathBuf> {
    let mut refs: HashSet<PathBuf> = HashSet::new();
    let Ok(entries) = std::fs::read_dir(storage.sessions()) else {
        return refs;
    };
    for session in entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
    {
        let Ok(text) = std::fs::read_to_string(&session) else {
            continue;
        };
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if v.get("__meta__").is_some() {
                // 首行 `__meta__`：压缩事件流水（`compaction_events`）里的指针也算引用
                if let Some(events) = v.get("compaction_events").and_then(Value::as_array) {
                    for e in events {
                        // 新键是 `path`；旧会话里叫 `raw_path`（这里裸读 `Value`，不走 serde 的 alias）
                        let raw = e
                            .get("path")
                            .or_else(|| e.get("raw_path"))
                            .and_then(Value::as_str);
                        if let Some(raw) = raw
                            && let Some(abs) = absolutize(raw)
                        {
                            refs.insert(abs);
                        }
                    }
                }
                continue;
            }
            // 消息里的压缩元数据：`{"compaction":{"kind":"turn","path":…}}`
            // （轮次级落盘失败时没有 `path`——那种消息本来就没引用什么）
            let raw = v
                .get("compaction")
                .and_then(|c| c.get("path"))
                .and_then(Value::as_str);
            if let Some(raw) = raw
                && let Some(abs) = absolutize(raw)
            {
                refs.insert(abs);
            }
        }
    }
    refs
}

/// 相对路径按「当前目录」补全（不要求文件存在）。
fn absolutize(path: &str) -> Option<PathBuf> {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        return Some(p);
    }
    std::env::current_dir().ok().map(|cwd| cwd.join(p))
}

/// `context/` 下没有被任何会话引用的落盘原文（可安全删除）。窗口块在 `windows/`，不受影响。
pub fn collect_context_garbage(storage: &config::Storage) -> Vec<PathBuf> {
    /// 是不是 `Storage::store` 落的那种文件：`<前缀>-<16 位十六进制 hash>`。
    /// 拿它当「只回收自己写的东西」的判据——同一个目录里还可能有别的文件，别误删。
    fn is_stored(path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        let Some((_, hash)) = name.rsplit_once('-') else {
            return false;
        };
        hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())
    }

    let referenced = referenced_raw_paths(storage);
    let mut garbage: Vec<PathBuf> = std::fs::read_dir(storage.context())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_stored(p))
        .filter(|p| !referenced.contains(p))
        .collect();
    garbage.sort();
    garbage
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Storage;

    /// `files gc` 的判据：**既没人引用、又过了保护窗**才回收。
    ///
    /// 用 `Storage::at(临时目录)` 注入（不动进程级 `PIE_DIR`、也不用环境锁）。
    #[test]
    fn garbage_needs_no_reference_and_past_protect_window() {
        let dir = std::env::temp_dir().join(format!("pie-cli-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::at(&dir);
        let blob = |data: &[u8]| {
            storage
                .store(config::StoreType::Blob {
                    data,
                    mime: "image/png",
                })
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
        let meta = serde_json::json!({
            "__meta__": true,
            "files": {"img-x": {"file_id": "f1", "local": referenced.display().to_string()}}
        });
        std::fs::write(storage.sessions().join("s.jsonl"), format!("{meta}\n")).unwrap();

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

    /// 造一个会话文件：首行 `__meta__`（带 `compaction_events`）+ 给定消息。
    fn write_session_with_events(
        storage: &Storage,
        events: &[Value],
        messages: &[crate::llm::Message],
    ) {
        let dir = storage.sessions();
        std::fs::create_dir_all(&dir).unwrap();
        let mut out = format!(
            "{}\n",
            serde_json::json!({"__meta__": true, "compaction_events": events})
        );
        for m in messages {
            out.push_str(&format!("{}\n", serde_json::to_string(m).unwrap()));
        }
        std::fs::write(dir.join("s.jsonl"), out).unwrap();
    }

    /// `context gc` 的判据：`context/` 下没被任何会话引用的落盘原文才回收。
    ///
    /// （旧键名 `raw_path` + 旧 `level` 也要认——会话文件是裸读 `Value`，不走 serde 的 alias。）
    #[test]
    fn context_gc_keeps_referenced_files_only() {
        let dir = std::env::temp_dir().join(format!("pie-cli-ctx-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Storage::at(&dir);
        std::fs::create_dir_all(storage.sessions()).unwrap();
        let put = |prefix: &str, body: &str| {
            storage
                .store(config::StoreType::Raw { prefix, body })
                .unwrap()
        };
        let referenced = put("bash", "referenced");
        let in_message = put("turn", "in-message");
        let orphan = put("turn", "orphan");
        write_session_with_events(
            &storage,
            &[serde_json::json!({"level": 1, "raw_path": referenced.display().to_string()})],
            &[{
                // 消息侧的引用来自 `compaction.path`（这里抽查轮次级那条路子）
                let mut m = crate::llm::Message::assistant("摘要");
                m.compaction = Some(crate::llm::Compaction::turn(Some(&in_message)));
                m
            }],
        );
        let garbage = collect_context_garbage(&storage);
        assert!(garbage.contains(&orphan), "{garbage:?}");
        assert!(!garbage.contains(&referenced), "{garbage:?}");
        assert!(
            !garbage.contains(&in_message),
            "消息里的指针也算引用：{garbage:?}"
        );
        assert!(referenced_raw_paths(&storage).contains(&referenced));
        assert!(referenced_raw_paths(&storage).contains(&in_message));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
