"""pie `repl` 工具的驱动：一个**长活**的 IPython，用「定长前缀 + JSON」跟 Rust 侧说话。

由 `src/repl.rs` 以 `python -u -c <本文件内容>` 启动；协议（与 `repl.rs` 对齐）：

    收： u32 大端长度 ++ utf-8 源码
    发： u32 大端长度 ++ utf-8 JSON {"stdout":…, "stderr":…, "error":…}

parent 一死，stdin 就 EOF → 本进程自杀（孤儿进程防护）。
"""

import io
import json
import os
import struct
import sys
import traceback

# 协议走真 stdin/stdout 的 **fd 副本**；把 sys.stdin 换成空的，让用户代码里的 input()
# 得到 EOF，而不是把协议字节吃掉。
proto_in = os.fdopen(os.dup(sys.stdin.fileno()), "rb", buffering=0)
proto_out = os.fdopen(os.dup(sys.stdout.fileno()), "wb", buffering=0)
sys.stdin = io.StringIO("")


def _no_input(*_args, **_kwargs):
    raise RuntimeError("本 REPL 没有终端，不支持交互式 input()；请把输入写死在代码里")


import builtins

builtins.input = _no_input

from IPython.core.interactiveshell import InteractiveShell
from IPython.utils.capture import capture_output

shell = InteractiveShell.instance()
try:
    shell.history_manager.enabled = False  # 不往 ~/.ipython 写历史
except Exception:
    pass
# 末表达式的展示：去掉 "Out[n]: " 前缀（正文 repr 仍会写进 stdout）——模型读起来干净。
shell.displayhook.write_output_prompt = lambda: None
# 关掉 IPython 自带的带色 traceback 打印：异常由我们统一成一份纯文本（见下）。
shell.showtraceback = lambda *_args, **_kwargs: None


def _read_exact(n):
    buf = b""
    while len(buf) < n:
        chunk = proto_in.read(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return buf


def _send(payload):
    body = json.dumps(payload).encode("utf-8")
    proto_out.write(struct.pack(">I", len(body)) + body)


def _format_exc(exc):
    return "".join(traceback.format_exception(type(exc), exc, exc.__traceback__)).rstrip("\n")


while True:
    try:
        (length,) = struct.unpack(">I", _read_exact(4))
        code = _read_exact(length).decode("utf-8")
    except EOFError:
        break  # 父进程没了 → 退出

    try:
        with capture_output() as cap:
            result = shell.run_cell(code, store_history=True)
        stdout = cap.stdout
        # display() 的富文本里能取 text/plain 的，也算作输出（图像之类取不到就跳过）。
        for output in getattr(cap, "outputs", []):
            data = output.get("data", {}) if isinstance(output, dict) else {}
            text = data.get("text/plain")
            if text:
                stdout += text if text.endswith("\n") else text + "\n"
        error = None
        for exc in (result.error_before_exec, result.error_in_exec):
            if exc is not None:
                error = _format_exc(exc)
                break
        payload = {"stdout": stdout, "stderr": cap.stderr, "error": error}
    except BaseException as exc:  # KeyboardInterrupt / 驱动自身异常：也要回一帧，别让 Rust 卡住
        payload = {"stdout": "", "stderr": "", "error": _format_exc(exc)}
    _send(payload)
