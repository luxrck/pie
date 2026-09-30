//! REPL 画布：给 `repl` 工具的输出一块**专属视图**（tab 切过去看），不挤进消息流。
//!
//! 为什么单独一块：`repl` 的 code / output 成对出现、而且通常多行——塞进对话流会把消息冲得
//! 七零八落；更重要的是，右下角那块 matplotlib 图的**固定 Rect** 意味着图像不会随滚动被
//! 反复重传（`ratatui-image` 卡在滚动列表里正是这一点）。
//!
//! 数据来源就是**已有的回合事件**（`TurnEvent::ToolCall` / `ToolResult` 里 `name == "repl"`）：
//! 工具层、session 层一行都不用改，视图只做「记录 + 折行 + 滚动」。
//!
//! 折行复用消息流的 [`pane::wrap_segments`]（同一把尺子、同一份词边界规则）——**不引第二个实现**。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::{Rect, Size};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};

use super::pane::{self, WrapMode};
use super::theme::{Palette, Status};
use crate::llm::Message;

/// code 行前的提示符。宽度**现算**（`❯` 是歧义宽度字符，不能写死 2）。
const PROMPT: &str = "❯ ";
/// 输出行的缩进：与 code 的正文（提示符右边）对齐。
const OUTPUT_INDENT: &str = "  ";
/// 图像浮层最多占几行（右下角给图尽量大的地方）。
const IMAGE_MAX_ROWS: u16 = 60;
/// 图像浮层最少占几行。
const IMAGE_MIN_ROWS: u16 = 4;
/// 画布矮于这个高度就不贴图了（太小没意思）。
const IMAGE_MIN_CANVAS_ROWS: u16 = 8;

/// 顶层视图（tab）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    /// 对话：消息流 + 输入框。
    Chat,
    /// REPL 画布：`repl` 工具的 code / 输出。
    Repl,
}

impl Tab {
    /// 状态栏上显示的名字：**一个字符**（`c` = 对话、`r` = REPL 画布）。
    ///
    /// 两档挨着显示成 `cr`，靠强调色 + 粗体区分当前那一档——一个字符就够认，省出状态栏宽度。
    pub fn label(self) -> &'static str {
        match self {
            Tab::Chat => "c",
            Tab::Repl => "r",
        }
    }
}

/// 一次 `repl` 调用 → 一条记录。
struct Entry {
    code: String,
    /// `None` = 还没收到结果（在跑）
    output: Option<String>,
    status: Status,
    /// 这次调用产出的图（`repl` 里的 matplotlib）。浮在**画布右下角**：显示这条（或上面最近
    /// 那条）的**最后一张**。
    images: Vec<PathBuf>,
}

/// 最近一张图：**编码一次**，之后每帧渲染都很便宜（`Protocol` 就是 ratatui-image 的缓存）。
struct Cached {
    path: PathBuf,
    /// 编码时用的单元格尺寸：区域（窗口大小 / 切分）变了就要重编。
    cells: Size,
    protocol: Protocol,
}

impl Cached {
    /// 解码 + 按区域大小编码（`Fit` 只缩不放）。有点阻塞，所以只在新图 / 区域变化时做。
    fn build(picker: &Picker, path: &Path, cells: Size) -> Result<Self, String> {
        let image = image::ImageReader::open(path)
            .map_err(|e| e.to_string())?
            .decode()
            .map_err(|e| e.to_string())?;
        let protocol = picker
            .new_protocol(image, cells, Resize::Fit(None))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            path: path.to_path_buf(),
            cells,
            protocol,
        })
    }
}

/// REPL 画布：记录 + 滚动偏移 + 最近一张图。
///
/// `scroll_from_bottom` 与消息流同一套语义（0 = 贴底跟随；正 = 往更早处），方向也跟消息流的
/// `App::scroll` 一致，免得两个视图的 PgUp/PgDn 手感相反。
pub struct Repl {
    entries: Vec<Entry>,
    scroll_from_bottom: u16,
    /// 上一帧的排版结果（`render` 里记账）：折行后的**总行数**、当前**顶部行**、**可见高度**。
    /// 前两个给滚动条用（`App::render` 画完画布后再读它们画 thumb），三个一起供点/拖换算偏移。
    total: usize,
    top: usize,
    viewport: u16,
    /// 终端图像能力（`tui::run` 启动时探一次）。探测不到就用 `halfblocks`（字符画，但至少能看）。
    picker: Picker,
    /// 待显示的图（`repl` 里 matplotlib 出的最新一张）。**编码推迟到渲染时**——
    /// 那时才知道画布给它留多大地方。
    image: Option<PathBuf>,
    /// 已编码的图（路径或尺寸变了就重编）。
    cached: Option<Cached>,
}

impl Default for Repl {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            scroll_from_bottom: 0,
            total: 0,
            top: 0,
            viewport: 0,
            picker: Picker::halfblocks(),
            image: None,
            cached: None,
        }
    }
}

impl Repl {
    pub fn new() -> Self {
        Self::default()
    }

    /// 换上真正探测到的终端探测器（`tui::run` 在进 TUI 前探一次，再传进来）。
    pub fn with_picker(mut self, picker: Picker) -> Self {
        self.picker = picker;
        self
    }

    /// 按会话历史重建画布（消息流那边是 `Cell::from_messages`，这是它的 repl 版）。
    ///
    /// ⚠ 图**不会**回来：图像是运行时附件，没进会话文件（临时文件过一个小时也被清了）。
    pub fn from_messages(messages: &[Message]) -> Self {
        let mut view = Self::default();
        // tool_call_id → 画布里的下标（结果消息只带 id，名字/参数在调用那条里）
        let mut pending: HashMap<String, usize> = HashMap::new();
        for m in messages {
            for call in m.tool_calls.iter().flatten() {
                if call.function.name != "repl" {
                    continue;
                }
                pending.insert(call.id.clone(), view.entries.len());
                view.entries.push(Entry {
                    code: code_of(&call.function.arguments),
                    output: None,
                    status: Status::Running,
                    images: Vec::new(), // 图是运行时附件，不进会话文件 → 回放不出图
                });
            }
            if m.role == "tool" {
                let id = m.tool_call_id.as_deref().unwrap_or_default();
                let Some(index) = pending.remove(id) else {
                    continue;
                };
                let content = m.content_text();
                view.entries[index].status = pane::tool_status(&content);
                view.entries[index].output = Some(content);
            }
        }
        view
    }

    /// 记一条工具调用；返回 `true` = 这是 `repl` 的调用（本视图吃下了）。
    ///
    /// 返回 `true` 时调用方可以选择**不**把它塞进消息流（少一行工具活动）。
    pub fn on_tool_call(&mut self, name: &str, arguments: &str) -> bool {
        if name != "repl" {
            return false;
        }
        self.entries.push(Entry {
            code: code_of(arguments),
            output: None,
            status: Status::Running,
            images: Vec::new(),
        });
        true
    }

    /// 收一条工具结果；返回 `true` = 这是 `repl` 的结果。
    ///
    /// 事件里没有 id，但同一个 `Session` 的 `repl` 调用是**串行**的（工具内部用 `Mutex` 排队）
    /// → 填给「最近一条还在跑的」就是对的。`images` 是这条结果附带的图（本地路径）。
    pub fn on_tool_result(
        &mut self,
        name: &str,
        content: &str,
        status: Status,
        images: &[PathBuf],
    ) -> bool {
        if name != "repl" {
            return false;
        }
        if let Some(entry) = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| e.status == Status::Running)
        {
            entry.output = Some(content.to_string());
            entry.status = status;
            // 图跟着**这条**记录走：滚动时按它选（见 `render`）
            entry.images = images.to_vec();
        }
        // 立刻切到最新一张（贴底时就是它；滚在上面时下一帧会按滚动位置重选）。
        if let Some(path) = images.last() {
            if self.image.as_deref() != Some(path.as_path()) {
                self.image = Some(path.clone());
                self.cached = None;
            }
        }
        true
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 清空（`/reset` 清历史时一起清）。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.scroll_from_bottom = 0;
        self.image = None;
        self.cached = None;
    }

    /// 滚：`delta` 加到「距底部行数」上（**正 = 往更早处**，与 `App::scroll` 一致）。
    /// 上限在渲染时按实际行数夹（那时才知道总行数）。
    pub fn scroll(&mut self, delta: i32) {
        self.scroll_from_bottom = (self.scroll_from_bottom as i32 + delta).max(0) as u16;
    }

    /// 上一帧折行后的**总行数**（空画布 = 0）——滚动条要用。
    pub fn total(&self) -> usize {
        self.total
    }

    /// 上一帧的**顶部行**（与 [`Self::total`] 同一趟记账）——滚动条 thumb 的位置。
    pub fn top(&self) -> usize {
        self.top
    }

    /// 点/拖画布右缘的滚动条：把画布内第 `y` 行（0 = 顶端）线性映到偏移（只改 `scroll_from_bottom`）。
    ///
    /// 与消息流的 `App::scrollbar_jump` 同一套算法（不按 thumb 尺寸做精确抓取，拖动时会“跳”到
    /// 线性位置——第一版够用，也不会因为 thumb 长度变化而抖）。
    pub fn jump_to_row(&mut self, y: usize) {
        let height = self.viewport as usize;
        if height == 0 || self.total <= height {
            return; // 没溢出：滚动条根本没画
        }
        let y = y.min(height - 1);
        let max_off = self.total - height;
        let offset = y * max_off / (height - 1).max(1);
        self.scroll_from_bottom = (max_off - offset) as u16;
    }

    /// 渲染到 `area`（调用方传**已扣掉滚动条**的正文区，与消息流同口径）。
    pub fn render(&mut self, frame: &mut Frame, area: Rect, palette: &Palette) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        // 文本**铺满整块画布**；有图时再在**右下角浮**一块**固定** Rect（固定 = 不随滚动重传）。
        let (text_area, image_area) = self.split(area);
        self.viewport = text_area.height;
        if self.entries.is_empty() {
            self.scroll_from_bottom = 0;
            self.image = None;
            self.cached = None;
            self.total = 0;
            self.top = 0;
            frame.render_widget(Paragraph::new(empty_hint(palette)), text_area);
            return;
        }
        let (lines, ranges) = self.lines(text_area.width as usize, palette);
        let total = lines.len();
        let bottom = total.saturating_sub(text_area.height as usize);
        // 内容变短时把偏移夹回来（与消息流同样「不留旧偏移」）
        self.scroll_from_bottom = self
            .scroll_from_bottom
            .min(bottom.min(u16::MAX as usize) as u16);
        let offset = bottom.saturating_sub(self.scroll_from_bottom as usize);
        // 记账给滚动条（`App::render` 在画布渲染完之后读）
        self.total = total;
        self.top = offset;
        // 图跟着**滚动位置**走：滚到哪条记录，就显示那条（或它之前最近那条）的图
        self.pick_image(offset, text_area.height as usize, &ranges);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((offset.min(u16::MAX as usize) as u16, 0)),
            text_area,
        );
        if let Some(image_area) = image_area {
            self.render_image(frame, image_area, palette);
        }
    }

    /// 画布里**有没有**图 —— 决定要不要给它留位置。
    ///
    /// 用「有没有」而不是 `self.image`：后者是「当前选中的」，滚到没图的区域时会变，
    /// 拿它做布局判断会让图区忽现忽没（画面抖）。
    fn any_image(&self) -> bool {
        self.entries.iter().any(|e| !e.images.is_empty())
    }

    /// 按滚动位置选图：取可见范围里**最后一条**记录，从它往前找第一条带图的。
    ///
    /// 「往前找」而不是只看可见范围：滚到一段没图的记录上时，显示它上面最近那张图，
    /// 比闪成空白（或者一直挂着最新那张）都更符合直觉；哪条都没有就沿用当前这张。
    fn pick_image(&mut self, offset: usize, height: usize, ranges: &[(usize, usize)]) {
        let view = offset..offset + height;
        let last_visible = ranges
            .iter()
            .enumerate()
            .filter(|(_, (start, end))| *start < view.end && *end > view.start)
            .map(|(i, _)| i)
            .next_back();
        let Some(last) = last_visible else {
            return;
        };
        let picked = self.entries[..=last]
            .iter()
            .rev()
            .find_map(|e| e.images.last())
            .cloned();
        if let Some(path) = picked {
            if self.image.as_deref() != Some(path.as_path()) {
                self.image = Some(path);
                self.cached = None; // 换图了 → 得重编
            }
        }
    }

    /// 切分画布：文本永远拿到**整块画布**；有图时额外在**右下角浮**一块固定 Rect。
    ///
    /// 图是**浮层**——压在文本上（不是把文本挤上去）。浮层取画布的 4/5 宽 × 4/5 高、右对齐/底对齐，
    /// 好让图尽量大。图比浮层小时（`Fit` 只缩不放）贴到浮层右下角，左上露出底下的文本，像浮在字上。
    fn split(&self, area: Rect) -> (Rect, Option<Rect>) {
        if !self.any_image() || area.height < IMAGE_MIN_CANVAS_ROWS {
            return (area, None);
        }
        let h = (area.height * 4 / 5).clamp(IMAGE_MIN_ROWS, IMAGE_MAX_ROWS);
        let w = (area.width * 4 / 5).max(1).min(area.width);
        let image = Rect {
            x: area.right() - w,
            y: area.bottom() - h,
            width: w,
            height: h,
        };
        (area, Some(image))
    }

    /// 画图：`Protocol` 按区域尺寸编码一次，之后每帧只渲染（便宜）。
    ///
    /// 编码拿**整个 `area`** 当上限（`Fit` 只缩不放 → 编出来的就是「能塞进 area 的最大尺寸」），
    /// 再把它贴到 area 的**右下角**：图比区域小时靠右下沉，不孤零零挂在左上。
    fn render_image(&mut self, frame: &mut Frame, area: Rect, palette: &Palette) {
        let Some(path) = self.image.clone() else {
            return;
        };
        let cells = area.as_size();
        let fresh = matches!(&self.cached, Some(c) if c.path == path && c.cells == cells);
        if !fresh {
            match Cached::build(&self.picker, &path, cells) {
                Ok(cached) => self.cached = Some(cached),
                Err(reason) => {
                    self.cached = None;
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            format!("  （图读不出来：{reason}）"),
                            palette.style_faint(),
                        ))),
                        area,
                    );
                    return;
                }
            }
        }
        if let Some(cached) = &self.cached {
            let size = cached.protocol.size();
            let width = size.width.min(area.width);
            let height = size.height.min(area.height);
            let rect = Rect {
                x: area.right().saturating_sub(width),
                y: area.bottom().saturating_sub(height),
                width,
                height,
            };
            frame.render_widget(Image::new(&cached.protocol), rect);
        }
    }

    /// 全部显示行（已按宽度折好）+ **每条记录占的行区间**（`[start, end)`，给「按滚动位置选图」用）。
    ///
    /// 返回行而不是直接渲染，是为了让测试能看排版结果。
    fn lines(&self, width: usize, palette: &Palette) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
        let prompt_w: usize = PROMPT.chars().map(pane::char_width).sum();
        let code_style = palette.style_accent();
        let out_style = palette.style_assistant();
        let faint = palette.style_faint();
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for (i, entry) in self.entries.iter().enumerate() {
            if i > 0 {
                out.push(Line::default()); // 条目之间空一行
            }
            let start = out.len();
            // —— code：首行带提示符，续行用等宽空白对齐
            let code_w = width.saturating_sub(prompt_w).max(1);
            for (n, text) in wrap_plain(&entry.code, code_w).iter().enumerate() {
                let prefix = if n == 0 {
                    PROMPT.to_string()
                } else {
                    " ".repeat(prompt_w)
                };
                out.push(Line::from(vec![
                    Span::styled(prefix, code_style),
                    Span::styled(text.clone(), code_style),
                ]));
            }
            // —— output
            out.extend(output_lines(entry, width, out_style, faint, palette));
            ranges.push((start, out.len()));
        }
        (out, ranges)
    }
}

/// 一条记录的输出部分：还没结果 / 空输出 / 正文，三种。
fn output_lines(
    entry: &Entry,
    width: usize,
    ok_style: Style,
    faint: Style,
    palette: &Palette,
) -> Vec<Line<'static>> {
    let indent = OUTPUT_INDENT.chars().count();
    let Some(text) = &entry.output else {
        return vec![indent_line(OUTPUT_INDENT, "执行中…", faint)];
    };
    let body = text.trim_end_matches('\n');
    if body.trim().is_empty() {
        return vec![indent_line(OUTPUT_INDENT, "（无输出）", faint)];
    }
    // 正常输出跟随助手正文色；失败/取消换成状态色（`repl` 的异常**不算**工具失败，所以这里
    // 基本只会看到 `Ok`；留这一手是为了取消那条路径也有颜色）
    let style = match entry.status {
        Status::Ok => ok_style,
        _ => Style::default().fg(palette.mark(entry.status).1),
    };
    wrap_plain(body, width.saturating_sub(indent).max(1))
        .into_iter()
        .map(|line| indent_line(OUTPUT_INDENT, &line, style))
        .collect()
}

fn indent_line(indent: &str, text: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled(text.to_string(), style),
    ])
}

/// 空画布的提示（第一次切过来时看到的就是它）。
fn empty_hint(palette: &Palette) -> Vec<Line<'static>> {
    let faint = palette.style_faint();
    vec![
        Line::default(),
        Line::from(Span::styled("  REPL 画布还是空的", faint)),
        Line::default(),
        Line::from(Span::styled(
            "  助手用 repl 工具跑 Python 后，code 与输出会显示在这里。",
            faint,
        )),
        Line::from(Span::styled("  ← / → 切换视图 · Esc 回到对话", faint)),
    ]
}

/// 工具参数 JSON → `code` 字段（解析不出来就原样显示，至少不丢信息）。
fn code_of(arguments: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(str::to_owned))
        .unwrap_or_else(|| arguments.to_owned())
}

/// 纯文本 → 折行后的显示行：先按 `\n` 切逻辑行，再逐行按宽度折（**空行保留**，不合并）。
fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for logical in text.split('\n') {
        if logical.is_empty() {
            out.push(String::new());
            continue;
        }
        let chars: Vec<char> = logical.chars().collect();
        for (start, len) in pane::wrap_segments(logical, width, WrapMode::WordOrGlyph) {
            out.push(chars[start..start + len].iter().collect());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui_image::FontSize;
    use ratatui_image::picker::ProtocolType;

    /// 排版结果的纯文本（显示行按行拼、行内 spans 直接连起来）。
    fn plain(view: &Repl, width: usize) -> String {
        view.lines(width, &Palette::mocha())
            .0
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn only_repl_events_are_kept() {
        let mut view = Repl::new();
        assert!(!view.on_tool_call("bash", r#"{"command":"ls"}"#));
        assert!(!view.on_tool_result("bash", "[exit=1]\n\nboom", Status::Fail, &[]));
        assert!(view.is_empty());
        assert!(view.on_tool_call("repl", r#"{"code":"1+1"}"#));
        assert!(!view.is_empty());
    }

    #[test]
    fn code_is_pulled_out_of_the_arguments_json() {
        let mut view = Repl::new();
        view.on_tool_call(
            "repl",
            r#"{"code":"import math\nprint(math.pi)","timeout":null}"#,
        );
        let text = plain(&view, 40);
        assert!(text.starts_with("❯ import math"), "{text}");
        assert!(text.contains("\n  print(math.pi)"), "{text}");
        // 还在跑 → 占位提示
        assert!(text.contains("执行中…"), "{text}");
    }

    #[test]
    fn result_fills_the_latest_running_entry() {
        let mut view = Repl::new();
        view.on_tool_call("repl", r#"{"code":"1+1"}"#);
        view.on_tool_call("repl", r#"{"code":"2+2"}"#);
        view.on_tool_result("repl", "4", Status::Ok, &[]);
        view.on_tool_result("repl", "2", Status::Ok, &[]);
        let text = plain(&view, 40);
        // 串行填坑：先到的结果填后一条…不，是「最近一条还在跑的」→ 2+2 拿到第一个结果
        assert!(text.contains("❯ 1+1\n  2"), "{text}");
        assert!(text.contains("❯ 2+2\n  4"), "{text}");
    }

    #[test]
    fn empty_output_says_so_and_long_lines_wrap() {
        let mut view = Repl::new();
        view.on_tool_call("repl", r#"{"code":"pass"}"#);
        view.on_tool_result("repl", "", Status::Ok, &[]);
        assert!(plain(&view, 40).contains("（无输出）"));
        assert!(view.on_tool_call("repl", r#"{"code":"print(1)"}"#));
        let long = "x".repeat(100);
        let mut view2 = Repl::new();
        view2.on_tool_call("repl", &format!(r#"{{"code":"{long}"}}"#));
        let wrapped = plain(&view2, 20);
        let rows: Vec<&str> = wrapped.lines().collect();
        assert!(rows.len() > 2, "长行应折成多行：{rows:?}");
        assert!(rows.iter().all(|r| r.chars().count() <= 20), "{rows:?}");
    }

    #[test]
    fn scroll_is_clamped_by_content_and_follows_the_bottom() {
        let mut view = Repl::new();
        for _ in 0..5 {
            view.on_tool_call("repl", r#"{"code":"1"}"#);
            view.on_tool_result("repl", "1", Status::Ok, &[]);
        }
        // 贴底 = 0；往更早处滚是正
        assert_eq!(view.scroll_from_bottom, 0);
        view.scroll(3);
        assert_eq!(view.scroll_from_bottom, 3);
        // 不会低于贴底
        view.scroll(-99);
        assert_eq!(view.scroll_from_bottom, 0);
    }

    /// 滚动条点/拖：屏幕行线性映到偏移，两端夹住（与消息流同一套算法）。
    #[test]
    fn jumping_by_row_maps_the_scrollbar_onto_offsets() {
        let mut view = Repl::new();
        for _ in 0..30 {
            view.on_tool_call("repl", r#"{"code":"print(1)"}"#);
            view.on_tool_result("repl", "1", Status::Ok, &[]);
        }
        const H: u16 = 10;
        render_text(&mut view, 40, H); // 记一帧账（total / top / viewport）
        let total = view.total();
        assert!(total > H as usize, "内容该溢出：total={total}");

        // 点在顶行 → 贴顶（顶部行 = 0）
        view.jump_to_row(0);
        render_text(&mut view, 40, H);
        assert_eq!(view.top(), 0);

        // 点/拖到底行 → 贴底（顶部行 = 总行数 - 可见高）
        view.jump_to_row(H as usize - 1);
        render_text(&mut view, 40, H);
        assert_eq!(view.top(), total - H as usize);

        // 中间位置：线性插值，不落在两端
        view.jump_to_row(1);
        render_text(&mut view, 40, H);
        assert!(
            view.top() > 0 && view.top() < total - H as usize,
            "中间行该落在中间：top={}（total={total}）",
            view.top()
        );
    }

    /// 没溢出时点/拖滚动条什么都不做（那一列压根没画 thumb）。
    #[test]
    fn jumping_does_nothing_without_overflow() {
        let mut view = Repl::new();
        view.on_tool_call("repl", r#"{"code":"1"}"#);
        view.on_tool_result("repl", "1", Status::Ok, &[]);
        render_text(&mut view, 40, 20);
        view.jump_to_row(0);
        assert_eq!(view.scroll_from_bottom, 0);
        render_text(&mut view, 40, 20);
        assert_eq!(view.top(), 0);
    }

    #[test]
    fn reset_clears_everything() {
        let mut view = Repl::new();
        view.on_tool_call("repl", r#"{"code":"1"}"#);
        view.scroll(5);
        view.clear();
        assert!(view.is_empty());
        assert_eq!(view.scroll_from_bottom, 0);
        assert!(view.image.is_none());
    }

    // ---------------------------------------------------------------- 图像

    /// 造一张真 PNG（图像路径确实是文件 → 才会被渲染）。
    fn temp_png(name: &str, w: u32, h: u32) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pie-repl-view-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 200])
        })
        .save(&path)
        .unwrap();
        path
    }

    /// 渲染一帧并把 buffer 拼成文本（图像序列就在 cell 的 symbol 里，能看得到）。
    fn render_text(view: &mut Repl, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| view.render(f, f.area(), &Palette::mocha()))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn halfblocks_draw_the_image_with_block_glyphs() {
        let mut view = Repl::new(); // 默认就是 halfblocks（永远可用）
        let png = temp_png("half.png", 60, 40);
        view.on_tool_call("repl", r#"{"code":"plt.show()"}"#);
        view.on_tool_result("repl", "", Status::Ok, &[png]);
        let screen = render_text(&mut view, 60, 20);
        assert!(
            screen.contains('▀') || screen.contains('▄'),
            "画布下半应出现半块字符：{screen}"
        );
    }

    #[test]
    fn iterm2_writes_the_osc_sequence_into_the_buffer() {
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Iterm2);
        let mut view = Repl::new().with_picker(picker);
        let png = temp_png("term.png", 60, 40);
        view.on_tool_call("repl", r#"{"code":"plt.show()"}"#);
        view.on_tool_result("repl", "", Status::Ok, &[png]);
        let screen = render_text(&mut view, 60, 20);
        // iTerm2 协议：图像数据（OSC 1337 …）塞在图像区左上角那一格的 symbol 里
        assert!(
            screen.contains("\u{1b}]1337;File=inline=1"),
            "应写出 iTerm2 图片序列"
        );
    }

    #[test]
    fn a_missing_image_file_degrades_to_a_note() {
        let mut view = Repl::new();
        view.on_tool_call("repl", r#"{"code":"1"}"#);
        view.on_tool_result(
            "repl",
            "",
            Status::Ok,
            &[PathBuf::from("/nonexistent/pie-nope.png")],
        );
        let screen = render_text(&mut view, 60, 20);
        // 宽字符在 buffer 里占两格 → 拼出来带空格，比较前先去掉
        assert!(screen.replace(' ', "").contains("图读不出来"), "{screen}");
    }

    #[test]
    fn without_an_image_the_whole_canvas_is_text() {
        let mut view = Repl::new();
        view.on_tool_call("repl", r#"{"code":"print(1)"}"#);
        view.on_tool_result("repl", "1", Status::Ok, &[]);
        // 没有图 → 不切图像区（code/output 占满）
        assert!(view.split(Rect::new(0, 0, 40, 20)).1.is_none());
        assert!(plain(&view, 40).contains("❯ print(1)"));
    }

    #[test]
    fn resume_rebuilds_code_and_output_from_history() {
        use crate::llm::{FunctionCall, ToolCall};
        let call = |id: &str, name: &str, args: &str| ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        };
        let assistant = Message {
            role: "assistant".into(),
            tool_calls: Some(vec![
                call("c1", "repl", r#"{"code":"print(1)"}"#),
                call("c2", "bash", r#"{"command":"ls"}"#), // 非 repl 的不该进画布
            ]),
            ..Default::default()
        };
        let messages = vec![
            Message::user("跑一下"),
            assistant,
            Message::tool_result("c1", "repl", "1\n"),
        ];
        let view = Repl::from_messages(&messages);
        let text = plain(&view, 40);
        assert!(text.contains("❯ print(1)"), "{text}");
        assert!(text.contains("  1"), "结果要配对到那条调用上：{text}");
        assert!(!text.contains("ls"), "只有 repl 的工具进画布：{text}");
        // 图是运行时附件（不进会话）→ 回放后没有图
        assert!(view.image.is_none());
    }

    #[test]
    fn the_image_sits_in_the_bottom_right_corner() {
        let mut view = Repl::new();
        let png = temp_png("corner.png", 60, 40);
        view.on_tool_call("repl", r#"{"code":"plot()"}"#);
        view.on_tool_result("repl", "", Status::Ok, &[png]);

        let area = Rect::new(0, 0, 40, 20);
        let (text, image) = view.split(area);
        let image = image.expect("有图就该切出图区");
        assert_eq!(image.right(), area.right(), "图区贴右缘");
        assert_eq!(image.bottom(), area.bottom(), "图区贴下缘");
        assert!(image.width < area.width, "图区不占满整行（右下角）");
        assert!(image.height < area.height, "图区不占满整列（右下角）");
        assert_eq!(text, area, "文本仍占**整块**画布（图是浮层）");
    }

    #[test]
    fn scrolling_picks_the_image_of_the_record_in_view() {
        let mut view = Repl::new();
        let a = temp_png("scroll-a.png", 40, 30);
        let b = temp_png("scroll-b.png", 40, 30);
        for (out, img) in [("A", &a), ("B", &b)] {
            view.on_tool_call("repl", r#"{"code":"plot()"}"#);
            view.on_tool_result("repl", out, Status::Ok, std::slice::from_ref(img));
        }
        let (lines, ranges) = view.lines(40, &Palette::mocha());
        assert!(lines.len() >= ranges[1].1, "行区间要落在排版里");
        assert_eq!(ranges.len(), 2);

        // 只看第一条 → 第一张图
        let (s, e) = ranges[0];
        view.pick_image(s, e - s, &ranges);
        assert_eq!(view.image.as_deref(), Some(a.as_path()));

        // 只看第二条 → 第二张图
        let (s, e) = ranges[1];
        view.pick_image(s, e - s, &ranges);
        assert_eq!(view.image.as_deref(), Some(b.as_path()));
    }

    #[test]
    fn a_record_without_images_falls_back_to_the_one_above() {
        let mut view = Repl::new();
        let a = temp_png("fallback-a.png", 40, 30);
        view.on_tool_call("repl", r#"{"code":"plot()"}"#);
        view.on_tool_result("repl", "A", Status::Ok, std::slice::from_ref(&a));
        view.on_tool_call("repl", r#"{"code":"print(1)"}"#); // 这条没图
        view.on_tool_result("repl", "1", Status::Ok, &[]);

        let (_, ranges) = view.lines(40, &Palette::mocha());
        let (s, e) = ranges[1]; // 只看第二条（没图）
        view.pick_image(s, e - s, &ranges);
        assert_eq!(
            view.image.as_deref(),
            Some(a.as_path()),
            "没图的记录应沿用上面最近那张"
        );
    }

    #[test]
    fn the_split_does_not_jitter_while_scrolling() {
        let mut view = Repl::new();
        let a = temp_png("jitter-a.png", 40, 30);
        view.on_tool_call("repl", r#"{"code":"plot()"}"#);
        view.on_tool_result("repl", "A", Status::Ok, std::slice::from_ref(&a));
        view.on_tool_call("repl", r#"{"code":"print(1)"}"#);
        view.on_tool_result("repl", "1", Status::Ok, &[]);

        // 只要画布里有图，布局就一直给它留位置（不随「当前选中的是哪张」变）
        let area = Rect::new(0, 0, 40, 20);
        assert!(view.split(area).1.is_some());
        view.image = None; // 就算当前没选中任何图
        assert!(
            view.split(area).1.is_some(),
            "图区不能忽现忽没（那会让文本区高度抖）"
        );
    }

    /// iTerm2 协议是按**像素**发图的（`width=Npx`），终端就照这个像素数 1:1 画 —— 所以
    /// `Picker` 的 `FontSize` 必须等于终端真实的字符格像素，否则图会按错误比例出屏。
    ///
    /// 这里拿本机 Kaku 实测的 20×58 当钉子：1210×440 的图 = 61×8 格，编出去必须是
    /// 1220×464 像素（= 格数 × 每格像素，向上取整到整格）；差一点就是半个图。
    #[test]
    fn the_image_is_encoded_in_the_cells_of_the_real_terminal_font() {
        #[allow(deprecated)] // 自定 FontSize 的唯一入口，生产走 `tui::pick_terminal_protocol`
        let mut picker = Picker::from_fontsize(FontSize::new(20, 58));
        picker.set_protocol_type(ProtocolType::Iterm2);
        let mut view = Repl::new().with_picker(picker);
        let png = temp_png("grid.png", 1210, 440);
        view.on_tool_call("repl", r#"{"code":"plt.show()"}"#);
        view.on_tool_result("repl", "", Status::Ok, &[png]);

        let screen = render_text(&mut view, 129, 27);
        let size = view.cached.as_ref().unwrap().protocol.size();
        assert_eq!((size.width, size.height), (61, 8), "1210×440 ÷ 20×58 = 61×8 格");

        let seq = screen.lines().find(|l| l.contains("1337;File")).unwrap();
        let head = &seq[seq.find("1337;File").unwrap()..];
        let head = &head[..head.find(':').unwrap_or(head.len())];
        assert!(
            head.contains("width=1220px;height=464px"),
            "发出去的必须是 61×8 格 × 20×58 像素：{head:?}"
        );

        // 而且贴在图区的**右下角**（不是左上角）
        let (_, Some(area)) = view.split(Rect::new(0, 0, 129, 27)) else {
            panic!("有图就该留出图区")
        };
        assert_eq!((area.right() - 61, area.bottom() - 8), (68, 19));
    }
}
