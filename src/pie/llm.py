"""模型层：LLM 抽象 + OpenAI 兼容实现（包名 pie）。

想换模型后端，实现 complete() 协议即可（官方、DeepSeek、Qwen、
vLLM、Ollama 等 OpenAI 兼容端点直接用 OpenAILLM）。
"""

from __future__ import annotations

import sys
from dataclasses import dataclass, field
from typing import Any, Protocol

from openai import OpenAI

DEFAULT_MODEL = "deepseek-v4-flash"


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
    """任何实现 complete() 的对象都可以作为 agent 的模型后端。"""

    def complete(
        self,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]],
        model: str | None = None,
    ) -> LLMResult: ...


class OpenAILLM:
    """OpenAI 兼容端点客户端。"""

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
        self.reasoning_effort = reasoning_effort
        self.api_key = api_key
        self.base_url = base_url or None
        if timeout is not None:
            client_kwargs["timeout"] = timeout
        if max_retries is not None:
            client_kwargs["max_retries"] = max_retries
        self.client_kwargs = client_kwargs
        self._client: OpenAI | None = None  # 懒初始化：进入 chat 不联网也能启动

    @property
    def client(self) -> OpenAI:
        if self._client is None:
            self._client = OpenAI(
                api_key=self.api_key,
                base_url=self.base_url,
                **self.client_kwargs,
            )
        return self._client

    def complete(
        self,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]],
        model: str | None = None,
    ) -> LLMResult:
        kwargs: dict[str, Any] = {
            "model": model or self.model,
            "messages": messages,
            "tools": tools or None,
        }
        if self.reasoning_effort:
            kwargs["reasoning_effort"] = self.reasoning_effort
        try:
            resp = self.client.chat.completions.create(**kwargs)
        except Exception:
            # 请求失败时打印诊断，便于定位格式问题
            tc_msgs = [m for m in messages if m.get("tool_calls")]
            missing_rc = [m for m in tc_msgs if m.get("reasoning_content") is None]
            print(
                f"[api] 请求失败，消息结构：{len(messages)} 条，"
                f"tool_calls 消息 {len(tc_msgs)} 条，"
                f"缺 reasoning_content 的 tool_calls 消息 {len(missing_rc)} 条",
                file=sys.stderr,
            )
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
        usage = getattr(resp, "usage", None)
        prompt_tokens = getattr(usage, "prompt_tokens", None) if usage is not None else None
        completion_tokens = (
            getattr(usage, "completion_tokens", None) if usage is not None else None
        )
        return LLMResult(
            content=msg.content,
            tool_calls=calls,
            prompt_tokens=prompt_tokens,
            completion_tokens=completion_tokens,
            reasoning_content=reasoning_content,
        )

