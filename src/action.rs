//! 扩展「改实例」的动作通道：宿主（另一个进程）发**意图** → 落到 [`crate::session::Session`] 上
//! 那批**已有**的方法。
//!
//! 为什么中间要一层队列：改实例的方法（`set_model` / `compact` …）要 `&mut self`，而宿主发帧的时刻
//! 不受我们控制（一批工具并发跑时 `Session` 只有 `&self`）。所以意图先收下，等回合循环手里有
//! `&mut self` 的那一刻（apply point）再执行。
//!
//! 两档（回执里的 `effective` 就是给插件看的语义）：
//!   - **立即**（[`IMMEDIATE`]）：不碰 `&mut Session` 的，当场执行 → `"now"`；
//!   - **排队**（[`QUEUED`]）：入队，apply point 执行 → `"next_step"`（回执只说「已受理」，
//!     不等于「已生效」——这一条必须让插件作者知道）。
//!
//! ⚠ 方法白名单**只有** `Session` 上已有的公开方法：这里不另造 API（扩展的能力面 = `Session`
//! 的能力面，不搞第二套）。

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::cancel::Cancel;

/// 一次方法调用：`{"method": "set_model", "args": {"name": "…"}}`。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MethodCall {
    pub method: String,
    #[serde(default)]
    pub args: Value,
}

/// **立即档**：不碰 `&mut Session` 的方法（当场执行，回执 `effective: "now"`）。
pub const IMMEDIATE: &[&str] = &["cancel"];

/// **排队档**：要 `&mut self` 的方法（apply point 执行，回执 `effective: "next_step"`）。
pub const QUEUED: &[&str] = &[
    "set_model",
    "set_reasoning_effort",
    "compact",
    "clear_window",
    "reset",
];

/// 会话的动作入口：立即档当场办；排队档攒着，等回合循环取走。
///
/// `Clone` 是共享语义（内部是 `Arc`）：`Session` 持一份，宿主桥按会话 id 另取一份。
#[derive(Clone)]
pub struct Actions {
    /// 立即档要用它 —— `Clone` 是共享语义（与 `Session.cancel` 是同一个信号）。
    cancel: Cancel,
    queue: Arc<Mutex<Vec<MethodCall>>>,
}

impl std::fmt::Debug for Actions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Actions")
            .field("queued", &self.queued())
            .finish_non_exhaustive()
    }
}

impl Actions {
    pub fn new(cancel: Cancel) -> Self {
        Self {
            cancel,
            queue: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 收下一次方法调用，**当场**给回执（`{"ok": …, "effective": …}`）。
    ///
    /// 排队档的回执字段叫 `effective: "next_step"`：它表示「已受理」，**不是**「已生效」
    /// —— 插件别拿它当生效凭据。
    pub fn submit(&self, method: &str, args: Value) -> Value {
        if !IMMEDIATE.contains(&method) && !QUEUED.contains(&method) {
            let mut known = IMMEDIATE.to_vec();
            known.extend_from_slice(QUEUED);
            return json!({
                "ok": false,
                "error": format!("未知方法: {method}（可用: {}）", known.join(", ")),
            });
        }
        if IMMEDIATE.contains(&method) {
            // 白名单目前只有 cancel（`Cancel` 共享语义 → 与 TUI 的 Esc 是同一个信号）
            self.cancel.cancel();
            return json!({"ok": true, "effective": "now"});
        }
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(MethodCall {
                method: method.to_string(),
                args,
            });
        json!({"ok": true, "effective": "next_step"})
    }

    /// apply point：取走排队档（调用方拿着 `&mut Session` 逐条执行）。
    pub fn take(&self) -> Vec<MethodCall> {
        std::mem::take(&mut *self.queue.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// 手上还排着几条（`Debug` 用）。
    fn queued(&self) -> usize {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediate_actions_fire_on_the_spot_and_queued_ones_line_up() {
        let cancel = Cancel::new();
        let actions = Actions::new(cancel.clone());

        let receipt = actions.submit("cancel", json!({}));
        assert_eq!(receipt["effective"], "now");
        assert!(cancel.is_cancelled(), "cancel 是立即档，回执之前就该生效");

        let receipt = actions.submit("set_model", json!({"name": "x"}));
        assert_eq!(receipt["effective"], "next_step");
        assert_eq!(actions.take().len(), 1, "排队档要留在队列里等 apply point");
        assert_eq!(actions.take().len(), 0, "取过就没了");
    }

    #[test]
    fn unknown_methods_are_rejected_with_the_whitelist() {
        let actions = Actions::new(Cancel::new());
        let receipt = actions.submit("rm_rf", json!({}));
        assert_eq!(receipt["ok"], false);
        assert!(
            receipt["error"].as_str().unwrap().contains("set_model"),
            "报错要列出可用方法：{receipt}"
        );
    }
}
