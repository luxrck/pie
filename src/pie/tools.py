"""工具层：内置 read / edit / write / shell，以及可扩展的工具注册表（包名 pie）。

加一个新工具三步：
    1. 写一个普通函数，参数带类型注解；
    2. 用 @tool() 装饰（自动从签名生成 JSON schema）；
    3. 注册进 ToolRegistry。
"""

from __future__ import annotations

import asyncio
import difflib
import inspect
import os
import re
import signal
import types
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Awaitable, Callable, Union, get_args, get_origin, get_type_hints

from . import aio
from .context import write_raw


MAX_TOOL_OUTPUT = 20_000  # 单个工具返回给模型的最大字符数
DEFAULT_MAX_IMAGE_BYTES = 32 * 1024 * 1024  # read 内联图片的默认字节上限（可由配置注入 _max_image_bytes 覆盖）


class ToolError(Exception):
    """工具执行失败但可恢复，错误信息会回传给模型让它修正。"""


def clip_output(s: str, limit: int = MAX_TOOL_OUTPUT) -> str:
    if len(s) <= limit:
        return s
    return s[:limit] + f"\n...[输出过长，已截断，共 {len(s)} 字符]"


def _type_to_schema(annotation: Any) -> dict[str, Any]:
    """把 Python 类型注解转成 OpenAI JSON schema 片段。"""
    if annotation is str:
        return {"type": "string"}
    if annotation is int:
        return {"type": "integer"}
    if annotation is float:
        return {"type": "number"}
    if annotation is bool:
        return {"type": "boolean"}
    origin = get_origin(annotation)
    args = get_args(annotation)
    if annotation is list or origin is list:
        return {"type": "array", "items": _type_to_schema(args[0] if args else str)}
    if annotation is dict or origin is dict:
        return {"type": "object"}
    if origin is Union or origin is types.UnionType:  # 兼容 typing.Union 与 PEP 604 的 str | None
        non_none = [a for a in args if a is not type(None)]
        if len(non_none) == 1:
            return _type_to_schema(non_none[0])
    raise ValueError(
        f"不支持的参数类型注解: {annotation!r}（支持 str/int/float/bool/list/dict/Optional）"
    )


@dataclass
class Tool:
    name: str
    description: str
    parameters: dict[str, Any]
    handler: Callable[..., str | Awaitable[str]]  # 支持 async 函数（异步工具）

    def definition(self) -> dict[str, Any]:
        """生成 OpenAI 接口要求的工具定义。"""
        return {
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            },
        }


def tool(
    name: str | None = None,
    description: str | None = None,
    parameters: dict[str, dict[str, Any]] | None = None,
) -> Callable[[Callable[..., str]], Tool]:
    """把普通函数变成 Tool：类型注解自动生成参数 schema。

    parameters 传入时按名字覆盖自动生成的结果。
    """

    def decorate(fn: Callable[..., str]) -> Tool:
        sig = inspect.signature(fn)
        try:
            hints = get_type_hints(fn)  # 处理 from __future__ import annotations 的字符串注解
        except Exception:
            hints = {}
        props: dict[str, Any] = {}
        required: list[str] = []
        for pname, param in sig.parameters.items():
            if pname in ("self", "cls") or pname.startswith("_"):
                continue  # 下划线开头的参数（如 _on_progress）是注入项，不进 schema
            if parameters and pname in parameters:
                schema = parameters[pname]
            else:
                anno = hints.get(pname, param.annotation)
                if anno is inspect.Parameter.empty:
                    anno = str
                schema = _type_to_schema(anno)
                if param.default is not inspect.Parameter.empty and param.default is not None:
                    schema = {**schema, "default": param.default}
            props[pname] = schema
            if param.default is inspect.Parameter.empty:
                required.append(pname)
        desc = description or (fn.__doc__ or fn.__name__).strip().splitlines()[0]
        return Tool(
            name=name or fn.__name__,
            description=desc,
            parameters={"type": "object", "properties": props, "required": required},
            handler=fn,
        )

    return decorate


def _inject_tool_defaults(
    name: str,
    args: dict[str, Any],
    handler: Callable[..., Any],
    tool_defaults: dict[str, dict[str, Any]] | None,
) -> dict[str, Any]:
    """把 config.tools[name] 里的配置合并进私有参数（下划线开头）。只注入：
      - handler 签名里存在的参数；
      - args 未显式提供的（不覆盖显式传参）；
      - 默认值里有的键。
    运行时注入项（_on_progress）由 adispatch 的 on_event 接管，不在此列。"""
    if not tool_defaults:
        return args
    defaults = tool_defaults.get(name)
    if not isinstance(defaults, dict):
        return args
    injected = dict(args)
    for pname in inspect.signature(handler).parameters:
        if not pname.startswith("_"):
            continue
        if pname == "_on_progress":
            continue
        if pname in injected:
            continue
        if pname in defaults:
            injected[pname] = defaults[pname]
    return injected


class ToolRegistry:
    """按名字管理工具：注册、生成 OpenAI 定义、按名字调用。"""

    def __init__(self) -> None:
        self._tools: dict[str, Tool] = {}

    def register(self, t: Tool) -> Tool:
        if t.name in self._tools:
            raise ValueError(f"工具已存在: {t.name}")
        self._tools[t.name] = t
        return t

    def get(self, name: str) -> Tool | None:
        return self._tools.get(name)

    def definitions(self) -> list[dict[str, Any]]:
        return [t.definition() for t in self._tools.values()]

    def dispatch(
        self,
        name: str,
        args: dict[str, Any],
        tool_defaults: dict[str, dict[str, Any]] | None = None,
    ) -> str:
        """同步分发（须在无事件循环的线程调用）：async 工具用 aio.run 包装。"""
        t = self._tools.get(name)
        if t is None:
            raise ToolError(f"未知工具: {name}（可用: {', '.join(self._tools)}）")
        handler = t.handler
        args = _inject_tool_defaults(name, args, handler, tool_defaults)
        if inspect.iscoroutinefunction(handler):
            return aio.run(handler(**args))
        return handler(**args)

    async def adispatch(
        self,
        name: str,
        args: dict[str, Any],
        on_event: Callable[[dict[str, Any]], None] | None = None,
        tool_defaults: dict[str, dict[str, Any]] | None = None,
    ) -> str:
        """异步分发：async 工具直接 await；同步工具丢线程池（不阻塞事件循环）。

        on_event 非 None 时注入给接受 `_on_progress` 参数的异步工具（如 shell
        的实时输出回调）；同步工具无法实时，忽略该参数。
        """
        t = self._tools.get(name)
        if t is None:
            raise ToolError(f"未知工具: {name}（可用: {', '.join(self._tools)}）")
        handler = t.handler
        args = _inject_tool_defaults(name, args, handler, tool_defaults)
        if inspect.iscoroutinefunction(handler):
            if on_event is not None and "_on_progress" in inspect.signature(handler).parameters:
                return await handler(**args, _on_progress=on_event)
            return await handler(**args)
        return await asyncio.to_thread(handler, **args)


# ---------------------------------------------------------------- 图片支持（read）


@dataclass
class ImageRef:
    """read 读取的图片引用：由返回文本的机器可读标记解析而来。"""

    path: str
    mime: str
    size: int  # 原始字节数
    width: int | None = None
    height: int | None = None


# 魔数嗅探：扩展名不可信，图片识别以文件头为准
_IMAGE_SIGNATURES: tuple[tuple[bytes, str], ...] = (
    (b"\x89PNG\r\n\x1a\n", "image/png"),
    (b"\xff\xd8\xff", "image/jpeg"),
    (b"GIF87a", "image/gif"),
    (b"GIF89a", "image/gif"),
    (b"RIFF", "image/webp"),  # RIFF....WEBP 需二次校验
    (b"BM", "image/bmp"),
)
_READ_HEAD_BYTES = 65_536  # 读文件头用于嗅探 + 尺寸解析（JPEG 的 SOF 段可能较靠后）

# read 返回文本中的机器可读图片标记（人类可读，loop 用 parse_image_marker 解析）
_IMAGE_MARKER_RE = re.compile(
    r"\[图片已读取: path=(?P<path>[^,\]]+), mime=(?P<mime>[^,\]]+), "
    r"size=(?P<size>\d+)(?:, dim=(?P<width>\d+)x(?P<height>\d+))?\]"
)


def _sniff_mime(head: bytes) -> str | None:
    """按魔数识别图片格式，非图片返回 None。"""
    for sig, mime in _IMAGE_SIGNATURES:
        if head.startswith(sig):
            if mime == "image/webp" and head[8:12] != b"WEBP":
                continue  # RIFF 但非 WEBP（如 WAV/AVI）
            return mime
    return None


def _image_size(mime: str, head: bytes) -> tuple[int, int] | None:
    """从文件头解析图片宽高；识别失败返回 None（绝不抛异常，尺寸仅供描述）。"""
    try:
        if mime == "image/png" and len(head) >= 24:
            return (
                int.from_bytes(head[16:20], "big"),
                int.from_bytes(head[20:24], "big"),
            )
        if mime == "image/gif" and len(head) >= 10:
            return (
                int.from_bytes(head[6:8], "little"),
                int.from_bytes(head[8:10], "little"),
            )
        if mime == "image/bmp" and len(head) >= 26:
            w = int.from_bytes(head[18:22], "little")
            h = int.from_bytes(head[22:26], "little")
            return (w, abs(h)) if w else None
        if mime == "image/jpeg":  # 扫 marker 找 SOFn 段（宽高 BE 各 2 字节）
            sof = {0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF}
            i, n = 2, len(head)
            while i + 9 < n:
                if head[i] != 0xFF:
                    i += 1
                    continue
                marker = head[i + 1]
                if marker in (0xD8, 0x01) or 0xD0 <= marker <= 0xD7:  # 无长度段
                    i += 2
                    continue
                seg_len = int.from_bytes(head[i + 2 : i + 4], "big")
                if marker in sof:
                    return (
                        int.from_bytes(head[i + 7 : i + 9], "big"),
                        int.from_bytes(head[i + 5 : i + 7], "big"),
                    )
                i += 2 + seg_len
            return None
        if mime == "image/webp" and len(head) >= 30:
            fourcc = head[12:16]
            if fourcc == b"VP8X":  # canvas 尺寸 24bit LE，实际值 = 存储值 + 1
                return (
                    int.from_bytes(head[24:27], "little") + 1,
                    int.from_bytes(head[27:30], "little") + 1,
                )
            if fourcc == b"VP8 " and len(head) >= 30:  # lossy 关键帧头 14bit LE
                return (
                    int.from_bytes(head[26:28], "little") & 0x3FFF,
                    int.from_bytes(head[28:30], "little") & 0x3FFF,
                )
            if fourcc == b"VP8L" and len(head) >= 25:  # lossless
                b = head[20:25]
                w = 1 + (b[1] | ((b[2] & 0x3F) << 8))
                h = 1 + (((b[2] & 0xC0) >> 6) | (b[3] << 2) | ((b[4] & 0x0F) << 10))
                return (w, h)
    except Exception:
        pass
    return None


def _read_image(p: Path, mime: str, head: bytes, max_bytes: int) -> str:
    """图片分支：返回机器可读标记（图片本体由 harness 读文件后按多模态消息注入）。
    offset/limit/_max_lines/_max_bytes 是文本分页概念，对图片无意义，直接忽略；
    图片只看 max_bytes（默认 DEFAULT_MAX_IMAGE_BYTES，可由配置注入 _max_image_bytes 覆盖）。"""
    size = p.stat().st_size
    if max_bytes is not None and size > max_bytes:
        raise ToolError(
            f"图片过大（{size:,} 字节 > {max_bytes:,} 上限），无法内联发送给模型；"
            "请先压缩/裁剪该图片再读取"
        )
    dim = _image_size(mime, head)
    dim_txt = f", dim={dim[0]}x{dim[1]}" if dim else ""
    return f"[图片已读取: path={p}, mime={mime}, size={size}{dim_txt}]"


def parse_image_marker(text: str) -> ImageRef | None:
    """从 read 返回文本解析图片标记；非图片读取结果返回 None。"""
    m = _IMAGE_MARKER_RE.search(text or "")
    if m is None:
        return None
    w, h = m.group("width"), m.group("height")
    return ImageRef(
        path=m.group("path"),
        mime=m.group("mime"),
        size=int(m.group("size")),
        width=int(w) if w else None,
        height=int(h) if h else None,
    )


# ---------------------------------------------------------------- 内置工具


def _lines_by_bytes(lines: list[str], start: int, max_bytes: int) -> int:
    """从 start 起累计行字节（UTF-8，含换行近似 +1）不超过 max_bytes，返回可读行数；
    至少取 1 行（文件非空时），保证单行超长也能读到内容（此时会略微超出字节预算）。"""
    n = 0
    sz = 0
    for line in lines[start:]:
        b = len(line.encode("utf-8")) + 1
        if sz + b > max_bytes and n > 0:
            break
        sz += b
        n += 1
    return n


def _tail_output(text: str, max_lines: int | None, max_bytes: int | None) -> str:
    """按行 / 字节预算截取尾部连续段（shell 超限只保留尾部）；未超限返回原文。"""
    if max_lines is None and max_bytes is None:
        return text
    lines = text.splitlines(keepends=True)
    total = len(lines)
    cap = total
    if max_lines is not None:
        cap = min(cap, max_lines)
    if max_bytes is not None:
        sz = 0
        cnt = 0
        for line in reversed(lines):
            b = len(line.encode("utf-8"))
            if sz + b > max_bytes and cnt > 0:
                break
            sz += b
            cnt += 1
        cap = min(cap, cnt)
    if cap >= total:
        return text
    return "".join(lines[-cap:])


def _format_output(headers: list[str], body: str | None = None) -> str:
    """按「Headers\\n\\nBody」统一工具返回：headers 为一行一个 `[...]` 方括号行；
    body 非空（truthy）时用空行分隔；body 为空 / 无 body 则省略空行，只返回 headers。
    Header 值内不要含 `]`。"""
    head = "\n".join(headers)
    if body:
        return head + "\n\n" + body
    return head


@tool(
    parameters={
        "path": {"type": "string", "description": "Path to the file to read (relative or absolute)"},
        "offset": {
            "type": "integer",
            "description": "Line number to start reading from (1-indexed); text files only, ignored for images",
        },
        "limit": {
            "type": "integer",
            "description": "Maximum number of lines to read; text files only, ignored for images",
        },
    }
)
def read(
    path: str,
    offset: int | None = None,
    limit: int | None = None,
    _max_lines: int | None = None,
    _max_bytes: int | None = None,
    _max_image_bytes: int | None = DEFAULT_MAX_IMAGE_BYTES,
) -> str:
    """读取文件内容：文本按 UTF-8 全文或 offset（1 起）/ limit 分页读取。
    _max_lines / _max_bytes 为私有容量上限（默认由配置 tools.read 注入，未提供则不设行数/字节限制）：
    从 offset 起点向后取连续段，行数 ≤ min(limit, _max_lines, 字节预算行数)，字节预算行数 =
    使所选行累计字节尽可能接近 _max_bytes（不超，至少 1 行）。
    图片（PNG/JPEG/GIF/WebP/BMP）返回图像引用，图像内容随多模态请求发送给模型，
    文本容量上限（offset/limit/_max_lines/_max_bytes）对图片不适用，图片只看 _max_image_bytes。"""
    p = Path(path)
    if not p.exists():
        raise ToolError(f"文件不存在: {path}")
    # 图片：先嗅探魔数（避免把大图按 UTF-8 全量解码），命中即走图片分支
    try:
        with p.open("rb") as f:
            head = f.read(_READ_HEAD_BYTES)
    except OSError as e:
        raise ToolError(f"无法读取 {path}: {e}")
    mime = _sniff_mime(head)
    if mime is not None:
        return _read_image(p, mime, head, _max_image_bytes)
    try:
        content = p.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        return f"[二进制文件，大小 {p.stat().st_size} 字节，无法按文本读取]"
    lines = content.splitlines()
    total = len(lines)
    if offset is not None and (offset < 1 or offset > total):
        raise ToolError(f"offset 无效: {offset}（文件共 {total} 行）")
    if limit is not None and limit <= 0:
        raise ToolError(f"limit 无效: {limit}（必须为正整数）")
    for label, v in (("_max_lines", _max_lines), ("_max_bytes", _max_bytes)):
        if v is not None and v <= 0:
            raise ToolError(f"{label} 无效: {v}（必须为正整数或 None）")
    start = max(0, (offset or 1) - 1)
    want = total - start
    if limit is not None:
        want = min(want, limit)
    cap = want
    if _max_lines is not None:
        cap = min(cap, _max_lines)
    if _max_bytes is not None:
        cap = min(cap, _lines_by_bytes(lines, start, _max_bytes))
    picked = lines[start : start + cap]
    omitted = want - len(picked)  # 仅容量上限导致的省略（limit 是模型显式分页，不算截断）
    body = "\n".join(picked)
    headers = [f"[行 {start + 1}-{start + len(picked)}，共 {total} 行]"]
    if omitted > 0:
        headers.append(f"[工具输出全文已保存: {write_raw(content, 'tool')}]")
    return _format_output(headers, body)


def _norm_ws(s: str) -> str:
    """去掉所有空白后的字符串（用于判断“只差缩进/换行”）。"""
    return re.sub(r"\s+", "", s)


def _nearest_fragment(content: str, old: str, *, max_diff_lines: int = 16) -> str:
    """oldText 在文件里找不到时，找出原文里**最接近**的片段，并给出一段简版 diff。

    目的：让模型一次就能看出差在哪（缩进？空行？某行写错了？），不必再 read 一遍。
    做法：拿 oldText 里**最长的一行**当锚（最可能是唯一标识），在原文里找字符级最相似的那一行，
    再以它为基准取一个同长度的窗口做 diff。返回多行文本；不够相似时返回空串（宁可不给，不给误导）。
    """
    old_lines = old.splitlines()
    if not old_lines or not content:
        return ""
    c_lines = content.splitlines()
    anchor_idx = max(range(len(old_lines)), key=lambda k: len(old_lines[k].strip()))
    anchor = old_lines[anchor_idx].strip()
    if not anchor:
        return ""
    sm = difflib.SequenceMatcher(autojunk=False)
    sm.set_seq2(anchor)                             # 字符级比对（不是拿“行列表”当元素比）
    best_i, best_r = -1, 0.0
    for i, ln in enumerate(c_lines):
        sm.set_seq1(ln.strip())
        r = sm.ratio()
        if r > best_r:
            best_i, best_r = i, r
    if best_i < 0 or best_r < 0.5:                  # 太不相似，给了反而误导
        return ""
    start = max(0, best_i - anchor_idx)
    window = c_lines[start:start + len(old_lines)]
    diff = list(difflib.unified_diff(
        old_lines, window,
        fromfile="你的 oldText", tofile=f"原文实际内容（第 {start + 1} 行起）",
        lineterm="", n=1,
    ))
    body = "\n".join(f"    {ln}" for ln in diff[:max_diff_lines])
    more = "" if len(diff) <= max_diff_lines else f"\n    …（diff 已截断，共 {len(diff)} 行）"
    return f"原文里最接近的位置在第 {start + 1} 行（锚行相似度 {best_r:.0%}）：\n{body}{more}"


@tool(
    description=(
        "一次调用做多个精确替换：每个 oldText 必须**唯一**且互不重叠，且都按**同一份原文**匹配（不是逐条叠加）。\n"
        "编辑纪律（不做很容易白花一次往返）：\n"
        "- oldText 从刚 read 到的内容里**整段复制**（含缩进与空行）；\n"
        "- 同一次调用的多条 edits **互不依赖**：不能引用另一条 edit 的新文本（要级联就分两次调用）；\n"
        "- 任何一条报错，**整次调用什么都不写入**（原子）→ 重新 read 再改，不要接着用旧内容；\n"
        "- 改动较大或相邻，就用**一条** edit 覆盖整块，不要拆成多条挨着的 edits。"
    ),
    parameters={
        "path": {"type": "string", "description": "Path to the file to edit (relative or absolute)"},
        "edits": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "oldText": {
                        "type": "string",
                        "description": (
                            "Exact text for one targeted replacement. It must be unique in the "
                            "original file and must not overlap with any other edits[].oldText "
                            "in the same call."
                        ),
                    },
                    "newText": {"type": "string", "description": "Replacement text for this targeted edit."},
                },
                "required": ["oldText", "newText"],
            },
            "description": (
                "One or more targeted replacements. Each edit is matched against the original "
                "file, not incrementally. Do not include overlapping or nested edits. If two "
                "changes touch the same block or nearby lines, merge them into one edit instead."
            ),
        },
    }
)
def edit(path: str, edits: list[dict[str, str]]) -> str:
    """一次调用做多个精确替换：每个 oldText 在原文中必须唯一且互不重叠，按原文一次性应用。

    失败时**不写入任何内容**，并尽量给出可自纠的诊断：究竟是「引用了同一次调用里另一条 edit 的
    产物」「只差空白/缩进」「出现多次」还是「原文实际长这样」（附一段简版 diff）；
    所有问题**一次报完**，省一次往返。
    """
    p = Path(path)
    if not p.exists():
        raise ToolError(f"文件不存在: {path}")
    content = p.read_text(encoding="utf-8")
    if not edits:
        raise ToolError("edits 不能为空")

    replacements: list[tuple[int, int, str]] = []  # (start, end, newText)，基于原文位置
    labels: list[str] = []
    problems: list[str] = []
    for i, e in enumerate(edits):
        old, new = e.get("oldText"), e.get("newText", "")
        if not isinstance(old, str) or old == "":
            problems.append(f"edits[{i}].oldText 必须是非空字符串")
            continue
        if not isinstance(new, str):
            problems.append(f"edits[{i}].newText 必须是字符串")
            continue
        matched = content.count(old)
        if matched == 0:
            msg = [f"edits[{i}].oldText 在 {path} 中找不到（区分大小写）：{old[:120]!r}"]
            # 诊断 1：级联——引用了同一次调用里另一条 edit 的产物（本工具对**原文**一次性应用）
            for j, other in enumerate(edits):
                other_new = other.get("newText") if isinstance(other, dict) else None
                if j != i and isinstance(other_new, str) and other_new and old in other_new:
                    msg.append(
                        f"    提示：这段文本出现在 edits[{j}].newText 里 —— edits 是对**原文**一次性应用的，"
                        f"不能引用同一次调用中另一条 edit 产生的文本；请拆成两次调用（先改前一处，再改后一处）。"
                    )
                    break
            # 诊断 2：只差空白 / 缩进 / 换行
            if _norm_ws(old) and _norm_ws(old) in _norm_ws(content):
                msg.append("    提示：忽略空白/换行后能匹配上 → 多半是缩进或空行与原文不完全一致"
                           "（请从刚读到的内容里整段复制）。")
            # 诊断 3：最接近的片段 + 简版 diff
            hint = _nearest_fragment(content, old)
            if hint:
                msg.append("    " + hint)
            problems.append("\n".join(msg))
            continue
        if matched > 1:
            problems.append(
                f"edits[{i}].oldText 在 {path} 中出现 {matched} 次，必须唯一：{old[:120]!r}"
                f"\n    提示：把 oldText 加长到能唯一确定位置（多带几行上下文），或与相邻改动合并成一个 edit。"
            )
            continue
        start = content.index(old)
        replacements.append((start, start + len(old), new))
        labels.append(f"edits[{i}]")

    order = sorted(range(len(replacements)), key=lambda k: replacements[k][0])
    for a, b in zip(order, order[1:]):
        if replacements[b][0] < replacements[a][1]:
            problems.append(
                f"edits 存在重叠：{labels[a]} 与 {labels[b]}（相邻/重叠改动请合并成一个 edit）"
            )

    if problems:
        raise ToolError(
            f"edits 有 {len(problems)} 处问题，**未写入任何内容**（本工具的 edits 对原文一次性应用）：\n"
            + "\n".join(f"- {m}" for m in problems)
        )

    parts: list[str] = []
    pos = 0
    for k in order:
        start, end, new = replacements[k]
        parts.append(content[pos:start])
        parts.append(new)
        pos = end
    parts.append(content[pos:])
    p.write_text("".join(parts), encoding="utf-8")
    return _format_output([f"[已替换 {len(edits)} 处: {path}]"])


@tool(
    parameters={
        "path": {"type": "string", "description": "Path to the file to write (relative or absolute)"},
        "content": {"type": "string", "description": "Content to write to the file"},
    }
)
def write(path: str, content: str) -> str:
    """把 content 写入 path，覆盖已有内容并自动创建父目录。"""
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(content, encoding="utf-8")
    return _format_output([f"[已写入 {p}（{len(content)} 字符，{content.count(chr(10)) + 1} 行）]"])


@tool(
    parameters={
        "command": {"type": "string", "description": "Shell command to execute"},
        "timeout": {
            "type": "integer",
            "description": "Timeout in seconds (optional, no default timeout)",
        },
    }
)
async def shell(
    command: str,
    timeout: int | None = None,
    _max_lines: int | None = None,
    _max_bytes: int | None = None,
    _on_progress: Callable[[dict[str, Any]], None] | None = None,
) -> str:
    """执行 shell 命令，返回 stdout/stderr 与退出码（YOLO，无权限确认）。

    async 实现：stdout/stderr 合并逐行读取，每行通过 _on_progress 实时推送
    （供 TUI 边执行边显示）；被取消（asyncio.CancelledError / 超时）时 kill 子进程。
    _max_lines / _max_bytes 为私有容量上限（默认由配置 tools.shell 注入，未提供则不限制）：
    超出时只保留输出尾部（行数 ≤ min(_max_lines, 字节预算行数)），并全文落盘指针，
    信息不丢；_on_progress 实时推送不受截断影响（TUI 可见全量）。
    """

    async def _run() -> tuple[int, str]:
        proc = await asyncio.create_subprocess_shell(
            command,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,  # 合并，保证输出顺序稳定
            start_new_session=True,  # 独立进程组：取消/超时时 killpg 连子孙进程一起杀
        )
        lines: list[str] = []
        try:
            while True:
                raw = await proc.stdout.readline()
                if not raw:
                    break
                line = raw.decode("utf-8", errors="replace")
                lines.append(line)
                if _on_progress is not None:
                    _on_progress(
                        {"type": "tool_progress", "name": "shell", "text": line.rstrip()}
                    )
            rc = await proc.wait()
        finally:
            if proc.returncode is None:  # 异常 / 取消 / 超时：杀整个进程组，避免僵尸 + 子孙残留
                _terminate_proc_group(proc)
                # wait 限时：正常 killpg 后 <10ms 即返回；进程若处于不可中断内核态
                # (D-state，如 NFS 卡 IO) SIGKILL 会排队暂不生效——不能无限等，
                # 否则取消/超时路径本身会被卡住
                try:
                    await asyncio.wait_for(proc.wait(), 3)
                except asyncio.TimeoutError:
                    pass
        return rc, "".join(lines)

    try:
        rc, out = await asyncio.wait_for(_run(), timeout)
    except asyncio.TimeoutError:
        return f"[shell] 命令超过 {timeout}s 超时，可能仍在后台运行：{command}"
    tail = _tail_output(out, _max_lines, _max_bytes)
    if tail == out:
        return _format_output([f"[exit={rc}]"], tail)
    spill = write_raw(out, "shell")
    return _format_output([f"[exit={rc}]", f"[工具输出全文已保存: {spill}]"], tail)


def _terminate_proc_group(proc: Any) -> None:
    """杀整个进程组（create_subprocess_shell 配 start_new_session → 组 id = pid）。

    只 kill shell 本体杀不掉它的子孙进程（真正干活的那个，且持有 stdout 管道写端）；
    asyncio 的 wait() 要等管道 EOF 才返回，子孙残留会导致取消/超时路径卡到它自然退出。
    killpg 连组一起杀，pipe 立即 EOF。"""
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        try:
            proc.kill()
        except OSError:
            pass


def register_builtins(registry: ToolRegistry) -> ToolRegistry:
    for t in (read, edit, write, shell):
        registry.register(t)
    return registry


def default_tools() -> ToolRegistry:
    return register_builtins(ToolRegistry())


BUILTIN_TOOLS = ("read", "edit", "write", "shell")


def _restricted_shell_tool(allow_cmds: list[str], base: Tool) -> Tool:
    """受限 shell 工具：command 首词必须命中白名单，否则 ToolError 回给模型修正。

    schema/参数与原 shell 一致（parameters 复用，只读）；description 追加白名单，
    让模型事先知道边界、少试错；handler 包装校验后转发原实现（含 _on_progress）。
    """
    allowed = ", ".join(allow_cmds)

    async def handler(
        command: str,
        timeout: int | None = None,
        _max_lines: int | None = None,
        _max_bytes: int | None = None,
        _on_progress: Callable[[dict[str, Any]], None] | None = None,
    ) -> str:
        stripped = command.strip()
        first = stripped.split(None, 1)[0] if stripped else ""
        if first not in allow_cmds:
            raise ToolError(
                f"[shell] 本次仅允许以这些命令开头: {allowed}（收到: {first or '(空命令)'}）"
            )
        return await base.handler(
            command,
            timeout=timeout,
            _max_lines=_max_lines,
            _max_bytes=_max_bytes,
            _on_progress=_on_progress,
        )

    return Tool(
        name="shell",
        description=base.description + f"\n本次运行仅允许以这些命令开头: {allowed}",
        parameters=base.parameters,
        handler=handler,
    )


def tools_from_spec(spec: str | None) -> ToolRegistry:
    """按 --tools 说明构建可用工具集（无 spec / 空串 → 默认全量）。

    解析优先级：内置工具名 > shell 子命令。
      - 名字 ∈ 内置（read/edit/write/shell）→ 启用该工具；
      - 其他名字 → 收集为 shell 允许的子命令白名单，并隐式启用 shell 工具；
      - 只要出现非内置名，shell 即受限（command 首词须命中白名单）；
      - 未显式列 shell 且无任何非内置名 → shell 工具禁用。

    示例:
      "read"                → 仅 read（shell 禁用）
      "read,shell"          → read + shell（不限子命令）
      "read,ls,grep,wc"     → read + shell（仅允许 ls/grep/wc）
      "ls,grep"             → 仅 shell（仅允许 ls/grep）
    """
    if not spec or not spec.strip():
        return default_tools()
    tokens = [t.strip() for t in spec.split(",") if t.strip()]
    enabled: set[str] = set()
    allow_cmds: list[str] = []
    for t in tokens:
        if t in BUILTIN_TOOLS:
            enabled.add(t)
        elif t not in allow_cmds:
            allow_cmds.append(t)
    if not enabled and not allow_cmds:
        return default_tools()
    # 全内置且无子命令白名单 = 默认全量
    if not allow_cmds and enabled == set(BUILTIN_TOOLS):
        return default_tools()

    names = set(enabled)
    if allow_cmds:
        names.add("shell")  # 有子命令白名单 → 隐式启用受限 shell
    registry = ToolRegistry()
    for t in (read, edit, write, shell):
        if t.name not in names:
            continue
        if t.name == "shell" and allow_cmds:
            registry.register(_restricted_shell_tool(allow_cmds, t))
        else:
            registry.register(t)
    return registry





