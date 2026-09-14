"""会话层：多轮对话（Session）+ AgentMessage 容器 + JSONL 持久化 + windows 窗口归档。"""

from __future__ import annotations

import asyncio
import json
from dataclasses import asdict, dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any, Callable

from .config import (
    PIE_DIR,
    REASONING_NONE,
    Config,
    SessionCompaction,
    build_system_prompt,
    resolve_config,
)
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
    """一次多轮对话：持有 AgentMessage 历史、windows 窗口归档，每轮走完整工具循环。"""

    config: Config
    llm: LLM
    tools: ToolRegistry
    messages: AgentMessage = field(default_factory=AgentMessage)
    windows: list[Path] = field(default_factory=list)  # 之前窗口的原始消息块文件
    path: Path | None = None  # 会话文件（JSONL）自身路径
    manifest: Path | None = None
    turn_count: int = 0
    usage: UsageTracker = field(default_factory=UsageTracker)
    title: str | None = None  # 会话标题 = 首个用户 query（写入 __meta__）
    files: dict[str, dict[str, Any]] = field(default_factory=dict)  # 图片 id 表：hash_id → 记录（见 files.py）
    available_models: list[str] | None = None  # 启动时拉取的可用模型 id（/model 切换/补全用，不持久化）

    async def fetch_models(self, timeout: float = 10.0) -> list[str]:
        """拉取端点可用模型 id 并缓存到 available_models；失败抛原异常（保留旧缓存）。

        启动时由交互界面（TUI worker / readline 入口）调用；后端不支持
        list_models 时抛 NotImplementedError。"""
        fetch = getattr(self.llm, "list_models", None)
        if fetch is None:
            raise NotImplementedError("当前模型后端不支持列出可用模型")
        models = await asyncio.wait_for(fetch(), timeout)
        self.available_models = models
        return models

    def set_model(self, name: str) -> str:
        """切换模型（/model <id>）：更新 config + llm 实例并持久化，返回提示 note。

        实际请求每轮从 config.model 读取（loop._model_call 传 model=cfg.model），
        llm.model 只是构造/预热的默认值——同步它保持顶栏与预热语义一致；
        下次模型请求即用新模型。"""
        cfg = self.config
        cfg.model = name
        llm = getattr(self, "llm", None)
        if llm is not None and hasattr(llm, "model"):
            llm.model = name
        return self._persist_note(cfg)

    def set_reasoning_effort(self, level: str) -> str:
        """切换思考深度（/thinking <none|low|high|max>）：更新 config + llm 实例并持久化。"""
        cfg = self.config
        cfg.reasoning_effort = level
        llm = getattr(self, "llm", None)
        if llm is not None and hasattr(llm, "reasoning_effort"):
            llm.reasoning_effort = None if level == REASONING_NONE else level
        return self._persist_note(cfg)

    @staticmethod
    def _persist_note(cfg: Config) -> str:
        """持久化当前 config 到来源文件；失败时返回提示（仅本次会话生效）。"""
        try:
            cfg.save(getattr(cfg, "config_file", None))
            return "已写入配置"
        except OSError as e:
            return f"配置写入失败: {e}（仅本次会话生效）"

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
            max_tokens=cfg.reserved_tokens,
            timeout=cfg.timeout_seconds,
            max_retries=cfg.max_retries,
        )
        messages = AgentMessage(
            [SystemMessage(build_system_prompt(cfg))],
            keep_last_steps=cfg.keep_last_steps,
        )
        if session_id:
            sid = Path(session_id)
            path = sid if sid.is_absolute() or sid.parent != Path(".") else (
                PIE_DIR / "sessions" / f"{sid.stem}.jsonl"
            )
        else:
            path = PIE_DIR / "sessions" / f"chat-{datetime.now():%Y%m%d-%H%M%S-%f}.jsonl"
        return cls(
            config=cfg,
            llm=backend,
            tools=registry,
            messages=messages,
            path=path,
            manifest=CONTEXT_DIR / f"{path.stem}.manifest.jsonl",
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
        windows_from_meta: list[str] = []
        title_from_meta: str | None = None
        files_from_meta: dict[str, dict[str, Any]] = {}
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
                            "total_tokens",
                            "prompt_cache_hit_tokens",
                            "prompt_cache_miss_tokens",
                            "reasoning_tokens",
                            "calls",
                        )
                        if k in data.get("usage", {})
                    }
                    usage = UsageTracker(**known)
                    windows_from_meta = data.get("windows") or data.get("fs") or []  # 旧键 fs 兼容
                    title_from_meta = data.get("title")
                    files_from_meta = data.get("files") or {}
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
        session.windows = [Path(p) for p in windows_from_meta]
        session.title = title_from_meta
        session.files = files_from_meta
        # 重建 system 层：丢弃文件里的旧 system（提示词/窗口摘要），
        # 按当前 SYSTEM.md + windows 历史窗口块动态组装（会话级摘要规则式）。
        dicts = [d for d in dicts if d.get("role") != "system"]
        head: list[dict[str, Any]] = [
            {"role": "system", "content": build_system_prompt(session.config)}
        ]
        sc = session.config.compaction.session if session.config.compaction else None
        if sc is not None:
            for block in session.windows:
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
        session.path = Path(path)
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
        # 优先「当前工作目录」下最新的会话；没有匹配（含无 cwd 的旧会话）则回退全局最新
        cwd = str(Path.cwd())
        for f in files:
            if _session_cwd(f) == cwd:
                return cls.load(f, config=config, llm=llm, tools=tools)
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
            image_files=self.files,
        )

    def _window_summaries(self) -> list[SystemMessage]:
        """为 windows 里所有历史窗口块生成“摘要 + 指针”SystemMessage（保持 windows 顺序）。
        会话级压缩关闭（session 为 None）时不生成摘要。"""
        sc = self.config.compaction.session if self.config.compaction else None
        summaries: list[SystemMessage] = []
        if sc is None:
            return summaries
        for block in self.windows:
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
        """/clear：把当前窗口（除 system 外）写入 windows 块，开新窗口；
        新窗口带上所有历史窗口块的规则式摘要 + 文件指针（不只最新一块）。"""
        # 过滤掉既有的窗口摘要：它们已在 windows 中，由 _window_summaries 统一重建，
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
            self.windows.append(block)
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
        reported = self.usage.prompt_tokens
        if reported is not None:
            total = reported
            label = "当前上下文占用（API 上报）："
        else:
            total = self.messages.tokens()
            label = "当前上下文占用（估算）："
        limit = self.config.context_budget()  # 可用输入预算 = 上下文窗口 - 为输出预留的 token
        reserved_label = f"{self.config.reserved_tokens:,}" if self.config.reserved_tokens else "服务端默认"
        soft = self.config.soft_limit()
        target = self.config.target_limit()
        pct = total * 100 / limit
        roles: dict[str, int] = {}
        for m in self.messages.messages:
            roles[m.role] = roles.get(m.role, 0) + m.tokens()
        parts = []
        if self.path is not None and self.path.exists():
            parts.append(f"会话文件：{self.path}")
        parts += [
            f"{label}{total:,} / {limit:,} tokens ({pct:.1f}%)",
            f"软阈值 {soft:,} ({self.config.soft_ratio:.0%}) | 目标水位 {target:,} ({self.config.target_ratio:.0%})",
            f"输入预算 {limit:,} = 上下文窗口 {self.config.context_window:,} − 输出预留 {reserved_label}",
            "各角色占用（估算）：" + " | ".join(f"{k} {v:,}" for k, v in sorted(roles.items())),
        ]
        comp_events = self.compression_history()
        if comp_events:
            evicted = 0
            level_counts = {1: 0, 2: 0, 3: 0}
            for e in comp_events:
                lvl = e.get("level")
                if lvl in level_counts:
                    level_counts[lvl] += 1
                raw_path = e.get("raw_path")
                if raw_path and Path(raw_path).exists():
                    try:
                        evicted += Path(raw_path).stat().st_size // 4
                    except OSError:
                        pass
            parts.append(
                "上下文压缩："
                f"{level_counts[1]} / {level_counts[2]} / {level_counts[3]}"
                " (工具级 / 轮次级 / 会话级)"
            )
            parts.append(
                f"本会话已压缩 {len(comp_events)} 次 (当前为压缩视图)，"
                f"落盘原文约 {evicted:,} tokens (可经指针恢复)"
            )
        else:
            parts.append(
                "上下文压缩："
                "0 / 0 / 0"
                " (工具级 / 轮次级 / 会话级)"
            )
            parts.append(
                f"本会话已压缩 0 次 (当前为压缩视图)，"
                f"落盘原文约 0 tokens (可经指针恢复)"
            )

        parts.append("API 用量：\n" + json.dumps(asdict(self.usage), ensure_ascii=False, indent=2))
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
        target = Path(path) if path is not None else self.path
        if target is None:
            target = PIE_DIR / "sessions" / f"chat-{datetime.now():%Y%m%d-%H%M%S-%f}.jsonl"
        target = Path(target)
        target.parent.mkdir(parents=True, exist_ok=True)
        meta: dict[str, Any] = {
            "__meta__": True,
            "usage": asdict(self.usage),
            "windows": [str(p) for p in self.windows],
            "cwd": str(Path.cwd()),
        }
        if self.title:
            meta["title"] = self.title
        if self.files:  # 图片 id 表（空则不写，别把每个会话文件都撑起来）
            meta["files"] = self.files
        with target.open("w", encoding="utf-8") as f:
            f.write(json.dumps(meta, ensure_ascii=False) + "\n")
            for m in self.messages.messages:
                f.write(json.dumps(m.to_dict(), ensure_ascii=False) + "\n")
        self.path = target
        return target


def _session_cwd(path: Path) -> str | None:
    """读会话文件 meta 行里的 cwd（旧会话可能没有，返回 None）。"""
    try:
        with path.open(encoding="utf-8") as f:
            for line in f:
                if line.strip():
                    data = json.loads(line)
                    if data.get("__meta__"):
                        return data.get("cwd")
                    return None
    except (OSError, json.JSONDecodeError):
        return None
    return None


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





