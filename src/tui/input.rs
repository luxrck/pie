//! 输入框：`ratatui-textarea`（原 `tui-textarea` 的官方接手版）的多行编辑 + 提交 / 换行 / 历史 / 粘贴。
//!
//! 键位（在 `app` 里分发，这里只提供动作）：
//!   - `⏎` 提交；`⇧⏎` / `Ctrl+J` 换行（多数终端把 ⇧⏎ 编成 LF，所以两个都认）
//!   - `↑`/`↓`：输入为空或正在翻历史时，翻输入历史
//!   - `Ctrl+G`：粘贴剪贴板**图片**的路径（纯文本用终端自己的粘贴键，见 `clipboard`）
//!
//! **鼠标拖选**：消息流靠鼠标捕获滚轮（终端自己的选择就没了），所以输入框里的选择也得
//! 自己算：控件只提供**字符坐标**（`CursorMove::Jump`），鼠标给的是**屏幕行列**，中间这层
//! 「显示行 → 逻辑行/字符」由 [`Input::hit`] 做（复用与 `desired_height` 同一套折行规则
//! `WrapMode::Glyph`，并复刻控件的视口滚动偏移）。

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Widget};
use ratatui::Frame;
use ratatui_textarea::{CursorMove, DataCursor, TextArea, WrapMode};
use unicode_width::UnicodeWidthChar;

use super::theme::Palette;

/// 输入框为空时显示什么：App 每帧把它改成当前的**键位提示**（`status::hint_text`）——
/// 键位提示不再自己占一行，而是住在这里（省一行给消息流）。
/// 这个常量只当「没人设过」时的默认值（单测直接建 `Input` 时也走它）。
const DEFAULT_PLACEHOLDER: &str = "问点什么…（/help 看命令）";

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
    /// 当前的占位提示（重建控件时要重新贴上，见 [`Input::set_placeholder`]）
    placeholder: String,
    /// 占位提示的样式（键位提示要跟原来的底栏一样淡）
    placeholder_style: Style,
    /// 上一帧控件占的整块区域（鼠标命中测试用；含上边框那一行）
    rect: Rect,
    /// 复刻控件的视口滚动偏移（复刻规则见 `scroll_top_row`）；命中测试要拿它换算内容行
    scroll_top: u16,
    /// 上一帧每个**可见内容行**「写到最右的格数」（含行尾的光标格），[`StaleTail`] 用。
    ///
    /// 行数 / 尺寸对不上时按「整行都算脏」处理（缺省 = 整行宽度），最坏也只是多刷一帧空白。
    last_used: Vec<u16>,
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl Input {
    /// 建一个配好样式的编辑区（`new` / `reset` / `replace_with` 共用）。
    fn new_area(placeholder: &str, style: Style) -> TextArea<'static> {
        let mut area = TextArea::default();
        area.set_placeholder_style(style);
        area.set_placeholder_text(placeholder.to_string());
        area.set_cursor_line_style(Style::default().add_modifier(Modifier::BOLD));
        // **软换行**：默认的 `WrapMode::None` 是水平滚动（光标越过右边界就整行左移，长行
        // 只看得到尾巴）。`Glyph` = 按字形宽度逐字断（CJK 逐字、英文词也会断），与终端
        // 原生换行一致，也让 `desired_height` 能按宽度精确折算显示行数。
        area.set_wrap_mode(WrapMode::Glyph);
        area
    }

    pub fn new() -> Self {
        Self {
            area: Self::new_area(DEFAULT_PLACEHOLDER, Style::default()),
            history: Vec::new(),
            hist_idx: None,
            draft: String::new(),
            placeholder: DEFAULT_PLACEHOLDER.to_string(),
            placeholder_style: Style::default(),
            rect: Rect::default(),
            scroll_top: 0,
            last_used: Vec::new(),
        }
    }

    /// 换掉占位提示（空输入时才看得到）。App 每帧把当前键位提示嗂进来；
    /// 文案没变就什么都不做（免得每帧重建 `Text`）。
    pub fn set_placeholder(&mut self, text: &str, style: Style) {
        if self.placeholder == text && self.placeholder_style == style {
            return;
        }
        self.placeholder = text.to_string();
        self.placeholder_style = style;
        self.area.set_placeholder_style(style);
        self.area.set_placeholder_text(text.to_string());
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
        self.area = Self::new_area(&self.placeholder, self.placeholder_style);
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

    /// 光标位置：`(逻辑行号, 行内字符下标)`（文件补全要知道光标在哪一段后面）。
    pub fn cursor(&self) -> (usize, usize) {
        let cursor = self.area.cursor();
        (cursor.0, cursor.1)
    }

    /// 某一逻辑行的文本（`@` token 解析用）；行号越界给空串（光标总在合法行上，防一手而已）。
    pub fn line(&self, row: usize) -> String {
        self.area.lines().get(row).cloned().unwrap_or_default()
    }

    /// 用 `text` 替换「`from`（行, 列）→ 光标」这一小段，光标落在插入文本末尾（文件补全用）。
    ///
    /// 交给控件的选区来做：`Jump` 到起点 → 开选区 → `Jump` 回原来的光标 → `insert_str`
    /// （`insert_str` 会先删掉选区）。这样行中光标 / 跨行 / 宽字符都不用自己拆字符串。
    pub fn replace_before_cursor(&mut self, from: (usize, usize), text: &str) {
        let end = self.cursor();
        self.area.cancel_selection();
        self.area
            .move_cursor(CursorMove::Jump(from.0 as u16, from.1 as u16));
        self.area.start_selection();
        self.area
            .move_cursor(CursorMove::Jump(end.0 as u16, end.1 as u16));
        self.area.insert_str(text);
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
        let mut area = Self::new_area(&self.placeholder, self.placeholder_style);
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

    // ---------------------------------------------------------------- 鼠标拖选

    /// 上一次渲染时输入框占的整块区域（含上边框）。
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// 鼠标按下：把光标放到点到的位置并**开始选区**（拖动就能选多行）。
    pub fn selection_start(&mut self, column: u16, row: u16) {
        if let Some((line, offset)) = self.hit(column, row) {
            self.area.cancel_selection();
            self.area
                .move_cursor(CursorMove::Jump(line as u16, offset as u16));
            self.area.start_selection();
        }
    }

    /// 鼠标拖动：把选区的另一头拉到该位置。没在选就什么都不做。
    pub fn selection_extend(&mut self, column: u16, row: u16) {
        if !self.area.is_selecting() {
            return;
        }
        if let Some((line, offset)) = self.hit(column, row) {
            self.area
                .move_cursor(CursorMove::Jump(line as u16, offset as u16));
        }
    }

    /// 选区文本（不改动选区）；空选区（点一下没拖）返回 `None`。
    pub fn selection_text(&self) -> Option<String> {
        let ((r1, c1), (r2, c2)) = self.area.selection_range()?;
        if (r1, c1) == (r2, c2) {
            return None;
        }
        let lines = self.area.lines();
        let chars = |r: usize| -> Vec<char> { lines.get(r).map(|l| l.chars().collect()).unwrap_or_default() };
        if r1 == r2 {
            let line = chars(r1);
            let from = c1.min(line.len());
            let to = c2.min(line.len()).max(from);
            return Some(line[from..to].iter().collect());
        }
        let mut out = String::new();
        for r in r1..=r2 {
            let line = chars(r);
            let (from, to) = match r {
                _ if r == r1 => (c1.min(line.len()), line.len()),
                _ if r == r2 => (0, c2.min(line.len())),
                _ => (0, line.len()),
            };
            if r > r1 {
                out.push('\n');
            }
            out.extend(&line[from..to.max(from)]);
        }
        Some(out)
    }

    /// 收掉选区（`Esc` / 点一下没拖 / 重新点一下都靠它）。
    pub fn clear_selection(&mut self) {
        self.area.cancel_selection();
    }

    /// 取走选区文本（松开鼠标时用）；**点一下没拖**就把那个零宽选区收掉、返回 `None`。
    pub fn take_selection(&mut self) -> Option<String> {
        match self.selection_text() {
            Some(text) => Some(text),
            None => {
                self.area.cancel_selection();
                None
            }
        }
    }

    /// 是否有**非空**选区（`Esc` 先收它再管别的）。
    pub fn has_selection(&self) -> bool {
        self.selection_text().is_some()
    }

    /// 屏幕坐标（`column`, `row`）→ 控件的（逻辑行, 字符下标）；不在输入框里就 `None`。
    ///
    /// 鼠标给的是屏幕行列，控件只认字符坐标，中间这层换算就在这里：显示行由
    /// [`Self::screen_rows`] 折行得出（与控件 `WrapMode::Glyph` 同规则），再叠上视口
    /// 滚动偏移 `scroll_top`。点在上边框那一行按「第一行内容」算。
    pub fn hit(&self, column: u16, row: u16) -> Option<(usize, usize)> {
        if self.rect.width == 0 || self.rect.height == 0 {
            return None;
        }
        if row < self.rect.y
            || row >= self.rect.bottom()
            || column < self.rect.x
            || column >= self.rect.right()
        {
            return None;
        }
        let inner = self.inner_rect();
        if inner.width == 0 || inner.height == 0 {
            return None;
        }
        let rows = self.screen_rows(inner.width as usize);
        if rows.is_empty() {
            return Some((0, 0));
        }
        let screen_row = row.saturating_sub(inner.y) as usize + self.scroll_top as usize;
        let screen_col = column.saturating_sub(inner.x) as usize;
        let (line_no, start, len) = rows[screen_row.min(rows.len() - 1)];
        // 显示列 → 行内字符下标：按显示宽度累加（宽字符占两格，点它中间算它的开头）
        let text = self.area.lines().get(line_no).cloned().unwrap_or_default();
        let mut used = 0usize;
        let mut offset = 0usize;
        for c in text.chars().skip(start).take(len) {
            if used + char_width(c) > screen_col {
                break;
            }
            used += char_width(c);
            offset += 1;
        }
        Some((line_no, start + offset))
    }

    /// 插入符落在**屏幕的哪一格**（`(列, 行)`）；区域还没布局（宽 / 高为 0）时 `None`。
    ///
    /// 只在 [`Self::render`] 之后调（那时 `rect` / `scroll_top` 才是这一帧的值）。用途是
    /// **把「文本光标在哪」告诉终端**：TUI 的光标块是自己画的，终端光标一直隐藏着，但输入法
    /// 的候选框不看我们画的那一格——看的是**终端光标单元格**（VS Code 的 xterm 会在光标移动 /
    /// 输入法起手时把隐藏 textarea 摆到那儿，Windows 就把候选框画在那个 textarea 的插入符上）。
    /// 不显式摆位置，终端光标就停在上一帧 diff 最后写入的格子上（点一下消息流都会跑偏）。
    /// 收口在 `App::run`：每帧 `draw` 之后把它交给 `Terminal::set_cursor_position`。
    pub fn caret_position(&self) -> Option<(u16, u16)> {
        let inner = self.inner_rect();
        if inner.width == 0 || inner.height == 0 {
            return None;
        }
        let rows = self.screen_rows(inner.width as usize);
        let screen_row = self.cursor_screen_row(&rows);
        let (line_no, start, _) = *rows.get(screen_row)?;
        let DataCursor(_, col) = self.area.cursor();
        // 行内显示列：从这一显示段的起点数到插入符（宽字符占两格）
        let text = self.area.lines().get(line_no).cloned().unwrap_or_default();
        let width: usize = text
            .chars()
            .skip(start)
            .take(col.saturating_sub(start))
            .map(char_width)
            .sum();
        let row = (screen_row as u16).saturating_sub(self.scroll_top);
        Some((
            (inner.x + width as u16).min(inner.right().saturating_sub(1)),
            (inner.y + row).min(inner.bottom().saturating_sub(1)),
        ))
    }

    /// 内容区（`Borders::TOP` 只吃一行）。
    fn inner_rect(&self) -> Rect {
        Rect {
            x: self.rect.x,
            y: self.rect.y.saturating_add(1),
            width: self.rect.width,
            height: self.rect.height.saturating_sub(1),
        }
    }

    /// 折行后的显示行表：每行给出 `(逻辑行号, 起始字符下标, 字符数)`。
    ///
    /// 与控件 `WrapMode::Glyph` 同规则（按显示宽度逐字断）。含 tab 的行会略偏——控件自己
    /// 有内部滚动兜底，且 `desired_height` 的估算也是这个口径。
    fn screen_rows(&self, width: usize) -> Vec<(usize, usize, usize)> {
        let width = width.max(1);
        let mut rows = Vec::new();
        for (line_no, line) in self.area.lines().iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();
            if chars.is_empty() {
                rows.push((line_no, 0, 0));
                continue;
            }
            let mut start = 0usize;
            let mut used = 0usize;
            for (i, c) in chars.iter().enumerate() {
                let w = char_width(*c);
                if used + w > width && i > start {
                    rows.push((line_no, start, i - start));
                    start = i;
                    used = w;
                } else {
                    used += w;
                }
            }
            rows.push((line_no, start, chars.len() - start));
        }
        rows
    }

    /// 光标所在的**内容行号**（显示行，未减滚动偏移）。
    fn cursor_screen_row(&self, rows: &[(usize, usize, usize)]) -> usize {
        let DataCursor(line, col) = self.area.cursor();
        let same: Vec<usize> = (0..rows.len()).filter(|i| rows[*i].0 == line).collect();
        for (k, &i) in same.iter().enumerate() {
            let (_, start, len) = rows[i];
            // 逻辑行的最后一段，行尾光标也算在里面（对齐控件 `array_to_screen` 的口径）
            let last = k + 1 == same.len();
            let inside = if last {
                start <= col && col <= start + len
            } else {
                start <= col && col < start + len
            };
            if inside {
                return i;
            }
        }
        *same.last().unwrap_or(&0)
    }

    /// 光标格的样式 + 上边框的聚焦色都由 [`Palette`] 决定（`style_caret` / `style_emphasis`）。
    ///
    /// 控件默认给的是裸 `REVERSED`（不带颜色）→ 颜色会跟着**所在行**的样式跑：空输入时那行是
    /// placeholder 的 muted，反色后是一块**暗灰**（看着像没光标）；有文本时才借到正文色变亮。
    /// 所以颜色**不能**交给控件（具体值在 [`Palette`]）：聚焦 = accent 亮块（与选中高亮同款），
    /// 失焦 = muted 暗块——窗口不在前台时一眼能看出来。
    pub fn render(&mut self, frame: &mut Frame, area: Rect, palette: &Palette, focused: bool) {
        self.rect = area;
        // **聚焦高亮**：本 TUI 只有输入框一个可聚焦控件（键盘事件全归它），所以「聚焦」
        // 在视觉上就是常亮的 accent 上边框。
        // 窗口**失焦**时降成 muted（见 [`Palette::style_emphasis`]：全界面同一口径，状态栏也用它）。
        // 例外：输入以 `!` 开头时切工具色——提醒这条会直接
        // 当 shell 跑、不进上下文（`App::submit` 认的就是同一个判据）；失焦时仍按 muted 显示。
        let border = if focused && self.text().trim_start().starts_with('!') {
            palette.style_tool()
        } else {
            palette.style_emphasis(focused)
        };
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(border);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.area.set_style(palette.style_assistant());
        // 选中高亮：控件默认是 `bg LightBlue`（浅蓝底 + 正文色字，深色终端下糊成一片）→
        // 统一到 `Palette::style_selection`。
        // 选区来源：`Shift+方向键` / `Ctrl+A`，或者**鼠标在输入框里拖**（见 `App::on_mouse`
        // 与 [`Input::hit`]——鼠标捕获开着，终端自己的选择用不了，那层屏幕坐标换算自己算）。
        self.area.set_selection_style(palette.style_selection());
        // **光标**：颜色自己给死（`Palette::style_caret`：聚焦 = accent 亮块、失焦 = muted），
        // **不能**用控件默认的裸 `REVERSED`（理由写在那个方法的 doc 里）。
        self.area.set_cursor_style(palette.style_caret(focused));
        // 复刻控件的**视口滚动偏移**（`widget.rs::next_scroll_top` 的同一规则）：命中测试
        // 要知道屏幕上这一行对应内容的第几行。控件每帧渲染时按同样的输入更新自己的视口，
        // 两边从 0 开始、用同一条公式，所以一直同步。
        let rows = self.screen_rows(inner.width as usize);
        if self.area.is_empty() {
            self.scroll_top = 0; // 空输入显示 placeholder 时控件把视口归零
        } else {
            let cursor = self.cursor_screen_row(&rows) as u16;
            let height = inner.height.max(1);
            self.scroll_top = if cursor < self.scroll_top {
                cursor
            } else if self.scroll_top + height <= cursor {
                cursor + 1 - height
            } else {
                self.scroll_top
            };
        }
        frame.render_widget(&self.area, inner);
        // 擦掉上一帧留在「已写区间右边」的残影（宽字符后半格 / placeholder 碎片，见 `StaleTail`）。
        let used = self.used_widths(inner, &rows);
        frame.render_widget(
            StaleTail {
                used: &used,
                prev: &self.last_used,
                width: inner.width,
            },
            inner,
        );
        self.last_used = used;
    }

    /// 这一帧每个**可见内容行**写了多少格：折行后的文本宽度（宽字符算两格）+ 行尾的光标格。
    ///
    /// 空输入时 placeholder 占着第一行，所以那行按整行算——不然它消失（用户开始打字）时，
    /// 提示里宽字符的后半格没人负责擦。
    fn used_widths(&self, inner: Rect, rows: &[(usize, usize, usize)]) -> Vec<u16> {
        let height = inner.height as usize;
        let caret = self.caret_position();
        let mut used = Vec::with_capacity(height);
        for i in 0..height {
            let mut w = if self.area.is_empty() {
                if i == 0 {
                    inner.width
                } else {
                    0
                }
            } else {
                match rows.get(self.scroll_top as usize + i) {
                    Some(&(line_no, start, len)) => self
                        .area
                        .lines()
                        .get(line_no)
                        .map(|line| {
                            line.chars()
                                .skip(start)
                                .take(len)
                                .map(char_width)
                                .sum::<usize>() as u16
                        })
                        .unwrap_or(0),
                    None => 0,
                }
            };
            if let Some((col, row)) = caret {
                if row == inner.y + i as u16 {
                    w = w.max(col.saturating_sub(inner.x) + 1);
                }
            }
            used.push(w.min(inner.width));
        }
        used
    }

    /// 光标放回行尾（提交/清空后）。
    pub fn move_cursor_end(&mut self) {
        self.area.move_cursor(CursorMove::End);
    }
}

/// 把输入框里「已经写过的右边」那一段标成**这一帧必须重画**的小控件（只为擦掉宽字符留下的残影）。
///
/// 背景：宽字符的**后半格**在 ratatui 里就是个普通空格（`Buffer::set_stringn` 写宽字形时会把
/// 后面那格 `reset()`），与真空格在 `Cell::eq`（比 symbol/underline_color/skip/fg/bg/modifier/
/// diff_option）上**完全相等** → 那一格永远进不了 `BufferDiff`。于是只要它以前被写过东西——被删掉的
/// 汉字右半边、placeholder 里 `⏎`/`⇧` 的碎片、光标 / 选区涂过的底色——就会**永久残留**在终端上
/// （`BufferDiff` 自带的强制重发只覆盖「前一格有底色或 REVERSED 之类可见修饰」的情形，普通汉字不满足）。
///
/// `AlwaysUpdate` 会跳过相等判断、强制把这一格发给终端（ratatui 造它就是给「别人可能盖过同一块区域」
/// 用的）；而 `Cell::reset()` 只对当帧有效（back buffer 每帧 `reset()`），所以只在「这一行比上一帧短」
/// 时标一次，其余帧一个字节都不多花。
///
/// ⚠ 只标**已写区间之外**：正文里宽字符的后半格绝不能写——真终端上写它会把汉字擦掉半个。
struct StaleTail<'a> {
    /// 每显示行「写了多少格」（含行尾的光标格）
    used: &'a [u16],
    /// 上一帧同一个位置的值（缺省 / 越界 = 整行都算脏）
    prev: &'a [u16],
    /// 整行宽度（`prev` 缺省用）
    width: u16,
}

impl Widget for StaleTail<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for (i, &used) in self.used.iter().enumerate() {
            let y = area.y + i as u16;
            if y >= area.bottom() {
                break;
            }
            let used = used.min(self.width);
            let prev = self.prev.get(i).copied().unwrap_or(self.width).min(self.width);
            for x in (area.x + used)..(area.x + prev).min(area.right()) {
                buf[(x, y)].set_diff_option(CellDiffOption::AlwaysUpdate);
            }
        }
    }
}

/// 字符占几个单元格（控制符算 0，与控件内部同一把尺子）。
fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
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
    fn caret_position_is_the_screen_cell_of_the_insertion_point() {
        let mut input = Input::new();
        // 输入框整块：x = 3、y = 10、宽 20、高 3 → 内容区从 (3, 11) 起（只吃一行上边框）
        input.rect = Rect {
            x: 3,
            y: 10,
            width: 20,
            height: 3,
        };
        assert_eq!(input.caret_position(), Some((3, 11)), "空输入 = 内容区左上角");
        input.insert("你好，");
        // 「你好，」占 6 个显示列（CJK 各两格）→ 插入符在第 7 格
        assert_eq!(input.caret_position(), Some((9, 11)));
    }

    #[test]
    fn caret_position_counts_wrapped_rows() {
        let mut input = Input::new();
        input.rect = Rect {
            x: 0,
            y: 0,
            width: 3,
            height: 4,
        };
        input.insert("abcde");
        // 宽 3 → 软换成 `abc` / `de`：插入符在第 2 显示行、行内第 3 格（内容区从 y = 1 起）
        assert_eq!(input.caret_position(), Some((2, 2)));
    }

    /// 删掉宽字符时，它后面那格（终端上会留着汉字右半边 / 光标底色残影）必须被**主动**重画。
    ///
    /// 机制：宽字符的后半格在模型里就是个普通空格（`set_stringn` 会 `reset()` 它），与真空格在
    /// `Cell::eq` 上全等 → `BufferDiff` 永远跳过它。所以 `render` 里用 `StaleTail` 在这一行变短时
    /// 把「新行尾 .. 旧行尾」那一段标 `AlwaysUpdate`。这里直接比两帧 buffer 的 diff（终端拿到的
    /// 就是它），顺便钉住「正文里那格不碰」——写它会把汉字擦掉半个。
    #[test]
    fn deleting_a_wide_char_repaints_the_stale_cell_behind_it() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let palette = Palette::mocha();
        let mut input = Input::new();
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).expect("terminal");
        // 画一帧并把这一帧的 buffer 拿回来（终端下一帧要比的就是它）
        let draw = |input: &mut Input, terminal: &mut Terminal<TestBackend>| {
            terminal
                .draw(|frame| input.render(frame, frame.area(), &palette, true))
                .expect("draw")
                .buffer
                .clone()
        };

        // 帧 1：打「你好」（各占两格，插入符在 x=4）
        input.insert("你好");
        let prev = draw(&mut input, &mut terminal);
        assert_eq!(prev[(0, 1)].symbol(), "你");
        assert_eq!(prev[(4, 1)].bg, palette.accent, "插入符在 x=4");

        // 帧 2：退格删掉「好」→ 正文变 `你`（x=0..1），插入符在 x=2
        input.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        let next = draw(&mut input, &mut terminal);
        let emitted: Vec<(u16, u16)> = prev.diff_iter(&next).map(|(x, y, _)| (x, y)).collect();

        assert_eq!(next[(2, 1)].bg, palette.accent, "插入符退到 x=2");
        assert!(
            emitted.contains(&(3, 1)),
            "被删汉字的后半格（x=3）必须重画，否则终端上留着它的右半边：{emitted:?}"
        );
        assert!(emitted.contains(&(4, 1)), "旧插入符号也要擦：{emitted:?}");
        // 正文里宽字符的后半格（x=1 是「你」的后半格）**不能**写：写了真终端会把汉字擦掉半个
        assert!(!emitted.contains(&(1, 1)), "正文里那格不该动：{emitted:?}");

        // 静止的帧一个格子都不该发（否则就是每帧白刷那一段）。
        // 注：擦完那一帧会有一次「回声」——上一帧 buffer 里那两格带着 `AlwaysUpdate` 标记，
        // 而 `Cell::eq` 把 `diff_option` 也比 → 下一帧会再发一次；再往后就安静了。
        let echo = draw(&mut input, &mut terminal);
        let quiet = draw(&mut input, &mut terminal);
        let emitted2: Vec<(u16, u16)> = echo.diff_iter(&quiet).map(|(x, y, _)| (x, y)).collect();
        assert!(emitted2.is_empty(), "静止帧不该再发格子：{emitted2:?}");
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

#[cfg(test)]
mod scratch {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn map(input: &mut Input, width: u16) -> String {
        let palette = Palette::mocha();
        let backend = TestBackend::new(width, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| input.render(frame, frame.area(), &palette, true))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for x in 0..width {
            let cell = &buf[(x, 1)];
            out.push(if cell.bg == palette.accent {
                'A'
            } else if cell.symbol() == " " || cell.symbol().is_empty() {
                '.'
            } else {
                '#'
            });
        }
        out
    }

    #[test]
    fn scratch_selection_over_blanks() {
        let mut input = Input::new();
        input.rect = Rect {
            x: 0,
            y: 0,
            width: 24,
            height: 3,
        };
        // 「你好，明天」+ 一个半角空格 + 4 个全角空格 + 一个半角空格
        input.set_text("你好，明天 \u{3000}\u{3000}\u{3000}\u{3000} ");
        println!("1 未选中                  {}", map(&mut input, 24));
        // 拖选尾部 6 个字符（第 1 内容行 = row 1；列 10 → 22）
        input.selection_start(10, 1);
        input.selection_extend(22, 1);
        println!("2 拖选尾部 6 个字符       {}", map(&mut input, 24));
        // 松手（pie 的 finish_drag：复制完留着选区）
        println!("3 take_selection → {:?}", input.take_selection());
        println!("4 松开后（选区留着）      {}", map(&mut input, 24));
        // 按一次 Backspace
        input.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        println!("5 Backspace 之后          {}", map(&mut input, 24));
        println!("   文本 = {:?}", input.text());
        input.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        println!("6 再按一次 Backspace      {}", map(&mut input, 24));
        println!("   文本 = {:?}", input.text());
    }
}
