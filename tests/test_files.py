"""图片上传（Files API）回归测试：本地副本 / 上传与复用 / 回退 / 回收（无 pytest 依赖）。

跑法（任一）：

    uv run python tests/test_files.py
    pytest tests/test_files.py        # 装了 pytest 也能直接跑

覆盖（对应 `src/pie/files.py` + `loop` 侧的图片注入）：

- 内容寻址：`hash_id = img-<sha256[:16]>`，同内容同 id；
- 本地副本落在 `~/.pie/files/<hash_id><ext>`，重复落盘幂等（测试里把 FILES_DIR 指到临时目录）；
- **上传一次、按 sha + key 指纹 + 有效期复用**；换 key / 过期 / 主动失效 → 重传；
- 上传失败 → 返回 None（调用方回退内联 base64），不写记录；
- 功能关闭（`enabled=False` / 模型不支持）→ 连本地副本都不落；
- `loop._build_image_parts`：有 file_id 出 `file` 块，否则出 `image_url` 内联；
- `loop._downgrade_file_blocks`：file_id 失效时把历史里的 `file` 块降级成内联；
- 本地副本回收：只删「没有被任何会话 `__meta__.files` 引用」的副本；
- `purge_remote_files`（`pie files gc --all`）：服务端全部上传件逐个删除、单个失败不中断；
  CLI 一侧的接线（stub 掉客户端与 purge，不碰网络）。
"""

from __future__ import annotations

import asyncio
import contextlib
import io
import json
import os
import pathlib
import re
import sys
import tempfile
import time
import types

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "src"))

from pie import cli as cli_mod  # noqa: E402
from pie import files as files_mod  # noqa: E402
from pie.context import AgentMessage, ImageMessage  # noqa: E402
from pie.files import (  # noqa: E402
    ImageStore,
    blob_path,
    collect_file_garbage,
    entry_is_usable,
    hash_id,
    is_stale_file_error,
    key_fingerprint,
    list_remote_files,
    model_supports_files,
    purge_remote_files,
    store_blob,
)
from pie.loop import _build_image_parts, _downgrade_file_blocks  # noqa: E402
from pie.tools import ImageRef  # noqa: E402

PNG = bytes.fromhex(
    "89504e470d0a1a0a0000000d4948445200000002000000020806000000f478d4fa"
    "0000001649444154789c6360f8cf801b18a9c1e40c0c000ea001fd3a1e1e1e0000000049454e44ae426082"
)
PNG2 = PNG[:-20] + b"\x00" * 8 + PNG[-12:]  # 改几个字节 → 不同内容


class _FakeFiles:
    """假的 `client.files`：记录每次上传，可让第 N 次失败。"""

    def __init__(self, fail: bool = False) -> None:
        self.uploads: list[dict] = []
        self.fail = fail
        self.counter = 0

    async def create(self, *, file, purpose, extra_body=None):
        self.counter += 1
        if self.fail:
            raise RuntimeError("boom")
        self.uploads.append(
            {"name": pathlib.Path(getattr(file, "name", "?")).name, "purpose": purpose,
             "extra_body": extra_body}
        )
        expires = None
        if extra_body:
            expires = 4102444800  # 2100 年 → 测试里当“未过期”
        return types.SimpleNamespace(id=f"file-api-{self.counter:04d}", expires_at=expires)


class _FakeClient:
    def __init__(self, fail: bool = False) -> None:
        self.files = _FakeFiles(fail=fail)


class _TempBlobs:
    """把 files.FILES_DIR 临时指到临时目录（别污染真实的 ~/.pie/files）。"""

    def __enter__(self) -> pathlib.Path:
        self._tmp = tempfile.TemporaryDirectory()
        self._old = files_mod.FILES_DIR
        files_mod.FILES_DIR = pathlib.Path(self._tmp.name)
        return files_mod.FILES_DIR

    def __exit__(self, *exc) -> None:
        files_mod.FILES_DIR = self._old
        self._tmp.cleanup()


def _store(**kwargs) -> ImageStore:
    base = dict(base_url="https://api.deepseek.com", key_fp=key_fingerprint("sk-test"), ttl_days=30)
    base.update(kwargs)
    return ImageStore(**base)


async def _ensure(store: ImageStore, client, data: bytes = PNG, mime: str = "image/png"):
    return await store.ensure(
        client, data=data, mime=mime, filename="shot.png", src="/tmp/shot.png"
    )


# ---------------------------------------------------------------- 基础


def test_hash_id_shape_and_content_addressing() -> None:
    hid = hash_id(PNG)
    assert hid.startswith("img-") and len(hid) == 4 + 16
    assert hid == hash_id(PNG)
    assert hid != hash_id(PNG2)


def test_model_supports_files() -> None:
    assert model_supports_files("deepseek-flash")
    assert model_supports_files("deepseek-v4-flash-vision-exp")
    assert not model_supports_files("deepseek-v4-pro")
    assert not model_supports_files(None)


def test_store_blob_is_idempotent() -> None:
    with _TempBlobs() as blobs_dir:
        hid, path = store_blob(PNG, mime="image/png")
        assert path == blobs_dir / f"{hid}.png"
        assert path.read_bytes() == PNG
        again = store_blob(PNG, mime="image/png")[1]
        assert again == path
        assert len(list(blobs_dir.iterdir())) == 1  # 没有临时文件残留


# ---------------------------------------------------------------- 上传 / 复用 / 回退


def test_ensure_uploads_then_reuses() -> None:
    with _TempBlobs():
        store, client = _store(), _FakeClient()
        first = asyncio.run(_ensure(store, client))
        assert first == "file-api-0001"
        assert len(client.files.uploads) == 1
        upload = client.files.uploads[0]
        assert upload["purpose"] == "user_data"
        assert upload["extra_body"] == {"expires_after": {"anchor": "created_at", "seconds": 30 * 86400}}
        entry = next(iter(store.entries.values()))
        assert entry["hash_id"].startswith("img-") and entry["src"] == "/tmp/shot.png"
        assert entry["filename"] == "shot.png" and entry["mime"] == "image/png"
        assert entry["size"] == len(PNG) and entry["local"].endswith(f"{entry['hash_id']}.png")

        second = asyncio.run(_ensure(store, client))
        assert second == first
        assert len(client.files.uploads) == 1  # 同内容不再重传


def test_ensure_reuploads_when_key_or_endpoint_changes() -> None:
    with _TempBlobs():
        store, client = _store(), _FakeClient()
        asyncio.run(_ensure(store, client))
        store.key_fp = key_fingerprint("sk-other")
        assert asyncio.run(_ensure(store, client)) == "file-api-0002"
        store.key_fp = key_fingerprint("sk-test")
        store.base_url = "https://other.example/v1"
        assert asyncio.run(_ensure(store, client)) == "file-api-0003"
        assert len(client.files.uploads) == 3


def test_ensure_reuploads_when_expired_or_invalidated() -> None:
    with _TempBlobs():
        store, client = _store(), _FakeClient()
        file_id = asyncio.run(_ensure(store, client))
        entry = next(iter(store.entries.values()))

        entry["expires_at"] = 1  # 已过期
        assert not entry_is_usable(entry, base_url=store.base_url, key_fp=store.key_fp)
        assert asyncio.run(_ensure(store, client)) == "file-api-0002"

        assert store.invalidate("file-api-0002") is True
        assert asyncio.run(_ensure(store, client)) == "file-api-0003"
        assert store.invalidate("不存在") is False
        assert file_id == "file-api-0001"


def test_ensure_returns_none_on_upload_failure() -> None:
    with _TempBlobs():
        store, client = _store(), _FakeClient(fail=True)
        assert asyncio.run(_ensure(store, client)) is None
        assert store.entries == {}  # 失败不留半条记录
        assert blob_path(hash_id(PNG), "image/png").exists()  # 但本地副本已落（重传用得上）


def test_ensure_disabled_skips_blob_and_upload() -> None:
    with _TempBlobs() as blobs_dir:
        store, client = _store(enabled=False), _FakeClient()
        assert asyncio.run(_ensure(store, client)) is None
        assert client.files.uploads == []
        assert list(blobs_dir.iterdir()) == []  # 功能关闭 → 连副本都不落


def test_is_stale_file_error() -> None:
    class _Err(Exception):
        status_code = 400

    stale = _Err("...the following file_ids do not exist or are not created under your account: file-api-x")
    assert is_stale_file_error(stale)
    assert not is_stale_file_error(_Err("maximum context length is 1048576 tokens"))

    class _Other(Exception):
        status_code = 500

    assert not is_stale_file_error(_Other("file_ids do not exist"))


# ---------------------------------------------------------------- loop 侧：parts 与降级


def test_build_parts_prefers_file_id() -> None:
    with tempfile.TemporaryDirectory() as d:
        img = pathlib.Path(d) / "shot.png"
        img.write_bytes(PNG)
        ref = ImageRef(path=str(img), mime="image/png", size=len(PNG), width=2, height=2)

        class _Store:
            async def ensure(self, client, **kwargs):
                return "file-api-9999"

        parts = asyncio.run(_build_image_parts(ref, store=_Store(), client=object()))
        assert [p["type"] for p in parts] == ["text", "file"]
        assert parts[1]["file_id"] == "file-api-9999"

        class _NoUpload:
            async def ensure(self, client, **kwargs):
                return None

        parts = asyncio.run(_build_image_parts(ref, store=_NoUpload(), client=object()))
        assert [p["type"] for p in parts] == ["text", "image_url"]
        assert parts[1]["image_url"]["url"].startswith("data:image/png;base64,")

        parts = asyncio.run(_build_image_parts(ref))  # 没有 store → 老行为（内联）
        assert [p["type"] for p in parts] == ["text", "image_url"]


def test_downgrade_file_blocks_to_inline() -> None:
    with _TempBlobs() as blobs_dir:
        hid, local = store_blob(PNG, mime="image/png")
        store = _store()
        store.entries[hid] = {
            "hash_id": hid, "file_id": "file-api-x", "local": str(local), "mime": "image/png",
            "base_url": store.base_url, "key_fp": store.key_fp, "expires_at": 4102444800,
        }
        messages = AgentMessage(
            [ImageMessage(content=[{"type": "text", "text": "[图片]"}, {"type": "file", "file_id": "file-api-x"}])]
        )
        assert _downgrade_file_blocks(messages, store) is True
        parts = messages.messages[0].content
        assert parts[1]["type"] == "image_url"
        assert parts[1]["image_url"]["url"].startswith("data:image/png;base64,")
        assert store.entries[hid]["expires_at"] == 0  # 标失效 → 下次重传
        assert _downgrade_file_blocks(messages, store) is False  # 已无 file 块

        # 副本也丢了 → 该 part 退化成文本（总比让请求 400 强）
        store.entries[hid]["local"] = str(blobs_dir / "missing.png")
        messages2 = AgentMessage([ImageMessage(content=[{"type": "file", "file_id": "file-api-x"}])])
        assert _downgrade_file_blocks(messages2, store) is True
        assert messages2.messages[0].content[0] == {"type": "text", "text": "[图片已失效]"}


# ---------------------------------------------------------------- 本地副本回收


def test_collect_file_garbage_keeps_referenced_blobs() -> None:
    """被会话引用的副本无论多旧都不动；没被引用且过了保护窗口的才回收。"""
    with _TempBlobs() as blobs_dir, tempfile.TemporaryDirectory() as sessions_dir:
        kept = store_blob(PNG, mime="image/png")[1]
        orphan = store_blob(PNG2, mime="image/png")[1]
        stale = time.time() - (files_mod.GC_PROTECT_HOURS + 1) * 3600
        os.utime(kept, (stale, stale))  # 两份都算 «旧»，只差在有没有被引用
        os.utime(orphan, (stale, stale))
        session = pathlib.Path(sessions_dir) / "chat-x.jsonl"
        session.write_text(
            json.dumps(
                {"__meta__": True, "files": {"img-x": {"local": str(kept)}}}, ensure_ascii=False
            )
            + "\n",
            encoding="utf-8",
        )
        garbage = collect_file_garbage(pathlib.Path(sessions_dir))
        assert garbage == [p for p in blobs_dir.iterdir() if p != kept]
        assert kept not in garbage


def test_collect_file_garbage_protects_recent_files() -> None:
    """保护窗口：未被引用但**很新**的副本先留着（刚粘贴进 files/、还没被 read 的图），旧的才回收。"""
    with _TempBlobs() as blobs_dir, tempfile.TemporaryDirectory() as sessions_dir:
        fresh = store_blob(PNG, mime="image/png")[1]
        old = store_blob(PNG2, mime="image/png")[1]
        stale = time.time() - (files_mod.GC_PROTECT_HOURS + 1) * 3600
        os.utime(old, (stale, stale))
        sessions = pathlib.Path(sessions_dir)
        assert collect_file_garbage(sessions) == [old], "保护窗口没拦住新副本"
        assert collect_file_garbage(sessions, protect_hours=0) == sorted([fresh, old])


# ---------------------------------------------------------------- 服务端清空（gc --all）


class _FakeFilesAPI:
    """假的 `client.files`：`list()` 返回异步可迭代（对齐 SDK 的 AsyncPaginator）。"""

    def __init__(self, items: list[dict], fail: set[str] | None = None) -> None:
        self.items = items
        self.fail = set(fail or ())
        self.deleted: list[str] = []

    def list(self):
        async def _gen():
            for item in self.items:
                yield types.SimpleNamespace(**item)

        return _gen()

    async def delete(self, file_id: str):
        if file_id in self.fail:
            raise RuntimeError("delete failed")
        self.deleted.append(file_id)
        return types.SimpleNamespace(id=file_id, deleted=True)


class _FakePurgeClient:
    def __init__(self, items: list[dict], fail: set[str] | None = None) -> None:
        self.files = _FakeFilesAPI(items, fail)


def test_purge_remote_files_deletes_everything() -> None:
    client = _FakePurgeClient(
        [
            {"id": "file-a", "filename": "a.png", "bytes": 10, "created_at": 1700000000},
            {"id": "file-b", "filename": "b.png", "bytes": 20, "created_at": 1700000001},
        ]
    )
    deleted, failed = asyncio.run(purge_remote_files(client))
    assert [d["id"] for d in deleted] == ["file-a", "file-b"]
    assert client.files.deleted == ["file-a", "file-b"]
    assert failed == {}
    assert deleted[0]["filename"] == "a.png" and deleted[0]["bytes"] == 10


def test_purge_remote_files_keeps_going_after_failure() -> None:
    client = _FakePurgeClient([{"id": "file-a"}, {"id": "file-b"}], fail={"file-a"})
    deleted, failed = asyncio.run(purge_remote_files(client))
    assert [d["id"] for d in deleted] == ["file-b"]  # 一条失败不拖垮整轮
    assert client.files.deleted == ["file-b"]
    assert list(failed) == ["file-a"] and "delete failed" in failed["file-a"]


class _StubLLM:
    """替身：记录构造参数，`files_client()` 给一个只记 `close()` 的空客户端。"""

    def __init__(self, **kwargs) -> None:
        _STUB_CALLS["llm"] = kwargs

    def files_client(self):
        class _Client:
            async def close(self) -> None:
                _STUB_CALLS["closed"] = True

        return _Client()


_STUB_CALLS: dict[str, object] = {}

_CFG = 'api_key = "sk-test"\nbase_url = "https://api.deepseek.com"\n'


def _run_cli_files(
    argv: list[str],
    cfg_text: str,
    *,
    purge=None,
    remote_list=None,
    sessions=None,
) -> tuple[int, str, str]:
    """在临时配置上跑 `files_main([*argv, "-c", cfg])`，stub 掉 LLM 与两个 Files API 调用。

    ⚠️ 必须把 `cli.OpenAILLM` / `cli.purge_remote_files` / `cli.list_remote_files` 都换掉：
    `Config()` 的 api_key 默认值是本部署的真实 key（`config.DEFAULT_API_KEY`），
    配置里没有这一行 ≠ 空 key —— 漏了这点，测试会**真的**去调线上 Files API（删文件）。
    踩过一次，别再踩。
    """
    _STUB_CALLS.clear()
    with tempfile.TemporaryDirectory() as d:
        cfg = pathlib.Path(d) / "config.toml"
        cfg.write_text(cfg_text, encoding="utf-8")
        saved = (
            cli_mod.OpenAILLM,
            cli_mod.purge_remote_files,
            cli_mod.list_remote_files,
            cli_mod.iter_session_files,
        )
        cli_mod.OpenAILLM = _StubLLM
        if purge is not None:
            cli_mod.purge_remote_files = purge
        if remote_list is not None:
            cli_mod.list_remote_files = remote_list
        if sessions is not None:
            rows = list(sessions)
            cli_mod.iter_session_files = lambda *a, **k: iter(rows)
        try:
            out, err = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                rc = cli_mod.files_main([*argv, "-c", str(cfg)])
        finally:
            (
                cli_mod.OpenAILLM,
                cli_mod.purge_remote_files,
                cli_mod.list_remote_files,
                cli_mod.iter_session_files,
            ) = saved
    return rc, out.getvalue(), err.getvalue()


# ---------------------------------------------------------------- list --all


def test_list_remote_files_normalizes_fields() -> None:
    """FileObject → 普通 dict；字段缺失给 None（展示层不用 getattr 猜）。"""
    full = _FakePurgeClient(
        [{"id": "file-a", "filename": "a.png", "bytes": 10, "created_at": 1700000000,
          "expires_at": 4102444800}]
    )
    assert asyncio.run(list_remote_files(full)) == [
        {"id": "file-a", "filename": "a.png", "bytes": 10, "created_at": 1700000000,
         "expires_at": 4102444800}
    ]
    bare = _FakePurgeClient([{"id": "file-b"}])
    assert asyncio.run(list_remote_files(bare)) == [
        {"id": "file-b", "filename": None, "bytes": None, "created_at": None, "expires_at": None}
    ]


def test_fmt_ts_tolerates_junk() -> None:
    """时间戳格式化：空值/0/坏值都给 default（服务端字段可能是 null 或缺）。"""
    assert cli_mod._fmt_ts(None) == "?"
    assert cli_mod._fmt_ts(0) == "?"
    assert cli_mod._fmt_ts("bogus") == "?"
    assert cli_mod._fmt_ts(0, default="永久") == "永久"
    assert re.fullmatch(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}", cli_mod._fmt_ts(1700000000))


def test_files_list_all_prints_cloud_rows() -> None:
    """`--all` 列出云端全部上传件，并用本地会话记录标注「这条是谁记的」。"""

    async def _fake_list(client):
        _STUB_CALLS["client"] = client
        return [
            {"id": "file-a", "filename": "a.png", "bytes": 1024, "created_at": 1700000000,
             "expires_at": None},
            {"id": "file-b", "filename": "b.png", "bytes": 2048, "created_at": 1700000001,
             "expires_at": 4102444800},
        ]

    sessions = [(pathlib.Path("chat-20260915-1.jsonl"), "img-x", {"file_id": "file-a"})]
    rc, out, err = _run_cli_files(
        ["list", "--all"], _CFG, remote_list=_fake_list, sessions=sessions
    )
    assert rc == 0, err
    assert "服务端上传件 2 个" in out
    assert "file-a" in out and "a.png" in out and "会话=chat-20260915-1" in out
    assert "file-b" in out and "会话=未记录" in out
    assert "过期=永久" in out and "过期=2100-" in out  # expires_at=None ↔ 有值的两种展示
    assert _STUB_CALLS["llm"]["api_key"] == "sk-test" and _STUB_CALLS["closed"] is True


def test_files_list_all_empty_cloud() -> None:
    """云端为空时说清楚，不是默默什么都不打印。"""

    async def _empty(client):
        return []

    rc, out, err = _run_cli_files(["list", "--all"], _CFG, remote_list=_empty)
    assert rc == 0, err
    assert "云端为空" in out


def test_files_list_all_without_api_key_is_noop() -> None:
    """api_key 为空：列表也调不动 Files API（返回 1，且不建客户端）。"""

    async def _boom(client):  # pragma: no cover —— guard 没拦住才会走到
        raise AssertionError("api_key 为空时不该调 Files API")

    rc, out, err = _run_cli_files(["list", "--all"], 'api_key = ""\n', remote_list=_boom)
    assert rc == 1
    assert "api_key" in err and "llm" not in _STUB_CALLS
    assert "服务端上传件" not in out


# ---------------------------------------------------------------- gc --all


def test_files_gc_all_calls_files_api() -> None:
    """`--all` 用配置里的 key 建客户端 → 调 Files API 清空 → 关掉客户端。"""

    async def _fake_purge(client):
        _STUB_CALLS["purge"] = client
        return (
            [{"id": "file-a", "filename": "a.png", "bytes": 1024, "created_at": 1700000000}],
            {},
        )

    rc, out, err = _run_cli_files(["gc", "--all"], _CFG, purge=_fake_purge)
    assert rc == 0, err
    assert "已删除 1 个" in out and "file-a" in out and "a.png" in out
    assert _STUB_CALLS["llm"]["api_key"] == "sk-test"  # 走的是 -c 指定的配置
    assert _STUB_CALLS["closed"] is True  # 用完关连接池（不留未关闭的 httpx client）


def test_files_gc_all_without_api_key_is_noop() -> None:
    """api_key 为空时不假装清空：返回 1，且连客户端都不建。"""

    async def _boom_purge(client):  # pragma: no cover —— guard 没拦住才会走到
        raise AssertionError("api_key 为空时不该调 Files API")

    rc, out, err = _run_cli_files(["gc", "--all"], 'api_key = ""\n', purge=_boom_purge)
    assert rc == 1
    assert "api_key" in err and "llm" not in _STUB_CALLS
    assert "已删除" not in out


def test_files_gc_all_reports_delete_failures() -> None:
    """有删不掉的文件 → 退出码 1（脚本能发现没清干净）。"""

    async def _fake_purge(client):
        return ([], {"file-a": "RuntimeError: delete failed"})

    rc, _out, err = _run_cli_files(["gc", "--all"], _CFG, purge=_fake_purge)
    assert rc == 1 and "file-a" in err


def test_files_gc_all_survives_listing_failure() -> None:
    """列不出文件（网络/鉴权挂了）→ 报错返回 1，不假装清空。"""

    async def _boom_purge(client):
        raise ConnectionError("network down")

    rc, out, err = _run_cli_files(["gc", "--all"], _CFG, purge=_boom_purge)
    assert rc == 1 and "network down" in err and "已删除" not in out


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
