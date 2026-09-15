"""剪贴板图片落盘回归测试（无 pytest 依赖）。

跑法（任一）：

    uv run python tests/test_clipboard.py
    pytest tests/test_clipboard.py

覆盖 `src/pie/clipboard.py` 与 TUI 侧的两个入口（Ctrl+V / `/paste`）：

- 剪贴板是图片 → 落成 `~/.pie/files/img-<sha256[:16]>.png`（0o600、PNG 魔数、重复粘贴复用同一文件）；
- **零复制**：粘贴落的就是 `read` 会用的那个副本（`files.store_blob` 同字节命中同一路径），
  不会出现「同字节两个名字」；
- 剪贴板是**文件列表**（Windows CF_HDROP）→ 只认图片文件，返回原路径、不复制；
- 剪贴板空 / Linux 缺 wl-paste+xclip（Pillow 抛 NotImplementedError）/ 未装 Pillow → 返回 None 不抛错；
- TUI：Ctrl+V 有图 → 插入路径，没图 → 回退成原本的文本粘贴（不能吞掉）；`/paste` 没图 → 给出提示。

测试用假的 `ImageGrab` 替身（`clipboard._engine`），不碰真实剪贴板；落盘目录是临时目录
（`_TempBlobs` 把 `files.FILES_DIR` 指过去，别污染真实的 ~/.pie/files）。
"""

from __future__ import annotations

import asyncio
import contextlib
import os
import pathlib
import re
import subprocess
import sys
import tempfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "src"))
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from PIL import Image  # noqa: E402

from pie import clipboard  # noqa: E402
from pie.files import store_blob  # noqa: E402
from pie.tui import PieApp  # noqa: E402
from test_files import _TempBlobs  # noqa: E402
from test_tui import _dummy_session  # noqa: E402

PNG_MAGIC = b"\x89PNG\r\n\x1a\n"


class _FakeGrab:
    """替身：返回值就是 `ImageGrab.grabclipboard()` 的返回值（异常则抛出来）。"""

    def __init__(self, result: object) -> None:
        self._result = result
        self.calls = 0

    def grabclipboard(self):
        self.calls += 1
        if isinstance(self._result, BaseException):
            raise self._result
        return self._result


@contextlib.contextmanager
def _fake(result: object):
    """把 clipboard 的后端换成替身（退出时还原，避免影响其它用例）。"""
    saved = clipboard._engine
    fake = _FakeGrab(result)
    clipboard._engine = lambda: fake  # type: ignore[assignment]
    try:
        yield fake
    finally:
        clipboard._engine = saved


def test_image_lands_in_files_dir_and_is_idempotent() -> None:
    """图片 → files/ 的内容寻址 PNG；同一张图两次粘贴 = 同一个文件。"""
    with _TempBlobs() as blobs_dir:
        with _fake(Image.new("RGBA", (8, 6), (255, 0, 0, 128))):
            first = clipboard.grab_image_path()
            second = clipboard.grab_image_path()
        assert first is not None and first == second, f"{first} != {second}"
        assert first.parent == blobs_dir, f"没落在 files/ 里: {first}"
        assert first.name.startswith("img-") and first.suffix == ".png"
        assert len(first.stem) == len("img-") + 16, f"命名不是 img-<sha256[:16]>: {first.name}"
        assert first.read_bytes().startswith(PNG_MAGIC), "落盘的不是 PNG"
        assert first.stat().st_mode & 0o777 == 0o600, "副本权限应为 0o600"
        assert Image.open(first).size == (8, 6), "尺寸在编码中丢了"
        assert [p.name for p in blobs_dir.iterdir()] == [first.name], "留下了临时文件残留"


def test_paste_and_read_share_one_blob() -> None:
    """**零复制**：粘贴落的那份就是 read 会命中的那份（同字节 → 同 hash → 同路径）。"""
    with _TempBlobs() as blobs_dir:
        with _fake(Image.new("RGB", (7, 5), (1, 2, 3))):
            pasted = clipboard.grab_image_path()
        assert pasted is not None
        # read 侧等价动作：拿同一份字节走 store_blob（loop 里 ImageStore.ensure 就是这么做的）
        _, for_read = store_blob(pasted.read_bytes(), mime="image/png")
        assert for_read == pasted, f"read 又落了一份: {for_read} != {pasted}"
        assert len(list(blobs_dir.iterdir())) == 1, "同字节出现了两个名字"


def test_file_list_returns_image_path_without_copying() -> None:
    """CF_HDROP（剪贴板里是文件）：图片文件直接用原路径，非图片一律不算。"""
    with tempfile.TemporaryDirectory() as d, _TempBlobs() as blobs_dir, _wsl_off(), _mac_off():
        root = pathlib.Path(d)
        shot = root / "shot.PNG"
        Image.new("RGB", (4, 4)).save(shot)
        note = root / "notes.txt"
        note.write_text("hello", encoding="utf-8")
        with _fake([str(note), str(shot)]):
            assert clipboard.grab_image_path() == shot
        with _fake([str(note)]):
            assert clipboard.grab_image_path() is None
        with _fake([str(root / "missing.png")]):
            assert clipboard.grab_image_path() is None
        assert not list(blobs_dir.iterdir()), "文件列表分支不该往 files/ 复制东西"


def test_no_image_returns_none() -> None:
    """没有图片的各种形态都返回 None（不能抛错、不能吞掉调用方）。"""
    with _TempBlobs() as blobs_dir, _wsl_off(), _mac_off():
        for result in (
            None,  # 剪贴板空
            NotImplementedError("wl-paste or xclip is required"),  # Linux 缺工具
            ChildProcessError("wl-paste error"),  # Linux 子进程异常（OSError 子类）
            RuntimeError("clipboard busy"),  # 其它异常也不该炸界面
        ):
            with _fake(result):
                assert clipboard.grab_image_path() is None, result
        assert not list(blobs_dir.iterdir()), "失败路径不该留下文件"
    saved = clipboard._engine
    clipboard._engine = lambda: None  # 未装 Pillow
    try:
        assert clipboard.grab_image_path() is None
    finally:
        clipboard._engine = saved


def test_ctrl_v_inserts_path_or_falls_back_to_text() -> None:
    """Ctrl+V：有图插路径；没图走原本的文本粘贴（回归：不能把纯文本粘贴吃掉）。"""

    async def run() -> None:
        with _TempBlobs() as blobs_dir:
            app = PieApp(_dummy_session())
            async with app.run_test(size=(78, 26)) as pilot:
                await pilot.pause()
                inp = app.query_one("#input")
                with _fake(Image.new("RGB", (5, 5))):
                    await pilot.press("ctrl+v")
                    await pilot.pause()
                inserted = inp.text
                assert inserted.startswith(str(blobs_dir)), f"没插入路径: {inserted!r}"
                assert pathlib.Path(inserted).exists(), "插进来的路径不存在"
                # 文本粘贴回退：剪贴板没有图片时行为与覆盖前一致
                inp.text = ""
                app.copy_to_clipboard("纯文本粘贴")
                with _fake(None), _wsl_off(), _mac_off():
                    await pilot.press("ctrl+v")
                    await pilot.pause()
                assert inp.text == "纯文本粘贴", f"文本粘贴被吞了: {inp.text!r}"

    asyncio.run(run())


def test_paste_command_reports_when_no_image() -> None:
    """`/paste`：有图插路径；没图给提示而不是静默。"""

    async def run() -> None:
        with _TempBlobs() as blobs_dir:
            app = PieApp(_dummy_session())
            async with app.run_test(size=(78, 26)) as pilot:
                await pilot.pause()
                notes: list[tuple[str, str]] = []
                app._notify = lambda text, role="system", **kw: notes.append((role, text))  # type: ignore[method-assign]
                inp = app.query_one("#input")
                with _fake(Image.new("RGB", (5, 5))):
                    inp.text = "/paste"
                    await pilot.press("enter")
                    await pilot.pause()
                assert inp.text.startswith(str(blobs_dir)), f"没插入路径: {inp.text!r}"
                assert any("已插入图片路径" in t for _, t in notes), notes
                inp.text = ""
                notes.clear()
                with _fake(None), _wsl_off(), _mac_off():
                    inp.text = "/paste"
                    await pilot.press("enter")
                    await pilot.pause()
                assert inp.text == "", "没图时不该往输入框塞东西"
                assert any(role == "error" and "没有图片" in t for role, t in notes), notes

    asyncio.run(run())


@contextlib.contextmanager
def _wsl_env(png: bytes | None, enabled: bool = True):
    """模拟 WSL：wslpath / powershell.exe 都可用，PowerShell「把图写成 PNG」。

    替身从 PS 脚本里抠出目标路径（正则）直接写文件，等价于真实 PowerShell 的 Save。
    """
    saved_run, saved_which = clipboard.subprocess.run, clipboard.shutil.which
    saved_env = os.environ.get("WSL_DISTRO_NAME")
    calls: list[list[str]] = []

    def fake_run(cmd, **kwargs):
        calls.append(list(cmd))
        if cmd[0] == "wslpath":
            return subprocess.CompletedProcess(cmd, 0, str(cmd[-1]).encode() + b"\n", b"")
        if png is None:  # Windows 剪贴板里没有图
            return subprocess.CompletedProcess(cmd, 0, b"0", b"")
        pathlib.Path(re.search(r"'([^']+\.png)'", cmd[-1]).group(1)).write_bytes(png)
        return subprocess.CompletedProcess(cmd, 0, b"1", b"")

    if enabled:
        os.environ["WSL_DISTRO_NAME"] = "Ubuntu"
    else:
        os.environ.pop("WSL_DISTRO_NAME", None)
    clipboard.subprocess.run = fake_run  # type: ignore[assignment]
    clipboard.shutil.which = lambda name: "/usr/bin/powershell.exe" if name == "powershell.exe" else None  # type: ignore[assignment]
    try:
        yield calls
    finally:
        clipboard.subprocess.run, clipboard.shutil.which = saved_run, saved_which  # type: ignore[assignment]
        if saved_env is None:
            os.environ.pop("WSL_DISTRO_NAME", None)
        else:
            os.environ["WSL_DISTRO_NAME"] = saved_env


@contextlib.contextmanager
def _wsl_off():
    """关掉 WSL 后备（让「没有图片」的用例保持 hermetic，不去真起 PowerShell）。"""
    with _wsl_env(None, enabled=False) as calls:
        yield calls


@contextlib.contextmanager
def _mac_env(path: str | None, enabled: bool = True):
    """模拟 macOS：`_IS_MAC` 为真 + osascript 可用，返回给定的 POSIX 路径（None = 剪贴板里没有文件引用）。

    `enabled=False` 时只把 `_IS_MAC` 置假、**不动** `subprocess.run`（给 `_mac_off()` 用，便于与 `_wsl_env` 叠）。
    """
    saved_flag = clipboard._IS_MAC
    saved_run, saved_which = clipboard.subprocess.run, clipboard.shutil.which
    calls: list[list[str]] = []

    def fake_run(cmd, **kwargs):
        calls.append(list(cmd))
        return subprocess.CompletedProcess(cmd, 0, (path or "").encode(), b"")

    clipboard._IS_MAC = enabled
    if enabled:
        clipboard.subprocess.run = fake_run  # type: ignore[assignment]
        clipboard.shutil.which = lambda name: "/usr/bin/osascript" if name == "osascript" else None  # type: ignore[assignment]
    try:
        yield calls
    finally:
        clipboard._IS_MAC = saved_flag
        clipboard.subprocess.run, clipboard.shutil.which = saved_run, saved_which  # type: ignore[assignment]


@contextlib.contextmanager
def _mac_off():
    """关掉 macOS 的「文件引用」分支（让「没有图片」的用例不去真起 osascript / 读真实剪贴板）。"""
    with _mac_env(None, enabled=False) as calls:
        yield calls


def test_mac_file_reference_returns_path() -> None:
    """macOS：Finder 里 ⌘C 复制的图片文件（«class furl»）→ 直接用原文件，不往 files/ 复制。

    Pillow 的 macOS 分支只请求位图（«class PNGf»），文件引用拿不到 → 它返回 None；
    没有这一刀，「截图存成文件、再从 Finder 复制」就永远粘不进来。
    """
    with tempfile.TemporaryDirectory() as d, _TempBlobs() as blobs_dir, _wsl_off():
        shot = pathlib.Path(d) / "截屏.png"
        Image.new("RGB", (4, 4)).save(shot)
        note = pathlib.Path(d) / "notes.txt"
        note.write_text("hi", encoding="utf-8")
        with _fake(None), _mac_env(str(shot)) as calls:
            assert clipboard.grab_image_path() == shot
        assert calls and calls[0][0] == "osascript", calls
        with _fake(None), _mac_env(str(note)):
            assert clipboard.grab_image_path() is None, "复制的不是图片"
        with _fake(None), _mac_env(str(pathlib.Path(d) / "gone.png")):
            assert clipboard.grab_image_path() is None, "文件已不在"
        with _fake(None), _mac_env(None):
            assert clipboard.grab_image_path() is None, "剪贴板里是文本"
        assert not list(blobs_dir.iterdir()), "文件引用分支不该往 files/ 复制东西"
    saved_flag = clipboard._IS_MAC  # 非 macOS：连 osascript 都不查
    clipboard._IS_MAC = False
    try:
        assert clipboard._mac_file_path() is None
    finally:
        clipboard._IS_MAC = saved_flag


def test_wsl_fallback_reads_windows_clipboard() -> None:
    """WSL：Pillow 看不到剪贴板（没装 wl-paste/xclip）时才回退去问 Windows 剪贴板。"""
    no_tools = NotImplementedError("wl-paste or xclip is required")
    with _TempBlobs() as blobs_dir, tempfile.TemporaryDirectory() as d:
        shot = pathlib.Path(d) / "win.png"
        Image.new("RGB", (3, 4)).save(shot)
        with _fake(no_tools), _wsl_env(shot.read_bytes()) as calls:
            path = clipboard.grab_image_path()
        assert path is not None and path.parent == blobs_dir, f"WSL 后备没生效: {path}"
        assert Image.open(path).size == (3, 4)
        assert [p.name for p in blobs_dir.iterdir()] == [path.name], "留下了临时文件残留"
        assert [c[0] for c in calls] == ["wslpath", "powershell.exe"], calls
        with _fake(no_tools), _wsl_env(None), _mac_off():  # Windows 剪贴板里也没图 → 乖乖返回 None
            assert clipboard.grab_image_path() is None
    # 工具在、只是剪贴板里没图：不该白起一次 PowerShell（0.5s 冷启动 × 每次文本粘贴）
    with _fake(None), _wsl_env(b"x"), _mac_off() as calls:
        assert clipboard.grab_image_path() is None
    assert calls == [], f"「没有图片」不该跑子进程: {calls}"
    # 非 WSL 环境也不该去起 PowerShell
    with _fake(no_tools), _wsl_off() as calls:
        assert clipboard.grab_image_path() is None
    assert calls == [], f"非 WSL 却跑了子进程: {calls}"


def test_ctrl_g_pastes_image() -> None:
    """Ctrl+G：终端截走 Ctrl+V 时的兜底入口，与 `/paste` 同一实现。"""

    async def run() -> None:
        with _TempBlobs() as blobs_dir:
            app = PieApp(_dummy_session())
            async with app.run_test(size=(78, 26)) as pilot:
                await pilot.pause()
                inp = app.query_one("#input")
                with _fake(Image.new("RGB", (5, 5))):
                    await pilot.press("ctrl+g")
                    await pilot.pause()
                assert inp.text.startswith(str(blobs_dir)), f"Ctrl+G 没插入路径: {inp.text!r}"
                assert pathlib.Path(inp.text).exists()

    asyncio.run(run())


def test_palette_lists_paste_command() -> None:
    """`/paste` 进补全表（/help 文案由它生成，避免命令发现不了）。"""
    from pie.tui import PALETTE_COMMANDS

    assert any(cmd == "/paste" for cmd, _ in PALETTE_COMMANDS)


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
