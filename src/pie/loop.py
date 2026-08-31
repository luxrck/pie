"""循环层：把任务、模型、工具编排成“想 → 做 → 看 → 总结”的 agent 循环（包名 pie）。"""

from __future__ import annotations

import json
import sys
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
from .llm import LLM, OpenAILLM, UsageTracker
from .tools import ToolError, ToolRegistry, clip_output, default_tools

MAX_LOG_OUTPUT = 300  # 控制台日志中展示的结果长度


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
) -> str:
    """处理一条用户消息：反复调用工具直到模型给出最终回复，并把回合追加进 messages。

    messages 由调用方持有，因此多轮对话可以共享同一份历史。
    """
    if user_turn is None:
        user_turn = sum(1 for m in flatten_messages(messages.messages) if m.role == "user") or 1
    step = 0
    while True:
        step += 1
        maybe_compact(messages, cfg, messages.tokens(), manifest)
        llm_out = backend.complete(messages.to_api(), registry.definitions(), model=cfg.model)
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
                    text = registry.dispatch(call.name, args)
                except ToolError as e:
                    text = f"[工具错误] {e}"
                except Exception as e:  # YOLO：任何异常都回传模型，让它自己修复
                    text = f"[工具异常] {type(e).__name__}: {e}"
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

