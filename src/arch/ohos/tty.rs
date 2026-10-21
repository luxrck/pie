//! OHOS 上的终端开关（`target_env = "ohos"` + `tui` feature 才编译）。
//!
//! 调用点是 `tui::init_terminal()` / `tui::restore_terminal()`：那边非 OHOS 走 ratatui 自己那套，
//! 这里替掉它。为什么不能直接用 `ratatui::try_init()` —— 两条都是上机实测（模拟器 + `hdc shell`）：
//!
//! 1. **raw 模式进不去**。crossterm 的 `enable_raw_mode` 走 `tcsetattr(TCSANOW)` → ioctl `TCSETS`，
//!    而 OHOS 的 shell 域对**继承来的 pty fd**只放行 `TCSETSW`（`TCSADRAIN`）/ `TCSETSF`
//!    （`TCSAFLUSH`），`TCSETS` 回 `EACCES` → `try_init()` 失败，`ratatui::init()` 直接
//!    panic（`failed to initialize terminal: Os { code: 13, PermissionDenied }`）。
//!    `TCSADRAIN` 对交互界面与 `TCSANOW` 没有区别（多等一次输出排空），所以这里自己设 raw 模式、
//!    绕开 crossterm 那套。⚠ 别改成「自己打开 `/dev/tty` 再用 `TCSETS`」：那条路确实放行，
//!    但 fd 指向的未必是 stdin 那个终端，pty 上没有意义。
//! 2. **窗口大小恒为 0×0**。`hdc` 不给设备侧 pty 传大小（`TIOCGWINSZ` 报 0，`TIOCSWINSZ` 也被拒），
//!    于是 `terminal::size()` == (0, 0) → 全屏视口退化成 0 行 0 列、整块 UI 什么都不画。
//!    报不出大小时退成**固定视口**，尺寸取 `COLUMNS` / `LINES` 环境变量，缺省 80×24
//!    （`export COLUMNS=100 LINES=30` 就能对上自己的终端）。
//!
//! panic hook 也自己装：`ratatui::set_panic_hook` 里那次 `restore()` 走的是 crossterm 的
//! raw 模式状态，我们没经它的手 → 它以为没进 raw 模式，什么也不恢复（终端会留在 raw 模式）。

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Mutex;

use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{DefaultTerminal, Terminal, TerminalOptions, Viewport};

/// 进 raw 模式前的 termios（`None` = 当前不在 raw 模式）。
static ORIGINAL: Mutex<Option<libc::termios>> = Mutex::new(None);

/// 进 raw 模式 + 备用屏 + `Terminal`；失败时把终端还回去再把错抛给 `main`。
pub(crate) fn init() -> io::Result<DefaultTerminal> {
    // 先装 hook：后面任何一步失败/崩溃，屏幕与回显都还在人手里。
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        hook(info);
    }));
    match enter() {
        Ok(terminal) => Ok(terminal),
        Err(e) => {
            restore();
            Err(e)
        }
    }
}

fn enter() -> io::Result<DefaultTerminal> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    match crossterm::terminal::size() {
        // 设备上的终端 app（不是 hdc 的 pty）能报出真实大小 → 照旧自适应
        Ok((cols, rows)) if cols > 0 && rows > 0 => Terminal::new(backend),
        // hdc 的 pty 恒报 0×0：退成固定视口，别让整块 UI 缩成 0 行 0 列
        _ => {
            let (cols, rows) = fallback_size();
            Terminal::with_options(
                backend,
                TerminalOptions {
                    viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
                },
            )
        }
    }
}

/// 还原 termios（`TCSADRAIN`，同 [`enable_raw_mode`] 的理由）+ 离开备用屏。
pub(crate) fn restore() {
    let original = ORIGINAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(original) = original {
        match tty() {
            // SAFETY: `original` 是本进程刚才 `tcgetattr` 拿到的同一终端的原始状态，
            // `fd` 是当前进程的 tty（继承的 stdin 或刚打开的 `/dev/tty`，`_tty` 活到调用结束）。
            Ok((fd, _tty)) => {
                unsafe { libc::tcsetattr(fd, libc::TCSADRAIN, &original) };
            }
            Err(e) => crate::log::warn(&format!("恢复终端失败: {e}")),
        }
    }
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

/// 打开 raw 模式（`TCSADRAIN`）。
fn enable_raw_mode() -> io::Result<()> {
    let mut slot = ORIGINAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_some() {
        return Ok(());
    }
    let (fd, _tty) = tty()?;
    // SAFETY: `fd` 是刚验过的 tty；`tcgetattr` 会把 `ios` 填满，之后只交给 `cfmakeraw`/`tcsetattr`。
    unsafe {
        let mut ios = std::mem::zeroed::<libc::termios>();
        if libc::tcgetattr(fd, &mut ios) != 0 {
            return Err(io::Error::last_os_error());
        }
        let original = ios;
        libc::cfmakeraw(&mut ios);
        if libc::tcsetattr(fd, libc::TCSADRAIN, &ios) != 0 {
            return Err(io::Error::last_os_error());
        }
        *slot = Some(original);
    }
    Ok(())
}

/// tty 的 fd：stdin 是终端就用它（与 crossterm 同口径），否则开 `/dev/tty`。
///
/// 返回的 `File`（若开了的话）要活到调用结束——fd 是它借出来的。
fn tty() -> io::Result<(RawFd, Option<File>)> {
    // SAFETY: `isatty` 只读 fd 的表项，没有内存副作用。
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        return Ok((libc::STDIN_FILENO, None));
    }
    let file = File::options().read(true).write(true).open("/dev/tty")?;
    let fd = file.as_raw_fd();
    Ok((fd, Some(file)))
}

/// 报不出窗口大小时的兜底尺寸：`COLUMNS` / `LINES`，缺省 80×24。
fn fallback_size() -> (u16, u16) {
    let num = |key: &str, default: u16| {
        std::env::var(key)
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(default)
    };
    (num("COLUMNS", 80), num("LINES", 24))
}
