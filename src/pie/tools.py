"""工具层：内置 read / edit / write / shell，以及可扩展的工具注册表（包名 pie）。

加一个新工具三步：
    1. 写一个普通函数，参数带类型注解；
    2. 用 @tool() 装饰（自动从签名生成 JSON schema）；
    3. 注册进 ToolRegistry。
"""

from __future__ import annotations

import inspect
import subprocess
import types
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Union, get_args, get_origin, get_type_hints

from .context import write_raw

MAX_TOOL_OUTPUT = 20_000  # 单个工具返回给模型的最大字符数


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
    handler: Callable[..., str]

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
            if pname in ("self", "cls"):
                continue
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


class ToolRegistry:
    """按名字管理工具：注册、生成 OpenAI 定义、按名字调用。"""

    def __init__(self) -> None:
        self._tools: dict[str, Tool] = {}

    def register(self, t: Tool) -> Tool:
        if t.name in self._tools:
            raise ValueError(f"工具已存在: {t.name}")
        self._tools[t.name] = t
        return t

    def unregister(self, name: str) -> None:
        self._tools.pop(name, None)

    def get(self, name: str) -> Tool | None:
        return self._tools.get(name)

    def names(self) -> list[str]:
        return list(self._tools)

    def definitions(self) -> list[dict[str, Any]]:
        return [t.definition() for t in self._tools.values()]

    def dispatch(self, name: str, args: dict[str, Any]) -> str:
        t = self._tools.get(name)
        if t is None:
            raise ToolError(f"未知工具: {name}（可用: {', '.join(self._tools)}）")
        return t.handler(**args)


# ---------------------------------------------------------------- 内置工具


@tool(
    parameters={
        "path": {"type": "string", "description": "Path to the file to read (relative or absolute)"},
        "offset": {"type": "integer", "description": "Line number to start reading from (1-indexed)"},
        "limit": {"type": "integer", "description": "Maximum number of lines to read"},
    }
)
def read(path: str, offset: int | None = None, limit: int | None = None) -> str:
    """读取文件内容（不截断）；大文件用 offset（1 起）/ limit 分页读取。"""
    p = Path(path)
    if not p.exists():
        raise ToolError(f"文件不存在: {path}")
    try:
        content = p.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        return f"[二进制文件，大小 {p.stat().st_size} 字节，无法按文本读取]"
    if offset is not None or limit is not None:
        lines = content.splitlines()
        total = len(lines)
        if offset is not None and (offset < 1 or offset > total):
            raise ToolError(f"offset 无效: {offset}（文件共 {total} 行）")
        if limit is not None and limit <= 0:
            raise ToolError(f"limit 无效: {limit}（必须为正整数）")
        start = max(0, (offset or 1) - 1)
        picked = lines[start : start + limit] if limit is not None else lines[start:]
        return f"[行 {start + 1}-{start + len(picked)}，共 {total} 行]\n" + "\n".join(picked)
    return content


@tool(
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
    """一次调用做多个精确替换：每个 oldText 在原文中必须唯一且互不重叠，按原文一次性应用。"""
    p = Path(path)
    if not p.exists():
        raise ToolError(f"文件不存在: {path}")
    content = p.read_text(encoding="utf-8")
    if not edits:
        raise ToolError("edits 不能为空")

    replacements: list[tuple[int, int, str]] = []  # (start, end, newText)，基于原文位置
    labels: list[str] = []
    for i, e in enumerate(edits):
        old, new = e.get("oldText"), e.get("newText", "")
        if not isinstance(old, str) or old == "":
            raise ToolError(f"edits[{i}].oldText 必须是非空字符串")
        matched = content.count(old)
        if matched == 0:
            raise ToolError(f"edits[{i}].oldText 在 {path} 中找不到（区分大小写）：{old[:200]!r}")
        if matched > 1:
            raise ToolError(f"edits[{i}].oldText 在 {path} 中出现 {matched} 次，必须唯一：{old[:200]!r}")
        start = content.index(old)
        replacements.append((start, start + len(old), new))
        labels.append(f"edits[{i}]")

    order = sorted(range(len(replacements)), key=lambda k: replacements[k][0])
    for a, b in zip(order, order[1:]):
        if replacements[b][0] < replacements[a][1]:
            raise ToolError(
                f"edits 存在重叠：{labels[a]} 与 {labels[b]}（相邻/重叠改动请合并成一个 edit）"
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
    return f"已替换 {len(edits)} 处: {path}"


@tool()
def write(path: str, content: str) -> str:
    """把 content 写入 path，覆盖已有内容并自动创建父目录。"""
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(content, encoding="utf-8")
    return f"已写入 {p}（{len(content)} 字符，{content.count(chr(10)) + 1} 行）"


@tool()
def shell(cmd: str, timeout: int = 120, cwd: str | None = None, limit: int = 200) -> str:
    """执行 shell 命令，返回 stdout/stderr 与退出码；输出超过 limit 行时全文落盘，只返回指针 + 最后 limit 行（YOLO，无权限确认）。"""
    if limit <= 0:
        raise ToolError("limit 必须为正整数")
    try:
        r = subprocess.run(
            cmd, shell=True, cwd=cwd, capture_output=True, text=True, timeout=timeout
        )
    except subprocess.TimeoutExpired:
        return f"[shell] 命令超过 {timeout}s 超时，可能仍在后台运行：{cmd}"
    out = (r.stdout or "") + (r.stderr or "")
    lines = out.splitlines()
    if len(lines) > limit:
        path = write_raw(out, "shell")  # 全文落盘（内容 hash 寻址），上下文只留指针 + 最后 limit 行
        return f"[exit={r.returncode}]\n[shell 输出全文已保存: {path}]\n" + "\n".join(lines[-limit:])
    return f"[exit={r.returncode}]\n{out}"


def register_builtins(registry: ToolRegistry) -> ToolRegistry:
    for t in (read, edit, write, shell):
        registry.register(t)
    return registry


def default_tools() -> ToolRegistry:
    return register_builtins(ToolRegistry())


