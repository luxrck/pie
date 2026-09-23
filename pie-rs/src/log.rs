//! 进程级告警出口：TUI 在跑时送进消息流，其余情况写 stderr。
//!
//! 为什么要有这层间接：TUI 期间终端是 raw mode + alternate screen，**任何直接写 stderr
//! 的字节都会落在当前光标处**，而 ratatui 每帧只重写有变化的单元格（双缓冲 diff）→
//! 被砸坏的那几行再也不会被重画（表现：屏幕残留旧文本、最新输出看不见）。
//! 重试提示、压缩统计、图片上传告警这类「回合中途」的消息正好踩这个坑，所以统一走这里。
//!
//! 非 TUI（一次性 / `--stat` 等）没人装 sink，行为与以前一致：直接 `eprintln!`。
//!
//! 两类含义不同：`warn` 是**一条独立告警**（进消息流就是一条新提示）；`progress` 是**进度**
//! （同一个东西在往前走，比如「第 N/M 次重试」）——TUI 侧把同 `key` 的进度**就地更新**成
//! 单个块，不然每重试一次就多一行；`progress_done` 宣布这串进度结束（成功或放弃），
//! TUI 侧把那个块撤掉。`key` 由发进度的一方给（重试就用「哪个请求」），这样并行的两条
//! 流程（回合请求 / 启动时拉模型列表）各占一块，不会互相覆盖。

use std::sync::{Mutex, MutexGuard, OnceLock};

/// 告警的类别（决定 TUI 侧怎么摆）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 独立告警：一条新提示（压缩统计、图片上传失败…）
    Warn,
    /// 进度：同一个块就地刷新（重试次数…）
    Progress,
    /// 这串进度结束了（成功或放弃）：把那个块撤掉
    ProgressDone,
}

/// 一条日志：`key` 只在 `Progress*` 上有意义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: Kind,
    pub key: String,
    pub text: String,
}

/// sink 返回 `false` = 没接住（界面已经退出）→ 退回 stderr。
type Sink = Box<dyn Fn(Notice) -> bool + Send>;

fn slot() -> &'static Mutex<Option<Sink>> {
    static SLOT: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn slot_lock() -> MutexGuard<'static, Option<Sink>> {
    // 锁里只有一次 channel send，不会 panic；真被毒化了也别让告警把进程带崩
    slot().lock().unwrap_or_else(|e| e.into_inner())
}

/// 安装出口（TUI 主循环持有）；`Guard` drop 时卸载。
pub fn install(sink: impl Fn(Notice) -> bool + Send + 'static) -> Guard {
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
    emit(Notice {
        kind: Kind::Warn,
        key: String::new(),
        text: msg.into(),
    })
}

/// 一行**进度**（详见 [`Kind::Progress`]）：TUI 侧会把同一 `key` 的进度写进同一个块。
pub fn progress(key: &str, msg: impl Into<String>) {
    emit(Notice {
        kind: Kind::Progress,
        key: key.to_string(),
        text: msg.into(),
    })
}

/// 这串进度（`key`）结束了：TUI 侧把那个块撤掉（成功、放弃都算结束）。
pub fn progress_done(key: &str) {
    emit(Notice {
        kind: Kind::ProgressDone,
        key: key.to_string(),
        text: String::new(),
    })
}

fn emit(notice: Notice) {
    let handled = slot_lock()
        .as_ref()
        .map(|sink| sink(notice.clone()))
        .unwrap_or(false);
    // 结束标记没有文字可打，没人接就静默丢掉（它只是个「撤块」指令）
    if !handled && notice.kind != Kind::ProgressDone {
        eprintln!("{}", notice.text);
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
        let guard = install(move |notice| {
            sink_seen
                .lock()
                .unwrap()
                .push(format!("{:?}/{}:{}", notice.kind, notice.key, notice.text));
            true
        });
        warn(format!("[retry] 第 {} 次", 2));
        progress("流式请求", "[retry] 第 3 次");
        progress_done("流式请求");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [
                "Warn/:[retry] 第 2 次",
                "Progress/流式请求:[retry] 第 3 次",
                "ProgressDone/流式请求:",
            ]
        );

        // 卸载后退回 stderr（这里只确认不再进 sink；那条会出现在测试捕获的 stderr 里）
        drop(guard);
        warn("[retry] 卸载之后写 stderr");
        assert_eq!(seen.lock().unwrap().len(), 3);

        // 拒收（界面已退出）也退回 stderr
        let guard = install(|_| false);
        warn("[retry] 没人接就写 stderr");
        drop(guard);
        assert_eq!(seen.lock().unwrap().len(), 3);
    }
}
