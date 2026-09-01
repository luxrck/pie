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
DEFAULT_API_KEY = "sk-469cb4875eac409bb8e4c1419c03d0bf"
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


SYSTEM_PROMPT = """\
你是 pie，一个运行在用户机器上的自动化 agent，通过反复调用工具完成用户任务。

## 上下文压缩

长对话中，pie 会把旧内容压缩成「文件指针 + 摘要」，信息不会删除，只是换了一种表示。具体如下：
- 工具输出截断：`[工具输出全文已保存: 路径]`（或 `[shell 输出全文已保存: 路径]`）后只附首尾若干行，中间被省略。
- 轮次压缩：`[轮次原文已保存: 路径]` 代表一轮完整的「用户 → 思考 → 工具调用 → 结果 → 回复」，摘要保留开头/末尾消息概览，被省略的中间部分用 `...[中间省略 N 条消息]...` 显式标注。
- 历史窗口摘要：`[历史窗口: 路径]` 后是规则式摘要，只保留最早 N 轮和最晚 M 轮的“用户提问 + 最终回复”，中间被省略的轮次用 `...[中间省略 N 轮]...` 显式标注；其中的 `pie:` 行是当时的最终回答，不是工具结果。

## 核心原则

- YOLO 模式：所有工具直接执行，无权限确认。这要求你更谨慎、更准确，避免破坏性操作。
- 先理解再动手：修改文件前，先用 read 或 shell 了解现状。
- 最小改动：小修改用 edit，整体重写才用 write。
- 用 shell 验证：写完代码就运行；状态不明就看目录、查日志、跑测试。
- 不假装完成：工具报错就读错误信息并修正；多种方法都失败时如实汇报，不编造成功。
- 当你遇到看起来上下文信息不足的提问时，先观察当前会话是否经过压缩，优先查找分析当前会话历史文件以补全上下文信息。

## 可用工具

- `read(path, offset=None, limit=None)`：读取文件内容（UTF-8，不截断；二进制返回大小提示）。offset 为 1 起的起始行，limit 为最大读取行数，用于大文件分页读取。
- `edit(path, edits)`：一次做多个精确替换。每个 edits 项的 oldText 在原文中必须唯一且互不重叠，按原文一次性应用，相邻改动请合并成一个 edit。
- `write(path, content)`：写入文件，自动创建父目录，覆盖已有内容。
- `shell(cmd, timeout=120, cwd=None, limit=200)`：执行 shell 命令，返回 stdout/stderr 和退出码。输出超过 limit 行时全文落盘，只返回文件指针 + 最后 limit 行。

## 工作流程

1. 拆解任务，先用 read / shell 了解现状（大文件先用 `wc -l` 或 `rg` 定位，再用 read 按行范围分段读）。
2. 按需调用工具，一次只做必要的事。
3. 每步确认结果，出错即修正。
4. 全部完成后，用简洁的中文总结：做了什么、结果如何、有无遗留问题。

## 记忆管理

你有两层记忆文件：
- **全局记忆**：`~/.pie/memory.md` — 跨项目的用户偏好、通用编码风格、工具链路径、个人习惯。
- **项目记忆**：`<项目根>/MEMORY.md` — 当前项目的架构决策、业务逻辑、技术选型、待办事项。

**当记忆冲突时，以项目记忆优先**，若冲突则在回答开头提醒：`⚠️ 记忆冲突：全局 X，项目 Y，我将遵循 Y`。

### 记忆更新

学到跨会话仍然有效的经验、用户偏好或项目决策时，用 `edit` 或 `write` 更新对应记忆文件。不要记录临时状态（如临时文件路径），只记长期事实。

| 写入内容 | 目标文件 | 触发条件 |
|---------|---------|---------|
| 跨项目的通用经验（编码风格、工具习惯、个人设置等） | `~/.pie/memory.md` | 用户显式要求记住；或从多次修正反馈中提炼出的新习惯 |
| 项目专属决策经验（架构选型、业务规则、技术栈版本等） | `<项目根>/MEMORY.md` | 完成重大功能、修复复杂 Bug、重构后；或用户说“记住这个决策” |
| 待办事项 / 路线图 | `<项目根>/MEMORY.md` | 规划新阶段、完成里程碑后更新进度 |

### 禁止写入

- 临时性报错堆栈、中间输出、调试日志
- 明文密钥或敏感凭证（应使用 `.env`）
- 已被项目记忆或全局记忆覆盖的临时性偏好
"""

GLOBAL_MEMORY_TEMPLATE = """\
# 全局记忆（~/.pie/memory.md）

本文件是 pie 的全局持久记忆：记录跨项目仍然有效的用户偏好、关键决策和踩过的坑。agent 在每次会话开始时读取，并可在运行中用 edit/write 更新。

保持简洁：只记跨会话仍然有效的事实，不要记临时状态（如临时文件路径）。

## 使用方式

- 学到跨会话仍然有效的经验、用户偏好或项目决策时，用 edit/write 更新记忆文件：跨项目通用的写到这里，项目专属的写到该项目根目录的 MEMORY.md。
- 不要删除本文件的说明段落；保持结构简单。
"""


@dataclass
class ToolCompaction:
    """工具级（level 1）压缩保留的行数。"""

    head: int = 50
    tail: int = 30


@dataclass
class TurnCompaction:
    """轮次级（level 2）压缩摘要：保留开头 head 条消息 + 末尾 tail 条消息的概览，
    中间省略部分显式标注（对齐工具级 head/tail 预览）。"""

    head: int = 5
    tail: int = 3


@dataclass
class SessionCompaction:
    """会话级（level 3）压缩摘要：保留开头 head 个轮次 + 末尾 tail 个轮次，
    每个轮次只保留 <user_q, model_last_response>（规则式，后续可换 LLM 摘要）。
    为 None 表示关闭会话级压缩。"""

    head: int = 5
    tail: int = 3


@dataclass
class CompactionConfig:
    """压缩总配置：写了 [compaction] 即开启，默认三级全开；
    某级为 None（显式 tool/turn/session = false）表示关闭该级；
    整个 compaction 为 None（不写 [compaction]）表示不做任何压缩。"""

    tool: ToolCompaction | None = field(default_factory=ToolCompaction)
    turn: TurnCompaction | None = field(default_factory=TurnCompaction)
    session: SessionCompaction | None = field(default_factory=SessionCompaction)


def _resolve_config_file(path: str | Path | None = None) -> Path:
    """配置路径：显式 -c > PIE_CONFIG_FILE 环境变量 > 默认 ~/.pie/config.toml。"""
    if path:
        return Path(path)
    env = os.environ.get("PIE_CONFIG_FILE")
    if env:
        return Path(env)
    return CONFIG_FILE


@dataclass
class Config:
    model: str = DEFAULT_MODEL
    base_url: str = DEFAULT_BASE_URL
    api_key: str = DEFAULT_API_KEY
    reasoning_effort: str = DEFAULT_REASONING_EFFORT
    max_seq_len: int = DEFAULT_MAX_SEQ_LEN
    keep_last_steps: int = DEFAULT_KEEP_LAST_STEPS  # 工具级压缩保护窗口：最近 N 个 step 批次（跨轮次滚动）
    compaction: CompactionConfig | bool | None = True  # None = 不做任何上下文压缩
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
                    if cd.get("enabled") is False:  # 旧键显式关闭 → 整体 None
                        cfg.compaction = None
                        continue
                    cfg.compaction = CompactionConfig()  # 默认三级全开
                    tool = cd.get("tool")
                    if isinstance(tool, dict):
                        cfg.compaction.tool = ToolCompaction(
                            head=int(tool.get("head", ToolCompaction.head)),
                            tail=int(tool.get("tail", ToolCompaction.tail)),
                        )
                    elif tool is False:  # 显式关闭工具级
                        cfg.compaction.tool = None
                    turn = cd.get("turn")
                    if isinstance(turn, dict):
                        cfg.compaction.turn = TurnCompaction(
                            head=int(turn.get("head", TurnCompaction.head)),
                            tail=int(turn.get("tail", TurnCompaction.tail)),
                        )
                    elif turn is False:  # 显式关闭轮次级
                        cfg.compaction.turn = None
                    sd = cd.get("session")
                    if isinstance(sd, dict):
                        cfg.compaction.session = SessionCompaction(
                            head=int(sd.get("head", SessionCompaction.head)),
                            tail=int(sd.get("tail", SessionCompaction.tail)),
                        )
                    elif sd is False:  # 显式关闭会话级
                        cfg.compaction.session = None
                    # 不写子表 = 保持默认开启；旧写法 session = true 无效果（默认即开）
                elif isinstance(cd, bool):  # 旧扁平写法 compaction = true / false
                    cfg.compaction = (
                        CompactionConfig(
                            tool=ToolCompaction(),
                            turn=TurnCompaction(),
                            session=SessionCompaction(),
                        )
                        if cd
                        else None
                    )
            else:
                setattr(cfg, f.name, data[f.name])
        # 更早的旧键 compress_tools/compress_turns/compress_session 迁移（新键优先）
        if "compaction" not in data and any(
            k in data for k in ("compress_tools", "compress_turns", "compress_session")
        ):
            tool = ToolCompaction() if data.get("compress_tools", True) else None
            turn = TurnCompaction() if data.get("compress_turns", True) else None
            session = SessionCompaction() if data.get("compress_session", True) else None
            cfg.compaction = (
                CompactionConfig(tool=tool, turn=turn, session=session)
                if any(c is not None for c in (tool, turn, session))
                else None
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
        # None 表示“未配置/关闭”，TOML 无 null，直接跳过。
        scalars = {k: v for k, v in d.items() if not isinstance(v, dict) and v is not None}
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


def _ensure_global_memory() -> None:
    """首次运行创建全局记忆种子文件（已存在则跳过，不覆盖）。"""
    if GLOBAL_MEMORY_FILE.exists():
        return
    GLOBAL_MEMORY_FILE.parent.mkdir(parents=True, exist_ok=True)
    GLOBAL_MEMORY_FILE.write_text(GLOBAL_MEMORY_TEMPLATE, encoding="utf-8")


def ensure_config(config_file: str | Path | None = None) -> Config:
    """首次运行先交互式写入配置文件，再从文件读取。"""
    _ensure_global_memory()
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

    说明性引导由默认 SYSTEM_PROMPT / SYSTEM.md 承担，此处只做纯内容拼接、不追加解释文字；
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
        parts.append(f"{agents}")

    memories: list[tuple[str, str]] = []
    global_memory = _read_text(GLOBAL_MEMORY_FILE)
    if global_memory:
        memories.append(("全局记忆（~/.pie/memory.md）", global_memory))
    project_memory = _read_text(_resolve_prompt_file(MEMORY_FILE))
    if project_memory:
        memories.append((f"项目记忆（{MEMORY_FILE}）", project_memory))
    if memories:
        block = "\n\n".join(f"## {name}\n{content}" for name, content in memories)
        parts.append(block)
    parts.extend(str(p) for p in append_prompts if p)
    return "\n\n".join(parts)


