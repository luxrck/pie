//! 顶栏 / 底栏 / 活动指示：模型、上下文占用、**思考计时**、spinner。
//!
//! 顶栏只读 `Snapshot`（App 在回合开始时/结束时抓一份），回合进行中不去抢会话锁。

use std::time::Instant;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::history::fmt_duration;
use super::theme::{spinner, Palette};
/// 当前活动（决定顶栏显示什么，也带着计时起点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Idle,
    /// 已提交、还没收到任何增量（等首个响应）
    Waiting {
        since: Instant,
    },
    /// 思考中（收到过 reasoning 增量）
    Thinking {
        since: Instant,
    },
    /// 正在输出正文
    Streaming,
    /// 正在跑工具
    Tool {
        since: Instant,
    },
    /// 已请求取消，等回合收尾（`Esc` 之后）
    Stopping {
        since: Instant,
    },
}

/// 顶栏用的会话快照（回合中锁被任务握着，所以读这份缓存）。
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub model: String,
    pub cwd: String,
    pub prompt_tokens: Option<i64>,
    pub budget: i64,
    pub calls: i64,
    pub busy: bool,
}

/// 活动指示文本：`⠹ Thinking… 3.4s`（空闲 → None）。
///
/// 这是纯函数（`now` 由调用方给），方便单测与快照测试。
pub fn activity_line(
    palette: &Palette,
    activity: Activity,
    frame: u64,
    now: Instant,
) -> Option<Line<'static>> {
    let (label, since) = match activity {
        Activity::Idle => return None,
        Activity::Waiting { since } => ("Waiting", since),
        Activity::Thinking { since } => ("Thinking", since),
        Activity::Tool { since } => ("Running tool", since),
        Activity::Stopping { since } => ("Stopping", since),
        Activity::Streaming => ("Responding", now),
    };
    let elapsed = now.saturating_duration_since(since);
    Some(Line::from(vec![
        Span::styled(
            format!("{} ", spinner(frame)),
            Style::default().fg(palette.accent),
        ),
        Span::styled(label.to_string(), palette.style_muted()),
        Span::styled(
            format!("… {}", fmt_duration(elapsed)),
            Style::default().fg(palette.accent),
        ),
    ]))
}

/// 顶栏：左边 `pie-rs · <模型> · <目录>`，右边上下文占用 + 活动指示。
pub fn header_line(
    palette: &Palette,
    snap: &Snapshot,
    activity: Activity,
    frame: u64,
    now: Instant,
    width: u16,
) -> Line<'static> {
    let left = format!("pie-rs · {} · {}", snap.model, snap.cwd);
    // 形如 `53,134/920,576 (5.8%)`：绝对值给千分位，百分比一眼看得出水位。
    // provider 还没上报（新会话）就**按 0 算** —— 显示 `0/920,576 (0.0%)`，
    // 位置固定，不会一会儿是 `N calls` 一会儿是百分比。
    // （配置没写窗口 → 没有分母，这一块整个不显示。）
    let usage = if snap.budget > 0 {
        let tokens = snap.prompt_tokens.unwrap_or(0);
        format!(
            "{}/{} ({:.1}%)",
            crate::config::thousands(tokens),
            crate::config::thousands(snap.budget),
            tokens as f64 * 100.0 / snap.budget as f64
        )
    } else {
        String::new()
    };
    let mut spans = vec![Span::styled(
        format!(
            " {} ",
            trim_to(
                &left,
                width.saturating_sub(usage.len() as u16 + 24) as usize
            )
        ),
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    )];
    if !usage.is_empty() {
        spans.push(Span::styled("│ ".to_string(), palette.style_muted()));
        spans.push(Span::styled(usage, palette.style_muted()));
    }
    if let Some(activity) = activity_line(palette, activity, frame, now) {
        spans.push(Span::styled("  │ ".to_string(), palette.style_muted()));
        spans.extend(activity.spans);
    }
    Line::from(spans)
}

/// 底栏提示（键位说明；codex 也是这么一行）。
pub fn hint_line(palette: &Palette, busy: bool) -> Line<'static> {
    let hint = if busy {
        "Esc 停止 · PgUp/PgDn 滚动 · Ctrl+C 退出"
    } else {
        "⏎ 发送 · ⇧⏎ 换行 · PgUp/PgDn 滚动 · Ctrl+G 粘贴图片 · Ctrl+C 退出"
    };
    Line::from(Span::styled(format!(" {hint} "), palette.style_faint()))
}

fn trim_to(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!(
        "{}…",
        text.chars().take(max.saturating_sub(1)).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn activity_line_counts_elapsed_time() {
        let palette = Palette::mocha();
        let now = Instant::now();
        let since = now - Duration::from_millis(3400);
        let line = activity_line(&palette, Activity::Thinking { since }, 0, now).unwrap();
        let text = line_text(&line);
        assert!(text.contains("Thinking"), "{text}");
        assert!(text.contains("3.4s"), "{text}");
        assert!(activity_line(&palette, Activity::Idle, 0, now).is_none());
    }

    #[test]
    fn header_shows_model_dir_and_usage() {
        let palette = Palette::mocha();
        let snap = Snapshot {
            model: "deepseek-flash".into(),
            cwd: "~/Projects/pie".into(),
            prompt_tokens: Some(24_048),
            budget: 920_576,
            calls: 2,
            busy: false,
        };
        let text = line_text(&header_line(
            &palette,
            &snap,
            Activity::Idle,
            0,
            Instant::now(),
            100,
        ));
        assert!(text.contains("deepseek-flash"), "{text}");
        // 24,048 / 920,576 = 2.61…% → 保留一位小数
        assert!(
            text.contains("24,048/920,576 (2.6%)"),
            "千分位 + 百分比：{text}"
        );
    }

    #[test]
    fn header_trims_long_left_side() {
        let palette = Palette::mocha();
        let snap = Snapshot {
            model: "m".into(),
            cwd: "/very/long/path/that/goes/on/and/on/for/ever/and/ever".into(),
            calls: 0,
            ..Default::default()
        };
        let text = line_text(&header_line(
            &palette,
            &snap,
            Activity::Idle,
            0,
            Instant::now(),
            40,
        ));
        assert!(text.contains("…"), "{text}");
        assert!(
            !text.contains("calls"),
            "没有用量上报时不显示 `0 calls`：{text}"
        );
    }

    /// 新会话（还没任何 provider 上报）显示 `0/<预算> (0.0%)`，而不是 `0 calls`。
    #[test]
    fn header_shows_zero_usage_before_any_report() {
        let palette = Palette::mocha();
        let snap = Snapshot {
            model: "deepseek-flash".into(),
            cwd: "~/Projects/pie".into(),
            budget: 920_576,
            ..Default::default()
        };
        let text = line_text(&header_line(
            &palette,
            &snap,
            Activity::Idle,
            0,
            Instant::now(),
            100,
        ));
        assert!(text.contains("0/920,576 (0.0%)"), "{text}");
        assert!(!text.contains("calls"), "{text}");

        // 窗口没配（预算 0）→ 没有分母，整块不显示
        let no_budget = Snapshot {
            budget: 0,
            calls: 3,
            ..snap.clone()
        };
        let text = line_text(&header_line(
            &palette,
            &no_budget,
            Activity::Idle,
            0,
            Instant::now(),
            100,
        ));
        assert!(!text.contains("│"), "{text}");
        assert!(!text.contains("calls"), "{text}");
    }
}
