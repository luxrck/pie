"""剪贴板图片 → `~/.pie/files/` 的内容寻址副本（TUI 的 Ctrl+V / `/paste` 用）。

只做一件事：把系统剪贴板里的**图片**编码成 PNG 落盘，返回路径；剪贴板里没有图片时返回
None，调用方回退成「普通文本粘贴」——绝不吞掉纯文本粘贴。

落盘**复用 `files.store_blob()`**，也就是和 `read` 读到图片时落的是同一个文件
（`~/.pie/files/img-<sha256[:16]>.png`）：
  - 回车后 `read` 这张图 → `store_blob` 命中已存在的副本 → **零复制**（不会出现同字节两个名字）；
  - 之后它被登记进会话 `__meta__.files` → 自然获得 gc 的引用保护，与普通图片副本同命运；
  - 代价：files/ 那条「未被引用 = 垃圾」的判据对**刚粘贴、还没被 read** 的图不成立，
    所以 gc 另设 mtime 保护窗口（`files.GC_PROTECT_HOURS`）挡误删。

后端是 Pillow 的 `ImageGrab.grabclipboard()`（号称跨平台，其实各平台实现都是外部调用）：

- **Windows**：Win32 剪贴板，返回 `Image`（PNG / DIB）；剪贴板里是**文件**时返回路径列表（CF_HDROP）；
- **macOS**：`osascript -e "get the clipboard as «class PNGf»"`，只认 PNG。截图到剪贴板多是 TIFF，
  但 macOS 的 pasteboard 在请求时会自动做 TIFF → PNG 转换，所以通常拿得到；读剪贴板不需要自动化权限；
- **Linux**：Pillow 内部转调 `wl-paste`（Wayland）或 `xclip`（X11）；**两个都没有时抛 NotImplementedError**
  → **只有这一种情况**才接着走 WSL 后备（下一条），仍未拿到就当「剪贴板里没有图片」返回 None；
- **WSL 后备**：Pillow 的 Linux 分支要外部工具，没装时可剪贴板其实在 Windows 侧 → 用
  `powershell.exe -sta` + `System.Windows.Forms.Clipboard::GetImage()` 存一份 PNG 再读回来
  （见 `_wsl_png_bytes`）。实测两件事：WSLg 装了 `wl-clipboard` 就不必走这条（WSLg 的剪贴板桥
  会把 Windows 剪贴板里的图以 `image/png` 转发给 `wl-paste`，0.04s 拿到）；而 PowerShell 冷启动
  约 0.5s，所以它**只**在 Pillow 根本看不到剪贴板时才跑，不在「剪贴板里没有图」时白跑。

阻塞：`grabclipboard()` / PowerShell 都是同步阻塞调用（Windows 本地 API、macOS 起 osascript、Linux 起子进程，
WSL 起 PowerShell 约 0.5s）→ 调用方（TUI）负责丢线程池，别在事件循环里直接调。
"""

from __future__ import annotations

import io
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

from .files import store_blob

_PNG_MAGIC = b"\x89PNG\r\n\x1a\n"
_WSL_TIMEOUT = 15.0  # PowerShell 冷启动 ~0.5s，给足余量；超时/报错都当没图

# 从 Windows 剪贴板取图存成 PNG。两个坑：`-sta` 必加（否则 GetImage 直接抛），
# 目标路径得先 `wslpath -w` 转成 Windows 认得的形式。
_PS_GRAB = (
    "Add-Type -AssemblyName System.Windows.Forms;"
    "Add-Type -AssemblyName System.Drawing;"
    "$i=[System.Windows.Forms.Clipboard]::GetImage();"
    "if($i){$i.Save('%s',[System.Drawing.Imaging.ImageFormat]::Png);'1'}else{'0'}"
)

# 剪贴板里是文件（CF_HDROP）时，只认这些扩展名 —— 复制个 .txt 过来仍走普通文本粘贴。
# 与 read 能吃的一致（PNG/JPEG/GIF/WebP/BMP），多收 TIFF（read 不吃，但用户可能自己再用）。
_IMAGE_SUFFIXES = {".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tif", ".tiff"}


def _engine():
    """惰性拿 ImageGrab：没装 Pillow 时返回 None（功能静默不可用，不报错）。"""
    try:
        from PIL import ImageGrab
    except ImportError:
        return None
    return ImageGrab


def _from_file_list(items: list[str]) -> Path | None:
    """Windows CF_HDROP：剪贴板里是文件 → 第一个存在的图片文件直接用它（不复制）。"""
    for item in items:
        p = Path(item)
        if p.suffix.lower() in _IMAGE_SUFFIXES and p.is_file():
            return p
    return None


def _store_png_bytes(data: bytes) -> Path | None:
    """PNG 字节 → files/ 的内容寻址副本（磁盘满了也不该炸界面）。"""
    try:
        return store_blob(data, mime="image/png")[1]
    except OSError:
        return None


def _wsl_png_bytes() -> bytes | None:
    """WSL 后备：从 **Windows** 剪贴板取图（WSLg 下 Pillow 那条路要 wl-paste/xclip，通常都没有）。

    做法：PowerShell 把图写成 PNG 到临时文件，再把字节读回来（整条链实测 ~0.5s）。
    """
    if not (os.environ.get("WSL_DISTRO_NAME") and shutil.which("powershell.exe")):
        return None
    tmp = Path(tempfile.gettempdir()) / f"pie-clip-{os.getpid()}.png"
    tmp.unlink(missing_ok=True)
    try:
        converted = subprocess.run(
            ["wslpath", "-w", str(tmp)], capture_output=True, timeout=_WSL_TIMEOUT
        )
        if converted.returncode != 0:
            return None
        win_path = converted.stdout.decode(errors="replace").strip()
        done = subprocess.run(
            ["powershell.exe", "-sta", "-NoProfile", "-NonInteractive", "-Command", _PS_GRAB % win_path],
            capture_output=True,
            timeout=_WSL_TIMEOUT,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    try:
        data = tmp.read_bytes() if done.returncode == 0 and tmp.exists() else None
    except OSError:
        data = None
    finally:
        tmp.unlink(missing_ok=True)
    return data if data and data.startswith(_PNG_MAGIC) else None


def _save(image) -> Path | None:
    """把 PIL Image 编码成 PNG，落进 files/ 的内容寻址副本（幂等 + 原子替换）。"""
    try:
        image.load()  # Windows 的 DIB / macOS 的 BytesIO 都是惰性的，先读完
        buf = io.BytesIO()
        image.save(buf, "PNG")
    except Exception:  # 编码失败（罕见模式等）→ 当作没拿到图
        return None
    return _store_png_bytes(buf.getvalue())


def grab_image_path() -> Path | None:
    """剪贴板里有图片 → 落成 `~/.pie/files/img-<hash16>.png` 返回路径；否则 None。

    注意：这是**同步阻塞**函数，TUI 侧必须用 `asyncio.to_thread` 调。
    """
    grabber = _engine()
    if grabber is None:
        return None
    try:
        result = grabber.grabclipboard()
    except NotImplementedError:
        # Linux 上 wl-paste / xclip 都没有 → Pillow 根本看不到剪贴板，才轮到 WSL 后备。
        # 注意：**只**在这时候回退。若 Pillow 只是「说剪贴板里没有图片」（工具在但内容不是图），
        # 再起一次 PowerShell 白等 0.4s——每次纯文本粘贴都要付这个钱。
        data = _wsl_png_bytes()
        return _store_png_bytes(data) if data else None
    except OSError:  # 子进程异常（ChildProcessError 是 OSError 的子类）
        return None
    except Exception:  # 剪贴板状态异常（被别的进程锁住等）不该炸掉整个界面
        return None
    if result is None:  # 剪贴板里没有图片（或只有文本）
        return None
    if isinstance(result, list):  # CF_HDROP：剪贴板里是文件而非位图
        return _from_file_list(result)
    return _save(result)
