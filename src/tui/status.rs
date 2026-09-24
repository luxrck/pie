//! 状态栏 / 活动指示：模型、上下文占用、**思考计时**、spinner（渲染在界面**最下方**）。
//!
//! 窗口不在前台时整条一起变灰（`focused = false` → “活的”那两档从 accent 降成 muted），
//! 与输入框上边框 / 光标同一口径（见 [`status_line`] / [`activity_line`]）。
//!
//! 状态栏只读 `Snapshot`（App 在回合开始时/结束时抓一份），回合进行中不去抢会话锁。

use std::time::Instant;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::history::fmt_duration;
use super::theme::{spinner, Palette};
/// 当前活动（决定状态栏显示什么，也带着计时起点）。
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
    /// 正在输出正文（`since` = **首个**正文增量那一刻）
    Streaming {
        since: Instant,
    },
    /// 正在跑工具
    Tool {
        since: Instant,
    },
    /// 已请求取消，等回合收尾（`Esc` 之后）
    Stopping {
        since: Instant,
    },
}

/// 状态栏用的会话快照（回合中锁被任务握着，所以读这份缓存）。
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub model: String,
    pub reasoning_effort: String,
    pub cwd: String,
    pub prompt_tokens: Option<i64>,
    pub budget: usize,
    pub calls: i64,
    pub busy: bool,
}

/// 活动指示文本：`⠹ Thinking… 3.4s`（空闲 → None）。
///
/// 这是纯函数（`now` 由调用方给），方便单测与快照测试。
///
/// `focused = false`（终端窗口不在前台）→ 强调色降成 muted（`Palette::style_emphasis`）：整条
/// 状态栏一起变灰，与输入框上边框 / 光标同一个口径。
pub fn activity_line(
    palette: &Palette,
    activity: Activity,
    frame: u64,
    now: Instant,
    focused: bool,
) -> Option<Line<'static>> {
    let (label, since) = match activity {
        Activity::Idle => return None,
        Activity::Waiting { since } => ("Waiting", since),
        Activity::Thinking { since } => ("Thinking", since),
        Activity::Tool { since } => ("Running tool", since),
        Activity::Stopping { since } => ("Stopping", since),
        Activity::Streaming { since } => ("Responding", since),
    };
    let accent = palette.style_emphasis(focused);
    let elapsed = now.saturating_duration_since(since);
    Some(Line::from(vec![
        Span::styled(format!("{} ", spinner(frame)), accent),
        Span::styled(label.to_string(), palette.style_muted()),
        Span::styled(format!("… {}", fmt_duration(elapsed)), accent),
    ]))
}

/// 状态栏：**左边**是常驻信息 `<模型> <思考深度> · <目录> │ <上下文用量> │ <余额>`
/// （**不带 `pie-rs` 前缀**——用户点名去掉，省下的列给目录）；**右边**贴屏幕右边缘的只有
/// **活动指示**（转圈 + 耗时）——它一直变，单独占右边一位，不会把用量/余额推来推去。
///
/// `balance` = [`balance_text`] 的产物（拿不到就给 `None`，那一块就不画）。
/// `focused = false`（窗口不在前台）→ 名字与活动指示一起降成 muted（与输入框上边框 / 光标同款）；
/// 用量 / 余额本来就用 muted，不动。
pub fn status_line(
    palette: &Palette,
    snap: &Snapshot,
    activity: Activity,
    frame: u64,
    now: Instant,
    width: u16,
    balance: Option<&str>,
    focused: bool,
) -> Line<'static> {
    // 思考深度为空就不占位（不然会多一个空段落：`deepseek-flash  · /tmp`）
    let effort = snap.reasoning_effort.trim();
    let name = if effort.is_empty() {
        format!("{} · {}", snap.model, snap.cwd)
    } else {
        format!("{} {} · {}", snap.model, effort, snap.cwd)
    };
    // 形如 `53,134/920,576 (5.8%)`：绝对值给千分位，百分比一眼看得出水位。
    // provider 还没上报（新会话）就**按 0 算** —— 显示 `0/920,576 (0.0%)`，
    // 位置固定，不会一会儿是 `N calls` 一会儿是百分比。
    // （配置没写窗口 → 没有分母，这一块整个不显示。）
    let usage = if snap.budget > 0 {
        let tokens = snap.prompt_tokens.unwrap_or(0);
        format!(
            "{}/{} ({:.1}%)",
            crate::config::thousands(tokens),
            crate::config::thousands(snap.budget as i64),
            tokens as f64 * 100.0 / snap.budget as f64
        )
    } else {
        String::new()
    };

    // —— 左侧尾部：用量 │ 余额（都是常驻信息，紧跟在名字后面）
    let mut tail: Vec<Span<'static>> = Vec::new();
    let mut tail_len = 0usize;
    if !usage.is_empty() {
        tail.push(Span::styled("│ ".to_string(), palette.style_muted()));
        tail_len += 2;
        tail_len += usage.chars().count();
        tail.push(Span::styled(usage, palette.style_muted()));
    }
    if let Some(balance) = balance {
        tail.push(Span::styled("  │ ".to_string(), palette.style_muted()));
        tail_len += 4;
        tail_len += balance.chars().count();
        tail.push(Span::styled(balance.to_string(), palette.style_muted()));
    }

    // —— 右侧：只有活动指示（空闲 → 什么也没有）
    let right = activity_line(palette, activity, frame, now, focused);
    let right_len = right
        .as_ref()
        .map(|line| line.spans.iter().map(|s| s.content.chars().count()).sum::<usize>())
        .unwrap_or(0);

    // 名字按剩余宽度截尾（右侧块 + 尾部 + 两边空格 + 至少 1 格间隔）
    let name_max = (width as usize).saturating_sub(tail_len + right_len + 3);
    let name_text = trim_to(&name, name_max);
    // 名字是这一行里最「亮」的一档——窗口失焦时它跟活动指示一起变灰（连粗体一起去掉）。
    let mut name_style = palette.style_emphasis(focused);
    if focused {
        name_style = name_style.add_modifier(Modifier::BOLD);
    }
    let mut spans = vec![Span::styled(format!(" {name_text} "), name_style)];
    spans.extend(tail);
    // 中间填空：把活动指示顶到屏幕右边缘
    let used = name_text.chars().count() + 2 + tail_len;
    let pad = (width as usize).saturating_sub(used + right_len);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    if let Some(right) = right {
        spans.extend(right.spans);
    }
    Line::from(spans)
}

/// 余额 → 状态栏右下角那一小段文本（`¥110.00`；多币种用 `·` 连）。
///
/// 金额**直接用服务端给的字符串**（它特意用字符串传输避免浮点误差，这里也不动它）；
/// 币种给个常见符号，认不出就用代码本身。一条明细都没有 → `None`（那一块不画）。
pub fn balance_text(balance: &crate::llm::Balance) -> Option<String> {
    if balance.balance_infos.is_empty() {
        return None;
    }
    Some(
        balance
            .balance_infos
            .iter()
            .map(|b| format!("{}{}", currency_symbol(&b.currency), b.total_balance))
            .collect::<Vec<_>>()
            .join(" · "),
    )
}

/// 余额明细（`/balance` 用）：总余额 + 赠金/充值拆开 + 能不能调 API。
pub fn balance_detail(balance: &crate::llm::Balance) -> String {
    let mut lines: Vec<String> = balance
        .balance_infos
        .iter()
        .map(|b| {
            let s = currency_symbol(&b.currency);
            format!(
                "{s}{}（赠金 {s}{} / 充值 {s}{}）",
                b.total_balance, b.granted_balance, b.topped_up_balance
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push("（服务端没给余额明细）".to_string());
    }
    lines.push(format!(
        "可调用 API：{}",
        if balance.is_available {
            "是"
        } else {
            "否（余额不足或已停用）"
        }
    ));
    lines.join("\n")
}

fn currency_symbol(currency: &str) -> String {
    match currency {
        "CNY" => "¥".to_string(),
        "USD" => "$".to_string(),
        other => format!("{other} "),
    }
}

/// 键位提示文案（**唯一事实来源**）。
///
/// 它不是单独占一行渲染的底栏，而是被 `App::render` 当成**输入框的 placeholder** 嗂进去
/// （空输入时才显示）——省下一整行给消息流；代价是输入框里有字时它就看不见了。
pub fn hint_text(busy: bool) -> &'static str {
    if busy {
        "Esc 停止 · PgUp/PgDn 滚动 · Ctrl+C 退出"
    } else {
        "⏎ 发送 · ⇧⏎ 换行 · PgUp/PgDn 滚动 · Ctrl+G 粘贴图片 · Ctrl+C 退出"
    }
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
        let line = activity_line(&palette, Activity::Thinking { since }, 0, now, true).unwrap();
        let text = line_text(&line);
        assert!(text.contains("Thinking"), "{text}");
        assert!(text.contains("3.4s"), "{text}");
        assert!(activity_line(&palette, Activity::Idle, 0, now, true).is_none());
    }

    #[test]
    fn status_line_shows_model_dir_and_usage() {
        let palette = Palette::mocha();
        let snap = Snapshot {
            model: "deepseek-flash".into(),
            reasoning_effort: "high".into(),
            cwd: "~/Projects/pie".into(),
            prompt_tokens: Some(24_048),
            budget: 920_576,
            calls: 2,
            busy: false,
        };
        let text = line_text(&status_line(
            &palette,
            &snap,
            Activity::Idle,
            0,
            Instant::now(),
            100,
            None,
            true,
        ));
        assert!(text.contains("deepseek-flash"), "{text}");
        assert!(
            text.contains("deepseek-flash high · ~/Projects/pie"),
            "左边一直显示思考深度（`/thinking` 改了能一眼看出）：{text}"
        );
        // 不带 `pie-rs` 前缀（用户点名去掉）——看行首就是模型名（别拿 `contains` 判：cwd 里
        // 完全可能正好叫 pie-rs）
        assert!(text.starts_with(" deepseek-flash"), "{text}");
        // 深度为空 → 不占位（`{模型} · {目录}`，不会多一个空段落）
        let no_effort = Snapshot {
            reasoning_effort: String::new(),
            ..snap.clone()
        };
        let plain = line_text(&status_line(
            &palette,
            &no_effort,
            Activity::Idle,
            0,
            Instant::now(),
            100,
            None,
            true,
        ));
        assert!(
            plain.contains("deepseek-flash · ~/Projects/pie"),
            "深度为空就不占位：{plain}"
        );
        // 24,048 / 920,576 = 2.61…% → 保留一位小数
        assert!(
            text.contains("24,048/920,576 (2.6%)"),
            "千分位 + 百分比：{text}"
        );
    }

    #[test]
    fn status_line_trims_long_left_side() {
        let palette = Palette::mocha();
        let snap = Snapshot {
            model: "m".into(),
            cwd: "/very/long/path/that/goes/on/and/on/for/ever/and/ever".into(),
            calls: 0,
            ..Default::default()
        };
        let text = line_text(&status_line(
            &palette,
            &snap,
            Activity::Idle,
            0,
            Instant::now(),
            40,
            None,
            true,
        ));
        assert!(text.contains("…"), "{text}");
        assert!(
            !text.contains("calls"),
            "没有用量上报时不显示 `0 calls`：{text}"
        );
    }

    /// 用量和余额都挂在**左边**（紧跟名字）；**右边**只有活动指示，贴屏幕右边缘。
    #[test]
    fn status_line_groups_usage_and_balance_on_the_left() {
        use crate::llm::{Balance, BalanceInfo};
        let palette = Palette::mocha();
        let snap = Snapshot {
            model: "deepseek-flash".into(),
            reasoning_effort: "high".into(),
            cwd: "~/Projects/pie".into(),
            prompt_tokens: Some(24_048),
            budget: 920_576,
            ..Default::default()
        };
        let balance = Balance {
            is_available: true,
            balance_infos: vec![BalanceInfo {
                currency: "CNY".into(),
                total_balance: "110.00".into(),
                granted_balance: "10.00".into(),
                topped_up_balance: "100.00".into(),
            }],
        };
        let text = balance_text(&balance).expect("有明细就有文本");
        assert_eq!(text, "¥110.00");
        let left = " deepseek-flash high · ~/Projects/pie │ 24,048/920,576 (2.6%)  │ ¥110.00";

        // 空闲：左边一串常驻信息，右边什么都没有（剩下的都是填空）
        let idle = line_text(&status_line(
            &palette,
            &snap,
            Activity::Idle,
            0,
            Instant::now(),
            100,
            Some(&text),
            true,
        ));
        assert!(idle.starts_with(left), "用量/余额都挨着名字：{idle:?}");
        assert_eq!(idle.chars().count(), 100, "整行照旧填满：{idle:?}");
        assert!(
            idle[left.len()..].trim().is_empty(),
            "空闲时右边不该有东西：{idle:?}"
        );

        // 忙时：活动指示贴到右边缘，左边的常驻信息一列不挪
        let since = Instant::now();
        let busy = line_text(&status_line(
            &palette,
            &snap,
            Activity::Waiting { since },
            0,
            Instant::now(),
            100,
            Some(&text),
            true,
        ));
        assert!(busy.starts_with(left), "左边不动：{busy:?}");
        assert!(busy.ends_with("Waiting… 0.0s"), "活动指示在右边缘：{busy:?}");
        assert_eq!(busy.chars().count(), 100, "{busy:?}");

        // 窄窗口：名字截尾，但用量/余额（左）与活动指示（右）都还在
        let narrow = line_text(&status_line(
            &palette,
            &snap,
            Activity::Waiting { since },
            0,
            Instant::now(),
            40,
            Some(&text),
            true,
        ));
        assert!(narrow.contains('…'), "名字截尾：{narrow:?}");
        assert!(narrow.contains("¥110.00"), "{narrow:?}");
        assert!(narrow.ends_with("Waiting… 0.0s"), "{narrow:?}");
    }

    #[test]
    fn balance_text_and_detail_cover_the_currency_cases() {
        use crate::llm::{Balance, BalanceInfo};
        let info = |currency: &str, total: &str| BalanceInfo {
            currency: currency.into(),
            total_balance: total.into(),
            granted_balance: "0.00".into(),
            topped_up_balance: total.into(),
        };
        assert_eq!(
            balance_text(&Balance {
                is_available: false,
                balance_infos: vec![],
            }),
            None,
            "一条明细都没就 None"
        );
        assert_eq!(
            balance_text(&Balance {
                is_available: true,
                balance_infos: vec![info("USD", "1.50")],
            }),
            Some("$1.50".into())
        );
        assert_eq!(
            balance_text(&Balance {
                is_available: true,
                balance_infos: vec![info("CNY", "110.00"), info("USD", "1.50")],
            }),
            Some("¥110.00 · $1.50".into()),
            "多币种用 · 连"
        );
        assert_eq!(
            balance_text(&Balance {
                is_available: true,
                balance_infos: vec![info("EUR", "2.00")],
            }),
            Some("EUR 2.00".into()),
            "认不出的币种就用代码"
        );

        let detail = balance_detail(&Balance {
            is_available: false,
            balance_infos: vec![BalanceInfo {
                currency: "CNY".into(),
                total_balance: "0.00".into(),
                granted_balance: "0.00".into(),
                topped_up_balance: "0.00".into(),
            }],
        });
        assert!(detail.contains("¥0.00（赠金 ¥0.00 / 充值 ¥0.00）"), "{detail}");
        assert!(detail.contains("可调用 API：否"), "{detail}");
    }

    /// 窗口失焦 → 状态栏里「亮」的几档（名字 / 转圈 / 耗时）一起降成 muted，与输入框上边框、
    /// 光标同一口径；用量 / 余额本来就用 muted，不动。**只是颜色变，文本一个字不变**。
    #[test]
    fn status_line_dims_when_the_window_loses_focus() {
        let palette = Palette::mocha();
        let snap = Snapshot {
            model: "deepseek-flash".into(),
            reasoning_effort: "high".into(),
            cwd: "/tmp".into(),
            budget: 0, // 不显示用量，省得混进尾部的 muted 分隔符
            ..Default::default()
        };
        let now = Instant::now();
        let line = |focused: bool| {
            status_line(
                &palette,
                &snap,
                Activity::Thinking { since: now },
                0,
                now,
                100,
                None,
                focused,
            )
        };
        let accent_spans = |line: &Line<'_>| {
            line.spans
                .iter()
                .filter(|s| s.style.fg == Some(palette.accent))
                .count()
        };

        let on = line(true);
        assert_eq!(accent_spans(&on), 3, "聚焦：名字 + 转圈 + 耗时：{on:?}");
        assert!(
            on.spans[0].style.add_modifier.contains(Modifier::BOLD),
            "聚焦：名字带粗体"
        );

        let off = line(false);
        assert_eq!(accent_spans(&off), 0, "失焦：一个 accent 都不剩：{off:?}");
        assert!(
            !off.spans[0].style.add_modifier.contains(Modifier::BOLD),
            "失焦：名字不再加粗（变灰就该真的变灰）"
        );
        assert_eq!(off.spans[0].style.fg, Some(palette.muted), "名字降成 muted");
        assert!(
            off.spans
                .iter()
                .all(|s| s.style.fg.is_none() || s.style.fg == Some(palette.muted)),
            "失焦后整行只有 muted 一档：{off:?}"
        );
        assert_eq!(line_text(&on), line_text(&off), "变的是颜色，不是文本");

        // 空闲（右边没活动指示）：失焦时名字一样变灰
        let idle = status_line(&palette, &snap, Activity::Idle, 0, now, 100, None, false);
        assert_eq!(accent_spans(&idle), 0, "{idle:?}");
        assert_eq!(idle.spans[0].style.fg, Some(palette.muted));
    }

    /// `Streaming` 也必须带自己的起点：之前拿 `now` 当 `since` → 一直显示 0.0s。
    #[test]
    fn streaming_counts_from_its_own_start() {
        let palette = Palette::mocha();
        let now = Instant::now();
        let line = activity_line(
            &palette,
            Activity::Streaming {
                since: now - std::time::Duration::from_millis(1500),
            },
            0,
            now,
            true,
        )
        .expect("非空闲就有行");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Responding"), "{text}");
        assert!(text.contains("1.5s"), "要按自己的起点算：{text}");
    }

    /// 新会话（还没任何 provider 上报）显示 `0/<预算> (0.0%)`，而不是 `0 calls`。
    #[test]
    fn status_line_shows_zero_usage_before_any_report() {
        let palette = Palette::mocha();
        let snap = Snapshot {
            model: "deepseek-flash".into(),
            cwd: "~/Projects/pie".into(),
            budget: 920_576,
            ..Default::default()
        };
        let text = line_text(&status_line(
            &palette,
            &snap,
            Activity::Idle,
            0,
            Instant::now(),
            100,
            None,
            true,
        ));
        assert!(text.contains("0/920,576 (0.0%)"), "{text}");
        assert!(!text.contains("calls"), "{text}");

        // 窗口没配（预算 0）→ 没有分母，整块不显示
        let no_budget = Snapshot {
            budget: 0,
            calls: 3,
            ..snap.clone()
        };
        let text = line_text(&status_line(
            &palette,
            &no_budget,
            Activity::Idle,
            0,
            Instant::now(),
            100,
            None,
            true,
        ));
        assert!(!text.contains("│"), "{text}");
        assert!(!text.contains("calls"), "{text}");
    }
}
