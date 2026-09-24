//! TUI：ratatui + crossterm 的交互界面（**参考 codex 的形态**：消息流 + 底部输入区 + 状态栏）。
//!
//! 与 Python 版 Textual 实现**不追求逐项对齐**：这边按 ratatui 的习惯来（每帧 `draw` 只写变化的
//! 单元格、内容随流式增量追加、滚动自己算偏移）。两点与「纯 ratatui 习惯」不同，都是为了复制：
//! **消息流自己折行**（`history::layout`，而非 `Paragraph::wrap`）→ 知道每个显示行对应源文本的哪一段；
//! **鼠标左键拖动框选、松开即复制**（`App::on_mouse` + `Layout::slice_text`）——鼠标捕获为了滚轮
//! 一直开着，终端自己的选择就不能用了，所以得自己做。
//!
//! 模块划分（对应 Python 的 `tui.py` / `theme.py` / `textkit.py` / `clipboard.py`）：
//!   - `app`：状态机 + 事件循环（`select!`：终端事件 / 回合事件 / tick）
//!   - `history`：消息流单元格（用户 / 助手 / 思考耗时 / 工具 / 提示）+ **折行与复制切片**
//!     （`layout` 返回带逻辑行号的 `Row`，`Layout::slice_text` 按源文本切选区）
//!   - `markdown`：markdown → `Text`（按单元格缓存，只重解析变化的那条）
//!   - `status`：状态栏（模型、上下文占用、**思考计时**/spinner）与键位提示文案
//!   - `palette`：`/` 命令补全候选表 + 匹配 + 面板渲染
//!   - `files`：`@` 文件路径补全（cwd 索引 + 匹配；索引由 `app` 在后台建）
//!   - `input`：输入框（`ratatui-textarea` + 历史 + 粘贴）
//!   - `theme`：语义色板 / 图标 / spinner 帧
//!   - `clipboard`：`arboard` —— 粘贴（图片 → 位图落本地副本 / 文件引用取原路径，插入路径）
//!     与**写剪贴板**（消息流框选复制）

pub mod app;
pub mod clipboard;
pub mod files;
pub mod history;
pub mod input;
pub mod markdown;
pub mod palette;
pub mod status;
pub mod theme;

use crate::session::Session;

/// 跑 TUI（`pie` 在 TTY 下不带任务时走这里）。
///
/// `max_steps` / `stream` 是 CLI 传下来的**按次**执行旋钮，原样转给每次 `Session::aturn`
/// （见它的文档）；`None` = 默认（不限步数 / 流式）。
///
/// 终端初始化/恢复用 ratatui 的 `init()` / `restore()`（它还顺带装了 panic hook，
/// 崩了也不会把终端留在 raw 模式）；鼠标捕获、**bracketed paste** 与**窗口焦点上报**要自己开，
/// 退出前一定关掉。
///
/// ⚠ bracketed paste 不能省：不开的话终端不会用 `\x1b[200~` 包住粘贴内容，多行粘贴会被拆成
/// 一个个按键（换行 = 回车）→ 粘一段多行文本会在第一行就发出去。
pub async fn run(session: Session, max_steps: Option<usize>, stream: Option<bool>) -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste,
        // 窗口焦点（`Event::FocusGained/FocusLost`）：目前只用来让输入框光标在失焦时变暗
        crossterm::event::EnableFocusChange
    );

    let (app, rx) = app::App::new(session, max_steps, stream);
    let outcome = app.run(&mut terminal, rx).await;

    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableFocusChange
    );
    ratatui::restore();
    // 退出时的提示（保存失败…）留到终端恢复后再写
    match outcome {
        Ok(warning) => {
            if let Some(warning) = warning {
                eprintln!("{warning}");
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}
