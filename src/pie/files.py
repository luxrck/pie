"""图片上传（DeepSeek Files API）：一次上传、按内容复用 `file_id`。

背景：`read` 读到的图片原先一律 base64 内联进请求（见 `loop._build_image_parts`），
而这条 ImageMessage 会留在历史里 —— **同一张图每轮请求都要重发**（3 MB 的图 ≈ 4 MiB
body/轮），还受 inline 的「单图 32 MiB、body 48 MiB」上限约束。
Files API 允许上传一次拿 `file_id`，之后用 `{"type":"file","file_id":...}` 引用
（file_id 的图单张可到 64 MiB，且不占 body 的图片额度）。

本地仍留一份**内容寻址**的副本 `~/.pie/files/<hash_id>`：
  - 它才是上传源（**先落副本、再从副本上传** → 「服务端那份 = 本地这份」这个不变量恒成立），
    原图被移动/删除/改写后照样能续用、重传；
  - 历史被压缩掉、或换回不支持 file_id 的模型时，还能从它拿回原始字节回退 inline。

上传结果按**会话**记录（`Session.files` → `__meta__.files`），不做全局缓存：
会话删了记录随之消失，不需要维护全局索引；代价是**跨会话不复用**（会重传一次，
服务端不做内容去重 —— 实测同一张图上传两次得到两个不同 file_id）。

命中判据 = 同一 `sha256` 且 `base_url`/`key_fp` 一致 且未过期：
`file_id` 属于 API key，换 key（`-c FILE` 多套配置）或换端点后旧 id 会 400。
"""

from __future__ import annotations

import hashlib
import json
import os
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any, Iterator

from .config import PIE_DIR

FILES_DIR = PIE_DIR / "files"  # 本地内容寻址副本（与 context/ 分开：那是压缩落盘的文本）

# 支持 `file` 内容块的模型（文档：上传的文件与 deepseek-flash 配套；旧名同源）
FILES_API_MODELS = ("deepseek-flash", "deepseek-v4-flash-vision-exp")

# 服务端允许的上传有效期上限（文档：1 小时 ~ 30 天；不传 = 永久保留）
TTL_MAX_DAYS = 30

_IMAGE_EXTS = {
    "image/jpeg": ".jpg",
    "image/png": ".png",
    "image/gif": ".gif",
    "image/webp": ".webp",
    "image/bmp": ".bmp",
}

_STALE_FILE_HINTS = ("do not exist or are not created", "file_ids do not exist")

# gc 保护窗口：比这新的文件一概先留着（小时）。理由见 collect_file_garbage：
# 粘贴进 files/ 的图在「被某次 read 登记」之前还没有任何会话引用它，
# 但它是活的（路径正躺在输入框里）→ 用 mtime 窗口挡住误删。
GC_PROTECT_HOURS = 24


def key_fingerprint(api_key: str | None) -> str:
    """API key 指纹（sha256 前 8 位）：用于判断缓存的 file_id 是否还属于当前 key。"""
    return hashlib.sha256((api_key or "").encode("utf-8")).hexdigest()[:8]


def hash_id(data: bytes) -> str:
    """图片的内容 id：`img-<sha256[:16]>`（形状与 context 的 `turn-<hash>` 一致）。"""
    return "img-" + hashlib.sha256(data).hexdigest()[:16]


def blob_path(image_hash: str, mime: str = "") -> Path:
    """本地副本路径（扩展名只为可读，不参与查找）。"""
    return FILES_DIR / f"{image_hash}{_IMAGE_EXTS.get(mime, '')}"


def store_blob(data: bytes, *, mime: str = "") -> tuple[str, Path]:
    """把图片复制进 `~/.pie/files/`（内容寻址、幂等），返回 `(hash_id, 副本路径)`。

    先写临时文件再 rename：避免读到别人写了一半的副本（多进程同时 read 同一张图）。
    权限固定 0o600：图是用户数据（截图/照片），没必要给同机其它用户看。
    """
    image_hash = hash_id(data)
    path = blob_path(image_hash, mime)
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_name(f".{path.name}.tmp-{datetime.now():%H%M%S%f}")
        tmp.write_bytes(data)
        os.chmod(tmp, 0o600)
        tmp.replace(path)
    return image_hash, path


def model_supports_files(model: str | None) -> bool:
    """当前模型是否支持 `file` 内容块（不支持则走 inline，避免白白 400）。"""
    return bool(model) and any(name in model for name in FILES_API_MODELS)


def entry_is_usable(
    entry: dict[str, Any] | None, *, base_url: str, key_fp: str, now: datetime | None = None
) -> bool:
    """记录还能不能直接用：同一 key/base_url + 未过期（`expires_at` 缺失 = 永久）。"""
    if not entry or not entry.get("file_id"):
        return False
    if entry.get("base_url") != base_url or entry.get("key_fp") != key_fp:
        return False
    expires_at = entry.get("expires_at")
    if expires_at is None:  # 未记有效期 = 服务端永久保留
        return True
    now_ts = (now or datetime.now()).timestamp()
    return now_ts < float(expires_at) - 60  # 留 1 分钟余量，别卡在过期边缘


def is_stale_file_error(exc: BaseException) -> bool:
    """判断异常是不是「file_id 失效/不属于本账号」——用来触发自愈（回退 inline + 重传）。"""
    if getattr(exc, "status_code", None) != 400:
        return False
    text = str(exc)
    return any(hint in text for hint in _STALE_FILE_HINTS)


async def upload_blob(client: Any, path: Path, *, ttl_days: int = TTL_MAX_DAYS) -> dict[str, Any]:
    """上传本地副本，返回 `{"file_id": ..., "expires_at": ...}`；失败抛原异常（调用方回退 inline）。

    `expires_after` 是 DeepSeek 的扩展字段，openai SDK 不认识 → 走 `extra_body`。
    """
    seconds = max(0, int(ttl_days)) * 86400
    extra_body = (
        {"expires_after": {"anchor": "created_at", "seconds": min(seconds, TTL_MAX_DAYS * 86400)}}
        if seconds > 0
        else None
    )
    with path.open("rb") as handle:
        kwargs: dict[str, Any] = {"file": handle, "purpose": "user_data"}
        if extra_body is not None:
            kwargs["extra_body"] = extra_body
        resp = await client.files.create(**kwargs)
    return {"file_id": resp.id, "expires_at": getattr(resp, "expires_at", None)}


@dataclass
class ImageStore:
    """会话级图片记录（就是 `Session.files` 本体）+ 上传策略。

    `entries` 是调用方传进来的 dict（`Session.files`），**就地更新** → 下次 `Session.save()`
    自然带上（`save()` 是全量重写，不需要追加/墓碑那套）。
    """

    entries: dict[str, dict[str, Any]] = field(default_factory=dict)
    base_url: str = ""
    key_fp: str = ""
    ttl_days: int = TTL_MAX_DAYS
    enabled: bool = True  # False = 只落本地副本、不上传（配置关闭 / 模型不支持）

    async def ensure(
        self, client: Any, *, data: bytes, mime: str, filename: str, src: str
    ) -> str | None:
        """保证这张图有一个可用的 `file_id`；拿不到（未开启/失败）返回 None → 调用方内联。

        未开启或没有客户端时**连本地副本都不落**（不开这个功能就没必要多存一份）。
        """
        if not self.enabled or client is None:
            return None
        image_hash, local = store_blob(data, mime=mime)
        entry = self.entries.get(image_hash)
        if entry_is_usable(entry, base_url=self.base_url, key_fp=self.key_fp):
            return str(entry["file_id"])
        try:
            uploaded = await upload_blob(client, local, ttl_days=self.ttl_days)
        except Exception as e:  # 网络/配额/权限……一律静默回退 inline，不阻塞用户
            print(
                f"[warn] 图片上传失败，本次改用内联 base64: {type(e).__name__}: {e}",
                file=sys.stderr,
            )
            return None
        self.entries[image_hash] = {
            "hash_id": image_hash,
            "sha256": hashlib.sha256(data).hexdigest(),
            "size": len(data),
            "mime": mime,
            "filename": filename,
            "src": src,
            "local": str(local),
            "file_id": uploaded["file_id"],
            "base_url": self.base_url,
            "key_fp": self.key_fp,
            "uploaded_at": datetime.now().isoformat(timespec="seconds"),
            "expires_at": uploaded["expires_at"],
        }
        return str(uploaded["file_id"])

    def invalidate(self, file_id: str) -> bool:
        """把某个 `file_id` 标成失效（服务端删了/换 key）：下次同图重新上传。"""
        for entry in self.entries.values():
            if entry.get("file_id") == file_id:
                entry["expires_at"] = 0
                return True
        return False


# ---------------------------------------------------------------- 本地副本与清点


def iter_session_files(sessions_dir: Path | None = None) -> Iterator[tuple[Path, str, dict[str, Any]]]:
    """遍历所有会话记录里的图片条目：产出 `(会话文件, hash_id, 条目)`。"""
    directory = sessions_dir or (PIE_DIR / "sessions")
    for session_file in sorted(directory.glob("*.jsonl")):
        try:
            with session_file.open(encoding="utf-8") as f:
                for line in f:  # meta 是第一行，读完就够
                    if not line.strip():
                        continue
                    data = json.loads(line)
                    if data.get("__meta__"):
                        for image_hash, entry in (data.get("files") or {}).items():
                            if isinstance(entry, dict):
                                yield session_file, image_hash, entry
                    break
        except (OSError, json.JSONDecodeError):
            continue


def referenced_blobs(sessions_dir: Path | None = None) -> set[Path]:
    """被任何会话引用到的本地副本路径。"""
    return {Path(entry["local"]) for _, _, entry in iter_session_files(sessions_dir) if entry.get("local")}


def collect_file_garbage(
    sessions_dir: Path | None = None, protect_hours: int = GC_PROTECT_HOURS
) -> list[Path]:
    """`~/.pie/files/` 下没有被任何会话引用、且**已经放了 protect_hours 小时**的副本。

    注意：副本是**跨会话共享**的（同一内容一个文件），所以「删会话」不会自动删副本 ——
    回收靠这次无状态扫描；服务端那份由 `expires_after`（默认 30 天）自行过期。

    为什么要 mtime 保护窗口：「未被引用」不等于「没人用」—— 刚剪贴板粘贴进 files/ 的图
    （见 `clipboard.py`）在它被某次 `read` 登记进 `__meta__.files` 之前没有任何引用，
    但路径可能正躺在输入框/某条命令里，删了就是死链接。
    """
    if not FILES_DIR.exists():
        return []
    referenced = referenced_blobs(sessions_dir)
    fresh_after = time.time() - max(0, protect_hours) * 3600
    garbage = []
    for p in FILES_DIR.iterdir():
        if not p.is_file() or p.name.startswith(".") or p in referenced:
            continue
        try:
            if p.stat().st_mtime >= fresh_after:
                continue  # 保护窗口内：可能是刚粘贴、还没被 read 的图
        except OSError:
            continue
        garbage.append(p)
    return sorted(garbage)

