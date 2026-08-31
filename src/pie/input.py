"""终端输入：优先 prompt_toolkit 做 Unicode 安全的行编辑，非 TTY 回退 input()。

背景：内置 input() 依赖 readline，在部分终端（WSL/mintty 等）退格按字节删除，
删中文时可能截断一个 UTF-8 字符导致乱码/报错；prompt_toolkit 按字符（字素）编辑。
"""

from __future__ import annotations

import sys
from pathlib import Path

try:
    from prompt_toolkit import PromptSession
    from prompt_toolkit.history import FileHistory

    _HAS_PROMPT_TOOLKIT = True
except ImportError:  # prompt_toolkit 缺失时静默回退 input()
    _HAS_PROMPT_TOOLKIT = False

_history: FileHistory | None = None


def read_input(prompt: str = "", history_file: str | Path | None = None) -> str:
    """读取一行输入；交互终端用 prompt_toolkit，管道/非交互回退 input()。"""
    global _history
    if _HAS_PROMPT_TOOLKIT and sys.stdin.isatty():
        if history_file is not None and _history is None:
            _history = FileHistory(str(history_file))
        return PromptSession(history=_history).prompt(prompt)
    return input(prompt)

