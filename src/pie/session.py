"""会话层：多轮对话（Session）+ AgentMessage 容器 + JSONL 持久化 + fs 窗口归档。"""

from __future__ import annotations

import asyncio
import json
from dataclasses import asdict, dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any, Callable

from .config import PIE_DIR, Config, SessionCompaction, build_system_prompt, resolve_config
from .context import (
    CONTEXT_DIR,
    WINDOWS_DIR,
    AgentMessage,
    Message,
    SystemMessage,
    UserMessage,
    build_window_summary,
    content_hash,
    summarize_turns,
    write_manifest,
)
from .llm import LLM, OpenAILLM, UsageTracker
from .loop import acomplete_turn
from .tools import ToolRegistry, default_tools


@dataclass
class Session:
    """一次多轮对话：持有 AgentMessage 历史、fs 窗口归档，每轮走完整工具循环。"""

    config: Config
    llm: LLM
    tools: ToolRegistry
    messages: AgentMessage = field(default_factory=AgentMessage)
    fs: list[Path] = field(default_factory=list)  # 之前窗口的原始消息块文件
    file: Path | None = None
    manifest: Path | None = None
    turn_count: int = 0
    usage: UsageTracker = field(default_factory=UsageTracker)
    title: str | None = None  # 会话标题 = 首个用户 query（写入 __meta__）

    @classmethod
    def new(
        cls,
        config: Config | None = None,
        llm: LLM | None = None,
        tools: ToolRegistry | None = None,
        session_id: str | None = None,
    ) -> "Session":
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
            [SystemMessage(build_system_prompt(cfg))],
            keep_last_steps=cfg.keep_last_steps,
        )
        if session_id:
            sid = Path(session_id)
            file = sid if sid.is_absolute() or sid.parent != Path(".") else (
                PIE_DIR / "sessions" / f"{sid.stem}.jsonl"
            )
        else:
            file = PIE_DIR / "sessions" / f"chat-{datetime.now():%Y%m%d-%H%M%S-%f}.jsonl"
        return cls(
            config=cfg,
            llm=backend,
            tools=registry,
            messages=messages,
            file=file,
            manifest=CONTEXT_DIR / f"{file.stem}.manifest.jsonl",
        )

    @classmethod
    def load(
        cls,
        path: str | Path,
        config: Config | None = None,
        llm: LLM | None = None,
        tools: ToolRegistry | None = None,
    ) -> "Session":
        """从 JSONL 会话文件恢复历史（新格式；旧扁平格式不兼容）。"""
        session = cls.new(config=config, llm=llm, tools=tools)
        dicts: list[dict] = []
        usage = UsageTracker()
        fs_from_meta: list[str] = []
        title_from_meta: str | None = None
        with Path(path).open(encoding="utf-8") as f:
            for line in f:
                if not line.strip():
                    continue
                data = json.loads(line)
                if data.get("__meta__"):
                    known = {
                        k: data["usage"][k]
                        for k in (
                            "prompt_tokens",
                            "completion_tokens",
                            "calls",
                            "last_prompt_tokens",
                            "last_completion_tokens",
                        )
                        if k in data.get("usage", {})
                    }
                    usage = UsageTracker(**known)
                    fs_from_meta = data.get("fs") or []
                    title_from_meta = data.get("title")
                    continue
                dicts.append(data)
        # 防御：带 tool_calls 但缺 reasoning_content 的消息补空串
        for d in dicts:
            if d.get("tool_calls") and d.get("reasoning_content") is None:
                d["reasoning_content"] = ""
        # 修复非法序列：user → 纯文本 assistant → assistant(tool_calls)
        repaired = True
        while repaired:
            repaired = False
            for i in range(1, len(dicts) - 1):
                a, b, c = dicts[i - 1 : i + 2]
                if (
                    a.get("role") == "user"
                    and b.get("role") == "assistant"
                    and not b.get("tool_calls")
                    and c.get("role") == "assistant"
                    and c.get("tool_calls")
                ):
                    del dicts[i]
                    repaired = True
                    break
        session.usage = usage
        session.fs = [Path(p) for p in fs_from_meta]
        session.title = title_from_meta
        # 重建 system 层：丢弃文件里的旧 system（提示词/窗口摘要），
        # 按当前 SYSTEM.md + fs 历史窗口块动态组装（会话级摘要规则式）。
        dicts = [d for d in dicts if d.get("role") != "system"]
        head: list[dict[str, Any]] = [
            {"role": "system", "content": build_system_prompt(session.config)}
        ]
        sc = session.config.compaction.session if session.config.compaction else None
        if sc is not None:
            for block in session.fs:
                if not block.exists():
                    continue
                try:
                    raw = block.read_text(encoding="utf-8")
                except OSError:
                    continue
                head.append(
                    {
                        "role": "system",
                        "content": build_window_summary(block, sc.head, sc.tail),
                        "compress_level": 3,
                        "raw_path": str(block),
                        "raw_hash": content_hash(raw),
                        "raw_len": len(raw),
                        "raw_tokens": len(raw) // 4,
                    }
                )
        session.messages = AgentMessage.from_flat(
            head + dicts, keep_last_steps=session.config.keep_last_steps
        )
        session.file = Path(path)
        session.manifest = CONTEXT_DIR / f"{Path(path).stem}.manifest.jsonl"
        session.turn_count = sum(
            1 for m in session.messages.messages if isinstance(m, UserMessage)
        )
        # title：meta 缺失（旧格式）时从首个真实 user 消息提取，并立即写回 meta
        if session.title is None:
            for d in dicts:
                if d.get("role") == "user" and not d.get("synthetic") and isinstance(
                    d.get("content"), str
                ):
                    session.title = str(d["content"]).strip().splitlines()[0]
                    break
            if session.title:
                _backfill_title(Path(path), session.title)
        return session

    @classmethod
    def resume(
        cls,
        config: Config | None = None,
        llm: LLM | None = None,
        tools: ToolRegistry | None = None,
    ) -> "Session":
        sessions_dir = PIE_DIR / "sessions"
        files = sorted(
            (f for f in sessions_dir.glob("*.jsonl") if f.is_file()),
            key=lambda f: f.stat().st_mtime,
            reverse=True,
        )
        if not files:
            raise FileNotFoundError(f"{sessions_dir} 中没有历史会话")
        return cls.load(files[0], config=config, llm=llm, tools=tools)

    def turn(
        self,
        user_input: str,
        on_event: Callable[[dict[str, Any]], None] | None = None,
    ) -> str:
        """同步入口（内部 asyncio.run；须在无事件循环的线程调用，如 CLI readline/print）。
        TUI / 其他 async 环境请用 aturn()。"""
        return asyncio.run(self.aturn(user_input, on_event=on_event))

    async def aturn(
        self,
        user_input: str,
        on_event: Callable[[dict[str, Any]], None] | None = None,
        cancel_event: asyncio.Event | None = None,
    ) -> str:
        """异步主路径：追加用户消息后跑完整工具循环。cancel_event 可手动取消当前回合。"""
        self.messages.add(UserMessage(user_input))
        self.turn_count += 1
        if self.title is None:
            self.title = (user_input.strip().splitlines() or [""])[0]
        return await acomplete_turn(
            self.messages,
            self.config,
            self.tools,
            self.llm,
            manifest=self.manifest,
            user_turn=self.turn_count,
            usage=self.usage,
            on_event=on_event,
            cancel_event=cancel_event,
        )

    def _window_summaries(self) -> list[SystemMessage]:
        """为 fs 里所有历史窗口块生成“摘要 + 指针”SystemMessage（保持 fs 顺序）。
        会话级压缩关闭（session 为 None）时不生成摘要。"""
        sc = self.config.compaction.session if self.config.compaction else None
        summaries: list[SystemMessage] = []
        if sc is None:
            return summaries
        for block in self.fs:
            if not block.exists():
                continue
            try:
                raw = block.read_text(encoding="utf-8")
            except OSError:
                continue
            m = SystemMessage(
                build_window_summary(block, sc.head, sc.tail), compress_level=3
            )
            m._set_raw(block, raw)
            summaries.append(m)
        return summaries

    def clear_window(self) -> None:
        """/clear：把当前窗口（除 system 外）写入 fs 块，开新窗口；
        新窗口带上所有历史窗口块的规则式摘要 + 文件指针（不只最新一块）。"""
        # 过滤掉既有的窗口摘要：它们已在 fs 中，由 _window_summaries 统一重建，
        # 避免“摘要的摘要”嵌套导致旧窗口信息从可见上下文丢失。
        old = [
            m
            for m in self.messages.messages[1:]
            if not (isinstance(m, SystemMessage) and m.compress_level == 3)
        ]
        if old:
            block = WINDOWS_DIR / f"window-{datetime.now():%Y%m%d-%H%M%S-%f}.jsonl"
            block.parent.mkdir(parents=True, exist_ok=True)
            raw = ""
            with block.open("w", encoding="utf-8") as f:
                for m in old:
                    line = json.dumps(m.to_dict(), ensure_ascii=False) + "\n"
                    raw += line
                    f.write(line)
            self.fs.append(block)
            if self.manifest is not None:
                sc = self.config.compaction.session if self.config.compaction else None
                head, tail = (
                    (sc.head, sc.tail) if sc is not None else (SessionCompaction.head, SessionCompaction.tail)
                )
                write_manifest(
                    self.manifest,
                    {
                        "ts": datetime.now().isoformat(timespec="seconds"),
                        "level": 3,
                        "kind": "session",
                        "raw_path": str(block),
                        "raw_hash": content_hash(raw),
                        "summary": summarize_turns(
                            [json.loads(l) for l in raw.splitlines() if l.strip()],
                            head,
                            tail,
                        )[:200],
                    },
                )
        self.messages = AgentMessage(
            [SystemMessage(build_system_prompt(self.config))] + self._window_summaries(),
            keep_last_steps=self.config.keep_last_steps,
        )

    def compact(self, mode: str = "auto") -> dict[str, Any]:
        """手动压缩：/compact [auto|tools|turns]。返回统计 dict。"""
        cfg = self.config
        stats: dict[str, Any] = {
            "saved_tokens": 0,
            "turns": 0,
            "tools": 0,
            "session": False,
        }
        if cfg.compaction is None:
            stats["skipped"] = "compaction disabled (未配置 [compaction])"
            return stats
        before = self.messages.tokens()
        self.messages.compact_counts = {"turns": 0, "tools": 0, "session": False}
        tool_cfg = cfg.compaction.tool
        turn_cfg = cfg.compaction.turn
        if mode in ("auto", "tools") and tool_cfg is not None:
            self.messages.compact(tools=True, tool_cfg=tool_cfg, manifest=self.manifest)
        if mode in ("auto", "turns") and turn_cfg:
            self.messages.compact(turns=True, turn_cfg=turn_cfg, target=None, manifest=self.manifest)
        stats["tools"] = self.messages.compact_counts["tools"]
        stats["turns"] = self.messages.compact_counts["turns"]
        stats["saved_tokens"] = max(0, before - self.messages.tokens())
        return stats

    def usage_report(self) -> str:
        total = self.messages.tokens()
        limit = max(1, self.config.max_seq_len)
        soft = self.config.soft_limit()
        target = self.config.target_limit()
        pct = total * 100 / limit
        roles: dict[str, int] = {}
        for m in self.messages.messages:
            roles[m.role] = roles.get(m.role, 0) + m.tokens()
        parts = []
        if self.file is not None and self.file.exists():
            parts.append(f"会话文件：{self.file}")
        parts += [
            f"当前上下文占用（估算）：{total:,} / {limit:,} tokens ({pct:.1f}%)",
            f"软阈值 {soft:,} ({self.config.context_soft_ratio:.0%}) | 目标水位 {target:,} ({self.config.context_target_ratio:.0%})",
            "各角色（估算）："
            + " | ".join(f"{k} {v:,}" for k, v in sorted(roles.items())),
        ]
        comp_events = self.compression_history()
        if comp_events:
            evicted = 0
            for e in comp_events:
                raw_path = e.get("raw_path")
                if raw_path and Path(raw_path).exists():
                    try:
                        evicted += Path(raw_path).stat().st_size // 4
                    except OSError:
                        pass
            parts.append(
                f"本会话已压缩 {len(comp_events)} 次 (当前为压缩视图)，"
                f"落盘原文约 {evicted:,} tokens (可经指针恢复)"
            )
        if self.usage.last_prompt_tokens is not None:
            parts.append(
                "最近一次 API 上报："
                f"prompt {self.usage.last_prompt_tokens:,}"
                f" | completion {self.usage.last_completion_tokens or 0:,}"
            )
        parts.append(
            "会话累计 API 用量："
            f"prompt {self.usage.prompt_tokens:,}"
            f" | completion {self.usage.completion_tokens:,}"
            f" ({self.usage.calls} 次调用)"
        )
        return "\n".join(parts)

    def compression_history(self) -> list[dict]:
        if self.manifest is None or not self.manifest.exists():
            return []
        with self.manifest.open(encoding="utf-8") as f:
            return [json.loads(line) for line in f if line.strip()]

    def full_history(self) -> list[dict]:
        """完整转录：按消息顺序展开压缩指针（工具级还原全文、轮次/会话级还原原始消息序列）。"""
        from .context import ToolMessage, load_raw_messages, message_raw_path

        out: list[Message] = []
        expanded: set[str] = set()
        for m in self.messages.messages:
            p = message_raw_path(m)
            if p is not None and p.exists():
                expanded.add(str(p.resolve()))
                if m.compress_level == 3:
                    out.extend(load_raw_messages(p))
                    continue
                if m.compress_level == 2:
                    raws = load_raw_messages(p)
                    if raws and raws[0].role == "user":
                        raws = raws[1:]
                    out.extend(raws)
                    continue
                if m.compress_level == 1 and m.role == "tool":
                    try:
                        full = p.read_text(encoding="utf-8")
                    except OSError:
                        full = m.content or ""
                    out.append(
                        ToolMessage(
                            content=full,
                            tool_call_id=m.tool_call_id,
                            tool_name=m.tool_name,
                        )
                    )
                    continue
            out.append(m)
        for entry in self.compression_history():
            raw_path = entry.get("raw_path")
            if raw_path and str(Path(raw_path).resolve()) not in expanded:
                out.extend(load_raw_messages(Path(raw_path)))
        return [m.to_dict() for m in out]

    def reset(self) -> None:
        self.messages = AgentMessage(
            self.messages.messages[:1], keep_last_steps=self.config.keep_last_steps
        )

    def save(self, path: str | Path | None = None) -> Path:
        target = Path(path) if path is not None else self.file
        if target is None:
            target = PIE_DIR / "sessions" / f"chat-{datetime.now():%Y%m%d-%H%M%S-%f}.jsonl"
        target = Path(target)
        target.parent.mkdir(parents=True, exist_ok=True)
        meta: dict[str, Any] = {
            "__meta__": True,
            "usage": asdict(self.usage),
            "fs": [str(p) for p in self.fs],
        }
        if self.title:
            meta["title"] = self.title
        with target.open("w", encoding="utf-8") as f:
            f.write(json.dumps(meta, ensure_ascii=False) + "\n")
            for m in self.messages.messages:
                f.write(json.dumps(m.to_dict(), ensure_ascii=False) + "\n")
        self.file = target
        return target


def _backfill_title(path: Path, title: str) -> None:
    """旧格式会话文件：meta 行缺失 title 时补写（保留其余字段与消息行）。"""
    lines = path.read_text(encoding="utf-8").splitlines()
    for i, line in enumerate(lines):
        if not line.strip():
            continue
        data = json.loads(line)
        if data.get("__meta__"):
            if data.get("title") == title:
                return
            data["title"] = title
            lines[i] = json.dumps(data, ensure_ascii=False)
            path.write_text("\n".join(lines) + "\n", encoding="utf-8")
            return


