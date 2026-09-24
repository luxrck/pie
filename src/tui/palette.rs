//! `/` 命令补全：候选表 + 匹配 + 面板渲染。
//!
//! **候选表是唯一事实来源**（`/help` 文案也从它生成，避免两处漂移）。
//!
//! `@` 文件路径补全（候选是扫出来的、不在这个表里）在 [`super::files`]；它只是**共用**
//! 这里的 [`panel_lines`] 画面板，匹配与接受都是另一条路（见 `App::completions`）。
//!
//! 匹配规则（三条，按序）：
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
    ("/thinking", "查看思考深度 / /thinking <level> 切换"),
    ("/model", "查看模型 / /model <id> 切换"),
    ("/balance", "查一次账号余额（结果也显示在状态栏右下角）"),
    ("/compact", "工具级 + 轮次级压缩（/compact [tools|turns|auto]）"),
    ("/compact tools", "只做工具级压缩"),
    ("/compact turns", "只做轮次级压缩"),
    ("/clear", "归档当前窗口，开新窗口"),
    ("/save", "保存会话"),
    ("/reset", "清空对话历史"),
    ("/exit", "退出"),
];

/// 已移除的旧命令 → 现在该用什么。
///
/// 「`/` 开头但不是已知命令」的输入本来是按普通消息发出去的（为了保护粘进来的绝对路径），
/// 但把 `/quit` 当成情给模型就很困惑了 —— 这几个名字拦一下，告诉用户改用什么。
pub const REMOVED: &[(&str, &str)] = &[
    ("stop", "`/stop` 已移除：按 `Esc` 停止本回合"),
    ("paste", "`/paste` 已移除：按 `Ctrl+G` 把剪贴板里的图片路径插进输入框"),
    ("quit", "`/quit` 已移除：用 `/exit`，或直接 `Ctrl+C`"),
];

/// 面板一次最多显示几项（**滚动窗口**：窗跟高亮走，见 [`panel_lines`]）。
pub const MAX_SHOWN: usize = 12;

/// `/help` 文案：由候选表生成（唯一事实来源）。
///
/// 第二行是**不住在候选表里**的几个键（`!cmd` / `Esc` / `Ctrl+G`）—— 它们不是 `/` 命令，
/// 但常用，而且 `/stop` `/paste` 移除后这两件事只剩按键入口了（2026-09-23）。
pub fn help_text() -> String {
    let rows = COMMANDS
        .iter()
        .filter(|(cmd, _)| !cmd.contains(' '))
        .map(|(cmd, desc)| format!("{cmd} {desc}"))
        .collect::<Vec<_>>()
        .join(" | ");
    format!(
        "{rows}\n\
         @path 文件路径补全（Tab 接受；默认以当前目录为根，按 .gitignore 排除；\
         `@..` 列上级目录、`@/` 列根目录、`@~/` 列主目录——这三档实时列目录，不必等扫盘）；\
         !cmd 直接执行 shell（不经过 LLM，不进会话上下文）；\
         Esc 停止当前回合；Ctrl+G 把剪贴板里的图片路径插进输入框"
    )
}

/// 是不是已移除的旧命令？是就给一句「改用什么」。
pub fn removed_hint(text: &str) -> Option<&'static str> {
    let name = text.split_whitespace().next().unwrap_or("");
    let name = name.strip_prefix('/').unwrap_or(name);
    REMOVED
        .iter()
        .find(|(cmd, _)| *cmd == name)
        .map(|(_, hint)| *hint)
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
/// 当**普通消息**发出去（否则消息会静默消失）。
pub fn is_known_command(text: &str) -> bool {
    let name = text.split_whitespace().next().unwrap_or("");
    COMMANDS
        .iter()
        .any(|(cmd, _)| cmd.split_whitespace().next() == Some(name))
}

/// 面板内容：最多 `MAX_SHOWN` 行（高亮项带 `▸` 前缀 + 亮色），放不下的折成一行提示。
///
/// 高亮项一定在窗口里：`index` 超过 `MAX_SHOWN` 时窗口**跟着高亮走**（尽量居中，贴边就贴住），
/// 行数恒为 `shown`（面板高度不跳），每行只取 `matches[start..end]` 里真正的那几项。
pub fn panel_lines(
    matches: &[(String, String)],
    index: usize,
    palette: &Palette,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if matches.is_empty() {
        return out;
    }
    let index = index.min(matches.len() - 1);
    let shown = matches.len().min(MAX_SHOWN);
    // ⚠ 两条边界都不能想当然：`saturating_sub` 防「前面不够扣」（`index < shown/2` 时
    // usize 下溢会 panic）；`.min(n - shown)` 防窗口越过后尾（shown ≤ n，所以不会反向溢出）。
    // 别用「以 index 为中心、两边各摊一份」那种算法：那样窗口长会变成 2×shown，
    // 高亮反而被 `take(shown)` 截掉。
    let start = index.saturating_sub(shown / 2).min(matches.len() - shown);
    let end = start + shown;
    for (offset, (cmd, desc)) in matches[start..end].iter().enumerate() {
        let (mark, style_cmd, style_desc) = if start + offset == index {
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
        // 说明为空就不画那截 ` — `（文件路径候选多数不带说明）
        let mut spans = vec![
            Span::styled(mark.to_string(), style_cmd),
            Span::styled(format!("{cmd} "), style_cmd),
        ];
        if !desc.is_empty() {
            spans.push(Span::styled(format!("— {desc}"), style_desc));
        }
        out.push(Line::from(spans));
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
        // `/balance` 也在表里（前缀命中）
        let found = matches("/bal", &models(), "deepseek-flash", "high");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "/balance");
        assert!(is_known_command("/balance"));
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
        assert!(help.contains("@path 文件路径补全"), "`@` 补全要进帮助：{help}");
        assert!(!help.contains("/compact tools"), "多词命令不进 /help：{help}");
    }

    /// `/stop` `/paste` `/quit` 已移除（2026-09-23，都是别名）：既不进候选表，也不进 `/help`；
    /// `/help` 的第二行必须把真正剩下的按键入口（Esc / Ctrl+G）说清楚。
    #[test]
    fn removed_aliases_are_gone_from_the_table_and_help() {
        for gone in ["/stop", "/paste", "/quit"] {
            assert!(
                !COMMANDS.iter().any(|(cmd, _)| *cmd == gone),
                "{gone} 不该还在候选表里"
            );
            assert!(!help_text().contains(gone), "{gone} 不该还在 /help 里");
        }
        // 只剩 `/exit` 一个退出入口，且 Esc / Ctrl+G 这两个别名宿主在帮助里
        assert!(is_complete_command("/exit"));
        let help = help_text();
        assert!(help.contains("Esc 停止当前回合"), "{help}");
        assert!(help.contains("Ctrl+G"), "{help}");

        // 敲旧名字要给一句「改用什么」（不要当普通消息发给模型）
        assert!(removed_hint("/stop").unwrap().contains("Esc"));
        assert!(removed_hint("/paste extra").unwrap().contains("Ctrl+G"));
        assert!(removed_hint("/quit").unwrap().contains("/exit"));
        assert!(removed_hint("/help").is_none(), "已移除的才提示");
        assert!(removed_hint("/home/x/a.png").is_none(), "拼进来的路径不拦");
    }

    #[test]
    fn known_command_guards_pasted_paths() {
        assert!(is_known_command("/help"));
        assert!(is_known_command("/model deepseek-flash"));
        assert!(!is_known_command("/Users/luxrck/.pie/files/img-abc.png"));
        assert!(!is_known_command("/tmp"));
        assert!(!is_known_command(""));
    }

    /// 高亮跑到第 8 项以后：窗口**跟着高亮走**（高亮始终可见、面板高度不变）。
    ///
    /// 早先的实现把「窗口内下标」拿去跟「原列表下标」比、还算出个能撑到 2×shown 长的窗口，
    /// 结果是 `take(shown)` 把高亮截掉（选到第 8 项就一个 ▸ 都没有）。
    #[test]
    fn panel_lines_window_follows_the_highlight() {
        let palette = Palette::mocha();
        // 样本要比上限多出富余（否则窗口根本没机会滚，`rows.len() == MAX_SHOWN` 这条也不成立）
        let n = MAX_SHOWN + 8;
        let many: Vec<(String, String)> = (0..n)
            .map(|i| (format!("/cmd{i}"), "说明".to_string()))
            .collect();
        let rows_of = |index: usize| -> Vec<String> {
            panel_lines(&many, index, &palette)
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
                .collect()
        };

        for index in 0..many.len() {
            let rows = rows_of(index);
            assert_eq!(rows.len(), MAX_SHOWN, "index={index}：窗口长度固定");
            let marked: Vec<&String> = rows.iter().filter(|r| r.starts_with('▸')).collect();
            assert_eq!(marked.len(), 1, "index={index}：有且只有一行带 ▸：{rows:?}");
            assert!(
                marked[0].starts_with(&format!("▸ /cmd{index} ")),
                "index={index}：高亮跑到别的项上了：{rows:?}"
            );
        }

        // 具体位置：开头贴顶、中间居中、末尾贴底
        assert!(rows_of(0)[0].starts_with("▸ /cmd0"), "开头贴顶");
        // 居中那一档：窗口起点 = index - MAX_SHOWN/2（两头都没贴住时）。
        // 取 `n/2` 是为了两头都留出富余：`index` 太靠前会贴顶、太靠后会贴底，都不叫居中。
        let mid = n / 2;
        assert!(
            rows_of(mid)[MAX_SHOWN / 2].starts_with(&format!("▸ /cmd{mid} ")),
            "中间居中：{:?}",
            rows_of(mid)
        );
        let tail = rows_of(n - 1);
        assert!(
            tail[0].contains(&format!("/cmd{}", n - MAX_SHOWN)),
            "末尾时窗口停在最后 {MAX_SHOWN} 项：{tail:?}"
        );
        assert!(
            tail[MAX_SHOWN - 1].starts_with(&format!("▸ /cmd{} ", n - 1)),
            "末尾贴底：{tail:?}"
        );
    }

    #[test]
    fn panel_lines_highlight_and_overflow_hint() {
        let palette = Palette::mocha();
        // 同上：样本要比上限多，窗口才真的在「有溢出」的状态下测
        let many: Vec<(String, String)> = (0..MAX_SHOWN + 8)
            .map(|i| (format!("/cmd{i}"), "说明".to_string()))
            .collect();
        let lines = panel_lines(&many, 1, &palette);
        // 面板是**滚动窗口**：最多 `MAX_SHOWN` 行（不再有「… 还有 N 个候选」那种尾巴）
        assert_eq!(lines.len(), MAX_SHOWN, "窗口最多 {MAX_SHOWN} 行");
        let text = |l: &Line<'_>| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert!(text(&lines[1]).starts_with("▸ /cmd1 "), "{}", text(&lines[1]));
        assert!(!text(&lines[0]).starts_with('▸'));

        // 项数不够就不凑数（窗口长度 = min(项数, MAX_SHOWN)）
        assert_eq!(panel_lines(&many[..3], 0, &palette).len(), 3);
        assert!(panel_lines(&[], 0, &palette).is_empty());
    }

    /// 说明为空时不再画那截 ` — `（`@` 文件候选多数不带说明）。
    #[test]
    fn panel_lines_omit_the_dash_when_there_is_no_description() {
        let palette = Palette::mocha();
        let items = vec![
            ("src/tui/app.rs".to_string(), String::new()),
            ("src/tui/".to_string(), "目录".to_string()),
        ];
        let text = |l: &Line<'_>| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        let lines = panel_lines(&items, 0, &palette);
        assert_eq!(text(&lines[0]), "▸ src/tui/app.rs ");
        assert_eq!(text(&lines[1]), "  src/tui/ — 目录");
    }
}
