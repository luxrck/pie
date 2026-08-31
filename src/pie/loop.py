"""循环层：把任务、模型、工具编排成“想 → 做 → 看 → 总结”的 agent 循环（包名 pie）。"""

from __future__ import annotations

import json
import sys
import threading
from datetime import datetime
from pathlib import Path
from typing import Any, Callable

from .config import Config, build_system_prompt, resolve_config
from .context import (
    AgentMessage,
    AssistantMessage,
    SystemMessage,
    ToolMessage,
    UserMessage,
    extract_spill_path,
    flatten_messages,
    maybe_compact,
    write_manifest,
)
from .llm import LLM, LLMResult, OpenAILLM, ToolCall, UsageTracker
from .tools import ToolError, ToolRegistry, clip_output, default_tools

MAX_LOG_OUTPUT = 300  # 控制台日志中展示的结果长度

CANCEL_TEXT = "用户手动终止"  # /stop 手动取消后写入历史 / 返回给 UI 的固定文本


def _run_cancellable(
    fn: Callable[[], Any],
    cancel_event: threading.Event | None,
) -> Any:
    """在子线程中执行 fn；cancel_event 触发时立即返回 None（结果丢弃，不写入历史）。

    用于模型请求与工具执行这类可能长时间阻塞的调用：UI 线程可以 poll
    cancel_event，让用户 /stop 后立即得到响应，而不必等底层请求真正结束。
    取消后子线程继续在后台跑，但结果被丢弃。
    """
    if cancel_event is None:
        return fn()
    holder: dict[str, Any] = {}

    def run() -> None:
        try:
            holder["result"] = fn()
        except Exception as e:  # 原样抛出，由调用方处理
            holder["error"] = e

    t = threading.Thread(target=run, daemon=True)
    t.start()
    while t.is_alive():
        if cancel_event.is_set():
            return None
        t.join(timeout=0.05)
    if "error" in holder:
        raise holder["error"]
    return holder["result"]


def run_agent(
    task: str,
    *,
    llm: LLM | None = None,
    tools: ToolRegistry | None = None,
    config: Config | None = None,
) -> str:
    """一次性任务：一条用户消息 → 工具循环 → 最终回复。"""
    cfg = config or resolve_config()
    registry = tools or default_tools()
    backend = llm or OpenAILLM(
        api_key=cfg.api_key,
        base_url=cfg.base_url,
        model=cfg.model,
        reasoning_effort=cfg.reasoning_effort,
        timeout=cfg.timeout_seconds,
        max_retries=cfg.max_retries,
    )

    messages = AgentMessage(
        [SystemMessage(build_system_prompt(config)), UserMessage(task)],
        keep_last_steps=cfg.keep_last_steps,
    )
    return complete_turn(messages, cfg, registry, backend, user_turn=1)


def complete_turn(
    messages: AgentMessage,
    cfg: Config,
    registry: ToolRegistry,
    backend: LLM,
    manifest: Path | None = None,
    user_turn: int | None = None,
    usage: UsageTracker | None = None,
    on_event: Callable[[dict[str, Any]], None] | None = None,
    cancel_event: threading.Event | None = None,
) -> str:
    """处理一条用户消息：反复调用工具直到模型给出最终回复，并把回合追加进 messages。

    messages 由调用方持有，因此多轮对话可以共享同一份历史。
    cancel_event 非 None 时：请求模型 / 执行工具期间可被外部 set() 手动取消——
    模型请求被取消则回合终止并返回 CANCEL_TEXT；工具执行被取消则该工具结果
    填充为 CANCEL_TEXT（未执行的 tool_calls 同样补 CANCEL_TEXT，保证 API 序列合法），
    回合随之终止。
    """
    if user_turn is None:
        user_turn = sum(1 for m in flatten_messages(messages.messages) if m.role == "user") or 1
    step = 0
    while True:
        step += 1
        if cancel_event is not None and cancel_event.is_set():
            return _cancel_turn(messages, on_event)
        maybe_compact(messages, cfg, messages.tokens(), manifest)
        llm_out = _run_cancellable(
            lambda: backend.complete(
                messages.to_api(), registry.definitions(), model=cfg.model
            ),
            cancel_event,
        )
        if llm_out is None:  # 用户 /stop 取消了模型请求
            return _cancel_turn(messages, on_event)
        if usage is not None and llm_out.prompt_tokens is not None:
            usage.record(llm_out.prompt_tokens, llm_out.completion_tokens)
        if llm_out.prompt_tokens is not None:
            messages.last_api_tokens = llm_out.prompt_tokens
            messages.dirty = False
            messages.compacted_since_api = False
            maybe_compact(messages, cfg, llm_out.prompt_tokens, manifest)

        if not llm_out.tool_calls:
            final = llm_out.content or ""
            messages.add(
                AssistantMessage(content=final, reasoning_content=llm_out.reasoning_content)
            )
            if on_event is not None:
                on_event({"type": "answer", "text": final})
            return final

        if on_event is not None:
            for c in llm_out.tool_calls:
                try:
                    parsed_args = json.loads(c.arguments or "{}")
                except json.JSONDecodeError:
                    parsed_args = {"raw": (c.arguments or "")[:200]}
                on_event(
                    {
                        "type": "tool_call",
                        "name": c.name,
                        "arguments": parsed_args,
                        "turn": user_turn,
                        "step": step,
                    }
                )
        messages.add(
            AssistantMessage(
                content=llm_out.content,
                reasoning_content=llm_out.reasoning_content,
                tool_calls=[
                    {
                        "id": c.id,
                        "type": "function",
                        "function": {"name": c.name, "arguments": c.arguments},
                    }
                    for c in llm_out.tool_calls
                ],
            )
        )
        for call in llm_out.tool_calls:
            raw = call.arguments or "{}"
            try:
                args = json.loads(raw)
            except json.JSONDecodeError:
                text = f"[参数解析失败] 模型返回了非法 JSON: {raw[:500]}"
            else:
                if cfg.verbose:
                    print(
                        f"[t{user_turn}s{step}] {call.name}({json.dumps(args, ensure_ascii=False)})",
                        file=sys.stderr,
                    )
                try:
                    text = _run_cancellable(
                        lambda: registry.dispatch(call.name, args), cancel_event
                    )
                except ToolError as e:
                    text = f"[工具错误] {e}"
                except Exception as e:  # YOLO：任何异常都回传模型，让它自己修复
                    text = f"[工具异常] {type(e).__name__}: {e}"
                if text is None:  # 用户 /stop 取消了工具执行
                    return _cancel_tool(messages, llm_out, call, on_event)
            if cfg.verbose:
                print(
                    f"[t{user_turn}s{step}] 结果: {clip_output(text, MAX_LOG_OUTPUT)}",
                    file=sys.stderr,
                )
            if on_event is not None:
                on_event({"type": "tool_result", "name": call.name, "text": clip_output(text, 500)})
            tool_msg = ToolMessage(content=text, tool_call_id=call.id, tool_name=call.name)
            if call.name == "shell" and manifest is not None:
                path = extract_spill_path(text)  # shell 自带 limit 落盘，记录指针进 manifest
                if path is not None:
                    tool_msg.compress_level = 1
                    tool_msg.raw_path = str(path)
                    tool_msg.raw_hash = path.stem.split("-")[-1]
                    write_manifest(
                        manifest,
                        {
                            "ts": datetime.now().isoformat(timespec="seconds"),
                            "level": 1,
                            "kind": "tool",
                            "tool": "shell",
                            "raw_path": str(path),
                            "raw_hash": path.stem.split("-")[-1],
                        },
                    )
            messages.add(tool_msg)


def _cancel_turn(
    messages: AgentMessage,
    on_event: Callable[[dict[str, Any]], None] | None,
) -> str:
    """回合被 /stop 取消：把“用户手动终止”作为最终回复写进历史并返回。"""
    messages.add(AssistantMessage(content=CANCEL_TEXT))
    if on_event is not None:
        on_event({"type": "answer", "text": CANCEL_TEXT})
    return CANCEL_TEXT


def _cancel_tool(
    messages: AgentMessage,
    llm_out: LLMResult,
    cancelled_call: ToolCall,
    on_event: Callable[[dict[str, Any]], None] | None,
) -> str:
    """工具执行被 /stop 取消：当前（以及尚未执行的）工具结果都填充 CANCEL_TEXT，
    保证每个 tool_call_id 都有对应 tool 消息（API 序列合法），然后终止回合。"""
    for call in llm_out.tool_calls:
        msg = ToolMessage(
            content=CANCEL_TEXT, tool_call_id=call.id, tool_name=call.name
        )
        messages.add(msg)
        if call is cancelled_call and on_event is not None:
            on_event({"type": "tool_result", "name": call.name, "text": CANCEL_TEXT})
    return _cancel_turn(messages, on_event)

