"""循环层 `aturn(stream=...)` 回归测试：流式开关三态 + 无 stream 后端回退（无 pytest 依赖，零网络）。

跑法（任一）：

    uv run python tests/test_loop.py
    pytest tests/test_loop.py          # 装了 pytest 也能直接跑

`stream: bool | None = None`（为嵌入方手动控制）：
- None（默认）= 后端实现了 `stream()` 就用流式（保持原行为），否则 `complete()`；
- False = 强制一次性 `complete()`（此时不再有 reasoning/content 增量事件）；
- True = 强制流式；后端没实现 `stream()` 时仍回退 `complete()`（不报错）。

判据用替身后端的**调用记录**（`calls`）而不是耗时：命中哪条路径一目了然。
"""

from __future__ import annotations

import inspect
import pathlib
import sys
from typing import Any, AsyncIterator

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "src"))

from pie import aio  # noqa: E402
from pie.config import Config  # noqa: E402
from pie.context import AgentMessage, SystemMessage, UserMessage  # noqa: E402
from pie.llm import LLMResult, StreamChunk  # noqa: E402
from pie.loop import aturn, run  # noqa: E402
from pie.tools import default_tools  # noqa: E402


class _StreamBackend:
    """同时实现 complete() 与 stream() 的替身（记录走了哪条路）。"""

    def __init__(self) -> None:
        self.calls: list[str] = []

    async def complete(self, messages: list[Any], tools: list[Any], model: Any = None) -> LLMResult:
        self.calls.append("complete")
        return LLMResult(content="done-complete")

    async def stream(
        self, messages: list[Any], tools: list[Any], model: Any = None
    ) -> AsyncIterator[StreamChunk]:
        self.calls.append("stream")
        yield StreamChunk(type="content", delta="he")
        yield StreamChunk(type="content", delta="llo")
        yield StreamChunk(type="done", result=LLMResult(content="hello"))


class _PlainBackend:
    """只实现 complete() 的替身（没有 stream 能力）。"""

    def __init__(self) -> None:
        self.calls: list[str] = []

    async def complete(self, messages: list[Any], tools: list[Any], model: Any = None) -> LLMResult:
        self.calls.append("complete")
        return LLMResult(content="done-plain")


def _cfg() -> Config:
    # compaction=None：完全不碰 ~/.pie（不落盘）；api_key 显式清空（默认值是本部署真 key）。
    return Config(api_key="", compaction=None, files_api=False, verbose=False)


def _messages() -> AgentMessage:
    return AgentMessage([SystemMessage("sys"), UserMessage("hi")], keep_last_steps=2)


async def _run_once(backend: Any, stream: bool | None) -> tuple[str, list[dict[str, Any]]]:
    events: list[dict[str, Any]] = []
    out = await aturn(
        _messages(), _cfg(), default_tools(), backend,
        image_files=None, on_event=events.append, stream=stream,
    )
    return out, events


def _case(backend: Any, stream: bool | None, expect_call: str, expect_out: str, expect_deltas: bool) -> None:
    out, events = aio.run(_run_once(backend, stream))
    deltas = [e for e in events if e["type"] == "content_delta"]
    assert backend.calls == [expect_call], f"stream={stream!r} 走了 {backend.calls}"
    assert out == expect_out, out
    assert bool(deltas) is expect_deltas, f"stream={stream!r} 增量 {len(deltas)} 个"


def test_auto_streams_when_backend_supports_it() -> None:
    """stream=None（默认）+ 后端有 stream() → 走流式，增量照常推送。"""
    _case(_StreamBackend(), None, "stream", "hello", True)


def test_false_forces_complete() -> None:
    """stream=False + 后端有 stream() → 强制一次性，没有增量事件。"""
    _case(_StreamBackend(), False, "complete", "done-complete", False)


def test_true_forces_stream() -> None:
    """stream=True + 后端有 stream() → 走流式。"""
    _case(_StreamBackend(), True, "stream", "hello", True)


def test_plain_backend_falls_back_on_everything() -> None:
    """后端没有 stream()：三种取值都回退 complete（不报错）。"""
    for stream in (None, False, True):
        _case(_PlainBackend(), stream, "complete", "done-plain", False)


def test_run_forwards_stream() -> None:
    """run() 是同步包装，同样透传 stream（签名里能传）。"""
    assert "stream" in inspect.signature(run).parameters
    assert "stream" in inspect.signature(aturn).parameters


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
