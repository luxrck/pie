//! App：状态机 + 事件循环。
//!
//! 事件流（codex 那套的简化版）：
//!
//! ```text
//!   crossterm 键盘/粘贴 ─┐
//!   tick（~15fps）      ─┼─→ tokio::select! → App 状态 → terminal.draw()
//!   回合事件（mpsc）    ─┘
//! ```
//!
//! 回合在**独立 task** 里跑（`Session` 用 `Arc<Mutex<_>>` 包着，回合期间被任务独占）；
//! 回合事件经 channel 回来 —— 所以界面不阻塞：**思考计时照走、能滚动、能看流式增量**。
//! 回合中要读会话（`/status` 之类）用 `try_lock`，拿不到就提示「回合进行中」。
//!
//! `terminal.draw` 每帧只写**变化的单元格**（ratatui 的双缓冲 diff），
//! 所以「不断追加增量」不会导致整屏重绘。

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;

use crate::cancel::{Cancel, CANCEL_TEXT};
use crate::llm::LlmError;
use crate::session::{Session, TurnEvent};

use super::clipboard;
use super::history::{self, Cell};
use super::input::Input;
use super::palette;
use super::status::{self, Activity, Snapshot};
use super::theme::{Palette, Status};

/// 主循环只 select 三路：终端事件、回合事件、tick。
pub enum UiEvent {
    Turn(TurnEvent),
    TurnDone(Result<String, LlmError>, Snapshot),
    /// 手动 `!cmd` 跑完了（状态 + 结果正文）
    ShellDone(Status, String),
    /// 后台拉到的可用模型列表（`/model ` 的补全候选）
    Models(Vec<String>),
    /// 进程级告警（重试 / 压缩 / 图片…，见 `crate::log`）：**不能写 stderr**，进消息流
    Notice(String),
}

/// tick 间隔（约 15fps）：思考计时的秒数要跟着走，但不必更高。
const TICK: Duration = Duration::from_millis(66);
/// 短于这个时长的思考不留痕（免得满屏 `Thought for 0.1s`）。
const THOUGHT_TRACE_MIN: Duration = Duration::from_millis(300);
/// 右下角临时提示活多久（Python 版是 Textual 的 `App.notify` Toast——默认 5s，用户看到的约 3s）。
const TOAST_TTL: Duration = Duration::from_secs(3);

/// 右下角浮出来的临时提示（对齐 Python 版 `self.app.notify(...)` 的 Textual Toast）：
/// 不占消息流、到点自消，用来报「刚发生了件小事」（复制了几字符…）。
struct Toast {
    text: String,
    /// 失败提示换错误色边框
    ok: bool,
    until: Instant,
}

pub struct App {
    /// 会话（回合任务与 UI 共用；回合期间锁被任务握着）。
    session: Arc<Mutex<Session>>,
    tx: UnboundedSender<UiEvent>,
    cells: Vec<Cell>,
    input: Input,
    palette: Palette,
    snapshot: Snapshot,
    activity: Activity,
    /// 思考起点（首个 reasoning 增量时记）
    thought_started: Option<Instant>,
    /// 当前回合的取消信号（`Esc` 触发；每回合新建）
    cancel: Cancel,
    /// 距底部的滚动偏移（0 = 贴底跟随）
    scroll_from_bottom: u16,
    /// 上一帧的消息流区域（鼠标坐标 → 显示行/列）
    body: Rect,
    /// 上一帧第一条可见的显示行号（同上；绝对行号 = `scroll_top` + 区域内行偏移）
    scroll_top: u16,
    /// 上一帧的排版（复制时按它切源文本）
    layout: history::Layout,
    /// 鼠标框选：起点 / 终点（**绝对**显示行, 单元格列）；`None` = 没在选
    selection: Option<((u16, u16), (u16, u16))>,
    /// 右下的临时提示（复制完弹一条，`TOAST_TTL` 后自消）
    toast: Option<Toast>,
    frame: u64,
    busy: bool,
    should_quit: bool,
    /// 简洁模式（`[tui] lean`）：成功的工具结果只留一行，失败/取消才带正文
    lean: bool,
    /// 端点可用模型（`/model ` 的补全候选；启动时后台拉，失败了就空着）
    models: Vec<String>,
    /// 补全面板的高亮下标
    palette_index: usize,
    /// 面板被 `Esc` 临时收起（输入一变自动恢复）
    palette_hidden: bool,
    /// 上一帧的输入文本（用来发现「输入变了」→ 恢复面板）
    last_input: String,
    /// 当前思考深度（`/thinking ` 候选的「← 当前」标注）
    effort: String,
}

impl App {
    /// 建 App + 取出事件接收端（`rx` 由主循环持有，避免和 `select!` 里的 `&mut self` 打架）。
    pub fn new(session: Session) -> (Self, UnboundedReceiver<UiEvent>) {
        let (tx, rx) = unbounded_channel();
        let snapshot = Snapshot {
            model: session.config.model.clone(),
            cwd: cwd_text(),
            prompt_tokens: session.usage.prompt_tokens,
            budget: session.config.context_budget(),
            calls: session.usage.calls,
            busy: false,
        };
        let lean = session.config.tui.lean;
        let effort = session.config.reasoning_effort.clone();
        // resume 的历史：把已有对话回放进消息流（新会话只有 system → 什么都不做）。
        // 走 `full_history()`，压缩过的回合也是「当初界面上看到的样子」而不是摘要 + 指针。
        let cells = history::cells_from_history(&session.full_history());
        let app = Self {
            session: Arc::new(Mutex::new(session)),
            tx,
            cells,
            input: Input::new(),
            palette: Palette::default(),
            snapshot,
            activity: Activity::Idle,
            thought_started: None,
            cancel: Cancel::new(),
            scroll_from_bottom: 0,
            body: Rect::default(),
            scroll_top: 0,
            layout: history::Layout::default(),
            selection: None,
            toast: None,
            frame: 0,
            busy: false,
            should_quit: false,
            lean,
            models: Vec::new(),
            palette_index: 0,
            palette_hidden: false,
            last_input: String::new(),
            effort,
        };
        (app, rx)
    }

    /// 主循环。退出时把会话保存好（回合还在跑就等最多 5s）。
    ///
    /// 返回 `Ok(Some(msg))` = 退出时想提示的一行，由 `tui::run` 在**终端恢复之后**打印
    /// （这时还是 raw mode，写 stderr 会砸花屏幕）。
    pub async fn run(
        mut self,
        terminal: &mut ratatui::DefaultTerminal,
        mut rx: UnboundedReceiver<UiEvent>,
    ) -> std::io::Result<Option<String>> {
        // 回合中途的告警走消息流：raw mode + alternate screen 下直接写 stderr 会把字符
        // 按在当前光标处，而 ratatui 只重写变化的单元格 → 砸坏的行再也不会被修复。
        let log_guard = {
            let tx = self.tx.clone();
            crate::log::install(move |msg| tx.send(UiEvent::Notice(msg)).is_ok())
        };
        let mut events = EventStream::new();
        let mut ticker = tokio::time::interval(TICK);
        // 启动不再往消息流里推一行键位提示：底栏已经在显示键位、`/help` 有完整命令表，
        // 那一行只会在每次开新会话时把历史区顶掉一行。
        self.spawn_fetch_models();
        while !self.should_quit {
            terminal.draw(|frame| self.render(frame))?;
            tokio::select! {
                maybe = events.next() => {
                    if let Some(Ok(event)) = maybe {
                        self.on_terminal_event(event);
                    }
                }
                Some(event) = rx.recv() => self.on_turn_event(event),
                _ = ticker.tick() => {
                    self.frame = self.frame.wrapping_add(1);
                }
            }
        }
        let warning = self.shutdown().await;
        drop(log_guard); // 出口卸下：之后再写的告警回到 stderr
        Ok(warning)
    }

    // ---------------------------------------------------------------- 事件

    fn on_terminal_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.on_key(key)
            }
            Event::Paste(text) => self.input.insert(&normalize_newlines(&text)),
            Event::Mouse(mouse) => self.on_mouse(mouse),
            _ => {}
        }
    }

    /// 鼠标：滚轮滚历史区；**左键拖动框选，松开即复制**（对齐 Python `SelectableRichLog`）。
    fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll(-3),
            MouseEventKind::ScrollDown => self.scroll(3),
            // 按在消息流区域内才开始选（点底栏/输入框不该起选区）；拖动时贴边（可拖到区外）
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.body;
                let inside = mouse.column >= area.x
                    && mouse.column < area.x.saturating_add(area.width)
                    && mouse.row >= area.y
                    && mouse.row < area.y.saturating_add(area.height);
                self.selection = inside
                    .then(|| self.cell_at(mouse.column, mouse.row))
                    .flatten()
                    .map(|pt| (pt, pt));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((start, _)) = self.selection {
                    if let Some(pt) = self.cell_at(mouse.column, mouse.row) {
                        self.selection = Some((start, pt));
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.finish_selection(),
            _ => {}
        }
    }

    /// 屏幕坐标 → 选区的 `(绝对显示行, 单元格列)`；超出消息流区域就贴到边上。
    fn cell_at(&self, column: u16, row: u16) -> Option<(u16, u16)> {
        let area = self.body;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let col = column.saturating_sub(area.x).min(area.width - 1);
        let line = row.saturating_sub(area.y).min(area.height - 1);
        Some((self.scroll_top.saturating_add(line), col))
    }

    /// 鼠标松开：花区文本写进剪贴板（Python 同款——松开即复制 + **右下角弹一条**临时提示）。
    fn finish_selection(&mut self) {
        let Some(text) = self.take_selection_text() else {
            return;
        };
        let copied = clipboard::copy_text(&text);
        let msg = if copied {
            format!("已复制 {} 字符到剪贴板", text.chars().count())
        } else {
            "复制失败：剪贴板不可用".to_string()
        };
        self.toast(msg, copied);
    }

    /// 弹一条右下角临时提示（`TOAST_TTL` 后自消）。
    fn toast(&mut self, text: impl Into<String>, ok: bool) {
        self.toast = Some(Toast {
            text: text.into(),
            ok,
            until: Instant::now() + TOAST_TTL,
        });
    }

    /// 右下角浮一条临时提示：`Clear` 先把底下的内容擦掉，再画一个小盒子。
    ///
    /// 贴着输入框上沿、屏幕右侧（`above` = 输入框的 y）——比盖住输入框更像「弹出」。
    /// 每帧重画一次（主循环 66ms 一 tick，所以到点最多差一帧就消失）。
    fn paint_toast(&self, frame: &mut Frame, area: Rect, above: u16, toast: &Toast) {
        let text = format!(" {} ", toast.text);
        let width = (Line::from(text.as_str()).width() as u16 + 2).min(area.width);
        let height = 3.min(area.height); // 上下边框 + 一行正文
        let rect = Rect {
            x: area.right().saturating_sub(width),
            y: above.saturating_sub(height),
            width,
            height,
        };
        let color = if toast.ok {
            self.palette.accent
        } else {
            self.palette.fail
        };
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, self.palette.style_assistant())))
                .block(Block::bordered().border_style(Style::default().fg(color))),
            rect,
        );
    }

    /// 取走选区文本（不碰剪贴板——这一半是纯逻辑，好测）。空选区返回 `None`。
    fn take_selection_text(&mut self) -> Option<String> {
        let ((r1, c1), (r2, c2)) = self.selection.take()?;
        let ((r1, c1), (r2, c2)) = if (r1, c1) <= (r2, c2) {
            ((r1, c1), (r2, c2))
        } else {
            ((r2, c2), (r1, c1))
        };
        let text = self
            .layout
            .slice_text(r1 as usize, c1 as usize, r2 as usize, c2 as usize);
        (!text.is_empty()).then_some(text)
    }

    /// 框选高亮：选中的单元格换成 accent 底 + accent_text 字（同终端选择 / 输入框选中）。
    ///
    /// 在消息流渲染**之后**直接改 buf 里的单元格样式——纯几何（跟屏幕上真正显示的行列一致），
    /// 不需要知道折行是怎么折的。
    fn paint_selection(&self, frame: &mut Frame, body: Rect) {
        let Some(((r1, c1), (r2, c2))) = self.selection else {
            return;
        };
        let ((r1, c1), (r2, c2)) = if (r1, c1) <= (r2, c2) {
            ((r1, c1), (r2, c2))
        } else {
            ((r2, c2), (r1, c1))
        };
        let style = Style::default()
            .bg(self.palette.accent)
            .fg(self.palette.accent_text);
        let buf = frame.buffer_mut();
        for r in r1..=r2 {
            let Some(line) = r.checked_sub(self.scroll_top) else {
                continue;
            };
            if line >= body.height {
                break; // 越往下越不可见，后面都不用画
            }
            let from = if r == r1 { c1 } else { 0 };
            let to = if r == r2 {
                c2.saturating_add(1) // 光标压住的那个单元格也算
            } else {
                body.width
            };
            for x in from..to.min(body.width) {
                buf[(body.x + x, body.y + line)].set_style(style);
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match (key.code, ctrl, shift) {
            (KeyCode::Char('c'), true, _) => self.should_quit = true,
            (KeyCode::Char('d'), true, _) if self.input.is_empty() => self.should_quit = true,
            (KeyCode::Char('g'), true, _) => self.paste_clipboard(),
            // Ctrl+A = 全选（对齐 Python/Textual TextArea；控件默认把它当跳到行首）
            (KeyCode::Char('a'), true, _) => self.input.select_all(),
            (KeyCode::Esc, _, _) => self.on_escape(),
            // ⇧⏎ 在多数终端里就是 LF（= Ctrl+J），两个都认
            (KeyCode::Enter, _, true) | (KeyCode::Char('j'), true, _) => self.input.newline(),
            (KeyCode::Enter, _, false) => self.submit(),
            (KeyCode::PageUp, ..) => self.scroll(-8),
            (KeyCode::PageDown, ..) => self.scroll(8),
            // 补全面板开着时：Tab 接受候选、↑/↓ 切候选（对齐 Python）
            (KeyCode::Tab, ..) if self.palette_visible() => self.palette_accept(),
            (KeyCode::Up, ..) if self.palette_visible() => self.palette_move(-1),
            (KeyCode::Down, ..) if self.palette_visible() => self.palette_move(1),
            (KeyCode::Up, ..) if self.input.is_empty() || self.input.history_active() => {
                self.input.history_prev()
            }
            (KeyCode::Down, ..) if self.input.history_active() => self.input.history_next(),
            _ => {
                self.input.handle_key(key);
            }
        }
    }

    fn on_turn_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Models(models) => self.models = models,
            UiEvent::Notice(text) => self.push_notice(&text),
            UiEvent::Turn(TurnEvent::Reasoning(_)) => {
                let since = *self.thought_started.get_or_insert_with(Instant::now);
                self.activity = Activity::Thinking { since };
            }
            UiEvent::Turn(TurnEvent::AssistantText(delta)) => {
                self.settle_thought();
                self.activity = Activity::Streaming;
                Cell::push_assistant_text(&mut self.cells, &delta);
            }
            UiEvent::Turn(TurnEvent::Answer(text)) => {
                self.settle_thought();
                Cell::push_assistant_text(&mut self.cells, &text);
            }
            UiEvent::Turn(TurnEvent::ToolCall {
                name, arguments, ..
            }) => {
                self.settle_thought();
                self.activity = Activity::Tool {
                    since: Instant::now(),
                };
                self.cells.push(Cell::Tool {
                    name,
                    summary: history::tool_summary(&arguments),
                    status: Status::Running,
                    body: None,
                    manual: false,
                });
            }
            UiEvent::Turn(TurnEvent::ToolResult { name, content, .. }) => {
                // 被取消的工具回的是哨兵文本（不是 `[exit=]` 头）→ 标 ⏹ 而不是 ✓
                let status = history::tool_status(&content);
                Cell::finish_tool(&mut self.cells, &name, status, &content);
                self.activity = Activity::Tool {
                    since: Instant::now(),
                };
            }
            UiEvent::ShellDone(status, body) => {
                self.busy = false;
                self.snapshot.busy = false;
                self.activity = Activity::Idle;
                Cell::finish_tool(&mut self.cells, "shell", status, &body);
            }
            UiEvent::TurnDone(result, snapshot) => {
                self.settle_thought();
                self.busy = false;
                self.activity = Activity::Idle;
                match result {
                    Ok(text) if text == crate::cancel::CANCEL_TEXT => {
                        // 没轮到的工具调用不会有结果事件 → 还挂着 `•` 的行结算成 ⏹
                        Cell::cancel_running(&mut self.cells);
                        // 取消成功：历史里已经有一条终止消息（在 session 里写的），这里只提示一下
                        self.push_notice("已停止本回合（历史里留了一条终止记录）");
                    }
                    Ok(text) => {
                        // 非流式时正文是由 Answer 事件推的；这里只防 “既没流式也没 Answer” 的极端情况
                        if !text.is_empty()
                            && !self.cells.iter().rev().take(3).any(
                                |c| matches!(c, Cell::Assistant { text: t, .. } if !t.is_empty()),
                            )
                        {
                            Cell::push_assistant_text(&mut self.cells, &text);
                        }
                    }
                    Err(e) => self.cells.push(Cell::Error(format!("{e}"))),
                }
                // 回合里若新建了助手 cell，去掉尾部的空行（没有正文的回合不留空块）
                if let Some(Cell::Assistant { text, .. }) = self.cells.last() {
                    if text.trim().is_empty() {
                        self.cells.pop();
                    }
                }
                self.snapshot = snapshot;
                self.snapshot.busy = false;
                self.input.move_cursor_end();
            }
        }
    }

    /// 思考结算：首个正文/工具/回合结束时把耗时留成一行（太短的不留）。
    fn settle_thought(&mut self) {
        let Some(start) = self.thought_started.take() else {
            return;
        };
        let elapsed = Instant::now().saturating_duration_since(start);
        if elapsed >= THOUGHT_TRACE_MIN {
            self.cells.push(Cell::Thought(elapsed));
        }
    }

    // ---------------------------------------------------------------- 提交与命令

    fn submit(&mut self) {
        // 半截命令：回车先接受面板里高亮的候选，再按完整命令提交（对齐 Python）
        if self.palette_visible() && !palette::is_complete_command(&self.input.text()) {
            self.palette_accept();
        }
        let text = self.input.take();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.busy {
            // 回合进行中只放行 `/stop`（等价 Esc）；其它命令/输入等回合结束
            if trimmed == "/stop" {
                self.request_stop();
            } else {
                self.push_notice("回合进行中（Esc 或 /stop 停止）");
            }
            return;
        }
        if let Some(cmd) = trimmed.strip_prefix('/') {
            // `/` 开头但不是已知命令（粘进来的绝对路径最常见）→ 当**普通消息**发出去
            if palette::is_known_command(trimmed) {
                self.command(cmd);
                return;
            }
        }
        if let Some(cmd) = trimmed.strip_prefix('!') {
            self.run_shell(cmd.trim());
            return;
        }
        self.cells.push(Cell::User(text.clone()));
        self.start_turn(text);
    }

    fn command(&mut self, cmd: &str) {
        let (name, arg) = match cmd.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (cmd, ""),
        };
        match name {
            "help" => {
                let text = palette::help_text();
                self.push_notice(&text);
            }
            "exit" | "quit" => self.should_quit = true,
            // 与 `Esc` 等价（回合进行中才有效）
            "stop" => self.request_stop(),
            "status" => self.with_session("状态", |s| s.usage_report()),
            "model" => {
                if arg.is_empty() {
                    self.with_session("当前模型", |s| s.config.model.clone());
                } else {
                    let name = arg.to_string();
                    self.with_session_mut("切换模型", move |s| s.set_model(&name));
                }
            }
            "thinking" => {
                if arg.is_empty() {
                    self.with_session("思考深度", |s| s.config.reasoning_effort.clone());
                } else {
                    let level = arg.to_string();
                    self.with_session_mut("切换思考深度", move |s| s.set_reasoning_effort(&level));
                }
            }
            "compact" => {
                let mode = match arg {
                    "tools" => crate::context::CompactMode::Tools,
                    "turns" => crate::context::CompactMode::Turns,
                    _ => crate::context::CompactMode::Auto,
                };
                self.with_session_mut("压缩", move |s| {
                    let stats = s.compact(mode);
                    format!(
                        "节省约 {} tokens（工具级 {} 条 / 轮次级 {} 轮 / 会话级 {}）{}",
                        stats.saved_tokens,
                        stats.tools,
                        stats.turns,
                        stats.session,
                        stats.skipped.map(|s| format!("（{s}）")).unwrap_or_default()
                    )
                });
            }
            "clear" | "reset" => {
                self.cells.clear();
                self.scroll_from_bottom = 0;
                self.with_session_mut("清空历史", |s| {
                    s.reset();
                    "已清空对话历史（窗口归档未接）".to_string()
                });
            }
            "save" => self.with_session_mut("保存", |s| match s.save() {
                Ok(()) => format!("已保存 {}", s.path.display()),
                Err(e) => format!("保存失败: {e}"),
            }),
            "paste" => self.paste_clipboard(),
            other => self.push_notice(&format!("未知命令：/{other}（/help 看列表）")),
        }
    }

    /// 回合进行中要读会话：锁被任务占着就提示，别卡住界面。
    fn with_session<F: FnOnce(&Session) -> String>(&mut self, what: &str, f: F) {
        // 先把结果取出来（借用结束后再碰 self）
        let outcome = match self.session.try_lock() {
            Ok(guard) => Ok(f(&guard)),
            Err(_) => Err(()),
        };
        match outcome {
            Ok(text) => self.push_notice(&text),
            Err(()) => self.push_notice(&format!("回合进行中，{what} 稍后再试")),
        }
    }

    fn with_session_mut<F: FnOnce(&mut Session) -> String>(&mut self, what: &str, f: F) {
        let outcome = match self.session.try_lock() {
            Ok(mut guard) => {
                let text = f(&mut guard);
                let snapshot = (
                    guard.config.model.clone(),
                    guard.config.reasoning_effort.clone(),
                    guard.usage.prompt_tokens,
                    guard.usage.calls,
                );
                Ok((text, snapshot))
            }
            Err(_) => Err(()),
        };
        match outcome {
            Ok((text, (model, effort, tokens, calls))) => {
                self.snapshot.model = model;
                self.effort = effort;
                self.snapshot.prompt_tokens = tokens;
                self.snapshot.calls = calls;
                self.push_notice(&text);
            }
            Err(()) => self.push_notice(&format!("回合进行中，{what} 稍后再试")),
        }
    }

    /// 起一个回合（后台 task；事件经 channel 回来）。
    fn start_turn(&mut self, input: String) {
        self.busy = true;
        self.snapshot.busy = true;
        self.thought_started = None;
        self.activity = Activity::Waiting {
            since: Instant::now(),
        };
        self.scroll_from_bottom = 0; // 回到贴底跟随
        let session = self.session.clone();
        let tx = self.tx.clone();
        self.cancel = Cancel::new();
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            let mut guard = session.lock().await;
            let event_tx = tx.clone();
            let mut on_event = move |event: TurnEvent| {
                let _ = event_tx.send(UiEvent::Turn(event));
            };
            let result = guard.aturn(&input, &mut on_event, &cancel).await;
            let snapshot = Snapshot {
                model: guard.config.model.clone(),
                cwd: cwd_text(),
                prompt_tokens: guard.usage.prompt_tokens,
                budget: guard.config.context_budget(),
                calls: guard.usage.calls,
                busy: false,
            };
            let _ = tx.send(UiEvent::TurnDone(result, snapshot));
        });
    }

    // ---------------------------------------------------------------- 手动 shell（`!cmd`）

    /// `!cmd`：直接执行 shell —— **不经过 LLM、不进会话上下文**（对齐 Python `_run_shell`）。
    ///
    /// 命令行回显借工具行的形状（摘要位置放 `$ cmd`），结算时强制展开正文（`manual`，
    /// 简洁模式也不例外）。跟回合一样占 `busy`：`Esc` / `/stop` 能中断（杀整个进程组）。
    fn run_shell(&mut self, cmd: &str) {
        if cmd.is_empty() {
            return;
        }
        self.cells.push(Cell::Tool {
            name: "shell".into(),
            summary: format!("$ {cmd}"),
            status: Status::Running,
            body: None,
            manual: true,
        });
        self.busy = true;
        self.snapshot.busy = true;
        self.activity = Activity::Tool {
            since: Instant::now(),
        };
        self.scroll_from_bottom = 0; // 回到贴底跟随
        self.cancel = Cancel::new();
        let cancel = self.cancel.clone();
        let cmd = cmd.to_string();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let (status, body) = exec_shell(&cmd, &cancel).await;
            let _ = tx.send(UiEvent::ShellDone(status, body));
        });
    }

    fn on_escape(&mut self) {
        if self.selection.is_some() {
            // 鼠标在窗口外松开时可能收不到 Up 事件 → 高亮会一直留着，Esc 先把选区收掉
            self.selection = None;
        } else if self.palette_visible() {
            // 面板开着就先收面板（对齐 Python：Esc 第一下只收面板，不当 /stop）
            self.palette_hidden = true;
        } else if self.busy {
            self.request_stop();
        } else if !self.input.is_empty() {
            self.input.clear();
        }
    }

    /// 请求停止本回合（`Esc` 与 `/stop` 共用）。
    fn request_stop(&mut self) {
        if !self.busy {
            self.push_notice("当前没有正在跑的回合");
            return;
        }
        // 取消当前回合：模型请求会被打断、shell 会杀掉进程组，收尾时历史里留一条终止消息
        self.cancel.cancel();
        self.activity = Activity::Stopping {
            since: Instant::now(),
        };
        self.push_notice("已请求停止…");
    }

    // ---------------------------------------------------------------- 命令补全

    /// 当前输入对应的补全候选（非 `/` 开头为空）。
    fn palette_matches(&self) -> Vec<(String, String)> {
        palette::matches(
            &self.input.text(),
            &self.models,
            &self.snapshot.model,
            &self.effort,
        )
    }

    /// 面板此刻是否可见（有候选，且没被 `Esc` 收起）。
    fn palette_visible(&self) -> bool {
        !self.palette_hidden && !self.palette_matches().is_empty()
    }

    /// `Tab`：接受高亮候选（把输入换成完整命令）。
    fn palette_accept(&mut self) {
        let matches = self.palette_matches();
        if matches.is_empty() {
            return;
        }
        let index = self.palette_index.min(matches.len() - 1);
        let cmd = matches[index].0.clone();
        self.input.set_text(&cmd);
    }

    /// `↑`/`↓`：在候选里移动高亮。
    fn palette_move(&mut self, delta: i32) {
        let len = self.palette_matches().len();
        if len == 0 {
            return;
        }
        self.palette_index = (self.palette_index as i32 + delta).clamp(0, len as i32 - 1) as usize;
    }

    /// 后台拉一次可用模型（`/model ` 的候选）；失败就保持空表（面板自然不弹）。
    fn spawn_fetch_models(&self) {
        let session = self.session.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut guard = session.lock().await;
            if let Ok(models) = guard.fetch_models().await {
                let _ = tx.send(UiEvent::Models(models));
            }
        });
    }

    /// `Ctrl+G` / `/paste`：**只**把剪贴板里的图片变成路径（对齐 Python `_paste_image`）。
    ///
    /// 纯文本不走这里 —— 终端自己的粘贴键（`⌘V` / `Ctrl+Shift+V`）经 bracketed paste
    /// 直接进输入框，见 `on_terminal_event` 的 `Event::Paste`。
    fn paste_clipboard(&mut self) {
        match clipboard::paste_image() {
            Some(path) => {
                self.input.insert(&path);
                self.push_notice("已插入图片路径（回车即 read 它）");
            }
            None => self.push_notice(&format!("剪贴板里没有图片{}", clipboard::no_image_hint())),
        }
    }

    fn scroll(&mut self, delta: i32) {
        let next = self.scroll_from_bottom as i32 + delta;
        self.scroll_from_bottom = next.max(0) as u16;
    }

    fn push_notice(&mut self, text: &str) {
        self.cells.push(Cell::Notice(text.to_string()));
    }

    // ---------------------------------------------------------------- 渲染

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();

        // 输入一变就恢复被 `Esc` 收起的补全面板（对齐 Python 的 on_text_area_changed）
        let text = self.input.text();
        if text != self.last_input {
            self.last_input = text;
            self.palette_hidden = false;
        }
        let matches = self.palette_matches();
        self.palette_index = if matches.is_empty() {
            0
        } else {
            self.palette_index.min(matches.len() - 1)
        };
        let panel = if self.palette_hidden {
            Vec::new()
        } else {
            palette::panel_lines(&matches, self.palette_index, &self.palette)
        };

        // 宽度要传进去：输入框现在是**软换行**的，长行会折成多行、高度跟着长
        // （`Borders::TOP` 不占左右格，所以可用宽 = 整区宽度）。
        let input_height = self.input.desired_height(area.width);
        let [header, body, palette_area, input, hint] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(panel.len() as u16),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .areas(area);

        let now = Instant::now();
        frame.render_widget(
            Paragraph::new(status::header_line(
                &self.palette,
                &self.snapshot,
                self.activity,
                self.frame,
                now,
                area.width,
            )),
            header,
        );

        let layout = history::layout(&mut self.cells, &self.palette, body.width, self.lean);
        let total = layout.rows.len();
        // 滚动偏移按**显示行**算（`Paragraph::scroll` 跳过的是折行之后的行）
        let bottom = total.saturating_sub(body.height as usize);
        let offset = bottom.saturating_sub(self.scroll_from_bottom as usize);
        // 鼠标框选要知道「屏幕坐标 ↔ 哪条显示行」，记下这一帧的几何信息与排版
        self.scroll_top = offset.min(u16::MAX as usize) as u16;
        self.body = body;
        let lines: Vec<Line<'static>> = layout.rows.iter().map(|r| r.line.clone()).collect();
        self.layout = layout;
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll_top, 0)),
            body,
        );
        self.paint_selection(frame, body);

        if !panel.is_empty() {
            frame.render_widget(Paragraph::new(panel), palette_area);
        }
        self.input.render(frame, input, &self.palette);
        frame.render_widget(
            Paragraph::new(status::hint_line(&self.palette, self.busy)),
            hint,
        );
        // 右下角临时提示：过期的先收掉（每帧一次，所以到点最多差一帧就消）；
        // 活着的画在**所有控件之后**——它是浮层。
        if self.toast.as_ref().is_some_and(|t| t.until <= now) {
            self.toast = None;
        }
        if let Some(toast) = &self.toast {
            self.paint_toast(frame, area, input.y, toast);
        }
    }

    /// 退出前保存会话（回合在跑就**先请求取消**，再等最多 5 秒拿锁）。
    ///
    /// 返回要提示的一行：此时终端还是 raw mode，写 stderr 会砸花屏幕（见 `crate::log`），
    /// 所以交给 `tui::run` 在 `restore()` 之后打印。
    async fn shutdown(&self) -> Option<String> {
        if self.busy {
            // 不退出的情况下等回合自然结束可能要几分钟（比如 shell 在跑长命令）——
            // 取消后模型请求 / 工具进程会立刻收尾，历史也就能落盘。
            self.cancel.cancel();
        }
        let locked = tokio::time::timeout(Duration::from_secs(5), self.session.lock()).await;
        let Ok(guard) = locked else {
            return Some("[session] 回合仍在进行，跳过保存".to_string());
        };
        if let Err(e) = guard.save() {
            return Some(format!("[session] 保存失败: {e}"));
        }
        None
    }

    // ---------------------------------------------------------------- 测试辅助

    /// 用 `TestBackend` 渲染一帧并返回屏幕文本（快照测试用）。
    #[cfg(test)]
    pub fn render_to_string(&mut self, width: u16, height: u16) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| self.render(frame)).expect("draw");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .filter_map(|x| {
                        let symbol = buffer[(x, y)].symbol();
                        // 宽字符（中文等）的后半格 symbol 是空串 → 跳过，不然会拼出“你 好”
                        (!symbol.is_empty()).then(|| symbol.to_string())
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 跑一条手动 `!cmd`，返回（状态，结果正文）。
///
/// 与工具层的 `shell` 同款：独立进程组 + 取消时 killpg（只杀 `sh` 本体的话，持有 stdout
/// 管道写端的子孙会让读端不 EOF、等不到结束）；stderr 合进 stdout 保证顺序。
/// 正文格式对齐 Python `_show_shell_result`：`[exit=N]\n\n<输出>`（取消 → `[exit=cancelled]`
/// + 一行 `[用户手动终止]`）。
async fn exec_shell(cmd: &str, cancel: &Cancel) -> (Status, String) {
    #[cfg(unix)]
    let mut command = {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };
    #[cfg(not(unix))]
    let mut command = {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    };
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        command.process_group(0); // 独立进程组：取消时 killpg 连子孙一起杀
        unsafe {
            command.pre_exec(|| {
                if libc::dup2(1, 2) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return (Status::Fail, format!("[exit=error]\n\n启动失败: {e}")),
    };
    let stdout = child.stdout.take().expect("piped");
    // tokio 的 id() 在进程被回收后返回 None；0 表示“拿不到 pid”（不杀组）
    let pid = child.id().unwrap_or(0);
    let mut chunks: Vec<String> = Vec::new();

    let read_and_wait = async {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            chunks.push(String::from_utf8_lossy(&buf).into_owned());
        }
        child.wait().await
    };
    // 取消分支不能碰 `chunks`/`child`（都被这个 future 借着）——先收尾，完了再放开它。
    let mut reading = Box::pin(read_and_wait);
    let mut cancelled = false;
    let outcome = tokio::select! {
        waited = &mut reading => Some(waited),
        _ = cancel.cancelled() => {
            #[cfg(unix)]
            if pid != 0 {
                unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
            }
            cancelled = true;
            None
        }
    };
    drop(reading);

    let out = chunks.concat();
    if cancelled {
        // 与工具层同款：kill 后 wait 限时 3s（D-state 进程不能把界面拖死）
        let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        return (
            Status::Cancelled,
            format!(
                "[exit=cancelled]\n\n{}\n[{CANCEL_TEXT}]",
                out.trim_end_matches('\n')
            ),
        );
    }
    let status = match outcome.expect("未取消时必然等到结果") {
        Ok(status) => status,
        Err(e) => return (Status::Fail, format!("[exit=error]\n\n等待子进程失败: {e}")),
    };
    // 退出码：被信号杀死时 Python 给负数，这里取 -1（信息量等价，都是“非正常退出”）
    let code = status.code().unwrap_or(-1);
    let mark = if code == 0 { Status::Ok } else { Status::Fail };
    (mark, format!("[exit={code}]\n\n{}", out.trim_end_matches('\n')))
}

fn cwd_text() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// 粘贴文本里的换行统一成 `\n`（bracketed paste 里可能是 CRLF；孤立 `\r` 当换行）。
fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_app() -> App {
        app_with_rx().0
    }

    /// 建 App + 事件接收端（需要等后台任务（回合 / `!cmd`）回传事件时用）。
    fn app_with_rx() -> (App, UnboundedReceiver<UiEvent>) {
        let cfg = Config {
            model: "deepseek-flash".into(),
            ..Default::default()
        };
        let llm = crate::llm::LlmClient::new(&cfg).expect("client");
        let tools = crate::tools::ToolRegistry::new(Default::default());
        App::new(Session::ephemeral(&cfg, llm, tools))
    }

    /// 断言用：把空白全去掉再比（`TestBackend` buffer 里宽字符占了两格，拼出来带空格）。
    fn squash(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// 渲染一帧并留下 `Buffer`（要断言**颜色**时用；`render_to_string` 只留文本）。
    fn render_buffer(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn lean_hides_successful_tool_body_unless_disabled() {
        let mut app = test_app();
        assert!(app.lean, "配置默认开简洁模式");
        app.cells.push(Cell::Tool {
            name: "read".into(),
            summary: "a.rs".into(),
            status: Status::Ok,
            body: Some("fn main() {}".into()),
            manual: false,
        });
        let flat = squash(&app.render_to_string(80, 24));
        assert!(flat.contains("✓read(a.rs)"), "{flat}");
        assert!(!flat.contains("fnmain()"), "简洁模式收起成功正文：{flat}");

        app.lean = false;
        let flat = squash(&app.render_to_string(80, 24));
        assert!(flat.contains("fnmain()"), "盒子式才展示正文：{flat}");
    }

    #[test]
    fn palette_panel_shows_matches_tab_accepts_and_esc_hides() {
        let mut app = test_app();
        app.input.insert("/co");
        let screen = app.render_to_string(80, 24);
        let flat = squash(&screen);
        assert!(flat.contains("▸/compact"), "面板高亮首项：{screen}");
        assert!(flat.contains("/compactturns"), "多词命令也在候选里：{screen}");

        // Tab 接受高亮候选
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input.text(), "/compact");
        // ↓ 切到第二个候选再 Tab 接受
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input.text(), "/compact tools");

        // Esc 先收面板（不当 /stop），输入一变又回来
        app.input.set_text("/co");
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.palette_visible(), "Esc 收起面板");
        let screen = app.render_to_string(80, 24);
        assert!(!squash(&screen).contains("▸/compact"), "收起后不渲染：{screen}");
        app.input.insert("m");
        let screen = app.render_to_string(80, 24);
        assert!(squash(&screen).contains("▸/compact"), "输入一变又回来：{screen}");
    }

    #[test]
    fn enter_completes_partial_command_instead_of_sending_it() {
        let mut app = test_app();
        app.input.insert("/statu");
        app.submit();
        assert!(
            app.cells.iter().all(|c| !matches!(c, Cell::User(_))),
            "半截命令不应作为消息发出去"
        );
        assert!(matches!(app.cells.last(), Some(Cell::Notice(_))));
    }

    #[test]
    fn renders_header_history_and_hint() {
        let mut app = test_app();
        app.cells.push(Cell::User("你好".into()));
        Cell::push_assistant_text(&mut app.cells, "**在**的");
        app.cells.push(Cell::Thought(Duration::from_millis(2100)));
        app.cells.push(Cell::Tool {
            name: "shell".into(),
            summary: "pwd".into(),
            status: Status::Ok,
            body: Some("/tmp".into()),
            manual: false,
        });
        app.cells.push(Cell::Notice("粘贴成功".into()));

        let screen = app.render_to_string(80, 24);
        let flat = squash(&screen);
        assert!(flat.contains("deepseek-flash"), "顶栏有模型：{screen}");
        assert!(flat.contains("›你好"), "{screen}");
        assert!(flat.contains("在的"), "助手正文：{screen}");
        assert!(flat.contains("Thoughtfor2.1s"), "思考耗时：{screen}");
        assert!(flat.contains("✓shell(pwd)"), "工具行：{screen}");
        assert!(!flat.contains("/tmp"), "默认简洁模式不展成功正文：{screen}");
        assert!(flat.contains("发送"), "底栏提示：{screen}");
        assert!(flat.contains("粘贴成功"), "{screen}");
    }

    /// 空白新会话：顶栏就显示 `0/<预算> (0.0%)`，消息流里也不该有键位提示行（底栏那份才是键位说明）。
    #[test]
    fn fresh_screen_shows_zero_usage_and_no_notice() {
        let mut app = test_app();
        let screen = app.render_to_string(80, 24);
        let flat = squash(&screen);
        assert!(!flat.contains("0calls"), "不再显示 0 calls：{screen}");
        assert!(flat.contains("0/920,576(0.0%)"), "新会话按 0 算：{screen}");
        assert!(
            !flat.contains("Tab补全命令"),
            "启动不推键位提示行：{screen}"
        );
        assert!(flat.contains("发送"), "底栏提示还在：{screen}");
    }

    /// resume：已有历史要回放进消息流（否则打开就是一片空白，看着像丢了对话）。
    #[test]
    fn resume_replays_history_into_the_message_stream() {
        let cfg = Config {
            model: "deepseek-flash".into(),
            ..Default::default()
        };
        let llm = crate::llm::LlmClient::new(&cfg).expect("client");
        let tools = crate::tools::ToolRegistry::new(Default::default());
        let mut session = Session::ephemeral(&cfg, llm, tools);
        session.messages.push(crate::llm::Message::user("上次的问题"));
        session.messages.push(crate::llm::Message {
            role: "assistant".into(),
            content: Some(crate::llm::Content::Text("上次的回答".into())),
            ..Default::default()
        });
        let mut app = App::new(session).0;
        assert!(
            app.cells.iter().any(|c| matches!(c, Cell::User(_))),
            "历史消息要回放成单元格"
        );
        let flat = squash(&app.render_to_string(80, 24));
        assert!(flat.contains("›上次的问题"), "{flat}");
        assert!(flat.contains("上次的回答"), "{flat}");
    }

    #[test]
    fn thinking_activity_shows_with_elapsed_time() {
        let mut app = test_app();
        app.activity = Activity::Thinking {
            since: Instant::now() - Duration::from_millis(1500),
        };
        let screen = app.render_to_string(80, 10);
        assert!(screen.contains("Thinking"), "{screen}");
        assert!(screen.contains("1.5s"), "计数要从计时起点算：{screen}");
    }

    #[test]
    fn paste_normalizes_newlines_and_log_warnings_become_notices() {
        let mut app = test_app();
        // CRLF / 孤立 CR 都当成换行
        app.on_terminal_event(Event::Paste("a\r\nb\rc".into()));
        assert_eq!(app.input.text(), "a\nb\nc");

        app.on_turn_event(UiEvent::Notice("[retry] 第 1 次".into()));
        assert!(
            matches!(app.cells.last(), Some(Cell::Notice(t)) if t.contains("retry")),
            "告警要进消息流，不是 stderr"
        );
    }

    #[test]
    fn shift_enter_and_ctrl_j_break_line_instead_of_submitting() {
        let mut app = test_app();
        app.input.insert("第一行");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        app.input.insert("第三行");
        assert_eq!(app.input.text(), "第一行\n\n第三行");
        assert!(
            app.cells.iter().all(|c| !matches!(c, Cell::User(_))),
            "换行不该把内容发出去"
        );
    }

    #[test]
    fn input_keeps_multiline_text_and_fills_its_height() {
        let mut app = test_app();
        app.input.insert("第一行");
        app.input.newline();
        app.input.insert("第二行");
        let screen = app.render_to_string(60, 24);
        let flat = squash(&screen);
        assert!(flat.contains("第一行"), "{screen}");
        assert!(flat.contains("第二行"), "输入框是多行的：{screen}");
    }

    #[test]
    fn input_scrolls_to_keep_cursor_visible_when_taller_than_max() {
        let mut app = test_app();
        for i in 0..15 {
            if i > 0 {
                app.input.newline();
            }
            app.input.insert(&format!("第{i}行"));
        }
        let screen = app.render_to_string(60, 24);
        let flat = squash(&screen);
        assert!(
            flat.contains("第14行"),
            "超过高度上限（9 行）时输入框要自己滚到光标：\n{screen}"
        );
    }

    #[test]
    fn ctrl_a_selects_all_so_next_input_replaces_it() {
        let mut app = test_app();
        app.input.insert("第一行\n第二行");
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.input.insert("换掉");
        assert_eq!(app.input.text(), "换掉", "Ctrl+A 全选后输入应替换原文");
    }

    /// 造一个鼠标事件（框选测试用）。
    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// 拖选从 `(x, y)` 到 `(x2, y2)`（先点后拖，不松手）。
    fn drag(app: &mut App, from: (u16, u16), to: (u16, u16)) {
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), from.0, from.1));
        app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1));
    }

    #[test]
    fn copy_toast_floats_at_the_bottom_right_then_expires() {
        let mut app = test_app();
        app.toast("已复制 146 字符到剪贴板", true);
        let screen = app.render_to_string(48, 12);
        let flat = squash(&screen);
        assert!(flat.contains("已复制146字符到剪贴板"), "弹出提示：\n{screen}");
        // 右下角：盒子最后一行的右端就是屏幕右端，且**不盖住输入框**（输入框上沿是 y=8）
        let lines: Vec<&str> = screen.lines().collect();
        let box_bottom = lines.iter().rposition(|l| l.contains('已')).unwrap();
        assert!(box_bottom < 8, "提示要浮在输入框上方：\n{screen}");
        assert!(
            lines[box_bottom].trim_end().ends_with('│'),
            "贴着屏幕右侧（右边框就在最后一列）：\n{screen}"
        );
        // 到点自消（把截止时间拨到过去，下一帧就没了）
        let past = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        app.toast.as_mut().unwrap().until = past;
        let screen = app.render_to_string(48, 12);
        assert!(!squash(&screen).contains("已复制"), "3 秒后自动消失：\n{screen}");
        assert!(app.toast.is_none(), "过期就清掉状态");
    }

    #[test]
    fn mouse_drag_copies_wrapped_line_as_one_line() {
        let mut app = test_app();
        app.cells
            .push(Cell::User("hello world 这是一个很长的句子用来测试折行".into()));
        app.render_to_string(30, 12); // 先渲染一帧：App 才会记下区域 / 偏移 / 排版
        let body = app.body;
        drag(
            &mut app,
            (body.x, body.y),
            (body.x + body.width - 1, body.y + body.height - 1),
        );
        let text = app.take_selection_text().expect("拖过内容就有选区");
        assert_eq!(
            text.trim_end(),
            "› hello world 这是一个很长的句子用来测试折行",
            "软换行的长行复制回来是一行（字符一个不丢）：{text:?}"
        );
    }

    #[test]
    fn dragging_paints_the_selection_in_accent() {
        let mut app = test_app();
        app.cells.push(Cell::User("选中我".into()));
        app.render_to_string(30, 12);
        let (x, y) = (app.body.x, app.body.y);
        drag(&mut app, (x, y), (x + 3, y));
        let buf = render_buffer(&mut app, 30, 12);
        assert_eq!(
            buf[(x, y)].style().bg,
            Some(app.palette.accent),
            "选中的单元格换成 accent 底（终端选择的同款）"
        );
        assert_ne!(
            buf[(x + 20, y)].style().bg,
            Some(app.palette.accent),
            "选区外的单元格不受影响"
        );
    }

    #[test]
    fn selection_uses_absolute_rows_so_it_follows_scrolling() {
        let mut app = test_app();
        for i in 0..20 {
            app.cells.push(Cell::Notice(format!("第{i}行")));
        }
        app.render_to_string(30, 12);
        app.scroll(-8); // 往上看（PgUp / 滚轮的效果）
        app.render_to_string(30, 12); // 重渲染：App 才会记下新的 scroll_top
        assert!(app.scroll_top > 0, "先真的滚上去了");
        let (x, y) = (app.body.x, app.body.y);
        let bottom = y + app.body.height - 1;
        // 从第二条可见行拖到底：滚上去后可见的已经不是最后几行
        drag(&mut app, (x, y + 1), (x + 5, bottom));
        let text = app.take_selection_text().unwrap_or_default();
        let first = text
            .lines()
            .find(|l| l.starts_with("· 第"))
            .unwrap_or_default()
            .to_string();
        assert!(first.starts_with("· 第"), "选到的是当前可见的那一行：{text:?}");
        assert!(
            first != "· 第19行",
            "滚上去之后选到的不该是最后一行：{text:?}"
        );
    }

    #[test]
    fn escape_clears_the_selection_and_click_outside_does_not_start_one() {
        let mut app = test_app();
        app.cells.push(Cell::User("abc".into()));
        app.render_to_string(30, 12);
        // 点输入框 / 底栏那一带（消息流区域之外）不该起选区
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 11));
        assert!(app.selection.is_none(), "区域外按下不算选区");
        // 区域内拖选，然后 Esc 收掉
        let (x, y) = (app.body.x, app.body.y);
        drag(&mut app, (x, y), (x + 2, y));
        assert!(app.selection.is_some());
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.selection.is_none(), "Esc 先收选区");
    }

    #[test]
    fn input_border_highlights_focus_and_shell_mode() {
        let mut app = test_app();
        // 布局（高 12）：header 1 行 + body(Min) + 输入框 3 行 + 底栏 1 行 → 上边框在 y=8
        let (w, h, border_y) = (40u16, 12u16, 8u16);
        let accent = app.palette.accent;
        let tool = app.palette.tool;

        let buf = render_buffer(&mut app, w, h);
        let fg = |x: u16| buf[(x, border_y)].style().fg;
        assert_eq!(fg(0), Some(accent), "聚焦时输入框上边框是强调色");
        assert_eq!(fg(w - 1), Some(accent), "整条边框线都高亮，不只开头");

        app.input.insert("!pwd");
        let buf = render_buffer(&mut app, w, h);
        assert_eq!(
            buf[(0, border_y)].style().fg,
            Some(tool),
            "输入以 `!` 开头（shell 模式）切工具色"
        );
    }

    #[test]
    fn single_line_input_keeps_one_blank_row_above_hint() {
        let mut app = test_app();
        app.input.insert("只有一行");
        let screen = app.render_to_string(60, 24);
        let lines: Vec<&str> = screen.lines().collect();
        let last = lines.len() - 1;
        assert!(squash(lines[last]).contains("发送"), "底栏在最后一行：\n{screen}");
        assert!(
            squash(lines[last - 1]).is_empty(),
            "单行文本下方留一行空白（默认 3 行高），不贴着底栏：\n{screen}"
        );
        assert!(
            squash(lines[last - 2]).contains("只有一行"),
            "输入文本在空白行的上一行：\n{screen}"
        );
    }

    #[test]
    fn long_input_line_wraps_instead_of_scrolling() {
        let mut app = test_app();
        app.input.insert(&"a".repeat(200));
        let screen = app.render_to_string(40, 24);
        let visible = screen.chars().filter(|c| *c == 'a').count();
        assert!(
            visible > 120,
            "长行要折行显示（水平滚动只露 40 个）：
{screen}"
        );
    }

    #[test]
    fn wrapped_input_scrolls_to_keep_cursor_visible() {
        let mut app = test_app();
        // 宽 40 → 25 个显示行，远超 9 行上限 → 控件自己滚到光标（光标在尾部）
        app.input.insert(&"x".repeat(1000));
        let screen = app.render_to_string(40, 24);
        let visible = screen.chars().filter(|c| *c == 'x').count();
        assert!(
            visible > 300,
            "折行后超上限时要滚到光标处（应看到满 9 行）：
{screen}"
        );
    }

    #[test]
    fn bottom_stays_visible_when_lines_wrap() {
        let mut app = test_app();
        // 每条都很长（显示宽度 40 下必然折成多行）：逻辑行数 ≠ 显示行数
        for i in 0..20 {
            app.cells.push(Cell::Notice(format!(
                "第{i}条 {}",
                "很长很长的中文内容".repeat(8)
            )));
        }
        let screen = app.render_to_string(40, 20);
        assert!(
            squash(&screen).contains("第19条"),
            "折行后底部内容必须可见：\n{screen}"
        );
    }

    #[test]
    fn tool_lines_and_notices_are_compact_by_default() {
        let mut app = test_app();
        let long_body: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        app.cells.push(Cell::Tool {
            name: "shell".into(),
            summary: "seq 1 40".into(),
            status: Status::Fail,
            body: Some(long_body),
            manual: false,
        });
        let screen = app.render_to_string(80, 60);
        let flat = squash(&screen);
        assert!(flat.contains("✗shell(seq140)"), "{screen}");
        assert!(flat.contains("line1"), "{screen}");
        assert!(!flat.contains("line40"), "正文截断：{screen}");
        assert!(flat.contains("已省略"), "{screen}");
    }

    #[test]
    fn multiline_notices_break_into_lines() {
        let mut app = test_app();
        app.push_notice("第一行\n第二行");
        let screen = app.render_to_string(40, 10);
        assert!(squash(&screen).contains("·第一行"), "首行带 ·：{screen}");
        let second = screen.lines().find(|l| l.contains('二')).expect("第二行");
        assert!(
            second.starts_with("  ") && !second.trim_start().starts_with('·'),
            "续行缩进对齐且不再带 ·：{screen}"
        );
    }

    #[test]
    fn bang_alone_does_nothing() {
        let mut app = test_app();
        app.input.insert("!");
        app.submit();
        assert!(app.cells.is_empty(), "空命令不产生行");
        assert!(!app.busy);
    }

    /// `!cmd` 是**手动 shell**：直接执行、不进会话上下文，且结果不吃简洁模式。
    #[tokio::test]
    async fn bang_runs_shell_and_stays_out_of_the_context() {
        let (mut app, mut rx) = app_with_rx();
        app.input.insert("!echo hi");
        app.submit();
        // 命令行回显立刻就位（运行中 = `•`），期间算忙（Esc 可中断）
        let flat = squash(&app.render_to_string(80, 24));
        assert!(flat.contains("•shell($echohi)"), "{flat}");
        assert!(app.busy, "跑 shell 期间 busy");

        let event = rx.recv().await.expect("ShellDone 事件");
        app.on_turn_event(event);
        let flat = squash(&app.render_to_string(80, 24));
        assert!(flat.contains("✓shell($echohi)"), "{flat}");
        // 手动 shell 不吃简洁模式（配置默认 lean=true）
        assert!(flat.contains("[exit=0]"), "结果正文：{flat}");
        assert!(flat.contains("hi"), "{flat}");
        assert!(!app.busy);
        let guard = app.session.try_lock().expect("没人握着锁");
        assert!(
            !guard.messages.iter().any(|m| m.role == "user"),
            "!cmd 不进会话上下文"
        );
    }

    /// `!cmd` 跑长命令时 `Esc` / `/stop` 要能中断（杀整个进程组，不是干等）。
    #[tokio::test]
    async fn bang_shell_can_be_stopped() {
        let (mut app, mut rx) = app_with_rx();
        app.input.insert("!sleep 30");
        app.submit();
        app.request_stop();
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("取消要立刻生效（不能等命令跑完）")
            .expect("ShellDone 事件");
        app.on_turn_event(event);
        let flat = squash(&app.render_to_string(80, 24));
        assert!(flat.contains("⏹shell($sleep30)"), "{flat}");
        assert!(flat.contains("[exit=cancelled]"), "{flat}");
        assert!(flat.contains("用户手动终止"), "{flat}");
        assert!(!app.busy);
    }
}
