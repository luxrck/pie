//! 平台专场：SELinux / 内核把某些 syscall、ioctl 拒掉时的本地兜底。
//!
//! 目前只有 OHOS（`target_os` 仍是 `linux`、OHOS 体现在 `target_env = "ohos"`，
//! 所以只能按 `target_env` 分）。这里放的是**与交互无关**的那几小块（网络 / 终端开关），
//! 上层逻辑（TUI 自己怎么用终端）仍住 `src/tui/`。

/// OHOS：`getsockopt(SO_ERROR)` 兜底（[`ohos::net`]）+ 终端 raw 模式与窗口大小兜底（`ohos::tty`）。
#[cfg(target_env = "ohos")]
pub mod ohos;
