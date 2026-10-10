"""`pie._pie_rs`（PyO3 扩展）的类型存根。

手写、与 Rust 侧一一对应；dict 的形状按事件 dict 的口径写成
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
    "arun",
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
    # 工具产出的图像（本地文件路径）——模型看不到，只有界面用（现在只有 `repl` 给）
    images: list[str]

class AnswerEvent(TypedDict):
    type: Literal["answer"]
    text: str

# ———— 会话生命周期（`SessionEvent`）————

class TurnStartEvent(TypedDict):
    type: Literal["turn_start"]
    input: str

class TurnDoneEvent(TypedDict):
    type: Literal["turn_done"]
    answer: str
    error: str | None
    elapsed_ms: int

class CompactedEvent(TypedDict):
    type: Literal["compacted"]
    level: int
    path: str
    hash: str

class ClearedEvent(TypedDict):
    type: Literal["cleared"]
    archived: str
    count: int

class ModelChangedEvent(TypedDict):
    type: Literal["model_changed"]
    # 不叫 `from`：那是 Python 关键字（Rust 侧原就叫 `from`，已特意避开）
    previous: str
    current: str

class CwdChangedEvent(TypedDict):
    type: Literal["cwd_changed"]
    to: str

class StartEvent(TypedDict):
    type: Literal["start"]
    id: str
    # `True` = 从磁盘恢复的会话（`Session.load`），`False` = 新建的
    resume: bool

TurnEvent = ContentDelta | ReasoningDelta | ToolCallEvent | ToolResultEvent | AnswerEvent
"""回合内事件（模型说话 / 工具活动）。"""

SessionEvent = (
    TurnStartEvent
    | TurnDoneEvent
    | CompactedEvent
    | ClearedEvent
    | ModelChangedEvent
    | CwdChangedEvent
    | StartEvent
)
"""会话生命周期事件。"""

PieEvent = TurnEvent | SessionEvent
"""`on_event` / `session.events()` 收到的**统一信封**（回合 + 会话）。"""

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
    skipped: str | None

class Message(TypedDict, total=False):
    """一条消息（字段与 JSONL 文件一致，含压缩元数据 `compaction`）。"""

    role: str
    content: Any
    tool_calls: list[dict[str, Any]]
    tool_call_id: str
    tool_name: str
    reasoning_content: str
    compaction: dict[str, Any]
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
        reasoning_effort: str | None = ...,
        response_format: Literal["text", "json_object"] | None = ...,
    ) -> dict[str, Any]:
        """同步调**一次**模型（不吃工具循环）：返回 `content` / `reasoning_content` /
        `tool_calls` / `usage`。跑回合请用 `Session.aturn`。

        `reasoning_effort`（`None` = 用客户端的思考深度；`"none"` = 关闭思考）与
        `response_format`（`None` / `"text"` = 不发该字段；`"json_object"` = 要求合法 JSON）
        是**按次**覆盖（口径同 `Session.aturn`）。"""

# ---------------------------------------------------------------- 工具

class ToolRegistry:
    """工具集：`ToolRegistry()` 空表自己 register，或 `builtins()` / `from_spec()`。"""

    def __init__(self) -> None: ...

    @staticmethod
    def builtins(config: Config) -> ToolRegistry:
        """内置工具（read / edit / writ / bash / repl）。"""

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
    """取消信号（独立对象，不绑定任何会话）。`Session` 自己有取消信号 —— `Session.stop()` 就够用。"""

    def __init__(self) -> None: ...
    def cancel(self) -> None: ...
    @property
    def cancelled(self) -> bool: ...

class Subscription:
    """`Session.on(..)` 的返回值：退订凭据。

    ⚠ **要留着它** —— 一被回收（或 `.close()`）就自动退订，之后就再也收不到事件。
    """

    def close(self) -> None:
        """退订（幂等）。之后不再收到任何事件。"""

    @property
    def closed(self) -> bool:
        """已经退订了吗？"""

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

    def compression_history(self) -> list[dict[str, Any]]:
        """本会话的压缩事件流水（`__meta__.compaction_events`）。"""
        ...
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

    def on(self, callback: Callable[[dict[str, Any]], None]) -> Subscription:
        """**跨回合订阅**本会话的事件：`callback(event: dict)`，形状与 `turn(on_event=…)` 同一套。

        与 `turn(on_event=…)` 的区别：那个只管**这一次**回合；`on` 一直有效，直到丢掉返回的
        `Subscription`（或 `.close()`）—— 所以 `set_model()` / `clear_window()` 这些**回合之外**
        的动作也能收到。

        ⚠ 回调跑在**发事件那条线程**上（回合期间是 Rust 侧的 worker 线程）：回调里别阻塞、
        别碰同一个 Session；回调抛异常会打到 stderr（不会传回调用方）。
        """

    def turn(
        self,
        input: str,
        on_event: Callable[[TurnEvent | SessionEvent], None] | None = ...,
        max_steps: int | None = ...,
        stream: bool | None = ...,
        parallel_tools: bool | None = ...,
        reasoning_effort: str | None = ...,
        response_format: Literal["text", "json_object"] | None = ...,
    ) -> str:
        """跑一个完整回合，返回最终答复（**同步**版：阻塞；期间释放 GIL）。

        要 await 用 `aturn`。后五个都是**按次**的执行旋钮（不写回配置）：`reasoning_effort`
        = 本回合的思考深度（`None` = 用配置里的；`"none"` = 关闭思考），`response_format` =
        `None` / `"text"`（不发该字段）或 `"json_object"`（要求合法 JSON 输出——⚠ 还得自己在
        prompt 里交代）。"""

    def aturn(
        self,
        input: str,
        max_steps: int | None = ...,
        stream: bool | None = ...,
        parallel_tools: bool | None = ...,
        reasoning_effort: str | None = ...,
        response_format: Literal["text", "json_object"] | None = ...,
    ) -> Coroutine[Any, Any, str]:
        """`await` 版回合（M5）：返回可 await 的对象，返回值同 `turn`。

        事件走 `async for ev in session.events()`；按次旋钮同 `turn`。
        `task.cancel()` / `session.stop()` 都能真停住（前者靠 Python 侧 glue 桥到 `stop()`）。
        """

    def events(self) -> AsyncIterator[TurnEvent | SessionEvent]:
        """本回合的事件流（按回合：先 `aturn` 再 `async for`）。"""

    def turn_future(
        self,
        input: str,
        sink: Callable[[dict[str, Any] | None], None],
        max_steps: int | None = ...,
        stream: bool | None = ...,
        parallel_tools: bool | None = ...,
        reasoning_effort: str | None = ...,
        response_format: Literal["text", "json_object"] | None = ...,
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
    reasoning_effort: str | None = ...,
    response_format: Literal["text", "json_object"] | None = ...,
) -> str:
    """一次性任务（无会话、不落盘），返回最终答复。**同步**版；按次旋钮同 `Session.turn`。"""

def arun(
    task: str,
    config: Config | None = ...,
    llm: LlmClient | None = ...,
    tools: ToolRegistry | None = ...,
    max_steps: int | None = ...,
    stream: bool | None = ...,
    parallel_tools: bool | None = ...,
    reasoning_effort: str | None = ...,
    response_format: Literal["text", "json_object"] | None = ...,
) -> Coroutine[Any, Any, str]:
    """`run` 的**异步**版：语义一样（一次性、无会话、不落盘），返回可 await 的对象。

    不绕线程（回合跑在进程级 runtime 上）；⚠ 要求调用时处于运行中的 asyncio loop。"""

def list_sessions(limit: int | None = ...) -> list[SessionRow]:
    """历史会话（按 mtime 降序；`limit=None` = 全部）。"""

def llm(
    prompt: str,
    *,
    system: str | None = ...,
    model: str | None = ...,
    config: Config | None = ...,
    max_tokens: int | None = ...,
    reasoning_effort: str | None = ...,
    response_format: Literal["text", "json_object"] | None = ...,
) -> str:
    """一次性问一句，返回 assistant 正文（内部就是 `LlmClient.complete`：有重试、期间释放 GIL）。

    `config=None` → 读 `~/.pie/config.toml`；`model` / `max_tokens` 是按次覆盖。
    要 `usage` / `tool_calls` 用 `LlmClient.complete`；要工具循环 / 事件 / 流式用 `Session`。"""

def allm(
    prompt: str,
    *,
    system: str | None = ...,
    model: str | None = ...,
    config: Config | None = ...,
    max_tokens: int | None = ...,
    reasoning_effort: str | None = ...,
    response_format: Literal["text", "json_object"] | None = ...,
) -> Coroutine[Any, Any, str]:
    """`llm` 的**异步**版：跑在进程级 runtime 上（不占线程）；⚠ 要求运行中的 asyncio loop。"""

def version() -> str: ...
