"""终端背景明暗探测：OSC 11 查询 → COLORFGBG 兜底 → None（未知）。

用于「一套主题自动适配深色/浅色终端」：TUI 启动前探测一次，决定用主题族的哪个变体。
只用标准库；任何不确定都返回 None，由调用方回退到默认变体。
"""

from __future__ import annotations

import os
import re
import select
import time
from functools import lru_cache

try:  # pragma: no cover - 非 Unix 平台没有 termios
    import termios
except ImportError:  # pragma: no cover
    termios = None  # type: ignore[assignment]

# OSC 11 响应：``ESC ] 11 ; rgb:RRRR/GGGG/BBBB ST``（分量 2 或 4 位十六进制；部分终端带 alpha）
_OSC11_RE = re.compile(
    r"\x1b\]11;rgba?:([0-9a-fA-F]{2,4})/([0-9a-fA-F]{2,4})/([0-9a-fA-F]{2,4})"
)


def parse_osc11(reply: str) -> bool | None:
    """解析 OSC 11 响应里的背景色 → 背景是否深色；解析不了返回 None。"""
    match = _OSC11_RE.search(reply)
    if match is None:
        return None
    channels = [int(part, 16) / (16 ** len(part) - 1) for part in match.groups()]
    luma = 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2]
    return luma < 0.5


def parse_colorfgbg(value: str | None) -> bool | None:
    """解析 COLORFGBG（``"fg;bg"``，颜色索引 0-15）→ 背景是否深色；解析不了返回 None。

    背景索引 0-7 是暗色、8-15 是亮色（xterm/rxvt/WezTerm 等会设置这个变量）。
    """
    if not value:
        return None
    parts = value.split(";")
    try:
        background = int(parts[-1])
    except (ValueError, IndexError):
        return None
    return background < 8


def query_osc11(timeout: float = 0.2) -> str | None:
    """向控制终端发 OSC 11 查询并读回响应；无控制终端 / 超时 / 出错返回 None。

    直接读写 ``/dev/tty``（不碰 stdin/stdout），所以在管道的 shell 里也能工作；
    读前临时关掉规范模式与回显，读完恢复。响应超时是必要的——不支持该查询的终端
    不会有任何回复，不能无限等。
    """
    if termios is None:  # pragma: no cover - 非 Unix
        return None
    try:
        fd = os.open("/dev/tty", os.O_RDWR | os.O_NOCTTY)
    except OSError:
        return None
    original = None
    try:
        original = termios.tcgetattr(fd)
        raw = termios.tcgetattr(fd)
        raw[3] &= ~(termios.ICANON | termios.ECHO)
        raw[6][termios.VMIN] = 0
        raw[6][termios.VTIME] = 0
        termios.tcsetattr(fd, termios.TCSANOW, raw)
        os.write(fd, b"\x1b]11;?\x1b\\")
        buf = b""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], max(0.0, deadline - time.monotonic()))
            if not ready:
                break
            chunk = os.read(fd, 256)
            if not chunk:
                break
            buf += chunk
            if buf.endswith(b"\x1b\\") or buf.endswith(b"\x07"):
                break
        return buf.decode("utf-8", "replace") or None
    except termios.error:  # type: ignore[union-attr]
        return None
    finally:
        if original is not None:
            try:
                termios.tcsetattr(fd, termios.TCSADRAIN, original)  # type: ignore[union-attr]
            except termios.error:  # type: ignore[union-attr]  # pragma: no cover
                pass
        os.close(fd)


@lru_cache(maxsize=1)
def detect_dark_background() -> bool | None:
    """探测终端背景是否深色：OSC 11 查询 → COLORFGBG → None（未知）。

    进程内只探测一次（结果缓存）：OSC 查询最多阻塞 timeout，别在每次取主题时都做。
    """
    dark = parse_osc11(query_osc11() or "")
    if dark is not None:
        return dark
    return parse_colorfgbg(os.environ.get("COLORFGBG"))
