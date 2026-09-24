//! pie 的 Rust 重构 —— 核心库（agent harness：config / llm / tools / session / context）。
//!
//! 这一层是**给外部用的库**（CLI 二进制、将来的 Python 绑定都建立在它之上），所以：
//!   - 只放与交互无关的东西；**TUI 不在必选依赖里**（`tui` feature，默认开——CLI 要用）。
//!   - 绑定侧用 `default-features = false` 依赖本库 → 不会把 ratatui / crossterm / arboard 编进来。
//!
//! 模块职责（与 Python 版 `src/pie/*.py` 一一对应）：
//!   - [`config`]  —— 配置加载/保存 + 分层 system prompt + 全局记忆种子
//!   - [`llm`]     —— OpenAI 兼容客户端（reqwest + 手写 SSE，无 SDK）+ 重试 + Files API
//!   - [`tools`]   —— read / edit / write / shell（`@tool` 的 Rust 形态：结构体即参数）
//!   - [`session`] —— 会话 JSONL + resume + **回合循环**（`Session::aturn`，原 loop.rs 已并入）
//!   - [`context`] —— 三级压缩 + 落盘指针 + manifest + gc
//!   - [`cancel`]  —— 取消信号（TUI 的 Esc）
//!   - [`log`]     —— 告警出口（TUI 期间不能直接写 stderr）
//!
//! ⚠ 目前是**全 pub**（M0 阶段）：先把模块边界立起来、让 CLI 与绑定都能用，真实 API 面等
//! 绑定稳定后再收紧。TUI 与 CLI 参数结构**不属于**对外 API（`pie` 对外就是那个可执行文件）。

pub mod cancel;
pub mod config;
pub mod context;
pub mod llm;
pub mod log;
pub mod session;
pub mod tools;

/// TUI（ratatui + crossterm）。**不进绑定**：`default-features = false` 时整个模块不参与编译，
/// 相关的可选依赖（ratatui / crossterm / ratatui-textarea / tui-markdown / arboard / unicode-width）
/// 也就不会进依赖图。
#[cfg(feature = "tui")]
pub mod tui;
