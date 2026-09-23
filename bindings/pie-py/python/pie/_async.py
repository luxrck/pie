"""asyncio 那一层胶水（M5）：`await session.aturn_async(...)` + `async for ev in session.events()`。

为什么胶水在 Python 侧（规划 §5.2 的三个坑）：

1. **事件怎么给**：Rust 侧从 tokio 线程回调，只能经 `loop.call_soon_threadsafe(queue.put_nowait, ev)`
   ——`asyncio.Queue` 不是线程安全的。用户代码仍然不在 tokio 线程上跑。
2. **取消语义**：`task.cancel()` **不会**停 Rust 侧的 future（tokio 任务照跑到结束）→ 这里捕
   `CancelledError` 后调 `session.stop()`，才真能停住模型请求与 shell 子进程。
3. **loop / runtime 生命周期**：Rust 侧用的是**进程级** runtime（`pyo3_async_runtimes::tokio`），
   不随 asyncio loop 生死；但事件队列是按 loop 的 —— **一个进程一个 loop 最稳**
   （`asyncio.run()` 跑两次：第二次的事件队列是新 loop 的，没问题；跨线程同时用两个 loop 才要小心）。
"""

from __future__ import annotations

import asyncio
from typing import Any

__all__ = ["aturn_async"]

# 哨兵：Rust 侧回合结束时推 `None`，这里换成它；用户看不到（迭代器见到就停）
_DONE = object()


class _Events:
    """`async for ev in session.events()` 的迭代器：从队列取事件，见到哨兵就结束。"""

    __slots__ = ("_queue",)

    def __init__(self, queue: "asyncio.Queue[Any]") -> None:
        self._queue = queue

    def __aiter__(self) -> "_Events":
        return self

    async def __anext__(self) -> dict[str, Any]:
        item = await self._queue.get()
        if item is _DONE:
            raise StopAsyncIteration
        return item


async def aturn_async(
    session: Any,
    input: str,
    *,
    queue: "asyncio.Queue[Any]",
    cancel: Any = None,
    max_steps: int | None = None,
    stream: bool | None = None,
    parallel_tools: bool | None = None,
) -> str:
    """`Session.aturn_async` 的实现体（Rust 侧只负责建队列 + 转交到这儿）。"""
    loop = asyncio.get_running_loop()

    def sink(payload: Any) -> None:
        # 从 tokio 线程被调用：只做「丢进队列」这一件事（队列本身线程不安全）
        loop.call_soon_threadsafe(queue.put_nowait, _DONE if payload is None else payload)

    fut = session.turn_future(input, sink, cancel, max_steps, stream, parallel_tools)
    try:
        return await fut
    except asyncio.CancelledError:
        # ⚠ 见模块 docstring 第 2 条：不 stop 的话 Rust 侧会继续烧 token
        session.stop()
        raise
