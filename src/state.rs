//! 会话级状态：**一会话一份**的东西放这儿（工具的长活对象 + 会话自己的派生数据）。
//!
//! 为什么合成一个容器：这些东西的共同点是「只在一次会话里有效、跨回合活着」，而读者很分散
//! —— 工具经 `ToolCtx.state` 拿（`repl` 的宿主进程、`read`/`edit` 的读快照），会话自己也要
//! （`repl_blocks` 拼 system prompt 那节）。按类型分区的槽表让「再加一样」不必动 `Session`
//! 的字段列表，也不必碰别的持有者。
//!
//! ⚠ **落盘的键仍写在 `Session::save` / `load` 里**（`__meta__.repl_blocks`）：会话文件是对外
//! 契约，不该随「表里有什么」漂移。目前只有一个可落盘条目（[`ReplBlocks`]），所以**没有**
//! 注册表 / `Persisted` trait 那一套 —— 等第二个出现再抽。
//!
//! 槽的语义是「**没有就懒建一个**」（`get_or_init`），所以每个使用者只管自己那一格；
//! 容器身份就是那个 `Arc<SessionState>`（共享就 clone 它，没有 `Clone` 实现）。

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// `repl_blocks` 最多留这么多个（超出丢最老的）——同时限住 `__meta__` 与那节 system prompt。
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

    /// 解释器跑过的代码块——唯一一个会落盘的条目（`repl` 攒、system prompt 那节读）。
    pub fn repl_blocks(&self) -> Arc<ReplBlocks> {
        self.get_or_init(ReplBlocks::default)
    }
}

impl std::fmt::Debug for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionState").finish_non_exhaustive()
    }
}

/// 解释器跑过的单个代码块（`__meta__.repl_blocks` 的一条）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReplBlock {
    /// 工具调用的 id（判「它还在不在当前对话里」就靠它）。
    pub id: String,
    /// 那次 `repl` 的 `code` 参数（逐字；过长的按字符截断并标一句）。
    pub code: String,
}

/// 代码块日志：**独立于压缩**攒着。
///
/// 为什么得单独存一份：压缩会把 `tool_calls` 从消息里删掉（轮次级把整段 span 换成摘要、
/// 会话级把整窗口归档），等要把代码摆进 system prompt 那节时消息里已经查不到了。所以它比
/// 消息活得久（每轮开头扫一遍消息里**还没记过**的 `repl` 调用、按 call id 去重）。
/// 而 system prompt 那节的内容由它派生：**只列 id 已不在 `messages` 里的那些** —— 两次压缩
/// 之间逐字不变（前缀缓存照旧命中），只有真被压走东西的那一刻才变。
#[derive(Default)]
pub struct ReplBlocks(Mutex<Vec<ReplBlock>>);

impl ReplBlocks {
    /// 追加一批（按 `id` 去重；超 [`REPL_BLOCKS_MAX`] 丢最老的）。
    pub fn record(&self, fresh: Vec<ReplBlock>) {
        let mut blocks = self.0.lock().unwrap_or_else(|e| e.into_inner());
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

    /// 快照（落盘、拼 system prompt 都读它）。
    pub fn snapshot(&self) -> Vec<ReplBlock> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 从 `__meta__` 装回（`-r` / `pie sessions` 读的那份）。
    pub fn load(&self, blocks: Vec<ReplBlock>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = blocks;
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
        let log = ReplBlocks::default();
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
        assert!(Arc::ptr_eq(&state.repl_blocks(), &state.repl_blocks()));
    }
}
