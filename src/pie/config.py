"""配置层：持久化到 ~/.pie/config.toml，并组装分层 system prompt。

配置只从 ~/.pie/config.toml 读取（首次运行由向导写入）；旧 config.json 会自动迁移。
"""

from __future__ import annotations

import json
import os
import sys
import tomllib
from dataclasses import asdict, dataclass, field, fields
from pathlib import Path
from typing import Any, Iterable

from .llm import DEFAULT_MODEL
from .input import read_input

# 可通过 PIE_DIR 环境变量重定向（测试/多环境），默认 ~/.pie
PIE_DIR = Path(os.environ.get("PIE_DIR") or Path.home() / ".pie")
CONFIG_FILE = PIE_DIR / "config.toml"
LEGACY_CONFIG_FILE = PIE_DIR / "config.json"
GLOBAL_MEMORY_FILE = PIE_DIR / "memory.md"

DEFAULT_BASE_URL = "https://api.deepseek.com/"
DEFAULT_API_KEY = "<API_KEY>"
DEFAULT_REASONING_EFFORT = "high"

# 上下文压缩默认值
DEFAULT_MAX_SEQ_LEN = 128_000
DEFAULT_KEEP_LAST_STEPS = 5
DEFAULT_CONTEXT_SOFT_RATIO = 0.8
DEFAULT_CONTEXT_TARGET_RATIO = 0.55
# 提示词文件路径固化（不再作为配置项）
SYSTEM_FILE = "SYSTEM.md"
AGENTS_FILE = "AGENTS.md"
MEMORY_FILE = "MEMORY.md"


@dataclass
class ToolCompaction:
    """工具级（level 1）压缩保留的行数。"""

    head: int = 10
    tail: int = 10


@dataclass
class SessionCompaction:
    """会话级（level 3）压缩摘要：保留开头 head 个轮次 + 末尾 tail 个轮次，
    每个轮次只保留 <user_q, model_last_response>（规则式，后续可换 LLM 摘要）。"""

    enabled: bool = True
    head: int = 3
    tail: int = 2


@dataclass
class CompactionConfig:
    """压缩总配置。"""

    enabled: bool = True
    tool: ToolCompaction = field(default_factory=ToolCompaction)
    session: SessionCompaction = field(default_factory=SessionCompaction)


def _resolve_config_file(path: str | Path | None = None) -> Path:
    """配置路径：显式 -c > PIE_CONFIG_FILE 环境变量 > 默认 ~/.pie/config.toml。"""
    if path:
        return Path(path)
    env = os.environ.get("PIE_CONFIG_FILE")
    if env:
        return Path(env)
    return CONFIG_FILE

SYSTEM_PROMPT = """\
你是 pie，一个运行在用户机器上的自动化 agent，可以反复调用工具来完成任务。

长对话中，pie 会把旧内容压缩成「文件指针 + 摘要」，信息不会删除，只是换了一种表示。具体如下：
- 工具输出截断：`[工具输出全文已保存: 路径]`（或 `[shell 输出全文已保存: 路径]`）后只附首尾若干行，中间被省略。
- 轮次压缩：`[轮次原文已保存: 路径]` 代表一轮完整的「用户 → 思考 → 工具调用 → 结果 → 回复」，摘要只保留最终回复。
- 历史窗口摘要：`[历史窗口: 路径]` 后是规则式摘要，只保留最早 N 轮和最晚 M 轮的“用户提问 + 最终回复”，中间轮次被省略；其中的 `pie:` 行是当时的最终回答，不是工具结果。

可用工具：
- read(path, offset=None, limit=None)：读取文件内容（不截断）；offset 为 1 起的起始行，limit 为最大读取行数，大文件分页读取。
- edit(path, edits)：一次做多个精确替换；每个 edits 项的 oldText 在原文中必须唯一且互不重叠，按原文一次性应用。
- write(path, content)：写入文件，自动创建父目录，覆盖已有内容。
- shell(cmd, timeout=120, cwd=None, limit=200)：执行 shell 命令，返回 stdout/stderr 和退出码；输出超过 limit 行时全文落盘，只返回文件指针 + 最后 limit 行。

工作方式：
0. 当你遇到一些看起来上下文不足的提问时，首先要观察当前会话上下文是否经过压缩，优先查找分析当前会话历史以补全上下文。
1. 先用 read / shell 了解现状，再动手修改（大文件先 wc -l / rg 定位，再用 read 的行范围分段读）。
2. 小改动优先用 edit，整体重写用 write。
3. 安装依赖、运行程序、看目录、查进程等一律用 shell。
4. 工具报错就读取错误信息并修正，不要轻易放弃。
5. 全部任务完成后，用一条简洁的中文消息总结你做了什么、结果如何。
"""

@dataclass
class Config:
    model: str = DEFAULT_MODEL
    base_url: str = DEFAULT_BASE_URL
    api_key: str = DEFAULT_API_KEY
    reasoning_effort: str = DEFAULT_REASONING_EFFORT
    max_seq_len: int = DEFAULT_MAX_SEQ_LEN
    keep_last_steps: int = DEFAULT_KEEP_LAST_STEPS  # 当前轮次内必须完整保留的最近 step 批次数
    compaction: CompactionConfig = field(default_factory=CompactionConfig)
    context_soft_ratio: float = DEFAULT_CONTEXT_SOFT_RATIO
    context_target_ratio: float = DEFAULT_CONTEXT_TARGET_RATIO
    timeout_seconds: float = 60.0  # HTTP 超时（OpenAI 兼容客户端）
    max_retries: int = 2  # 请求重试次数
    max_retry_delay_seconds: float = 1.0  # 重试间隔（客户端内部退避时保留字段）
    verbose: bool = True

    def soft_limit(self) -> int:
        """软阈值：触发自动压缩的 token 水位；--auto-compact-threshold 覆盖。"""
        override = getattr(self, "auto_compact_threshold", None)
        if override:
            return int(override)
        return int(self.max_seq_len * self.context_soft_ratio)

    def target_limit(self) -> int:
        """目标水位：压缩后应降到该值以下（与软阈值同比例缩放）。"""
        soft = self.soft_limit()
        return max(1, int(soft * self.context_target_ratio / max(0.001, self.context_soft_ratio)))

    @classmethod
    def load(cls, config_file: str | Path | None = None) -> "Config":
        """从配置文件加载；旧 config.json 只在默认位置时自动迁移。"""
        path = _resolve_config_file(config_file)
        cfg = cls()
        data: dict[str, Any] = {}
        if path.exists():
            with path.open("rb") as f:
                data = tomllib.load(f)
        elif path == CONFIG_FILE and LEGACY_CONFIG_FILE.exists():
            data = json.loads(LEGACY_CONFIG_FILE.read_text(encoding="utf-8"))
        for f in fields(cls):
            if f.name not in data:
                continue
            if f.name == "compaction":
                cd = data["compaction"]
                if isinstance(cd, dict):
                    tool = cd.get("tool", {}) if isinstance(cd.get("tool"), dict) else {}
                    sd = cd.get("session", {})
                    if isinstance(sd, dict):
                        session_cfg = SessionCompaction(
                            enabled=bool(sd.get("enabled", SessionCompaction.enabled)),
                            head=int(sd.get("head", SessionCompaction.head)),
                            tail=int(sd.get("tail", SessionCompaction.tail)),
                        )
                    else:  # 旧写法 session = false
                        session_cfg = SessionCompaction(enabled=bool(sd))
                    cfg.compaction = CompactionConfig(
                        enabled=bool(cd.get("enabled", True)),
                        tool=ToolCompaction(
                            head=int(tool.get("head", ToolCompaction.head)),
                            tail=int(tool.get("tail", ToolCompaction.tail)),
                        ),
                        session=session_cfg,
                    )
                elif isinstance(cd, bool):  # 旧扁平写法 compaction = true
                    cfg.compaction.enabled = cd
            else:
                setattr(cfg, f.name, data[f.name])
        # 更早的旧键 compress_tools/compress_turns/compress_session 迁移到 enabled（新键优先）
        if "compaction" not in data and any(
            k in data for k in ("compress_tools", "compress_turns", "compress_session")
        ):
            cfg.compaction.enabled = all(
                data.get(k, True)
                for k in ("compress_tools", "compress_turns", "compress_session")
            )
            cfg.compaction.session = SessionCompaction(
                enabled=bool(data.get("compress_session", True))
            )
        if not path.exists() and path == CONFIG_FILE and LEGACY_CONFIG_FILE.exists():
            cfg.save()
        cfg.config_file = str(path)  # 运行时属性：记录配置来源路径
        return cfg

    def save(self, config_file: str | Path | None = None) -> "Config":
        path = _resolve_config_file(config_file)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(_toml_dump(asdict(self)), encoding="utf-8")
        return self


def _toml_dump(data: dict[str, Any]) -> str:
    """极简 TOML 序列化：标量 + 嵌套表（如 [compaction] / [compaction.tool]）。"""

    def fmt(value: Any) -> str:
        if isinstance(value, bool):
            return "true" if value else "false"
        if isinstance(value, int):
            return str(value)
        if isinstance(value, float):
            return str(value)
        if isinstance(value, str):
            escaped = value.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")
            return f'"{escaped}"'
        raise TypeError(f"不支持的 TOML 值类型: {type(value).__name__}")

    lines: list[str] = []

    def emit(prefix: str, d: dict[str, Any]) -> None:
        # 先输出本层标量，再输出子表，避免后续标量被误归入已开始的 [表]。
        scalars = {k: v for k, v in d.items() if not isinstance(v, dict)}
        tables = {k: v for k, v in d.items() if isinstance(v, dict)}
        for key, value in scalars.items():
            lines.append(f"{key} = {fmt(value)}")
        for key, value in tables.items():
            lines.append("")
            lines.append(f"[{prefix}{key}]")
            emit(f"{prefix}{key}.", value)

    emit("", data)
    return "\n".join(lines).strip("\n") + "\n"


def _prompt(label: str, default: str) -> str:
    try:
        value = read_input(f"{label} [{default}]: ").strip()
    except EOFError:
        return default
    return value or default


def ensure_config(config_file: str | Path | None = None) -> Config:
    """首次运行先交互式写入配置文件，再从文件读取。"""
    path = _resolve_config_file(config_file)
    if path.exists():
        return Config.load(config_file=path)
    print(f"首次运行：请先配置模型（将写入 {path}）", file=sys.stderr)
    cfg = Config()
    cfg.model = _prompt("模型名", cfg.model)
    cfg.base_url = _prompt("API 地址（OpenAI 兼容，回车用官方）", cfg.base_url)
    cfg.api_key = _prompt("API key（回车留空）", cfg.api_key)
    cfg.save(path)
    print(f"已写入 {path}，现在从文件读取并启动。", file=sys.stderr)
    return Config.load(config_file=path)


def resolve_config() -> Config:
    """兼容入口：直接读取 ~/.pie/config.toml（配置只从文件读取）。"""
    return Config.load()


def _find_project_root() -> Path:
    """从 cwd 向上找最近的“项目根”：含任一提示词文件或 .git 的目录。"""
    markers = ("AGENTS.md", "MEMORY.md", "SYSTEM.md", ".git")
    current = Path.cwd().resolve()
    for directory in (current, *current.parents):
        if any((directory / marker).exists() for marker in markers):
            return directory
    return current


def _resolve_prompt_file(name: str) -> Path | None:
    """解析提示词文件：绝对路径直接用；相对路径先找项目根，再退回 cwd。"""
    p = Path(name)
    if p.is_absolute():
        return p if p.exists() else None
    root = _find_project_root()
    for candidate in (root / p, Path.cwd() / p):
        if candidate.exists():
            return candidate
    return None


def _read_text(path: str | Path | None) -> str:
    if path is None:
        return ""
    try:
        return Path(path).read_text(encoding="utf-8")
    except OSError:
        return ""


def build_system_prompt(
    config,
    *,
    system_prompt: str | None = None,
    append_prompts: Iterable[str] = (),
) -> str:
    """按分层组装 system prompt：SYSTEM.md（角色）+ AGENTS.md（项目）+ 记忆（存在即加载）。

    system_prompt 非空时替换 SYSTEM.md 基础提示（--system-prompt 字面文本或文件内容）；
    append_prompts 追加到最末（--append-system-prompt，可重复）。
    """
    base = system_prompt if system_prompt is not None else _read_text(
        _resolve_prompt_file(SYSTEM_FILE)
    )
    parts: list[str] = [
        base or SYSTEM_PROMPT,
        f"当前模型最大上下文长度：{config.max_seq_len}",
    ]

    agents = _read_text(_resolve_prompt_file(AGENTS_FILE))
    if agents:
        parts.append(f"# 项目说明（{AGENTS_FILE}）\n{agents}")

    memories: list[tuple[str, str]] = []
    project_memory = _read_text(_resolve_prompt_file(MEMORY_FILE))
    if project_memory:
        memories.append((f"项目记忆（{MEMORY_FILE}）", project_memory))
    global_memory = _read_text(GLOBAL_MEMORY_FILE)
    if global_memory:
        memories.append(("全局记忆（~/.pie/memory.md）", global_memory))
    if memories:
        block = "\n\n".join(f"## {name}\n{content}" for name, content in memories)
        parts.append(
            "# 持久记忆\n"
            + block
            + "\n\n重要经验或用户偏好请用 edit/write 更新对应记忆文件，跨会话保留。"
        )
    parts.extend(str(p) for p in append_prompts if p)
    return "\n\n".join(parts)

