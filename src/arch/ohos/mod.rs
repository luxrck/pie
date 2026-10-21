//! OHOS（OpenHarmony）专场：SELinux 把一批 syscall / ioctl 拒了，这里逐个补。
//!
//! 各有各的来龙去脉，写在各自模块的注释里：
//!
//! - [`net`]：`getsockopt(SO_ERROR)` 被拒 → 给 `getsockopt` 一份同名定义（tokio 判连接成败要用）。
//! - `tty`：`TCSETS`（`tcsetattr(TCSANOW)`）被拒 + `hdc` 的 pty 恒报 0×0 → TUI 自己进 raw 模式、
//!   自己兜窗口大小。只有 `tui` feature 才编（它要用 ratatui / crossterm）。

pub mod net;

/// TUI 的终端开关（raw 模式 + 窗口大小）。
#[cfg(feature = "tui")]
pub mod tty;
