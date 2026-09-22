//! 进程级告警出口：TUI 在跑时送进消息流，其余情况写 stderr。
//!
//! 为什么要有这层间接：TUI 期间终端是 raw mode + alternate screen，**任何直接写 stderr
//! 的字节都会落在当前光标处**，而 ratatui 每帧只重写有变化的单元格（双缓冲 diff）→
//! 被砸坏的那几行再也不会被重画（表现：屏幕残留旧文本、最新输出看不见）。
//! 重试提示、压缩统计、图片上传告警这类「回合中途」的消息正好踩这个坑，所以统一走这里。
//!
//! 非 TUI（一次性 / `--stat` 等）没人装 sink，行为与以前一致：直接 `eprintln!`。

use std::sync::{Mutex, MutexGuard, OnceLock};

/// sink 返回 `false` = 没接住（界面已经退出）→ 退回 stderr。
type Sink = Box<dyn Fn(String) -> bool + Send>;

fn slot() -> &'static Mutex<Option<Sink>> {
    static SLOT: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn slot_lock() -> MutexGuard<'static, Option<Sink>> {
    // 锁里只有一次 channel send，不会 panic；真被毒化了也别让告警把进程带崩
    slot().lock().unwrap_or_else(|e| e.into_inner())
}

/// 安装出口（TUI 主循环持有）；`Guard` drop 时卸载。
pub fn install(sink: impl Fn(String) -> bool + Send + 'static) -> Guard {
    *slot_lock() = Some(Box::new(sink));
    Guard
}

/// 卸载出口的凭据（drop 即卸载）。
pub struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        *slot_lock() = None;
    }
}

/// 一行告警：装了出口就交给它，否则写 stderr。
pub fn warn(msg: impl Into<String>) {
    let msg = msg.into();
    let handled = slot_lock()
        .as_ref()
        .map(|sink| sink(msg.clone()))
        .unwrap_or(false);
    if !handled {
        eprintln!("{msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// ⚠ 全局只有一个槽位、测试并行跑，所以「装 sink」只在这一个用例里做。
    #[test]
    fn installed_sink_takes_over_until_dropped() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink_seen = seen.clone();
        let guard = install(move |msg| {
            sink_seen.lock().unwrap().push(msg);
            true
        });
        warn(format!("[retry] 第 {} 次", 2));
        assert_eq!(seen.lock().unwrap().as_slice(), ["[retry] 第 2 次"]);

        // 卸载后退回 stderr（这里只确认不再进 sink；那条会出现在测试捕获的 stderr 里）
        drop(guard);
        warn("[retry] 卸载之后写 stderr");
        assert_eq!(seen.lock().unwrap().len(), 1);

        // 拒收（界面已退出）也退回 stderr
        let guard = install(|_| false);
        warn("[retry] 没人接就写 stderr");
        drop(guard);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
