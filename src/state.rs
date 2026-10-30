//! 会话级状态：**一会话一份**的东西放这儿（工具的长活对象 + 会话自己的派生数据）。
//!
//! 为什么合成一个容器：这些东西的共同点是「只在一次会话里有效、跨回合活着」，而读者很分散
//! —— 工具经 `ToolCtx.state` 拿（`repl` 的宿主进程、`read`/`edit` 的读快照），会话自己也要
//! （`repl_blocks` 拼 system prompt 那节）。按类型分区的槽表让「再加一样」不必动 `Session`
//! 的字段列表，也不必碰别的持有者。
//!
//! ⚠ **落盘的键仍写在 `Session::save` / `load` 里**（`meta["state"]["repl"]`）：会话文件是对外
//! 契约，不该随「表里有什么」漂移。目前只有 `repl` 与插件的裸 JSON 两种条目，所以**没有**
//! 注册表 / `Persisted` trait 那一套 —— 等第二个内置条目出现再抽。
//!
//! 槽的语义是「**没有就懒建一个**」（`get_or_init`），所以每个使用者只管自己那一格；
//! 容器身份就是那个 `Arc<SessionState>`（共享就 clone 它，没有 `Clone` 实现）。

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `repl_blocks` 最多留这么多个（超出丢最老的）——同时限住 `meta["state"]["repl"]` 与那节 system prompt。
pub const REPL_BLOCKS_MAX: usize = 24;
/// 单个 cell 存进日志/显示的字符上限（超了截断 + 提示看原文）。
pub const REPL_BLOCK_MAX_CHARS: usize = 2000;

/// 会话级状态的槽表：按类型分区、懒建。
#[derive(Default)]
pub struct SessionState {
    slots: Mutex<HashMap<TypeId, Box<dyn Any + Send + Sync>>>,
}

impl SessionState {
    /// 取本会话里类型 `T` 的共享状态；没有就用 `init` 建一个（**仅第一次**）。
    pub fn get_or_init<T: Send + Sync + 'static>(&self, init: impl FnOnce() -> T) -> Arc<T> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(Arc::new(init())))
            .downcast_ref::<Arc<T>>()
            .expect("会话状态槽的类型不一致")
            .clone()
    }

    /// `repl` 的会话状态（代码块 + 画布图索引）：`repl` 攒、system prompt 那节与画布读。
    pub fn repl(&self) -> Arc<ReplState> {
        self.get_or_init(ReplState::default)
    }

    /// 插件工具的裸 JSON（`meta["state"][<工具名>]`）：形状由工具自己定，Rust 只存取。
    pub fn tool_states(&self) -> Arc<ToolStates> {
        self.get_or_init(ToolStates::default)
    }
}

impl std::fmt::Debug for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionState").finish_non_exhaustive()
    }
}

/// 解释器跑过的单个代码块（`meta["state"]["repl"].blocks` 的一条）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReplBlock {
    /// 工具调用的 id（判「它还在不在当前对话里」就靠它）。
    pub id: String,
    /// 那次 `repl` 的 `code` 参数（逐字；过长的按字符截断并标一句）。
    pub code: String,
}

/// `repl` 的会话状态：**代码块日志** + **画布图索引**，一起落在 `meta["state"]["repl"]`：
///
/// ```jsonc
/// "repl": { "blocks": [ {"id": "call_1", "code": "…"} ],
///           "images": { "call_7": ["/…/files/img-3f9a….png"] } }
/// ```
///
/// 代码块为什么得单独存一份：压缩会把 `tool_calls` 从消息里删掉（轮次级把整段 span 换成摘要、
/// 会话级把整窗口归档），等要把代码摆进 system prompt 那节时消息里已经查不到了。所以它比
/// 消息活得久（每轮开头扫一遍消息里**还没记过**的 `repl` 调用、按 call id 去重）。
/// 而 system prompt 那节的内容由它派生：**只列 id 已不在 `messages` 里的那些** —— 两次压缩
/// 之间逐字不变（前缀缓存照旧命中），只有真被压走东西的那一刻才变。
///
/// 画布图（`Attachment::Canvas`）只给界面看、**不上传**，所以它不属于 `Session.files`
/// （那是"要送给模型"的上传缓存）；画布回放靠这里 `call_id → 本地路径`。
#[derive(Default)]
pub struct ReplState {
    blocks: Mutex<Vec<ReplBlock>>,
    images: Mutex<BTreeMap<String, Vec<String>>>,
}

impl ReplState {
    /// 追加一批代码块（按 `id` 去重；超 [`REPL_BLOCKS_MAX`] 丢最老的）。
    pub fn record(&self, fresh: Vec<ReplBlock>) {
        let mut blocks = self.blocks.lock().unwrap_or_else(|e| e.into_inner());
        for block in fresh {
            if !blocks.iter().any(|b| b.id == block.id) {
                blocks.push(block);
            }
        }
        if blocks.len() > REPL_BLOCKS_MAX {
            let cut = blocks.len() - REPL_BLOCKS_MAX;
            blocks.drain(..cut); // 丢最老的
        }
    }

    /// 代码块快照（落盘、拼 system prompt 都读它）。
    pub fn snapshot(&self) -> Vec<ReplBlock> {
        self.blocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 记一次调用产出的画布图（**按 call 去重追加**：同一次调用可能出多张）。
    pub fn record_images(&self, call_id: &str, paths: &[std::path::PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let mut images = self.images.lock().unwrap_or_else(|e| e.into_inner());
        let entry = images.entry(call_id.to_string()).or_default();
        for path in paths {
            let text = path.display().to_string();
            if !entry.contains(&text) {
                entry.push(text);
            }
        }
    }

    /// 画布索引快照（`-r` 回放用：`call_id → 本地路径`）。
    pub fn images(&self) -> BTreeMap<String, Vec<String>> {
        self.images
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 从 `meta["state"]["repl"]` 装回（`-r` 读的那份）。
    pub fn load(&self, blocks: Vec<ReplBlock>, images: BTreeMap<String, Vec<String>>) {
        *self.blocks.lock().unwrap_or_else(|e| e.into_inner()) = blocks;
        *self.images.lock().unwrap_or_else(|e| e.into_inner()) = images;
    }

    /// 清空（`reset`：历史没了，这些引用也就成了噪声）。
    pub fn clear(&self) {
        self.blocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.images
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

/// 插件工具写进来的**裸 JSON**：`meta["state"][<工具名>]`。
///
/// 形状由工具自己定（`S.state` 是个 dict）——Rust 侧只负责**存与取**，不解释、不校验：
/// 键就是工具名，值是那次调用返回时交回来的整个对象。
///
/// 为什么不需要 `Persisted` / 注册表：它不是 Rust 类型，是宿主帧送来的 JSON；而"哪些内置条目
/// 要落盘"目前只有 `repl` 一个，直接写在 `Session::save` / `load` 里更直白。
#[derive(Default)]
pub struct ToolStates(Mutex<BTreeMap<String, Value>>);

/// 内置条目的键 —— 工具条目的键不能撞它们（工具名本来就唯一，这里只防"工具叫 repl"）。
pub const RESERVED_STATE_KEYS: [&str; 1] = ["repl"];

impl ToolStates {
    /// 一次调用结束时的提交（宿主帧带回来的 `state`）：**整条覆盖** —— 形状是工具自己的，
    /// 合并语义只能它自己定。
    pub fn commit(&self, tool: &str, value: Value) {
        if RESERVED_STATE_KEYS.contains(&tool) {
            crate::log::warn(format!(
                "[state] 工具名 {tool} 撞了内置状态键，这次写入被丢弃"
            ));
            return;
        }
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tool.to_string(), value);
    }

    /// 取某个工具的当前值（下发调用帧时用）。
    pub fn get(&self, tool: &str) -> Option<Value> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(tool)
            .cloned()
    }

    /// 快照（落盘用）：`(工具名, 值)`。
    pub fn snapshot(&self) -> Vec<(String, Value)> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// 从 `meta["state"]` 装回：**除内置键**之外的都是工具条目（坏形状当空）。
    pub fn load(&self, state: &Value) {
        let Some(map) = state.as_object() else {
            return;
        };
        let mut out = self.0.lock().unwrap_or_else(|e| e.into_inner());
        out.clear();
        for (key, value) in map {
            if RESERVED_STATE_KEYS.contains(&key.as_str()) {
                continue;
            }
            out.insert(key.clone(), value.clone());
        }
    }

    /// 清空（`reset`）。
    pub fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(id: &str) -> ReplBlock {
        ReplBlock {
            id: id.to_string(),
            code: format!("code-{id}"),
        }
    }

    /// 去重 + 上限：重复的 id 不再进，超 24 个丢最老的。
    #[test]
    fn record_dedupes_by_id_and_caps_the_log() {
        let log = ReplState::default();
        log.record(vec![block("a"), block("b")]);
        log.record(vec![block("b"), block("c")]); // b 重复
        let got: Vec<String> = log.snapshot().into_iter().map(|b| b.id).collect();
        assert_eq!(got, ["a", "b", "c"]);

        for i in 0..40 {
            log.record(vec![block(&format!("x{i}"))]);
        }
        let blocks = log.snapshot();
        assert_eq!(blocks.len(), REPL_BLOCKS_MAX);
        assert_eq!(blocks.last().unwrap().id, "x39", "留最新的");
    }

    /// 槽表按类型分区：两种类型互不干扰，`get_or_init` 只建一次。
    #[test]
    fn slots_are_partitioned_by_type() {
        let state = SessionState::default();
        let a = state.get_or_init(|| 1u32);
        let b = state.get_or_init(|| "x".to_string());
        assert_eq!(*a, 1);
        assert_eq!(*b, "x");
        assert!(
            Arc::ptr_eq(&a, &state.get_or_init(|| 2u32)),
            "第二次拿到同一个"
        );
        assert!(Arc::ptr_eq(&state.repl(), &state.repl()));
    }
}
