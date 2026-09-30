//! TUI：ratatui + crossterm 的交互界面（**参考 codex 的形态**：消息流 + 底部输入区 + 状态栏）。
//!
//! 按 ratatui 的习惯来（每帧 `draw` 只写变化的单元格、内容随流式增量追加、滚动自己算偏移）。
//! 两点与「纯 ratatui 习惯」不同，都是为了复制：
//! **消息流自己折行**（`pane::layout`，而非 `Paragraph::wrap`）→ 知道每个显示行对应源文本的哪一段；
//! **鼠标左键拖动框选、松开即复制**（`App::on_mouse` + `Pane::slice_text`）——鼠标捕获为了滚轮
//! 一直开着，终端自己的选择就不能用了，所以得自己做。
//!
//! 模块划分：
//!   - `app`：状态机 + 事件循环（`select!`：终端事件 / 回合事件 / tick）
//!   - `pane`：消息流面板（用户 / 助手 / 思考耗时 / 工具 / 提示各一条 `Cell`）+ **折行与复制切片**
//!     （`Pane::layout` 增量重排，每块显示行 `Row` 自带 `indent` / `continues`；`Pane::slice_text` 按源文本切选区）
//!   - `repl`：REPL 画布（tab 切过去看 `repl` 工具的 code / 输出；折行复用 `pane::wrap_segments`）
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
pub mod pane;
pub mod repl;
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
    // 先按环境变量定好终端图像协议（**不读 stdin**，见 [`pick_terminal_protocol`]）。
    let picker = pick_terminal_protocol();
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste,
        // 窗口焦点（`Event::FocusGained/FocusLost`）：目前只用来让输入框光标在失焦时变暗
        crossterm::event::EnableFocusChange
    );

    let (app, rx) = app::App::new(session, max_steps, stream, picker);
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

/// 定终端图像协议（看环境变量）与**字符格的像素尺寸**（问终端）。
///
/// ⚠ **故意不用 `Picker::from_query_stdio()`**：它把「写查询序列 + 读终端回应」丢到一个
/// **独立线程**里做，超时（终端不回应时很正常）之后那个线程**杀不掉**，会留下来跟
/// crossterm 抢 stdin —— 多字节转义序列（如 `←`/`→`）会被它吃掉，方向键直接失效
/// （实测：接线后 `→` 切不了视图，而普通字符偶尔能进去）。所以协议只看环境变量。
///
/// 代价是协议只能看环境变量（认不出就退 `halfblocks`）——不值得为它冒险弄坏键盘；
/// 字符格像素则问终端（`window_size()`，同样是纯 `ioctl`）。
fn pick_terminal_protocol() -> ratatui_image::picker::Picker {
    use ratatui_image::FontSize;
    use ratatui_image::picker::{Picker, ProtocolType};

    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let term_program = env("TERM_PROGRAM");
    let protocol = if !env("KITTY_WINDOW_ID").is_empty()
        || term_program.contains("kitty")
        || term_program.contains("ghostty") // 它把 kitty 的 unicode placeholder 实现全了
    {
        ProtocolType::Kitty
    } else if term_program.contains("iTerm")
        || term_program.contains("WezTerm")
        || term_program.contains("Kaku") // Kaku 是 WezTerm 的 fork，内核同源
        || !env("WEZTERM_EXECUTABLE").is_empty() // …但它的 TERM_PROGRAM 不一定叫 WezTerm
        || term_program.contains("rio")
        || term_program.contains("vscode")
    {
        // WezTerm 系（含 Kaku）上**只有 iTerm2 是无 bug 的**（kitty 的 placeholder 不完整）
        ProtocolType::Iterm2
    } else {
        ProtocolType::Halfblocks
    };

    // 一个字符格多少像素：**必须问终端**，不能猜。
    //
    // iTerm2 协议是按**像素**把图发出去的（`1337;File=…;width=Npx;height=Mpx`），终端就照这个
    // 像素数 1:1 画——所以 `FontSize` 与终端的真实字符格差多少，图就按那个比例缩错多少。
    // `halfblocks()` 自带的 10×20 只是「常见比例」的猜测：本机 Kaku 的字符格是 20×58
    // （`ws_xpixel/ws_ypixel ÷ 列/行`），拿 10×20 算出来的图**只有应有尺寸的一半**，
    // 而且顶在图像区左上角 —— 看上去就是「图小小一张浮在画布中间、压着代码」。
    //
    // `window_size()` 只做 `TIOCGWINSZ`（不读 stdin，没有 [`from_query_stdio`] 那个抢 stdin 的毛病），
    // 它给的 xpixel/ypixel 正是终端画图用的那套像素。问不到就退回 10×20。
    let font_size = crossterm::terminal::window_size()
        .ok()
        .filter(|w| w.columns > 0 && w.rows > 0)
        .map(|w| FontSize::new(w.width / w.columns, w.height / w.rows))
        .filter(|f| f.width > 0 && f.height > 0)
        .unwrap_or(FontSize::new(10, 20));
    #[allow(deprecated)] // 自定 FontSize 的唯一入口（`from_query_stdio` 会读 stdin，见上）
    let mut picker = Picker::from_fontsize(font_size);
    picker.set_protocol_type(protocol);
    picker
}
