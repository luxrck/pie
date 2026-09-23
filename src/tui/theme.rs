//! 配色/图标：TUI 的展示数据都收在这一处（后面要加深浅自适应，也在这里加）。
//!
//! 与 Python 版 `theme.py` 的思路一致（视图层不出现颜色字面量），但**不追求逐项对齐**：
//! 这里先给一套静态语义色（近似 Catppuccin Mocha），够用且好读。

use ratatui::style::{Color, Modifier, Style};

/// 语义色板：视图代码只认这些名字，不直接写颜色。
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// 强调色（输入提示、用户消息前缀、活动指示）
    pub accent: Color,
    /// accent 底上的文字色（输入框内选中高亮用，对齐 Python `accent_text`）
    pub accent_text: Color,
    /// 用户消息正文
    pub user: Color,
    /// 助手正文
    pub assistant: Color,
    /// 工具活动
    pub tool: Color,
    /// 成功 / 失败 / 取消
    pub ok: Color,
    pub fail: Color,
    pub cancelled: Color,
    /// 次要信息（时间、摘要、边框）
    pub muted: Color,
    /// 警示（上传失败、配置失败…）——目前给 markdown/提示预留
    #[allow(dead_code)]
    pub warn: Color,
    /// 代码块底色——目前交给 `tui-markdown` 自己的样式，留着给自写渲染用
    #[allow(dead_code)]
    pub code_bg: Color,
}

impl Default for Palette {
    fn default() -> Self {
        Self::mocha()
    }
}

impl Palette {
    /// 近似 Catppuccin Mocha（深色终端）。
    pub fn mocha() -> Self {
        Self {
            accent: Color::Rgb(137, 180, 250),
            accent_text: Color::Rgb(6, 18, 31),
            user: Color::Rgb(166, 227, 161),
            assistant: Color::Rgb(205, 214, 244),
            tool: Color::Rgb(148, 226, 213),
            ok: Color::Rgb(166, 227, 161),
            fail: Color::Rgb(243, 139, 168),
            cancelled: Color::Rgb(147, 153, 178),
            muted: Color::Rgb(108, 112, 134),
            warn: Color::Rgb(249, 226, 175),
            code_bg: Color::Rgb(30, 30, 46),
        }
    }
}

impl Palette {
    /// 「执行结果」字形只有一套（状态轴与角色轴分开，跟 Python 版约定一致）。
    pub fn mark(&self, status: Status) -> (&'static str, Color) {
        match status {
            Status::Ok => ("✓", self.ok),
            Status::Fail => ("✗", self.fail),
            Status::Cancelled => ("⏹", self.cancelled),
            Status::Running => ("•", self.muted),
        }
    }

    pub fn style_user(&self) -> Style {
        Style::default().fg(self.user)
    }
    /// 强调色（输入框聚焦边框）
    pub fn style_accent(&self) -> Style {
        Style::default().fg(self.accent)
    }
    pub fn style_assistant(&self) -> Style {
        Style::default().fg(self.assistant)
    }
    pub fn style_tool(&self) -> Style {
        Style::default().fg(self.tool)
    }
    pub fn style_muted(&self) -> Style {
        Style::default().fg(self.muted)
    }
    pub fn style_faint(&self) -> Style {
        Style::default()
            .fg(self.muted)
            .add_modifier(Modifier::ITALIC)
    }
    pub fn style_error(&self) -> Style {
        Style::default().fg(self.fail)
    }
}

/// 工具活动的状态轴（盒子/单行/状态栏共用）。
///
/// `Cancelled` 暂时没被构造（取消通路是下一步的事）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Status {
    Running,
    Ok,
    Fail,
    Cancelled,
}

/// 活动指示的 spinner 帧（braille，与 codex 那种滚动点类似但更省宽度）。
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner(frame: u64) -> &'static str {
    SPINNER[(frame as usize) % SPINNER.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_cycles_and_status_marks_are_stable() {
        assert_eq!(spinner(0), SPINNER[0]);
        assert_eq!(spinner(10), SPINNER[0], "周期回绕");
        let p = Palette::mocha();
        assert_eq!(p.mark(Status::Ok).0, "✓");
        assert_eq!(p.mark(Status::Fail).0, "✗");
        assert_eq!(p.mark(Status::Cancelled).0, "⏹");
        assert_eq!(p.mark(Status::Running).0, "•");
    }
}
