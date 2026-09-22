//! `/` 命令补全：候选表 + 匹配 + 面板渲染。
//!
//! 对应 Python 版 `tui.py` 的 `PALETTE_COMMANDS` / `_palette_matches` / `_render_palette`：
//! **候选表是唯一事实来源**（`/help` 文案也从它生成，避免两处漂移）。
//!
//! 匹配规则（三条，与 Python 同序）：
//!   1. `/model ` 前缀 → 端点可用模型列表（「← 当前」标注现用那个）；
//!   2. `/thinking ` 前缀 → 思考级别（`REASONING_LEVELS`，同上标注）；
//!   3. 其它以 `/` 开头 → 候选表里**前缀命中**的项（完整命令也在内，所以刚打完就还留着面板）。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::Palette;

/// 命令补全候选：(命令, 说明)。
///
/// 多词命令（`/compact tools`）照样进表：前缀匹配天然支持「打 `/c` 看到 `/compact` 和
/// `/compact tools`」；`is_complete_command` 也认它们。
pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "显示帮助"),
    ("/status", "查看 token 用量"),
    ("/stop", "取消当前回合（等价 Esc）"),
    ("/thinking", "查看思考深度 / /thinking <level> 切换"),
    ("/model", "查看模型 / /model <id> 切换"),
    ("/paste", "剪贴板里的图片 → 路径插进输入框"),
    ("/compact", "工具级 + 轮次级压缩（/compact [tools|turns|auto]）"),
    ("/compact tools", "只做工具级压缩"),
    ("/compact turns", "只做轮次级压缩"),
    ("/clear", "清空对话历史（窗口归档未接）"),
    ("/save", "保存会话"),
    ("/reset", "清空对话历史"),
    ("/exit", "退出"),
    ("/quit", "退出"),
];

/// 面板一次最多显示几项（多出来的折成一行「… 还有 N 个候选」）。
pub const MAX_SHOWN: usize = 7;

/// `/help` 文案：由候选表生成（对齐 Python 的「唯一事实来源」做法）。
///
/// 第二行是 `!cmd`（不是 `/` 命令、不进候选表，但对等常用）——与 Python `_help_text` 同款。
pub fn help_text() -> String {
    let rows = COMMANDS
        .iter()
        .filter(|(cmd, _)| !cmd.contains(' '))
        .map(|(cmd, desc)| format!("{cmd} {desc}"))
        .collect::<Vec<_>>()
        .join(" | ");
    format!("{rows}\n!cmd 直接执行 shell（不经过 LLM，不进会话上下文；Esc 或 /stop 可中断）")
}

/// 输入值对应的补全候选（空 `Vec` = 不显示面板）。
pub fn matches(value: &str, models: &[String], model: &str, effort: &str) -> Vec<(String, String)> {
    if !value.starts_with('/') {
        return Vec::new();
    }
    // `/model <prefix>`：列可用模型（当前那个标 ←）
    if let Some(prefix) = value.strip_prefix("/model ") {
        return models
            .iter()
            .filter(|m| m.starts_with(prefix))
            .map(|m| {
                let desc = if m == model { "← 当前" } else { "切换模型" };
                (format!("/model {m}"), desc.to_string())
            })
            .collect();
    }
    // `/thinking <prefix>`：列思考级别
    if let Some(prefix) = value.strip_prefix("/thinking ") {
        return crate::config::REASONING_LEVELS
            .iter()
            .filter(|lv| lv.starts_with(prefix))
            .map(|lv| {
                let desc = if *lv == effort {
                    "← 当前"
                } else {
                    "切换思考深度"
                };
                (format!("/thinking {lv}"), desc.to_string())
            })
            .collect();
    }
    COMMANDS
        .iter()
        .filter(|(cmd, _)| cmd.starts_with(value))
        .map(|(cmd, desc)| ((*cmd).to_string(), (*desc).to_string()))
        .collect()
}

/// 输入值是否已经是完整命令（此时回车直接提交，不再替换成候选）。
pub fn is_complete_command(value: &str) -> bool {
    COMMANDS.iter().any(|(cmd, _)| *cmd == value)
}

/// 首词是不是已知命令名。
///
/// 不能只看 `/` 开头——粘贴进来的绝对路径（`/Users/.../img-x.png`）是最常见的误伤，那种一律
/// 当**普通消息**发出去（对齐 Python 的 `is_known_command`，否则消息会静默消失）。
pub fn is_known_command(text: &str) -> bool {
    let name = text.split_whitespace().next().unwrap_or("");
    COMMANDS
        .iter()
        .any(|(cmd, _)| cmd.split_whitespace().next() == Some(name))
}

/// 面板内容：最多 `MAX_SHOWN` 项（高亮项带 `▸` 前缀 + 亮色），超出折成一行提示。
pub fn panel_lines(
    matches: &[(String, String)],
    index: usize,
    palette: &Palette,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if matches.is_empty() {
        return out;
    }
    let shown = matches.len().min(MAX_SHOWN);
    let index = index.min(shown - 1);
    for (i, (cmd, desc)) in matches.iter().take(shown).enumerate() {
        let (mark, style_cmd, style_desc) = if i == index {
            (
                "▸ ",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
                palette.style_muted(),
            )
        } else {
            ("  ", palette.style_muted(), palette.style_faint())
        };
        out.push(Line::from(vec![
            Span::styled(mark.to_string(), style_cmd),
            Span::styled(format!("{cmd} "), style_cmd),
            Span::styled(format!("— {desc}"), style_desc),
        ]));
    }
    if matches.len() > shown {
        out.push(Line::from(Span::styled(
            format!("  … 还有 {} 个候选", matches.len() - shown),
            palette.style_faint(),
        )));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Vec<String> {
        vec!["deepseek-chat".into(), "deepseek-flash".into()]
    }

    #[test]
    fn matches_commands_by_prefix() {
        let found = matches("/co", &models(), "deepseek-flash", "high");
        let names: Vec<&str> = found.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(names, vec!["/compact", "/compact tools", "/compact turns"]);

        // 完整命令也留着（面板不闪断）
        let found = matches("/help", &models(), "deepseek-flash", "high");
        assert_eq!(found.len(), 1);

        // 非 `/` 开头：不显示
        assert!(matches("你好", &models(), "deepseek-flash", "high").is_empty());
        // 不是已知命令的绝对路径也别乱弹（`/home/x.png` 不命中任何候选）
        assert!(matches("/home/luxrck/x.png", &models(), "deepseek-flash", "high").is_empty());
    }

    #[test]
    fn matches_model_and_thinking_prefixes() {
        let found = matches("/model deepseek-f", &models(), "deepseek-flash", "high");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "/model deepseek-flash");
        assert_eq!(found[0].1, "← 当前");

        let found = matches("/model ", &models(), "deepseek-chat", "high");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].1, "← 当前");

        let found = matches("/thinking ", &models(), "x", "high");
        assert_eq!(found.len(), 4);
        assert!(found.iter().any(|(c, d)| c == "/thinking high" && d == "← 当前"));
        let found = matches("/thinking l", &models(), "x", "high");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "/thinking low");
    }

    #[test]
    fn complete_command_and_help_text() {
        assert!(is_complete_command("/help"));
        assert!(is_complete_command("/compact tools"));
        assert!(!is_complete_command("/he"));
        let help = help_text();
        assert!(help.contains("/help 显示帮助"), "{help}");
        assert!(help.contains("!cmd 直接执行 shell"), "！模式要进帮助：{help}");
        assert!(!help.contains("/compact tools"), "多词命令不进 /help：{help}");
    }

    #[test]
    fn known_command_guards_pasted_paths() {
        assert!(is_known_command("/help"));
        assert!(is_known_command("/model deepseek-flash"));
        assert!(!is_known_command("/Users/luxrck/.pie/files/img-abc.png"));
        assert!(!is_known_command("/tmp"));
        assert!(!is_known_command(""));
    }

    #[test]
    fn panel_lines_highlight_and_overflow_hint() {
        let palette = Palette::mocha();
        let many: Vec<(String, String)> = (0..10)
            .map(|i| (format!("/cmd{i}"), "说明".to_string()))
            .collect();
        let lines = panel_lines(&many, 1, &palette);
        assert_eq!(lines.len(), MAX_SHOWN + 1, "7 项 + 溢出提示");
        let text = |l: &Line<'_>| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert!(text(&lines[1]).starts_with("▸ /cmd1 "), "{}", text(&lines[1]));
        assert!(!text(&lines[0]).starts_with('▸'));
        assert!(text(&lines[MAX_SHOWN]).contains("还有 3 个候选"));
    }
}
