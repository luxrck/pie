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

from .input import read_input
from .theme import DEFAULT_THEME_NAME

# 可通过 PIE_DIR 环境变量重定向（测试/多环境），默认 ~/.pie
PIE_DIR = Path(os.environ.get("PIE_DIR") or Path.home() / ".pie")
CONFIG_FILE = PIE_DIR / "config.toml"
LEGACY_CONFIG_FILE = PIE_DIR / "config.json"
GLOBAL_MEMORY_FILE = PIE_DIR / "memory.md"

DEFAULT_MODEL = "deepseek-flash"
DEFAULT_BASE_URL = "https://api.deepseek.com/"
DEFAULT_API_KEY = "sk-469cb4875eac409bb8e4c1419c03d0bf"
DEFAULT_REASONING_EFFORT = "high"

# 思考深度（/reasoning 命令与配置 reasoning_effort 的合法值）
REASONING_LEVELS = ("none", "low", "high", "max")
REASONING_NONE = REASONING_LEVELS[0]  # none = 关闭思考：请求时不发 reasoning_effort

# 上下文默认值：context_window = 模型最大上下文长度（服务端把输入 + 输出算在一起）；
# reserved_tokens = 每次请求为输出预留的 token（就是发给 API 的 max_tokens）。
# 于是「可用输入预算 = context_window - reserved_tokens」。
DEFAULT_CONTEXT_WINDOW = 1024 * 1024
DEFAULT_KEEP_LAST_STEPS = 5
# 压缩水位比例：相对「可用输入预算」（不是整个窗口）
DEFAULT_SOFT_RATIO = 0.8
DEFAULT_TARGET_RATIO = 0.55

# 每次请求为输出预留的 token：默认 128000（128k）；显式设为 None 才不发送该参数（用服务端默认）
# （DeepSeek：不传时非思考 8K / 思考 64K / reasoning_effort=max 时 128K；上限 384K）
DEFAULT_RESERVED_TOKENS = 128_000
RESERVED_TOKENS_AUTO = ("auto", "default", "none", "0", "")

# 图片上传（DeepSeek Files API，见 files.py）：默认开启，任何失败都静默回退内联 base64。
# files_ttl_days = 上传件在服务端的保留天数（1~30；0 = 不设过期、永久保留）。
DEFAULT_FILES_API = True
DEFAULT_FILES_TTL_DAYS = 30
# read 图片的字节上限：内联时受单图 32 MiB 限制；开了 Files API 后放宽到 64 MiB（file_id 单图上限）。
IMAGE_MAX_BYTES_INLINE = 32 * 1024 * 1024
IMAGE_MAX_BYTES_FILES = 64 * 1024 * 1024


def parse_reserved_tokens(raw: str) -> int | None:
    """解析 reserved_tokens 输入：auto/default/none/0/空 → None（不发送 max_tokens，用服务端默认）；
    支持 64k / 384K 简写。

    非法输入抛 ValueError（由调用方转成提示文案）。
    """
    text = (raw or "").strip().lower()
    if text in RESERVED_TOKENS_AUTO:
        return None
    multiplier = 1
    if text.endswith("k"):
        multiplier, text = 1000, text[:-1]
    elif text.endswith("m"):
        multiplier, text = 1_000_000, text[:-1]
    try:
        value = int(float(text) * multiplier)
    except ValueError:
        raise ValueError(f"无法识别的 token 数: {raw!r}（例：65536 / 64k / auto）") from None
    if value < 1:
        raise ValueError("reserved_tokens 必须 ≥ 1（要恢复默认请用 auto）")
    return value


# 提示词文件路径固化（不再作为配置项）
SYSTEM_FILE = "SYSTEM.md"
AGENTS_FILE = "AGENTS.md"
MEMORY_FILE = "MEMORY.md"


SYSTEM_PROMPT = """\
你是 pie，一个运行在用户机器上的自动化 agent，通过反复调用工具完成用户任务。

## 上下文压缩

长对话中，pie 会把旧内容压缩成「文件指针 + 摘要」，信息不会删除，只是换了一种表示。具体如下：
- 工具输出截断：`[工具输出全文已保存: 路径]`（或 `[shell 输出全文已保存: 路径]`）后只附首尾若干行，中间被省略。
- 轮次压缩：`[轮次原文已保存: 路径]` 代表一轮完整的「用户 → 思考 → 工具调用 → 结果 → 回复」，摘要只保留该轮次的模型最终输出，中间过程被省略时用 `...[中间过程省略]...` 显式标注（无中间过程则不标注）。
- 历史窗口摘要：`[历史窗口: 路径]` 后是规则式摘要，只保留最早 N 轮和最晚 M 轮的“用户提问 + 最终回复”，中间被省略的轮次用 `...[中间省略 N 轮]...` 显式标注；其中的 `pie:` 行是当时的最终回答，不是工具结果。

## 核心原则

- YOLO 模式：所有工具直接执行，无权限确认。这要求你更谨慎、更准确，避免破坏性操作。
- 先理解再动手：修改文件前，先用 read 或 shell 了解现状。
- 最小改动：小修改用 edit，整体重写才用 write。
- 用 shell 验证：写完代码就运行；状态不明就看目录、查日志、跑测试。
- 不假装完成：工具报错就读错误信息并修正；多种方法都失败时如实汇报，不编造成功。
- 当你遇到看起来上下文信息不足的提问时，先观察当前会话是否经过压缩，优先查找分析当前会话历史文件以补全上下文信息。

## 可用工具

- `read`：读取文件内容（UTF-8，不截断；二进制返回大小提示）。offset 为 1 起的起始行，limit 为最大读取行数，用于大文件分页读取。读取图片（PNG/JPEG/GIF/WebP/BMP）时返回图像引用，图片内容会作为多模态图像消息随对话发送给模型，offset/limit 不适用。
- `edit`：一次做多个精确替换。每个 edits 项的 oldText 在原文中必须唯一且互不重叠，按原文一次性应用，相邻改动请合并成一个 edit。
- `write`：写入文件，自动创建父目录，覆盖已有内容。
- `shell`：执行 shell 命令，返回 stdout/stderr 和退出码。超长输出不做内部截断，交回 harness 由工具级压缩（head+tail 落盘指针）处理。

## 工作流程

1. 拆解任务，先用 read / shell 了解现状（大文件先用 `wc -l` 或 `rg` 定位，再用 read 按行范围分段读）。
2. 按需调用工具，一次只做必要的事。
3. 每步确认结果，出错即修正。
4. 全部完成后，用简洁的中文总结：做了什么、结果如何、有无遗留问题。

## 记忆管理

你有两层记忆文件，用 `edit`/`write` 更新，只记跨会话有效的长期事实：
- **全局记忆**：`~/.pie/memory.md` — 跨项目的用户偏好、通用编码风格、工具链路径、个人习惯。
- **项目记忆**：`<项目根>/MEMORY.md` — 当前项目的架构决策、业务逻辑、技术选型、待办事项。

**当记忆冲突时，以项目记忆优先**，若冲突则在回答开头提醒：`⚠️ 记忆冲突：全局 X，项目 Y，我将遵循 Y`。

| 写入内容 | 目标文件 | 触发条件 |
|---------|---------|---------|
| 跨项目通用经验（编码风格、工具习惯、个人设置等） | `~/.pie/memory.md` | 用户显式要求；或从多次修正反馈提炼出的新习惯 |
| 项目专属决策经验（架构选型、业务规则、技术栈版本等） | `<项目根>/MEMORY.md` | 完成重大功能、修复复杂 Bug、重构后；或用户说“记住这个决策” |
| 待办事项 / 路线图 | `<项目根>/MEMORY.md` | 规划新阶段、完成里程碑后更新进度 |
"""

GLOBAL_MEMORY_TEMPLATE = """\
# 全局记忆（~/.pie/memory.md）

跨项目的持久记忆：用户偏好、关键约定、踩过的坑。每次会话自动注入 system prompt，保持简洁。

只记跨会话仍然有效的事实。不记临时状态（临时文件路径、报错堆栈、调试日志）；密钥放 `.env`；项目专属内容写到项目根 `MEMORY.md`。

## 写入时机

- 用户显式要求记住；或从多次修正反馈中提炼出的新习惯。
- 完成重大功能、修复复杂 Bug、重构后沉淀的关键决策。

## 内容分类

- **编码风格／工具链**：跨项目的代码风格、命名习惯、工具路径、个人设置。
- **关键约定与踩坑**：容易再踩的规则，如「xxx 必须先 yyy，否则报 zzz」。

说明段落不要删除，追加内容写到对应分类下。
"""


@dataclass
class ToolCompaction:
    """工具级（level 1）压缩保留的行数。"""

    head: int = 30
    tail: int = 50


@dataclass
class SessionCompaction:
    """会话级（level 3）压缩摘要：保留开头 head 个轮次 + 末尾 tail 个轮次，
    每个轮次只保留 <user_q, model_last_response>（规则式，后续可换 LLM 摘要）。
    为 None 表示关闭会话级压缩。"""

    head: int = 3
    tail: int = 5


@dataclass
class TuiConfig:
    """TUI 渲染配置（[tui]）：lean = true 时用简洁模式渲染。

    简洁模式只给 user / assistant 消息套盒子，工具调用与工具结果压成单行
    （`icon 工具名 摘要`，结果行尾带 ✅/❌，失败时下方缩进输出错误正文）。
    """

    lean: bool = False


@dataclass
class CompactionConfig:
    """压缩总配置：写了 [compaction] 即开启，默认三级全开；
    某级为 None（显式 tool/turn/session = false）表示关闭该级；
    整个 compaction 为 None（不写 [compaction]）表示不做任何压缩。

    两个水位比例都相对「可用输入预算」（= context_window - reserved_tokens）：
    soft_ratio 触发自动压缩，target_ratio 是压缩后的目标水位（迟滞防抖，应小于 soft_ratio）。
    """

    tool: ToolCompaction | None = field(default_factory=ToolCompaction)
    turn: bool | None = True  # 轮次级（level 2）：摘要只保留用户输入 + 模型最终输出，中间过程显式省略标注
    session: SessionCompaction | None = field(default_factory=SessionCompaction)
    soft_ratio: float = DEFAULT_SOFT_RATIO  # 软阈值 = 可用输入预算 × soft_ratio
    target_ratio: float = DEFAULT_TARGET_RATIO  # 目标水位 = 可用输入预算 × target_ratio


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
    reserved_tokens: int | None = DEFAULT_RESERVED_TOKENS  # 每次请求为输出预留的 token（= API 的 max_tokens；None = 不发该参数、用服务端默认）
    context_window: int = DEFAULT_CONTEXT_WINDOW  # 模型最大上下文长度（输入 + 输出一起算）
    keep_last_steps: int = DEFAULT_KEEP_LAST_STEPS  # 工具级压缩保护窗口：最近 N 个 step 批次（跨轮次滚动）
    compaction: CompactionConfig | bool | None = field(default_factory=CompactionConfig)  # None = 不做任何上下文压缩
    timeout_seconds: float = 60.0  # HTTP 超时（OpenAI 兼容客户端）
    max_retries: int = 2  # 请求重试次数
    max_retry_delay_seconds: float = 1.0  # 重试间隔（客户端内部退避时保留字段）
    verbose: bool = True
    theme: str = DEFAULT_THEME_NAME  # TUI 主题：族名（如 catppuccin，按终端明暗自适应）或变体名（catppuccin-mocha / catppuccin-latte）
    # 按工具名设置默认私有参数（下划线开头，不进 schema）：如 read: {_max_lines, _max_bytes, _max_image_bytes}
    tools: dict[str, dict[str, Any]] = field(default_factory=dict)
    tui: TuiConfig = field(default_factory=TuiConfig)  # TUI 渲染（[tui] lean = true → 简洁模式）
    files_api: bool = DEFAULT_FILES_API  # 图片走 Files API（上传一次拿 file_id，失败回退内联）
    files_ttl_days: int = DEFAULT_FILES_TTL_DAYS  # 上传件在服务端的保留天数（1~30；0 = 永久）

    def __post_init__(self) -> None:
        # 归一化旧 bool 写法（compaction = true / false），保证下游只见到
        # CompactionConfig（开启）或 None（关闭），避免 bool.session 崩溃。
        if self.compaction is True:
            self.compaction = CompactionConfig()
        elif self.compaction is False:
            self.compaction = None

    def tool_defaults(self) -> dict[str, dict[str, Any]]:
        """工具私有默认参数（下划线开头，由 dispatch 注入）。用户配置优先，未配置时给派生默认。

        目前唯一的派生默认是 `read` 的 `_max_image_bytes`：内联受单图 32 MiB 上限约束，
        开了 Files API 后放宽到 64 MiB（file_id 单图上限）。
        """
        defaults = {
            name: dict(values) for name, values in self.tools.items() if isinstance(values, dict)
        }
        image_cap = IMAGE_MAX_BYTES_FILES if self.files_api else IMAGE_MAX_BYTES_INLINE
        defaults.setdefault("read", {}).setdefault("_max_image_bytes", image_cap)
        return defaults

    def context_budget(self) -> int:
        """可用输入预算：服务端按「输入 tokens + max_tokens ≤ 窗口」判超限，
        所以真正能装历史的只有 context_window - reserved_tokens。"""
        return max(1, self.context_window - int(self.reserved_tokens or 0))

    def _ratio(self, *, soft: bool) -> float:
        comp = self.compaction if isinstance(self.compaction, CompactionConfig) else None
        if comp is None:  # 压缩关闭时水位无用，给默认值只为展示
            return DEFAULT_SOFT_RATIO if soft else DEFAULT_TARGET_RATIO
        return comp.soft_ratio if soft else comp.target_ratio

    @property
    def soft_ratio(self) -> float:
        """软阈值比例（相对可用输入预算）。"""
        return self._ratio(soft=True)

    @property
    def target_ratio(self) -> float:
        """目标水位比例（相对可用输入预算）。"""
        return self._ratio(soft=False)

    def soft_limit(self) -> int:
        """软阈值：触发自动压缩的 token 水位；--auto-compact-threshold 覆盖。"""
        override = getattr(self, "auto_compact_threshold", None)
        if override:
            return int(override)
        return max(1, int(self.context_budget() * self.soft_ratio))

    def target_limit(self) -> int:
        """目标水位：压缩后应降到该值以下（相对同一份可用输入预算）。"""
        return max(1, int(self.context_budget() * self.target_ratio))

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
        # 旧键迁移（2026-09 改名）：max_tokens → reserved_tokens、max_seq_len → context_window
        for old, new in (("max_tokens", "reserved_tokens"), ("max_seq_len", "context_window")):
            if old in data and new not in data:
                data[new] = data[old]
        for f in fields(cls):
            if f.name not in data:
                continue
            if f.name == "tui":
                td = data["tui"]
                if isinstance(td, dict):
                    cfg.tui = TuiConfig(lean=bool(td.get("lean", TuiConfig.lean)))
            elif f.name == "compaction":
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
                    if isinstance(turn, bool):  # 新版 bool：true 开启 / false 关闭（旧 dict 写法视为开启）
                        cfg.compaction.turn = turn
                    elif turn is False:  # 显式关闭轮次级（未写 turn / 旧 dict 写法保持默认开启）
                        cfg.compaction.turn = False
                    sd = cd.get("session")
                    if isinstance(sd, dict):
                        cfg.compaction.session = SessionCompaction(
                            head=int(sd.get("head", SessionCompaction.head)),
                            tail=int(sd.get("tail", SessionCompaction.tail)),
                        )
                    elif sd is False:  # 显式关闭会话级
                        cfg.compaction.session = None
                    # 不写子表 = 保持默认开启；旧写法 session = true 无效果（默认即开）
                    # 水位比例：相对可用输入预算；旧顶层键 context_soft_ratio/context_target_ratio 迁进来
                    comp = cfg.compaction
                    comp.soft_ratio = float(
                        cd.get("soft_ratio", data.get("context_soft_ratio", comp.soft_ratio))
                    )
                    comp.target_ratio = float(
                        cd.get("target_ratio", data.get("context_target_ratio", comp.target_ratio))
                    )
                elif isinstance(cd, bool):  # 旧扁平写法 compaction = true / false
                    cfg.compaction = (
                        CompactionConfig(
                            tool=ToolCompaction(),
                            turn=True,
                            session=SessionCompaction(),
                            soft_ratio=float(data.get("context_soft_ratio", DEFAULT_SOFT_RATIO)),
                            target_ratio=float(data.get("context_target_ratio", DEFAULT_TARGET_RATIO)),
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
            turn = bool(data.get("compress_turns", True))
            session = SessionCompaction() if data.get("compress_session", True) else None
            cfg.compaction = (
                CompactionConfig(tool=tool, turn=turn, session=session)
                if any(c is not None for c in (tool, turn, session))
                else None
            )
        if "compaction" not in data and isinstance(cfg.compaction, CompactionConfig):
            # 没有 [compaction] 段时，旧顶层水位比例直接写进默认 compaction
            if "context_soft_ratio" in data:
                cfg.compaction.soft_ratio = float(data["context_soft_ratio"])
            if "context_target_ratio" in data:
                cfg.compaction.target_ratio = float(data["context_target_ratio"])
        if not path.exists() and path == CONFIG_FILE and LEGACY_CONFIG_FILE.exists():
            cfg.save()
        # reserved_tokens 容错：允许手写成字符串（"auto" / "64k" / "384K"），0/负数按“不发送”处理
        if isinstance(cfg.reserved_tokens, str):
            try:
                cfg.reserved_tokens = parse_reserved_tokens(cfg.reserved_tokens)
            except ValueError:
                cfg.reserved_tokens = DEFAULT_RESERVED_TOKENS  # 认不出的写法按默认值处理，不让它挡住启动
        elif isinstance(cfg.reserved_tokens, int) and cfg.reserved_tokens < 1:
            cfg.reserved_tokens = None
        cfg.config_file = str(path)  # 运行时属性：记录配置来源路径
        return cfg

    def save(self, config_file: str | Path | None = None) -> "Config":
        path = _resolve_config_file(config_file)
        path.parent.mkdir(parents=True, exist_ok=True)
        data = asdict(self)
        # TOML 没有 null：reserved_tokens = None（不发送 max_tokens）写成 "auto"，load 时再解析回去
        if data.get("reserved_tokens", 0) is None:
            data["reserved_tokens"] = "auto"
        path.write_text(_toml_dump(data), encoding="utf-8")
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
        tables = {k: v for k, v in d.items() if isinstance(v, dict) and v}
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


def _context_line(config) -> str:
    """system prompt 里的上下文说明：窗口多大、预留给输出多少、历史能占多少。"""
    reserved = (
        f"为输出预留 {config.reserved_tokens:,}" if config.reserved_tokens else "输出预留用服务端默认"
    )
    return (
        f"当前模型上下文窗口：{config.context_window:,} tokens（{reserved}，"
        f"可用输入预算约 {config.context_budget():,}）"
    )


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
        _context_line(config),
        f"当前工作目录：{os.getcwd()}",
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





