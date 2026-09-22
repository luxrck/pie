"""pie_rs —— pie-rs（Rust 版 agent harness）的 Python 绑定。

与纯 Python 的 ``pie`` 包**并存**：那个是 Textual TUI 那套，这个是核心层（config / llm /
tools / session / context）的原生扩展，两边共用同一份 ``~/.pie/``（配置文件、会话 JSONL、
压缩记录、图片副本）。

最小用法：

.. code-block:: python

    import pie_rs

    cfg = pie_rs.Config.load()                 # ~/.pie/config.toml
    cfg.model = "deepseek-flash"
    llm = pie_rs.LlmClient(cfg)
    tools = pie_rs.ToolRegistry.builtins(cfg)
    session = pie_rs.Session.ephemeral(cfg, llm, tools)   # 不落盘

    answer = session.aturn("看看当前目录", on_event=lambda ev: print(ev["type"]))
    print(answer)

约定（详见仓内 ``docs/python-bindings.md``）：
  - **同步外观**：``aturn`` 阻塞到回合结束，期间**释放 GIL**（别的 Python 线程照常跑）。
    要并发就用 ``asyncio.to_thread``；原生 ``await`` 版本是后续里程碑。
  - **事件是 dict**：键名与纯 Python 版 ``loop.aturn`` 的 ``on_event`` 一致
    （``content_delta`` / ``reasoning_delta`` / ``tool_call`` / ``tool_result`` / ``answer``）。
  - **消息是 dict**：``session.messages`` 返回 dict 列表，字段名与 JSONL 文件一致
    （含压缩元数据），因此与 Python 版写下的会话可以互读。
  - **一个 Session 同一时刻只跑一个回合**：回合进行中再调 ``aturn`` / 读属性会抛
    ``RuntimeError("session 正忙")``；**事件回调里不要碰同一个 Session**。
"""

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
    version,
)

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
    "version",
]

__version__ = version()
