"""循环层：把任务、模型、工具编排成“想 → 做 → 看 → 总结”的 agent 循环（包名 pie）。

异步主路径：acomplete_turn() 是唯一实现；run_agent() 是同步薄包装
（内部 aio.run，须在无事件循环的线程调用）。流式模型输出
（reasoning/content 增量）与工具实时输出（shell 逐行）通过 on_event 推送，
供 TUI 边生成边显示。
"""

from __future__ import annotations

import asyncio
import base64
import inspect
import json
import sys
from datetime import datetime
from pathlib import Path
from typing import Any, Awaitable, Callable

from . import aio
from .config import Config, build_system_prompt, resolve_config
from .context import (
    AgentMessage,
    AssistantMessage,
    ImageMessage,
    SystemMessage,
    ToolMessage,
    UserMessage,
    extract_spill_path,
    maybe_compact,
    write_manifest,
)
from .files import ImageStore, is_stale_file_error, key_fingerprint, model_supports_files
from .llm import LLM, LLMResult, OpenAILLM, ToolCall, UsageTracker
from .tools import (
    ToolError,
    ToolRegistry,
    clip_output,
    default_tools,
    parse_image_marker,
)

MAX_LOG_OUTPUT = 300  # 控制台日志中展示的结果长度

CANCEL_GRACE = 3.0  # /stop 后等待工具清理的宽限秒数：超时则让清理在后台继续，先终止回合

CANCEL_TEXT = "用户手动终止"  # /stop 手动取消后写入历史 / 返回给 UI 的固定文本


async def _wait_cancellable(
    coro: Awaitable[Any],
    cancel_event: asyncio.Event | None,
) -> Any:
    """await coro；cancel_event 触发时取消它并返回 None。

    用于模型请求与工具执行：UI 侧 set() 事件即可让当前回合/工具优雅终止
    （工具内部会被 CancelledError 打断并做清理，如 shell kill 子进程）。
    """
    if cancel_event is None:
        return await coro
    task = asyncio.ensure_future(coro)
    cancel_task = asyncio.ensure_future(cancel_event.wait())
    done, _ = await asyncio.wait(
        {task, cancel_task}, return_when=asyncio.FIRST_COMPLETED
    )
    if cancel_task in done:  # 用户取消优先（即使任务同时完成）
        task.cancel()
        # 等工具清理结束（asyncio.wait 不打断清理）；但有限度——个别工具
        # 清理可能真卡（如进程不可中断 D-state），不能无限等，否则 /stop 本身不返回
        await asyncio.wait({task}, timeout=CANCEL_GRACE)
        if task.done() and not task.cancelled():
            try:
                task.exception()  # 消费异常，避免 "never retrieved" 警告
            except Exception:
                pass
        return None
    cancel_task.cancel()
    return task.result()


async def _model_call(
    backend: LLM,
    messages: AgentMessage,
    registry: ToolRegistry,
    cfg: Config,
    cancel_event: asyncio.Event | None,
    on_event: Callable[[dict[str, Any]], None] | None,
) -> LLMResult | None:
    """调用模型。优先流式（增量经 on_event 推送）；后端无 stream 则回退 complete。
    被取消时返回 None。"""
    api = messages.to_api()
    tools = registry.definitions()
    model = cfg.model

    async def _consume_stream() -> LLMResult | None:
        result: LLMResult | None = None
        async for chunk in backend.stream(api, tools, model=model):  # type: ignore[attr-defined]
            if chunk.type == "reasoning" and on_event is not None:
                on_event({"type": "reasoning_delta", "text": chunk.delta})
            elif chunk.type == "content" and on_event is not None:
                on_event({"type": "content_delta", "text": chunk.delta})
            elif chunk.type == "done":
                result = chunk.result
        return result

    async def _call_complete() -> LLMResult:
        if inspect.iscoroutinefunction(backend.complete):
            return await backend.complete(api, tools, model=model)
        return await asyncio.to_thread(backend.complete, api, tools, model)  # 同步自定义后端

    if inspect.isasyncgenfunction(getattr(backend, "stream", None)):
        return await _wait_cancellable(_consume_stream(), cancel_event)
    return await _wait_cancellable(_call_complete(), cancel_event)


async def _tool_call(
    registry: ToolRegistry,
    name: str,
    args: dict[str, Any],
    cancel_event: asyncio.Event | None,
    on_event: Callable[[dict[str, Any]], None] | None,
    tool_defaults: dict[str, dict[str, Any]] | None = None,
) -> str | None:
    """执行工具；被取消时返回 None（工具内部已做清理）。"""
    if cancel_event is None:
        return await registry.adispatch(name, args, on_event=on_event, tool_defaults=tool_defaults)
    return await _wait_cancellable(
        registry.adispatch(name, args, on_event=on_event, tool_defaults=tool_defaults),
        cancel_event,
    )


async def _run_tool_call(
    registry: ToolRegistry,
    call: ToolCall,
    cfg: Config,
    tag: str,
    cancel_event: asyncio.Event | None,
    on_event: Callable[[dict[str, Any]], None] | None,
) -> str | None:
    """并行执行单个 tool_call：解析参数 → 执行 → 错误文本化 + 实时日志/事件。
    返回工具输出文本；None = 被 /stop 取消（由调用方统一收尾）；
    asyncio.CancelledError 不捕获（整体取消时向上传播）。
    单工具失败（ToolError / 普通异常）文本化后照常返回，不拖累同批其他工具。"""
    raw = call.arguments or "{}"
    try:
        args = json.loads(raw)
    except json.JSONDecodeError:
        text = f"[参数解析失败] 模型返回了非法 JSON: {raw[:500]}"
        if on_event is not None:
            on_event({"type": "tool_result", "name": call.name, "text": clip_output(text, 500)})
        return text
    if cfg.verbose:
        print(
            f"{tag}{call.name}({json.dumps(args, ensure_ascii=False)})",
            file=sys.stderr,
        )
    try:
        text = await _tool_call(
            registry, call.name, args, cancel_event, on_event, tool_defaults=cfg.tool_defaults()
        )
    except ToolError as e:
        text = f"[工具错误] {e}"
    except asyncio.CancelledError:
        raise  # 整体取消（非 cancel_event），保持传播
    except Exception as e:  # YOLO：任何异常都回传模型，让它自己修复
        text = f"[工具异常] {type(e).__name__}: {e}"
    if text is None:  # /stop 取消该工具：on_event 由 _cancel_tools 统一补 CANCEL_TEXT
        return None
    if cfg.verbose:
        print(f"{tag}结果: {clip_output(text, MAX_LOG_OUTPUT)}", file=sys.stderr)
    if on_event is not None:
        # arguments 一并推送：TUI 简洁模式的结果行要拿它取摘要（path / command）
        on_event(
            {
                "type": "tool_result",
                "name": call.name,
                "text": clip_output(text, 500),
                "arguments": args,
            }
        )
    return text


def _finalize_tool_message(
    call: ToolCall,
    text: str,
    manifest: Path | None,
) -> ToolMessage:
    """按 call 构造 ToolMessage；shell 自带落盘的结果把 spill 指针同步进 manifest。"""
    tool_msg = ToolMessage(content=text, tool_call_id=call.id, tool_name=call.name)
    # 仅 shell 会在 loop 层落盘（超 _max_lines/_max_bytes 时写“全文已保存”指针）。
    # read/edit/write 的结果文本里可能恰好含该格式字符串（如读取的源码中的字面量），
    # 若对所有工具全局搜索会误判成 spill 指针、产生假的压缩事件 → 按工具名 gate。
    if manifest is not None and call.name == "shell":
        path = extract_spill_path(text)
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
                    "tool": call.name,
                    "raw_path": str(path),
                    "raw_hash": path.stem.split("-")[-1],
                },
            )
    return tool_msg


def _image_parts_from_raw(ref: Any, data: bytes, file_id: str | None) -> list[dict[str, Any]]:
    """把一张图变成多模态 user content parts：优先 `file` 块（Files API），否则内联 base64。"""
    dim = f"{ref.width}x{ref.height} " if ref.width and ref.height else ""
    parts: list[dict[str, Any]] = [
        {
            "type": "text",
            "text": f"[图片（由 read 工具读取，非用户输入）: {ref.path} {dim}{len(data)} 字节 {ref.mime}]",
        }
    ]
    if file_id:
        parts.append({"type": "file", "file_id": file_id})
        return parts
    b64 = base64.b64encode(data).decode("ascii")
    parts.append({"type": "image_url", "image_url": {"url": f"data:{ref.mime};base64,{b64}"}})
    return parts


async def _build_image_parts(
    ref: Any, *, store: Any = None, client: Any = None
) -> list[dict[str, Any]] | None:
    """读取 read 标记指向的图片并组装 parts。

    先落一份**内容寻址副本**（`~/.pie/files/`，见 files.ImageStore）再从副本上传，
    拿到 `file_id` 就用 `file` 块；未开启/模型不支持/上传失败 → 回退内联 base64。
    文件缺失或已变化（大小不符）返回 None（标记文本仍在 tool 结果里，不硬塞）。
    """
    try:
        data = Path(ref.path).read_bytes()
    except OSError:
        return None
    if ref.size and len(data) != ref.size:
        return None
    file_id = None
    if store is not None and client is not None:
        file_id = await store.ensure(
            client, data=data, mime=ref.mime, filename=Path(ref.path).name, src=str(ref.path)
        )
    return _image_parts_from_raw(ref, data, file_id)


async def _inject_read_images(
    messages: AgentMessage,
    calls: list[ToolCall],
    results: list[str | None],
    *,
    store: Any = None,
    client: Any = None,
) -> None:
    """把本批 read 工具读到的图片作为 ImageMessage 注入（紧随全部工具结果之后，
    下轮模型请求即可看到图）。图片消息不是轮次边界，不影响压缩/轮数语义。"""
    for call, text in zip(calls, results):
        if call.name != "read" or not text:
            continue
        ref = parse_image_marker(text)
        if ref is None:
            continue
        parts = await _build_image_parts(ref, store=store, client=client)
        if parts:
            messages.add(ImageMessage(content=parts))


def _files_client(backend: LLM) -> Any:
    """取后端的上传客户端（OpenAI 兼容的后端有 _client()）；没有就返回 None（走内联）。"""
    factory = getattr(backend, "_client", None)
    if not callable(factory):
        return None
    try:
        return factory()
    except Exception:
        return None


def _downgrade_file_blocks(messages: AgentMessage, store: Any) -> bool:
    """把历史里的 `file` 块就地换成内联 base64（本地副本还在，字节拿得回来）。

    服务端删了文件 / 会话中途换了 API key 时，历史里旧的 `file_id` 会让请求 400；
    这时把所有 file 块降级成内联、并把对应记录标失效（下次同图重传）即可继续。
    返回是否真的改动了什么。"""
    changed = False
    for message in messages.messages:
        content = message.content
        if not isinstance(content, list):
            continue
        for i, part in enumerate(content):
            if not isinstance(part, dict) or part.get("type") != "file":
                continue
            file_id = str(part.get("file_id") or "")
            entry = next(
                (e for e in store.entries.values() if e.get("file_id") == file_id), None
            )
            local = Path(entry["local"]) if entry and entry.get("local") else None
            try:
                data = local.read_bytes() if local is not None else b""
            except OSError:
                data = b""
            if not data:  # 副本也没了 → 干掉这个 part（总比让请求 400 强）
                content[i] = {"type": "text", "text": "[图片已失效]"}
                store.invalidate(file_id)
                changed = True
                continue
            mime = str(entry.get("mime") or "image/png") if entry else "image/png"
            b64 = base64.b64encode(data).decode("ascii")
            content[i] = {
                "type": "image_url",
                "image_url": {"url": f"data:{mime};base64,{b64}"},
            }
            store.invalidate(file_id)
            changed = True
    return changed


async def acomplete_turn(
    messages: AgentMessage,
    cfg: Config,
    registry: ToolRegistry,
    backend: LLM,
    manifest: Path | None = None,
    user_turn: int | None = None,
    usage: UsageTracker | None = None,
    on_event: Callable[[dict[str, Any]], None] | None = None,
    cancel_event: asyncio.Event | None = None,
    image_files: dict[str, dict[str, Any]] | None = None,
) -> str:
    """处理一条用户消息：反复调用工具直到模型给出最终回复，并把回合追加进 messages。

    messages 由调用方持有，因此多轮对话可以共享同一份历史。
    image_files 是会话级的图片记录（`Session.files`）：read 到的图片先落本地副本、
    再上传拿 `file_id`（命中则不重传），历史里只留几十字节的 `file` 块；
    传 None 则完全不碰上传（如不需要跨轮复用的场景）。
    on_event 回调（可选）实时推送：
      reasoning_delta / content_delta（流式生成增量）
      tool_call / tool_result / tool_progress（工具调用与实时输出；tool_result 带
        name / text / arguments，arguments 供 UI 取“这次调的是哪个文件 / 命令”的摘要，
        因为结果文本本身不一定含路径，且结果按真实完成顺序回推、与调用顺序不一定一致）
      answer（最终回复）
    同一批 tool_calls 用 asyncio.gather 并行执行；全部收尾后按模型返回顺序回填
    ToolMessage（历史扁平序列与串行一致，compaction 的 step 批次认定不受影响），
    tool_result 事件则按真实完成顺序实时推送。
    cancel_event 非 None 时：请求模型 / 执行工具期间可被外部 set() 手动取消——
    模型请求被取消则回合终止并返回 CANCEL_TEXT；工具执行被取消则该工具结果
    填充为 CANCEL_TEXT（未执行的 tool_calls 同样补 CANCEL_TEXT，保证 API 序列合法），
    回合随之终止。
    """
    if user_turn is None:
        user_turn = sum(1 for m in messages.messages if isinstance(m, UserMessage)) or 1
    # 图片记录：entries 就是 Session.files（就地更新 → 下次 save 自然带上）
    store = (
        ImageStore(
            entries=image_files,
            base_url=cfg.base_url,
            key_fp=key_fingerprint(cfg.api_key),
            ttl_days=cfg.files_ttl_days,
            enabled=cfg.files_api and model_supports_files(cfg.model),
        )
        if image_files is not None
        else None
    )
    step = 0
    while True:
        step += 1
        if cancel_event is not None and cancel_event.is_set():
            return _cancel_turn(messages, on_event)
        maybe_compact(messages, cfg, messages.tokens(), manifest)
        try:
            llm_out = await _model_call(
                backend, messages, registry, cfg, cancel_event, on_event
            )
        except Exception as e:
            # file_id 失效（服务端删了/中途换了 key）：把历史里的 file 块降级成内联、
            # 记录标失效，再试一次（不重试就整个回合报废，代价太大）
            if store is None or not is_stale_file_error(e) or not _downgrade_file_blocks(messages, store):
                raise
            print(f"[warn] file_id 已失效，已把历史里的图片降级为内联 base64 并重试：{e}", file=sys.stderr)
            llm_out = await _model_call(
                backend, messages, registry, cfg, cancel_event, on_event
            )
        if llm_out is None:  # 用户 /stop 取消了模型请求
            return _cancel_turn(messages, on_event)
        if usage is not None and llm_out.prompt_tokens is not None:
            usage.record(
                llm_out.prompt_tokens, llm_out.completion_tokens,
                total=llm_out.total_tokens,
                reasoning=llm_out.reasoning_tokens,
                cache_hit=llm_out.prompt_cache_hit_tokens,
                cache_miss=llm_out.prompt_cache_miss_tokens,
            )
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
        # 并行执行一批 tool_calls：全部收尾后按原顺序回填 ToolMessage（历史扁平
        # 序列与串行一致 → compaction 的 step 批次 / keep_last_steps 认定不受影响）
        calls = llm_out.tool_calls
        results: list[str | None] = await asyncio.gather(
            *(
                _run_tool_call(
                    registry, c, cfg, f"[t{user_turn}s{step}] ", cancel_event, on_event
                )
                for c in calls
            )
        )
        if any(r is None for r in results):  # /stop 取消了部分工具 → 收尾后终止回合
            return _cancel_tools(messages, llm_out, results, manifest, on_event)
        for call, text in zip(calls, results):
            messages.add(_finalize_tool_message(call, text, manifest))
        # 工具结果全部回填后，统一注入 read 读取的图片（多模态 user 消息，紧随本批结果）
        await _inject_read_images(
            messages, calls, results, store=store, client=_files_client(backend) if store else None
        )


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
        max_tokens=cfg.reserved_tokens,
        timeout=cfg.timeout_seconds,
        max_retries=cfg.max_retries,
        max_retry_delay_seconds=cfg.max_retry_delay_seconds,
    )

    messages = AgentMessage(
        [SystemMessage(build_system_prompt(cfg)), UserMessage(task)],
        keep_last_steps=cfg.keep_last_steps,
    )
    return aio.run(
        acomplete_turn(messages, cfg, registry, backend, user_turn=1, image_files={})
    )


def _cancel_turn(
    messages: AgentMessage,
    on_event: Callable[[dict[str, Any]], None] | None,
) -> str:
    """回合被 /stop 取消：把“用户手动终止”作为最终回复写进历史并返回。"""
    messages.add(AssistantMessage(content=CANCEL_TEXT))
    if on_event is not None:
        on_event({"type": "answer", "text": CANCEL_TEXT})
    return CANCEL_TEXT


def _cancel_tools(
    messages: AgentMessage,
    llm_out: LLMResult,
    results: list[str | None],
    manifest: Path | None,
    on_event: Callable[[dict[str, Any]], None] | None,
) -> str:
    """并行工具执行被 /stop 取消：真实执行完的结果保留、被取消（None）的填充
    CANCEL_TEXT，按原顺序补全 ToolMessage（保证每个 tool_call_id 都有对应 tool
    消息，API 序列合法），然后终止回合。"""
    for call, text in zip(llm_out.tool_calls, results):
        content = text if text is not None else CANCEL_TEXT
        if text is None and on_event is not None:
            on_event(
                {
                    "type": "tool_result",
                    "name": call.name,
                    "text": CANCEL_TEXT,
                    "arguments": call.arguments,
                }
            )
        messages.add(_finalize_tool_message(call, content, manifest))
    return _cancel_turn(messages, on_event)




