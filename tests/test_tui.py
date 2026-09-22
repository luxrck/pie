"""TUI 回归测试：框选复制保真 + CJK 断行（无 pytest 依赖）。

跑法（任一）：

    uv run python tests/test_tui.py
    pytest tests/test_tui.py        # 装了 pytest 也能直接跑（不需要 asyncio 插件）

覆盖两块容易回归、又不好靠肉眼发现的行为：

- **断行**（显示层）：纯 ASCII 与 Rich 原实现逐字节一致；全角字可断、行被填满；英文词不被切开；
  随机混排（中文/全角标点/emoji/韩文/ASCII）不产生重复或越界的断点。
- **复制**（框选）：多种 Markdown / 纯文本形态 × 多种终端宽，整盒复制 = 源文本（长行不断行、空格不丢）；
  部分行选择不含换行；真机鼠标拖拽；真实 PieApp 挂载后同样成立。

注：测试直接读 `SelectableRichLog` 的私有选中状态（`_sel_start/_sel_end/_entries`）来模拟拖拽，
属于白盒测试——鼠标事件坐标换算另有真机拖拽用例覆盖。
"""

from __future__ import annotations

import asyncio
import random
import string
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from rich._wrap import divide_line as rich_divide_line
from rich.cells import cell_len
from rich.padding import Padding
from rich.panel import Panel
from textual.app import App, ComposeResult

from pie import textkit
from pie.textkit import cjk_compute_wrap_offsets, cjk_divide_line
from pie.theme import get_theme
from pie.tui import (
    PieApp,
    PieTextArea,
    SelectableRichLog,
    _box,
    _error_body,
    _lean,
    _panel,
    _wide_text,
)

THEME = get_theme("catppuccin-mocha")

LONG_CMD = "wget --header='X-Token: abc' https://example.com/very/long/path?a=1&b=2 -O out.bin"
LONG_CMD_PATH = "sed -n '1,5p' /etc/hosts && echo 这是一条很长的命令用来验证框选复制不会再断行"

# 整盒复制应逐字节等于「宽渲染后的源文本」的 Markdown 形态
MARKDOWN_CASES = {
    "段落（长行）": "这段说明文字特意写得很长很长，用来触发软换行，看看复制出来会不会被断成好几行。" * 2,
    "无序列表（悬挂缩进）": "- 框选复制时按字符区间**切源文本**；同一个 `src` 的相邻显示行合并成一个切片，"
    "于是被吃掉的空格和真实换行都随切片一起还原。\n- 第二项也写长一点，确保续行会带悬挂缩进，考验对齐。",
    "有序列表": "1. 有序项内容很长很长很长很长很长很长很长很长很长很长很长很长很长\n2. 第二项",
    "嵌套列表": "- 外层\n  - 内层列表项也很长很长很长需要折行看看缩进怎么处理才行啊\n- 外层二",
    "块引用": "> 引用一段比较长的话用来触发换行，看看块引用的续行是不是每行都带竖线，中文没有空格更要看仔细。\n\nok",
    "围栏代码块": f"跑一下：\n\n```bash\n{LONG_CMD}\n```\n结束。",
    "缩进代码块": "```py\ndef f(x):\n    return x + 1  # 这一行比较长会折行，缩进要保住\n```",
    "标题": "# 一级标题很短\n\n正文也短。",
    "表格": "| 列名一 | 列名二 |\n| --- | --- |\n| 单元格内容比较长会折行 | 2 |",
    "加粗与行内代码": "这里有 **加粗** 和 `code` 以及普通文字，混在一行里会比较长一些用来触发换行效果。",
}

# 纯文本盒（工具结果 / 用户消息）—— 复制应等于源文本（tab 按显示展开成空格）
TEXT_CASES = {
    "长行": LONG_CMD_PATH * 2,
    "含 tab 缩进": "def f():\n\tif x:\n\t\treturn 1  # 注释\n\n末尾",
    "英文单词不切开": "aaa keep-intact-token bbb ccc keep-intact-token ddd eee keep-intact-token fff",
}

# Markdown 分隔线会被渲染成「随宽度铺满」的规则线，不存在与宽度无关的源文本 → 只验证优雅回退
FALLBACK_CASES = {"分隔线": "上面\n\n---\n\n下面"}

WIDTHS = (46, 74, 104)


class _LogOnly(App):
    """只挂一个日志区的最小 App（渲染 / 复制用，不启动会话）。"""

    def compose(self) -> ComposeResult:
        yield SelectableRichLog(wrap=True, highlight=True, markup=True, id="log")


def _copy_whole_box(log: SelectableRichLog) -> str:
    """选中最近一次写入的整个盒子（含边框行）并返回复制结果。"""
    entry = log._entries[-1]
    log._sel_start = (entry.row, 0)
    log._sel_end = (entry.row + entry.count - 1, 10**6)
    return log._selected_text()


def _box_border_colors(log: SelectableRichLog, entry=None) -> set[str]:
    """取某条日志（默认最近一次写入）**标题行**出现的样式串集合。

    盒子标题行整行都是边框样式（`╭─ 标题 ─╮`）→ 集合里含哪个 role 色，就知道这条盒子用的哪个
    role 的边框（`str(Style)` 即 CSS 色字符串，如 `#585b70`）。
    """
    entry = entry if entry is not None else log._entries[-1]
    return {str(seg.style) for seg in log.lines[entry.row] if seg.text.strip()}


def _normalize_first_line(text: str) -> str:
    """允许首行前导空白差异：显示层的居中/悬挂缩进不算内容。"""
    lines = text.splitlines()
    if not lines:
        return ""
    return "\n".join([lines[0].strip(), *lines[1:]])


def _split(text: str, width: int) -> list[str]:
    offsets = cjk_divide_line(text, width, True)
    parts, prev = [], 0
    for offset in [*offsets, len(text)]:
        parts.append(text[prev:offset])
        prev = offset
    return parts


# ---- 断行 ----


def test_wrap_matches_rich_on_ascii() -> None:
    """纯 ASCII 文本必须与 Rich 原实现逐字节一致（含 fold=False），否则英文排版会漂。"""
    rng = random.Random(0)
    for _ in range(4000):
        text = " ".join(
            "".join(rng.choice(string.ascii_letters + ".,;:/=<>-_") for _ in range(rng.randint(1, 14)))
            for _ in range(rng.randint(1, 10))
        )
        for width in (4, 9, 30, 81):
            for fold in (True, False):
                assert cjk_divide_line(text, width, fold) == rich_divide_line(text, width, fold), (
                    text,
                    width,
                    fold,
                )


def test_wrap_cjk_fills_width() -> None:
    """中文长句没有空格：应逐字折行、把行填满（Rich 词级断行只会用掉一半宽度）。"""
    text = "• 框选复制时按字符区间切源文本；同一个 src 的相邻显示行合并成一个切片，于是被吃掉的空格和真实换行都随切片一起还原。"
    width = 74
    first = _split(text, width)[0]
    assert cell_len(first) >= width - 2, f"首行只用了 {cell_len(first)}/{width} 格"
    assert "".join(_split(text, width)) == text, "分段后拼不回原文（丢字符）"
    # 英文词不被切开（词长 < 行宽时应整词挪到下一行，而不是拆开）
    assert "keep-intact-token" in _split("aaa keep-intact-token bbb", 20)[1]


def test_wrap_offsets_are_sane_on_fuzz() -> None:
    """随机混排不产生重复/越界断点（宽度极小时 chop_cells 会产出空块，是回归高发区）。"""
    rng = random.Random(1)
    pool = "中文测试全角（）；：，。！？abcXYZ019 -_/=.\t😀한글カタカナ"
    for _ in range(3000):
        text = "".join(rng.choice(pool) for _ in range(rng.randint(0, 60)))
        width = rng.choice([1, 2, 3, 5, 8, 13, 21, 40, 80])
        offsets = cjk_divide_line(text, width, True)
        assert offsets == sorted(set(offsets)), (text, width, offsets)
        assert all(0 < offset < len(text) for offset in offsets), (text, width, offsets)
        assert "".join(_split(text, width)) == text
    assert cjk_divide_line("", 10) == []
    assert cjk_divide_line("中文", 0) == []


def _segments(text: str, offsets: list[int]) -> list[str]:
    parts, prev = [], 0
    for offset in [*offsets, len(text)]:
        parts.append(text[prev:offset])
        prev = offset
    return parts


def test_textarea_wrap_never_exceeds_width() -> None:
    """输入框（TextArea）的显示行宽度不能超过可用宽度。

    回归点：Textual 的 `compute_wrap_offsets` 把**行尾空白**也算进「放得下」判断，
    而 Rich 的 `divide_line` 按 `rstrip()` 算。两者曾经共用同一实现 → 含中文的行
    遇到尾随空格时会产出 width + 1 格（多出来的正是那个空格）的显示行，
    输入框里光标/滚动位置会随之偏 1 格。
    """
    rng = random.Random(7)
    pool = "中文测试全角（）；：，。！？abcXYZ019 -_/=."
    for _ in range(3000):
        text = "".join(rng.choice(pool) for _ in range(rng.randint(0, 60)))
        width = rng.choice([4, 9, 13, 35, 80])
        offsets = cjk_compute_wrap_offsets(text, width, tab_size=4)
        assert offsets == sorted(set(offsets)), (text, width, offsets)
        for seg in _segments(text, offsets):
            assert cell_len(seg) <= width, (
                f"{text!r} w={width} 行超宽 {cell_len(seg)}: {seg!r}"
            )


def test_textarea_wrap_breaks_ascii_at_char() -> None:
    """输入框里英文也按字符断：长单词/长路径不会整块挪到下一行留一大片空白。"""
    width = 35
    for text in (
        "a" * (width * 3),
        "hello world 这是一段 mixed with english words 的混排文本用来测断行",
        "/Users/luxrck/Projects/synthetic_phone/src/synthetic_phone/tools.py",
        "wget --header='X-Token: abc' https://example.com/very/long/path?a=1&b=2 -O out.bin",
    ):
        segs = _segments(text, cjk_compute_wrap_offsets(text, width, 4))
        assert "".join(segs) == text, (text, segs)
        assert all(cell_len(seg) <= width for seg in segs), (text, segs)
        # 行被填满（词级断行的老毛病：行尾动不动空十几格）
        assert all(cell_len(seg) >= width - 1 for seg in segs[:-1]), (
            f"{text!r} 行没填满：{[cell_len(s) for s in segs]}"
        )


# ---- 复制 ----


def _write_box(log: SelectableRichLog, body, *, palette=None, **kwargs) -> None:
    """把 `_box` 产出的 renderable 逐个写进日志区（_box 返回列表：简洁模式可能是两行）。

    palette 默认用测试的 mocha 主题（THEME）。
    """
    for r in _box(palette or THEME, body, **kwargs):
        log.write(r)


async def _check_boxes(log: SelectableRichLog, expect_truncate: bool = True) -> None:
    for name, md in MARKDOWN_CASES.items():
        panel = _box(THEME, md, role="assistant")[0]
        want = _wide_text(panel.renderable).strip("\n")
        log.write(panel)
        entry = log._entries[-1]
        got = _copy_whole_box(log)
        assert entry.spans is not None, f"{name}: 对齐失败（应能对齐，回退会断行）"
        assert _normalize_first_line(got) == _normalize_first_line(want), (
            f"{name}:\n  got ={got!r}\n  want={want!r}"
        )
    for name, body in TEXT_CASES.items():
        # 长行用 shell 结果盒（正文就是那一行长路径）；其余用普通文本盒（只考换行/复制）
        box = (
            _panel(THEME, {"name": "shell", "content": body}, role="tool_result")[0]
            if name == "长行"
            else _box(THEME, body)[0]
        )
        log.write(box)
        entry = log._entries[-1]
        got = _copy_whole_box(log)
        want = body.expandtabs(8).rstrip() if expect_truncate else body
        assert entry.spans is not None, f"{name}: 对齐失败"
        assert got == want, f"{name}:\n  got ={got!r}\n  want={want!r}"


def test_copy_matches_source_at_many_widths() -> None:
    async def run() -> None:
        for width in WIDTHS:
            app = _LogOnly()
            async with app.run_test(size=(width, 24)) as pilot:
                await pilot.pause()
                log = app.query_one("#log", SelectableRichLog)
                await _check_boxes(log)
                # 回退路径：分隔线（随宽度铺满）应当优雅回退，不报错、不崩
                for md in FALLBACK_CASES.values():
                    _write_box(log, md, role="assistant")
                    assert _copy_whole_box(log)

    asyncio.run(run())


def test_copy_partial_rows_has_no_break() -> None:
    """部分行选择：不能把显示行的软换行带进复制内容。"""

    async def run() -> None:
        app = _LogOnly()
        async with app.run_test(size=(46, 20)) as pilot:
            await pilot.pause()
            log = app.query_one("#log", SelectableRichLog)
            _write_box(log, LONG_CMD_PATH * 2)
            entry = log._entries[-1]
            log._sel_start = (entry.row + 1, 5)
            log._sel_end = (entry.row + entry.count - 2, 7)
            got = log._selected_text()
            assert got and "\n" not in got, f"部分行选择仍带换行: {got!r}"
            assert got in (LONG_CMD_PATH * 2), f"复制内容不在源文本里: {got!r}"

    asyncio.run(run())


def test_copy_real_mouse_drag() -> None:
    """真机鼠标拖拽（走 _cell_at 坐标换算 + 剪贴板），长行应复制成一行。"""

    async def run() -> None:
        app = _LogOnly()
        async with app.run_test(size=(60, 20)) as pilot:
            await pilot.pause()
            log = app.query_one("#log", SelectableRichLog)
            _write_box(log, {"name": "shell", "content": LONG_CMD_PATH}, role="tool_result")
            entry = log._entries[-1]
            region = log.scrollable_content_region
            await pilot.mouse_down(log, offset=(region.x - log.region.x, region.y - log.region.y))
            await pilot.mouse_up(
                log,
                offset=(
                    region.x - log.region.x + 56,
                    region.y - log.region.y + entry.row + entry.count - 1,
                ),
            )
            await pilot.pause()
            assert app.clipboard == LONG_CMD_PATH, f"剪贴板 ={app.clipboard!r}"

    asyncio.run(run())


def _dummy_session(lean: bool = False, theme: str = "catppuccin-mocha"):
    """空会话（不连模型），供 PieApp 冒烟。lean=True 走简洁模式渲染。

    主题默认固定 mocha：测试不依赖终端背景探测的结果。
    """
    from pie.config import Config, TuiConfig
    from pie.session import Session
    from pie.tools import default_tools

    class _DummyLLM:
        def complete(self, messages, tools, model=None):  # pragma: no cover - 冒烟不调用
            raise NotImplementedError

    return Session(
        config=Config(theme=theme, tui=TuiConfig(lean=lean)),
        llm=_DummyLLM(),
        tools=default_tools(),
    )


def test_pie_app_mount_and_copy() -> None:
    """真实 PieApp 挂载（空会话，不调模型）：界面正常起、日志区复制仍等于源文本。"""

    async def run() -> None:
        app = PieApp(_dummy_session())
        async with app.run_test(size=(78, 26)) as pilot:
            await pilot.pause()
            log = app.query_one("#log")
            body = "这是一句没有空格的中文长句，用来确认全角字可以逐字折行，而英文单词 keep-intact-token 不会被切开。"
            _write_box(log, body, role="assistant", palette=app.palette)
            assert _copy_whole_box(log) == body
            app.query_one("#input").text = "测试中文输入"
            await pilot.pause()
            assert app.query_one("#input").text == "测试中文输入"

    asyncio.run(run())


def test_slash_path_is_treated_as_plain_text() -> None:
    """以 / 开头但**不是已知命令**的输入按普通消息发出去（粘贴进来的绝对路径常被误伤）。"""

    async def run() -> None:
        app = PieApp(_dummy_session())
        async with app.run_test(size=(78, 26)) as pilot:
            await pilot.pause()
            submitted: list[str] = []
            commands: list[str] = []
            app._submit = lambda text: submitted.append(text)  # type: ignore[method-assign]
            app._command = lambda text: commands.append(text)  # type: ignore[method-assign]
            inp = app.query_one("#input", PieTextArea)
            for text in ("/home/cc/.pie/files/img-1863cc4256104f41.png", "/help", "/nope"):
                inp.text = text
                await pilot.press("enter")
                await pilot.pause()
            assert submitted == ["/home/cc/.pie/files/img-1863cc4256104f41.png", "/nope"], submitted
            assert commands == ["/help"], commands

    asyncio.run(run())


def test_input_newline_keys() -> None:
    """换行只认 Shift+Enter / Ctrl+J / Ctrl+Enter，裸 Enter 一律提交。

    Ctrl+Enter 只在支持修饰键上报的终端（kitty 协议）才独立送达；多数终端把它编码成
    LF（= Ctrl+J）→ 两者都收，行为才不因终端而异。
    """

    async def run() -> None:
        app = PieApp(_dummy_session())
        async with app.run_test(size=(78, 26)) as pilot:
            await pilot.pause()
            submitted: list[str] = []
            app._submit = lambda text: submitted.append(text)  # type: ignore[method-assign]
            inp = app.query_one("#input", PieTextArea)
            inp.text = "a"
            inp.cursor_location = (0, 1)  # 程序化设 text 后光标在行首，移到末尾更像真人输入
            for key in ("shift+enter", "ctrl+j", "ctrl+enter"):
                await pilot.press(key)
                await pilot.pause()
                assert inp.text == "a\n", (key, inp.text)
                assert submitted == [], (key, submitted)
                inp.text = "a"
                inp.cursor_location = (0, 1)
            await pilot.press("enter")
            await pilot.pause()
            assert submitted == ["a"], submitted

    asyncio.run(run())


def test_tool_render_helpers() -> None:
    """工具调用/结果渲染：实时与历史共用一套（参数 dict/JSON 串归一、历史才截断超长）。"""

    async def run() -> None:
        app = PieApp(_dummy_session())
        async with app.run_test(size=(80, 30)) as pilot:
            await pilot.pause()
            log = app.query_one("#log")
            # 参数归一：dict（实时）与 JSON 串（历史）一样，空参/坏 JSON 有兜底
            app._render_tool_call("read", {"path": "a.txt"})
            assert _copy_whole_box(log) == '{"path": "a.txt"}'
            app._render_tool_call("read", '{"path": "a.txt"}')
            assert _copy_whole_box(log) == '{"path": "a.txt"}'
            app._render_tool_call("shell", "")
            assert _copy_whole_box(log) == "(无参数)"
            app._render_tool_call("edit", "{半截 JSON")
            assert _copy_whole_box(log) == "{半截 JSON"
            # shell 结果：exit code 进标题、正文去掉 header；失败框染红（role）
            app._render_tool_result("shell", "[exit=1]\n\nboom")
            entry = log._entries[-1]
            assert "shell [1]" in log.lines[entry.row].text
            assert _copy_whole_box(log) == "boom"
            app._render_tool_result("read", "[工具错误] 文件不存在: x")
            assert _copy_whole_box(log) == "[工具错误] 文件不存在: x"
            # 超长：盒子模式也只显示前 N 行（实时与 resume 回放同一条路，不再“历史才截”）
            from pie.tui import BOX_BODY_LINES

            long_text = "\n".join(f"line{i}" for i in range(BOX_BODY_LINES + 200))
            app._render_tool_result("read", long_text)
            shown = _copy_whole_box(log)
            assert f"line{BOX_BODY_LINES - 1}" in shown and f"line{BOX_BODY_LINES}" not in shown
            assert "已省略 200 行" in shown
            assert f"共 {BOX_BODY_LINES + 200} 行" in log.lines[log._entries[-1].row].text

    asyncio.run(run())


def test_error_box_shows_cause_chain() -> None:
    """出错盒：首行仍是 `类名: 消息`，另加 `↳` 异常链。

    没这两行时，连接类错误只能看到一个被 SDK 包过的 `APIConnectionError: Connection error.`
    ——真实原因（httpx 的 DNS / 连接 / TLS 错误）全在 `__cause__` 里，等于没说。
    """

    class ConnectError(Exception):
        pass

    class APIConnectionError(Exception):
        pass

    exc = APIConnectionError("Connection error.")
    exc.__cause__ = ConnectError("[Errno -3] Temporary failure in name resolution")

    lines = _error_body(exc).splitlines()
    assert lines[0] == "APIConnectionError: Connection error."
    assert lines[1].startswith("↳ ") and "ConnectError" in lines[1]
    assert "name resolution" in lines[1]
    assert len(lines) == 2  # 只首行 + 链，不再多拼提示

    # 链取不到就只剩首行
    assert _error_body(ValueError("随便一个错")) == "ValueError: 随便一个错"

    # `raise ... from None` 抑制掉的 context 不算真原因；异常链成环也不会转不出来
    suppressed = RuntimeError("外层")
    suppressed.__context__ = ValueError("被抑制的")
    suppressed.__suppress_context__ = True
    assert _error_body(suppressed) == "RuntimeError: 外层"
    a, b = RuntimeError("a"), RuntimeError("b")
    a.__cause__ = b
    b.__cause__ = a
    assert _error_body(a) == "RuntimeError: a\n↳ RuntimeError: b"

    async def run() -> None:
        app = PieApp(_dummy_session())
        async with app.run_test(size=(100, 30)) as pilot:
            await pilot.pause()
            log = app.query_one("#log")
            app._fail_turn(exc)
            shown = _copy_whole_box(log)
            assert shown.splitlines()[0] == "APIConnectionError: Connection error."
            assert "name resolution" in shown

    asyncio.run(run())


def _panel_body(renderable) -> str:
    """盒子（Panel）内的正文纯文本（Text / Markdown 都归一）。"""
    body = renderable.renderable
    return body.plain if hasattr(body, "plain") else str(body)


def test_box_dispatch_and_single_writer() -> None:
    """渲染分层约束：盒子/简洁的分派全在 _box（纯函数，不碰 App），#log 只有 _notify 一个写入口。

    这两条保证「实时事件 / resume 历史回放 / 命令反馈」不可能各自写一份样式——
    新增写入路径（绕过 _notify）或把样式决策塞回 App 方法里，这条用例就红。
    """
    from pie.tui import BOX_BODY_LINES, MANUAL_SHELL_ROLE, _box

    def call(*args, **kwargs):
        return _box(THEME, *args, **kwargs)

    # 工具调用：盒子 vs 简洁单行（body 与 _panel 同构：{"name", "arguments"}）
    read_call = {"name": "read", "arguments": {"path": "a.txt"}}
    assert isinstance(call(read_call, role="tool_call")[0], Panel)
    assert isinstance(call(read_call, role="tool_call", lean=True)[0], Padding)
    # 工具结果：盒子（shell exit code 进标题）；简洁模式成功 1 行、失败 2 行（单行 + 正文块）
    shell_box = call({"name": "shell", "content": "[exit=1]\n\nboom"}, role="tool_result")[0]
    assert isinstance(shell_box, Panel) and "shell [1]" in str(shell_box.title)
    assert len(call({"name": "read", "arguments": {"path": "a.txt"}, "content": "ok"},
                    role="tool_result", lean=True)) == 1
    assert len(call({"name": "read", "content": "[工具错误] x"}, role="tool_result", lean=True)) == 2
    # 手动 !cmd：由**调用方**显式 lean=False（_box 不再特判 manual）→ 仍套盒子、边框默认灰
    manual_box = call({"name": "shell", "arguments": "", "content": "[exit=1]\n\nout"},
                      role="tool_result", border_role=MANUAL_SHELL_ROLE, lean=False)[0]
    assert isinstance(manual_box, Panel)
    # 超长正文：盒子模式也只显示前 N 行（实时与 resume 回放同一条路，不再分实时/历史）
    long_text = "\n".join(f"line{i}" for i in range(BOX_BODY_LINES + 200))
    hot = call({"name": "read", "content": long_text}, role="tool_result")[0]
    shown = _panel_body(hot)
    assert f"line{BOX_BODY_LINES - 1}" in shown and f"line{BOX_BODY_LINES}" not in shown
    assert "已省略 200 行" in shown and f"只显示前 {BOX_BODY_LINES} 行" in str(hot.title)
    # 消息正文（user/assistant）不截：那是用户要看的内容（title 由 role 推，不再手传）
    assert _panel_body(call(long_text, role="user")[0]) == long_text

    # #log 的唯一写入口是 _notify（tui.py 里唯一的 `log.write(`）
    src = (Path(__file__).resolve().parents[1] / "src" / "pie" / "tui.py").read_text(
        encoding="utf-8"
    )
    assert src.count("log.write(") == 1, "出现了绕过 _notify 的 #log 写入"


def test_resume_history_uses_same_renderers() -> None:
    """resume 回放走的就是实时事件那套渲染（盒子 / 简洁单行都要跑通）。

    覆盖两个入口的共用关系：user → "你"、assistant+tool_calls → 工具调用、tool → 结果，
    且简洁模式下结果行的摘要靠 tool_call_id 与调用参数配对（历史里结果排在调用后面）。
    """
    from pie.context import AssistantMessage, ToolMessage, UserMessage

    tool_calls = [
        {
            "id": "call_1",
            "type": "function",
            "function": {"name": "shell", "arguments": '{"command": "ls src"}'},
        }
    ]

    def _seed(session) -> None:
        session.messages.messages.extend(
            [
                UserMessage("看看 src"),
                AssistantMessage("好的", tool_calls=tool_calls),
                ToolMessage("[exit=0]\n\npie\ntests", tool_call_id="call_1", tool_name="shell"),
                AssistantMessage("两个目录。"),
            ]
        )

    async def run() -> None:
        # 盒子模式：三个盒子（你 / pie / shell[0]）+ 工具调用盒子；工具结果用 tool_result 棕框
        app = PieApp(_dummy_session())
        _seed(app.session)
        async with app.run_test(size=(80, 40)) as pilot:
            await pilot.pause()
            log = app.query_one("#log")
            text = "\n".join(s.text for s in log.lines)
            assert "你" in text and "看看 src" in text and "好的" in text
            assert "shell [0]" in text and "pie" in text
            # 工具结果框用 tool_result 棕框（倒数第二条写入；最后一条是末尾的 assistant 回复）
            assert app.palette.role_border("tool_result") in _box_border_colors(log, log._entries[-2])
            assert "两个目录。" in text

        # 简洁模式：回放的工具行也要带摘要（ls src），而不是退化成参数 JSON
        app = PieApp(_dummy_session(lean=True))
        _seed(app.session)
        async with app.run_test(size=(80, 40)) as pilot:
            await pilot.pause()
            log = app.query_one("#log")
            text = "\n".join(s.text.rstrip() for s in log.lines)
            assert f"{app.palette.tool_icon('shell')} shell ls src" in text
            assert f"{app.palette.lean_mark('ok')} shell ls src" in text
            assert "{\"command\"" not in text, "回放丢了 tool_call_id 配对，摘要退成参数 JSON"

    asyncio.run(run())


def test_lean_tool_lines_are_padded() -> None:
    """简洁模式：工具单行状态标记在行首、左右留白与盒内正文同列，且复制不带留白。"""

    async def run() -> None:
        from pie.tui import BOX_INSET

        app = PieApp(_dummy_session(lean=True))
        async with app.run_test(size=(80, 24)) as pilot:
            await pilot.pause()
            log = app.query_one("#log")
            pal = app.palette
            assert app.lean

            first = len(log.lines)          # 下面 4 行的起始下标（留白不污染源文本的断言要用）
            app._render_tool_call("read", {"path": "a.txt"})
            app._render_tool_result("read", "内容", arguments={"path": "a.txt"})
            app._render_tool_result("read", "[工具错误] 文件不存在: x", arguments={"path": "a.txt"})
            await pilot.pause()
            call_icon = pal.tool_icon("read")
            ok_mark, fail_mark = pal.lean_mark("ok"), pal.lean_mark("fail")
            # 行尾空格来自 Padding 的右留白（RichLog 不再自行补宽），比对时去掉
            texts = [s.text.rstrip() for s in log.lines]
            assert texts[-4] == " " * BOX_INSET + f"{call_icon} read a.txt"
            # 结果行：状态标记接管行首（不再用 tool_result 图标），标记不会被截断
            assert texts[-3] == " " * BOX_INSET + f"{ok_mark} read a.txt"
            assert texts[-2] == " " * BOX_INSET + f"{fail_mark} read a.txt"
            assert texts[-1] == " " * BOX_INSET + "↳ [工具错误] 文件不存在: x"
            # 工具行不画盒子：没有边框字符；正文列与盒内正文列一致
            assert not any(ch in "".join(texts[-4:]) for ch in "│╭╰╮╯")
            _write_box(log, "回复", role="assistant", palette=pal)
            await pilot.pause()
            box_body = log.lines[-2].text.rstrip()
            assert box_body[0] == "│" and box_body[1] == " " and box_body[2] == "回"
            assert box_body.index("回") == BOX_INSET  # 盒内正文列 == 工具行正文列

            # 框选复制：留白是表现层的，不进源文本（不再多出前导空格）
            log._sel_start, log._sel_end = (first, 0), (first + 3, 200)
            assert log._selected_text() == (f"{call_icon} read a.txt\n{ok_mark} read a.txt"
                                            f"\n{fail_mark} read a.txt\n↳ [工具错误] 文件不存在: x")

            # 超长行：恒为一行、行首标记保留、行尾省略号、不超出内容区宽度
            # （`_lean` 现在返回**元素列表**：调用/成功 1 个，失败/取消会多一个正文块 → 取 [0] 这一行）
            from pie.loop import CANCEL_TEXT

            row0 = len(log.lines)
            log.write(_lean(pal, {"name": "shell", "arguments": {"command": "x" * 500}},
                            role="tool_call")[0])
            log.write(_lean(pal, {"name": "shell", "arguments": {"command": "y"}, "content": "y"},
                            role="tool_result")[0])
            log.write(_lean(pal, {"name": "shell", "arguments": {"command": "cancel"},
                                  "content": CANCEL_TEXT}, role="tool_result")[0])
            await pilot.pause()
            long_row = log.lines[row0].text
            short_row = log.lines[row0 + 1].text.rstrip()
            assert long_row.startswith(" " * BOX_INSET) and long_row.rstrip().endswith("…")
            assert "\n" not in long_row and cell_len(long_row.rstrip()) <= log.scrollable_content_region.width
            assert short_row == " " * BOX_INSET + f"{ok_mark} shell y"
            assert log.lines[row0 + 2].text.rstrip() == " " * BOX_INSET + f"{pal.lean_mark('cancelled')} shell cancel"

            # 窄宽下标记也不能丢（这是状态标记放行首的全部意义）
            narrow = PieApp(_dummy_session(lean=True))
            async with narrow.run_test(size=(24, 12)) as p2:
                await p2.pause()
                nlog = narrow.query_one("#log")
                nlog.write(_lean(narrow.palette, {"name": "shell", "arguments": {"command": "z" * 200},
                                                  "content": "[工具错误] " + "z" * 200},
                                 role="tool_result")[0])
                await p2.pause()
                fail_mark_n = narrow.palette.lean_mark("fail")
                assert any(ln.text.startswith(" " * BOX_INSET + fail_mark_n) for ln in nlog.lines)

    asyncio.run(run())


def test_manual_shell_is_boxed_even_in_lean_mode() -> None:
    """手动 !cmd 不吃简洁模式：始终套盒子，边框用**默认灰**（成功/取消）/ 红（失败）。

    题目：!cmd 是用户主动执行、输出本身就是要看的东西，不是可折叠的工具活动，所以 lean 只压
    agent 回合里的工具行；同时它不该占 tool_call 的橙棕身份色（那是 agent 工具调用的），
    用 role_border 的兑底色 system 灰，状态图标 ✓/✗/■ 照旧。
    """

    async def run() -> None:
        app = PieApp(_dummy_session(lean=True))
        async with app.run_test(size=(70, 30)) as pilot:
            await pilot.pause()
            log = app.query_one("#log")
            pal = app.palette
            assert app.lean, "本用例的前提是简洁模式已开"

            async def _noop(cmd: str) -> None:  # 只验渲染，不起真进程
                return None

            app._exec_shell_async = _noop  # type: ignore[method-assign]
            app._run_shell("ls")
            entry = log._entries[-1]
            head = log.lines[entry.row].text
            assert head.startswith(f"╭─ {pal.tool_icon('shell')} shell"), head
            assert "$ ls" in log.lines[entry.row + 1].text
            assert pal.role_border("system") in _box_border_colors(log)

            # 结果框：成功/取消 = 默认灰，失败 = 红；状态字形保留（与简洁模式同一套）
            cases = (
                (0, pal.lean_mark("ok"), pal.role_border("system")),
                (1, pal.lean_mark("fail"), pal.role_border("error")),
                ("cancelled", pal.lean_mark("cancelled"), pal.role_border("system")),
            )
            for code, mark, color in cases:
                app._show_shell_result("out\n", code)
                entry = log._entries[-1]
                head = log.lines[entry.row].text
                assert head.startswith("╭") and f"{mark} shell" in head, head
                assert color in _box_border_colors(log, entry), head
                assert "out" in log.lines[entry.row + 1].text

            # 不外溢到 agent 回合：shell 工具结果仍是棕框 tool_result
            app._render_tool_result("shell", "[exit=0]\n\nhi")
            entry = log._entries[-1]
            assert pal.role_border("tool_result") in _box_border_colors(log, entry)

    asyncio.run(run())


def test_input_scrollbar_matches_log() -> None:
    """输入框的滚动条样式必须与 #log 一致（默认是 Textual 的 2 cell 黑底蓝条，很跳）。

    TextArea 是 ScrollView：它的 ScrollBar 子控件渲染时读的是**父控件**（即 #input）的
    scrollbar-* 样式 → 把 build_css 里那份 `scrollbar` 也写进 #input 即可，无需另起一套。
    """

    props = (
        "scrollbar_size_vertical",
        "scrollbar_size_horizontal",
        "scrollbar_background",
        "scrollbar_background_hover",
        "scrollbar_background_active",
        "scrollbar_color",
        "scrollbar_color_hover",
        "scrollbar_color_active",
    )

    async def run() -> None:
        app = PieApp(_dummy_session())
        async with app.run_test(size=(78, 26)) as pilot:
            await pilot.pause()
            log = app.query_one("#log")
            inp = app.query_one("#input", PieTextArea)
            for prop in props:
                assert getattr(inp.styles, prop) == getattr(log.styles, prop), (
                    prop,
                    getattr(log.styles, prop),
                    getattr(inp.styles, prop),
                )
            # 内容溢出时确实用的是 1 cell 窄条（不是默认的 2 cell）
            inp.text = "\n".join(f"line{i}" for i in range(20))
            await pilot.pause()
            assert inp.vertical_scrollbar.display
            assert inp.vertical_scrollbar.thickness == 1
            assert inp.vertical_scrollbar.size.width == 1

    asyncio.run(run())


def test_command_smoke() -> None:
    """命令反馈：/help 由 PALETTE_COMMANDS 生成，未知命令/参数报错，都能写回消息流。"""

    async def run() -> None:
        app = PieApp(_dummy_session())
        async with app.run_test(size=(100, 30)) as pilot:
            await pilot.pause()
            log = app.query_one("#log")
            app._command("/help")
            help_text = _copy_whole_box(log)
            assert "/status" in help_text and "!cmd 直接执行 shell" in help_text
            app._command("/status")
            assert _copy_whole_box(log).strip()
            app._command("/nope")
            assert "未知命令" in _copy_whole_box(log)
            app._command("/compact bad")
            assert "未知压缩模式" in _copy_whole_box(log)
            app._command("/thinking weird")
            assert "未知思考级别" in _copy_whole_box(log)

    asyncio.run(run())


def test_markdown_code_styles() -> None:
    """Markdown 代码样式：深色变体走 Rich 原生（黑底青 + monokai 代码块），
    浅色变体只改代码（去黑底）且代码块换浅色高亮主题。

    背景：RichLog 用 App console 渲染，我们从没给 console 设 theme → 走 Rich 的
    DEFAULT_STYLES（`markdown.code = bold cyan on black` + 代码块 monokai `#272822`），
    浅色终端下像一块墨。
    """
    from pygments.styles import get_style_by_name
    from rich.color import Color

    fence_md = "```python\nprint(1)\n```\n"

    def fence_bg(log):
        segs = [seg for strip in log.lines for seg in strip if "print" in seg.text]
        assert segs, [[seg.text for seg in strip] for strip in log.lines]
        return segs[0].style.bgcolor

    async def run() -> None:
        # 深色变体：完全不动
        mocha = PieApp(_dummy_session())
        async with mocha.run_test(size=(78, 26)) as pilot:
            await pilot.pause()
            code = mocha.console.get_style("markdown.code")
            assert code.bgcolor is not None and code.bgcolor.name == "black", code.bgcolor
            assert "magenta" in str(mocha.console.get_style("markdown.h2"))
            log = mocha.query_one("#log")
            _write_box(log, fence_md, role="assistant", palette=mocha.palette)
            await pilot.pause()
            bg = fence_bg(log)
            assert bg is not None and bg.triplet == Color.parse("#272822").triplet, bg
        # 浅色变体：代码去黑底（值取自 palette.markdown_code），fence 换浅色主题
        # （期望底色从 palette.code_theme 派生，不写死颜色：改主题不必改测试）
        latte = PieApp(_dummy_session(theme="catppuccin-latte"))
        async with latte.run_test(size=(78, 26)) as pilot:
            await pilot.pause()
            for key in ("markdown.code", "markdown.code_block"):
                style = latte.console.get_style(key)
                assert style.bgcolor is None, (key, style.bgcolor)
            assert latte.palette.markdown_code in str(latte.console.get_style("markdown.code"))
            # 未接管的元素仍是 Rich 默认
            assert "magenta" in str(latte.console.get_style("markdown.h2"))
            log = latte.query_one("#log")
            _write_box(log, fence_md, role="assistant", palette=latte.palette)
            await pilot.pause()
            bg = fence_bg(log)
            expected_bg = Color.parse(get_style_by_name(latte.palette.code_theme).background_color)
            assert bg is not None and bg.triplet == expected_bg.triplet, bg

    asyncio.run(run())


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
