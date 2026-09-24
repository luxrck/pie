"""`pie` 绑定的回归测试（不联网）。

假 LLM 端点：本地 `http.server` 回放固定的 OpenAI 风格 SSE —— 第一次请求让模型「调一个工具」，
第二次请求给最终答复。这样整条主链（流式解析 → 工具执行 → 事件回传 → 落库）都真的跑了一遍，
但一个字节都不出网。

跑法：`cd pie/bindings/pie-py && .venv/bin/python -m pytest tests -q`
"""

from __future__ import annotations

import json
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

import pie

# ---------------------------------------------------------------- 假端点


def _sse(*chunks: dict) -> bytes:
    body = "".join(f"data: {json.dumps(c, ensure_ascii=False)}\n\n" for c in chunks)
    return (body + "data: [DONE]\n\n").encode()


def _tool_call_chunk(call_id: str, name: str, arguments: str, index: int = 0) -> dict:
    return {
        "choices": [
            {
                "delta": {
                    "tool_calls": [
                        {
                            # ⚠️ `index` 是流式里 tool_call 的**槽位号**：两个调用必须给不同 index，
                            # 否则客户端会把它们当成同一个的两段增量合并。
                            "index": index,
                            "id": call_id,
                            "type": "function",
                            "function": {"name": name, "arguments": arguments},
                        }
                    ]
                }
            }
        ]
    }


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):  # 别把访问日志打到 stderr
        pass

    def do_GET(self):  # noqa: N802（http.server 的命名）
        """`GET /user/balance`（查余额）；其余路径 404。"""
        if not self.path.endswith("/user/balance"):
            self.send_error(404)
            return
        if not getattr(self.server, "balance_ok", True):
            self.send_error(404)
            return
        body = json.dumps(
            {
                "is_available": True,
                "balance_infos": [
                    {
                        "currency": "CNY",
                        "total_balance": "110.00",
                        "granted_balance": "10.00",
                        "topped_up_balance": "100.00",
                    }
                ],
            }
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):  # noqa: N802（http.server 的命名）
        if not self.path.endswith("/v1/chat/completions"):
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return

        length = int(self.headers.get("Content-Length", 0))
        request = json.loads(self.rfile.read(length) or b"{}")
        self.server.requests.append(request)  # type: ignore[attr-defined]

        if self.server.delay:  # type: ignore[attr-defined]
            time.sleep(self.server.delay)  # type: ignore[attr-defined]

        role_of_last = request["messages"][-1]["role"]
        if role_of_last == "tool":  # 工具结果已回传 → 给最终答复
            text = "搞定了"
            tool_calls = None
            chunks = (
                {"choices": [{"delta": {"content": "搞定"}}]},
                {"choices": [{"delta": {"content": "了"}, "finish_reason": "stop"}]},
                {
                    "usage": {
                        "prompt_tokens": 42,
                        "completion_tokens": 7,
                        "total_tokens": 49,
                    }
                },
            )
        else:  # 第一轮 → 让模型调 shell
            text = ""
            tool_calls = [
                {
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": getattr(self.server, "tool_name", "bash"),
                        "arguments": getattr(self.server, "tool_arguments", '{"command": "echo hi"}'),
                    },
                }
            ]
            chunks = (
                _tool_call_chunk(
                    "call_1",
                    getattr(self.server, "tool_name", "bash"),
                    getattr(self.server, "tool_arguments", '{"command": "echo hi"}'),
                ),
                {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]},
                {"usage": {"prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14}},
            )
            if getattr(self.server, "two_calls", False):
                # 一批两个 tool_call：给「并发 vs 串行」的计时用例用
                tool_calls = [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "bash", "arguments": '{"command": "sleep 0.6; echo one"}'},
                    },
                    {
                        "id": "call_2",
                        "type": "function",
                        "function": {"name": "bash", "arguments": '{"command": "sleep 0.6; echo two"}'},
                    },
                ]
                chunks = (
                    _tool_call_chunk("call_1", "bash", '{"command": "sleep 0.6; echo one"}', 0),
                    _tool_call_chunk("call_2", "bash", '{"command": "sleep 0.6; echo two"}', 1),
                    {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]},
                    {"usage": {"prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14}},
                )

        # 非流式（`aturn(stream=False)`）→ 一次性 JSON；与上面的 SSE 路线同一份内容
        if not request.get("stream"):
            body = json.dumps(
                {
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": text or None,
                                "tool_calls": tool_calls,
                            }
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 11,
                        "completion_tokens": 3,
                        "total_tokens": 14,
                    },
                }
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        payload = _sse(*chunks)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


@pytest.fixture()
def fake_llm():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
    server.requests = []  # type: ignore[attr-defined]
    server.delay = 0.0  # type: ignore[attr-defined]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        thread.join(timeout=5)


@pytest.fixture()
def env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, fake_llm):
    """把 `~/.pie` 指到临时目录（别碰真实会话/配置），并给一个指向假端点的配置。"""
    monkeypatch.setenv("PIE_DIR", str(tmp_path / "pie"))
    cfg = pie.Config()
    cfg.base_url = f"http://127.0.0.1:{fake_llm.server_port}/v1/"
    cfg.api_key = "test-key"
    cfg.model = "test-model"
    llm = pie.LlmClient(cfg)
    tools = pie.ToolRegistry.builtins(cfg)
    return cfg, llm, tools, fake_llm


# ---------------------------------------------------------------- 用例


def test_config_defaults_and_repr_masks_key():
    cfg = pie.Config()
    assert cfg.model and cfg.base_url and cfg.context_window > 0
    # ⚠ 核心 `Config::default()` 里压缩是**开着**的（`Some(CompactionConfig::default())`）：
    # 与 Python 版「不写 `[compaction]` 就不压」不同，这里是「不写就按默认三级压」。
    assert cfg.compaction is True
    cfg.compaction = False
    assert cfg.compaction is False
    # api_key 不许整串漏进 repr
    assert cfg.api_key not in repr(cfg)
    assert "…" in repr(cfg)


def test_exception_hierarchy():
    assert issubclass(pie.ConfigError, pie.PieError)
    assert issubclass(pie.LlmError, pie.PieError)
    assert issubclass(pie.ToolError, pie.PieError)
    assert issubclass(pie.PieError, Exception)


def test_fetch_balance_returns_the_documented_shape(env):
    """`LlmClient.fetch_balance()`（`GET /user/balance`）：金额是**字符串**，与服务端一致。"""
    cfg, llm, tools, server = env
    balance = llm.fetch_balance()
    assert balance["is_available"] is True
    info = balance["balance_infos"][0]
    assert info["currency"] == "CNY"
    assert info["total_balance"] == "110.00"
    assert info["granted_balance"] == "10.00"
    assert info["topped_up_balance"] == "100.00"

    # 端点没有这个接口（非 OpenAI 兼容的常见情形）→ 抛 LlmError，不当崩溃
    server.balance_ok = False
    with pytest.raises(pie.LlmError):
        llm.fetch_balance()


def test_turn_runs_tool_and_streams_events(env):
    cfg, llm, tools, server = env
    session = pie.Session.ephemeral(cfg, llm, tools)

    events = []
    answer = session.aturn("打个招呼", on_event=events.append)

    assert answer == "搞定了"
    kinds = [e["type"] for e in events]
    assert kinds == ["tool_call", "tool_result", "content_delta", "content_delta"]

    call = events[0]
    assert call["name"] == "bash"
    assert call["arguments"] == {"command": "echo hi"}  # 参数已解析成 dict
    assert call["arguments_raw"] == '{"command": "echo hi"}'

    result = events[1]
    assert result["name"] == "bash"
    # 成功**只有正文**（`[exit=N]` 头只在失败时给，2026-09-23 定稿）：正文就是 `echo` 的输出，
    # 说明 shell 真跑了
    assert result["text"] == "hi\n"
    assert "hi" in result["text"]

    assert [e["text"] for e in events[2:]] == ["搞定", "了"]

    # 历史：user → assistant(tool_calls) → tool → assistant
    roles = [m["role"] for m in session.messages]
    assert roles == ["system", "user", "assistant", "tool", "assistant"]
    assert session.messages[3]["content"] == "hi\n"
    assert session.messages[-1]["content"] == "搞定了"

    # 两次请求都带上了工具 schema，且第二次把工具结果回传了
    assert len(server.requests) == 2
    assert [t["function"]["name"] for t in server.requests[0]["tools"]][:1] == ["read"]
    assert server.requests[1]["messages"][-1]["role"] == "tool"

    # 用量按 provider 上报累计
    assert session.usage["calls"] == 2
    assert session.usage["prompt_tokens"] == 42
    assert "上下文" in session.usage_report()
    assert "1 轮历史" in session.summary()


def test_ephemeral_session_never_touches_disk(env, tmp_path):
    cfg, llm, tools, _ = env
    session = pie.Session.ephemeral(cfg, llm, tools)
    session.aturn("打个招呼")
    sessions_dir = tmp_path / "pie" / "sessions"
    assert not sessions_dir.exists() or not any(sessions_dir.iterdir())

    # 落盘会话则真的写文件，且能被 load 回来
    saved = pie.Session.new(cfg, llm, tools, id="bindings-test")
    saved.aturn("打个招呼")
    saved.save()
    assert Path(saved.path).exists()

    reopened = pie.Session.load(saved.path, cfg, llm, tools)
    assert reopened.turn_count == 1
    assert reopened.messages[-1]["content"] == "搞定了"


def test_callback_may_not_reenter_the_session(env):
    cfg, llm, tools, _ = env
    session = pie.Session.ephemeral(cfg, llm, tools)
    seen, busy = [], []

    def on_event(event):
        seen.append(event["type"])
        try:
            session.usage_report()
        except RuntimeError as exc:
            busy.append(str(exc))

    session.aturn("打个招呼", on_event=on_event)
    assert seen  # 回调确实跑过
    # ⚠️ 不能要求**每一个**回调都撞上锁：回合可能在 pump 线程排空队列之前就结束了
    # （最后几个事件是锁释放之后才被取到的）→ 只断言「回合进行中确实锁着」。
    assert busy, f"回合进行中的回调应该拿到「正忙」：{seen}"
    assert "正忙" in busy[0]


def test_parallel_tools_runs_the_batch_concurrently(env):
    """`parallel_tools`：默认（跟 `Config.parallel_tools`）并发，`False` 则按顺序串行。

    一批两个 `sleep 0.6` 的 shell：并发 ≈ 0.6s，串行 ≈ 1.2s。
    """
    cfg, llm, tools, server = env
    server.two_calls = True

    session = pie.Session.ephemeral(cfg, llm, tools)
    started = time.monotonic()
    session.aturn("并行跑两个")
    parallel = time.monotonic() - started

    serial_session = pie.Session.ephemeral(cfg, llm, tools)
    started = time.monotonic()
    serial_session.aturn("串行跑两个", parallel_tools=False)
    serial = time.monotonic() - started

    assert parallel < 1.0, f"并发应该重叠执行，实际 {parallel:.2}s"
    assert serial > 1.1, f"串行是两个之和，实际 {serial:.2}s"
    assert serial > parallel


def test_turn_accepts_per_turn_knobs(env):
    """`max_steps` / `stream` / `parallel_tools` 是 `aturn` 的**形参**（不是配置项）：
    `stream=False` → 不再推 `content_delta`，只推一次 `answer`。
    """
    cfg, llm, tools, _ = env
    session = pie.Session.ephemeral(cfg, llm, tools)

    events = []
    answer = session.aturn("打个招呼", on_event=events.append, stream=False)

    assert answer == "搞定了"
    kinds = [e["type"] for e in events]
    assert kinds == ["tool_call", "tool_result", "answer"], kinds
    assert events[-1]["text"] == "搞定了"


def test_stop_from_another_thread_aborts_the_turn(env):
    cfg, llm, tools, server = env
    server.delay = 3.0  # 让模型请求挂住，好从中途取消
    session = pie.Session.ephemeral(cfg, llm, tools)

    token = pie.Cancel()
    result: list = []

    def run():
        result.append(session.aturn("写一篇长文", cancel=token))

    worker = threading.Thread(target=run)
    started = time.monotonic()
    worker.start()
    time.sleep(0.4)
    assert token.cancelled is False
    token.cancel()
    worker.join(timeout=10)
    assert not worker.is_alive(), "取消之后回合没停下来"

    assert token.cancelled is True
    assert result == ["用户手动终止"]
    assert time.monotonic() - started < 3.0  # 没等模型回完
    assert session.messages[-1]["content"] == "用户手动终止"


def test_llm_error_carries_status(env):
    cfg, llm, tools, server = env
    cfg.base_url = f"http://127.0.0.1:{server.server_port}/nope/"  # 404
    broken = pie.LlmClient(cfg)
    session = pie.Session.ephemeral(cfg, broken, tools)
    with pytest.raises(pie.LlmError) as info:
        session.aturn("在吗")
    assert info.value.status == 404
    assert "404" in str(info.value)


def test_tool_registry_specs_and_tool_defaults():
    cfg = pie.Config()
    names = pie.ToolRegistry.builtins(cfg).names()
    assert names == ["read", "edit", "writ", "bash"]
    limited = pie.ToolRegistry.from_spec("read,ls", cfg)
    assert limited.names() == ["read", "bash"]  # 非内置名 → 受限 bash


def test_config_load_from_file(tmp_path, monkeypatch):
    monkeypatch.setenv("PIE_DIR", str(tmp_path / "pie"))
    config_file = tmp_path / "custom.toml"
    config_file.write_text('model = "from-file"\ncontext_window = 1234\n', encoding="utf-8")
    cfg = pie.Config.load(str(config_file))
    assert cfg.model == "from-file"
    assert cfg.context_window == 1234
    assert cfg.config_file == str(config_file)
    # 首跑写一份全局记忆种子（已存在则不动）
    memory = tmp_path / "pie" / "memory.md"
    assert memory.exists()
    memory.write_text("我的记忆", encoding="utf-8")
    pie.Config.load(str(config_file))
    assert memory.read_text(encoding="utf-8") == "我的记忆"


def test_context_budget_follows_reserved_tokens():
    cfg = pie.Config()
    cfg.context_window = 1000
    cfg.reserved_tokens = 400
    assert cfg.context_budget() == 600
    cfg.reserved_tokens = None
    assert cfg.context_budget() == 1000


# ---------------------------------------------------------------- 模块级函数 / 新 API


def test_module_level_run_uses_an_ephemeral_session(env):
    """`pie.run(task, config=…)`：无会话、不落盘，返回最终答复。"""
    cfg, llm, tools, _ = env
    assert pie.run("打个招呼", config=cfg) == "搞定了"
    sessions_dir = Path(os.environ["PIE_DIR"]) / "sessions"
    assert not sessions_dir.exists() or not any(sessions_dir.iterdir()), "不该落盘"


def test_list_sessions_returns_cli_shaped_dicts(env):
    """`pie.list_sessions()` 的键名与 CLI `sessions --json` 一致。"""
    cfg, llm, tools, _ = env
    before = pie.list_sessions()
    assert isinstance(before, list)

    saved = pie.Session.new(cfg, llm, tools, id="list-me")
    saved.aturn("记一笔")
    saved.save()

    rows = pie.list_sessions(limit=5)
    assert 1 <= len(rows) <= 5
    top = next(r for r in rows if r["id"] == "list-me")
    assert set(top) == {"id", "file", "mtime", "size", "turns", "api_calls", "first_query"}
    assert top["first_query"] == "记一笔"
    assert top["turns"] >= 1 and top["size"] > 0


def test_config_to_dict_and_update_round_trip(tmp_path, monkeypatch):
    """`to_dict()` ↔ `update()`：只覆盖给出的键；未知键报错；运行时字段不受影响。"""
    monkeypatch.setenv("PIE_DIR", str(tmp_path / "pie"))
    cfg = pie.Config()
    d = cfg.to_dict()
    assert d["model"] == cfg.model and d["context_window"] == cfg.context_window
    assert d["reserved_tokens"] == 128_000
    assert "config_file" not in d, "to_dict 只给持久字段"

    # 只改一个键：别的不动
    cfg.update({"model": "另一个", "reserved_tokens": "64k"})
    assert cfg.model == "另一个"
    assert cfg.reserved_tokens == 64_000
    assert cfg.context_window == d["context_window"], "没给的键保持原值"

    # `auto` → None；关压缩
    cfg.update({"reserved_tokens": "auto", "compaction": False})
    assert cfg.reserved_tokens is None
    assert cfg.compaction is False

    # 未知键（打错字）要报错，不能静默吞掉
    with pytest.raises(pie.PieError):
        cfg.update({"contxt_window": 1})
    # 负数在 usize 字段上被拒
    with pytest.raises(pie.PieError):
        cfg.update({"context_window": -1})


def test_session_clear_window_setters_and_transcript(env, tmp_path, monkeypatch):
    """`clear_window()` 归档；`set_model` / `set_reasoning_effort` 改配置并写回。"""
    cfg, llm, tools, _ = env
    cfg.config_file = str(tmp_path / "cfg.toml")  # 别写到用户真实的配置
    session = pie.Session.ephemeral(cfg, llm, tools)
    session.aturn("第一问")

    assert session.clear_window() == 1, "首个窗口块"
    assert len(session.messages) == 2, "system + 窗口摘要"
    # 归档不丢信息：full_history 能展开回原文
    roles = [m["role"] for m in session.full_history]
    assert roles[:3] == ["system", "user", "assistant"], roles

    note = session.set_model("deepseek-v4-pro")
    assert "已写入配置" in note, note                     # 与 Python `_persist_note` 同款
    assert session.config.model == "deepseek-v4-pro", "会话内配置跟着改"
    assert "deepseek-v4-pro" in Path(cfg.config_file).read_text(), "写回了指定的配置文件"
    assert cfg.model == "test-model", "传给 Session 的那个 cfg 是副本，不会跟着变"
    session.set_reasoning_effort("low")
    assert session.config.reasoning_effort == "low"


# ---------------------------------------------------------------- 存根对拍（防漂移）


def _stub_tree():
    import ast

    stub = Path(__file__).resolve().parents[1] / "python" / "pie" / "_pie_rs.pyi"
    return ast.parse(stub.read_text(encoding="utf-8"))


def test_stub_covers_every_exported_name():
    """`.pyi` 存根必须覆盖 `__all__` 里每个名字（否则类型检查器看到的是残缺面）。"""
    tree = _stub_tree()
    import ast

    declared = {n.name for n in tree.body if isinstance(n, (ast.ClassDef, ast.FunctionDef))}
    # `tool` / `Tool` 由纯 Python 的 `pie/_tool.py` 提供，不在扩展模块的存根里
    python_layer = {"tool", "Tool"}
    assert set(pie.__all__) - python_layer <= declared, set(pie.__all__) - python_layer - declared


def test_stub_declares_every_public_attribute():
    """真实类上的公开属性 / 方法都要在存根里声明（新加了 getter 忘了写存根就会红）。"""
    import ast
    import inspect

    tree = _stub_tree()
    classes = {n.name: n for n in tree.body if isinstance(n, ast.ClassDef)}
    for name in pie.__all__:
        cls = getattr(pie, name, None)
        if not isinstance(cls, type) or issubclass(cls, BaseException):
            continue  # 异常类只查「存根里有这个名字」（继承来的 args 之类不用写）
        if name == "Tool":
            continue  # 纯 Python 层（`pie/_tool.py`）的 dataclass，存根不管它
        node = classes[name]
        declared = set()
        for item in node.body:
            if isinstance(item, ast.AnnAssign) and isinstance(item.target, ast.Name):
                declared.add(item.target.id)
            elif isinstance(item, ast.FunctionDef):
                declared.add(item.name)
        # `__init__` 不看：有的类只能走 staticmethod 造（Session / ToolRegistry），存根里本来就不写构造器
        real = {attr for attr, _ in inspect.getmembers(cls) if not attr.startswith("_")}
        missing = real - declared
        assert not missing, f"{name} 存根缺: {sorted(missing)}"
        extra = {a for a in declared if not a.startswith("_")} - real
        assert not extra, f"{name} 存根多出来（Rust 侧已没有）: {sorted(extra)}"


def test_stub_event_shapes_match_the_real_events(env):
    """事件 dict 的键必须在存根里那几个 TypedDict 里出现过（形状漂移就红）。"""
    import ast

    tree = _stub_tree()
    typed: dict[str, set[str]] = {}
    for node in tree.body:
        if isinstance(node, ast.ClassDef) and any(
            (isinstance(b, ast.Name) and b.id == "TypedDict") for b in node.bases
        ):
            keys = set()
            for item in node.body:
                if isinstance(item, ast.AnnAssign) and isinstance(item.target, ast.Name):
                    keys.add(item.target.id)
            typed[node.name] = keys
    by_type_literal = {
        "content_delta": "ContentDelta",
        "reasoning_delta": "ReasoningDelta",
        "tool_call": "ToolCallEvent",
        "tool_result": "ToolResultEvent",
        "answer": "AnswerEvent",
    }

    cfg, llm, tools, _ = env
    events: list[dict] = []
    pie.Session.ephemeral(cfg, llm, tools).aturn("打个招呼", on_event=events.append)
    assert events
    for ev in events:
        keys = typed[by_type_literal[ev["type"]]]
        assert set(ev) <= keys, f"{ev['type']} 存根缺键: {sorted(set(ev) - keys)}"



# ---------------------------------------------------------------- M3：Python 工具


def test_tool_schema_matches_the_pure_python_oracle():
    """`@pie.tool` 的 schema 与纯 Python 版 `@tool` **逐字相同**（值是用 oracle 跑出来对过的）。

    ⚠ 测试文件本身有 `from __future__ import annotations`，所以注解是字符串；嵌套函数里的注解
    得能从**模块全局/内建**解析出来（`list[str]` / `int | None` 行；函数内 import 的名字不行）——
    这与纯 Python 版同一条限制。
    """

    def fetch(url: str, retries: int = 3, timeout: float = 1.5) -> str:
        """抓一个 URL 的正文"""
        return url

    t = pie.tool()(fetch)
    assert t.definition() == {
        "type": "function",
        "function": {
            "name": "fetch",
            "description": "抓一个 URL 的正文",
            "parameters": {
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "retries": {"type": "integer", "default": 3},
                    "timeout": {"type": "number", "default": 1.5},
                },
                "required": ["url"],
            },
        },
    }

    def maybe(pattern: str, limit: int | None = None) -> str:
        """按正则找"""
        return pattern

    params = pie.tool()(maybe).parameters
    assert params["required"] == ["pattern"], "带默认值的不是必填"
    assert params["properties"]["limit"] == {"type": "integer"}, "`X | None` → X"

    def bad(x: complex) -> str:
        return ""

    with pytest.raises(ValueError, match="不支持的参数类型注解"):
        pie.tool()(bad)
    with pytest.raises((TypeError, ValueError)):
        pie.tool()(None)  # type: ignore[arg-type]  # 不是函数

    # `parameters=` 按名字覆盖自动生成的结果（例：补 enum）
    def pick(mode: str) -> str:
        return mode

    params = pie.tool(
        name="pick", description="选一个", parameters={"mode": {"type": "string", "enum": ["a", "b"]}}
    )(pick).parameters
    assert params["properties"]["mode"] == {"type": "string", "enum": ["a", "b"]}

    # 下划线开头的参数是注入项，不进 schema
    def hidden(x: str, _secret: int = 0) -> str:
        return x

    assert list(pie.tool()(hidden).parameters["properties"]) == ["x"]


def test_register_python_tool_end_to_end(env):
    """注册的 Python 工具能被模型调起来：参数按名字给、返回值当结果文本回传。"""
    cfg, llm, tools, server = env
    seen: list[tuple] = []

    @pie.tool(name="fetch", description="抓一个 URL 的正文")
    def fetch(url: str, retries: int = 0) -> str:
        seen.append((url, retries))
        return f"正文来自 {url}"

    tools.register(fetch)
    assert "fetch" in tools.names()
    assert any(t["function"]["name"] == "fetch" for t in tools.specs())

    server.tool_name = "fetch"
    server.tool_arguments = '{"url": "https://example.com", "retries": 2}'
    events: list[dict] = []
    answer = pie.Session.ephemeral(cfg, llm, tools).aturn("抓一下", on_event=events.append)

    assert answer == "搞定了"
    assert seen == [("https://example.com", 2)], "handler 按关键字拿到解析后的参数"
    result = next(e for e in events if e["type"] == "tool_result")
    assert result["name"] == "fetch"
    assert "正文来自 https://example.com" in result["text"], result


def test_python_tool_exception_is_textualized(env):
    """工具抛异常 → 文本化回给模型（`[工具错误] …`），不打断回合、继续跑完。"""
    cfg, llm, tools, server = env

    def boom(x: str) -> str:
        """炸一个"""
        raise ValueError("炸了")

    tools.register(name="boom", description="炸一个", handler=boom)
    server.tool_name = "boom"
    server.tool_arguments = '{"x": "1"}'
    events: list[dict] = []
    answer = pie.Session.ephemeral(cfg, llm, tools).aturn("炸一下", on_event=events.append)

    assert answer == "搞定了", "回合照常跑完"
    result = next(e for e in events if e["type"] == "tool_result")
    assert "[工具错误]" in result["text"] and "炸了" in result["text"], result


def test_register_validates_name_duplicate_and_async(env):
    """注册期的三类错都在注册时拒掉：名字不合法 / 重名 / async handler。"""
    cfg, llm, tools, _ = env
    with pytest.raises(ValueError, match="非法的工具名"):
        tools.register(name="bad name!", handler=lambda: "x")
    tools.register(name="ok", description="随便", handler=lambda: "x")
    with pytest.raises(ValueError, match="工具已存在"):
        tools.register(name="ok", handler=lambda: "y")

    async def a(x: str) -> str:
        return x

    with pytest.raises(ValueError, match="async"):
        tools.register(name="a", handler=a)
    with pytest.raises(ValueError, match="async"):
        pie.tool()(a)   # 装饰期就拦


# ---------------------------------------------------------------- M5：asyncio 入口


def test_events_before_any_aturn_async_is_an_error(env):
    """事件流是**按回合**的：没跑 `aturn_async` 就没有队列，`events()` 要报错而不是静默空转。"""
    cfg, llm, tools, _ = env
    session = pie.Session.ephemeral(cfg, llm, tools)
    with pytest.raises(RuntimeError, match="aturn_async"):
        session.events()


def test_aturn_async_streams_events_and_returns_answer(env):
    """`await session.aturn_async(...)` + `async for ev in session.events()`（事件按序、含结束）。"""
    import asyncio

    cfg, llm, tools, _ = env
    session = pie.Session.ephemeral(cfg, llm, tools)

    async def run() -> tuple[str, list[str]]:
        task = asyncio.create_task(session.aturn_async("打个招呼"))
        # `aturn_async` 里就建好队列了 → 不用先 await 一下让协程跑起来
        kinds = [ev["type"] async for ev in session.events()]
        return await task, kinds

    answer, kinds = asyncio.run(run())
    assert answer == "搞定了"
    assert kinds == ["tool_call", "tool_result", "content_delta", "content_delta"], kinds


def test_aturn_async_cancel_really_stops_the_turn(env):
    """`task.cancel()` → 真的停住（模型请求还挂着也要立刻返回），之后会话仍可用。

    这是 M5 三个坑里最要命的那个：tokio 任务不会因为 Python 侧取消而自己停，
    所以 `pie/_async.py` 捕 `CancelledError` 后要调 `session.stop()`。
    """
    import asyncio
    import time as _time

    cfg, llm, tools, server = env
    server.delay = 3.0  # 让模型请求挂住，好从中途取消
    session = pie.Session.ephemeral(cfg, llm, tools)

    async def run() -> float:
        task = asyncio.create_task(session.aturn_async("写一篇长文"))
        await asyncio.sleep(0.3)
        started = _time.monotonic()
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        return _time.monotonic() - started

    elapsed = asyncio.run(run())
    assert elapsed < 2.0, f"取消要立刻返回（模型请求挂着 3s），实际 {elapsed:.2f}s"

    # 锁已释放、历史合法 → 还能接着聊（取消没把会话弄坏）
    server.delay = 0.0
    for _ in range(20):  # 等 Rust 侧收尾（正常几毫秒）
        try:
            assert session.aturn("再来一次") == "搞定了"
            break
        except RuntimeError:
            import time as __time

            __time.sleep(0.05)
    else:
        pytest.fail("取消之后会话一直锁着")
