//! 配色/图标：TUI 的展示数据都收在这一处（后面要加深浅自适应，也在这里加）。
//!
//! 色值不再手写：全部取自 `catppuccin` crate 的四个 **flavor**（Mocha / Macchiato / Frappé / Latte）。
//! 之前那套「近似 Mocha」的 RGB 本来就逐个对应得上，现在直接取真值；语义槽↔槽位的映射只有
//! `Palette::from_flavor` 一处（四个 flavor 的槽位名完全一致，所以映射共用）。
//! 视图代码依旧只认语义名（`Palette` 的字段），不认 Catppuccin 的槽位名。

use catppuccin::{FlavorName, PALETTE};
use ratatui::style::{Color, Modifier, Style};

/// 语义色板：视图代码只认这些名字，不直接写颜色。
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// 强调色（输入提示、用户消息前缀、活动指示）—— mocha `blue`
    pub accent: Color,
    /// accent 底上的文字色（输入框内选中高亮用，对齐 Python `accent_text`）—— mocha `crust`
    pub accent_text: Color,
    /// 用户消息正文 —— mocha `green`
    pub user: Color,
    /// 助手正文 —— mocha `text`
    pub assistant: Color,
    /// 工具活动 —— mocha `teal`
    pub tool: Color,
    /// 成功 / 失败 / 取消 —— mocha `green` / `red` / `overlay2`
    pub ok: Color,
    pub fail: Color,
    pub cancelled: Color,
    /// 次要信息（时间、摘要、边框）—— mocha `overlay0`
    pub muted: Color,
    /// 警示（上传失败、配置失败…）——目前给 markdown/提示预留 —— mocha `yellow`
    #[allow(dead_code)]
    pub warn: Color,
    /// 代码块底色——目前交给 `tui-markdown` 自己的样式，留着给自写渲染用 —— mocha `base`
    #[allow(dead_code)]
    pub code_bg: Color,
}

impl Default for Palette {
    fn default() -> Self {
        Self::mocha()
    }
}

impl Palette {
    /// 一份「语义槽 → Catppuccin 槽位」的映射，四个 flavor 共用（各 flavor 的槽位名一致）。
    ///
    /// `.into()` 命中的是 crate `ratatui` feature 给的
    /// `From<catppuccin::Color> for ratatui_core::style::Color`（展开就是 `Color::Rgb`）——
    /// 它锁的 ratatui-core 与 ratatui 0.30 用的是同一份，所以类型直接对得上。
    pub fn from_flavor(flavor: FlavorName) -> Self {
        let m = PALETTE.get_flavor(flavor).colors;
        Self {
            accent: m.blue.into(),
            accent_text: m.crust.into(),
            user: m.green.into(),
            assistant: m.text.into(),
            tool: m.teal.into(),
            ok: m.green.into(),
            fail: m.red.into(),
            cancelled: m.overlay2.into(),
            muted: m.overlay0.into(),
            warn: m.yellow.into(),
            code_bg: m.base.into(),
        }
    }

    /// 四个 flavor 的快捷构造（由深到浅：Mocha / Macchiato / Frappé / Latte）。
    ///
    /// `latte` 是唯一的**浅色**变体（其余三个都是深色）——它对应的终端应该配深色前景，
    /// 但 `Config.theme` 还没接线，所以目前只有 `Mocha` 会被真正用到（`Default`）。
    pub fn mocha() -> Self {
        Self::from_flavor(FlavorName::Mocha)
    }
    pub fn macchiato() -> Self {
        Self::from_flavor(FlavorName::Macchiato)
    }
    pub fn frappe() -> Self {
        Self::from_flavor(FlavorName::Frappe)
    }
    pub fn latte() -> Self {
        Self::from_flavor(FlavorName::Latte)
    }

    /// 把 `Config.theme` 里的字符串解成色板：认具体 flavor（`mocha` / `macchiato` / `frappe` /
    /// `latte`）、Python 那种带族名的写法（`catppuccin-<flavor>`）、以及族名 `catppuccin` 本身。
    /// 大小写、前后空白、重音都宽容（`Frappé` / `catppuccin-frappé` 都认）。
    ///
    /// 族名不带变体时怎么选：**没有终端背景（OSC 11）探测** → 按深色取 `mocha`（与 Python 版
    /// 「探测不到就按深色」的习惯一致）；真想要浅色得写全名 `catppuccin-latte`。
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "mocha" | "catppuccin-mocha" | "catppuccin" => Some(Self::mocha()),
            "macchiato" | "catppuccin-macchiato" => Some(Self::macchiato()),
            "frappe" | "frappé" | "catppuccin-frappe" | "catppuccin-frappé" => Some(Self::frappe()),
            "latte" | "catppuccin-latte" => Some(Self::latte()),
            _ => None,
        }
    }

    /// [`Self::from_name`] + 认不出来时该提的那句告警（`None` = 认出来了，不用提）。
    ///
    /// 告警文案住在这里（而不是调用方），因为「哪些名字有效」只有本模块知道；TUI 拿它去
    /// `log::warn`（界面里就是消息流一条 `· …`）。
    pub fn resolve(name: &str) -> (Self, Option<String>) {
        match Self::from_name(name) {
            Some(palette) => (palette, None),
            None => (
                Self::mocha(),
                Some(format!("[theme] 认不出的主题 `{name}`，按 catppuccin-mocha 显示")),
            ),
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
    /// 强调色（accent 槽原样；「聚焦才亮」的那一档见 [`Palette::style_emphasis`]）
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

    /// 选中高亮（输入框选区 / 消息流框选）：accent 底 + accent_text 字。
    pub fn style_selection(&self) -> Style {
        Style::default().bg(self.accent).fg(self.accent_text)
    }

    /// **需要注意力的一档颜色**（输入框上边框 / 状态栏名字 / spinner / 耗时…）：
    /// 聚焦 = accent；**窗口失焦 = muted**。
    ///
    /// 全界面统一口径就这一处：`focused` 从 `App::focused`（`FocusGained` / `FocusLost`）一路传下来，
    /// 各视图调这个方法就行，**别在模块里再写一遍「失焦就 muted」**（写了就会各自跑偏）。
    pub fn style_emphasis(&self, focused: bool) -> Style {
        if focused {
            self.style_accent()
        } else {
            self.style_muted()
        }
    }

    /// 光标块（输入框那个色块）：聚焦 = [`Self::style_selection`]（accent 底 + accent_text 字）；
    /// **失焦 = muted 底**（跟状态栏一起暗下去）。
    ///
    /// 颜色必须给死：控件默认是裸 `REVERSED`，那颜色会跟着**所在行**的样式跑——空输入那行是
    /// placeholder 的 muted + ITALIC，反色出来是一块暗灰（看着像没光标）。
    pub fn style_caret(&self, focused: bool) -> Style {
        if focused {
            self.style_selection()
        } else {
            Style::default().bg(self.muted)
        }
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

    /// 「聚焦才亮」的几档颜色全在色板这一处定义：`style_emphasis`（失焦 = muted）、
    /// `style_selection` / `style_caret`（accent 底 + accent_text 字；失焦的光标 = muted 底）。
    #[test]
    fn focus_sensitive_styles_live_in_the_palette() {
        let p = Palette::mocha();

        // 需要注意力的一档：聚焦 = accent，失焦 = muted（输入框上边框 / 状态栏名字都走它）
        assert_eq!(p.style_emphasis(true).fg, Some(p.accent));
        assert_eq!(p.style_emphasis(false).fg, Some(p.muted));
        assert_eq!(
            p.style_emphasis(false).add_modifier,
            p.style_muted().add_modifier,
            "失焦时不该带任何额外修饰（粗体由调用方自己加）"
        );

        // 选中高亮：accent 底 + accent_text 字（输入框选区 / 消息流框选同一对颜色）
        assert_eq!(p.style_selection().bg, Some(p.accent));
        assert_eq!(p.style_selection().fg, Some(p.accent_text));

        // 光标：聚焦就是选中高亮那对颜色；失焦只剩 muted 底（不再有上色）
        assert_eq!(p.style_caret(true), p.style_selection());
        assert_eq!(p.style_caret(false).bg, Some(p.muted));
        assert_eq!(p.style_caret(false).fg, None, "失焦光标不给字色");

        // 换个 flavor 也成立（不是把 mocha 的值写死了）
        let latte = Palette::latte();
        assert_eq!(latte.style_emphasis(true).fg, Some(latte.accent));
        assert_eq!(latte.style_caret(false).bg, Some(latte.muted));
    }

    /// 色板 → [(字段名, 颜色)]：让「11 个语义槽都接上了」这条能遍历断言。
    fn fields(p: &Palette) -> [(&'static str, Color); 11] {
        [
            ("accent", p.accent),
            ("accent_text", p.accent_text),
            ("user", p.user),
            ("assistant", p.assistant),
            ("tool", p.tool),
            ("ok", p.ok),
            ("fail", p.fail),
            ("cancelled", p.cancelled),
            ("muted", p.muted),
            ("warn", p.warn),
            ("code_bg", p.code_bg),
        ]
    }

    fn rgb(hex: u32) -> Color {
        Color::Rgb(
            (hex >> 16) as u8,
            ((hex >> 8) & 0xff) as u8,
            (hex & 0xff) as u8,
        )
    }

    #[test]
    fn palette_uses_catppuccin_mocha_values() {
        // 钉住槽位映射（槽接错了 / crate 换版改了色值，这里就红）。
        let p = Palette::mocha();
        assert_eq!(p.accent, Color::Rgb(0x89, 0xb4, 0xfa), "mocha blue");
        assert_eq!(p.assistant, Color::Rgb(0xcd, 0xd6, 0xf4), "mocha text");
        assert_eq!(p.fail, Color::Rgb(0xf3, 0x8b, 0xa8), "mocha red");
        assert_eq!(p.muted, Color::Rgb(0x6c, 0x70, 0x86), "mocha overlay0");
        assert_eq!(p.code_bg, Color::Rgb(0x1e, 0x1e, 0x2e), "mocha base");
        assert_eq!(Palette::default().accent, p.accent, "默认就是 mocha");
        // 色板里不该出现 16 色 / 默认色（全是从 crate 取的真彩）。
        for (name, color) in fields(&p) {
            assert!(matches!(color, Color::Rgb(..)), "{name} 不是真彩色");
        }
    }

    #[test]
    fn all_four_flavors_are_wired_to_catppuccin() {
        // 每个 flavor 两个取样点（accent = blue / assistant = text，hex 取自官方 palette.json）。
        // 这颗能同时抓到「某个 flavor 没接上」「四个都指向同一个 flavor」两类错误。
        for (name, p, blue, text) in [
            ("mocha", Palette::mocha(), 0x89b4fa, 0xcdd6f4),
            ("macchiato", Palette::macchiato(), 0x8aadf4, 0xcad3f5),
            ("frappe", Palette::frappe(), 0x8caaee, 0xc6d0f5),
            ("latte", Palette::latte(), 0x1e66f5, 0x4c4f69),
        ] {
            assert_eq!(p.accent, rgb(blue), "{name} 的 accent 不是官方 blue");
            assert_eq!(p.assistant, rgb(text), "{name} 的 assistant 不是官方 text");
            for (field, color) in fields(&p) {
                assert!(matches!(color, Color::Rgb(..)), "{name}.{field} 不是真彩色");
            }
        }
        // 浅色 vs 深色确实不同（否则「接了 flavor」是假的）；from_flavor 与四个快捷构造等价。
        assert_ne!(Palette::latte().assistant, Palette::mocha().assistant);
        for (flavor, p) in [
            (FlavorName::Mocha, Palette::mocha()),
            (FlavorName::Macchiato, Palette::macchiato()),
            (FlavorName::Frappe, Palette::frappe()),
            (FlavorName::Latte, Palette::latte()),
        ] {
            assert_eq!(
                fields(&Palette::from_flavor(flavor)),
                fields(&p),
                "from_flavor({flavor:?}) 与快捷构造应等价"
            );
        }
    }

    #[test]
    fn from_name_accepts_flavor_and_family_spellings() {
        for (name, want) in [
            ("mocha", Palette::mocha()),
            ("Mocha", Palette::mocha()),
            (" catppuccin-mocha ", Palette::mocha()),
            // 族名不带变体：没有 OSC 11 背景探测 → 按深色解
            ("catppuccin", Palette::mocha()),
            ("CATPPUCCIN", Palette::mocha()),
            ("macchiato", Palette::macchiato()),
            ("catppuccin-macchiato", Palette::macchiato()),
            ("frappe", Palette::frappe()),
            ("Frappé", Palette::frappe()),
            ("catppuccin-frappé", Palette::frappe()),
            ("latte", Palette::latte()),
            ("catppuccin-latte", Palette::latte()),
        ] {
            let got = Palette::from_name(name).unwrap_or_else(|| panic!("`{name}` 应该认得"));
            assert_eq!(fields(&got), fields(&want), "`{name}` 解错 flavor 了");
        }
        for name in ["", "  ", "dracula", "catppuccin-", "mochaa", "catppuccin - latte"] {
            assert!(Palette::from_name(name).is_none(), "`{name}` 不该认");
        }
    }

    #[test]
    fn resolve_falls_back_to_mocha_and_carries_a_warning() {
        assert_eq!(Palette::resolve("latte").1, None, "认得出来就不提告警");
        let (p, warning) = Palette::resolve("dracula");
        assert_eq!(fields(&p), fields(&Palette::mocha()), "认不出就用 mocha");
        let warning = warning.expect("认不出要提一句");
        assert!(warning.starts_with("[theme]"), "告警样式要与别的告警一致：{warning}");
        assert!(warning.contains("dracula"), "告警里要带上认不出的名字：{warning}");
        // 空串（用户写了 `theme = ""`）走同一条退路
        assert!(Palette::resolve("").1.is_some());
    }
}
