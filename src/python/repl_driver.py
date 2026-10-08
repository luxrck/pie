"""pie `repl` 工具的驱动：一个**长活**的 IPython，用「定长前缀 + JSON」跟 Rust 侧说话。

由 `src/repl.rs` 以 `python -u -c <本文件内容>` 启动；协议（与 `repl.rs` 对齐）：

    收： u32 大端长度 ++ utf-8 源码
    发： u32 大端长度 ++ utf-8 JSON {"stdout":…, "stderr":…, "error":…}

parent 一死，stdin 就 EOF → 本进程自杀（孤儿进程防护）。
"""

import glob
import hashlib
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
# 每个 figure 上一次上报时的内容指纹：替「抓完就 close」做去重（见 `_capture_figures`）。
_last_png_hash = {}


def _capture_figures():
    """把**内容变了的** matplotlib figure 存成 PNG。

    为什么在 `post_execute` 扫、而不去 patch `plt.show`：无显示后端下 `show()` 是 no-op，
    而用户往往在**同一个 cell** 里 `import` + `plot` + `show` —— 执行前 patch 来不及。
    执行后扫「还没关掉的 figure」则一定抓得到（`show()` 在 Agg 下不会关它们）。

    去重靠内容 hash，**不**用 `plt.close("all")`：close 会把 figure 从 pyplot 注销
    （对象还在用户命名空间里，所以照常能改），于是**跨 cell 增量改同一个 `fig`**
    （`fig, ax = plt.subplots()` 之后几个 cell 慢慢加图层）就再也不会被 `get_fignums()`
    看到 —— 图变了却不上报，画布一直显示旧图。
    代价：figure 不再自动关闭（长会话里画很多张会堆着，matplotlib 到 20 张时自己会警告）。
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
            buf = io.BytesIO()
            fig.savefig(buf, format="png", dpi=110)
            data = buf.getvalue()
            digest = hashlib.sha256(data).hexdigest()
            if _last_png_hash.get(n) == digest:
                continue  # 这个 figure 自上次上报后没变 → 不重复上报
            path = os.path.join(_IMAGE_DIR, f"fig-{int(time.time() * 1000)}-{n}.png")
            with open(path, "wb") as fh:
                fh.write(data)
            _images.append(path)
            _last_png_hash[n] = digest
        except Exception:
            pass


try:
    shell.events.register("post_execute", _capture_figures)
except Exception:
    pass  # 老版本 IPython 没有 events 就放弃（图就看不到，但工具照常工作）


def _namespace():
    """用户自己的名字：丢掉 IPython 注入的 `_i1` / `In` / `Out` / `exit` / `open` 那些噪音
    （后者在 `user_ns_hidden` 里，前者以 `_` 开头）。"""
    try:
        hidden = getattr(shell, "user_ns_hidden", {})
        return sorted(n for n in shell.user_ns if not n.startswith("_") and n not in hidden)
    except Exception:
        return []


# ---------------------------------------------------------------- 历史即数据
#
# `history()`：把**本会话的转录**变成解释器里的一份数据。快照由 Rust 侧每轮开头重写
# （路径走 `PIE_TRANSCRIPT`）—— 会话文件只在退出 / `/save` 时落盘，会话进行中它根本还不存在。
# 压缩指针默认展开：那正是它存在的理由（压缩之后模型的历史里已经没有那些代码块了）。
_TRANSCRIPT = os.environ.get("PIE_TRANSCRIPT") or ""


def _read_records(path):
    """读一份消息落盘件。两种形状都认：JSONL（一行一个对象）与 pretty JSON 数组。"""
    with open(path, encoding="utf-8") as fh:
        text = fh.read()
    if text.lstrip().startswith("["):
        return json.loads(text)
    out = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        obj = json.loads(line)
        if isinstance(obj, dict) and obj.get("__meta__"):
            continue  # 会话文件的首行元信息
        out.append(obj)
    return out


def _annotate(msg, turn):
    """每条消息额外带两个东西：`turn`（第几轮）与漏盘全文的路径。"""
    entry = {**msg, "turn": turn}
    comp = msg.get("compaction") or {}
    if comp.get("kind") == "tool" and comp.get("path"):
        entry["full_output_path"] = comp["path"]
    return entry


def history(expand=True, turn=None):
    """本会话的转录（从快照读，不是解释器里的变量）→ 按原顺序的 list[dict]。

    每条就是会话文件里那条消息（字段名一致，**只在适用的字段才出现**），另加：
      - `turn`：属于第几个用户回合（0 起）
      - `full_output_path`：工具输出被截断/落盘时**全文**的路径（`open(path).read()` 取）
    默认把压缩指针**展开**（`turn` / `session` 归档整段插回来）→ 拿到的是没被压过的完整对话；
    `expand=False` 看原始（压缩后）形态，`turn=2` 只看第 2 轮。
    快照每轮开头重写：含当前这轮的用户输入，不含本轮尚未落盘的消息。没有快照 → 空列表。
    """
    if not _TRANSCRIPT or not os.path.isfile(_TRANSCRIPT):
        return []
    try:
        records = _read_records(_TRANSCRIPT)
    except Exception:
        return []
    out = []
    current = -1
    for msg in records:
        if not isinstance(msg, dict):
            continue
        if msg.get("role") == "user":
            current += 1
        comp = msg.get("compaction") or {}
        kind, path = comp.get("kind"), comp.get("path")
        if expand and kind in ("turn", "session") and path:
            try:
                archived = _read_records(path)
            except Exception:
                archived = []
            if archived:
                out.extend(_annotate(raw, current) for raw in archived if isinstance(raw, dict))
                continue
        out.append(_annotate(msg, current))
    return out if turn is None else [e for e in out if e["turn"] == turn]


shell.user_ns["history"] = history
# 同时同步进 `user_ns_hidden`（IPython 自己的惯例：`exit` / `quit` / `open` 就是这么藏的，
# 好让 `%who` 看不见）—— 它是我们预置的**库函数**，不该混进 `[解释器] …` 那行的名字清单里；
# 它仍然可用，发现渠道是工具描述（每次请求都在）。
# ⚠ 只管**显示**：用户写 `history = 5` 照样会盖掉它。
shell.user_ns_hidden["history"] = history


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
    before = set(_namespace())
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
    # 解释器状态随每一帧回去（异常帧也要）：模型看不到解释器内部，压缩之后它连代码块都没了
    # —— 有这一行，它才不会按记忆里的变量名瞎写。Rust 侧拼成输出**头区**那行 `[解释器] …`。
    names = _namespace()
    payload["state"] = {
        "names": names[:32],
        "total": len(names),
        "defined": [n for n in names if n not in before][:16],
    }
    _send(payload)
