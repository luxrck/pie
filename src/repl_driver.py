"""pie `repl` 工具的驱动：一个**长活**的 IPython，用「定长前缀 + JSON」跟 Rust 侧说话。

由 `src/repl.rs` 以 `python -u -c <本文件内容>` 启动；协议（与 `repl.rs` 对齐）：

    收： u32 大端长度 ++ utf-8 源码
    发： u32 大端长度 ++ utf-8 JSON {"stdout":…, "stderr":…, "error":…}

parent 一死，stdin 就 EOF → 本进程自杀（孤儿进程防护）。
"""

import glob
import io
import json
import os
import struct
import sys
import tempfile
import time
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

# ---------------------------------------------------------------- 图像捕获（matplotlib）
#
# 图落到哪个目录：Rust 侧用 `PIE_REPL_IMAGE_DIR` 指定（缺省用系统临时目录）。
_IMAGE_DIR = os.environ.get("PIE_REPL_IMAGE_DIR") or os.path.join(
    tempfile.gettempdir(), "pie-repl-images"
)
try:
    os.makedirs(_IMAGE_DIR, exist_ok=True)
    # 顺手清掉一小时之前的旧图：会话一多，这里就是个垃圾场。
    _cutoff = time.time() - 3600
    for _f in glob.glob(os.path.join(_IMAGE_DIR, "*.png")):
        try:
            if os.path.getmtime(_f) < _cutoff:
                os.remove(_f)
        except OSError:
            pass
except OSError:
    _IMAGE_DIR = None

# 本次执行产出的图（每轮开始清空；帧里带给 Rust 侧）。
_images = []


def _capture_figures():
    """把当前**还开着**的 matplotlib figure 存成 PNG。

    为什么在 `post_execute` 扫、而不去 patch `plt.show`：无显示后端下 `show()` 是 no-op，
    而用户往往在**同一个 cell** 里 `import` + `plot` + `show` —— 执行前 patch 来不及。
    执行后扫「还没关掉的 figure」则一定抓得到（`show()` 在 Agg 下不会关它们）。
    """
    if _IMAGE_DIR is None:
        return
    plt = sys.modules.get("matplotlib.pyplot")
    if plt is None:
        return
    try:
        nums = plt.get_fignums()
    except Exception:
        return
    for n in nums:
        try:
            fig = plt.figure(n)
            path = os.path.join(_IMAGE_DIR, f"fig-{int(time.time() * 1000)}-{n}.png")
            fig.savefig(path, dpi=110)
            _images.append(path)
        except Exception:
            pass
    try:
        plt.close("all")  # 关掉才不会下一轮重复抓
    except Exception:
        pass


try:
    shell.events.register("post_execute", _capture_figures)
except Exception:
    pass  # 老版本 IPython 没有 events 就放弃（图就看不到，但工具照常工作）


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

    _images.clear()
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
        payload = {
            "stdout": stdout,
            "stderr": cap.stderr,
            "error": error,
            "images": list(_images),
        }
    except BaseException as exc:  # KeyboardInterrupt / 驱动自身异常：也要回一帧，别让 Rust 卡住
        payload = {"stdout": "", "stderr": "", "error": _format_exc(exc), "images": []}
    _send(payload)
