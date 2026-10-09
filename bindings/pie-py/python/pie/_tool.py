"""`@pie.tool`：把普通函数变成工具（类型注解 → JSON schema）。

规则与 schema 形状：
`str` / `int` / `float` / `bool` / `list[...]` / `dict` / `Optional[...]` 认，其余注解直接报错；
下划线开头的参数（注入项）不进 schema；描述取 `description` → docstring 首行 → 函数名。
三种写法都行：`@tool`（裸用）/ `@tool()` / `@tool(name="…", description="…")`。

`async def` 也**能**装饰：`pytool` 宿主有后台 loop，会跑协程（同步 handler 仍走线程池）。
**绑定**跑不了协程，所以它那边的 `ToolRegistry.register(...)` 会在注册那一步拦。
"""

from __future__ import annotations

import inspect
import types
from dataclasses import dataclass
from typing import Any, Callable, Union, get_args, get_origin, get_type_hints, overload

__all__ = ["Tool", "tool"]


@dataclass
class Tool:
    """一个可注册的工具（`@pie.tool` 的产物；`ToolRegistry.register(tool)` 消费它）。"""

    name: str
    description: str
    parameters: dict[str, Any]
    handler: Callable[..., str]

    def definition(self) -> dict[str, Any]:
        """OpenAI 线上形状的工具定义（与 `ToolRegistry.specs()` 里那条同形）。"""
        return {
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            },
        }


def _type_to_schema(annotation: Any) -> dict[str, Any]:
    """Python 类型注解 → JSON schema 片段（支持 str/int/float/bool/list/dict/Optional）。"""
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
    if origin is Union or origin is types.UnionType:  # typing.Union 与 PEP 604 的 `str | None`
        non_none = [a for a in args if a is not type(None)]
        if len(non_none) == 1:
            return _type_to_schema(non_none[0])
    raise ValueError(
        f"不支持的参数类型注解: {annotation!r}（支持 str/int/float/bool/list/dict/Optional）"
    )


@overload
def tool(fn: Callable[..., str]) -> Tool: ...


@overload
def tool(
    name: str | None = None,
    description: str | None = None,
    parameters: dict[str, dict[str, Any]] | None = None,
) -> Callable[[Callable[..., str]], Tool]: ...


def tool(
    name: str | Callable[..., str] | None = None,
    description: str | None = None,
    parameters: dict[str, dict[str, Any]] | None = None,
):
    """把普通函数变成 [`Tool`][pie.Tool]：类型注解自动生成参数 schema。

    三种写法都行：`@tool`（裸用）/ `@tool()` / `@tool(name="…", description="…")`。
    `parameters` 传入时按参数名覆盖自动生成的结果（例：给某个字段补 `"enum"`）。
    """

    # `@tool` 裸用（不带括号）时 Python 把被装饰的函数当**第一个位置参数**递进来。
    # 不认这一种会返回 `decorate` 而不是 `Tool`：调用方（pytool 宿主 / 绑定）只挑 `Tool` 实例，
    # 于是那个工具**静默消失**（连一条告警都没有），而且裸用还顺带跳过了下面那道 async 检查。
    # 真踩过 —— 所以裸用必须认。
    direct = name if name is not None and not isinstance(name, str) else None
    tool_name = name if isinstance(name, str) else None

    def decorate(fn: Callable[..., str]) -> Tool:
        sig = inspect.signature(fn)
        try:
            hints = get_type_hints(fn)  # 处理 `from __future__ import annotations` 的字符串注解
        except Exception:
            hints = {}
        props: dict[str, Any] = {}
        required: list[str] = []
        for pname, param in sig.parameters.items():
            if pname in ("self", "cls") or pname.startswith("_"):
                continue  # 下划线开头的参数是注入项，不进 schema
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
            name=tool_name or fn.__name__,
            description=desc,
            parameters={"type": "object", "properties": props, "required": required},
            handler=fn,
        )

    return decorate(direct) if direct is not None else decorate
