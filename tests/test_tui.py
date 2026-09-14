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
from textual.app import App, ComposeResult

from pie.textkit import cjk_divide_line
from pie.theme import get_theme
from pie.tui import PieApp, SelectableRichLog, _box, _lean_line, _tool_result_box, _wide_text

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


# ---- 复制 ----


async def _check_boxes(log: SelectableRichLog, expect_truncate: bool = True) -> None:
    for name, md in MARKDOWN_CASES.items():
        panel = _box(THEME, md, role="assistant", title="pie")
        want = _wide_text(panel.renderable).strip("\n")
        log.write(panel)
        entry = log._entries[-1]
        got = _copy_whole_box(log)
        assert entry.spans is not None, f"{name}: 对齐失败（应能对齐，回退会断行）"
        assert _normalize_first_line(got) == _normalize_first_line(want), (
            f"{name}:\n  got ={got!r}\n  want={want!r}"
        )
    for name, body in TEXT_CASES.items():
        box = (
            _tool_result_box(THEME, body, title="shell [0]", role="tool_result", tool="shell")
            if name == "长行"
            else _box(THEME, body, tool="read")
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
                    log.write(_box(THEME, md, role="assistant", title="pie"))
                    assert _copy_whole_box(log)

    asyncio.run(run())


def test_copy_partial_rows_has_no_break() -> None:
    """部分行选择：不能把显示行的软换行带进复制内容。"""

    async def run() -> None:
        app = _LogOnly()
        async with app.run_test(size=(46, 20)) as pilot:
            await pilot.pause()
            log = app.query_one("#log", SelectableRichLog)
            log.write(_box(THEME, LONG_CMD_PATH * 2, tool="read"))
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
            log.write(_box(THEME, LONG_CMD_PATH, tool="shell", title="shell"))
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
            log.write(_box(app.palette, body, title="pie", role="assistant"))
            assert _copy_whole_box(log) == body
            app.query_one("#input").text = "测试中文输入"
            await pilot.pause()
            assert app.query_one("#input").text == "测试中文输入"

    asyncio.run(run())


def test_tool_render_helpers() -> None:
    """工具调用/结果渲染：实时与历史共用一套（参数 dict/JSON 串归一、历史才截断超长）。"""

    async def run() -> None:
        app = PieApp(_dummy_session())
        async with app.run_test(size=(80, 30)) as pilot:
            await pilot.pause()
            log = app.query_one("#log")
            # 参数归一：dict（实时）与 JSON 串（历史）一样，空参/坏 JSON 有兜底
            app._render_tool_call(log, "read", {"path": "a.txt"})
            assert _copy_whole_box(log) == '{"path": "a.txt"}'
            app._render_tool_call(log, "read", '{"path": "a.txt"}')
            assert _copy_whole_box(log) == '{"path": "a.txt"}'
            app._render_tool_call(log, "shell", "")
            assert _copy_whole_box(log) == "(无参数)"
            app._render_tool_call(log, "edit", "{半截 JSON")
            assert _copy_whole_box(log) == "{半截 JSON"
            # shell 结果：exit code 进标题、正文去掉 header；失败框染红（role）
            app._render_tool_result(log, "shell", "[exit=1]\n\nboom")
            entry = log._entries[-1]
            assert "shell [1]" in log.lines[entry.row].text
            assert _copy_whole_box(log) == "boom"
            app._render_tool_result(log, "read", "[工具错误] 文件不存在: x")
            assert _copy_whole_box(log) == "[工具错误] 文件不存在: x"
            # 超长：实时原样，历史回放截断 head/tail
            long_text = "\n".join(f"line{i}" for i in range(300))
            app._render_tool_result(log, "read", long_text)
            assert _copy_whole_box(log) == long_text
            app._render_tool_result(log, "read", long_text, truncate=True)
            truncated = _copy_whole_box(log)
            assert "[中间省略，全文见原始文件]" in truncated and "line299" in truncated
            assert "共 300 行" in log.lines[log._entries[-1].row].text

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
            app._render_tool_call(log, "read", {"path": "a.txt"})
            app._render_tool_result(log, "read", "内容", summary="a.txt")
            app._render_tool_result(log, "read", "[工具错误] 文件不存在: x", summary="a.txt")
            await pilot.pause()
            call_icon = pal.tool_icon("read")
            ok_mark, fail_mark = pal.lean_mark("ok"), pal.lean_mark("fail")
            # 行尾空格来自 Padding 的右留白（RichLog 不再自行补宽），比对时去掉
            texts = [s.text.rstrip() for s in log.lines]
            assert texts[-4] == " " * BOX_INSET + f"{call_icon} read a.txt"
            # 结果行：状态标记接管行首（不再用 tool_result 图标），标记不会被截断
            assert texts[-3] == " " * BOX_INSET + f"{ok_mark} read a.txt"
            assert texts[-2] == " " * BOX_INSET + f"{fail_mark} read a.txt"
            assert texts[-1] == " " * BOX_INSET + "→ [工具错误] 文件不存在: x"
            # 工具行不画盒子：没有边框字符；正文列与盒内正文列一致
            assert not any(ch in "".join(texts[-4:]) for ch in "│╭╰╮╯")
            log.write(_box(pal, "回复", title="pie", role="assistant"))
            await pilot.pause()
            box_body = log.lines[-2].text.rstrip()
            assert box_body[0] == "│" and box_body[1] == " " and box_body[2] == "回"
            assert box_body.index("回") == BOX_INSET  # 盒内正文列 == 工具行正文列

            # 框选复制：留白是表现层的，不进源文本（不再多出前导空格）
            log._sel_start, log._sel_end = (first, 0), (first + 3, 200)
            assert log._selected_text() == (f"{call_icon} read a.txt\n{ok_mark} read a.txt"
                                            f"\n{fail_mark} read a.txt\n→ [工具错误] 文件不存在: x")

            # 超长行：恒为一行、行首标记保留、行尾省略号、不超出内容区宽度
            log.write(_lean_line(pal, "shell", "x" * 500, role="tool_call"))
            log.write(_lean_line(pal, "shell", "y", role="tool_result", mark=ok_mark))
            log.write(_lean_line(pal, "shell", "cancel", role="tool_result",
                                 mark=pal.lean_mark("cancelled")))
            await pilot.pause()
            long_row, short_row = log.lines[-3].text, log.lines[-2].text.rstrip()
            assert long_row.startswith(" " * BOX_INSET) and long_row.rstrip().endswith("…")
            assert "\n" not in long_row and cell_len(long_row.rstrip()) <= log.scrollable_content_region.width
            assert short_row == " " * BOX_INSET + f"{ok_mark} shell y"
            assert log.lines[-1].text.rstrip() == " " * BOX_INSET + f"{pal.lean_mark('cancelled')} shell cancel"

            # 窄宽下标记也不能丢（这是状态标记放行首的全部意义）
            narrow = PieApp(_dummy_session(lean=True))
            async with narrow.run_test(size=(24, 12)) as p2:
                await p2.pause()
                nlog = narrow.query_one("#log")
                nlog.write(_lean_line(narrow.palette, "shell", "z" * 200, role="error",
                                     mark=narrow.palette.lean_mark("fail")))
                await p2.pause()
                assert nlog.lines[-1].text.startswith(" " * BOX_INSET + narrow.palette.lean_mark("fail"))

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
            log.write(_box(mocha.palette, fence_md, role="assistant"))
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
            log.write(_box(latte.palette, fence_md, role="assistant"))
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
