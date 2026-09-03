"""模型层：LLM 抽象 + OpenAI 兼容实现（包名 pie）。

想换模型后端，实现 complete() / stream() 协议即可（官方、DeepSeek、Qwen、
vLLM、Ollama 等 OpenAI 兼容端点直接用 OpenAILLM）。

异步为唯一主路径：complete() / stream() 都是 async。同步调用方用
asyncio.run() 包装（见 session.Session.turn）。
"""

from __future__ import annotations

import asyncio
import sys
import weakref
from dataclasses import dataclass, field
from typing import Any, AsyncIterator, Protocol

from openai import AsyncOpenAI

from .config import DEFAULT_MODEL, REASONING_NONE


@dataclass
class ToolCall:
    id: str
    name: str
    arguments: str  # JSON 字符串，由 agent 循环负责解析


@dataclass
class LLMResult:
    content: str | None
    tool_calls: list[ToolCall] = field(default_factory=list)
    prompt_tokens: int | None = None  # 本次请求的上下文 token 数（provider 上报）
    completion_tokens: int | None = None  # 本次请求的输出 token 数（provider 上报）
    reasoning_content: str | None = None  # thinking 模式的思考内容，必须原样回传


@dataclass
class StreamChunk:
    """流式响应块。

    type:
      reasoning  → delta 为思考内容增量
      content    → delta 为正文增量
      tool_call  → 工具调用增量（index/id/name/arguments 增量拼接）
      done       → result 为完整 LLMResult（含 usage / 拼装好的 tool_calls）
    """

    type: str
    delta: str = ""
    index: int = 0  # tool_call 序号（增量按 index 归并）
    id: str = ""  # tool_call id（首次出现时填充）
    name: str = ""  # 工具名（首次出现时填充）
    arguments: str = ""  # 本次增量的 arguments 片段
    result: LLMResult | None = None  # type == "done" 时的完整结果


@dataclass
class UsageTracker:
    """会话级 token 用量累计（provider 上报值）。"""

    prompt_tokens: int = 0
    completion_tokens: int = 0
    calls: int = 0
    last_prompt_tokens: int | None = None
    last_completion_tokens: int | None = None

    def record(self, prompt: int, completion: int | None = None) -> None:
        self.prompt_tokens += prompt
        if completion:
            self.completion_tokens += completion
        self.calls += 1
        self.last_prompt_tokens = prompt
        self.last_completion_tokens = completion


class LLM(Protocol):
    """任何实现 complete() 的对象都可以作为 agent 的模型后端。

    stream() 是可选的：实现了就支持流式（边生成边显示），loop 会优先使用；
    未实现则回退 complete()（一次性返回，仍可取消）。
    """

    async def complete(
        self,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]],
        model: str | None = None,
    ) -> LLMResult: ...

    def stream(
        self,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]],
        model: str | None = None,
    ) -> AsyncIterator[StreamChunk]: ...


def _diagnose(messages: list[dict[str, Any]]) -> None:
    """请求失败时打印消息结构诊断，便于下次直接定位格式问题。"""
    tc_msgs = [m for m in messages if m.get("tool_calls")]
    missing_rc = [m for m in tc_msgs if m.get("reasoning_content") is None]
    print(
        f"[api] 请求失败，消息结构：{len(messages)} 条，"
        f"tool_calls 消息 {len(tc_msgs)} 条，"
        f"缺 reasoning_content 的 tool_calls 消息 {len(missing_rc)} 条",
        file=sys.stderr,
    )


def _usage_tokens(usage: Any) -> tuple[int | None, int | None]:
    if usage is None:
        return None, None
    return (
        getattr(usage, "prompt_tokens", None),
        getattr(usage, "completion_tokens", None),
    )


class OpenAILLM:
    """OpenAI 兼容端点客户端（异步）。

    客户端按事件循环懒创建并缓存（WeakKeyDictionary）：每个 asyncio.run()
    新建的事件循环都会得到独立的 AsyncOpenAI 实例，避免 httpx 连接池
    跨事件循环复用导致的“Event loop is closed”问题。
    """

    def __init__(
        self,
        *,
        api_key: str | None = None,
        base_url: str | None = None,
        model: str | None = None,
        reasoning_effort: str | None = None,
        timeout: float | None = None,
        max_retries: int | None = None,
        **client_kwargs: Any,
    ) -> None:
        self.model = model or DEFAULT_MODEL
        # "none"（关闭思考）归一为 None：_request_kwargs 只在非空时发参数
        self.reasoning_effort = None if reasoning_effort == REASONING_NONE else reasoning_effort
        self.api_key = api_key
        self.base_url = base_url or None
        if timeout is not None:
            client_kwargs["timeout"] = timeout
        if max_retries is not None:
            client_kwargs["max_retries"] = max_retries
        self.client_kwargs = client_kwargs
        self._clients: weakref.WeakKeyDictionary[
            asyncio.AbstractEventLoop, AsyncOpenAI
        ] = weakref.WeakKeyDictionary()

    def _client(self) -> AsyncOpenAI:
        """返回当前事件循环对应的 AsyncOpenAI 实例（懒创建，按 loop 缓存）。"""
        loop = asyncio.get_running_loop()
        client = self._clients.get(loop)
        if client is None:
            client = AsyncOpenAI(
                api_key=self.api_key,
                base_url=self.base_url,
                **self.client_kwargs,
            )
            self._clients[loop] = client
        return client

    def _request_kwargs(
        self, messages: list[dict[str, Any]], tools: list[dict[str, Any]], model: str | None
    ) -> dict[str, Any]:
        kwargs: dict[str, Any] = {
            "model": model or self.model,
            "messages": messages,
            "tools": tools or None,
        }
        effort = self.reasoning_effort
        if effort and effort != REASONING_NONE:  # 兜底：运行时被赋 "none" 也不发参数
            kwargs["reasoning_effort"] = effort
        else:
            kwargs["extra_body"] = {"thinking": {"type": "disabled"}}
        return kwargs

    async def complete(
        self,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]],
        model: str | None = None,
    ) -> LLMResult:
        """非流式完整请求（一次返回 LLMResult，带 usage）。"""
        try:
            resp = await self._client().chat.completions.create(
                **self._request_kwargs(messages, tools, model)
            )
        except Exception:
            _diagnose(messages)
            raise
        msg = resp.choices[0].message
        reasoning_content = getattr(msg, "reasoning_content", None)
        if reasoning_content is None:
            reasoning_content = (getattr(msg, "model_extra", None) or {}).get(
                "reasoning_content"
            )
        calls = [
            ToolCall(id=c.id, name=c.function.name, arguments=c.function.arguments)
            for c in (msg.tool_calls or [])
        ]
        prompt_tokens, completion_tokens = _usage_tokens(getattr(resp, "usage", None))
        return LLMResult(
            content=msg.content,
            tool_calls=calls,
            prompt_tokens=prompt_tokens,
            completion_tokens=completion_tokens,
            reasoning_content=reasoning_content,
        )

    async def stream(
        self,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]],
        model: str | None = None,
    ) -> AsyncIterator[StreamChunk]:
        """流式请求：增量 yield reasoning / content / tool_call，最后 yield done。

        usage 尽量通过 stream_options.include_usage 获取；个别兼容端点不认
        该参数时自动去掉重试一次（仅丢失 usage 统计，不影响流式本身）。
        """
        kwargs = self._request_kwargs(messages, tools, model)
        kwargs["stream"] = True

        async def _iterate(with_usage: bool) -> AsyncIterator[Any]:
            kw = dict(kwargs)
            if with_usage:
                kw["stream_options"] = {"include_usage": True}
            stream = await self._client().chat.completions.create(**kw)
            try:
                async for chunk in stream:
                    yield chunk
            finally:
                # 显式关闭底层连接，避免 GC 时 httpcore 对未消费响应体的
                # GeneratorExit 处理告警（generator didn't stop after athrow）
                await stream.close()

        tool_calls: dict[int, dict[str, Any]] = {}
        content_parts: list[str] = []
        reasoning_parts: list[str] = []
        usage: Any = None
        emitted = False
        with_usage = True
        while True:
            try:
                async for chunk in _iterate(with_usage):
                    if getattr(chunk, "usage", None) is not None:
                        usage = chunk.usage
                    if not chunk.choices:
                        continue
                    delta = chunk.choices[0].delta
                    reasoning = getattr(delta, "reasoning_content", None)
                    if reasoning:
                        reasoning_parts.append(reasoning)
                        emitted = True
                        yield StreamChunk(type="reasoning", delta=reasoning)
                    if delta.content:
                        content_parts.append(delta.content)
                        emitted = True
                        yield StreamChunk(type="content", delta=delta.content)
                    for tc in (delta.tool_calls or []):
                        idx = tc.index or 0
                        slot = tool_calls.setdefault(
                            idx, {"index": idx, "id": "", "name": "", "arguments": ""}
                        )
                        if tc.id:
                            slot["id"] = tc.id
                        fn = tc.function
                        if fn:
                            if fn.name:
                                slot["name"] = fn.name
                            if fn.arguments:
                                slot["arguments"] += fn.arguments
                        emitted = True
                        yield StreamChunk(
                            type="tool_call",
                            index=idx,
                            id=slot["id"],
                            name=slot["name"],
                            arguments=fn.arguments if fn else "",
                        )
                break
            except Exception:
                if not (with_usage and not emitted):
                    _diagnose(messages)
                    raise
                with_usage = False  # 兼容端点不认 stream_options → 去掉重试一次

        calls = [
            ToolCall(id=s["id"], name=s["name"], arguments=s["arguments"])
            for _, s in sorted(tool_calls.items())
        ]
        prompt_tokens, completion_tokens = _usage_tokens(usage)
        yield StreamChunk(
            type="done",
            result=LLMResult(
                content="".join(content_parts) or None,
                tool_calls=calls,
                prompt_tokens=prompt_tokens,
                completion_tokens=completion_tokens,
                reasoning_content="".join(reasoning_parts) or None,
            ),
        )


