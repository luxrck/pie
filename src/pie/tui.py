"""Textual TUI：pi / tau 风格的聊天界面。非 TTY 或 Textual 缺失时回退 readline。"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import threading
import time
from pathlib import Path
from typing import Any

from rich.panel import Panel
from rich.cells import cell_len
from rich.segment import Segment
from rich.style import Style
from rich.text import Text
from textual import events
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal
from textual.message import Message
from textual.strip import Strip
from textual.widgets import Button, Footer, Header, RichLog, Static, TextArea

from .chat import Session

# catppuccin mocha 配色
# SCREEN_BG = "#1e1e2e"
# LOG_BG = "#181828"
SCREEN_BG = "#1a1a24"
LOG_BG = "#1b1b1b"
BODY_TEXT = "#cdd6f4"

# role → 边框颜色（不同 role 用不同颜色区分）
ROLE_BORDERS: dict[str, str] = {
    "user": "#5b78a6", #"#89b4fa",        # 你
    "assistant": "#0f766e", #"#7f9cf5",   # pie 回复
    "tool_call": "#9c4916", #"#fab387",   # 工具调用
    "tool_result": "#805b45", #"#94e2d5", # 工具结果
    "error": "#f38ba8",       # 出错
    "system": "#585b70",      # 命令反馈 / 系统提示（低调灰）
}
SELECTION_STYLE = Style(bgcolor="#89b4fa", color="#06121f")  # 鼠标框选高亮

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
    ("/reset", "清空对话历史"),
    ("/exit", "退出"),
    ("/quit", "退出"),
]


def _box(
    body: str,
    *,
    title: str = "",
    role: str = "system",
    icon: str = "",
) -> Panel:
    """把一条输出装进带边框的盒子；边框颜色按 role 区分（ROLE_BORDERS）。"""
    color = ROLE_BORDERS.get(role, ROLE_BORDERS["system"])
    return Panel(
        Text(body or "(空回复)", style=BODY_TEXT),
        title=f"{icon} {title}".strip() or None,
        title_align="left",
        border_style=color,
        padding=(0, 1),
    )


class CommandPalette(Static):
    """/ 命令补全候选面板：输入以 / 开头时显示，位于输入框上方。"""

    DEFAULT_CSS = """
    CommandPalette {
        height: auto;
        max-height: 10;
        border: round #45475a;
        background: #1e1e2e;
        color: #cdd6f4;
        padding: 0 1;
        display: none;
    }
    """


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

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
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

    @staticmethod
    def _apply_selection(strip: Strip, start: int, end: int) -> Strip:
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
                    (seg.style + SELECTION_STYLE)
                    if seg.style is not None
                    else SELECTION_STYLE
                )
                segs.append(Segment(selected, sel_style, seg.control))
            if after:
                segs.append(Segment(after, seg.style, seg.control))
        return Strip(segs, strip.cell_length)


class PieApp(App):
    """聊天主界面：Header + 消息流 + 状态栏 + 补全面板 + 底部输入。"""

    TITLE = "pie"
    CSS = f"""
    Screen {{ layout: vertical; background: {SCREEN_BG}; }}
    #log {{
        height: 1fr;
        border: round #313244;
        padding: 0 1;
        background: {LOG_BG};
    }}
    #meta, #status {{
        height: auto;
        color: #a6adc8;
        padding: 0 1;
    }}
    #input-bar {{
        height: auto;
    }}
    #input {{
        width: 1fr;
        height: auto;
        min-height: 5;
        max-height: 5;
        background: {SCREEN_BG};
        color: {BODY_TEXT};
        border: round #45475a;
    }}
    #input:focus {{
        border: round #89b4fa;
    }}
    #input.shell-mode, #input.shell-mode:focus {{
        border: round #9c4916;
    }}
    #send-btn {{
        min-width: 0;
        padding: 1;
        text-align: center;
        background: transparent;
        border: round #45475a;
        color: #a6adc8;
    }}
    #send-btn:hover {{
        background: transparent;
        border: round #89b4fa;
        color: #cdd6f4;
    }}
    #send-btn.busy {{
        background: transparent;
        border: round #f38ba8;
        color: #f38ba8;
    }}
    #send-btn.busy:hover {{
        background: transparent;
        border: round #ffb4c8;
        color: #ffb4c8;
    }}
    Header {{ background: {SCREEN_BG}; color: {BODY_TEXT}; }}
    Footer {{ background: {SCREEN_BG}; color: {BODY_TEXT}; }}
    """

    def __init__(self, session: Session, initial_prompt: str | None = None) -> None:
        super().__init__()
        self.session = session
        self.initial_prompt = initial_prompt
        self._worker: threading.Thread | None = None
        self._cancel_event: threading.Event | None = None
        self._palette_index = 0

    def compose(self) -> ComposeResult:
        yield Header()
        yield SelectableRichLog(highlight=True, markup=True, wrap=True, id="log")
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
        yield Footer()

    def on_mount(self) -> None:
        log = self.query_one("#log", RichLog)
        if self.session.fs:
            log.write(_box(f"已归档 {len(self.session.fs)} 个历史窗口块（~/.pie/windows/）"))
        self._render_history()
        self.query_one("#meta", Static).update(
            f"模型: {self.session.config.model}"
            f"  |  目录: {Path.cwd()}"
            f"  |  归档: {len(self.session.fs)}"
        )
        self.query_one("#input", PieTextArea).focus()
        send_btn = self.query_one("#send-btn", Button)
        send_btn.can_focus = False  # 右侧按钮不抢焦点，避免干扰输入
        self._update_send_button()
        self._update_status()
        if self.initial_prompt:
            self._submit(self.initial_prompt)

    def _update_status(self) -> None:
        rep = self.session.usage_report().splitlines()
        # 跳过“会话文件”行（路径长，不适合状态栏），仍取首/尾两行摘要
        lines = [ln for ln in rep if not ln.startswith("会话文件")]
        self.query_one("#status", Static).update(f"{lines[0]}\n{lines[-1]}")

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
            content = d.get("content") or ""
            if role == "user":
                log.write(_box(content, title="你", role="user", icon="▎"))
            elif role == "assistant":
                tool_calls = d.get("tool_calls")
                if tool_calls:
                    for tc in tool_calls:
                        self._write_tool_call(log, tc)
                else:
                    log.write(_box(content, title="pie", role="assistant", icon="▎"))
            elif role == "tool":
                self._write_tool_result(log, d)
            else:
                log.write(_box(content, role="system"))

    def _write_tool_call(self, log: RichLog, tc: dict) -> None:
        """渲染一条历史 tool_call（OpenAI 格式：function.arguments 是 JSON 字符串）。"""
        fn = tc.get("function") or {}
        name = fn.get("name") or "工具"
        args = fn.get("arguments") or ""
        try:
            arg_txt = json.dumps(json.loads(args), ensure_ascii=False) if args else "(无参数)"
        except (ValueError, TypeError):
            arg_txt = args or "(无参数)"
        log.write(_box(arg_txt, title=name, role="tool_call", icon="⚙"))

    def _write_tool_result(self, log: RichLog, d: dict) -> None:
        """渲染一条历史工具结果；超长（>200 行）截断显示 head/tail，避免 resume 一次性撑爆 TUI。"""
        content = d.get("content") or ""
        name = d.get("tool_name") or "工具"
        lines = content.splitlines()
        if len(lines) > 200:
            preview = "\n".join(lines[:50] + ["...[中间省略，全文见原始文件]..."] + lines[-50:])
            log.write(
                _box(
                    preview,
                    title=f"{name}（共 {len(lines)} 行，显示前后 50 行）",
                    role="tool_result",
                    icon="↳",
                )
            )
        else:
            log.write(_box(content, title=name, role="tool_result", icon="↳"))

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
                lines.append(f"[bold #06121f on #89b4fa] {cmd} [/][dim] — {desc}[/]")
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
            btn.label = "■"
            btn.add_class("busy")
        else:
            btn.label = "▶"
            btn.remove_class("busy")

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id != "send-btn":
            return
        if self._busy():
            self._command("/stop")
        else:
            inp = self.query_one("#input", PieTextArea)
            text = inp.text
            if text.strip():
                self.on_message_submitted(MessageSubmitted(text))

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

    def on_message_submitted(self, event: MessageSubmitted) -> None:
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
                    self._worker.join(timeout=2)
                    if self._busy():
                        self.query_one("#log", RichLog).write(
                            _box("正在取消中，请稍候…", role="system")
                        )
                        return
                else:
                    self.query_one("#log", RichLog).write(
                        _box("正在处理中，输入 /stop 可取消", role="system")
                    )
                    return
            self._submit(text)

    def _busy(self) -> bool:
        """是否有 worker 线程正在跑（模型请求 / 工具执行 / shell）。"""
        return self._worker is not None and self._worker.is_alive()

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
                    log.write(_box("已请求取消，正在终止…", role="system"))
                else:
                    log.write(_box("正在执行 shell，等待其结束", role="system"))
            else:
                log.write(_box("当前没有正在执行的任务", role="system"))
        elif cmd == "/help":
            log.write(_box("/exit /quit 退出 | /stop 取消当前模型请求/工具执行（等待期间可继续输入） | "
                           "/reset 清空历史 | /clear 归档并开新窗口 | "
                           "/compact [tools|turns] 手动压缩 | /save [文件] 保存 | "
                           "/status 用量 | /help 帮助 | 鼠标拖动日志可复制文本\n"
                           "!cmd 直接执行 shell（不经过 LLM，不进会话上下文；输入框变橙色即 shell 模式，/stop 可终止）"))
        elif cmd == "/reset":
            self.session.reset()
            log.write(_box("已清空历史（保留 system prompt 与记忆）"))
            self._safe_save()
        elif cmd == "/clear":
            self.session.clear_window()
            log.write(_box(f"已切换新窗口（归档 {len(self.session.fs)} 个，fs 在 ~/.pie/windows/）"))
            self._safe_save()
        elif cmd == "/compact":
            mode = arg.strip() or "auto"
            if mode not in ("auto", "tools", "turns"):
                log.write(_box(f"未知压缩模式: {mode}（/compact [tools|turns]）", role="error"))
            else:
                stats = self.session.compact(mode=mode)
                if stats.get("skipped"):
                    log.write(_box(f"未压缩：{stats['skipped']}"))
                else:
                    log.write(
                        _box(
                            f"压缩完成：节省约 {stats['saved_tokens']:,} tokens"
                            f"（tools={stats['tools']}，turns={stats['turns']}）"
                        )
                    )
                self._safe_save()
        elif cmd == "/save":
            self.session.save(arg.strip() or None)
            log.write(_box(f"会话已保存: {self.session.file}"))
        elif cmd == "/status":
            log.write(_box(self.session.usage_report()))
        else:
            log.write(_box(f"未知命令: {cmd}（/help 查看）", role="error"))
        self._update_status()

    def _safe_save(self) -> None:
        try:
            self.session.save()
        except OSError:
            pass

    # ---- shell 模式（! 前缀）：直接执行，不经过 LLM、不进会话上下文 ----

    def _run_shell(self, cmd: str) -> None:
        if not cmd:
            return
        self.query_one("#log", RichLog).write(
            _box(f"$ {cmd}", title="shell", role="tool_call", icon="⚙")
        )
        self._cancel_event = threading.Event()
        self._worker = threading.Thread(
            target=self._exec_shell, args=(cmd, self._cancel_event), daemon=True
        )
        self._worker.start()
        self._update_send_button()

    def _exec_shell(self, cmd: str, cancel_event: threading.Event) -> None:
        """Popen 轮询执行：/stop 时 kill 整个进程组并返回“用户手动终止”；保留 120s 超时。"""
        try:
            proc = subprocess.Popen(
                cmd,
                shell=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,  # 独立进程组，取消时可 killpg 连子进程一起杀
            )
            start = time.monotonic()
            while proc.poll() is None:
                if cancel_event.is_set():
                    self._kill_proc(proc)
                    out, _ = proc.communicate()
                    code = "cancelled"
                    self.call_from_thread(self._show_shell_result, cmd, out, code)
                    return
                if time.monotonic() - start > 120:
                    self._kill_proc(proc)
                    out, _ = proc.communicate()
                    code = "timeout(120s)"
                    self.call_from_thread(self._show_shell_result, cmd, out, code)
                    return
                time.sleep(0.05)
            out, _ = proc.communicate()
            code = proc.returncode
        except Exception as e:  # 兜底：异常显示为红色盒子
            out = f"{type(e).__name__}: {e}"
            code = "error"
        self.call_from_thread(self._show_shell_result, cmd, out, code)

    @staticmethod
    def _kill_proc(proc: subprocess.Popen) -> None:
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
        lines = out.splitlines()
        if len(lines) > 200:
            preview = "\n".join(lines[:100] + ["...[输出过长，已截断]..."] + lines[-50:])
            body = preview
            title = f"shell [{code}]（共 {len(lines)} 行，显示前 100 后 50）"
        else:
            body = out
            title = f"shell [{code}]" if out else f"shell [{code}]（无输出）"
        log.write(_box(body or "(无输出)", title=title, role="tool_result", icon="↳"))
        self._cancel_event = None
        self._worker = None
        self.query_one("#input", PieTextArea).focus()
        self._update_send_button()

    def _submit(self, text: str) -> None:
        self.query_one("#log", RichLog).write(
            _box(text, title="你", role="user", icon="▎")
        )
        # 输入框保持可用：等待期间用户仍可输入 /stop 取消当前回合
        self._cancel_event = threading.Event()
        self._worker = threading.Thread(target=self._run_turn, args=(text,), daemon=True)
        self._worker.start()
        self._update_send_button()

    def _run_turn(self, text: str) -> None:
        def on_event(ev: dict[str, Any]) -> None:
            self.call_from_thread(self._append_event, ev)

        try:
            answer = self.session.turn(
                text, on_event=on_event, cancel_event=self._cancel_event
            )
        except Exception as e:  # 兜底：异常显示为红色盒子，不让 worker 静默死亡
            self.call_from_thread(self._fail_turn, e)
            return
        self.call_from_thread(self._finish_turn, answer)

    def _append_event(self, ev: dict[str, Any]) -> None:
        log = self.query_one("#log", RichLog)
        ev_type = ev.get("type")
        if ev_type == "tool_call":
            args = ev.get("arguments", {})
            try:
                arg_txt = json.dumps(args, ensure_ascii=False) if args else "(无参数)"
            except TypeError:
                arg_txt = str(args) or "(无参数)"
            log.write(
                _box(arg_txt, title=ev.get("name", "工具"), role="tool_call", icon="⚙")
            )
        elif ev_type == "tool_result":
            log.write(
                _box(ev.get("text", ""), title=ev.get("name", "工具"), role="tool_result", icon="↳")
            )
        elif ev_type == "answer":
            log.write(_box(ev.get("text", ""), title="pie", role="assistant", icon="▎"))
        self._update_status()

    def _fail_turn(self, exc: Exception) -> None:
        log = self.query_one("#log", RichLog)
        log.write(_box(f"{type(exc).__name__}: {exc}", title="出错", role="error", icon="✗"))
        self._cancel_event = None
        self._worker = None
        self.query_one("#input", PieTextArea).focus()
        self._update_status()
        self._update_send_button()

    def _finish_turn(self, answer: str) -> None:
        self._cancel_event = None
        self._worker = None
        self._safe_save()
        self._update_status()
        self._update_send_button()

    def on_unmount(self) -> None:
        if self._worker and self._worker.is_alive():
            self._worker.join(timeout=5)


def run_tui(session: Session, initial_prompt: str | None = None) -> None:
    PieApp(session, initial_prompt).run()

