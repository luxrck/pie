"""Textual TUI：pi / tau 风格的聊天界面。非 TTY 或 Textual 缺失时回退 readline。

异步架构：回合在 Textual worker（同一事件循环）里 await session.aturn()，
流式增量（模型 reasoning/content、shell 逐行输出）经 on_event 实时渲染到
消息流下方的 #stream 区；/stop 通过 asyncio.Event 优雅取消当前回合。
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import time
from pathlib import Path
from typing import Any

from rich.panel import Panel
from rich.cells import cell_len
from rich.segment import Segment
from rich.style import Style
from rich.text import Text
from rich.markdown import Markdown
from textual import events
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal
from textual.message import Message
from textual.strip import Strip
from textual.widgets import Button, Footer, Header, RichLog, Static, TextArea
from textual.worker import Worker

from .session import Session
from .config import REASONING_LEVELS, REASONING_NONE
from .context import content_text
from .theme import Theme, get_theme

# 命令补全候选：(命令, 说明)
PALETTE_COMMANDS: list[tuple[str, str]] = [
    ("/help", "显示帮助"),
    ("/status", "查看 token 用量"),
    ("/stop", "取消当前正在执行的模型请求/工具"),
    ("/compact", "工具级 + 轮次级压缩"),
    ("/compact tools", "只做工具级压缩"),
    ("/compact turns", "只做轮次级压缩"),
    ("/clear", "归档当前窗口，开新窗口"),
    ("/save", "保存会话（可带文件路径）"),
    ("/reasoning none", "思考深度: 关闭"),
    ("/reasoning low", "思考深度: low"),
    ("/reasoning high", "思考深度: high（默认）"),
    ("/reasoning max", "思考深度: max"),
    ("/reset", "清空对话历史"),
    ("/exit", "退出"),
    ("/quit", "退出"),
]


STREAM_MAX_LINES = 8  # #stream 区最多显示的行数（!shell 实时输出）


def _box(
    palette: Theme,
    body: str,
    *,
    title: str = "",
    role: str = "system",
    icon: str = "",
) -> Panel:
    """把一条输出装进带边框的盒子；边框颜色按 role 区分（palette.role_border）。"""
    cls = Markdown if role == "assistant" else Text
    return Panel(
        cls(body or "(空回复)", style=palette.body_text),
        title=f"{icon} {title}".strip() or None,
        title_align="left",
        border_style=palette.role_border(role),
        padding=(0, 1),
    )


def _tool_failed_role(text: str) -> str:
    """工具结果边框 role：执行失败染成 error 红框，其余保持 tool_result。

    判定：shell 返回以 [exit=N] 开头且 N ≠ 0（命令执行失败）；或以工具调用层
    失败前缀开头（超时 [shell] / [工具错误] / [工具异常] / [参数解析失败]）。
    """
    if text.startswith("[exit="):
        rc = text[6:].split("]", 1)[0]
        if rc.lstrip("-").isdigit() and rc != "0":
            return "error"
    elif text.startswith(("[工具错误]", "[工具异常]", "[参数解析失败]", "[shell] ")):
        return "error"
    return "tool_result"


def _split_shell_exit(text: str) -> tuple[str | None, str]:
    """shell 工具返回文本：解析首行 [exit=N] → (code, 去掉首行后的正文)。

    非 shell 文本（read/edit/write 结果、[工具错误] 等失败前缀、取消文本）原样
    返回 (None, text)，由调用方按普通工具结果渲染。
    """
    if text.startswith("[exit="):
        code = text[6:].split("]", 1)[0]
        rest = text.split("\n", 1)[1] if "\n" in text else ""
        return code, rest
    return None, text


def _shell_result_box(body: str, code: Any) -> tuple[str, str, str]:
    """按 !shell 结果（_show_shell_result）的样式生成 (title, body, role)：
    exit code 进标题 `shell [{code}]`，正文不带 [exit=] 头；超长 (>200 行) head/tail
    截断。agent 回合里 shell 工具的结果与用户直接 !cmd 执行保持同一套视觉。"""
    lines = body.splitlines()
    if len(lines) > 200:
        preview = "\n".join(lines[:100] + ["...[输出过长，已截断]..."] + lines[-50:])
        title = f"shell [{code}]（共 {len(lines)} 行，显示前 100 后 50）"
    else:
        preview = body
        title = f"shell [{code}]" if body else f"shell [{code}]（无输出）"
    if str(code) == "0":
        role = "tool_result"
    elif code == "cancelled":
        role = "system"  # 用户主动 /stop，非错误，低调灰
    else:  # 非 0 退出码 / 超时 / 异常 → 红框
        role = "error"
    return title, preview or "(无输出)", role


def build_css(palette: Theme) -> str:
    """由主题（palette）生成 PieApp 的 Textual CSS：布局 + 配色，无硬编码颜色。

    颜色全部来自 Theme；布局/滚动条配置固定。运行时在 __init__ 注入到 self.CSS，
    Textual 在 load 阶段读取的是实例属性 self.CSS（而非类级 CSS），因此可按所选主题动态生成。
    """
    return f"""
Screen {{ layout: vertical; background: {palette.screen_bg}; }}
#log {{
    height: 1fr;
    border: round {palette.border_dim};
    padding: 0 1;
    background: {palette.log_bg};
    /* 滚动条：窄（1 cell）+ 半透明灰轨道 + 亮灰滑块，替换默认的 2 cell 黑底蓝条 */
    scrollbar-size: 0 1;
    /* 轨道：带 alpha 的灰。ScrollBar 渲染时若背景 alpha<1 会与父级背景（沿 transparent
       链最终是终端默认背景色）alpha 混合 → 半透明灰透出终端底色。
       不要用 transparent（纯透明轨道会隐形）。 */
    scrollbar-background: {palette.scrollbar_track};
    scrollbar-background-hover: {palette.scrollbar_track_hover};
    scrollbar-background-active: {palette.scrollbar_track_hover};
    scrollbar-color: {palette.muted};
    scrollbar-color-hover: {palette.body_text};
    scrollbar-color-active: {palette.body_text};
}}
#stream {{
    height: auto;
    max-height: 3;
    color: {palette.muted};
    padding: 0 1;
    display: none;
    border: none;
}}
#meta, #status {{
    height: auto;
    color: {palette.muted};
    padding: 0 1;
}}
#input-bar {{
    height: auto;
}}
CommandPalette {{
    height: auto;
    max-height: 6;
    border: round {palette.border};
    background: transparent;
    color: {palette.body_text};
    padding: 0 1;
    display: none;
}}
#input {{
    width: 1fr;
    height: auto;
    min-height: 5;
    max-height: 5;
    background: {palette.screen_bg};
    color: {palette.body_text};
    border: round {palette.border};
    & .text-area--placeholder {{
        color: {palette.faint};
    }}
    /* 选中高亮与 #log 鼠标框选统一：覆盖 TextArea 内置的
       .text-area--selection（ansi 下默认 background: transparent + reverse，
       与 #log 的选中高亮不一致）。#input 是 ID 选择器，优先级更高。 */
    & .text-area--selection {{
        background: {palette.accent};
        color: {palette.accent_text};
    }}
}}
#input:focus {{
    border: round {palette.accent};
}}
#input.shell-mode, #input.shell-mode:focus {{
    border: round {palette.role_tool_call};
}}
#send-btn {{
    min-width: 0;
    padding: 1;
    text-align: center;
    background: transparent;
    border: round {palette.border};
    color: {palette.muted};
}}
#send-btn:hover {{
    background: transparent;
    border: round {palette.accent};
    color: {palette.body_text};
}}
#send-btn.busy {{
    background: transparent;
    border: round {palette.busy_border};
    color: {palette.busy_text};
}}
#send-btn.busy:hover {{
    background: transparent;
    border: round {palette.busy_border_hover};
    color: {palette.busy_border_hover};
}}
Header {{ background: {palette.screen_bg}; color: {palette.body_text}; }}
Footer {{ background: {palette.screen_bg}; color: {palette.body_text}; }}
"""


class CommandPalette(Static):
    """/ 命令补全候选面板：输入以 / 开头时显示，位于输入框上方。"""


class MessageSubmitted(Message):
    """多行消息输入框提交（Enter）。"""

    def __init__(self, text: str) -> None:
        super().__init__()
        self.text = text


class PieTextArea(TextArea):
    """多行消息输入框：Enter 提交，Shift+Enter 换行（支持多行粘贴）；
    Tab 接受命令补全，↑/↓ 切换候选，Esc 隐藏面板。"""

    BINDINGS = [
        *TextArea.BINDINGS,
        Binding("tab", "palette_accept", "接受命令补全", show=False),
        Binding("escape", "palette_hide", "隐藏补全", show=False),
    ]

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
        self.app.palette_hide()

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


class SelectableRichLog(RichLog):
    """RichLog + 鼠标框选复制：按住左键拖动选择，松开自动复制到剪贴板。"""

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

    def _selected_text(self) -> str:
        if self._sel_start is None or self._sel_end is None:
            return ""
        r1, c1 = self._sel_start
        r2, c2 = self._sel_end
        if (r1, c1) > (r2, c2):
            r1, c1, r2, c2 = r2, c2, r1, c1
        out: list[str] = []
        for r in range(r1, r2 + 1):
            if r < 0 or r >= len(self.lines):
                continue
            width = self.lines[r].cell_length
            a = c1 if r == r1 else 0
            b = c2 if r == r2 else width
            if b <= a:
                out.append("")
                continue
            cropped = self.lines[r].crop(a, min(b, width))
            out.append(self._clean_copied_line(cropped.text))
        # 去掉首尾空行（Panel 顶/底边框与填充产生的空行）
        while out and out[0] == "":
            out.pop(0)
        while out and out[-1] == "":
            out.pop()
        return "\n".join(out)

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

    def render_line(self, y: int) -> Strip:
        strip = super().render_line(y)
        if self._sel_start is None or self._sel_end is None:
            return strip
        r1, c1 = self._sel_start
        r2, c2 = self._sel_end
        if (r1, c1) > (r2, c2):
            r1, c1, r2, c2 = r2, c2, r1, c1
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

    def __init__(self, session: Session, initial_prompt: str | None = None) -> None:
        super().__init__()
        self.session = session
        self.initial_prompt = initial_prompt
        self.palette = get_theme(session.config.theme)
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

    def compose(self) -> ComposeResult:
        # yield Header()
        yield SelectableRichLog(
            selection_style=Style(bgcolor=self.palette.accent, color=self.palette.accent_text),
            highlight=True,
            markup=True,
            wrap=True,
            id="log",
        )
        yield Static("", id="stream")
        yield CommandPalette("", id="palette")
        with Horizontal(id="input-bar"):
            yield PieTextArea(
                placeholder="输入消息（! 开头直接执行 shell，/stop 取消当前任务，/ 显示命令补全，Shift+Enter 换行）",
                id="input",
                tab_behavior="focus",
                highlight_cursor_line=False,
            )
            yield Button("▶", id="send-btn", variant="default")
        yield Static("", id="meta")
        yield Static("", id="status")
        # yield Footer()

    def on_mount(self) -> None:
        # 用 ansi-dark 主题：background=ansi_default + ansi=True（native ANSI），
        # 背景输出 `49`（终端默认背景）→ 透明，露出终端窗口背景色；
        # 默认主题 ansi=False 会经 ANSIToTruecolor 把 default 背景映射成主题色（不透明）。
        self.theme = "ansi-dark"
        log = self.query_one("#log", RichLog)
        if self.session.fs:
            log.write(_box(self.palette, f"已归档 {len(self.session.fs)} 个历史窗口块（~/.pie/windows/）"))
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
        if self.initial_prompt:
            self._submit(self.initial_prompt)

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
        """顶栏元信息：模型 / 思考深度 / 目录 / 归档窗口数（/reasoning 切换后刷新）。"""
        self.query_one("#meta", Static).update(
            f"{self.session.config.model} {self.session.config.reasoning_effort}"
            f" · {Path.cwd()}"
            f" | 归档: {len(self.session.fs)}"
        )

    def _update_status(self) -> None:
        rep = self.session.usage_report().splitlines()
        # 跳过“会话文件”行（路径长，不适合状态栏），仍取首/尾两行摘要
        lines = [ln for ln in rep if not ln.startswith("会话文件")]
        self.query_one("#status", Static).update(f"{lines[0].split("：", 1)[-1].strip()} | {lines[-1].split("：", 1)[-1].strip()}")

    # ---- 历史渲染 ----

    def _render_history(self) -> None:
        """resume 时把已有对话历史渲染进消息流（压缩指针展开为完整转录）。

        与实时事件渲染保持一致的盒子样式：user → "你"，assistant → "pie"，
        tool_calls → ⚙，tool → ↳；system（system prompt / 窗口摘要）不显示。
        """
        log = self.query_one("#log", RichLog)
        for d in self.session.full_history():
            role = d.get("role")
            if role == "system":
                continue
            content = content_text(d.get("content"))
            if role == "user":
                log.write(_box(self.palette, content, title="你", role="user", icon="▎"))
            elif role == "assistant":
                tool_calls = d.get("tool_calls")
                if tool_calls:
                    for tc in tool_calls:
                        self._write_tool_call(log, tc)
                else:
                    log.write(_box(self.palette, content, title="pie", role="assistant", icon="▎"))
            elif role == "tool":
                self._write_tool_result(log, d)
            else:
                log.write(_box(self.palette, content, role="system"))

    def _write_tool_call(self, log: RichLog, tc: dict) -> None:
        """渲染一条历史 tool_call（OpenAI 格式：function.arguments 是 JSON 字符串）。"""
        fn = tc.get("function") or {}
        name = fn.get("name") or "工具"
        args = fn.get("arguments") or ""
        try:
            arg_txt = json.dumps(json.loads(args), ensure_ascii=False) if args else "(无参数)"
        except (ValueError, TypeError):
            arg_txt = args or "(无参数)"
        log.write(_box(self.palette, arg_txt, title=name, role="tool_call", icon="⚙"))

    def _write_tool_result(self, log: RichLog, d: dict) -> None:
        """渲染一条历史工具结果；超长（>200 行）截断显示 head/tail，避免 resume 一次性撑爆 TUI。

        shell 工具结果（以 [exit=N] 开头）解析 exit code 进标题、正文去掉 [exit=] 头，
        与 !shell 的 _show_shell_result 展示一致；其余工具原样显示。"""
        content = d.get("content") or ""
        name = d.get("tool_name") or "工具"
        code, body = _split_shell_exit(content)
        if code is not None:  # shell：标题带 exit code，正文不带 [exit=] 头
            title, body, role = _shell_result_box(body, code)
        else:
            title = name
            role = _tool_failed_role(content)
            lines = body.splitlines()
            if len(lines) > 200:
                body = "\n".join(lines[:50] + ["...[中间省略，全文见原始文件]..."] + lines[-50:])
                title = f"{name}（共 {len(lines)} 行，显示前后 50 行）"
        log.write(_box(self.palette, body, title=title, role=role, icon="↳"))

    # ---- 命令补全 ----

    def _palette_matches(self) -> list[tuple[str, str]]:
        value = self.query_one("#input", PieTextArea).text
        if not value.startswith("/"):
            return []
        return [c for c in PALETTE_COMMANDS if c[0].startswith(value)]

    def _render_palette(self) -> None:
        palette = self.query_one("#palette", CommandPalette)
        matches = self._palette_matches()
        if not matches:
            self._palette_index = 0
            palette.display = False
            return
        self._palette_index = min(self._palette_index, len(matches) - 1)
        shown = matches[:9]
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

    # ---- 消息 ----

    async def on_message_submitted(self, event: MessageSubmitted) -> None:
        text = event.text.strip()
        self.query_one("#input", PieTextArea).text = ""
        self._update_input_border()
        if not text:
            return
        if text.startswith("/"):
            self._command(text)
        elif text.startswith("!"):
            self._run_shell(text[1:].strip())
        else:
            if self._busy():
                if self._cancelling():
                    # 正在取消收尾：等它结束（cancel 路径很快），避免新旧回合并发写历史
                    await self._turn_worker.wait()  # type: ignore[union-attr]
                    if self._busy():
                        self.query_one("#log", RichLog).write(
                            _box(self.palette, "正在取消中，请稍候…", role="system")
                        )
                        return
                else:
                    self.query_one("#log", RichLog).write(
                        _box(self.palette, "正在处理中，输入 /stop 可取消", role="system")
                    )
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

    def _command(self, text: str) -> None:
        log = self.query_one("#log", RichLog)
        cmd, _, arg = text.partition(" ")
        if cmd in ("/exit", "/quit"):
            self.exit()
        elif cmd == "/stop":
            if self._busy():
                if self._cancel_event is not None:
                    self._cancel_event.set()
                    log.write(_box(self.palette, "已请求取消，正在终止…", role="system"))
                else:
                    log.write(_box(self.palette, "当前没有正在执行的任务", role="system"))
            else:
                log.write(_box(self.palette, "当前没有正在执行的任务", role="system"))
        elif cmd == "/help":
            log.write(_box(self.palette, "/exit /quit 退出 | /stop 取消当前模型请求/工具执行（等待期间可继续输入） | "
                           "/reset 清空历史 | /clear 归档并开新窗口 | "
                           "/compact [tools|turns] 手动压缩 | /save [文件] 保存 | "
                           "/reasoning <none|low|high|max> 思考深度 | "
                           "/status 用量 | /help 帮助 | 鼠标拖动日志可复制文本\n"
                           "!cmd 直接执行 shell（不经过 LLM，不进会话上下文；输入框变橙色即 shell 模式，/stop 可终止）"))
        elif cmd == "/reset":
            self.session.reset()
            log.write(_box(self.palette, "已清空历史（保留 system prompt 与记忆）"))
            self._safe_save()
        elif cmd == "/clear":
            self.session.clear_window()
            log.write(_box(self.palette, f"已切换新窗口（归档 {len(self.session.fs)} 个，fs 在 ~/.pie/windows/）"))
            self._safe_save()
        elif cmd == "/compact":
            mode = arg.strip() or "auto"
            if mode not in ("auto", "tools", "turns"):
                log.write(_box(self.palette, f"未知压缩模式: {mode}（/compact [tools|turns]）", role="error"))
            else:
                stats = self.session.compact(mode=mode)
                if stats.get("skipped"):
                    log.write(_box(self.palette, f"未压缩：{stats['skipped']}"))
                else:
                    log.write(
                        _box(
                            self.palette,
                            f"压缩完成：节省约 {stats['saved_tokens']:,} tokens"
                            f"（tools={stats['tools']}，turns={stats['turns']}）"
                        )
                    )
                self._safe_save()
        elif cmd == "/save":
            self.session.save(arg.strip() or None)
            log.write(_box(self.palette, f"会话已保存: {self.session.file}"))
        elif cmd == "/status":
            log.write(_box(self.palette, self.session.usage_report()))
        elif cmd == "/reasoning":
            level = arg.strip().lower()
            if level not in REASONING_LEVELS:
                log.write(
                    _box(
                        self.palette,
                        f"未知思考级别: {level or '(空)'}（可选: {' / '.join(REASONING_LEVELS)}）",
                        role="error",
                    )
                )
            else:
                cfg = self.session.config
                cfg.reasoning_effort = level
                # 当前 llm 实例立即生效（后续请求即用新深度，无需重启）
                llm = getattr(self.session, "llm", None)
                if llm is not None and hasattr(llm, "reasoning_effort"):
                    llm.reasoning_effort = None if level == REASONING_NONE else level
                try:
                    cfg.save(getattr(cfg, "config_file", None))  # 持久化，重启后仍生效
                    note = "已写入配置"
                except OSError as e:
                    note = f"配置写入失败: {e}（仅本次会话生效）"
                log.write(_box(self.palette, f"思考深度: {level}（{note}）", role="system"))
                self._update_meta()
        else:
            log.write(_box(self.palette, f"未知命令: {cmd}（/help 查看）", role="error"))
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
            stream.update(Text("\n".join(shown), style="dim"))
            stream.display = True
        elif self._shell_worker is not None and self._stream_tool:
            shown = self._stream_tool[-STREAM_MAX_LINES:]
            stream.update(Text("\n".join(shown)))
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
        self._render_stream()

    # ---- shell 模式（! 前缀）：直接执行，不经过 LLM、不进会话上下文 ----

    def _run_shell(self, cmd: str) -> None:
        if not cmd:
            return
        self.query_one("#log", RichLog).write(
            _box(self.palette, f"$ {cmd}", title="shell", role="tool_call", icon="⚙")
        )
        self._cancel_event = asyncio.Event()
        self._shell_worker = self.run_worker(
            self._exec_shell_async(cmd), group="shell", exclusive=True, exit_on_error=False
        )
        self._update_send_button()

    async def _exec_shell_async(self, cmd: str) -> None:
        """asyncio 子进程逐行执行：/stop 时 kill 整个进程组；保留 120s 超时。
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
            started = time.monotonic()
            timed_out = False
            cancelled = False
            while True:
                if self._cancel_event is not None and self._cancel_event.is_set():
                    cancelled = True
                    self._kill_proc(proc)
                    break
                # if time.monotonic() - started > 120:
                #     timed_out = True
                #     self._kill_proc(proc)
                #     break
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
            rc = await proc.wait()
            code = "cancelled" if cancelled else ("timeout(120s)" if timed_out else rc)
            out = "".join(lines)
        except Exception as e:  # 兜底：异常显示为红色盒子，不让 worker 静默死亡
            out = f"{type(e).__name__}: {e}"
            code = "error"
        self._show_shell_result(cmd, out, code)

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

    def _show_shell_result(self, cmd: str, out: str, code: Any) -> None:
        log = self.query_one("#log", RichLog)
        if code == "cancelled":
            out = (out.rstrip() + "\n[用户手动终止]").strip()
        title, body, result_role = _shell_result_box(out, code)
        log.write(_box(self.palette, body, title=title, role=result_role, icon="↳"))
        self._cancel_event = None
        self._shell_worker = None
        self._clear_stream()
        self.query_one("#input", PieTextArea).focus()
        self._update_send_button()

    # ---- 回合（Textual worker 内 await session.aturn） ----

    def _submit(self, text: str) -> None:
        self.query_one("#log", RichLog).write(
            _box(self.palette, text, title="你", role="user", icon="▎")
        )
        # 输入框保持可用：等待期间用户仍可输入 /stop 取消当前回合
        self._cancel_event = asyncio.Event()
        self._clear_stream()
        self._turn_worker = self.run_worker(
            self._run_turn(text), group="turn", exclusive=True, exit_on_error=False
        )
        self._update_send_button()

    async def _run_turn(self, text: str) -> None:
        try:
            answer = await self.session.aturn(
                text, on_event=self._append_event, cancel_event=self._cancel_event
            )
        except asyncio.CancelledError:
            raise
        except Exception as e:  # 兜底：异常显示为红色盒子，不让 worker 静默死亡
            self._fail_turn(e)
            return
        self._finish_turn(answer)

    def _append_event(self, ev: dict[str, Any]) -> None:
        """回合事件回调：worker 与 UI 同事件循环，直接更新组件（无需跨线程）。"""
        log = self.query_one("#log", RichLog)
        ev_type = ev.get("type")
        if ev_type == "reasoning_delta":
            self._push_reasoning(ev.get("text", ""))
            self._render_stream()
        elif ev_type == "content_delta":
            pass  # 正文不进 #stream（回合结束由 answer 盒子一次性固化）
        elif ev_type == "tool_progress":
            self._stream_tool.append(ev.get("text", ""))
            if self._shell_worker is not None:  # 仅 !shell 模式实时显示（模型回合不用）
                self._render_stream()
        elif ev_type == "tool_call":
            args = ev.get("arguments", {})
            try:
                arg_txt = json.dumps(args, ensure_ascii=False) if args else "(无参数)"
            except TypeError:
                arg_txt = str(args) or "(无参数)"
            log.write(
                _box(self.palette, arg_txt, title=ev.get("name", "工具"), role="tool_call", icon="⚙")
            )
        elif ev_type == "tool_result":
            text = ev.get("text", "")
            code, body = _split_shell_exit(text)
            if code is not None:  # shell 结果：样式与 !shell（_show_shell_result）一致
                title, body, role = _shell_result_box(body, code)
            else:
                title = ev.get("name", "工具")
                role = _tool_failed_role(text)
            log.write(_box(self.palette, body, title=title, role=role, icon="↳"))
            self._stream_tool.clear()  # agent 工具逐行不显示，结果落地后清空
        elif ev_type == "answer":
            text = ev.get("text", "")
            # 正文不在 #stream 实时显示：直接固化最终内容为 log 盒子
            log.write(_box(self.palette, text, title="pie", role="assistant", icon="▎"))
            self._clear_stream()
        self._update_status()

    def _fail_turn(self, exc: Exception) -> None:
        log = self.query_one("#log", RichLog)
        log.write(_box(self.palette, f"{type(exc).__name__}: {exc}", title="出错", role="error", icon="✗"))
        self._cancel_event = None
        self._turn_worker = None
        self._clear_stream()
        self.query_one("#input", PieTextArea).focus()
        self._update_status()
        self._update_send_button()

    def _finish_turn(self, answer: str) -> None:
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
    PieApp(session, initial_prompt).run()
