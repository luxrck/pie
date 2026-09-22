//! 输入框：`ratatui-textarea`（原 `tui-textarea` 的官方接手版）的多行编辑 + 提交 / 换行 / 历史 / 粘贴。
//!
//! 键位（在 `app` 里分发，这里只提供动作）：
//!   - `⏎` 提交；`⇧⏎` / `Ctrl+J` 换行（多数终端把 ⇧⏎ 编成 LF，所以两个都认）
//!   - `↑`/`↓`：输入为空或正在翻历史时，翻输入历史
//!   - `Ctrl+G`：粘贴剪贴板**图片**的路径（纯文本用终端自己的粘贴键，见 `clipboard`）

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders};
use ratatui::Frame;
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

use super::theme::Palette;

const PLACEHOLDER: &str = "问点什么…（/help 看命令）";

/// 输入框高度夹在这个区间（内容多时最多 9 行文本）。
///
/// ⚠ 只画了上边框（`Borders::TOP`）→ 内容区 = 高度 - 1，所以折算只 `+ 1`。
/// `MIN_H = 3` = 1 行边框 + 2 行内容 → **空输入框也占 3 行**，单行文本下方自然留一行
/// 空白、不贴着底栏（用户点名要的默认行高，2026-09-23；此前 MIN_H = 2 会紧贴底栏）。
const MIN_H: u16 = 3;
const MAX_H: u16 = 10;

pub struct Input {
    area: TextArea<'static>,
    history: Vec<String>,
    /// 当前正在翻的历史下标（None = 在编辑新内容）
    hist_idx: Option<usize>,
    /// 历史保存的草稿（翻回来时恢复）
    draft: String,
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl Input {
    /// 建一个配好样式的编辑区（`new` / `reset` / `replace_with` 共用）。
    fn new_area() -> TextArea<'static> {
        let mut area = TextArea::default();
        area.set_placeholder_text(PLACEHOLDER);
        area.set_cursor_line_style(Style::default().add_modifier(Modifier::BOLD));
        // **软换行**：默认的 `WrapMode::None` 是水平滚动（光标越过右边界就整行左移，长行
        // 只看得到尾巴）。`Glyph` = 按字形宽度逐字断（CJK 逐字、英文词也会断），与终端
        // 原生换行一致，也让 `desired_height` 能按宽度精确折算显示行数。
        area.set_wrap_mode(WrapMode::Glyph);
        area
    }

    pub fn new() -> Self {
        Self {
            area: Self::new_area(),
            history: Vec::new(),
            hist_idx: None,
            draft: String::new(),
        }
    }

    pub fn text(&self) -> String {
        self.area.lines().join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.text().trim().is_empty()
    }

    /// 取走当前内容（回车提交）；内容进历史。
    pub fn take(&mut self) -> String {
        let text = self.text();
        if !text.trim().is_empty() {
            self.history.push(text.clone());
        }
        self.reset();
        text
    }

    /// 清空（不记历史）。
    pub fn clear(&mut self) {
        self.reset();
    }

    fn reset(&mut self) {
        // 重建比逐个删更稳（ratatui-textarea 没有 clear()）
        self.area = Self::new_area();
        self.hist_idx = None;
        self.draft.clear();
    }

    /// 全选（`Ctrl+A`：控件默认把它当「跳到行首」，对齐 Textual TextArea 的 `select_all`）。
    pub fn select_all(&mut self) {
        self.area.select_all();
    }

    /// 插入文本（粘贴 / 图片路径）。
    pub fn insert(&mut self, text: &str) {
        self.area.insert_str(text);
    }

    /// 插入一个换行（⇧⏎ / Ctrl+J）。
    pub fn newline(&mut self) {
        self.area.insert_newline();
    }

    /// 用一段文本替换当前内容（接受命令补全候选时用）；光标落在末尾，不记历史。
    pub fn set_text(&mut self, text: &str) {
        self.replace_with(text.to_string());
        self.hist_idx = None;
        self.draft.clear();
    }

    /// 输入框想要的高度（**软换行后**的显示行数 + 上边框）。
    ///
    /// 宽度要由调用方给：`App::render` 先算竖向布局、宽度就在手边，而控件拿到宽度要等到
    /// 真正渲染那一刻（那时高度已经定死了）。`WrapMode::Glyph` 是按显示宽度逐字断行 →
    /// 每个逻辑行的显示行数 = `ceil(显示宽度 / 可用宽度)`（含 tab 的行会略偏，控件自带
    /// 内部滚动兜底）。
    pub fn desired_height(&self, width: u16) -> u16 {
        let width = width.max(1) as usize;
        let rows: usize = self
            .area
            .lines()
            .iter()
            .map(|line| {
                let w = Line::from(line.as_str()).width();
                if w == 0 {
                    1
                } else {
                    w.div_ceil(width)
                }
            })
            .sum();
        (rows as u16 + 1).clamp(MIN_H, MAX_H)
    }

    /// ↑：翻到更早的历史（在翻历史或输入为空时生效）。
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.hist_idx {
            None => {
                self.draft = self.text();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.hist_idx = Some(next);
        self.replace_with(self.history[next].clone());
    }

    /// ↓：翻回更近的历史（翻到底就恢复草稿）。
    pub fn history_next(&mut self) {
        let Some(i) = self.hist_idx else { return };
        if i + 1 < self.history.len() {
            self.hist_idx = Some(i + 1);
            self.replace_with(self.history[i + 1].clone());
        } else {
            self.hist_idx = None;
            let draft = self.draft.clone();
            self.replace_with(draft);
        }
    }

    fn replace_with(&mut self, text: String) {
        let mut area = Self::new_area();
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                area.insert_newline();
            }
            area.insert_str(line);
        }
        area.move_cursor(CursorMove::End);
        self.area = area;
    }

    /// 把按键交给 `ratatui-textarea`（移动/编辑/选择）。
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        self.area.input(key);
    }

    /// 是否正在翻阅输入历史（决定 ↑/↓ 该给历史还是给光标）
    pub fn history_active(&self) -> bool {
        self.hist_idx.is_some()
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, palette: &Palette) {
        // **聚焦高亮**：本 TUI 只有输入框一个可聚焦控件（键盘事件全归它），所以「聚焦」
        // 在视觉上就是常亮的 accent 上边框 —— 对齐 Python 版 `#input:focus`（border: accent）。
        // 例外：输入以 `!` 开头时切工具色（Python `#input.shell-mode`）——提醒这条会直接
        // 当 shell 跑、不进上下文（`App::submit` 认的就是同一个判据）。
        let border = if self.text().trim_start().starts_with('!') {
            palette.style_tool()
        } else {
            palette.style_accent()
        };
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(border);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.area.set_style(palette.style_assistant());
        // 选中高亮：控件默认是 `bg LightBlue`（浅蓝底 + 正文色字，深色终端下糊成一片）→
        // 与 Python `#input .text-area--selection` 一致：accent 底 + accent_text 字。
        // 选中靠 Shift+方向键 / Ctrl+A（鼠标事件被 App 拿去滚历史区了）。
        self.area.set_selection_style(
            Style::default()
                .bg(palette.accent)
                .fg(palette.accent_text),
        );
        frame.render_widget(&self.area, inner);
    }

    /// 光标放回行尾（提交/清空后）。
    pub fn move_cursor_end(&mut self) {
        self.area.move_cursor(CursorMove::End);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_moves_content_to_history() {
        let mut input = Input::new();
        input.insert("你好");
        assert_eq!(input.take(), "你好");
        assert!(input.is_empty());
        // 历史里有了：↑ 能翻回来
        input.history_prev();
        assert_eq!(input.text(), "你好");
        // ↓ 回到（空的）草稿
        input.history_next();
        assert_eq!(input.text(), "");
    }

    #[test]
    fn height_follows_content_within_limits() {
        let mut input = Input::new();
        assert_eq!(input.desired_height(80), MIN_H, "空输入框默认 3 行（边框 + 2 行）");
        input.newline();
        assert_eq!(input.desired_height(80), MIN_H, "两行文本仍装得下，不长个");
        input.newline();
        assert_eq!(input.desired_height(80), MIN_H + 1, "第三行文本才长一格");
        for _ in 0..20 {
            input.newline();
        }
        assert_eq!(input.desired_height(80), MAX_H, "最多 9 行文本");
    }

    #[test]
    fn height_counts_wrapped_rows_too() {
        let mut input = Input::new();
        // 200 个 ASCII 字符，宽 40 → 折成 5 行（+ 上边框 = 6 行高）
        input.insert(&"a".repeat(200));
        assert_eq!(input.desired_height(40), 6);
        // 窗口很宽就不用折（高度停在最小 3 行）
        assert_eq!(input.desired_height(200), MIN_H);
        // 空行也算一行，不然高度会少（宽 100 → 2 行 'a' + 1 行空行 = 4）
        input.insert("\n");
        assert_eq!(input.desired_height(100), MIN_H + 1);
        // 上限仍生效（折行后很容易超）
        assert_eq!(input.desired_height(10), MAX_H);
    }

    #[test]
    fn width_zero_is_treated_as_one() {
        let mut input = Input::new();
        input.insert("abc");
        assert_eq!(input.desired_height(0), 3 + 1, "除数为 0 不能 panic");
    }

    #[test]
    fn newline_keeps_editing_multiline() {
        let mut input = Input::new();
        input.insert("第一行");
        input.newline();
        input.insert("第二行");
        assert_eq!(input.text(), "第一行\n第二行");
        assert_eq!(input.area.lines().len(), 2);
        // 回车只在提交时用：内容原样带走（多行不丢）
        assert_eq!(input.take(), "第一行\n第二行");
        assert!(input.is_empty());
    }
}
