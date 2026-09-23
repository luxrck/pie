//! 回合取消信号（TUI 的 `Esc`；`aturn` 的 `cancel` 形参也能外部触发）。
//!
//! 一个很薄的东西：`AtomicBool`（随时可问「取消了没」）+ `Notify`（取消时唤醒正在等的人）。
//! 放在模块顶层是因为**三层都要用**：`session`（回合循环）、`llm`（请求 race）、`tools`（shell 杀进程组）；
//! 塞进任何一层都会让另外两层反向依赖它。
//!
//! `Clone` 是共享语义（内部 `Arc`），所以能随手发给工具、传给等待点。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

/// 取消后写入历史 / 返回给调用方的固定文本（与 Python 版 `CANCEL_TEXT` 同字面量）。
pub const CANCEL_TEXT: &str = "用户手动终止";

#[derive(Clone, Default)]
pub struct Cancel {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl std::fmt::Debug for Cancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cancel(cancelled={})", self.is_cancelled())
    }
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// 触发取消（幂等；已经取消过就什么都不做）。
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// 等到取消触发（已经取消了就立刻返回）。
    ///
    /// ⚠️ `notify_waiters` 只唤醒**当时**正在等的人，所以这里必须先查一次标志位，
    /// 否则「先 cancel 后 await」会永远等下去。
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_returns_immediately_if_already_cancelled() {
        let cancel = Cancel::new();
        assert!(!cancel.is_cancelled());
        cancel.cancel();
        assert!(cancel.is_cancelled());
        // 先 cancel 后 await：不能卡住（notify_waiters 不会补发通知）
        tokio::time::timeout(std::time::Duration::from_millis(200), cancel.cancelled())
            .await
            .expect("已经取消了就该立刻返回");
        cancel.cancel(); // 幂等
    }

    #[tokio::test]
    async fn cancelled_wakes_waiters() {
        let cancel = Cancel::new();
        let waiter = cancel.clone();
        let task = tokio::spawn(async move { waiter.cancelled().await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!task.is_finished(), "还没取消，应该还在等");
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_millis(500), task)
            .await
            .expect("取消后要唤醒")
            .expect("join");
    }
}
