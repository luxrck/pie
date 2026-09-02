"""上下文管理：AgentMessage 容器（扁平叶子列表）+ 分级压缩 + 全文落盘。

信息不删除只换表示：压缩时全文写入 ~/.pie/context/<前缀>-<hash>.txt，
消息自描述（raw_path/raw_hash/raw_len/raw_tokens）。压缩级别只升不降：
  0=原始 1=工具级（文本落盘） 2=轮次级（整轮摘要） 3=会话级（旧区指针）

容器：
  AgentMessage  整场对话：扁平叶子列表 [SystemMessage, UserMessage, ...]，
                轮次边界由 UserMessage 隐式表达（一个 User 及其后的
                assistant/tool 叶子构成一轮）
叶子：SystemMessage / UserMessage / AssistantMessage / ToolMessage
"""

from __future__ import annotations

import hashlib
import json
import re
import sys
from dataclasses import dataclass, fields
from datetime import datetime
from pathlib import Path
from typing import Any, Iterable

from .config import PIE_DIR

CONTEXT_DIR = PIE_DIR / "context"
WINDOWS_DIR = PIE_DIR / "windows"  # fs 历史窗口块（在 context/ 外，GC 不碰）

_SHELL_SPILL_RE = re.compile(r"\[shell 输出全文已保存: ([^\]]+)\]")
_POINTER_RE = re.compile(r"\[(?:会话原文|轮次原文|工具输出全文)已保存: ([^\]]+)\]")
WINDOW_SUMMARY_MARKER = "[历史窗口:"


def content_text(content: Any) -> str:
    """把消息 content 归一为可读文本：纯文本原样；多模态 parts（list）拼 text 片段、
    图片 part 用占位符表示（供摘要/标题/TUI 显示，data URI 不进人读文本）。"""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts: list[str] = []
        for part in content:
            if not isinstance(part, dict):
                continue
            t = part.get("type")
            if t == "text":
                parts.append(str(part.get("text") or ""))
            elif t == "image_url":
                parts.append("[图片]")
        return "\n".join(x for x in parts if x)
    return ""


def _image_token_estimate(url: str) -> int:
    """图片 part 的 token 估算：data URI 按 base64 负载量粗略估计（普通图 ~1-4K），
    封顶防失真；外部 URL 无法估算取常见中间值。真实值以 provider 上报为准。"""
    if url.startswith("data:"):
        return min(12000, 800 + len(url) // 256)
    return 2000


def _content_tokens(content: Any) -> int:
    """content 的 token 估算：str 按字符/4；多模态 parts 逐段算（text 按字符，image 按图像估算）。"""
    if isinstance(content, str):
        return len(content) // 4
    if isinstance(content, list):
        n = 0
        for part in content:
            if not isinstance(part, dict):
                continue
            t = part.get("type")
            if t == "image_url":
                url = (part.get("image_url") or {}).get("url") or ""
                n += _image_token_estimate(url)
            elif t == "text":
                n += len(str(part.get("text") or "")) // 4
        return n
    return 0


# ---------------------------------------------------------------- 落盘与工具函数


def content_hash(text: str) -> str:
    """内容寻址：sha256 前 16 位，同内容只落一份文件。"""
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:16]


def write_raw(blob: str, prefix: str) -> Path:
    path = CONTEXT_DIR / f"{prefix}-{content_hash(blob)}.txt"
    path.parent.mkdir(parents=True, exist_ok=True)
    if not path.exists():
        path.write_text(blob, encoding="utf-8")
    return path


def write_manifest(manifest: Path, entry: dict[str, Any]) -> None:
    manifest.parent.mkdir(parents=True, exist_ok=True)
    with manifest.open("a", encoding="utf-8") as f:
        f.write(json.dumps(entry, ensure_ascii=False) + "\n")


def extract_spill_path(text: str) -> Path | None:
    m = _SHELL_SPILL_RE.search(text or "")
    return Path(m.group(1)) if m else None


def load_window_dicts(path: Path) -> list[dict[str, Any]]:
    """读取压缩落盘 / 历史窗口块：JSON 数组或 JSONL 都支持；纯文本 → 空列表。"""
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return []
    stripped = text.lstrip()
    if stripped.startswith("["):
        try:
            data = json.loads(text)
            return data if isinstance(data, list) else []
        except (json.JSONDecodeError, TypeError):
            return []
    dicts: list[dict[str, Any]] = []
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(d, dict):
            dicts.append(d)
    return dicts


def load_raw_messages(path: Path) -> list["Message"]:
    """读取压缩落盘的原始内容为消息列表；纯文本（工具输出）→ 空列表。"""
    return [Message.from_dict(d) for d in load_window_dicts(path)]


def summarize_turns(dicts: list[dict[str, Any]], head: int, tail: int) -> str:
    """规则式会话摘要：保留开头 head 轮 + 末尾 tail 轮，每轮只留 <user_q, 模型最终回复>；
    中间被省略的轮次显式标注（对齐工具级压缩的省略提示），编号保留原始轮次序号。"""
    turns: list[tuple[str, str]] = []
    q: str | None = None
    final: str | None = None
    for d in dicts:
        role = d.get("role")
        if role == "user" and not d.get("synthetic"):  # 图片消息（synthetic）不是轮次
            if q is not None:
                turns.append((q, final or ""))
            q = content_text(d.get("content") or "") or "[图片输入]"
            final = None
        elif role == "assistant" and q is not None and not d.get("tool_calls"):
            if d.get("content"):
                final = content_text(d["content"])
    if q is not None:
        turns.append((q, final or ""))
    head = max(0, int(head or 0))
    tail = max(0, int(tail or 0))
    total = len(turns)
    if total == 0:
        return ""
    omitted = 0
    if tail and head + tail < total:
        parts: list[tuple[list[tuple[str, str]], int]] = [
            (turns[:head], 1),
            (turns[-tail:], total - tail + 1),
        ]
        omitted = total - head - tail
    else:
        parts = [(turns, 1)]
    lines: list[str] = []
    for i, (part, base) in enumerate(parts):
        prev: tuple[str, str] | None = None
        idx = base
        for t in part:
            if prev == t:  # 相邻重复合并，序号仍推进（表示原始位置）
                idx += 1
                continue
            prev = t
            q_, a_ = t
            lines.append(f"{idx}. 用户: {q_}")
            if a_:
                lines.append(f"   pie: {a_}")
            idx += 1
        if omitted and i == 0:
            lines.append(f"...[中间省略 {omitted} 轮]...")
    return "\n".join(lines)


def build_window_summary(path: Path, head: int, tail: int) -> str:
    """历史窗口的摘要文本：文件指针 + 规则式 <user_q, 模型最终回复> 对。"""
    body = summarize_turns(load_window_dicts(path), head, tail)
    pointer = f"{WINDOW_SUMMARY_MARKER} {path}]"
    return f"{pointer}\n{body}" if body else f"{pointer}（无可摘要内容）"


# ---------------------------------------------------------------- 消息模型


@dataclass
class Message:
    """叶子消息：System/User/Assistant/Tool/Image 的公共基类。"""

    role: str
    content: str | list[dict[str, Any]] | None = None
    compress_level: int = 0  # 0=原始 1=工具级 2=轮次级 3=会话级（只升不降）
    tool_call_id: str | None = None
    tool_calls: list[dict[str, Any]] | None = None
    tool_name: str | None = None
    reasoning_content: str | None = None  # thinking 模式思考内容（回传时必须保留）
    raw_path: str | None = None  # 压缩落盘文件路径（自描述指针）
    raw_hash: str | None = None
    raw_len: int | None = None  # 原始文本长度（压缩前），tokens() 比例估算用
    raw_tokens: int | None = None  # 原始 token 估算（压缩时 raw_len//4）
    synthetic: bool = False  # 非用户输入注入的消息（如图片）：不构成轮次边界、不计轮数

    def to_api(self) -> dict[str, Any]:
        """转成 OpenAI 兼容的消息 dict（去掉内部压缩元数据）。
        content 为多模态 parts（list）时原样透传（如 ImageMessage 的图片 user 消息）。"""
        if self.role == "tool":
            return {
                "role": "tool",
                "tool_call_id": self.tool_call_id,
                "content": self.content or "",
            }
        if self.role == "assistant" and self.tool_calls:
            return {
                "role": "assistant",
                "content": self.content,
                "tool_calls": self.tool_calls,
                "reasoning_content": self.reasoning_content or "",
            }
        if self.role == "assistant" and self.reasoning_content is not None:
            return {
                "role": "assistant",
                "content": self.content,
                "reasoning_content": self.reasoning_content,
            }
        return {"role": self.role, "content": self.content}

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "role": self.role,
            "content": self.content,
            "compress_level": self.compress_level,
            "cls": type(self).__name__,
        }
        for key in (
            "tool_call_id",
            "tool_calls",
            "tool_name",
            "reasoning_content",
            "raw_path",
            "raw_hash",
            "raw_len",
            "raw_tokens",
        ):
            value = getattr(self, key)
            if value is not None:
                d[key] = value
        if self.synthetic:  # 只在 True 时写出（False 默认省略，保持旧文件干净）
            d["synthetic"] = True
        return d

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "Message":
        """按 cls（新格式）/ role（旧格式兼容）还原子类。"""
        ctor = _MESSAGE_CLASSES.get(d.get("cls")) or {
            "system": SystemMessage,
            "user": UserMessage,
            "assistant": AssistantMessage,
            "tool": ToolMessage,
        }.get(d.get("role", "assistant"), AssistantMessage)
        kwargs = {f.name: d.get(f.name) for f in fields(Message) if f.name != "role"}
        if kwargs.get("synthetic") is None:  # 旧文件无该字段 → 默认 False
            kwargs["synthetic"] = False
        return ctor(**kwargs)

    def compact(self, **kwargs: Any) -> Any:
        """叶子默认不可压缩（ToolMessage 覆盖为工具级文本落盘）。"""
        return 0

    def tokens(self) -> int:
        """token 估算：压缩过的消息用（当前长度/原始长度 × 原始 tokens）比例；
        多模态 parts 按 text 字符 + 图片估算分别计。"""
        if self.raw_len and self.raw_tokens is not None and self.compress_level >= 1:
            cur = _content_tokens(self.content)
            return max(1, round(cur / max(1, self.raw_len) * self.raw_tokens))
        n = _content_tokens(self.content) + 12
        if self.tool_calls:
            n += len(json.dumps(self.tool_calls, ensure_ascii=False)) // 4
        if self.reasoning_content:
            n += len(self.reasoning_content) // 4
        return n

    def __str__(self) -> str:
        """日志/显示用：content 为多模态 parts 时输出可读文本，避免 data URI 淹没。"""
        if self.role == "tool":
            return f"<Tool {self.tool_name} {self.tool_call_id}>"
        if self.tool_calls:
            return f"<Assistant tool_calls={[tc.get('function', {}).get('name') for tc in self.tool_calls]}>"
        return f"<{type(self).__name__} {content_text(self.content)[:80]}>"

    def _set_raw(self, path: Path, blob: str) -> None:
        self.raw_path = str(path)
        self.raw_hash = path.stem.split("-")[-1]
        self.raw_len = len(blob)
        self.raw_tokens = len(blob) // 4


class SystemMessage(Message):
    """系统消息，不可压缩。"""

    def __init__(self, content: str | None = None, **kwargs: Any) -> None:
        super().__init__(role="system", content=content, **kwargs)


class UserMessage(Message):
    """用户消息，不可压缩；真实轮次边界（role=user）。"""

    def __init__(self, content: str | None = None, **kwargs: Any) -> None:
        super().__init__(role="user", content=content, **kwargs)


class ImageMessage(Message):
    """图片消息：把工具读取的图片作为多模态 user 内容注入（模型“看”图的载体，synthetic）。

    不是 UserMessage 子类 → 不构成轮次边界，不影响轮次级/会话级压缩的轮次认定、
    轮数统计与标题提取；可随所在轮次/窗口一起被压缩落盘（原始 data URI 保留在
    raw 文件里，full_history 可恢复）。role 为 user：OpenAI 兼容 API 要求图片只能
    出现在 user 消息的 content parts。
    content = [{"type": "text", "text": ...}, {"type": "image_url", "image_url": {"url": "data:..."}}]
    """

    def __init__(self, content: list[dict[str, Any]] | None = None, **kwargs: Any) -> None:
        kwargs.setdefault("synthetic", True)  # 图片恒为注入消息（from_dict 还原时用存档值）
        super().__init__(role="user", content=content, **kwargs)


class AssistantMessage(Message):
    """LLM 返回的消息（含 tool_calls），可被轮次级压缩。"""

    def __init__(
        self,
        content: str | None = None,
        tool_calls: list[dict[str, Any]] | None = None,
        reasoning_content: str | None = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(
            role="assistant",
            content=content,
            tool_calls=tool_calls,
            reasoning_content=reasoning_content,
            **kwargs,
        )


class ToolMessage(Message):
    """工具返回的消息，可被工具级压缩（文本落盘，消息保留）。"""

    def __init__(
        self,
        content: str | None = None,
        tool_call_id: str | None = None,
        tool_name: str | None = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(
            role="tool",
            content=content,
            tool_call_id=tool_call_id,
            tool_name=tool_name,
            **kwargs,
        )

    def compact(self, tool_cfg: Any = None, manifest: Path | None = None, **kwargs: Any) -> int:
        """工具级压缩：行数 > head+tail 时全文落盘，消息保留。
        tool_cfg 为 None（工具压缩关闭）时不做任何事。"""
        if self.compress_level >= 1 or not self.content or tool_cfg is None:
            return 0
        head = max(0, int(tool_cfg.head))
        tail = max(0, int(tool_cfg.tail))
        content = self.content
        lines = content.splitlines()
        if len(lines) <= head + tail:
            return 0
        path = write_raw(content, self.tool_name or "tool")
        preview = lines[:head] + ["...[中间省略]..."] + (lines[-tail:] if tail else [])
        self.content = f"[工具输出全文已保存: {path}]\n" + "\n".join(preview)
        self.compress_level = 1
        self._set_raw(path, content)  # raw 记录原始文本
        if manifest is not None:
            write_manifest(
                manifest,
                {
                    "ts": datetime.now().isoformat(timespec="seconds"),
                    "level": 1,
                    "kind": "tool",
                    "tool": self.tool_name,
                    "raw_path": str(path),
                    "raw_hash": path.stem.split("-")[-1],
                },
            )
        return 1


# ---------------------------------------------------------------- 容器


_MESSAGE_CLASSES: dict[str, type] = {
    "SystemMessage": SystemMessage,
    "UserMessage": UserMessage,
    "ImageMessage": ImageMessage,
    "AssistantMessage": AssistantMessage,
    "ToolMessage": ToolMessage,
}


class AgentMessage:
    """整场对话容器：扁平叶子列表 [SystemMessage, UserMessage, AssistantMessage, ToolMessage, ...]，
    轮次边界由 UserMessage 隐式表达（一个 User 及其后的 assistant/tool 叶子构成一轮）。"""

    def __init__(
        self,
        messages: Iterable[Message] | None = None,
        keep_last_steps: int = 5,
    ) -> None:
        self.messages: list[Message] = list(messages or [])
        self.keep_last_steps = max(1, int(keep_last_steps))
        self.compact_counts: dict[str, Any] = {"turns": 0, "tools": 0, "session": False}
        self.last_api_tokens: int | None = None  # provider 上报的最近一次 prompt_tokens
        self.compacted_since_api: bool = False
        self.dirty: bool = True  # 有新增消息后 provider 基线失效

    def __len__(self) -> int:
        return len(self.messages)

    def __iter__(self) -> Iterable[Message]:
        return iter(self.messages)

    def __getitem__(self, i: int | slice) -> Any:
        if isinstance(i, slice):
            return AgentMessage(self.messages[i], keep_last_steps=self.keep_last_steps)
        return self.messages[i]

    def __add__(self, other: Any) -> "AgentMessage":
        return AgentMessage(
            self.messages + list(other), keep_last_steps=self.keep_last_steps
        )

    @staticmethod
    def from_flat(dicts: list[dict[str, Any]], keep_last_steps: int = 5) -> "AgentMessage":
        """从扁平消息 dict 列表重建容器：叶子直存，轮次由 UserMessage 隐式表达。"""
        return AgentMessage(
            [Message.from_dict(d) for d in dicts], keep_last_steps=keep_last_steps
        )

    def add(self, m: Message) -> None:
        """追加叶子消息（assistant/tool/user/system 一律直存）。"""
        self.dirty = True
        self.messages.append(m)

    def tokens(self) -> int:
        if self.last_api_tokens is not None and not self.compacted_since_api and not self.dirty:
            return self.last_api_tokens
        return sum(e.tokens() for e in self.messages)

    def to_api(self) -> list[dict[str, Any]]:
        return [m.to_api() for m in self.messages]

    def compact(
        self,
        *,
        tools: bool = False,
        turns: bool = False,
        session: bool = False,
        tool_cfg: Any = None,
        turn_cfg: Any = None,
        session_cfg: Any = None,
        target: int | None = None,
        manifest: Path | None = None,
        fs: list[Path] | None = None,
    ) -> Any:
        if tools:
            return self._compact_tools(tool_cfg, manifest)
        if turns:
            return self._compact_turns(turn_cfg, target, manifest)
        if session:
            return self._compact_session(session_cfg, manifest, fs)
        return self

    def _compact_tools(self, tool_cfg: Any, manifest: Path | None) -> "AgentMessage":
        """工具级压缩：保护最近 keep_last_steps 个 step 批次（跨轮次滚动，
        每批 = 一次 assistant(tool_calls) + 其后的 tool 结果），窗口外的
        未压缩 ToolMessage（从最老开始）全文落盘成指针。"""
        flat = self.messages
        protected = _protected_step_tool_indices(flat, self.keep_last_steps)
        n = 0
        for i, m in enumerate(flat):
            if isinstance(m, ToolMessage) and m.compress_level == 0 and i not in protected:
                m.compact(tool_cfg=tool_cfg, manifest=manifest)
                n += 1
        self.compact_counts["tools"] = n
        if n:
            self.compacted_since_api = True
        return self

    def _compact_turns(
        self, turn_cfg: Any, target: int | None, manifest: Path | None
    ) -> "AgentMessage":
        """从最老开始压缩已完成的轮次（最后一个 UserMessage 之后为进行中，不压），
        直到低于目标或无可压缩。每轮 = [UserMessage, assistant/tool 叶子...]，
        user 保留、其后的叶子替换为摘要 assistant。
        循环上限 = 已完成轮次数 + 1：每次迭代要么压掉一个轮次（压缩过的轮不再
        可压），要么 break，结构上保证不会死循环。"""
        completed = max(
            0,
            sum(1 for e in self.messages if isinstance(e, UserMessage)) - 1,
        )
        count = 0
        for _ in range(completed + 1):
            if target is not None and self.tokens() <= target:
                break
            # 每次重扫索引：切片替换会漂移后续元素位置，预计算索引会误压进行中轮次
            user_idxs = [i for i, e in enumerate(self.messages) if isinstance(e, UserMessage)]
            victim: tuple[int, int] | None = None
            for k in range(len(user_idxs) - 1):  # 最后一个 user 之后 = 进行中，不压
                u, end = user_idxs[k], user_idxs[k + 1]
                span = self.messages[u + 1:end]
                if not span:  # 连续 user，无内容
                    continue
                if all(
                    isinstance(m, AssistantMessage) and m.compress_level >= 2 for m in span
                ):  # 已轮次级压缩过 → 跳过
                    continue
                victim = (u, end)
                break
            if victim is None:  # 无可压缩轮次
                break
            u, end = victim
            span = self.messages[u + 1:end]
            self.messages[u + 1:end] = [
                self._compact_turn_span(span, manifest)
            ]
            count += 1
        self.compact_counts["turns"] = count
        if count:
            self.compacted_since_api = True
        return self

    def _compact_turn_span(
        self,
        span: list[Message],
        manifest: Path | None,
    ) -> AssistantMessage:
        """轮次级（规则式）：把一轮的 assistant/tool 叶子压成摘要 assistant
        （user 保留在 AgentMessage 中，不重复写入摘要）+ 指针；摘要只保留
        该轮次的模型最终输出（pie: ...），中间过程被省略时显式标注。"""
        raw = json.dumps([m.to_dict() for m in span], ensure_ascii=False, indent=1)
        path = write_raw(raw, "turn")
        final = ""  # 模型最终输出 = 最后一个有文本的 assistant 回复
        for m in span:
            if isinstance(m, AssistantMessage) and m.content:
                final = m.content
        lines: list[str] = []
        if len(span) > 1:  # 存在中间过程（工具调用等）→ 显式省略标注
            lines.append("...[中间过程省略]...")
        lines.append(f"{final}" if final else "[该轮次无最终文本，原文已保存]")
        body = "\n".join(lines)
        if manifest is not None:
            write_manifest(
                manifest,
                {
                    "ts": datetime.now().isoformat(timespec="seconds"),
                    "level": 2,
                    "kind": "turn",
                    "raw_path": str(path),
                    "raw_hash": content_hash(raw),
                    "summary": body[:200],
                },
            )
        msg = AssistantMessage(content=f"[轮次原文已保存: {path}]\n{body}", compress_level=2)
        msg._set_raw(path, raw)
        return msg

    def _compact_session(
        self, session_cfg: Any, manifest: Path | None, fs: list[Path] | None
    ) -> "AgentMessage":
        """当前轮之前的历史整段落盘成新窗口，保留所有既有窗口摘要，
        插入新窗口的“摘要 + 指针”SystemMessage；新窗口块追加进 fs。"""
        user_idx = [i for i, e in enumerate(self.messages) if isinstance(e, UserMessage)]
        if not user_idx:
            self.compact_counts["session"] = False
            return self
        current_start = user_idx[-1]
        if current_start <= 1:
            self.compact_counts["session"] = False
            return self
        # 已摘要的旧窗口（compress_level==3 的 SystemMessage）不重复归档，继续保留在上下文中
        summaries = [
            e
            for e in self.messages[1:current_start]
            if isinstance(e, SystemMessage) and e.compress_level == 3
        ]
        old = [
            e
            for e in self.messages[1:current_start]
            if not (isinstance(e, SystemMessage) and e.compress_level == 3)
        ]
        if not old:
            self.compact_counts["session"] = False
            return self
        raw = json.dumps([m.to_dict() for m in old], ensure_ascii=False, indent=1)
        path = write_raw(raw, "session")
        if fs is not None:
            fs.append(path)
        sc = session_cfg
        summary = build_window_summary(path, sc.head, sc.tail)
        if manifest is not None:
            write_manifest(
                manifest,
                {
                    "ts": datetime.now().isoformat(timespec="seconds"),
                    "level": 3,
                    "kind": "session",
                    "raw_path": str(path),
                    "raw_hash": content_hash(raw),
                    "summary": summarize_turns(load_window_dicts(path), sc.head, sc.tail)[:200],
                },
            )
        ptr = SystemMessage(content=summary, compress_level=3)
        ptr._set_raw(path, raw)
        self.messages = [self.messages[0]] + summaries + [ptr] + self.messages[current_start:]
        self.compacted_since_api = True
        self.compact_counts["session"] = True
        return self


def _step_batches(flat: list[Message]) -> list[tuple[int, int]]:
    """把扁平消息序列切成 step 批次（半开区间 [start, end)）：
    每批 = 一次 assistant(tool_calls) + 其后的连续 tool 结果。"""
    batches: list[tuple[int, int]] = []
    i, n = 0, len(flat)
    while i < n:
        m = flat[i]
        if isinstance(m, AssistantMessage) and m.tool_calls:
            j = i + 1
            while j < n and isinstance(flat[j], ToolMessage):
                j += 1
            batches.append((i, j))
            i = j
        else:
            i += 1
    return batches


def _protected_step_tool_indices(flat: list[Message], keep: int) -> set[int]:
    """返回最近 keep 个 step 批次内 ToolMessage 的索引集合（跨轮次滚动保护）。"""
    protected: set[int] = set()
    for start, end in _step_batches(flat)[-max(1, int(keep)):]:
        for i in range(start, end):
            if isinstance(flat[i], ToolMessage):
                protected.add(i)
    return protected


def message_raw_path(m: Message) -> Path | None:
    if m.raw_path:
        return Path(m.raw_path)
    if m.content:
        mm = _POINTER_RE.search(m.content)
        if mm:
            return Path(mm.group(1))
    return None


# ---------------------------------------------------------------- 压缩驱动


def maybe_compact(
    agent: AgentMessage,
    cfg: Any,
    current_tokens: int | None = None,
    manifest: Path | None = None,
    fs: list[Path] | None = None,
) -> dict[str, Any]:
    """按需压缩：软阈值触发，tools → turns → session（各级受对应子配置 None 门控）。"""
    stats: dict[str, Any] = {
        "saved_tokens": 0,
        "turns": 0,
        "tools": 0,
        "session": False,
    }
    limit = cfg.max_seq_len
    if limit <= 0 or cfg.compaction is None:  # compaction 未配置 = 不做任何压缩
        return stats
    tokens = current_tokens if current_tokens is not None else agent.tokens()
    soft = cfg.soft_limit()
    if tokens < soft:
        return stats
    target = cfg.target_limit()
    before = agent.tokens()
    agent.compact_counts = {"turns": 0, "tools": 0, "session": False}
    tool_cfg = cfg.compaction.tool
    turn_cfg = cfg.compaction.turn
    session_cfg = cfg.compaction.session
    if tool_cfg is not None:
        agent.compact(tools=True, tool_cfg=tool_cfg, manifest=manifest)
    if turn_cfg and agent.tokens() > target:
        agent.compact(turns=True, turn_cfg=turn_cfg, target=target, manifest=manifest)
    if session_cfg is not None and agent.tokens() > target:
        agent.compact(session=True, session_cfg=session_cfg, manifest=manifest, fs=fs)
    stats["tools"] = agent.compact_counts["tools"]
    stats["turns"] = agent.compact_counts["turns"]
    stats["session"] = agent.compact_counts["session"]
    stats["saved_tokens"] = max(0, before - agent.tokens())
    if cfg.verbose:
        print(
            f"[context] 压缩节省约 {stats['saved_tokens']} tokens"
            f"（turns={stats['turns']}, tools={stats['tools']}, session={stats['session']}）",
            file=sys.stderr,
        )
    return stats


# ---------------------------------------------------------------- GC


def referenced_raw_paths() -> set[Path]:
    """收集被引用的原始文件路径：manifest 条目 + 所有会话消息字段里的 raw_path。"""
    refs: set[Path] = set()
    for manifest in CONTEXT_DIR.glob("*.manifest.jsonl"):
        try:
            lines = manifest.read_text(encoding="utf-8").splitlines()
        except OSError:
            continue
        for line in lines:
            if not line.strip():
                continue
            try:
                raw_path = json.loads(line).get("raw_path")
            except json.JSONDecodeError:
                continue
            if raw_path:
                refs.add(Path(raw_path).resolve())
    for session in (PIE_DIR / "sessions").glob("*.jsonl"):
        try:
            lines = session.read_text(encoding="utf-8").splitlines()
        except OSError:
            continue
        for line in lines:
            if not line.strip():
                continue
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if data.get("__meta__"):
                continue
            raw_path = data.get("raw_path")
            if raw_path:
                refs.add(Path(raw_path).resolve())
    return refs


def collect_context_garbage() -> list[Path]:
    """返回 context/ 下未被任何会话引用的文件（可安全删除）。fs 窗口块在 windows/ 下，不受影响。"""
    referenced = referenced_raw_paths()
    return sorted(f for f in CONTEXT_DIR.glob("*.txt") if f.resolve() not in referenced)


