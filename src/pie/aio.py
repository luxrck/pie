"""事件循环收尾：替代 `asyncio.run` 的薄封装（包名 pie）。

背景（2026-09 修）：CLI 每个回合跑一次 `asyncio.run(...)`，收尾时 CPython 会调
`loop.shutdown_asyncgens()`。而 openai 的流式响应一旦读到 SSE 的 `[DONE]` 就
**就地 break**（`AsyncStream.__stream__`），于是 httpx2 / httpcore2 那串「响应
字节流」异步生成器会一直挂起在 yield 上：

    AsyncStream.__stream__ → SSEDecoder.aiter_bytes → Response.aiter_bytes
      → Response.aiter_raw → PoolByteStream.__aiter__
      → HTTP11ConnectionByteStream.__aiter__ → safe_async_iterate
      → AsyncHTTP11Connection._receive_response_body

`shutdown_asyncgens()` 会按 `loop._asyncgens`（WeakSet）的顺序**一次性**对它们
调 `aclose()`；一旦「内层先关」，httpcore2 的 `safe_async_iterate` 就会抛
`RuntimeError("generator didn't stop after athrow()")`，默认异常处理器把它打成
一大段 Traceback 到 stderr（`pie -p 你好` 即可复现）。顺序由对象地址决定，所以
时好时坏、看起来像随机故障。连接其实早就被正确释放了（`HTTP11ConnectionByteStream
.aclose()` 在抛错之前已经调用过），纯属收尾噪音。

这里在收尾之前自己关一遍：分多轮重扫、单次失败只当「没关成」留给下一轮——
与关闭顺序无关，几轮后必然清空（实测 2 轮）。
（长驻循环里这些生成器是被 GC 逐个回收关闭的，本来就不会出这个错。）
"""

from __future__ import annotations

import asyncio
import contextlib
import gc
from typing import Any, Coroutine, Generator, TypeVar

_T = TypeVar("_T")

_ROUNDS = 4  # 收尾尝试轮数（实测 2 轮足够，留余量）


async def close_asyncgens(loop: asyncio.AbstractEventLoop | None = None) -> int:
    """关掉当前事件循环里残留的异步生成器；返回成功 `aclose()` 的次数。

    与 `loop.shutdown_asyncgens()` 的差别：**分多轮**，每轮重扫一遍活着的生成器——
    关成功的下一轮是无害空操作，关失败的下一轮状态已变（外层被关掉会连带关内层），
    因此不依赖关闭顺序；单个生成器关失败不抛异常。拿不到循环内部集合时降级为
    「什么都不做」（仍走原生 shutdown_asyncgens）。
    """
    loop = loop or asyncio.get_running_loop()
    pool = getattr(loop, "_asyncgens", None)  # CPython 内部结构（3.6+ 稳定）
    closed = 0
    for _ in range(_ROUNDS):
        if pool is None:
            break
        gens = list(pool)
        if not gens:
            break
        failed = 0
        for gen in gens:
            try:
                await gen.aclose()
                closed += 1
            except BaseException:  # noqa: BLE001 —— 收尾路径：只当“没关成”
                failed += 1
        if not failed:  # 本轮全关成功：收工
            break
        gc.collect()  # 顺手回收这一轮解链掉的对象，不留给解释器退出
    return closed


async def _with_cleanup(coro: Coroutine[Any, Any, _T]) -> _T:
    """跑完（或抛错）后先把残留异步生成器关干净，再让 asyncio.run 收尾。"""
    try:
        return await coro
    finally:
        with contextlib.suppress(Exception):
            await close_asyncgens()


def run(coro: Coroutine[Any, Any, _T]) -> _T:
    """`asyncio.run` 的替代：收尾前关干净残留异步生成器。

    其余语义与 `asyncio.run` 一致（执行完取消残留任务、`shutdown_asyncgens()`、
    `shutdown_default_executor()`、关闭循环）。同步入口（CLI / `run_agent` /
    `ToolRegistry.dispatch`）统一用它，避免那种随机的收尾 Traceback。
    """
    return asyncio.run(_with_cleanup(coro))


def _cancel_pending_tasks(loop: asyncio.AbstractEventLoop) -> None:
    """取消还没结束的任务并等它们收完（对齐 asyncio.run 的语义）。"""
    tasks = asyncio.all_tasks(loop)
    if not tasks:
        return
    for task in tasks:
        task.cancel()
    loop.run_until_complete(asyncio.gather(*tasks, return_exceptions=True))


@contextlib.contextmanager
def event_loop() -> Generator[asyncio.AbstractEventLoop, None, None]:
    """自建事件循环并交给调用方（如 `textual.App.run(loop=...)`），退出时收尾干净。

    循环由这里持有，所以能在 `close()` 之前先取消残留任务 + `close_asyncgens()`
    （这些生成器什么时候被 GC 回收是不确定的，早点关掉更干净）。
    """
    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)
    try:
        yield loop
    finally:
        with contextlib.suppress(Exception):
            _cancel_pending_tasks(loop)
        for step in (
            close_asyncgens(loop),
            loop.shutdown_asyncgens(),
            loop.shutdown_default_executor(),
        ):
            with contextlib.suppress(Exception):
                loop.run_until_complete(step)
        asyncio.set_event_loop(None)
        loop.close()
