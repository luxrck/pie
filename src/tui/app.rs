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

use std::path::PathBuf;
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
use super::files;
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
    /// 后台建好的 `@` 文件补全索引（`None` = 建失败，当作没有）
    FilesIndex(Option<Arc<files::Index>>),
    /// 余额查询结果（启动时 / 每回合结束后 / `/balance`）
    Balance(Result<crate::llm::Balance, LlmError>),
    /// 进程级告警（压缩 / 图片…，见 `crate::log`）：**不能写 stderr**，进消息流
    Notice(String),
    /// 进度（重试次数…）：进消息流，同 `key` 的**就地更新同一个块**（不再一行一条）
    Retry { key: String, text: String },
    /// 这串进度（`key`）结束了：把那个块撤掉
    RetryDone { key: String },
}

/// tick 间隔（约 15fps）：思考计时的秒数要跟着走，但不必更高。
const TICK: Duration = Duration::from_millis(66);
/// 短于这个时长的思考不留痕（免得满屏 `Thought for 0.1s`）。
const THOUGHT_TRACE_MIN: Duration = Duration::from_millis(300);
/// 右下角临时提示活多久（Python 版是 Textual 的 `App.notify` Toast——默认 5s，用户看到的约 3s）。
const TOAST_TTL: Duration = Duration::from_secs(3);
/// `@` 补全的文件索引多久算过期（在这之前沿用，免得反复扫盘）。
///
/// 真正的失效信号是「回合 / `!cmd` 结束」（那时可能刚写过文件），这个是兑底：外部
/// （编辑器、别的进程）改了文件也能自己好。
const FILE_INDEX_TTL: Duration = Duration::from_secs(60);

/// 右下角浮出来的临时提示（对齐 Python 版 `self.app.notify(...)` 的 Textual Toast）：
/// 不占消息流、到点自消，用来报「刚发生了件小事」（复制了几字符…）。
struct Toast {
    text: String,
    /// 失败提示换错误色边框
    ok: bool,
    until: Instant,
}

/// 鼠标能拖选的两个「文本面」。
///
/// 为什么不把它们合成一个「选区模型」：两者的**坐标、渲染、文本提取**本来就是两套——
///   - 消息流：`App` 自己持有 `(绝对显示行, 单元格列)`，因为折行与源文本的对应关系就在
///     `history::Layout` 里（`slice_text` 按源文本切），高亮也是直接改 frame buffer；
///   - 输入框：选区在 `TextArea` 控件内部（**字符坐标**），折行、渲染、以及「输入替换
///     选区」都是控件白送的——搬出来反而要多写一套，还会丢掉那些语义。
/// 所以只在**手势层**统一：按下时定下拖谁、拖动与松开都交给它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    /// 消息流（`App.selection` + `history::Layout`）
    Log,
    /// 输入框（`TextArea` 自己的选区）
    Input,
}

/// 屏幕坐标在不在这个区域里（宽/高为 0 的未布局区域一律不算）。
fn inside(area: Rect, column: u16, row: u16) -> bool {
    area.width > 0
        && area.height > 0
        && column >= area.x
        && column < area.right()
        && row >= area.y
        && row < area.bottom()
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
    /// 左键正在拖哪个「文本面」（按下的那一刻定下，拖动中不变）
    drag: Option<Surface>,
    /// 右下的临时提示（复制完弹一条，`TOAST_TTL` 后自消）
    toast: Option<Toast>,
    /// 系统剪贴板写入句柄（**长活**：见 `clipboard::Copier`——写完就 drop 会把屏幕砸花）
    copier: clipboard::Copier,
    frame: u64,
    busy: bool,
    should_quit: bool,
    /// 简洁模式（`[tui] lean`）：成功的工具结果只留一行，失败/取消才带正文
    lean: bool,
    /// 端点可用模型（`/model ` 的补全候选；启动时后台拉，失败了就空着）
    models: Vec<String>,
    /// `@` 文件补全的索引（cwd 的文件树，尊重 `.gitignore`）；`None` = 还没建好
    file_index: Option<Arc<files::Index>>,
    /// 「目录已补全」留下的锚点：`@` 已经被吃掉了，靠它接着往下钻（见 [`files::PathSession`]）
    path_session: Option<files::PathSession>,
    /// 正在后台建索引（防重复 spawn）
    index_building: bool,
    /// 索引过期（回合 / `!cmd` 结束后置上）→ 下次用到时重建
    index_stale: bool,
    /// 上次建好的时刻（配合 [`FILE_INDEX_TTL`]）
    index_built: Option<Instant>,
    /// 截断提示只提一次（回合结束就会重建，每次重建都提就把消息流刷屏了）
    index_trunc_warned: bool,
    /// 索引以哪个目录为根（= 进程 cwd，与 `read` 解析相对路径的口径一致）
    index_root: PathBuf,
    /// 补全面板的高亮下标
    palette_index: usize,
    /// 面板被 `Esc` 临时收起（输入一变自动恢复）
    palette_hidden: bool,
    /// 上一帧的输入文本（用来发现「输入变了」→ 恢复面板）
    last_input: String,
    /// 余额（状态栏右下角那一小段，`status::balance_text` 的产物）；`None` = 没拿到
    balance: Option<String>,
    /// 余额接口是否可用（查成功过）→ 回合结束后自动再查一次；失败过就不再每回合白试
    balance_supported: bool,
    /// 按次执行旋钮（CLI `--max-steps` / `--no-stream` 传下来，转给每次 `Session::aturn`）
    max_steps: Option<usize>,
    stream: Option<bool>,
}

impl App {
    /// 建 App + 取出事件接收端（`rx` 由主循环持有，避免和 `select!` 里的 `&mut self` 打架）。
    pub fn new(session: Session, max_steps: Option<usize>, stream: Option<bool>) -> (Self, UnboundedReceiver<UiEvent>) {
        let (tx, rx) = unbounded_channel();
        let lean = session.config.tui.lean;
        let snapshot = Snapshot::capture(&session);
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
            drag: None,
            toast: None,
            copier: clipboard::Copier::default(),
            frame: 0,
            busy: false,
            should_quit: false,
            lean,
            models: Vec::new(),
            file_index: None,
            path_session: None,
            index_building: false,
            index_stale: false,
            index_built: None,
            index_trunc_warned: false,
            index_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            palette_index: 0,
            palette_hidden: false,
            last_input: String::new(),
            balance: None,
            balance_supported: false,
            max_steps,
            stream,
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
            crate::log::install(move |notice| {
                let event = match notice.kind {
                    crate::log::Kind::Progress => UiEvent::Retry {
                        key: notice.key,
                        text: notice.text,
                    },
                    crate::log::Kind::ProgressDone => UiEvent::RetryDone { key: notice.key },
                    crate::log::Kind::Warn => UiEvent::Notice(notice.text),
                };
                tx.send(event).is_ok()
            })
        };
        let mut events = EventStream::new();
        let mut ticker = tokio::time::interval(TICK);
        // 启动不再往消息流里推一行键位提示：底栏已经在显示键位、`/help` 有完整命令表，
        // 那一行只会在每次开新会话时把历史区顶掉一行。
        self.spawn_fetch_models();
        self.spawn_fetch_balance(false);
        while !self.should_quit {
            // `@` 补全的索引：需要时才后台建（**不能放 render 里**，见 `ensure_index`）
            self.ensure_index();
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

    /// 鼠标：滚轮滚历史区；**左键拖动框选，松开即复制**（消息流对齐 Python `SelectableRichLog`；
    /// 输入框里则是选输入框里的文本，松开也复制到剪贴板）。
    ///
    /// 两个「文本面」的坐标换算 / 渲染 / 文本提取各是一套（见 [`Surface`]），所以这里只做
    /// **手势分发**：按下定下拖谁（顺便收掉另一个面的选区），拖动、松开都只交给它。
    fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll(-3),
            MouseEventKind::ScrollDown => self.scroll(3),
            // 没按在任何一个面上（底栏 / 空白）就不起选区
            MouseEventKind::Down(MouseButton::Left) => {
                self.drag = None;
                match self.surface_at(mouse.column, mouse.row) {
                    Some(Surface::Input) => {
                        self.selection = None;
                        self.input.selection_start(mouse.column, mouse.row);
                        self.drag = Some(Surface::Input);
                    }
                    Some(Surface::Log) => {
                        self.input.clear_selection();
                        self.selection = self.cell_at(mouse.column, mouse.row).map(|pt| (pt, pt));
                        self.drag = self.selection.map(|_| Surface::Log);
                    }
                    None => self.selection = None,
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => match self.drag {
                Some(Surface::Input) => self.input.selection_extend(mouse.column, mouse.row),
                Some(Surface::Log) => {
                    if let Some((start, _)) = self.selection {
                        if let Some(pt) = self.cell_at(mouse.column, mouse.row) {
                            self.selection = Some((start, pt));
                        }
                    }
                }
                None => {}
            },
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(target) = self.drag.take() {
                    self.finish_drag(target);
                }
            }
            _ => {}
        }
    }

    /// 这个屏幕坐标落在哪个「文本面」上（输入框优先：它压在消息流下面）。
    fn surface_at(&self, column: u16, row: u16) -> Option<Surface> {
        if inside(self.input.rect(), column, row) {
            Some(Surface::Input)
        } else if inside(self.body, column, row) {
            Some(Surface::Log)
        } else {
            None
        }
    }

    /// 松开鼠标：从**正在拖的那个面**取选区文本，写剪贴板 + 右下角弹一条提示。
    ///
    /// 两个面取文本的方式不同（消息流按显示行切源文本、输入框问控件要字符区间），
    /// 但「取到就复制 + Toast」这条收尾只有一份。空选区什么都不做（输入框还顺手收掉
    /// 那个零宽选区）。
    fn finish_drag(&mut self, target: Surface) {
        let text = match target {
            Surface::Log => self.take_selection_text(),
            Surface::Input => self.input.take_selection(),
        };
        let Some(text) = text else {
            return;
        };
        let copied = self.copier.copy(&text);
        let msg = if copied {
            format!("已复制 {} 字符到剪贴板", text.chars().count())
        } else {
            "复制失败：剪贴板不可用".to_string()
        };
        self.toast(msg, copied);
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
            // `@` token 在、索引还没落地（刚敲下 `@` 那一两帧）时，Tab 不能变成一个字面
            // tab 打进输入框——等面板出来再按就是了
            (KeyCode::Tab, ..) if self.file_token().is_some() => {}
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
            UiEvent::FilesIndex(result) => {
                self.index_building = false;
                self.index_stale = false;
                self.index_built = Some(Instant::now());
                match result {
                    Some(index) => {
                        if index.truncated() && !self.index_trunc_warned {
                            self.index_trunc_warned = true;
                            let cap = files::MAX_ENTRIES;
                            self.push_notice(&format!("[文件补全] 目录太大，只索引了前 {cap} 条"));
                        }
                        self.file_index = Some(index);
                    }
                    None => {
                        self.file_index = None;
                        self.push_notice("[文件补全] 目录索引失败（`@` 补全暂时不可用）");
                    }
                }
            }
            UiEvent::Balance(result) => match result {
                Ok(balance) => {
                    self.balance = status::balance_text(&balance);
                    self.balance_supported = true;
                }
                // 拿不到（非 DeepSeek 端点常见）→ À 降级成「不显示」，也不每回合再试
                Err(_) => self.balance_supported = false,
            },
            UiEvent::Notice(text) => self.push_notice(&text),
            UiEvent::Retry { key, text } => self.push_retry(&key, &text),
            UiEvent::RetryDone { key } => self.clear_retry(&key),
            UiEvent::Turn(TurnEvent::Reasoning(_)) => {
                self.settle_retry(); // 模型有响应了 = 重试成功
                let since = *self.thought_started.get_or_insert_with(Instant::now);
                self.activity = Activity::Thinking { since };
            }
            UiEvent::Turn(TurnEvent::AssistantText(delta)) => {
                self.settle_retry(); // 模型开始出字 = 重试成功了
                self.settle_thought();
                // 首个正文增量：开一段「Responding」计时（⚠ 不能每个增量都重设，否则永远是 0.0s）
                if !matches!(self.activity, Activity::Streaming { .. }) {
                    self.activity = Activity::Streaming {
                        since: Instant::now(),
                    };
                }
                Cell::push_assistant_text(&mut self.cells, &delta);
            }
            UiEvent::Turn(TurnEvent::Answer(text)) => {
                self.settle_retry();
                self.settle_thought();
                Cell::push_assistant_text(&mut self.cells, &text);
            }
            UiEvent::Turn(TurnEvent::ToolCall {
                name, arguments, ..
            }) => {
                self.settle_retry();
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
                // 手打的命令可能刚建/删了文件
                self.index_stale = true;
                Cell::finish_tool(&mut self.cells, "bash", status, &body);
            }
            UiEvent::TurnDone(result, snapshot) => {
                self.settle_retry();
                self.settle_thought();
                self.busy = false;
                self.activity = Activity::Idle;
                // 回合里可能刚写过 / 删过文件（`writ` / `edit` / `bash`）→ 下次用到 `@` 时重建
                self.index_stale = true;
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
                // 刚花掉一点 token，顺手把余额刷新一下（只在接口确实可用时）
                if self.balance_supported {
                    self.spawn_fetch_balance(false);
                }
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
        // 面板开着时，回车先把候选落进输入框（规则见 `enter_accepts_candidate`）
        if self.enter_accepts_candidate() {
            self.palette_accept();
        }
        let text = self.input.take();
        // 发出去了就不该再接着补全（输入框已清空，留着锚点只会在下一句话上误触发）
        self.path_session = None;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.busy {
            // 回合进行中先把输入挡回去（唯一的停止入口是 `Esc`，2026-09-23 移除了 `/stop`）
            self.push_notice("回合进行中（按 Esc 停止）");
            return;
        }
        if let Some(cmd) = trimmed.strip_prefix('/') {
            // `/` 开头但不是已知命令（粘进来的绝对路径最常见）→ 当**普通消息**发出去
            if palette::is_known_command(trimmed) {
                self.command(cmd);
                return;
            }
            // 已移除的旧命令（`/stop` `/paste` `/quit`）别当消息发给模型，只提一句改用什么
            if let Some(hint) = palette::removed_hint(trimmed) {
                self.push_notice(hint);
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
            "exit" => self.should_quit = true,
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
            "clear" => self.with_session_mut("切换窗口", |s| match s.clear_window() {
                Ok(n) => {
                    let _ = s.save();
                    // 消息流**不动**（刚归档的东西还能往上翻着看；Python 同款）
                    format!("已切换新窗口（现有 {n} 个历史窗口块，文件在 ~/.pie/windows/，可经指针回查）")
                }
                Err(e) => format!("切换窗口失败：{e}"),
            }),
            "reset" => {
                self.cells.clear();
                self.scroll_from_bottom = 0;
                self.with_session_mut("清空历史", |s| {
                    s.reset();
                    let _ = s.save();
                    "已清空对话历史（保留 system prompt 与记忆）".to_string()
                });
            }
            "save" => self.with_session_mut("保存", |s| match s.save() {
                Ok(()) => format!("已保存 {}", s.path.display()),
                Err(e) => format!("保存失败: {e}"),
            }),
            "balance" => self.spawn_fetch_balance(true),
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
                // 改完配置顺手把状态栏那份快照重生一遍（模型 / 思考深度 / 用量）
                Ok((text, Snapshot::capture(&guard)))
            }
            Err(_) => Err(()),
        };
        match outcome {
            Ok((text, snapshot)) => {
                self.snapshot = snapshot;
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
        let (max_steps, stream) = (self.max_steps, self.stream);
        tokio::spawn(async move {
            let mut guard = session.lock().await;
            let event_tx = tx.clone();
            let mut on_event = move |event: TurnEvent| {
                let _ = event_tx.send(UiEvent::Turn(event));
            };
            let result = guard
                // `parallel_tools: None` = 跟随 `config.parallel_tools`（TUI 没有覆盖它的入口）
                .aturn(&input, &mut on_event, &cancel, max_steps, stream, None)
                .await;
                let snapshot = Snapshot::capture(&guard);
            let _ = tx.send(UiEvent::TurnDone(result, snapshot));
        });
    }

    // ---------------------------------------------------------------- 手动 shell（`!cmd`）

    /// `!cmd`：直接执行 shell —— **不经过 LLM、不进会话上下文**（对齐 Python `_run_shell`）。
    ///
    /// 命令行回显借工具行的形状（摘要位置放 `$ cmd`），结算时强制展开正文（`manual`，
    /// 简洁模式也不例外）。跟回合一样占 `busy`：`Esc` 能中断（杀整个进程组）。
    fn run_shell(&mut self, cmd: &str) {
        if cmd.is_empty() {
            return;
        }
        self.cells.push(Cell::Tool {
            name: "bash".into(),
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
        } else if self.input.has_selection() {
            // 输入框里的选区同理（先收选区，再轮到面板 / 停回合）
            self.input.clear_selection();
        } else if self.palette_visible() {
            // 面板开着就先收面板（Esc 第一下只收面板，不当停止键）
            self.palette_hidden = true;
        } else if self.busy {
            self.request_stop();
        } else if !self.input.is_empty() {
            self.input.clear();
        }
    }

    /// 请求停止本回合（`Esc` 触发——这是唯一的停止入口）。
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

    // ---------------------------------------------------------------- 补全

    /// 当前输入的候选 + **接受时怎么落到输入框**：
    ///
    ///   - `replace_from = Some((行, 列))`：只替换「这一列 → 光标」这一小段（光标处的 `@`
    ///     文件补全；起点是 `@` 自己那一列—— 接受候选时连 `@` 一起吃掉，插进去的是干净路径）；
    ///   - `None`：整条输入换成候选（`/` 命令，含 `/model `/`/thinking `）。
    ///
    /// 光标处的 `@` 优先：它就住在光标旁，而 `/` 命令是整行的事。
    fn completions(&self) -> (Vec<(String, String)>, Option<(usize, usize)>) {
        if let Some((from, fragment)) = self.file_token() {
            if let Some(index) = &self.file_index {
                let items = index.matches(&fragment, files::MATCH_LIMIT);
                if !items.is_empty() {
                    return (items, Some(from));
                }
            }
        }
        (
            palette::matches(
                &self.input.text(),
                &self.models,
                &self.snapshot.model,
                &self.snapshot.reasoning_effort,
            ),
            None,
        )
    }

    /// 光标处的路径 token：`(替换起点, 片段)`；没有就 `None`。
    ///
    /// 两个来源（顺序不能换）：
    ///   1. `@` 打头——用户显式开局（认读规则见 [`files::token`]）；
    ///   2. 目录候选接受后留下的锚点——`@` 已经吃掉了，但这一段路径还在补全中
    ///      （看 [`files::PathSession`]），所以能接着列下一层。
    fn file_token(&self) -> Option<((usize, usize), String)> {
        let (row, col) = self.input.cursor();
        let line = self.input.line(row);
        if let Some((at, fragment)) = files::token(&line, col) {
            return Some(((row, at), fragment));
        }
        let session = self.path_session.as_ref()?;
        let fragment = session.fragment(&line, (row, col))?;
        Some((session.anchor(), fragment))
    }

    /// 给 `@` 补全用的文件索引：需要时（有 token 且没建好 / 过期）在**后台**建一次。
    ///
    /// ⚠ 只能在事件循环里调（`App::run`）：`tokio::spawn` 要有 runtime，而单测直接
    /// `render_to_string` / `submit` 时没有 runtime（会 panic）——所以**不能**放 `render` 里。
    /// 没人敲 `@` 就不扫盘（拿 `file_token` 当开关）。
    fn ensure_index(&mut self) {
        if self.index_building || self.file_token().is_none() {
            return;
        }
        let fresh = self.file_index.is_some()
            && !self.index_stale
            && self.index_built.is_some_and(|at| at.elapsed() < FILE_INDEX_TTL);
        if fresh {
            return;
        }
        self.index_building = true;
        let root = self.index_root.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            // 扫盘是同步的（9p 上本仓 ~40ms，原生盘 18k 文件 ~55ms）：丢给 blocking 池，
            // 别把 UI 的事件循环占住
            let built = tokio::task::spawn_blocking(move || files::Index::build(&root)).await;
            let _ = tx.send(UiEvent::FilesIndex(built.ok().map(Arc::new)));
        });
    }

    /// 面板此刻是否可见（有候选，且没被 `Esc` 收起）。
    fn palette_visible(&self) -> bool {
        !self.palette_hidden && !self.completions().0.is_empty()
    }

    /// `Tab`：接受高亮候选。
    ///
    /// 文件路径只替掉「起点 → 光标」那一小段（光标留在插入文本末尾，接着打下一段路径就行），
    /// `/` 命令则是整条输入换掉。**目录**候选接受后还留一个「路径补全会话」的锚点：
    /// 输入框里已经没 `@` 了，但面板接着列下一层（用户要的逐层下钻）。
    fn palette_accept(&mut self) {
        let (items, replace_from) = self.completions();
        if items.is_empty() {
            return;
        }
        let index = self.palette_index.min(items.len() - 1);
        let text = items[index].0.clone();
        match replace_from {
            Some(from) => {
                self.input.replace_before_cursor(from, &text);
                // 目录 → 续上会话（接着钻）；文件 → 结束（面板收起，不需要再列什么）
                self.path_session = text
                    .ends_with('/')
                    .then(|| files::PathSession::new(from, &text));
            }
            None => self.input.set_text(&text),
        }
    }

    /// 回车前要不要先把面板里高亮的候选落进输入框（`submit` 的开场白）：
    ///
    ///   - `/` 命令：半截命令先补全再发（对齐 Python）；
    ///   - `@` 文件：**只接受文件候选**（顺手吃掉 `@`）——目录候选不动（那是 Tab 的活），
    ///     免得正文里随手打的 `@词` 被改成 `词/`。
    fn enter_accepts_candidate(&self) -> bool {
        if !self.palette_visible() {
            return false;
        }
        let (items, replace_from) = self.completions();
        let index = self.palette_index.min(items.len().saturating_sub(1));
        match replace_from {
            Some(_) => !items[index].0.ends_with('/'),
            None => !palette::is_complete_command(&self.input.text()),
        }
    }

    /// `↑`/`↓`：在候选里移动高亮。
    fn palette_move(&mut self, delta: i32) {
        let len = self.completions().0.len();
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

    /// 后台查一次余额（启动时、每回合结束后、`/balance` 共用）。
    ///
    /// `report` = 用户主动问的（`/balance`）：成败都把话说到（进消息流的 Notice）。
    /// 失败不当错误抛：非 OpenAI 兼容端点大多没这个接口（一般回 404）。
    ///
    /// ⚠ 只借一下会话锁把客户端**克隆**出来：HTTP 请求不占会话锁（否则回合进行中
    /// 查余额会被一个长回合堵住）。
    fn spawn_fetch_balance(&self, report: bool) {
        let session = self.session.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let llm = session.lock().await.llm.clone();
            let result = llm.fetch_balance().await;
            if report {
                let text = match &result {
                    Ok(balance) => format!("余额：\n{}", status::balance_detail(balance)),
                    Err(e) => format!("余额查询失败：{e}"),
                };
                let _ = tx.send(UiEvent::Notice(text));
            }
            // 余额本身交给状态栏（右下角）
            let _ = tx.send(UiEvent::Balance(result));
        });
    }

    /// `Ctrl+G`：**只**把剪贴板里的图片变成路径（对齐 Python `_paste_image`）。
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

    /// 重试进度：**就地更新**同一 `key` 的那个块（没有就新建）——「只显示 1 个块」。
    ///
    /// 按 `key`（哪个请求）分组：并行的两条流程（回合请求 / 启动时拉模型列表）各占一块，
    /// 不会互相把内容改掉。
    fn push_retry(&mut self, key: &str, text: &str) {
        for cell in self.cells.iter_mut().rev() {
            if let Cell::Retry { key: k, text: t } = cell {
                if k == key {
                    *t = text.to_string();
                    return;
                }
            }
        }
        self.cells.push(Cell::Retry {
            key: key.to_string(),
            text: text.to_string(),
        });
    }

    /// 这串进度结束了（`with_retry` 出口）：撤掉那个块。
    fn clear_retry(&mut self, key: &str) {
        self.cells.retain(|cell| match cell {
            Cell::Retry { key: k, .. } => k != key,
            _ => true,
        });
    }

    /// 模型恢复响应了（或回合收尾）：重试块是**临时进度**，一并撤掉（含启动时拉模型列表
    /// 留下的那块——它已经过时了）。
    fn settle_retry(&mut self) {
        self.cells.retain(|cell| !matches!(cell, Cell::Retry { .. }));
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
        let (matches, _) = self.completions();
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
        // 状态行（模型 / 工作目录 / 上下文用量 / 活动指示）在最**下**方——像 codex / claude code
        // 那样当状态栏用：消息流从屏幕第一行开始，底部一整块 chrome（面板 + 输入框 + 状态）。
        let [body, palette_area, input, status_row] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(panel.len() as u16),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .areas(area);

        let now = Instant::now();

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
        self.input
            .set_placeholder(status::hint_text(self.busy), self.palette.style_faint());
        self.input.render(frame, input, &self.palette);
        // 状态栏：整个界面唯一一处常驻 chrome，放最下方（避开消息流的第一眼位置）
        frame.render_widget(
            Paragraph::new(status::status_line(
                &self.palette,
                &self.snapshot,
                self.activity,
                self.frame,
                now,
                area.width,
                self.balance.as_deref(),
            )),
            status_row,
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
    (mark, format!("{}", out.trim_end_matches('\n')))
}

fn cwd_text() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// 从会话当前状态抓一份状态栏快照（`App::new` / 回合收尾 / 命令改配置后共用）。
///
/// ⚠ 别就地手写 `Snapshot { … }` 字面量：给结构体加字段时漏掉某一处就会**编不过**
/// （2026-09-23 加 `reasoning_effort` 时正好踩到）。字段列表只住在这里。
impl Snapshot {
    fn capture(session: &Session) -> Self {
        Self {
            model: session.config.model.clone(),
            reasoning_effort: session.config.reasoning_effort.clone(),
            cwd: cwd_text(),
            prompt_tokens: session.usage.prompt_tokens,
            budget: session.config.context_budget(),
            calls: session.usage.calls,
            busy: false,
        }
    }
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
    ///
    /// ⚠ 会**写配置**的命令（`/model` / `/thinking`）会把配置文件写回：`Config::default()` 的
    /// `config_file` 是 `None` → 落到 `~/.pie/config.toml`（**用户真实的配置**）。
    /// 用到这类命令的用例请自己建 `Config { config_file: Some(临时路径), .. }`（别改 `PIE_DIR`
    /// 环境变量：那是进程级的，会跟并行跑的用例抢），见 `thinking_change_shows_up_in_the_status_bar`。
    fn app_with_rx() -> (App, UnboundedReceiver<UiEvent>) {
        let config = Config {
            model: "deepseek-flash".into(),
            ..Default::default()
        };
        let llm = crate::llm::LlmClient::new(&config).expect("client");
        let tools = crate::tools::ToolRegistry::new(Default::default());
        App::new(Session::ephemeral(&config, llm, tools), None, None)
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

        // Esc 先收面板（不当停止键），输入一变又回来
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

    // ---------------------------------------------------------- `@` 文件路径补全

    /// `@` 补全用例的 cwd：一个带 `src/tui/app.rs` / `src/session.rs` 的小目录。
    fn files_tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pie-tui-files-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/tui")).expect("建临时目录");
        std::fs::write(dir.join("src/tui/app.rs"), "").expect("写文件");
        std::fs::write(dir.join("src/session.rs"), "").expect("写文件");
        std::fs::write(dir.join("README.md"), "").expect("写文件");
        dir
    }

    /// 往左挪光标 n 次（走真实按键路径，用来把光标停在 `@` token 中间）。
    fn move_left(app: &mut App, n: usize) {
        for _ in 0..n {
            app.input
                .handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        }
    }

    /// `@` 补全：候选来自（注入的）索引；接受时**连 `@` 一起**换成干净路径、面板收起。
    #[test]
    fn at_completion_replaces_the_token_and_drops_the_at() {
        let dir = files_tmp("accept");
        let (mut app, _rx) = app_with_rx();
        app.file_index = Some(Arc::new(files::Index::build(&dir)));
        // 行中的 `@` 也算：光标停在 token 中间
        app.input.insert("看看 @app 的实现");
        move_left(&mut app, 4); // " 的实现" ← 挪回 `app` 后面
        let (items, from) = app.completions();
        assert_eq!(items.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>(), ["src/tui/app.rs"]);
        assert_eq!(from, Some((0, 3)), "替换起点是 `@` 那一列（连它一起替换）");
        assert!(app.palette_visible());
        let screen = app.render_to_string(80, 24);
        assert!(squash(&screen).contains("▸src/tui/app.rs"), "{screen}");

        // Tab 接受：`@` 被吃掉，尾巴保留，光标落在插入文本末尾
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input.text(), "看看 src/tui/app.rs 的实现");
        assert_eq!(app.input.cursor(), (0, 17), "光标落在插入路径末尾");
        assert!(!app.palette_visible(), "`@` 没了 → 面板自然收起");
    }

    /// 索引没建好就不弹面板（首次 `@` 到索引回来之前那一下）；邮箱/裸词不误伤。
    #[test]
    fn at_completion_needs_an_index_and_ignores_emails() {
        let dir = files_tmp("noindex");
        let (mut app, _rx) = app_with_rx();
        app.input.set_text("@app");
        assert!(!app.palette_visible(), "没索引就不弹：{:?}", app.completions().0);

        app.file_index = Some(Arc::new(files::Index::build(&dir)));
        assert!(app.palette_visible());
        // 邮箱不弹
        app.input.set_text("mail a@b.com");
        assert_eq!(app.file_token(), None, "邮箱不该被当成 `@` token");
        assert!(!app.palette_visible());
        // 光标在 `@` 左边也不弹
        app.input.set_text("@app");
        app.input.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert!(!app.palette_visible());
    }

    /// 回车：**文件**候选先补全（顺手吃掉 `@`）再发；目录候选不动——那是 Tab 的活，
    /// 不然正文里随手打的 `@词` 会被改成 `词/`。
    #[test]
    fn enter_accepts_a_file_candidate_but_not_a_directory_one() {
        let dir = files_tmp("enter");
        let (mut app, _rx) = app_with_rx();
        app.file_index = Some(Arc::new(files::Index::build(&dir)));

        app.input.set_text("@app");
        assert!(app.enter_accepts_candidate());
        app.palette_accept();
        assert_eq!(app.input.text(), "src/tui/app.rs");

        app.input.set_text("@src");
        assert_eq!(app.completions().0[0].0, "src/", "首个候选是目录自己");
        assert!(!app.enter_accepts_candidate(), "目录候选回车不动它（那是 Tab 的活）");
        // 已定下来的 `/` 命令不改，半截命令仍要先补全（原有行为）
        app.input.set_text("/status");
        assert!(!app.enter_accepts_candidate());
        app.input.set_text("/statu");
        assert!(app.enter_accepts_candidate());
    }

    /// 目录候选 Tab 之后**接着列下一层**：`@` 已被吃掉，靠“路径补全会话”的锚点继续。
    #[test]
    fn tab_on_a_directory_keeps_listing_the_next_level() {
        let dir = files_tmp("drill");
        let (mut app, _rx) = app_with_rx();
        app.file_index = Some(Arc::new(files::Index::build(&dir)));

        app.input.set_text("@src");
        assert_eq!(app.completions().0[0].0, "src/", "首个候选是目录自己");
        app.palette_accept();
        assert_eq!(app.input.text(), "src/");
        // 目录已接受：面板不倒，直接列这一层的子项（都是 `dir/xxx` 的完整相对路径）
        let items: Vec<String> = app.completions().0.into_iter().map(|(i, _)| i).collect();
        assert_eq!(items, ["src/session.rs", "src/tui/"], "接着列 `src/` 的下一层");
        assert!(app.palette_visible());
        let screen = app.render_to_string(80, 24);
        assert!(squash(&screen).contains("▸src/session.rs"), "{screen}");

        // 接着往下打 / 再 Tab：逐层钻进去
        app.input.insert("tu");
        assert_eq!(app.input.text(), "src/tu");
        assert_eq!(app.completions().0[0].0, "src/tui/");
        app.palette_accept();
        assert_eq!(app.input.text(), "src/tui/");
        assert_eq!(app.completions().0[0].0, "src/tui/app.rs");
        // 接受**文件** = 补全结束（面板收起、锚点扔掉）
        app.palette_accept();
        assert_eq!(app.input.text(), "src/tui/app.rs");
        assert!(app.path_session.is_none());
        assert!(!app.palette_visible());
    }

    /// 补全会话只活在「那条路径上」：打了空白 / 全选重打就结束，不会在别的文本上乱弹。
    #[test]
    fn path_session_ends_when_the_text_leaves_it() {
        let dir = files_tmp("session-end");
        let (mut app, _rx) = app_with_rx();
        app.file_index = Some(Arc::new(files::Index::build(&dir)));
        app.input.set_text("@src");
        app.palette_accept();
        assert!(app.palette_visible(), "接受目录后面板不倒");

        // 打了空白 = 去写别的了
        app.input.insert(" 看看");
        assert_eq!(app.file_token(), None);
        assert!(!app.palette_visible());

        // 全选重打（跟那条路径没关系了）也一样
        app.input.select_all();
        app.input.insert("hello");
        assert_eq!(app.file_token(), None);
        assert!(!app.palette_visible());
    }

    /// 后台索引回来：装上、清 `building`；建失败就静默降级（当作没索引）+ 一句提示。
    #[test]
    fn files_index_event_installs_the_index() {
        let dir = files_tmp("install");
        let (mut app, _rx) = app_with_rx();
        app.input.set_text("@app");
        app.index_building = true;
        app.on_turn_event(UiEvent::FilesIndex(Some(Arc::new(files::Index::build(&dir)))));
        assert!(!app.index_building, "到了就不再算「在建」");
        assert!(!app.index_stale);
        assert!(app.palette_visible(), "索引到了就能弹面板");

        app.on_turn_event(UiEvent::FilesIndex(None));
        assert!(app.file_index.is_none());
        assert!(!app.palette_visible());
        assert!(last_notice(&app).contains("索引失败"), "{}", last_notice(&app));
    }

    #[test]
    fn renders_header_history_and_hint() {
        let mut app = test_app();
        app.cells.push(Cell::User("你好".into()));
        Cell::push_assistant_text(&mut app.cells, "**在**的");
        app.cells.push(Cell::Thought(Duration::from_millis(2100)));
        app.cells.push(Cell::Tool {
            name: "bash".into(),
            summary: "pwd".into(),
            status: Status::Ok,
            body: Some("/tmp".into()),
            manual: false,
        });
        app.cells.push(Cell::Notice("粘贴成功".into()));

        let screen = app.render_to_string(80, 24);
        let flat = squash(&screen);
        assert!(flat.contains("deepseek-flash"), "状态栏有模型：{screen}");
        assert!(flat.contains("›你好"), "{screen}");
        assert!(flat.contains("在的"), "助手正文：{screen}");
        assert!(flat.contains("Thoughtfor2.1s"), "思考耗时：{screen}");
        assert!(flat.contains("✓bash(pwd)"), "工具行：{screen}");
        assert!(!flat.contains("/tmp"), "默认简洁模式不展成功正文：{screen}");
        assert!(flat.contains("发送"), "底栏提示：{screen}");
        assert!(flat.contains("粘贴成功"), "{screen}");
    }

    /// 空白新会话：状态栏就显示 `0/<预算> (0.0%)`，消息流里也不该有键位提示行（键位现在在输入框 placeholder 里）。
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
        let config = Config {
            model: "deepseek-flash".into(),
            ..Default::default()
        };
        let llm = crate::llm::LlmClient::new(&config).expect("client");
        let tools = crate::tools::ToolRegistry::new(Default::default());
        let mut session = Session::ephemeral(&config, llm, tools);
        session.messages.push(crate::llm::Message::user("上次的问题"));
        session.messages.push(crate::llm::Message {
            role: "assistant".into(),
            content: Some(crate::llm::Content::Text("上次的回答".into())),
            ..Default::default()
        });
        let mut app = App::new(session, None, None).0;
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
        // 右下角：盒子最后一行的右端就是屏幕右端，且**不盖住输入框**
        let lines: Vec<&str> = screen.lines().collect();
        let box_bottom = lines.iter().rposition(|l| l.contains('已')).unwrap();
        assert!(
            box_bottom < app.input.rect().y as usize,
            "提示要浮在输入框上方：\n{screen}"
        );
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
    fn retry_progress_is_one_block_per_flow_and_clears_at_the_end() {
        let mut app = test_app();
        let retry = |key: &str, n: u32| UiEvent::Retry {
            key: key.into(),
            text: format!("[retry] {key}失败（boom），1.0s 后第 {n}/3 次重试"),
        };
        app.on_turn_event(retry("流式请求", 1));
        let screen = app.render_to_string(80, 24);
        assert!(squash(&screen).contains("⟳[retry]流式请求失败"), "{screen}");
        assert!(squash(&screen).contains("第1/3次重试"), "{screen}");
        // 标记是半角字（宽度 2）——不能把行挤歪
        assert_eq!(Line::from("⟳ ").width(), 2);

        // 再重试一次：**还是只有一个块**，内容就地改写
        app.on_turn_event(retry("流式请求", 2));
        let screen = app.render_to_string(80, 24);
        assert_eq!(screen.matches('⟳').count(), 1, "只留一个块：\n{screen}");
        assert!(squash(&screen).contains("第2/3次重试"), "{screen}");
        assert!(!squash(&screen).contains("第1/3次重试"), "旧内容被改写：{screen}");

        // 另一条流程（启动时拉模型列表）**各占一块**，不互相覆盖
        app.on_turn_event(retry("模型列表请求", 1));
        let screen = app.render_to_string(80, 24);
        assert_eq!(screen.matches('⟳').count(), 2, "两条流程各一块：\n{screen}");
        app.on_turn_event(retry("模型列表请求", 2));
        let screen = app.render_to_string(80, 24);
        assert_eq!(screen.matches('⟳').count(), 2, "同 key 刷新不新增：\n{screen}");
        let flat = squash(&screen);
        assert!(flat.contains("流式请求失败") && flat.contains("模型列表请求失败"), "{screen}");

        // 这串进度结束（`with_retry` 出口）→ 只撤自己那块
        app.on_turn_event(UiEvent::RetryDone {
            key: "流式请求".into(),
        });
        let screen = app.render_to_string(80, 24);
        assert_eq!(screen.matches('⟳').count(), 1, "只撤自己那块：\n{screen}");
        let flat = squash(&screen);
        assert!(!flat.contains("流式请求失败"), "结束的那块要撤：{flat}");
        assert!(flat.contains("模型列表请求失败"), "别人的块不动：{flat}");

        // 模型开始出字 = 重试成功 → 所有临时进度块都撤掉
        app.on_turn_event(UiEvent::Turn(TurnEvent::AssistantText("好了".into())));
        let flat = squash(&app.render_to_string(80, 24));
        assert!(!flat.contains('⟳'), "恢复后不留重试块：{flat}");
        assert!(flat.contains("好了"), "正文照常：{flat}");
    }

    #[test]
    fn retry_block_is_replaced_by_the_error_when_the_turn_gives_up() {
        let mut app = test_app();
        app.on_turn_event(UiEvent::Retry {
            key: "流式请求".into(),
            text: "[retry] 请求失败（boom），1.0s 后第 3/3 次重试".into(),
        });
        let snapshot = app.snapshot.clone();
        app.on_turn_event(UiEvent::TurnDone(
            Err(LlmError::Protocol("boom".into())),
            snapshot,
        ));
        let screen = app.render_to_string(80, 24);
        let flat = squash(&screen);
        assert!(flat.contains("boom"), "错误要看得见：{screen}");
        assert!(!flat.contains('⟳'), "回合结束不留临时进度：{screen}");
    }

    #[test]
    fn input_mouse_drag_selects_text_and_copies_on_release() {
        let mut app = test_app();
        app.input.insert("hello world");
        app.render_to_string(80, 12); // 先渲染一帧：控件才会记下区域（命中测试用）
        let rect = app.input.rect();
        assert!(rect.height > 0, "渲染后才有区域");
        let (x, y) = (rect.x, rect.y + 1); // 上边框下面就是第一行内容

        // 输入框里按下 + 拖到第 5 格 → 选中 "hello"
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        app.on_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            x + 5,
            y,
        ));
        assert_eq!(app.input.selection_text().as_deref(), Some("hello"));
        assert!(app.selection.is_none(), "输入框里拖选不该起消息流选区");

        // 选中的单元格在屏幕上高亮成 accent 底（用的是控件自己的选中样式）
        let buf = render_buffer(&mut app, 80, 12);
        assert_eq!(
            buf[(x, y)].style().bg,
            Some(app.palette.accent),
            "输入框里的选中要高亮"
        );

        // 松开：写剪贴板 + 弹提示（无剪贴板时会提示失败，但提示本身要在）
        app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x + 5, y));
        assert!(app.toast.is_some(), "松开鼠标要弹一条提示");
        assert!(app.drag.is_none(), "松手后结束拖选状态");
    }

    #[test]
    fn input_click_moves_cursor_and_escape_clears_selection() {
        let mut app = test_app();
        app.input.insert("第一行\n第二行");
        app.render_to_string(80, 12);
        let rect = app.input.rect();
        let (x, y) = (rect.x, rect.y + 1);
        let second = (rect.x, rect.y + 2);

        // 点第二行第 2 格：光标跟着走（上下行都能命中）
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), second.0 + 2, second.1));
        app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), second.0 + 2, second.1));
        let (line, col) = app.input.hit(second.0 + 2, second.1).expect("命中第二行");
        assert_eq!((line, col), (1, 1), "点在第 2 行第 2 格");
        assert!(app.input.selection_text().is_none(), "点一下不算选区");

        // 拖选后再按 Esc：先收选区（不是清空输入）
        app.input.insert("abcd");
        app.render_to_string(80, 12);
        drag(&mut app, (x, y), (x + 3, y));
        assert!(app.input.has_selection());
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.input.has_selection(), "Esc 收掉输入框选区");
        assert!(!app.input.is_empty(), "不能顺手把输入也清了");
    }

    #[test]
    fn click_outside_the_input_does_not_touch_it() {
        let mut app = test_app();
        app.input.insert("abc");
        app.cells.push(Cell::User("日志内容".into()));
        app.render_to_string(80, 12);
        let body = app.body;
        // 消息流里拖：走的是消息流框选，输入框不受影响
        drag(&mut app, (body.x, body.y), (body.x + 3, body.y));
        assert!(app.selection.is_some());
        assert!(app.drag.is_some(), "拖动中记着拖的是哪个面");
        assert!(!app.input.has_selection());
        // 松开：走消息流那条收尾
        app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), body.x + 3, body.y));
        assert!(app.drag.is_none() && app.selection.is_none(), "消息流选区松开即取走");
    }

    #[test]
    fn drag_target_is_fixed_when_the_mouse_goes_down() {
        let mut app = test_app();
        app.cells.push(Cell::User("日志内容".into()));
        app.input.insert("abc");
        app.render_to_string(80, 12);
        let body = app.body;
        let rect = app.input.rect();
        // 从消息流按下，一路拖到输入框上面：拖的还是消息流（按下那一刻定下的）
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), body.x, body.y));
        app.on_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            rect.x + 2,
            rect.y + 1,
        ));
        assert_eq!(app.drag, Some(Surface::Log));
        assert!(!app.input.has_selection(), "没把输入框卷进来：{rect:?}");
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
        let (w, h) = (40u16, 12u16);
        let accent = app.palette.accent;
        let tool = app.palette.tool;

        let buf = render_buffer(&mut app, w, h);
        let border_y = app.input.rect().y;
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

    /// 键位提示现在住在输入框的 placeholder 里（空输入时才显示），不再单独占一行。
    #[test]
    fn hint_lives_in_the_input_placeholder() {
        let mut app = test_app();
        let (w, h) = (60u16, 24u16);
        let screen = app.render_to_string(w, h);
        let flat = squash(&screen);
        // ⚠ 末行是空的时候 `str::lines()` 不会多吐一条，所以用屏幕高度当基准，别用 lines().len()
        let rows: Vec<&str> = screen.lines().collect();
        let row = |y: usize| rows.get(y).copied().unwrap_or("");
        assert!(flat.contains("⏎发送"), "空输入时显示键位提示：\n{screen}");
        // 提示在输入框内容的第一行；输入框只在下面留一行（状态栏）
        let rect = app.input.rect();
        assert_eq!(rect.bottom(), h - 1, "输入框下方只剩状态栏：{rect:?}");
        assert!(squash(row(rect.y as usize + 1)).contains("⏎发送"), "\n{screen}");
        assert!(squash(row(rect.y as usize + 2)).is_empty(), "最后一行留白：\n{screen}");

        // placeholder 语义：有字就不显示
        app.input.insert("你好");
        let flat = squash(&app.render_to_string(w, h));
        assert!(!flat.contains("⏎发送"), "输入框里有字时不占位：{flat}");

        // 回合进行中换文案（这行字就在光标待的地方，比原来那行更显眼）
        app.busy = true;
        app.input.clear();
        let flat = squash(&app.render_to_string(w, h));
        assert!(flat.contains("Esc停止"), "忙时提示 Esc：{flat}");
    }

    /// 状态栏（模型 / 目录 / 用量 / 活动）在最**下**方，屏幕第一行就是消息流。
    /// 余额事件 → 状态栏**左边**出现 `¥110.00`（跟在用量后面）；活动指示独占**右边**。
    #[test]
    fn balance_lands_in_the_status_bar() {
        let mut app = test_app();
        app.cells.push(Cell::User("你好".into()));
        // 100 列：名字 + 用量 + 余额 + 右侧的活动指示都放得下，名字不会被截
        let (w, h) = (100u16, 16u16);
        let before = app.render_to_string(w, h);
        assert!(!before.contains('¥'), "还没拿到就不显示：{before}");

        app.on_turn_event(UiEvent::Balance(Ok(crate::llm::Balance {
            is_available: true,
            balance_infos: vec![crate::llm::BalanceInfo {
                currency: "CNY".into(),
                total_balance: "110.00".into(),
                granted_balance: "10.00".into(),
                topped_up_balance: "100.00".into(),
            }],
        })));
        let screen = app.render_to_string(w, h);
        let status = screen.lines().last().expect("状态栏在最后一行");
        assert!(status.starts_with(" deepseek-flash high ·"), "{status:?}");
        assert!(
            status.contains("0/920,576 (0.0%)  │ ¥110.00"),
            "余额紧跟在用量后面（都在左边）：{status:?}"
        );
        assert!(app.balance_supported, "查成功过 → 以后每回合刷新");

        // 活动指示转圈/计时贴到右边（不会把用量/余额推来推去）
        app.activity = Activity::Waiting {
            since: Instant::now(),
        };
        let screen = app.render_to_string(w, h);
        let status = screen.lines().last().unwrap();
        assert!(status.starts_with(" deepseek-flash high ·"), "{status:?}");
        assert!(status.ends_with("Waiting… 0.0s"), "{status:?}");
        assert!(status.contains("¥110.00"), "{status:?}");

        // 查询失败（非 DeepSeek 端点常见）→ 不再自动重试；已显示的值先留着
        app.activity = Activity::Idle;
        app.on_turn_event(UiEvent::Balance(Err(LlmError::Protocol("404".into()))));
        assert!(!app.balance_supported);
        assert!(
            app.render_to_string(w, h)
                .lines()
                .last()
                .unwrap()
                .contains("¥110.00"),
            "拿不到新的就先留旧的"
        );
    }

    /// `/thinking` 改了深度 → 状态栏（快照）跟着变；补全面板的「← 当前」也读同一份。
    ///
    /// ⚠ 会话的 `Config` 要显式指定 `config_file`：`/thinking` 会把配置**写回文件**，而
    /// `Config::default()` 的 `config_file` 是 `None` → 会落到**用户真实的** `~/.pie/config.toml`。
    /// （别用环境变量改 `PIE_DIR` 代替：那是进程级的，会跟并行跑的其他用例抢。）
    #[test]
    fn thinking_change_shows_up_in_the_status_bar() {
        let dir = std::env::temp_dir().join(format!("pie-tui-thinking-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = crate::config::Config {
            model: "deepseek-flash".into(),
            config_file: Some(dir.join("config.toml")),
            ..Default::default()
        };
        let llm = crate::llm::LlmClient::new(&config).expect("client");
        let tools = crate::tools::ToolRegistry::new(Default::default());
        let mut app = App::new(Session::ephemeral(&config, llm, tools), None, None).0;

        let before = app.render_to_string(90, 16);
        assert!(
            before.contains("deepseek-flash high ·"),
            "起手的深度来自配置：{before}"
        );

        app.command("thinking low");

        let after = app.render_to_string(90, 16);
        assert!(
            after.contains("deepseek-flash low ·"),
            "换了深度状态栏就要变：{after}"
        );
        assert!(!after.contains(" high "), "旧深度要消失：{after}");
        let status = after.lines().last().expect("状态栏在最后一行");
        assert!(
            status.starts_with(" deepseek-flash"),
            "前缀去掉就是去掉（行首就是模型名）：{status:?}"
        );
        assert!(
            dir.join("config.toml").exists(),
            "配置写回了指定的那个文件"
        );

        // 补全面板的「← 当前」读的是同一份快照
        app.input.set_text("/thinking ");
        let (matches, replace_from) = app.completions();
        assert!(replace_from.is_none(), "`/` 命令是整条替换");
        assert!(
            matches
                .iter()
                .any(|(cmd, desc)| cmd == "/thinking low" && desc.contains("当前")),
            "{matches:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 正文在流式增量的**首个**分片上开表，后面的分片不许重设（否则永远 0.0s）。
    #[test]
    fn streaming_timer_starts_once_and_keeps_ticking() {
        fn since_of(app: &App) -> Option<Instant> {
            match app.activity {
                Activity::Streaming { since } => Some(since),
                _ => None,
            }
        }
        let mut app = test_app();
        app.activity = Activity::Thinking {
            since: Instant::now(),
        };
        app.on_turn_event(UiEvent::Turn(TurnEvent::AssistantText("第一".into())));
        let started = since_of(&app).expect("第一个增量开表");
        std::thread::sleep(std::time::Duration::from_millis(20));
        app.on_turn_event(UiEvent::Turn(TurnEvent::AssistantText("第二".into())));
        assert_eq!(
            since_of(&app),
            Some(started),
            "第二个增量不能把起点往后挪"
        );
    }

    #[test]
    fn status_bar_lives_at_the_bottom() {
        let mut app = test_app();
        app.cells.push(Cell::User("你好".into()));
        let (w, h) = (70u16, 20u16);
        let screen = app.render_to_string(w, h);
        let rows: Vec<&str> = screen.lines().collect();
        let last = squash(rows[h as usize - 1]);
        assert!(last.contains("deepseek-flash"), "状态栏在最下：\n{screen}");
        assert!(last.contains("0/920,576"), "带上下文用量：\n{screen}");
        // 第一行就是消息流内容（没有顶栏了）
        assert!(
            squash(rows[0]).contains("›你好"),
            "消息流从第一行开始：\n{screen}"
        );
        // 输入框在状态栏上方
        assert_eq!(app.input.rect().bottom(), h - 1, "{}", app.input.rect().y);
    }

    #[test]
    fn single_line_input_keeps_one_blank_row_inside_the_box() {
        let mut app = test_app();
        app.input.insert("只有一行");
        let (w, h) = (60u16, 24u16);
        let screen = app.render_to_string(w, h);
        let rows: Vec<&str> = screen.lines().collect();
        let row = |y: usize| rows.get(y).copied().unwrap_or("");
        let rect = app.input.rect();
        assert_eq!(rect.height, 3, "默认 3 行高");
        assert!(
            squash(row(rect.y as usize + 1)).contains("只有一行"),
            "输入文本在内容第一行：\n{screen}"
        );
        assert!(
            squash(row(rect.y as usize + 2)).is_empty(),
            "下方留一行空白（默认 3 行高）：\n{screen}"
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
        // 100 行 ≈ 900 字（**超过 500**）：展示层自己截，不依赖 Session 上游截断
        // （`tool_result` 事件的 text 现在是原样的，见 `Session::tool_call`）
        let long_body: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        assert!(long_body.len() > 500);
        app.cells.push(Cell::Tool {
            name: "bash".into(),
            summary: "seq 1 100".into(),
            status: Status::Fail,
            body: Some(long_body),
            manual: false,
        });
        let screen = app.render_to_string(80, 60);
        let flat = squash(&screen);
        assert!(flat.contains("✗bash(seq1100)"), "{screen}");
        assert!(flat.contains("line1"), "{screen}");
        assert!(!flat.contains("line100"), "正文截断：{screen}");
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

    /// 已移除的 `/stop` `/paste` `/quit`：不当事给模型，只提醒改用什么（别静默变成一条消息）。
    #[test]
    fn removed_commands_are_not_sent_as_messages() {
        let mut app = test_app();
        for (cmd, needle) in [("/stop", "Esc"), ("/paste", "Ctrl+G"), ("/quit", "/exit")] {
            app.input.set_text(cmd);
            app.submit();
            assert!(!app.busy, "{cmd} 不该起回合");
            assert!(
                !app.cells.iter().any(|c| matches!(c, Cell::User(_))),
                "{cmd} 不该进消息流（那是发给模型的）"
            );
            let notice = last_notice(&app);
            assert!(notice.contains(needle), "{cmd} → 提示里该说 {needle}：{notice}");
        }
        assert!(!app.should_quit, "/quit 不再退出（只有 /exit 与 Ctrl+C）");

        // 回合进行中敲 `/stop`：只提醒按 Esc（它不再是一条命令）
        app.busy = true;
        app.input.set_text("/stop");
        app.submit();
        app.busy = false;
        assert!(last_notice(&app).contains("Esc"), "{}", last_notice(&app));
    }

    /// 最后一条 Notice 文本（命令反馈都走它）。
    fn last_notice(app: &App) -> String {
        app.cells
            .iter()
            .rev()
            .find_map(|c| match c {
                Cell::Notice(t) => Some(t.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// `/clear` 归档当前窗口（**消息流不动**，刚归档的东西还能往上翻），
    /// `/reset` 只清历史（消息流也清）。
    ///
    /// ⚠ 走 `PIE_DIR` 临时目录：窗口块落在 `$PIE_DIR/windows/`、会话与配置也写在那下面，
    /// 不重定向就落到用户真实的 `~/.pie/`。
    #[test]
    fn clear_archives_window_and_reset_wipes_history() {
        let _g = crate::config::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pie-tui-clear-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("PIE_DIR", &dir);

        let mut app = test_app();
        app.cells.push(Cell::User("旧消息".into()));
        app.session.try_lock().expect("空闲").push_user("旧问题");

        app.command("clear");
        let notice = last_notice(&app);
        assert!(notice.contains("窗口块"), "{notice}");
        assert!(dir.join("windows").exists(), "窗口块落在 $PIE_DIR/windows/");
        assert!(
            app.cells.iter().any(|c| matches!(c, Cell::User(_))),
            "clear 不该清屏：{}",
            last_notice(&app)
        );
        {
            let guard = app.session.try_lock().expect("空闲");
            assert_eq!(guard.windows.len(), 1, "会话手上多了一个窗口块");
            assert_eq!(guard.messages.len(), 2, "system + 窗口摘要");
        }

        app.command("reset");
        assert!(
            !app.cells.iter().any(|c| matches!(c, Cell::User(_))),
            "reset 清屏（只剩命令反馈那条 Notice）"
        );
        assert!(last_notice(&app).contains("已清空"), "{}", last_notice(&app));
        assert_eq!(app.session.try_lock().expect("空闲").messages.len(), 1);
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
        assert!(flat.contains("•bash($echohi)"), "{flat}");
        assert!(app.busy, "跑 shell 期间 busy");

        let event = rx.recv().await.expect("ShellDone 事件");
        app.on_turn_event(event);
        let flat = squash(&app.render_to_string(80, 24));
        assert!(flat.contains("✓bash($echohi)"), "{flat}");
        // 手动 shell 不吃简洁模式（配置默认 lean=true），且**只给 body、不给头区**
        assert!(flat.contains("hi"), "{flat}");
        assert!(!flat.contains("[exit="), "`!cmd` 结果只有 body：{flat}");
        assert!(!app.busy);
        let guard = app.session.try_lock().expect("没人握着锁");
        assert!(
            !guard.messages.iter().any(|m| m.role == "user"),
            "!cmd 不进会话上下文"
        );
    }

    /// `!cmd` 跑长命令时 `Esc` 要能中断（杀整个进程组，不是干等）。
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
        assert!(flat.contains("⏹bash($sleep30)"), "{flat}");
        assert!(flat.contains("[exit=cancelled]"), "{flat}");
        assert!(flat.contains("用户手动终止"), "{flat}");
        assert!(!app.busy);
    }
}
