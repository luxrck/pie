"""pie_rs 绑定的回归测试（不联网）。

假 LLM 端点：本地 `http.server` 回放固定的 OpenAI 风格 SSE —— 第一次请求让模型「调一个工具」，
第二次请求给最终答复。这样整条主链（流式解析 → 工具执行 → 事件回传 → 落库）都真的跑了一遍，
但一个字节都不出网。

跑法：`cd pie-rs/bindings/pie-py && .venv/bin/python -m pytest tests -q`
"""

from __future__ import annotations

import json
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

import pie_rs

# ---------------------------------------------------------------- 假端点


def _sse(*chunks: dict) -> bytes:
    body = "".join(f"data: {json.dumps(c, ensure_ascii=False)}\n\n" for c in chunks)
    return (body + "data: [DONE]\n\n").encode()


def _tool_call_chunk(call_id: str, name: str, arguments: str) -> dict:
    return {
        "choices": [
            {
                "delta": {
                    "tool_calls": [
                        {
                            "index": 0,
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
            chunks = (
                _tool_call_chunk("call_1", "shell", '{"command": "echo hi"}'),
                {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]},
                {"usage": {"prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14}},
            )

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
    cfg = pie_rs.Config()
    cfg.base_url = f"http://127.0.0.1:{fake_llm.server_port}/v1/"
    cfg.api_key = "test-key"
    cfg.model = "test-model"
    llm = pie_rs.LlmClient(cfg)
    tools = pie_rs.ToolRegistry.builtins(cfg)
    return cfg, llm, tools, fake_llm


# ---------------------------------------------------------------- 用例


def test_config_defaults_and_repr_masks_key():
    cfg = pie_rs.Config()
    assert cfg.model and cfg.base_url and cfg.context_window > 0
    assert cfg.stream is True
    # ⚠ 核心 `Config::default()` 里压缩是**开着**的（`Some(CompactionConfig::default())`）：
    # 与 Python 版「不写 `[compaction]` 就不压」不同，这里是「不写就按默认三级压」。
    assert cfg.compaction is True
    cfg.compaction = False
    assert cfg.compaction is False
    # api_key 不许整串漏进 repr
    assert cfg.api_key not in repr(cfg)
    assert "…" in repr(cfg)


def test_exception_hierarchy():
    assert issubclass(pie_rs.ConfigError, pie_rs.PieError)
    assert issubclass(pie_rs.LlmError, pie_rs.PieError)
    assert issubclass(pie_rs.ToolError, pie_rs.PieError)
    assert issubclass(pie_rs.PieError, Exception)


def test_turn_runs_tool_and_streams_events(env):
    cfg, llm, tools, server = env
    session = pie_rs.Session.ephemeral(cfg, llm, tools)

    events = []
    answer = session.aturn("打个招呼", on_event=events.append)

    assert answer == "搞定了"
    kinds = [e["type"] for e in events]
    assert kinds == ["tool_call", "tool_result", "content_delta", "content_delta"]

    call = events[0]
    assert call["name"] == "shell"
    assert call["arguments"] == {"command": "echo hi"}  # 参数已解析成 dict
    assert call["arguments_raw"] == '{"command": "echo hi"}'
    assert (call["turn"], call["step"]) == (1, 1)

    result = events[1]
    assert result["name"] == "shell"
    assert result["text"].startswith("[exit=0]")  # 头区在，说明真的执行了 shell
    assert "hi" in result["text"]

    assert [e["text"] for e in events[2:]] == ["搞定", "了"]

    # 历史：user → assistant(tool_calls) → tool → assistant
    roles = [m["role"] for m in session.messages]
    assert roles == ["system", "user", "assistant", "tool", "assistant"]
    assert session.messages[3]["content"].startswith("[exit=0]")
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
    session = pie_rs.Session.ephemeral(cfg, llm, tools)
    session.aturn("打个招呼")
    sessions_dir = tmp_path / "pie" / "sessions"
    assert not sessions_dir.exists() or not any(sessions_dir.iterdir())

    # 落盘会话则真的写文件，且能被 load 回来
    saved = pie_rs.Session.new(cfg, llm, tools, id="bindings-test")
    saved.aturn("打个招呼")
    saved.save()
    assert Path(saved.path).exists()

    reopened = pie_rs.Session.load(saved.path, cfg, llm, tools)
    assert reopened.turn_count == 1
    assert reopened.messages[-1]["content"] == "搞定了"


def test_callback_may_not_reenter_the_session(env):
    cfg, llm, tools, _ = env
    session = pie_rs.Session.ephemeral(cfg, llm, tools)
    seen = []

    def on_event(event):
        seen.append(event["type"])
        with pytest.raises(RuntimeError, match="正忙"):
            session.usage_report()

    session.aturn("打个招呼", on_event=on_event)
    assert seen  # 回调确实跑过（而且里面的断言没炸出来）


def test_stop_from_another_thread_aborts_the_turn(env):
    cfg, llm, tools, server = env
    server.delay = 3.0  # 让模型请求挂住，好从中途取消
    session = pie_rs.Session.ephemeral(cfg, llm, tools)

    token = pie_rs.Cancel()
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
    broken = pie_rs.LlmClient(cfg)
    session = pie_rs.Session.ephemeral(cfg, broken, tools)
    with pytest.raises(pie_rs.LlmError) as info:
        session.aturn("在吗")
    assert info.value.status == 404
    assert "404" in str(info.value)


def test_tool_registry_specs_and_tool_defaults():
    cfg = pie_rs.Config()
    names = pie_rs.ToolRegistry.builtins(cfg).names()
    assert names == ["read", "edit", "write", "shell"]
    limited = pie_rs.ToolRegistry.from_spec("read,ls", cfg)
    assert limited.names() == ["read", "shell"]  # 非内置名 → 受限 shell


def test_config_load_from_file(tmp_path, monkeypatch):
    monkeypatch.setenv("PIE_DIR", str(tmp_path / "pie"))
    config_file = tmp_path / "custom.toml"
    config_file.write_text('model = "from-file"\ncontext_window = 1234\n', encoding="utf-8")
    cfg = pie_rs.Config.load(str(config_file))
    assert cfg.model == "from-file"
    assert cfg.context_window == 1234
    assert cfg.config_file == str(config_file)
    # 首跑写一份全局记忆种子（已存在则不动）
    memory = tmp_path / "pie" / "memory.md"
    assert memory.exists()
    memory.write_text("我的记忆", encoding="utf-8")
    pie_rs.Config.load(str(config_file))
    assert memory.read_text(encoding="utf-8") == "我的记忆"


def test_context_budget_follows_reserved_tokens():
    cfg = pie_rs.Config()
    cfg.context_window = 1000
    cfg.reserved_tokens = 400
    assert cfg.context_budget() == 600
    cfg.reserved_tokens = None
    assert cfg.context_budget() == 1000
