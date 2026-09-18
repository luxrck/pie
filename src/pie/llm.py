"""模型层：LLM 抽象 + OpenAI 兼容实现（包名 pie）。

想换模型后端，实现 complete() / stream() 协议即可（官方、DeepSeek、Qwen、
vLLM、Ollama 等 OpenAI 兼容端点直接用 OpenAILLM）。

异步为唯一主路径：complete() / stream() 都是 async。同步调用方用
asyncio.run() 包装（见 session.Session.turn）。
"""

from __future__ import annotations

import asyncio
import random
import sys
import weakref
from dataclasses import dataclass, field
from typing import Any, AsyncIterator, Awaitable, Callable, Protocol

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
    total_tokens: int | None = None
    prompt_cache_hit_tokens: int | None = None
    prompt_cache_miss_tokens: int | None = None
    reasoning_tokens: int | None = None  # 思考 token（completion_tokens_details.reasoning_tokens）
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
    """最近一次 API 上报的 token 用量（字段与 DeepSeek usage 对齐，**不累计求和**；calls 除外）。"""

    prompt_tokens: int | None = None
    completion_tokens: int | None = None
    total_tokens: int | None = None
    prompt_cache_hit_tokens: int | None = None
    prompt_cache_miss_tokens: int | None = None
    reasoning_tokens: int | None = None
    calls: int = 0

    def record(
        self,
        prompt: int,
        completion: int | None = None,
        *,
        total: int | None = None,
        cache_hit: int | None = None,
        cache_miss: int | None = None,
        reasoning: int | None = None,
    ) -> None:
        self.prompt_tokens = prompt
        self.completion_tokens = completion
        self.total_tokens = total
        self.prompt_cache_hit_tokens = cache_hit
        self.prompt_cache_miss_tokens = cache_miss
        self.reasoning_tokens = reasoning
        self.calls += 1


# ---- 重试（自己实现，不用 SDK 自带的那套）----
# 为什么不用 SDK 的：它的重试只在「发请求 + 拿响应头」阶段生效（读 body 中途断了不重试），
# 且跟 pie 自己的兼容回退叠在一起后，实际请求次数不可预期（max_retries=2 会发 3~6 次）。
# 次数/等待上限、以及对应的「没传就 0（不重试）」归一都在下面；config 里只放等待上限的默认值。
_RETRYABLE_STATUS = frozenset({408, 409, 429})  # 另外 5xx 一律重试
_TRANSIENT_MODULES = (  # 无 status_code 时，只有网络栈的异常才算「可恢复」
    "openai", "httpx", "httpx2", "httpcore", "httpcore2", "ssl",
)


def _retryable(exc: BaseException) -> bool:
    """这个异常值不值得重试。

    值得：408/409/429/5xx，以及网络栈的传输层异常（连接 / 超时 / 流中断）。
    不值得：其它 4xx（参数、鉴权、file_id…重试也一样错），以及我们自己代码的类型错。
    """
    status = getattr(exc, "status_code", None)
    if status is not None:
        return status in _RETRYABLE_STATUS or status >= 500
    return type(exc).__module__.split(".")[0] in _TRANSIENT_MODULES


def _retry_delay(max_delay_seconds: float) -> float:
    """本次重试前等待的秒数：`max(1.0, random.uniform(0, max_delay_seconds))`。

    随机是为了避免一批请求同时退避、又同时撞回来；1.0 是下限（不然退避形同虚设）。
    默认 `max_retry_delay_seconds = 1.0` 时恒等于 1.0（随机值不可能超过上限）。
    """
    return max(1.0, random.uniform(0.0, float(max_delay_seconds)))


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


def _usage_fields(usage: Any) -> dict[str, int | None]:
    """按 DeepSeek create-chat-completion 的 usage 结构提取字段。"""
    if usage is None:
        return {}
    details = getattr(usage, "completion_tokens_details", None)
    return {
        "prompt_tokens": getattr(usage, "prompt_tokens", None),
        "completion_tokens": getattr(usage, "completion_tokens", None),
        "total_tokens": getattr(usage, "total_tokens", None),
        "prompt_cache_hit_tokens": getattr(usage, "prompt_cache_hit_tokens", None),
        "prompt_cache_miss_tokens": getattr(usage, "prompt_cache_miss_tokens", None),
        "reasoning_tokens": getattr(details, "reasoning_tokens", None) if details else None,
    }


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
        max_tokens: int | None = None,
        timeout: float | None = None,
        max_retries: int | None = None,
        max_retry_delay_seconds: float | None = None,
        **client_kwargs: Any,
    ) -> None:
        self.model = model or DEFAULT_MODEL
        # "none"（关闭思考）归一为 None：_request_kwargs 只在非空时发参数
        self.reasoning_effort = None if reasoning_effort == REASONING_NONE else reasoning_effort
        # 单次生成上限：None = 不发送 max_tokens，由服务端默认（DeepSeek 思考模式 64K，上限 384K）
        self.max_tokens = max_tokens
        self.api_key = api_key
        self.base_url = base_url or None
        # 重试自己实现（见 _retry / _retryable）：SDK 自带那套必须关掉，否则两层叠加、
        # 实际请求次数不可预期（max_retries=2 会变成 3~6 次）。
        self.max_retries = (
            0 if max_retries is None else max(0, int(max_retries))
        )
        self.max_retry_delay_seconds = (
            0.0
            if max_retry_delay_seconds is None
            else float(max_retry_delay_seconds)
        )
        if timeout is not None:
            client_kwargs["timeout"] = timeout
        self.client_kwargs = {**client_kwargs, "max_retries": 0}
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

    def files_client(self) -> AsyncOpenAI:
        """底层 AsyncOpenAI 实例（Files API 维护命令 `pie files gc --all` 用）。

        与 `_client()` 同一份按事件循环缓存的实例；**必须在事件循环内调用**。
        """
        return self._client()

    async def list_models(self) -> list[str]:
        """拉取端点可用模型 id 列表（OpenAI 兼容 GET /models，DeepSeek 亦支持）。

        返回按 id 排序的列表；网络/鉴权失败时抛原异常（由调用方决定降级——
        交互启动拉取失败只提示、不阻塞，/model 仍可手动指定任意 id）。
        """
        client = self._client()
        resp = await self._retry(client.models.list, "模型列表请求")
        return sorted(str(m.id) for m in (resp.data or []))

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
        if self.max_tokens is not None:  # 未配置就不发，交给服务端默认
            kwargs["max_tokens"] = int(self.max_tokens)
        return kwargs

    async def _sleep_before_retry(self, attempt: int, what: str) -> None:
        """退避等待（`_retry_delay`：随机 + 1s 下限），并在 stderr 报一行。"""
        delay = _retry_delay(self.max_retry_delay_seconds)
        print(
            f"[retry] {what}失败，{delay:.1f}s 后第 {attempt}/{self.max_retries} 次重试",
            file=sys.stderr,
        )
        await asyncio.sleep(delay)

    async def _retry(self, factory: Callable[[], Awaitable[Any]], what: str) -> Any:
        """跑一个请求工厂，失败的按 `max_retries` 重试（只重試 `_retryable` 认可的失败）。

        总共最多 `1 + max_retries` 次尝试；不可恢复的失败立刻往上抛。
        """
        for attempt in range(self.max_retries + 1):
            try:
                return await factory()
            except Exception as e:
                if attempt >= self.max_retries or not _retryable(e):
                    raise
                await self._sleep_before_retry(attempt + 1, what)

    async def complete(
        self,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]],
        model: str | None = None,
    ) -> LLMResult:
        """非流式完整请求（一次返回 LLMResult，带 usage）。"""
        client = self._client()
        kwargs = self._request_kwargs(messages, tools, model)
        try:
            resp = await self._retry(lambda: client.chat.completions.create(**kwargs), "请求")
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
        u = _usage_fields(getattr(resp, "usage", None))
        return LLMResult(
            content=msg.content,
            tool_calls=calls,
            prompt_tokens=u.get("prompt_tokens"),
            completion_tokens=u.get("completion_tokens"),
            total_tokens=u.get("total_tokens"),
            prompt_cache_hit_tokens=u.get("prompt_cache_hit_tokens"),
            prompt_cache_miss_tokens=u.get("prompt_cache_miss_tokens"),
            reasoning_tokens=u.get("reasoning_tokens"),
            reasoning_content=reasoning_content,
        )

    async def stream(
        self,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]],
        model: str | None = None,
    ) -> AsyncIterator[StreamChunk]:
        """流式请求：增量 yield reasoning / content / tool_call，最后 yield done。

        usage 尽量通过 stream_options.include_usage 获取；端点以 400 拒该参数时
        自动摘掉重来一次（只丢 usage 统计，不影响流式本身）。

        请求失败重试：`max_retries` 次（等待 = `max(1.0, random(0, max_retry_delay_seconds))`，
        见 `_retry_delay`），但**只在还没 yield 过任何增量时**——已经吐出去的内容没法撤回，重来会重复。
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
        attempt = 0
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
            except Exception as e:
                if emitted:  # 已吐过增量 → 重来会重复内容，原样往上抛
                    _diagnose(messages)
                    raise
                if with_usage and getattr(e, "status_code", None) == 400:
                    # 兼容端点不认 stream_options → 摘掉该参数重来一次（不占重试额度）
                    with_usage = False
                    usage = None
                    continue
                if attempt >= self.max_retries or not _retryable(e):
                    _diagnose(messages)
                    raise
                attempt += 1
                usage = None  # 上一轮没有交付任何内容，它带回的 usage 不算数
                await self._sleep_before_retry(attempt, "流式请求")

        calls = [
            ToolCall(id=s["id"], name=s["name"], arguments=s["arguments"])
            for _, s in sorted(tool_calls.items())
        ]
        u = _usage_fields(usage)
        yield StreamChunk(
            type="done",
            result=LLMResult(
                content="".join(content_parts) or None,
                tool_calls=calls,
                prompt_tokens=u.get("prompt_tokens"),
                completion_tokens=u.get("completion_tokens"),
                total_tokens=u.get("total_tokens"),
                prompt_cache_hit_tokens=u.get("prompt_cache_hit_tokens"),
                prompt_cache_miss_tokens=u.get("prompt_cache_miss_tokens"),
                reasoning_tokens=u.get("reasoning_tokens"),
                reasoning_content="".join(reasoning_parts) or None,
            ),
        )




