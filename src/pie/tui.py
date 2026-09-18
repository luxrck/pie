"""Textual TUI：pi / tau 风格的聊天界面。非 TTY 或 Textual 缺失时回退 readline。

本模块包含：应用编排与控件（布局 / 命令 / 回合 worker / 流式渲染）、日志区控件与框选复制
（SelectableRichLog，见文件内分区注释）。显示层文本处理（CJK 断行 / 转义清洗）在 textkit.py，
配色与 CSS 在 theme.py。

异步架构：回合在 Textual worker（同一事件循环）里 await session.aturn()，
流式增量（模型 reasoning/content、shell 逐行输出）经 on_event 实时渲染到
消息流下方的 #stream 区；/stop（或 Esc）通过 asyncio.Event 优雅取消当前回合。
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import sys
from bisect import bisect_right
from pathlib import Path
from typing import Any, NamedTuple

from rich.console import Console
from rich.panel import Panel
from rich.padding import Padding
from rich.cells import cell_len
from rich.segment import Segment
from rich.style import Style
from rich.text import Text
from rich.markdown import Markdown
from rich.theme import Theme as RichTheme
from textual import events
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, VerticalScroll
from textual.message import Message
from textual.strip import Strip
from textual.widgets import Button, RichLog, Static, TextArea
from textual.worker import Worker

from . import aio, clipboard
from .config import REASONING_LEVELS
from .context import content_text
from .loop import CANCEL_TEXT
from .session import Session
from .termbg import detect_dark_background
from .textkit import install_cjk_wrap, rich_text, strip_escapes
from .theme import Theme, build_css, get_theme

install_cjk_wrap()  # 显示层断行改成 CJK 友好（Rich 正文 + TextArea 输入框）

# 命令补全候选：(命令, 说明)
PALETTE_COMMANDS: list[tuple[str, str]] = [
    ("/help", "显示帮助"),
    ("/status", "查看 token 用量"),
    ("/stop", "取消当前正在执行的模型请求/工具（等价 Esc）"),
    ("/thinking", "查看/切换思考深度（/thinking <level>，Tab 补全）"),
    ("/model", "查看模型列表 / 切换模型（/model <id>，Tab 补全）"),
    ("/paste", "剪贴板里的图片存成文件，把路径插进输入框"),
    ("/compact", "工具级 + 轮次级压缩"),
    ("/compact tools", "只做工具级压缩"),
    ("/compact turns", "只做轮次级压缩"),
    ("/clear", "归档当前窗口，开新窗口"),
    ("/save", "保存会话（可带文件路径）"),
    ("/reset", "清空对话历史"),
    ("/exit", "退出"),
    ("/quit", "退出"),
]


STREAM_MAX_LINES = 8  # #stream 区最多显示的行数（!shell 实时输出）

# 粘贴不到图时按平台补一句「能照做」的提示（macOS 的坑：⇧⌘4 是**存文件**，不进剪贴板）
_NO_IMAGE_HINT = {
    "darwin": "（macOS：截图要按 ⌃⇧⌘4 才会进剪贴板；⇧⌘4 是存成文件——"
              "存成文件后在 Finder 里 ⌘C 复制也行，会直接插入那个路径）",
    "linux": "（Linux 下还需装 wl-clipboard 或 xclip）",
}.get(sys.platform, "（图片要先进系统剪贴板）")

# 盒子的几何（Rich Panel）：round 边框固定 1 列/边，内边距见 _BOX_PADDING。
# 简洁模式的工具单行要跟**盒内正文**对齐，缩进就从这里推——别在两边各写一份 2。
_BOX_BORDER = 1
_BOX_PADDING = (0, 1)
BOX_INSET = _BOX_BORDER + _BOX_PADDING[1]  # 盒内正文相对日志区左边的列偏移（= 2）

# 盒子模式（lean=False）下工具正文最多显示的行数：read 大文件 / shell 长输出 / resume 回放
# 一口气几百行会把消息流刷没 → **只显示前 N 行**（末尾一行省略提示、标题里标总行数）。
# 消息正文（user / assistant / 命令反馈）不截：那些本来就是用户要看的内容。
BOX_BODY_LINES = 24

# 手动 !cmd 的盒子边框色：role_border 的兑底色 = 默认灰（与命令反馈盒同色）。
# !cmd 不是 agent 的工具调用 → 不占 tool_call 的橙棕身份色；失败/取消仍走各自的 role。
MANUAL_SHELL_ROLE = "system"

# 异常盒正文的组装（回合失败 `_fail_turn` / !shell 兜底共用）：首行 `类名: 消息`（= 以前显示的
# 全部内容），后面再补 `↳ mod.Type: 消息` 异常链——SDK 会把真实原因包一层（openai 的
# APIConnectionError 裹着 httpx 的 DNS / 连接 / TLS 错误），只显示最外层等于没说。
_EXC_CHAIN_MAX = 4  # 异常链最多展开几层：够定位，又不至于把消息流刷没
_EXC_MODULE_SKIP = {"builtins", "__main__"}  # 这些模块前缀加在类名前没意义


# 文本盒的标题从 role 推（工具盒的标题是工具名，由 body 里的 name 推）
ROLE_TITLES = {"user": "你", "assistant": "pie"}

# 简洁模式（lean=True）失败/取消时下方的正文块：**截断规则与 `_panel` 完全一致**
# （只显示前 BOX_BODY_LINES 行 + 省略提示），不再自留一套 head/tail 额度。
# C0 控制字符（含 \x7f）：单行工具行的摘要/工具名里不允许出现
_CONTROL_CHARS = frozenset(map(chr, range(0x20))) | {"\x7f"}


def _exc_chain(exc: BaseException, limit: int = _EXC_CHAIN_MAX) -> list[str]:
    """异常链（`__cause__` / `__context__`）→ 一行一条，**不含**最外层那条。

    只走真原因：`raise ... from None` 抑制掉的 `__context__` 不算。每层压成单行
    （`APIStatusError` 的 `str()` 带多行 body，直接塞进盒子会很难看）。
    """
    out: list[str] = []
    seen = {id(exc)}
    cur = exc.__cause__ or (None if exc.__suppress_context__ else exc.__context__)
    while cur is not None and len(out) < limit and id(cur) not in seen:
        seen.add(id(cur))
        try:
            msg = " ".join(str(cur).split())
        except Exception:  # 自定义 __str__ 可能抛
            msg = ""
        text = f"{type(cur).__name__}: {msg}" if msg else type(cur).__name__
        mod = type(cur).__module__.split(".")[0]
        out.append(text if mod in _EXC_MODULE_SKIP else f"{mod}.{text}")
        cur = cur.__cause__ or (None if cur.__suppress_context__ else cur.__context__)
    return out


def _error_body(exc: BaseException) -> str:
    """异常盒的正文：首行 `类名: 消息` + `↳` 异常链（链取不到就只剩首行）。"""
    return "\n".join([f"{type(exc).__name__}: {exc}"] + [f"↳ {line}" for line in _exc_chain(exc)])


def _panel(palette: Theme, body: Any, *, role: str = "system", border_role: str = "") -> list[Panel]:
    """**盒子形态**：把一条输出画成带边框的盒子（与 `_lean` 平级、参数和它一致）。

    `role` 只有三类取值，**工具名一律写在 body 里**（不再当 role 用）：

    - **消息类**（system / user / assistant / error / cancelled）：body 就是正文；
      标题查 `ROLE_TITLES`（user→「你」、assistant→「pie」），图标 `role_icon(role)`。
    - **`tool_call`**：body = `{"name": <工具名>, "arguments": <参数>}` → 正文是参数文本、
      标题/图标取工具名（标题里不另加修饰）。
    - **`tool_result`**：body = `{"name": <工具名>, "arguments": <参数>, "content": <结果正文>}
      → 退出码 / 失败前缀 / 取消文本定边框色与 ✓/✗/⏹，shell 的退出码进标题。

    `border_role` 显式覆盖边框色（不填就按上面推）：唯一用途是手动 `!cmd` **成功**的结果框用默认灰
    （MANUAL_SHELL_ROLE），同时保住 role 带来的 ✓/✗/⏹ 图标。工具正文超过 BOX_BODY_LINES 行时
    只显示前 N 行；消息正文不截。
    """
    tool_body, shell = False, False
    if role == "tool_call":
        data = body if isinstance(body, dict) else {}
        name = str(data.get("name") or "")
        args = data.get("arguments")
        # 参数 → 展示文本：解析不了的字符串（截断/手写）原样显示，空参数标一下
        text = ""
        if isinstance(args, str):
            raw = args.strip()
            if not raw:
                text = "(无参数)"
            else:
                try:
                    args = json.loads(raw)
                except ValueError:
                    text = args
        if not text:
            try:
                text = json.dumps(args, ensure_ascii=False) if args else "(无参数)"
            except TypeError:
                text = str(args) or "(无参数)"
        title, icon, tool_body = name, palette.tool_icon(name), True
    elif role == "tool_result":
        data = body if isinstance(body, dict) else {}
        name = str(data.get("name") or "")
        text = str((data.get("content") if data else body) or "")
        code: Any = None
        if text.startswith("[exit="):  # shell 风格：退出码进标题、正文去掉 [exit=] 头
            shell = True
            code = text[6:].split("]", 1)[0]
            text = text.split("\n\n", 1)[1].lstrip() if "\n" in text else ""
        # 成败判定：退出码（0 成功 / cancelled 取消 / 其余失败）或工具层的失败前缀
        if shell:
            status = "tool_result" if str(code) == "0" else (
                "cancelled" if code == "cancelled" else "error"
            )
        elif text.strip() == CANCEL_TEXT:
            status = "cancelled"
        elif text.startswith(("[工具错误]", "[工具异常]", "[参数解析失败]", "[shell] ")):
            status = "error"
        else:
            status = "tool_result"
        title = f"{name} [{code}]" if shell else name
        icon, tool_body = palette.result_icon(status, name), True
        role = status  # 边框色/下标靠状态
    else:
        text, title, icon = str(body or ""), ROLE_TITLES.get(role, ""), palette.role_icon(role)
    if tool_body and len(text.splitlines()) > BOX_BODY_LINES:  # 工具正文只显示前 N 行
        lines = text.splitlines()
        omitted = len(lines) - BOX_BODY_LINES
        text = "\n".join(
            lines[:BOX_BODY_LINES] + [f"...[已省略 {omitted} 行，共 {len(lines)} 行]..."]
        )
        title += f"（共 {len(lines)} 行，只显示前 {BOX_BODY_LINES} 行）"
    if shell:
        title += "" if text else "（无输出）"
        text = text or "(无输出)"
    else:
        text = text or "(空回复)"
    content: Text | Markdown = (
        Markdown(strip_escapes(text, keep_sgr=False), code_theme=palette.code_theme)
        if role == "assistant"
        else rich_text(text, palette.body_text)
    )
    return [
        Panel(
            content,
            title=f"{icon} {title}".strip() or None,
            title_align="left",
            border_style=palette.role_border(border_role or role),
            padding=_BOX_PADDING,
        )
    ]


def _lean(
    palette: Theme, body: Any, *, role: str = "system", border_role: str = ""
) -> list[Padding]:
    """**简洁形态**：把一条输出画成单行（工具结果失败/取消时再跟一个缩进正文块）。

    参数与 `_panel` 一致、body/role 语义也一样（见它的说明），只是画成紧凑形态：

    - `tool_call` → `图标 工具名 摘要` 一行；
    - `tool_result` → `状态标记 工具名 摘要`（**标记放行首**，向右裁天然保住它）+ 失败/取消时
      的正文块（超过 BOX_BODY_LINES 行时只显示前 N 行——与 `_panel` 同一条规则）。

    单行用的是「一句话摘要」而不是完整参数：**read / write / edit 取 path、shell 取 command**
    （`LEAN_SUMMARY_KEYS`），没命中就退回参数文本——摘要就在这一层从 `arguments` 推出来。
    消息类 role 没有单行形态 → 由 `_box` 交给 `_panel`，不会走到这里。
    """
    data = body if isinstance(body, dict) else {}
    name = str(data.get("name") or "")
    args = data.get("arguments")
    # 摘要：read/write/edit 取 path、shell 取 command；没命中退回参数文本（实时给 dict、
    # 历史回放给 JSON 串，两种形态都接受）。与 _panel 的「参数→文本」是同一套规则。
    parsed = args
    summary = ""
    if isinstance(args, str):
        raw = args.strip()
        if not raw:
            summary = "(无参数)"
        else:
            try:
                parsed = json.loads(raw)
            except ValueError:
                summary = raw  # 截断 / 手写的参数：原样当摘要
    if not summary and isinstance(parsed, dict):
        key = LEAN_SUMMARY_KEYS.get(name)
        value = parsed.get(key) if key else None
        if isinstance(value, str) and value:
            summary = value
    if not summary:
        try:
            summary = json.dumps(parsed, ensure_ascii=False) if parsed else "(无参数)"
        except TypeError:
            summary = str(parsed) or "(无参数)"
    # 压成**单行可打印文本**：no_wrap 只管「不按空白回绕」，真换行照样断行；tab 会被摊成
    # 变宽空格，让宽度计算与标记位置失准 → 换行/回车写字面 \n、tab 换空格、其余控制符丢掉
    summary = summary.replace("\r\n", "\n").replace("\r", "\n").replace("\t", " ")
    summary = "".join(ch for ch in summary.replace("\n", "\\n") if ch not in _CONTROL_CHARS)

    if role == "tool_call":
        icon, status = palette.tool_icon(name), None
        color = palette.role_border(border_role or "tool_call")
    else:
        text = str((data.get("content") if isinstance(data, dict) else body) or "")
        # 成败与 _panel 同一套：退出码定成败、正文去掉 [exit=] 头；否则看取消文本/失败前缀
        if text.startswith("[exit="):
            code: Any = text[6:].split("]", 1)[0]
            body_text = text.split("\n\n", 1)[1].lstrip() if "\n" in text else ""
            status = (
                "tool_result" if str(code) == "0" else "cancelled" if code == "cancelled" else "error"
            )
        else:
            body_text = text
            status = (
                "cancelled"
                if text.strip() == CANCEL_TEXT
                else "error"
                if text.startswith(("[工具错误]", "[工具异常]", "[参数解析失败]", "[shell] "))
                else "tool_result"
            )
        # 行首标记只认 ok/fail/cancelled（见 palette.lean_mark），状态→它的映射就在这里
        icon = palette.lean_mark({"tool_result": "ok", "error": "fail", "cancelled": "cancelled"}[status])
        color = palette.role_border(border_role or status)

    # 单行（无盒子）：`[标记|图标] 工具名 摘要`；左右留白用 Padding（不进源文本）
    line = Text(no_wrap=True, overflow="ellipsis")
    if icon:
        line.append(f"{icon} ", style=color)
    line.append(name, style=f"bold {color}")
    if summary:
        line.append(f" {summary}", style=color if status == "error" else palette.muted)
    out = [Padding(line, (0, BOX_INSET))]

    if status not in (None, "tool_result") and body_text.strip():
        # 失败/取消才在下方输出正文：首行 `↳ `、续行两格对齐；截断与 _panel 同一条规则
        style = palette.role_error if status == "error" else palette.muted
        lines = body_text.rstrip("\n").splitlines()
        total = len(lines)
        if total > BOX_BODY_LINES:          # 只显示前 N 行（不再自留 head/tail）
            lines = lines[:BOX_BODY_LINES] + [
                f"...[已省略 {total - BOX_BODY_LINES} 行，共 {total} 行]..."
            ]
        block = Text()
        for i, ln in enumerate(lines):
            if i:
                block.append("\n")
            block.append(("↳ " if i == 0 else "  ") + ln, style=style)
        out.append(Padding(block, (0, BOX_INSET)))
    return out


def _box(
    palette: Theme,
    body: Any,
    *,
    role: str = "system",
    border_role: str = "",
    lean: bool = False,
) -> list[Panel | Padding]:
    """渲染一条输出 → 要写进消息流的 renderable（**整个视图层唯一的样式决策点**）。

    `body` / `role` / `border_role` 的语义见 `_panel`；`lean=True` 且是**工具活动**
    （`role` 为 `tool_call` / `tool_result`）→ `_lean` 压成单行，其余（消息类）恒套盒子；
    手动 !cmd 由调用方传 `lean=False` 显式豁免（见 PieApp._notify）。
    """
    if lean and role in ("tool_call", "tool_result"):
        return _lean(palette, body, role=role, border_role=border_role)
    return _panel(palette, body, role=role, border_role=border_role)


# ---- 简洁单行（lean）----
#
# 单行摘要取哪个参数（未列出的工具退回参数文本）：规则用在 _lean 里，不在 App 侧。
# 状态标记（✅/❌/⏹）的字形在主题里（Theme.icon_ok / icon_error / icon_cancelled，
# 与盒子模式结果框的标题图标同一套，见 palette.lean_mark / palette.role_icon）。
LEAN_SUMMARY_KEYS = {"read": "path", "write": "path", "edit": "path", "shell": "command"}


def _help_text() -> str:
    """由 PALETTE_COMMANDS 生成 /help 文案：补全候选是唯一事实来源，避免两处漂移。"""
    rows = " | ".join(f"{cmd} {desc}" for cmd, desc in PALETTE_COMMANDS if " " not in cmd)
    return (
        f"{rows}\n"
        "!cmd 直接执行 shell（不经过 LLM，不进会话上下文；输入框变橙色即 shell 模式，"
        "/stop 或 Esc 可终止）\n"
        "Ctrl+V / Ctrl+G / /paste 把剪贴板里的图片存成文件并插入路径（终端截走 Ctrl+V 时用后两者；"
        "macOS 截图要 ⌃⇧⌘4 才进剪贴板，Finder 里 ⌘C 图片文件也可以）；"
        "以 / 开头但不是已知命令的输入按普通消息发出\n"
        "鼠标拖动日志可复制文本（按源文本复制，长行不会断行）"
    )


class CommandPalette(Static):
    """/ 命令补全候选面板：输入以 / 开头时显示，位于输入框上方。"""


class MessageSubmitted(Message):
    """多行消息输入框提交（Enter）。"""

    def __init__(self, text: str) -> None:
        super().__init__()
        self.text = text


class PieTextArea(TextArea):
    """多行消息输入框：Enter 提交，Shift+Enter 换行（支持多行粘贴）；
    Tab 接受命令补全，↑/↓ 切换候选，Esc 隐藏面板 / 取消当前任务。"""

    BINDINGS = [
        *TextArea.BINDINGS,
        Binding("tab", "palette_accept", "接受命令补全", show=False),
        # Esc：补全面板开着先收起，否则等价 /stop（取消当前回合 / !shell）
        Binding("escape", "palette_hide", "隐藏补全 / 取消当前任务", show=False),
    ]

    async def action_paste(self) -> None:
        """Ctrl+V：剪贴板里是图片 → 落盘后插入路径；否则走原本的文本粘贴。

        覆盖 TextArea.action_paste（其绑定 ctrl+v/super+v 指向 "paste"），所以两种入口
        （按键、程序化 run_action）行为一致。终端若自己截了 Ctrl+V（Windows Terminal
        就把它绑到终端侧粘贴，图片内容到不了应用）→ 用 /paste 命令，见 PieApp._command。

        grabclipboard() 是阻塞调用（Windows 本地 API / macOS 起 osascript / Linux 起子进程）
        → 丢线程池，别冻住 UI。
        """
        if self.read_only:
            return
        path = await asyncio.to_thread(clipboard.grab_image_path)
        if path is None:
            super().action_paste()
            return
        self.insert(str(path))

    async def _on_key(self, event: events.Key) -> None:
        if event.key == "enter":
            event.stop()
            event.prevent_default()
            app = self.app
            if app.palette_displayed() and not app.is_palette_command(self.text):
                # 半截命令：先接受候选框里高亮的命令，再提交完整命令
                app.palette_accept()
                app.palette_hide()
            self.post_message(MessageSubmitted(self.text))
            return
        if event.key in ("shift+enter", "ctrl+j"):
            event.stop()
            event.prevent_default()
            self.insert("\n")
            return
        await super()._on_key(event)

    def action_palette_accept(self) -> None:
        app = self.app
        if app.palette_displayed():
            app.palette_accept()
        else:
            app.action_focus_next()

    def action_palette_hide(self) -> None:
        self.app.action_escape()

    def action_cursor_up(self, select: bool = False) -> None:
        if self.app.palette_displayed():
            self.app.palette_up()
        else:
            super().action_cursor_up(select)

    def action_cursor_down(self, select: bool = False) -> None:
        if self.app.palette_displayed():
            self.app.palette_down()
        else:
            super().action_cursor_down(select)


# ---- 日志区控件：RichLog + 鼠标框选复制 ----
#
# 复制取「源文本」而不是显示行：显示行是 Rich 软换行的产物（长行断成多行、换行点空格还会被
# 吃掉），按显示行拼接会把一行复制成多行。SelectableRichLog 每次 write 记下源文本与
# 「显示行 → 源文本字符区间」的对齐关系，复制时按字符区间切源文本（见其类 docstring）。
# 对齐/切片本身是纯函数，测试见 tests/test_tui.py。


class _CopyRow(NamedTuple):
    """一条显示行拆出的可复制内容：kind=content 时 text 是去掉盒边框/填充后的正文，
    x0 是正文在该显示行里的起始单元格（单元格 → 字符换算用）。"""

    kind: str  # "content" | "border"
    text: str
    x0: int


class _CopyEntry(NamedTuple):
    """一次 log.write 的复制记录：源文本 + 每行内容在源文本里的字符区间。

    spans 与显示行一一对应（边框行为 None），每项为 (start, end, off)：
    行内容在 src 中的 [start, end)，off 是该行开头被忽略的显示装饰字符数
    （如引用的 “▌ ” 续行装饰）；整体为 None 表示对不上源文本，
    复制时该次写入回退成「按显示行拼接」。"""

    row: int  # 起始显示行（RichLog.lines 下标）
    count: int  # 占用的显示行数
    src: str
    spans: list[tuple[int, int, int] | None] | None


# 「复制源」宽渲染宽度：足够宽 → 不软换行，拿到每条逻辑行的完整文本
_COPY_RENDER_WIDTH = 4096
_COPY_CONSOLE = Console(width=_COPY_RENDER_WIDTH, force_terminal=False)


def _wide_text(renderable: Any) -> str | None:
    """宽渲染一个 renderable，取它的纯文本逻辑行（不软换行）。失败返回 None。"""
    options = _COPY_CONSOLE.options.update_width(_COPY_RENDER_WIDTH)
    try:
        segments = _COPY_CONSOLE.render(renderable, options)
    except Exception:
        return None
    rows = ["".join(seg.text for seg in line) for line in Segment.split_lines(segments)]
    return "\n".join(row.rstrip() for row in rows)


def _copy_source(content: Any) -> str | None:
    """取写入内容对应的可复制源文本：[Panel/Padding → 剥掉盒子与留白后的内容，Text → .plain，Markdown → 渲染后纯文本]。

    Markdown 不走 .markup：渲染会重排（去围栏、加缩进、合并段落），源文本与显示行对不上；
    改用宽渲染纯文本——既是屏幕上看到的文字，每条逻辑行又保持完整（长行复制不会断行）。
    拿不到（其它渲染对象）返回 None → 该次写入不记录，复制回退按显示行拼接。"""
    obj = content
    while isinstance(obj, (Panel, Padding)):   # 盒子/留白：源文本取里面真正的内容
        obj = obj.renderable
    if isinstance(obj, Text):
        # 显示时 Rich 会把 tab 展开成空格（tab_size=8）；源文本跟着展开才与显示行对得上
        plain = obj.copy()
        plain.expand_tabs()
        return plain.plain
    if isinstance(obj, str):
        return obj
    if isinstance(obj, Markdown):
        return _wide_text(obj)
    return None


def _split_log_row(strip: Strip) -> _CopyRow:
    """显示行 → 可复制内容：剥掉 Panel 左右边框与行首空白，上下边框行归为 border。

    行首空白（Panel padding=(0,1) + Rich 渲染产生的悬挂缩进/居中填充）一律不算内容：
    复制按源文本切，缩进以源文本为准；它只计入 x0，供单元格 → 字符换算。"""
    text = strip.text
    x0 = 0
    if text.startswith("│"):
        text = text[1:]
        x0 += 1
    if text.endswith("│"):
        text = text[:-1]
    text = text.rstrip()
    if text.startswith(("╭", "╰")) and text.endswith(("╮", "╯")):
        return _CopyRow("border", "", 0)
    if not text.strip():
        return _CopyRow("content", "", x0)
    body = text.lstrip()
    return _CopyRow("content", body, x0 + cell_len(text) - cell_len(body))


# 显示装饰：Rich 渲染 Markdown 引用时，每条显示行开头都会重复一个 “▌”
# （源文本里只有首个逻辑行有）——匹配时允许忽略它，见 _align_spans
_ROW_DECOR = "▌"


def _match_row(src: str, text: str, pos: int) -> tuple[int, int] | None:
    """从 src 的 pos 起（允许跳过剩下的空白）匹配 text → (start, end)；对不上返回 None。"""
    if not text:
        return (pos, pos)
    p = pos
    while not src.startswith(text, p):
        if p >= len(src) or src[p] not in " \t\n":
            return None
        p += 1
    return (p, p + len(text))


def _align_spans(src: str, rows: list[_CopyRow]) -> list[tuple[int, int, int] | None] | None:
    """把显示行内容对齐回源文本，得到每行内容在 src 里的字符区间 (start, end, off)。

    逐行在 src 里顺序匹配（允许跳过 Rich 在换行点吃掉/回填的空白）；若整行匹配不上，
    再试去掉行首显示装饰（引用续行的 “▌ ”）；还不行就返回 None
    （该次写入复制回退按显示行拼接），要求所有行都对上、且 src 尾部只剩空白。

    为什么要对齐：显示行是软换行的产物，Rich 在盒内换行点会直接吃掉那个空格
    （实测 'aaaaaa bbbb cccc' + 'dddd…'），按显示行拼接既多出换行又丢空格；
    按字符区间切 src 才能连同被吃掉的空格一起复原。
    """
    spans: list[tuple[int, int, int] | None] = []
    pos = 0
    for row in rows:
        if row.kind != "content":
            spans.append(None)
            continue
        if not row.text:
            spans.append((pos, pos, 0))
            continue
        text, off = row.text, 0
        hit = _match_row(src, text, pos)
        if hit is None and text.startswith(_ROW_DECOR):
            text = text.lstrip(_ROW_DECOR).lstrip()
            off = len(row.text) - len(text)
            hit = _match_row(src, text, pos)
        if hit is None:
            return None
        spans.append((hit[0], hit[1], off))
        pos = hit[1]
    if src[pos:].strip():
        return None
    return spans


def _cell_to_char(text: str, cell: int) -> int:
    """内容文本里第 cell 个单元格之前的字符数（宽字符按占位算）。"""
    if cell <= 0:
        return 0
    used = 0
    for i, ch in enumerate(text):
        used += cell_len(ch)
        if used >= cell:
            return i + 1
    return len(text)


class SelectableRichLog(RichLog):
    """RichLog + 鼠标框选复制：按住左键拖动选择，松开自动复制到剪贴板。

    复制取「源文本」而非显示行：长行在盒内会软换行成多行、换行点空格还会被吃掉，
    按显示行拼接会把一行复制成多行（且丢空格）。所以每次 write 记下源文本与
    「显示行 → 源文本字符区间」的对齐关系，复制时按字符区间切源文本。
    """

    def __init__(
        self,
        *args: Any,
        selection_style: Style | None = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(*args, **kwargs)
        self._selection_style = selection_style or Style()
        self._selecting = False
        self._sel_start: tuple[int, int] | None = None
        self._sel_end: tuple[int, int] | None = None
        self._entries: list[_CopyEntry] = []

    # ---- 写入：记录复制映射 ----

    def write(self, content: Any, *args: Any, **kwargs: Any) -> Any:
        """写入一条内容，并记录它的「源文本 + 显示行对齐」。"""
        before = len(self.lines)
        result = super().write(content, *args, **kwargs)
        if len(self.lines) == before:
            # 尺寸未知时 RichLog 会延迟渲染（on_resize 时再 write 一遍）→ 此处不记录
            return result
        src = _copy_source(content)
        if src is None:
            return result
        rows = [_split_log_row(self.lines[i]) for i in range(before, len(self.lines))]
        self._entries.append(
            _CopyEntry(before, len(self.lines) - before, src, _align_spans(src, rows))
        )
        return result

    def clear(self) -> Any:
        result = super().clear()
        self._entries.clear()
        return result

    # ---- 鼠标事件 ----

    def on_mouse_down(self, event: events.MouseDown) -> None:
        if event.button != 1:  # 仅左键
            return
        self._selecting = True
        self._sel_start = self._cell_at(event)
        self._sel_end = self._sel_start
        self.capture_mouse()
        self.refresh()

    def on_mouse_move(self, event: events.MouseMove) -> None:
        if not self._selecting or self._sel_start is None:
            return
        self._sel_end = self._cell_at(event)
        self.refresh()

    def on_mouse_up(self, event: events.MouseUp) -> None:
        if event.button != 1 or not self._selecting:
            return
        self._selecting = False
        self.release_mouse()
        self._sel_end = self._cell_at(event)
        text = self._selected_text()
        self._sel_start = self._sel_end = None
        self.refresh()
        if text:
            self.app.copy_to_clipboard(text)
            self.app.notify(f"已复制 {len(text)} 字符到剪贴板")

    # ---- 坐标与文本 ----

    def _cell_at(self, event: events.MouseEvent) -> tuple[int, int]:
        region = self.scrollable_content_region
        col = max(0, int(event.screen_x or 0) - region.x) + self.scroll_offset.x
        row = max(0, int(event.screen_y or 0) - region.y) + self.scroll_offset.y
        return row, col

    def _entry_at(self, row: int) -> _CopyEntry | None:
        i = bisect_right(self._entries, row, key=lambda e: e.row) - 1
        if i < 0:
            return None
        entry = self._entries[i]
        return entry if row < entry.row + entry.count else None

    def _slice_of_row(self, row: int, a: int, b: int | None) -> tuple[str, int, int] | None:
        """显示行 [a, b) 单元格对应的源文本切片（src, start, end）。
        无映射（未记录 / 边框行 / 对齐失败）返回 None，由调用方回退按显示行拼接。"""
        entry = self._entry_at(row)
        if entry is None or entry.spans is None:
            return None
        span = entry.spans[row - entry.row]
        if span is None:
            return None
        start, _end, off = span
        info = _split_log_row(self.lines[row])
        text = info.text[off:]  # off：被忽略的显示装饰字符数（“▌ ” 等）
        x0 = info.x0 + cell_len(info.text[:off])
        begin = start + _cell_to_char(text, a - x0)
        end = start + (len(text) if b is None else _cell_to_char(text, b - x0))
        return (entry.src, begin, max(begin, end))

    def _row_fragment(self, row: int, a: int, b: int | None) -> str:
        """回退路径：按显示行裁剪 + 清洗（无映射时的老行为）。
        整行是盒边框（哪怕只选到半截）→ 空串，避免把 `╰───` 这类残留复制进去。"""
        if row < 0 or row >= len(self.lines):
            return ""
        if _split_log_row(self.lines[row]).kind == "border":
            return ""
        end = self.lines[row].cell_length if b is None else min(b, self.lines[row].cell_length)
        if end <= a:
            return ""
        return self._clean_copied_line(self.lines[row].crop(a, end).text)

    def _selected_text(self) -> str:
        selection = self._sel_range()
        if selection is None:
            return ""
        r1, c1, r2, c2 = selection
        pieces: list[str] = []
        acc: tuple[str, int, int] | None = None  # 正在累积的同一次写入的源文本切片

        def flush() -> None:
            nonlocal acc
            if acc is not None:
                pieces.append(acc[0][acc[1] : acc[2]])
                acc = None

        for row in range(r1, r2 + 1):
            a = c1 if row == r1 else 0
            b = c2 if row == r2 else None
            part = self._slice_of_row(row, a, b)
            if part is None:
                flush()
                fragment = self._row_fragment(row, a, b)
                if fragment:
                    pieces.append(fragment)
                continue
            # 同一次写入的相邻显示行合并成一个切片：源文本本来就连续，合并后
            # 连被软换行吃掉的空格/换行一起还原（这正是长行复制不断行的关键）
            if acc is not None and acc[0] is part[0] and part[1] >= acc[2]:
                acc = (acc[0], acc[1], max(acc[2], part[2]))
            else:
                flush()
                acc = part
        flush()
        # 去掉首尾空行（Panel 顶/底边框与填充产生的空行）
        return "\n".join(pieces).strip("\n")

    @staticmethod
    def _clean_copied_line(text: str) -> str:
        """清理复制行：去掉 rich Panel 边框（左右 │、顶/底 ╭╮╰╯ 标题框）
        与显示填充空白，只留内容本身（保留内容自带的缩进）。"""
        # Panel 左右边框
        if text.startswith("│"):
            text = text[1:]
        if text.endswith("│"):
            text = text[:-1]
        text = text.rstrip()
        # 顶/底边框行（含标题行，如 ╭─ shell ─╮ / ╰─────╯）无内容
        if text.startswith(("╭", "╰")) and text.endswith(("╮", "╯")):
            return ""
        # 纯空白行（Panel 内部填充）
        if not text.strip():
            return ""
        # Panel padding=(0,1)：去掉行首一个填充空格（内容自身的缩进保留）
        if text.startswith(" "):
            text = text[1:]
        return text

    # ---- 渲染：选中区间叠加高亮 ----

    def _sel_range(self) -> tuple[int, int, int, int] | None:
        """规范化的选中区间 (r1, c1, r2, c2)（按行/列排序，含端点）；无选中返回 None。"""
        if self._sel_start is None or self._sel_end is None:
            return None
        r1, c1 = self._sel_start
        r2, c2 = self._sel_end
        if (r1, c1) > (r2, c2):
            return (r2, c2, r1, c1)
        return (r1, c1, r2, c2)

    def render_line(self, y: int) -> Strip:
        strip = super().render_line(y)
        selection = self._sel_range()
        if selection is None:
            return strip
        r1, c1, r2, c2 = selection
        row = self.scroll_offset.y + y
        if row < r1 or row > r2:
            return strip
        start = c1 if row == r1 else 0
        end = c2 if row == r2 else strip.cell_length
        return self._apply_selection(strip, start, end)

    def _apply_selection(self, strip: Strip, start: int, end: int) -> Strip:
        """给 [start, end) 单元格区间叠加高亮。

        按字符边界切分而非按单格切开：宽字符（中文/emoji）整体保留，
        不会像 Segment.split_cells(1) 那样被替换成空格而“消失”。
        """
        if start >= end or not strip.text:
            return strip
        segs: list[Segment] = []
        pos = 0
        for seg in strip:
            cells = seg.cell_length
            seg_start, seg_end = pos, pos + cells
            pos = seg_end
            if cells <= 0 or seg_end <= start or seg_start >= end:
                segs.append(seg)
                continue
            a = max(seg_start, start)
            b = min(seg_end, end)
            before = after = ""
            selected = ""
            cur = seg_start
            for ch in seg.text:
                w = cell_len(ch)
                if cur + w <= a:
                    before += ch
                elif cur >= b:
                    after += ch
                else:
                    selected += ch
                cur += w
            if before:
                segs.append(Segment(before, seg.style, seg.control))
            if selected:
                sel_style = (
                    (seg.style + self._selection_style)
                    if seg.style is not None
                    else self._selection_style
                )
                segs.append(Segment(selected, sel_style, seg.control))
            if after:
                segs.append(Segment(after, seg.style, seg.control))
        return Strip(segs, strip.cell_length)


class PieApp(App):
    """聊天主界面：Header + 消息流 + 流式区 + 状态栏 + 补全面板 + 底部输入。"""

    TITLE = "pie"

    BINDINGS = [
        *App.BINDINGS,  # ctrl+q 退出 / ctrl+c help_quit 等
        # Esc 焦点在输入框时由 PieTextArea 的绑定处理（同一动作），
        # 焦点在别处（如日志区）时由这里兜底。
        Binding("escape", "escape", "取消当前任务", show=False),
        # Ctrl+V 的兜底：终端把 ctrl+v 截给自己时（Windows Terminal 默认如此），
        # 按键根本到不了应用 → ctrl+g 没被终端/Textual/本应用占用，拿它当第二入口。
        Binding("ctrl+g", "paste_image", "粘贴剪贴板图片路径", show=False),
    ]

    def __init__(self, session: Session, initial_prompt: str | None = None) -> None:
        super().__init__()
        self.session = session
        self.initial_prompt = initial_prompt
        # 终端背景明暗：主题族（如 catppuccin）据此选深/浅变体，Textual 主题也跟着选 ansi-dark/light
        self.dark_bg = detect_dark_background()
        self.palette = get_theme(session.config.theme, dark=self.dark_bg)
        # 简洁模式（Config.tui.lean）：只给 user / assistant 套盒子，工具调用/结果压成单行
        self.lean = bool(getattr(getattr(session.config, "tui", None), "lean", False))
        self.CSS = build_css(self.palette)
        self._turn_worker: Worker | None = None
        self._shell_worker: Worker | None = None
        self._cancel_event: asyncio.Event | None = None
        self._palette_index = 0
        # 流式区状态（当前回合进行中的增量）
        # 模型 thinking 只维护尾部窗口：Static 不滚动、内容 top 对齐，若把完整
        # reasoning 塞进去，超过 max-height 的新行全部落在可视区外 → 表现成
        # “输出几行后就不更新了”。改成增量维护尾部窗口行，与 !shell 一致。
        self._reasoning_lines: list[str] = []  # 已完成行（尾部窗口，≤ STREAM_MAX_LINES）
        self._reasoning_tail = ""  # 尚未换行的累积片段（跨 chunk 拼接）
        self._stream_tool: list[str] = []  # 仅 !shell 模式的逐行实时输出
        self._assistant_text = ""  # 当前回合 assistant 正文增量（content_delta 累积），
        # 用于 #assistant-stream 实时渲染，answer 固化后清空
        self._content_streaming = False  # 当前回合是否已进入 content 阶段（首次时清 reasoning）

    def compose(self) -> ComposeResult:
        yield SelectableRichLog(
            selection_style=Style(bgcolor=self.palette.accent, color=self.palette.accent_text),
            highlight=True,
            markup=True,
            wrap=True,
            # min_width=0：RichLog 默认 78，会把渲染宽度抬到 78，而本控件的可滚动内容区
            # （减去边框/内边距/滚动条）其实更窄 → 每行末尾裁掉几列：盒子的右边框看不见、
            # 简洁模式超长命令的尾巴被截掉。置 0 后渲染宽度 = min(内容宽度, 内容区宽度)。
            min_width=0,
            id="log",
        )
        with VerticalScroll(id="assistant-stream"):
            yield Static("", id="assistant-panel")
        yield Static("", id="stream")
        yield CommandPalette("", id="palette")
        with Horizontal(id="input-bar"):
            yield PieTextArea(
                placeholder="输入消息（! 开头直接执行 shell，/stop 或 Esc 取消当前任务，/ 显示命令补全，Shift+Enter 换行）",
                id="input",
                tab_behavior="focus",
                highlight_cursor_line=False,
            )
            yield Button("▶", id="send-btn", variant="default")
        with Horizontal(id="foot-bar"):
            yield Static("", id="meta")
            yield Static("", id="status")

    def on_mount(self) -> None:
        # 浅色变体把 Markdown 代码的硬编码黑底换成浅色（最小覆盖；深色变体为空字典 → 不动）。
        markdown_styles = self.palette.markdown_styles()
        if markdown_styles:
            self.console.push_theme(RichTheme(markdown_styles, inherit=True))
        # 用 ansi-dark / ansi-light 主题：background=ansi_default + ansi=True（native ANSI），
        # 背景输出 `49`（终端默认背景）→ 透明，露出终端窗口背景色；
        # 默认主题 ansi=False 会经 ANSIToTruecolor 把 default 背景映射成主题色（不透明）。
        # 浅色终端用 ansi-light（其 ansi-foreground/background 假定浅底），与 palette 保持一致。
        self.theme = "ansi-light" if self.dark_bg is False else "ansi-dark"
        if self.session.windows:
            self._notify(f"已归档 {len(self.session.windows)} 个历史窗口块（~/.pie/windows/）")
        self._render_history()
        self._update_meta()
        self.query_one("#input", PieTextArea).focus()
        send_btn = self.query_one("#send-btn", Button)
        send_btn.can_focus = False  # 右侧按钮不抢焦点，避免干扰输入
        self._update_send_button()
        self._update_status()
        # 启动预热：后台发一次极小请求（max_tokens=1），把 openai client + HTTPS 连接
        # 在启动阶段建好，避免首次真实发送阻塞事件循环数秒（WSL 首次建连慢）。
        self.run_worker(
            self._prewarm(), group="prewarm", exclusive=False, exit_on_error=False
        )
        # 启动拉取可用模型列表（供 /model 切换/补全）：后台异步，不阻塞 UI；
        # 静默（notify=False）——不打扰启动界面，/model 查看、/model refresh 可主动触发
        self.run_worker(
            self._fetch_models(), group="prewarm", exclusive=False, exit_on_error=False
        )
        if self.initial_prompt:
            self._submit(self.initial_prompt)

    async def _fetch_models(self, notify: bool = False) -> None:
        """拉取可用模型列表到 session.available_models（后台 worker，失败不打断）。

        notify=True（/model refresh 主动触发）时才在消息流里报告结果；启动时静默。
        """
        try:
            models = await self.session.fetch_models()
        except NotImplementedError:
            pass  # 后端不支持列出模型（自定义 LLM）→ 静默，/model 走手动指定
        except Exception as e:
            if notify:
                self._notify(
                    f"获取可用模型列表失败: {e}（/model refresh 重试，或 /model <id> 直接切换）"
                )
        else:
            if notify:
                cur = self.session.config.model
                hit = "" if cur in models else "（不在列表中）"
                self._notify(
                    f"可用模型 {len(models)} 个，当前: {cur}{hit}；/model 查看，/model <id> 切换"
                )

    async def _prewarm(self) -> None:
        """预热模型连接：极小请求（~几 token，成本可忽略），静默失败不影响后续。

        OpenAILLM 懒创建 AsyncOpenAI/httpx 连接池，首次真实请求在事件循环里
        同步完成 TLS 握手 + 建连，WSL 下可达 ~6s → UI 冻结。预热后连接池
        keep-alive 复用，首次发送不再卡。非 OpenAILLM 后端（无 _client）跳过。
        """
        llm = getattr(self.session, "llm", None)
        make_client = getattr(llm, "_client", None)
        if make_client is None:
            return
        try:
            client = make_client()
            await asyncio.wait_for(
                client.chat.completions.create(
                    model=llm.model,
                    messages=[{"role": "user", "content": "ping"}],
                    max_tokens=1,
                ),
                timeout=8,
            )
        except Exception:
            pass  # 预热失败静默：真实请求自带重试，不受影响

    def _update_meta(self) -> None:
        """顶栏元信息：模型 / 思考深度 / 目录（/thinking / /model 切换后刷新）。"""
        self.query_one("#meta", Static).update(
            f"{self.session.config.model} {self.session.config.reasoning_effort}"
            f" · {Path.cwd()}"
        )

    def _update_status(self) -> None:
        rep = self.session.usage_report().splitlines()
        # 跳过“会话文件”行（路径长，不适合状态栏），仍取首/尾两行摘要
        lines = [ln for ln in rep if not ln.startswith("会话文件")]
        context = lines[0].split("：", 1)[-1].strip()
        self.query_one("#status", Static).update(f"{context}")

    # ---- 渲染：所有输出都经 _notify 落到 #log ----
    #
    # 分层：**样式决策全在模块级纯函数 `_box`**（它再分派到 _panel / _lean），
    # PieApp 侧只有一个出口 `_notify`（把 _box 的产物写进 #log，
    # 也是 tui.py 里唯一的 `log.write`）。实时事件、resume 历史回放、命令反馈全走它，
    # 所以不存在「某个入口另写一份样式」。

    def _notify(self, body: Any, role: str = "system", **kwargs: Any) -> None:
        """往消息流写一条内容（#log 的**唯一写入口**）。

        盒子还是简洁单行、图标、正文截断全在 _box 里定（role 兼做角色与工具）；这里只负责落写。
        `lean` 默认取 App 的简洁模式开关，手动 !cmd 传 `lean=False` 显式豁免（它是用户主动执行，
        输出本身就是要看的，任何模式都套盒子）。
        """
        kwargs.setdefault("lean", self.lean)
        log = self.query_one("#log", RichLog)
        for r in _box(self.palette, body, role=role, **kwargs):
            log.write(r)

    def _render_tool_call(self, name: str, args: Any) -> None:
        """渲染一条工具调用（历史传 JSON 字符串，实时事件传 dict，渲染层自己归一）。"""
        self._notify({"name": name, "arguments": args}, role="tool_call")

    def _render_tool_result(self, name: str, content: str, *, arguments: Any = None) -> None:
        """渲染一条工具结果（实时事件与 resume 历史共用）。

        `arguments` 是配对的那次调用参数——简洁模式的结果行要从中推出摘要（正文里没有它，
        所以随 body 一起交给渲染层）。
        """
        self._notify({"name": name, "arguments": arguments, "content": content}, role="tool_result")

    def _flush_assistant_text(self) -> None:
        """把流式累积的正文固化成 #log 盒子（content 与 tool_call 并存时先固化）。"""
        if not self._assistant_text:
            return
        self._notify(self._assistant_text, role="assistant")
        self._assistant_text = ""
        self._content_streaming = False  # 下一轮模型输出时重新清 reasoning
        self._render_assistant_stream()

    def _render_history(self) -> None:
        """resume 时把已有对话历史渲染进消息流（压缩指针展开为完整转录）。

        与实时事件渲染保持一致的盒子样式：user → "你"，assistant → "pie"，
        tool_calls → ⚙/工具图标，tool → ✅/❌/⏹（执行结果）；system（system prompt / 窗口摘要）不显示。
        标题与图标都由 role 决定，见 Theme.role_icon / Theme.result_icon。
        简洁模式下工具行需要「调用摘要」，用 tool_call_id 把工具结果与它对应的
        调用参数配起来（历史里结果按调用顺序排在后面）。
        """
        calls: dict[str, tuple[str, Any]] = {}  # tool_call_id → (工具名, 参数)，供结果行取摘要
        for d in self.session.full_history():
            role = d.get("role")
            if role == "system":
                continue
            content = content_text(d.get("content"))
            if role == "user":
                self._notify(content, role="user")
            elif role == "assistant":
                tool_calls = d.get("tool_calls")
                if tool_calls:
                    if content:  # content+tool_call 并存：先固化正文再写工具调用框
                        self._notify(content, role="assistant")
                    for tc in tool_calls:
                        fn = tc.get("function") or {}
                        name = fn.get("name") or "工具"
                        args = fn.get("arguments") or ""
                        calls[tc.get("id") or ""] = (name, args)
                        self._render_tool_call(name, args)
                else:
                    self._notify(content, role="assistant")
            elif role == "tool":
                name, args = calls.pop(
                    d.get("tool_call_id") or "", (d.get("tool_name") or "工具", None)
                )
                self._render_tool_result(name, d.get("content") or "", arguments=args)
            else:
                self._notify(content, role="system")

    # ---- 命令补全 ----

    def _palette_matches(self) -> list[tuple[str, str]]:
        value = self.query_one("#input", PieTextArea).text
        if not value.startswith("/"):
            return []
        if value.startswith("/model "):  # 已带前缀 → 展开可用模型候选（Tab/上下键补全）
            prefix = value[len("/model "):]
            cur = self.session.config.model
            return [
                (f"/model {m}", "← 当前" if m == cur else "切换模型")
                for m in (self.session.available_models or [])
                if m.startswith(prefix)
            ]
        if value.startswith("/thinking "):  # 同 /model：前缀补全思考级别，当前级标注 ←
            prefix = value[len("/thinking "):]
            cur = self.session.config.reasoning_effort
            return [
                (f"/thinking {lv}", "← 当前" if lv == cur else "切换思考深度")
                for lv in REASONING_LEVELS
                if lv.startswith(prefix)
            ]
        return [c for c in PALETTE_COMMANDS if c[0].startswith(value)]

    def _render_palette(self) -> None:
        palette = self.query_one("#palette", CommandPalette)
        matches = self._palette_matches()
        if not matches:
            self._palette_index = 0
            palette.display = False
            return
        shown = matches[:7]
        self._palette_index = min(self._palette_index, len(shown) - 1)
        lines = []
        for i, (cmd, desc) in enumerate(shown):
            if i == self._palette_index:
                lines.append(f"[bold {self.palette.accent_text} on {self.palette.accent}] {cmd} [/][dim] — {desc}[/]")
            else:
                lines.append(f" {cmd} [dim]— {desc}[/]")
        if len(matches) > len(shown):
            lines.append(f"[dim]  … 还有 {len(matches) - len(shown)} 个候选[/]")
        palette.update("\n".join(lines))
        palette.display = True

    def palette_displayed(self) -> bool:
        return self.query_one("#palette", CommandPalette).display

    def is_palette_command(self, value: str) -> bool:
        """输入值是否已经是完整命令（此时回车应直接提交，不再替换）。"""
        return any(cmd == value for cmd, _ in PALETTE_COMMANDS)

    def is_known_command(self, text: str) -> bool:
        """首词是不是已知命令名 —— 不是就当**普通文本**发出去。

        不能单凭 `/` 开头就认定是命令：粘贴进来的绝对路径（`/home/.../x.png`）
        是最常见的误伤——那样只能收到一句「未知命令」而丢消息。
        """
        name = text.partition(" ")[0]
        return any(name == cmd.partition(" ")[0] for cmd, _ in PALETTE_COMMANDS)

    def on_text_area_changed(self, event: TextArea.Changed) -> None:
        try:
            self._render_palette()
        except Exception:
            pass  # 组件正在卸载时忽略
        self._update_input_border()

    def _update_send_button(self) -> None:
        """右侧按钮两用：空闲=发送，处理中（busy）=停止（等价 /stop）。"""
        try:
            btn = self.query_one("#send-btn", Button)
        except Exception:
            return
        if self._busy():
            btn.label = "■" # ⏹
            btn.add_class("busy")
        else:
            btn.label = "▶"
            btn.remove_class("busy")

    async def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id != "send-btn":
            return
        if self._busy():
            self._command("/stop")
        else:
            inp = self.query_one("#input", PieTextArea)
            text = inp.text
            if text.strip():
                await self.on_message_submitted(MessageSubmitted(text))

    def _update_input_border(self) -> None:
        """输入以 ! 开头时切换 shell 模式边框（tool_call 橙色），否则恢复正常。"""
        inp = self.query_one("#input", PieTextArea)
        if inp.text.strip().startswith("!"):
            inp.add_class("shell-mode")
        else:
            inp.remove_class("shell-mode")

    def palette_up(self) -> None:
        if not self.query_one("#palette", CommandPalette).display:
            return
        if self._palette_index > 0:
            self._palette_index -= 1
            self._render_palette()

    def palette_down(self) -> None:
        if not self.query_one("#palette", CommandPalette).display:
            return
        matches = self._palette_matches()
        if matches and self._palette_index < len(matches) - 1:
            self._palette_index += 1
            self._render_palette()

    def palette_accept(self) -> None:
        palette = self.query_one("#palette", CommandPalette)
        if not palette.display:
            return
        matches = self._palette_matches()
        if not matches:
            palette.display = False
            return
        cmd = matches[self._palette_index][0]
        inp = self.query_one("#input", PieTextArea)
        inp.text = cmd
        lines = inp.text.splitlines() or [""]
        inp.move_cursor((len(lines) - 1, len(lines[-1])))
        inp.focus()

    def palette_hide(self) -> None:
        self.query_one("#palette", CommandPalette).display = False

    def action_escape(self) -> None:
        """Esc：补全面板开着就先收起；否则若有任务在跑，等价 /stop 取消。"""
        if self.palette_displayed():
            self.palette_hide()
        elif self._busy():
            self._stop()

    async def action_paste_image(self) -> None:
        """Ctrl+G：粘贴剪贴板图片（无图就提示）—— 与 `/paste` 同一个实现。"""
        await self._paste_image(notify=True)

    # ---- 消息 ----

    async def on_message_submitted(self, event: MessageSubmitted) -> None:
        text = event.text.strip()
        self.query_one("#input", PieTextArea).text = ""
        self._update_input_border()
        if not text:
            return
        if text.startswith("/") and self.is_known_command(text):
            self._command(text)
        elif text.startswith("!"):
            self._run_shell(text[1:].strip())
        else:
            if self._busy():
                if self._cancelling():
                    # 正在取消收尾：等它结束（cancel 路径很快），避免新旧回合并发写历史
                    await self._turn_worker.wait()  # type: ignore[union-attr]
                    if self._busy():
                        self._notify("正在取消中，请稍候…")
                        return
                else:
                    self._notify("正在处理中，输入 /stop 或按 Esc 可取消")
                    return
            self._submit(text)

    def _busy(self) -> bool:
        """是否有 worker 正在跑（回合 / shell）。"""
        for w in (self._turn_worker, self._shell_worker):
            if w is not None and w.is_running:
                return True
        return False

    def _cancelling(self) -> bool:
        """正在取消收尾：worker 还活着但已请求取消。"""
        return self._cancel_event is not None and self._cancel_event.is_set()

    def _stop(self) -> None:
        """取消当前正在执行的回合 / !shell（/stop 与 Esc 共用；已在取消中则忽略重复触发）。"""
        if not self._busy() or self._cancel_event is None:
            self._notify("当前没有正在执行的任务")
            return
        if self._cancel_event.is_set():
            return
        self._cancel_event.set()
        self._notify("已请求取消，正在终止…")

    async def _paste_image(self, notify: bool = False) -> None:
        """把剪贴板里的图片落成文件，路径插进输入框（Ctrl+G / `/paste`；Ctrl+V 走 PieTextArea）。"""
        path = await asyncio.to_thread(clipboard.grab_image_path)
        if path is None:
            if notify:
                self._notify("剪贴板里没有图片" + _NO_IMAGE_HINT, role="error")
            return
        inp = self.query_one("#input", PieTextArea)
        inp.insert(str(path))
        inp.focus()
        if notify:
            self._notify(f"已插入图片路径: {path}")

    def _command(self, text: str) -> None:
        cmd, _, arg = text.partition(" ")
        if cmd in ("/exit", "/quit"):
            self.exit()
        elif cmd == "/stop":
            self._stop()
        elif cmd == "/help":
            self._notify(_help_text())
        elif cmd == "/reset":
            self.session.reset()
            self._notify("已清空历史（保留 system prompt 与记忆）")
            self._safe_save()
            self._update_status()
        elif cmd == "/clear":
            self.session.clear_window()
            self._notify(f"已切换新窗口（归档 {len(self.session.windows)} 个历史窗口块，文件在 ~/.pie/windows/）")
            self._safe_save()
            self._update_status()
        elif cmd == "/paste":
            # 终端可能截走 Ctrl+V（Windows Terminal 的粘贴绑定），所以给命令入口 + Ctrl+G
            self.run_worker(
                self._paste_image(notify=True), group="paste", exclusive=False,
                exit_on_error=False,
            )
        elif cmd == "/compact":
            mode = arg.strip() or "auto"
            if mode not in ("auto", "tools", "turns"):
                self._notify(f"未知压缩模式: {mode}（/compact [tools|turns]）", role="error")
            else:
                stats = self.session.compact(mode=mode)
                if stats.get("skipped"):
                    self._notify(f"未压缩：{stats['skipped']}")
                else:
                    self._notify(
                        f"压缩完成：节省约 {stats['saved_tokens']:,} tokens"
                        f"（tools={stats['tools']}，turns={stats['turns']}）"
                    )
                self._safe_save()
        elif cmd == "/save":
            self.session.save(arg.strip() or None)
            self._notify(f"会话已保存: {self.session.path}")
        elif cmd == "/status":
            self._notify(self.session.usage_report())
        elif cmd == "/thinking":
            level = arg.strip().lower()
            if not level:  # 无参数 → 查看当前深度
                self._notify(
                    f"当前思考深度: {self.session.config.reasoning_effort}"
                    f"（可选: {' / '.join(REASONING_LEVELS)}）"
                )
            elif level not in REASONING_LEVELS:
                self._notify(
                    f"未知思考级别: {level}（可选: {' / '.join(REASONING_LEVELS)}）", role="error"
                )
            else:
                note = self.session.set_reasoning_effort(level)
                self._notify(f"思考深度: {level}（{note}，重启后仍生效）")
                self._update_meta()
        elif cmd == "/model":
            name = arg.strip()
            avail = self.session.available_models
            if not name:  # 无参数 → 查看当前模型 + 可用列表
                cur = self.session.config.model
                if avail:
                    lines = [f"当前: {cur}"] + [
                        f"  {m}  ←" if m == cur else f"  {m}" for m in avail
                    ]
                else:
                    lines = [
                        f"当前: {cur}",
                        "（可用模型列表未获取到：输入 /model <id> 直接切换，或 /model refresh 重新拉取）",
                    ]
                self._notify("\n".join(["可用模型"] + lines))
            elif name == "refresh":  # 重新拉取模型列表（异步 worker，不阻塞 UI）
                self._notify("正在重新拉取可用模型列表…")
                # notify=True：用户主动触发，成功/失败都要给反馈
                self.run_worker(
                    self._fetch_models(notify=True), group="prewarm", exclusive=False,
                    exit_on_error=False,
                )
            elif avail and name not in avail:  # avail 为空（=列表未获取到）时不拦，允许手动指定
                self._notify(
                    f"未知模型: {name}（/model 查看可用 {len(avail)} 个；输入 /model refresh 重新拉取）",
                    role="error",
                )
            else:
                note = self.session.set_model(name)
                self._notify(f"模型已切换: {name}（{note}），下个请求生效")
                self._update_meta()
        else:
            self._notify(f"未知命令: {cmd}（/help 查看）", role="error")
        self._update_status()

    def _safe_save(self) -> None:
        try:
            self.session.save()
        except OSError:
            pass

    # ---- 流式区（#stream）：模型生成中 / 工具执行中的实时输出 ----

    def _render_stream(self) -> None:
        """#stream：模型回合只流式显示 reasoning；!shell 模式显示逐行输出。
        reasoning / shell 行都是外部文本，用 Text 按字面渲染（不走 markup 解析，
        避免 `[xxx=...]` 片段触发 MarkupError）。正文与 agent 工具输出不进 stream。
        """
        stream = self.query_one("#stream", Static)
        if self._reasoning_lines or self._reasoning_tail:
            shown = list(self._reasoning_lines)
            if self._reasoning_tail:
                shown.append(self._reasoning_tail)
            stream.update(rich_text("\n".join(shown), "dim"))
            stream.display = True
        elif self._shell_worker is not None and self._stream_tool:
            shown = self._stream_tool[-STREAM_MAX_LINES:]
            stream.update(rich_text("\n".join(shown)))
            stream.display = True
        else:
            stream.update("")
            stream.display = False

    def _push_reasoning(self, delta: str) -> None:
        """增量维护 reasoning 尾部行窗口：跨 chunk 拼接未换行的片段，
        只保留最后 STREAM_MAX_LINES 行供 #stream 渲染（避免超大 Text 全量重绘）。"""
        if not delta:
            return
        text = (self._reasoning_tail + delta).replace("\r", "")
        lines = text.split("\n")
        self._reasoning_tail = lines.pop()  # 无换行结尾的片段留到下一个 chunk
        if lines:
            self._reasoning_lines.extend(lines)
            over = len(self._reasoning_lines) - STREAM_MAX_LINES
            if over > 0:
                del self._reasoning_lines[:over]

    def _clear_stream(self) -> None:
        self._reasoning_lines.clear()
        self._reasoning_tail = ""
        self._stream_tool.clear()
        self._content_streaming = False
        self._render_stream()

    def _render_assistant_stream(self) -> None:
        """把当前回合的 assistant 正文增量渲染进 #assistant-stream（VerticalScroll+Static+Panel）。

        正文用 Text 而非 _box 的 Markdown：流式中半截 Markdown 易解析错乱，
        故流式态用纯文本，完成时由 answer 分支用 Markdown 固化到 #log。
        """
        box = self.query_one("#assistant-stream", VerticalScroll)
        panel = self.query_one("#assistant-panel", Static)
        if self._assistant_text:
            panel.update(
                Panel(
                    rich_text(self._assistant_text, self.palette.body_text),
                    title=f"{self.palette.role_icon('assistant')} pie".strip(),
                    title_align="left",
                    border_style=self.palette.role_border("assistant"),
                    padding=_BOX_PADDING,
                )
            )
            box.display = True
            box.scroll_end(animate=False, immediate=True)
        else:
            box.display = False

    # ---- shell 模式（! 前缀）：直接执行，不经过 LLM、不进会话上下文 ----
    #
    # 手动 !cmd **不吃简洁模式**：它是用户主动执行、输出本身就是要看的东西（不是可折叠的
    # 工具活动），所以任何时候都套盒子，边框用**默认灰**（role_border 的兜底色 system，与
    # 命令反馈盒同色）——它不是 agent 的工具调用，不占 tool_call 的橙棕身份色，也跟输入框
    # 的橙棕 shell 模式边框区分开；执行失败仍染红、状态图标（✓/✗/■）照旧。

    def _run_shell(self, cmd: str) -> None:
        if not cmd:
            return
        # 命令行回显：借工具调用的形状（图标/标题自然取 shell），“参数文本”位置放 $ 原文；
        # 手动 !cmd 永远是灰边框（不占 tool_call 的橙棕身份色）、不吃简洁模式。
        self._notify(
            {"name": "shell", "arguments": f"$ {cmd}"},
            role="tool_call",
            border_role=MANUAL_SHELL_ROLE,
            lean=False,
        )
        self._cancel_event = asyncio.Event()
        self._shell_worker = self.run_worker(
            self._exec_shell_async(cmd), group="shell", exclusive=True, exit_on_error=False
        )
        self._update_send_button()

    async def _exec_shell_async(self, cmd: str) -> None:
        """asyncio 子进程逐行执行：/stop（或 Esc）时 kill 整个进程组。
        每行输出经 tool_progress 实时显示到 #stream。"""
        lines: list[str] = []
        code: Any = 0
        try:
            proc = await asyncio.create_subprocess_shell(
                cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT,
                start_new_session=True,  # 独立进程组，取消时可 killpg 连子进程一起杀
            )
            cancelled = False
            while True:
                if self._cancel_event is not None and self._cancel_event.is_set():
                    cancelled = True
                    self._kill_proc(proc)
                    break
                try:
                    line = await asyncio.wait_for(proc.stdout.readline(), 0.2)
                except asyncio.TimeoutError:
                    continue
                if not line:
                    break
                line = line.decode("utf-8", errors="replace")
                lines.append(line)
                self._append_event(
                    {"type": "tool_progress", "name": "shell", "text": line.rstrip()}
                )
            try:
                rc = await asyncio.wait_for(proc.wait(), 3)
            except asyncio.TimeoutError:
                rc = "uninterruptible"  # 进程组内仍有不可中断(D-state)进程，SIGKILL 排队；不再阻塞 UI
            code = "cancelled" if cancelled else rc
            out = "".join(lines)
        except Exception as e:  # 兜底：异常显示为红色盒子，不让 worker 静默死亡
            out = _error_body(e)
            code = "error"
        self._show_shell_result(out, code)

    @staticmethod
    def _kill_proc(proc: asyncio.subprocess.Process) -> None:
        """杀整个进程组（含 shell 的子命令），失败时退回杀 shell 本身。"""
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except (ProcessLookupError, PermissionError, OSError):
            try:
                proc.kill()
            except OSError:
                pass

    def _show_shell_result(self, out: str, code: Any) -> None:
        """!cmd 的结果框（cmd 本身已经写在上面那个命令行盒子里，这里不再要）。

        与 agent 的 shell 结果共用 `role="tool_result"` 的渲染，区别只有两点：退出码由这里补进
        `[exit=]` 头（手动执行没有工具返回格式）、成功框的边框用默认灰
        （border_role=MANUAL_SHELL_ROLE）+ 不吃简洁模式（lean=False）。
        """
        body = f"[exit={code}]\n\n{out}".rstrip("\n")
        if code == "cancelled":  # agent 那条路是循环层写取消文本，这里自己补一句
            body += f"\n[{CANCEL_TEXT}]"
        self._notify(
            {"name": "shell", "arguments": "", "content": body},
            role="tool_result",
            border_role=MANUAL_SHELL_ROLE if str(code) == "0" else "",
            lean=False,
        )
        self._cancel_event = None
        self._shell_worker = None
        self._clear_stream()
        self.query_one("#input", PieTextArea).focus()
        self._update_send_button()

    # ---- 回合（Textual worker 内 await session.aturn） ----

    def _submit(self, text: str) -> None:
        self._notify(text, role="user")
        # 输入框保持可用：等待期间用户仍可输入 /stop 或按 Esc 取消当前回合
        self._cancel_event = asyncio.Event()
        self._clear_stream()
        self._turn_worker = self.run_worker(
            self._run_turn(text), group="turn", exclusive=True, exit_on_error=False
        )
        self._update_send_button()

    async def _run_turn(self, text: str) -> None:
        try:
            await self.session.aturn(
                text, on_event=self._append_event, cancel_event=self._cancel_event
            )
        except asyncio.CancelledError:
            raise
        except Exception as e:  # 兜底：异常显示为红色盒子，不让 worker 静默死亡
            self._fail_turn(e)
            return
        self._finish_turn()

    def _append_event(self, ev: dict[str, Any]) -> None:
        """回合事件回调：worker 与 UI 同事件循环，直接更新组件（无需跨线程）。"""
        ev_type = ev.get("type")
        if ev_type == "reasoning_delta":
            self._push_reasoning(ev.get("text", ""))
            self._render_stream()
        elif ev_type == "content_delta":
            # 正文逐 chunk 实时渲染到 #assistant-stream（VerticalScroll+Static+Panel），
            # 回合结束由 answer 盒子一次性固化到 #log。
            if not self._content_streaming:
                # content 开始 → 清掉仍显示的 reasoning（#stream 只在 reasoning 阶段用）
                self._clear_stream()
                self._content_streaming = True
            self._assistant_text += ev.get("text", "")
            self._render_assistant_stream()
        elif ev_type == "tool_progress":
            self._stream_tool.append(ev.get("text", ""))
            if self._shell_worker is not None:  # 仅 !shell 模式实时显示（模型回合不用）
                self._render_stream()
        elif ev_type == "tool_call":
            # 修复 content+tool_call 并存：先把已累积的正文固化到 #log，再写工具调用框
            self._flush_assistant_text()
            self._render_tool_call(ev.get("name", "工具"), ev.get("arguments", {}))
        elif ev_type == "tool_result":
            # arguments 与 tool_call 事件同源：简洁模式的结果行要拿它取摘要（path / 命令）
            name = ev.get("name", "工具")
            self._render_tool_result(
                name, ev.get("text", ""), arguments=ev.get("arguments")
            )
            self._stream_tool.clear()  # agent 工具逐行不显示，结果落地后清空
        elif ev_type == "answer":
            # 正文流式显示已在 #assistant-stream 完成：此处把最终内容（以 answer 为准）
            # 固化为 #log 的 assistant 盒子，并清掉流式框。
            self._assistant_text = ev.get("text", "")
            self._flush_assistant_text()
            self._clear_stream()
        self.query_one("#log", RichLog).scroll_end(animate=False, force=True)
        self._update_status()

    def _fail_turn(self, exc: Exception) -> None:
        # 异常链由 _error_body 组装（首行仍是 `类名: 消息`，与以前一致）
        self._notify(_error_body(exc), role="error")
        self._cancel_event = None
        self._turn_worker = None
        self._clear_stream()
        self.query_one("#input", PieTextArea).focus()
        self._update_status()
        self._update_send_button()

    def _finish_turn(self) -> None:
        """回合正常结束：清 worker 状态、存会话、刷新状态栏（正文已由 answer 事件固化）。"""
        self._cancel_event = None
        self._turn_worker = None
        self._safe_save()
        self._update_status()
        self._update_send_button()

    def on_unmount(self) -> None:
        for w in (self._turn_worker, self._shell_worker):
            if w is not None and w.is_running:
                w.cancel()


def run_tui(session: Session, initial_prompt: str | None = None) -> None:
    """跑 TUI。循环由 pie 自建（`aio.event_loop`）而不是 textual 自取全局/新循环，
    这样退出时能先把残留的异步生成器关干净（否则见 aio.py 的说明）。"""
    with aio.event_loop() as loop:
        PieApp(session, initial_prompt).run(loop=loop)

