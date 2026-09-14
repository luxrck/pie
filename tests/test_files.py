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
- 本地副本回收：只删「没有被任何会话 `__meta__.files` 引用」的副本。
"""

from __future__ import annotations

import asyncio
import json
import pathlib
import sys
import tempfile
import types

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "src"))

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
    model_supports_files,
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
    with _TempBlobs() as blobs_dir, tempfile.TemporaryDirectory() as sessions_dir:
        kept = store_blob(PNG, mime="image/png")[1]
        store_blob(PNG2, mime="image/png")
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

