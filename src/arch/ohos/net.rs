//! OHOS（OpenHarmony）上的网络兜底：给 `getsockopt(SO_ERROR)` 一份本地定义。
//!
//! # 为什么
//!
//! OHOS 的 SELinux 策略**拒绝 `getsockopt()`**（`SO_ERROR` / `SO_TYPE` / `SO_RCVBUF` /
//! `SO_LINGER` … 一律 `EACCES`）。而 tokio 判断「非阻塞 connect 到底成没成」靠的正是
//! `getsockopt(SO_ERROR)`（`tokio::net::tcp::stream::connect_mio`：等到可写 → `take_error()`）：
//!
//! ```text
//! 非阻塞 connect → reactor 报可写 → getsockopt(SO_ERROR) 判成败
//! ```
//!
//! 最后一步被拒 → reqwest 报 `client error (Connect) ↳ tcp connect error ↳ Permission denied
//! (os error 13)`，任何走 tokio 网络栈的程序都连不上网（`std::net` 的阻塞 connect 不受影响，
//! 它只看 `connect(2)` 的返回值）。
//!
//! # 为什么是「自己定义同名符号」，而不是 wrap 一层 connect
//!
//! 两条路都堵着：
//!
//! 1. **被拒的 syscall 不是 `connect`** —— 换掉建连方式（阻塞 connect + `spawn_blocking`、
//!    自写 connector）都到不了「最后那句问话」；tokio 的 `connect_mio` 里
//!    `take_error()` 是写死的，没有开关。
//! 2. **就算想换，也没有位置可换**：reqwest 的连接器是 **sealed** 的
//!    （`connector_layer` 只收 `Service<Unnameable, Response = Conn>`，`Unnameable` / `Conn`
//!    都是私有类型，bound 在外部写不出来），`hyper-util` 的 `HttpConnector` 也没有 connect
//!    hook（`set_nodelay` / keepalive 这类的失败它只 warn）——只剩「不用 reqwest、自己写
//!    HTTP 层」那条路，为一个 syscall 重写网络层不划算。
//!
//! 所以只能在 syscall 这一层补：定义 `getsockopt` 同名符号 —— 这是**链接期静态解析**
//! （同一次链接里，我们的定义满足了 tokio 里那句未定义引用），不是 ELF 运行时插桩：
//! 不需要 `--export-dynamic`、不需要 `install()`、也不影响别的进程；那份引用本身既是接线，
//! 也是这个函数不会被 `--gc-sections` 裁掉的唯一理由。
//!
//! ⚠ **只有「和 tokio 编进同一个链接产物」才成立**：可执行文件（`pie`）✓；共享库（Python 绑定
//! 的 `pie._pie_rs`）✗ —— 共享库里对全局函数的调用默认是可抢占的，运行时按全局作用域先找到
//! libc 那份。那种场合要在该目标的链接参数里补 `-Wl,-Bsymbolic-functions`
//! （`[target.<triple>] rustflags`），否则这层会**静默不生效**。
//!
//! # 语义
//!
//! 只接管 `SO_ERROR`，其余选项**原样转发**给 libc（它们本来也被拒，行为与没这层时一致）。
//!
//! 判断「有没有挂着的错误」用 `recv(MSG_PEEK | MSG_DONTWAIT)`：出错（connect 失败）的 socket
//! 上，内核会把挂着的 errno 直接当返回值给它 —— 也就是 `SO_ERROR` 本该给出的那个**真实**
//! errno；而 `MSG_PEEK` 不吃数据、`MSG_DONTWAIT` 不阻塞。异步性不受影响：等仍然是 tokio 的
//! reactor 在等，这里只做「事后确认」。
//!
//! 诊断：`PIE_OHOS_NET=1` 时把「接管到了」与「还有哪个选项问到就被拒」各报一次
//! （走 [`crate::log`]，TUI 下也不会砸屏）。真机上加不了 `strace`，这两个问题
//! （兜底生效了没 / 面积够不够）只能靠它回答。

use std::ffi::c_void;
use std::os::raw::c_int;
use std::sync::{Mutex, Once, OnceLock};

/// libc 的 `getsockopt` 签名（转发时用）。
type Getsockopt =
    unsafe extern "C" fn(c_int, c_int, c_int, *mut c_void, *mut libc::socklen_t) -> c_int;

/// 覆盖 libc 的 `getsockopt`（只在 OHOS 编译进来；见模块注释）。
///
/// # Safety
///
/// 与 libc 的 `getsockopt(2)` 完全同签名、同语义，只是 `SO_ERROR` 换了实现。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getsockopt(
    fd: c_int,
    level: c_int,
    optname: c_int,
    optval: *mut c_void,
    optlen: *mut libc::socklen_t,
) -> c_int {
    /// 读 errno（musl 的口径）。
    fn errno() -> c_int {
        unsafe { *libc::__errno_location() }
    }

    /// 问一声「这个 socket 上挂着错误吗」：问得到的如实报，问不到的当 0（= 没错误）。
    fn pending_error(fd: c_int) -> c_int {
        // `SO_ERROR` 被拒 → 换个问法问同一件事：连接失败的 socket 上，内核会让 `recv`
        // 直接返回挂着的 errno（`MSG_PEEK` 不吃数据，`MSG_DONTWAIT` 不阻塞，
        // 而且和 `SO_ERROR` 一样是「读一次就清」）。
        let mut byte = 0u8;
        let got = unsafe {
            libc::recv(
                fd,
                (&raw mut byte).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        match if got >= 0 { 0 } else { errno() } {
            // 没数据 / 连接还在路上：调用方已经等到可写，算成功
            libc::EAGAIN | libc::EINTR => 0,
            // 连 recv 都不让问（元数据类 syscall 被整体关掉）→ 退回 poll，只看「是不是错误态」
            libc::EACCES | libc::EPERM => {
                if tracing() {
                    static FALLBACK: Once = Once::new();
                    FALLBACK.call_once(|| {
                        crate::log::warn(
                            "[ohos/net] recv 也被拒（EACCES/EPERM）→ 退回 poll；errno 只能猜",
                        )
                    });
                }
                poll_error(fd)
            }
            // 其余就是真实错误（ECONNREFUSED / ECONNRESET / EHOSTUNREACH / ETIMEDOUT …）
            e => e,
        }
    }

    /// `recv` 也被拒时的退路：只知道有没有出事，不知道 errno → 报最常见的 `ECONNREFUSED`。
    fn poll_error(fd: c_int) -> c_int {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // 超时 0：绝不阻塞（调用到这里时连接已经有结论了）
        let r = unsafe { libc::poll(&mut pfd, 1, 0) };
        if r > 0 && pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            libc::ECONNREFUSED
        } else {
            0
        }
    }

    /// libc 真那个（其余选项转发给它）。
    fn real() -> Option<Getsockopt> {
        static REAL: OnceLock<Option<Getsockopt>> = OnceLock::new();
        *REAL.get_or_init(|| {
            let p = unsafe { libc::dlsym(libc::RTLD_NEXT, c"getsockopt".as_ptr()) };
            if p.is_null() {
                None
            } else {
                // SAFETY: dlsym 给的就是 getsockopt 的地址，签名一致
                Some(unsafe { std::mem::transmute::<*mut c_void, Getsockopt>(p) })
            }
        })
    }

    /// 诊断开关（`PIE_OHOS_NET=1`），只读一次环境。
    fn tracing() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var("PIE_OHOS_NET").is_ok_and(|v| !v.is_empty() && v != "0"))
    }

    // 只接管这一个选项；其余原样转发（它们本来也会被 SELinux 拒，行为与没这层时一样）。
    if level != libc::SOL_SOCKET || optname != libc::SO_ERROR || optval.is_null() {
        let Some(real) = real() else {
            // 到不了（libc 一定在）；真到了也别假装成功
            unsafe { *libc::__errno_location() = libc::ENOSYS };
            return -1;
        };
        let ret = unsafe { real(fd, level, optname, optval, optlen) };
        if ret < 0 && tracing() {
            // 每个「被转发且仍失败」的选项报一次：它说明这层兜底的面积不够（还有别的调用方
            // 依赖别的选项）。真机上调这个正好当探测器。
            static DENIED: Mutex<Vec<(c_int, c_int)>> = Mutex::new(Vec::new());
            let fresh = {
                let mut seen = DENIED.lock().unwrap_or_else(|e| e.into_inner());
                let fresh = !seen.contains(&(level, optname));
                if fresh {
                    seen.push((level, optname));
                }
                fresh
            };
            if fresh {
                crate::log::warn(format!(
                    "[ohos/net] getsockopt(level={level}, opt={optname}) 转发后仍失败（errno={}）",
                    errno()
                ));
            }
        }
        return ret;
    }

    let err = pending_error(fd);
    unsafe { *(optval as *mut c_int) = err };
    if !optlen.is_null() {
        unsafe { *optlen = std::mem::size_of::<c_int>() as libc::socklen_t };
    }
    if tracing() {
        // 只报第一次：这行出现 = 这层真的被 tokio 调到了（没出现 = 没接线 / 没生效）
        static TAKEN: Once = Once::new();
        TAKEN.call_once(|| {
            crate::log::warn(format!(
                "[ohos/net] 接管 getsockopt(SO_ERROR) fd={fd} → {err}（此后不重复）"
            ))
        });
    }
    0
}
