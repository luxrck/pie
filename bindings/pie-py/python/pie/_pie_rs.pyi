"""`pie._pie_rs`（PyO3 扩展）的类型存根。

手写、与 Rust 侧一一对应；dict 的形状按「与纯 Python 版 `loop.aturn` 的 `on_event` 一致」写成
`TypedDict`。改了 Rust 的公开面记得同步这里（`pytest` 里有一条对拍键名的用例）。
"""

from __future__ import annotations

from collections.abc import AsyncIterator, Callable, Coroutine, Mapping, Sequence

from ._tool import Tool
from typing import Any, Literal, TypedDict

__all__ = [
    "Cancel",
    "Config",
    "ConfigError",
    "LlmClient",
    "LlmError",
    "PieError",
    "Session",
    "ToolError",
    "ToolRegistry",
    "list_sessions",
    "run",
    "version",
]

# ---------------------------------------------------------------- 事件 / 数据形状

class ContentDelta(TypedDict):
    type: Literal["content_delta"]
    text: str

class ReasoningDelta(TypedDict):
    type: Literal["reasoning_delta"]
    text: str

class ToolCallEvent(TypedDict):
    type: Literal["tool_call"]
    name: str
    arguments: dict[str, Any]
    arguments_raw: str

class ToolResultEvent(TypedDict):
    type: Literal["tool_result"]
    name: str
    text: str
    arguments: dict[str, Any]

class AnswerEvent(TypedDict):
    type: Literal["answer"]
    text: str

TurnEvent = ContentDelta | ReasoningDelta | ToolCallEvent | ToolResultEvent | AnswerEvent
"""`on_event` 收到的事件。"""

class Usage(TypedDict):
    """token 是**最近一次** provider 上报值（不求和），只有 `calls` 累计。"""

    prompt_tokens: int | None
    completion_tokens: int | None
    total_tokens: int | None
    prompt_cache_hit_tokens: int | None
    prompt_cache_miss_tokens: int | None
    reasoning_tokens: int | None
    calls: int

class CompactStats(TypedDict):
    """一次压缩的产出（`Session.compact()` / `/compact`）。"""

    tools: int
    turns: int
    session: int
    saved_tokens: int
    skipped: str | None

class Message(TypedDict, total=False):
    """一条消息（字段与 JSONL 文件一致，含压缩元数据）。"""

    role: str
    content: Any
    tool_calls: list[dict[str, Any]]
    tool_call_id: str
    tool_name: str
    reasoning_content: str
    compress_level: int
    raw_path: str
    raw_hash: str
    synthetic: bool

class BalanceInfo(TypedDict):
    currency: str
    total_balance: str
    granted_balance: str
    topped_up_balance: str

class Balance(TypedDict):
    """`GET /user/balance` 的原样响应（金额是**字符串**）。"""

    is_available: bool
    balance_infos: list[BalanceInfo]

class SessionRow(TypedDict):
    """`list_sessions()` 的一行（键名同 CLI `sessions --json`）。"""

    id: str
    file: str
    mtime: int
    size: int
    turns: int
    api_calls: int
    first_query: str

# ---------------------------------------------------------------- 异常

class PieError(Exception): ...
class ConfigError(PieError): ...
class LlmError(PieError):
    """模型请求失败；服务端报错时实例上带 `status`。"""

    status: int | None

class ToolError(PieError): ...

# ---------------------------------------------------------------- 配置

class Config:
    def __init__(self) -> None: ...

    @staticmethod
    def load(path: str | None = ...) -> Config:
        """读配置文件（不给路径就用 `~/.pie/config.toml`；文件不在 → 默认值）。"""

    # 常用项（其余走 to_dict / update）
    model: str
    base_url: str
    api_key: str
    reasoning_effort: str
    context_window: int
    reserved_tokens: int | None
    """`None` = 不发 `max_tokens`（配置文件里写 `"auto"`）。"""
    max_retries: int
    keep_last_steps: int
    auto_compact_threshold: int | None
    timeout_seconds: float
    compaction: bool
    """`False` = 不做任何压缩；`True` = 三级全开（要细调就改配置文件）。"""
    config_file: str | None
    append_system_prompt: list[str]
    system_prompt: str | None
    """替换基础 system prompt（运行时属性、不落盘）；`None` = 用内置/仓库那份。"""
    parallel_tools: bool
    """同一批 tool_calls 是否并发（工具共享可变状态时必须 False）。"""

    def context_budget(self) -> int:
        """可用输入预算 = `context_window - reserved_tokens`。"""

    def soft_limit(self) -> int:
        """触发自动压缩的水位。"""

    def target_limit(self) -> int:
        """压缩后的目标水位。"""

    def tool_defaults(self) -> dict[str, Any]:
        """按工具名给的私有默认参数（dispatch 时注入）。"""

    def to_dict(self) -> dict[str, Any]:
        """整个配置 → dict（**只含持久字段**，键名同 TOML）。"""

    def update(self, data: Mapping[str, Any]) -> None:
        """用 dict 覆盖配置：只覆盖给出的键，键名写错直接报错。"""

    def save(self) -> str:
        """写回配置文件，返回路径。"""

# ---------------------------------------------------------------- 模型客户端

class LlmClient:
    def __init__(self, config: Config) -> None: ...

    model: str
    """当前模型 id（`Session.set_model()` 会同步它）。"""

    @staticmethod
    def model_supports_files(model: str) -> bool:
        """这个模型支不支持 Files API（图片走 `file` 块）。"""

    def list_models(self) -> list[str]:
        """端点可用模型 id（已排序）。"""

    def fetch_balance(self) -> Balance:
        """查询账号余额（`GET /user/balance`，DeepSeek 扩展）。"""

    def set_reasoning_effort(self, level: str | None = ...) -> None:
        """切换思考深度；`None` / `"none"` = 关闭思考。"""

    def complete(
        self,
        messages: Sequence[Mapping[str, Any]],
        tools: Sequence[Mapping[str, Any]] | None = ...,
    ) -> dict[str, Any]:
        """同步调**一次**模型（不吃工具循环）：返回 `content` / `reasoning_content` /
        `tool_calls` / `usage`。跑回合请用 `Session.aturn`。"""

# ---------------------------------------------------------------- 工具

class ToolRegistry:
    """工具集：`ToolRegistry()` 空表自己 register，或 `builtins()` / `from_spec()`。"""

    def __init__(self) -> None: ...

    @staticmethod
    def builtins(config: Config) -> ToolRegistry:
        """内置工具（read / edit / writ / bash）。"""

    @staticmethod
    def from_spec(spec: str, config: Config | None = ...) -> ToolRegistry:
        """按 `--tools` 那套 spec 裁剪（非内置名 → 受限 bash 的子命令白名单）。"""

    def names(self) -> list[str]: ...
    def specs(self) -> list[dict[str, Any]]:
        """OpenAI 形状的工具定义（可直接塞进请求体）。"""

    def register(
        self,
        tool: Tool | None = ...,
        *,
        name: str | None = ...,
        description: str | None = ...,
        parameters: Mapping[str, Any] | None = ...,
        handler: Callable[..., str] | None = ...,
    ) -> None:
        """注册一个 Python 函数当工具（`@pie.tool` 的封装，或直接给零件）。

        handler 返回值当工具结果文本；抛异常会被文本化回给模型；**必须是同步函数**。
        """

# ---------------------------------------------------------------- 会话

class Cancel:
    """取消信号：传给 `Session.aturn(cancel=…)`，或从别的线程停住正在跑的回合。"""

    def __init__(self) -> None: ...
    def cancel(self) -> None: ...
    @property
    def cancelled(self) -> bool: ...

class Session:
    @staticmethod
    def new(config: Config, llm: LlmClient, tools: ToolRegistry, id: str | None = ...) -> Session:
        """新建会话（落盘，`save()` 时写文件）。"""

    @staticmethod
    def ephemeral(config: Config, llm: LlmClient, tools: ToolRegistry) -> Session:
        """临时会话：不落盘、不写 manifest（服务 / notebook 用）。"""

    @staticmethod
    def load(path: str, config: Config, llm: LlmClient, tools: ToolRegistry) -> Session: ...
    @staticmethod
    def resume(config: Config, llm: LlmClient, tools: ToolRegistry) -> Session: ...

    path: str
    messages: list[Message]
    full_history: list[Message]
    """把压缩指针展开成完整转录（调试视图）。"""
    api_messages: list[Message]
    """**API 形状**的消息（去掉压缩元数据）——存档 / 喂给别的模型用。"""
    usage: Usage
    title: str | None
    turn_count: int
    config: Config
    """当前配置的**快照**（改它不影响会话）。"""

    def summary(self) -> str: ...
    def usage_report(self) -> str:
        """`/stat` 那份报告文本。"""

    def compression_history(self) -> list[dict[str, Any]]: ...
    def save(self) -> None: ...
    def reset(self) -> None:
        """清历史（保留 system prompt 与记忆）。"""

    def push_assistant(self, text: str) -> None:
        """补一条 assistant 消息（纯文本、无工具调用）——嵌入方补历史用。"""

    def clear_window(self) -> int:
        """把当前窗口归档成窗口块后开新窗口；返回手上的窗口块总数。"""

    def compact(self, mode: str) -> CompactStats: ...
    def set_model(self, name: str) -> str:
        """改模型 + 同步客户端 + 写回配置文件（返回提示）。"""

    def set_reasoning_effort(self, level: str) -> str:
        """改思考深度 + 写回配置文件（返回提示）。"""

    def stop(self) -> bool:
        """停住正在跑的回合；返回是否确实发过信号。"""

    def aturn(
        self,
        input: str,
        on_event: Callable[[TurnEvent], None] | None = ...,
        cancel: Cancel | None = ...,
        max_steps: int | None = ...,
        stream: bool | None = ...,
        parallel_tools: bool | None = ...,
    ) -> str:
        """跑一个完整回合，返回最终答复（阻塞；期间释放 GIL）。"""

    def aturn_async(
        self,
        input: str,
        cancel: Cancel | None = ...,
        max_steps: int | None = ...,
        stream: bool | None = ...,
        parallel_tools: bool | None = ...,
    ) -> Coroutine[Any, Any, str]:
        """`await` 版回合（M5）：事件走 `async for ev in session.events()`。

        `task.cancel()` / `session.stop()` 都能真停住（前者靠 Python 侧 glue 桥到 `stop()`）。
        """

    def events(self) -> AsyncIterator[TurnEvent]:
        """本回合的事件流（按回合：先 `aturn_async` 再 `async for`）。"""

    def turn_future(
        self,
        input: str,
        sink: Callable[[dict[str, Any] | None], None],
        cancel: Cancel | None = ...,
        max_steps: int | None = ...,
        stream: bool | None = ...,
        parallel_tools: bool | None = ...,
    ) -> Coroutine[Any, Any, str]:
        """底层口：回合 + 事件经 `sink` 推送（`pie._async` 用它，一般不用手动调）。"""

# ---------------------------------------------------------------- 模块级

def run(
    task: str,
    config: Config | None = ...,
    llm: LlmClient | None = ...,
    tools: ToolRegistry | None = ...,
    max_steps: int | None = ...,
    stream: bool | None = ...,
    parallel_tools: bool | None = ...,
) -> str:
    """一次性任务（无会话、不落盘），返回最终答复。"""

def list_sessions(limit: int | None = ...) -> list[SessionRow]:
    """历史会话（按 mtime 降序；`limit=None` = 全部）。"""

def version() -> str: ...
