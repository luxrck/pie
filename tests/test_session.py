"""会话持久化回归测试：Session.path / Session.windows / Session.files（无 pytest 依赖）。

跑法（任一）：

    uv run python tests/test_session.py
    pytest tests/test_session.py        # 装了 pytest 也能直接跑

覆盖 2026-09 的改名（三个名字容易混，改成与语义一致）：

- `Session.file` → **`Session.path`**（会话 JSONL 文件本身的路径）；
- `Session.fs`   → **`Session.windows`**（`/clear` 归档的历史窗口块，落在 `~/.pie/windows/`）；
- `Session.files`（新增）不改名：图片 id 表（hash_id → file_id / 本地副本 / 过期时间），随 `__meta__` 落盘。

`__meta__` 里的键同步改成 `windows`，但**旧会话文件的 `fs` 键仍能读回**（不迁移、不改写用户文件）。
"""

from __future__ import annotations

import json
import pathlib
import sys
import tempfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "src"))

from pie.config import Config  # noqa: E402
from pie.session import Session  # noqa: E402
from pie.tools import ToolRegistry  # noqa: E402


def _new_session() -> Session:
    cfg = Config()
    cfg.verbose = False
    return Session.new(config=cfg, llm=object(), tools=ToolRegistry())


def _read_meta(path: pathlib.Path) -> dict:
    with path.open(encoding="utf-8") as f:
        for line in f:
            if line.strip():
                return json.loads(line)
    raise AssertionError("会话文件为空")


def test_save_writes_windows_key_and_sets_path() -> None:
    """save() 把窗口块写进 __meta__.windows，并把自身文件路径记到 .path。"""
    with tempfile.TemporaryDirectory() as d:
        target = pathlib.Path(d) / "chat-x.jsonl"
        session = _new_session()
        block = pathlib.Path(d) / "window-1.jsonl"
        block.write_text("", encoding="utf-8")
        session.windows = [block]
        session.save(target)

        meta = _read_meta(target)
        assert meta["windows"] == [str(block)]
        assert "fs" not in meta  # 新键不再写旧名
        assert session.path == target
        assert not hasattr(session, "file")


def test_load_restores_path_and_windows() -> None:
    """load() 还原 .path 与 .windows。"""
    with tempfile.TemporaryDirectory() as d:
        target = pathlib.Path(d) / "chat-y.jsonl"
        session = _new_session()
        block = pathlib.Path(d) / "window-2.jsonl"
        block.write_text("", encoding="utf-8")
        session.windows = [block]
        session.save(target)

        loaded = Session.load(target, config=session.config, llm=object(), tools=ToolRegistry())
        assert loaded.path == target
        assert loaded.windows == [block]


def test_load_reads_legacy_fs_key() -> None:
    """旧会话文件（__meta__ 用 fs 键）也能读回 → .windows，不需要迁移文件。"""
    with tempfile.TemporaryDirectory() as d:
        target = pathlib.Path(d) / "chat-legacy.jsonl"
        block = pathlib.Path(d) / "window-old.jsonl"
        block.write_text("", encoding="utf-8")
        target.write_text(
            json.dumps({"__meta__": True, "usage": {}, "fs": [str(block)], "title": "旧会话"},
                       ensure_ascii=False)
            + "\n"
            + json.dumps({"role": "user", "content": "你好", "cls": "UserMessage"}, ensure_ascii=False)
            + "\n",
            encoding="utf-8",
        )
        loaded = Session.load(target, config=Session.new(
            config=Config(), llm=object(), tools=ToolRegistry()
        ).config, llm=object(), tools=ToolRegistry())
        assert loaded.windows == [block]
        assert loaded.path == target


def test_save_persists_files_and_load_restores() -> None:
    """图片 id 表（`Session.files`）随会话一起落盘/恢复；空表不写进 __meta__。"""
    entry = {
        "hash_id": "img-85fbd97740797558",
        "sha256": "85fbd97740797558",
        "size": 3290,
        "mime": "image/png",
        "filename": "half.png",
        "src": "/tmp/half.png",
        "local": "/home/cc/.pie/files/img-85fbd97740797558.png",
        "file_id": "file-api-x",
        "base_url": "https://api.deepseek.com",
        "key_fp": "31d7a4c2",
        "uploaded_at": "2026-09-14T18:00:00",
        "expires_at": 1791972581,
    }
    with tempfile.TemporaryDirectory() as d:
        target = pathlib.Path(d) / "chat-f.jsonl"
        session = _new_session()
        assert "files" not in _read_meta(session.save(target))  # 空表不写
        session.files = {entry["hash_id"]: entry}
        session.save(target)
        assert _read_meta(target)["files"][entry["hash_id"]]["file_id"] == "file-api-x"

        loaded = Session.load(target, config=session.config, llm=object(), tools=ToolRegistry())
        assert loaded.files == {entry["hash_id"]: entry}


def test_usage_report_shows_session_path() -> None:
    """/stat 的「会话文件」行读 .path。"""
    with tempfile.TemporaryDirectory() as d:
        target = pathlib.Path(d) / "chat-z.jsonl"
        session = _new_session()
        session.save(target)
        report = session.usage_report()
        assert f"会话文件：{target}" in report
        assert f"/ {session.config.context_budget():,} tokens" in report


def _main() -> int:
    tests = [(n, f) for n, f in sorted(globals().items()) if n.startswith("test_") and callable(f)]
    failed = []
    for name, func in tests:
        try:
            func()
        except AssertionError as exc:
            failed.append((name, exc))
            print(f"[FAIL] {name}: {exc}")
        else:
            print(f"[PASS] {name}")
    print(f"\n{len(tests) - len(failed)}/{len(tests)} 通过")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(_main())

