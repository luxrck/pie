"""事件循环收尾（`aio.py`）回归测试：残留异步生成器关干净、收尾不再报错（无 pytest 依赖）。

跑法（任一）：

    uv run python tests/test_aio.py
    pytest tests/test_aio.py          # 装了 pytest 也能直接跑

背景：`pie -p 你好` 收尾时会冒一段
`RuntimeError: generator didn't stop after athrow()`（httpx2/httpcore2 的
「响应字节流」生成器在 `asyncio.run` 收尾时被按错误顺序 aclose）。`aio.run`
在收尾前先 `close_asyncgens()`（多轮、吞异常、与顺序无关），所以：

- 循环里残留的异步生成器会被真的关掉（`ag_frame is None`）；
- 关闭时抛错的（模拟 httpcore2）不会冒到默认异常处理器——
  用「`loop.set_exception_handler` 记事件，断言收尾阶段一个事件都没有」来验。
"""

from __future__ import annotations

import asyncio
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "src"))

from pie import aio  # noqa: E402


async def _hang():
    """永远停在 yield 上的异步生成器（模拟「没读完就 break」的响应流）。"""
    yield 1


_KEEP: list[object] = []  # 撑住假生成器：不让引用计数在 main 返回时就把它们回收掉


class _Unruly:
    """前 N 次 `aclose()` 抛 RuntimeError 的假生成器（模拟 httpcore2 的顺序问题）。

    `close_asyncgens()` 只要求对象有 `aclose()`，所以可以塞进 `loop._asyncgens` 冒充。
    """

    def __init__(self, fails: int = 1) -> None:
        self.fails = fails
        self.closed = 0

    async def aclose(self) -> None:
        if self.fails:
            self.fails -= 1
            raise RuntimeError("generator didn't stop after athrow()")
        self.closed += 1


def test_close_asyncgens_closes_leftovers() -> None:
    """残留的异步生成器被真关掉；之后再走原生 `shutdown_asyncgens()` 无事发生。"""

    async def main() -> None:
        events: list[dict] = []
        loop = asyncio.get_running_loop()
        loop.set_exception_handler(lambda _loop, ctx: events.append(ctx))
        gens = []
        for _ in range(3):
            g = _hang()
            assert await g.__anext__() == 1
            assert g.ag_frame is not None  # 起来后挂起在 yield
            gens.append(g)
        assert len(list(loop._asyncgens)) >= 3
        closed = await aio.close_asyncgens()
        assert closed >= 3, closed
        assert all(g.ag_frame is None for g in gens)  # 真被关了
        await loop.shutdown_asyncgens()  # 已关干净 → 不应该再报给异常处理器
        assert events == [], events

    aio.run(main())


def test_close_asyncgens_retries_unruly_ones() -> None:
    """第一次关失败的留给下一轮，不抛异常。"""

    async def main() -> None:
        loop = asyncio.get_running_loop()
        bad = _Unruly(fails=1)
        loop._asyncgens.add(bad)  # type: ignore[arg-type]  # WeakSet 只要弱引用得动
        await aio.close_asyncgens()
        assert bad.fails == 0 and bad.closed == 1

    aio.run(main())


def test_run_keeps_shutdown_quiet() -> None:
    """aio.run 收尾不把生成器关闭失败报给默认异常处理器（核心回归）。

    假生成器必须用 `_KEEP` 撑住：否则 `main` 一返回引用计数就回收，
    `loop._asyncgens` 变空，原生 asyncio.run 也不会报错（假绿）。
    """
    events: list[dict] = []
    _KEEP.clear()

    async def main() -> None:
        loop = asyncio.get_running_loop()
        loop.set_exception_handler(lambda _loop, ctx: events.append(ctx))
        for _ in range(2):
            bad = _Unruly(fails=1)
            _KEEP.append(bad)
            loop._asyncgens.add(bad)  # type: ignore[arg-type]

    aio.run(main())
    # 原生 asyncio.run 在这里会收到 2 条 "an error occurred during closing of
    # asynchronous generator ..."（RuntimeError），aio.run 不会。
    assert events == [], events
    _KEEP.clear()


def test_run_still_propagates_exceptions() -> None:
    """收尾逻辑不改语义：协程里的异常照常抛出。"""

    async def boom() -> None:
        raise ValueError("boom")

    try:
        aio.run(boom())
    except ValueError as exc:
        assert str(exc) == "boom"
    else:  # pragma: no cover
        raise AssertionError("异常没有传播出来")


def _main() -> int:
    tests = [(n, f) for n, f in sorted(globals().items()) if n.startswith("test_") and callable(f)]
    failed = []
    for name, func in tests:
        try:
            func()
        except AssertionError as exc:
            failed.append((name, exc))
            print(f"[FAIL] {name}: {exc}")
        else:
            print(f"[PASS] {name}")
    print(f"\n{len(tests) - len(failed)}/{len(tests)} 通过")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(_main())
