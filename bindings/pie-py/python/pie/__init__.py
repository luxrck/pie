"""pie —— pie-rs（Rust 版 agent harness）的 Python 绑定。

与纯 Python 的 ``pie`` 包**并存**：那个是 Textual TUI 那套，这个是核心层（config / llm /
tools / session / context）的原生扩展，两边共用同一份 ``~/.pie/``（配置文件、会话 JSONL、
压缩记录、图片副本）。

最小用法：

.. code-block:: python

    import pie

    cfg = pie.Config.load()                 # ~/.pie/config.toml
    cfg.model = "deepseek-flash"
    llm = pie.LlmClient(cfg)
    tools = pie.ToolRegistry.builtins(cfg)
    session = pie.Session.ephemeral(cfg, llm, tools)   # 不落盘

    answer = session.aturn("看看当前目录", on_event=lambda ev: print(ev["type"]))
    print(answer)

一次性用法（无会话、不落盘）：``print(pie.run("总结这个仓库"))``。

约定（详见仓内 ``docs/python-bindings.md``）：
  - **同步外观**：``aturn`` 阻塞到回合结束，期间**释放 GIL**（别的 Python 线程照常跑）。
    要并发就用 ``asyncio.to_thread``；原生 ``await`` 版本是后续里程碑。
  - **事件是 dict**：键名与纯 Python 版 ``loop.aturn`` 的 ``on_event`` 一致
    （``content_delta`` / ``reasoning_delta`` / ``tool_call`` / ``tool_result`` / ``answer``）。
    ⚠ 一处**有意不同**：``tool_result.text`` **不截断**（纯 Python 版截到 500 字）——
    要少显示自己截；同理，一批多个 tool_call 时事件顺序是「先全部 tool_call、再按完成顺序
    tool_result」（``parallel_tools`` 只在并发度上有区别）。
  - **消息是 dict**：``session.messages`` 返回 dict 列表，字段名与 JSONL 文件一致
    （含压缩元数据），因此与 Python 版写下的会话可以互读。
  - **一个 Session 同一时刻只跑一个回合**：回合进行中再调 ``aturn`` / 读属性会抛
    ``RuntimeError("session 正忙")``；**事件回调里不要碰同一个 Session**。
"""

from typing import TYPE_CHECKING

from ._pie_rs import (
    Cancel,
    Config,
    ConfigError,
    LlmClient,
    LlmError,
    PieError,
    Session,
    ToolError,
    ToolRegistry,
    list_sessions,
    run,
    version,
)

from ._tool import Tool, tool

__all__ = [
    "Cancel",
    "Config",
    "ConfigError",
    "LlmClient",
    "LlmError",
    "PieError",
    "Session",
    "ToolError",
    "ToolRegistry",
    "Tool",
    "list_sessions",
    "run",
    "tool",
    "version",
]

# 类型专用的名字（只在 `.pyi` 里存在，运行时没有）——放进 `__all__` 会让 `import *` 炸，
# 所以只给类型检查器看：`if TYPE_CHECKING` 块里头导进来即可。
if TYPE_CHECKING:
    # ⚠ 必须写成 `X as X`（显式再导出）：`--strict` 下 mypy 不认隐式再导出，
    # 写成普通 import 的话 `pie.TurnEvent` 在外面就「未定义」。
    from ._pie_rs import AnswerEvent as AnswerEvent
    from ._pie_rs import Balance as Balance
    from ._pie_rs import BalanceInfo as BalanceInfo
    from ._pie_rs import CompactStats as CompactStats
    from ._pie_rs import ContentDelta as ContentDelta
    from ._pie_rs import Message as Message
    from ._pie_rs import ReasoningDelta as ReasoningDelta
    from ._pie_rs import SessionRow as SessionRow
    from ._pie_rs import ToolCallEvent as ToolCallEvent
    from ._pie_rs import ToolResultEvent as ToolResultEvent
    from ._pie_rs import TurnEvent as TurnEvent
    from ._pie_rs import Usage as Usage

__version__ = version()
