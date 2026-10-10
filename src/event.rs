//! 事件词汇表：**harness 说出去的话**（也是「要跨到 Python 的东西」的唯一事实来源）。
//!
//! 这里放两样东西：**事件词汇表**（层无关的纯数据）与**事件出口**（[`EventBus`]）。
//!
//! 词汇表 —— 从 `session` / `llm` / `context` 三处搬来的三类事件：
//!   - [`TurnEvent`]    语义：回合 → 消费者（TUI / CLI / 绑定）
//!   - [`StreamChunk`]  传输增量：模型层内部（`stream()` 的增量，被翻成 `TurnEvent`）
//!   - [`CompactEvent`] 压缩记账：随会话落盘（`__meta__.compaction_events`）
//!
//! **故意不在这里的东西**：
//!   - `tui::UiEvent` —— 它引用 `LlmError` / `Balance` / `Snapshot` / `Status` / `files::Index`，
//!     搬进来会把 TUI 依赖拖进核心、破坏 `tui` feature 隔离；
//!   - `llm::Retry` —— 它是控制动词（重试怎么走），不是「发生了什么」；
//!   - `log::Kind` / `log::Notice` —— 带外诊断，是**进程内**的告警出口，不走事件总线。
//!
//! 各原模块用 `pub use crate::event::…` 再导出，所以调用点（`pie::session::TurnEvent`、
//! `crate::llm::StreamChunk` …）路径不变。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- 语义（回合 → 消费者）

/// 回合过程中推给调用方的事件（TUI / CLI 边跑边渲染用）。
///
/// 事件形状与绑定的 `on_event` dict 一一对应（`{"type": …}`）；`arguments` 传**原始 JSON 字符串**，
/// 由消费者自己解析出「这次调的是哪个文件 / 命令」的摘要。
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// 正文增量
    AssistantText(String),
    /// 思考增量
    Reasoning(String),
    /// 即将执行某个工具
    ToolCall { name: String, arguments: String },
    /// 工具执行完毕（按真实完成顺序推；`content` 是**原样**文本，不再截断——
    /// 少显示是展示层的事，见 `Session::tool_call`）
    ToolResult {
        name: String,
        content: String,
        arguments: String,
        /// 工具产出的**图像**（本地文件路径）——只给界面用（模型看不到）。
        images: Vec<PathBuf>,
    },
    /// 最终答复。
    ///
    /// **只在非流式（`aturn(stream = Some(false))`）时推**：流式下正文已经通过 `AssistantText` 增量
    /// 推过了，再推一次消费者会重复显示。
    Answer(String),
}

// ---------------------------------------------------------------- 传输增量（模型层内）

/// 流式增量。`done` 不在这里——最终结果由 `stream()` 直接返回。
#[derive(Clone, Debug)]
pub enum StreamChunk {
    Reasoning(String),
    Content(String),
    ToolCall {
        index: usize,
        id: String,
        name: String,
        arguments: String,
    },
}

// ---------------------------------------------------------------- 压缩（记账，随会话落盘）

/// 一次压缩的记录（`Session.compaction_events` 的一条，随会话落盘）。
///
/// 类型由 variant 决定，落盘写成 `kind` 标签（`#[serde(tag = "kind")]`）——不再另存冗余的
/// `level`（要数字用 [`CompactEvent::level`]）；旧会话里多出来的 `level` 键被 serde 忽略，照读。
///
/// 字段名不带 `raw_` 前缀（`path` / `hash`）：事件本身已经说明这是压缩件。旧会话里叫
/// `raw_path` / `raw_hash` → `#[serde(alias)]` 兜住。
///
/// `hash` 取自**文件名**里的 hash：它总是内容寻址得到的那段，
/// 而 shell 自带落盘的内容是 stdout 原文——若拿结果文本重算就对不上了。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CompactEvent {
    /// 工具级（level 1）：工具输出落盘。`tool` 是触发落盘的工具名。
    Tool {
        ts: i64,
        tool: String,
        #[serde(alias = "raw_path")]
        path: PathBuf,
        #[serde(alias = "raw_hash")]
        hash: String,
    },
    /// 轮次级（level 2）：已完成的轮次压成摘要。
    Turn {
        ts: i64,
        #[serde(alias = "raw_path")]
        path: PathBuf,
        #[serde(alias = "raw_hash")]
        hash: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        summary: String,
    },
    /// 会话级（level 3）：整段历史落盘成窗口块。
    Session {
        ts: i64,
        #[serde(alias = "raw_path")]
        path: PathBuf,
        #[serde(alias = "raw_hash")]
        hash: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        summary: String,
    },
}

impl CompactEvent {
    /// 工具级事件（level 1）。
    pub fn tool(tool: &str, path: &Path) -> Self {
        Self::Tool {
            ts: crate::config::now().as_secs() as i64,
            tool: tool.to_string(),
            path: path.to_path_buf(),
            hash: crate::config::hash_of(path),
        }
    }

    /// 轮次级事件（level 2）。摘要只留前 200 字。
    pub fn turn(path: &Path, summary: &str) -> Self {
        Self::Turn {
            ts: crate::config::now().as_secs() as i64,
            path: path.to_path_buf(),
            hash: crate::config::hash_of(path),
            summary: summary.chars().take(200).collect(),
        }
    }

    /// 会话级事件（level 3）。摘要只留前 200 字。
    pub fn session(path: &Path, summary: &str) -> Self {
        Self::Session {
            ts: crate::config::now().as_secs() as i64,
            path: path.to_path_buf(),
            hash: crate::config::hash_of(path),
            summary: summary.chars().take(200).collect(),
        }
    }

    /// 压缩级别（1 工具级 / 2 轮次级 / 3 会话级，与 `Message.compaction` 的变体同口径）。
    pub fn level(&self) -> u8 {
        match self {
            Self::Tool { .. } => 1,
            Self::Turn { .. } => 2,
            Self::Session { .. } => 3,
        }
    }

    /// 落盘原文的路径。
    pub fn raw_path(&self) -> &Path {
        match self {
            Self::Tool { path, .. } | Self::Turn { path, .. } | Self::Session { path, .. } => path,
        }
    }

    /// 落盘件的 hash 段（文件名里的内容寻址 hash）。
    pub fn hash(&self) -> &str {
        match self {
            Self::Tool { hash, .. } | Self::Turn { hash, .. } | Self::Session { hash, .. } => hash,
        }
    }
}

// ---------------------------------------------------------------- 会话生命周期

/// 会话生命周期事件（`Session` 层）。
///
/// 与 [`TurnEvent`] 同属「说出去的话」，但描述的是**会话本身**：回合边界、压缩、窗口、模型 / 目录。
/// 自带 `#[serde(tag = "type")]` → 落成 `{"type": "turn_start", …}` 这种**扁平词表**，
/// 与绑定那套（`content_delta` / `tool_call` …）同形。
///
/// ⚠ 生产点现状：`aturn` 里发 `TurnStart` / `TurnDone` / `Compacted`；`Session::clear_window` 发
/// `Cleared`；`Session::set_model` 发 `ModelChanged`。**还没接的**：`CwdChanged`（`/cd` 改的是进程
/// 工作目录，触发点在 TUI 那层）、`Start`（发生在 `Session::new` / `Session::load` 的构造期 ——
/// 那时监听者还没注册上，发了也没人收，所以要发得由调用方在订阅之后补）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// 一个回合开始（`aturn` 入口，早于任何请求）。
    TurnStart { input: String },
    /// 一个回合结束：`answer` 是答复（取消时是取消文案）；`error` 非空 = 请求失败。
    TurnDone {
        answer: String,
        error: Option<String>,
        elapsed_ms: u64,
    },
    /// 发生了一次压缩（[`CompactEvent`] 的投影：只带级别 / 路径 / hash，不带记账细节）。
    Compacted {
        level: u8,
        path: PathBuf,
        hash: String,
    },
    /// 历史窗口被归档（`/clear`）。
    Cleared { archived: PathBuf, count: usize },
    /// 模型切换。
    ModelChanged { previous: String, current: String },
    /// 工作目录切换。
    CwdChanged { to: PathBuf },
    /// 会话开始（构造时就定了）：`resume` = 这是**从磁盘恢复**的会话（`Session::load`），
    /// 而不是新建的。
    Start { id: String, resume: bool },
}

/// **对外观察的统一信封**：所有事件都从这一个出口出去。
///
/// 现在只有 `Turn` / `Session` 两支；将来拆 `ToolEvent` 时加一支、把工具搬出 `TurnEvent`。
#[derive(Debug, Clone)]
pub enum Event {
    /// 回合内的事件（模型说话、工具活动）。
    Turn(TurnEvent),
    /// 会话生命周期。
    Session(SessionEvent),
}

impl From<TurnEvent> for Event {
    fn from(e: TurnEvent) -> Self {
        Self::Turn(e)
    }
}

impl From<SessionEvent> for Event {
    fn from(e: SessionEvent) -> Self {
        Self::Session(e)
    }
}

// ---------------------------------------------------------------- 事件出口（发射器）

/// 事件出口：**发射器 + 订阅**。`Session` 全程持有它，任何位置（含 `aturn` 之外）都能 `emit`。
///
/// `emit` 是**同步、内联**的（按注册顺序逐个调用监听者）——所以：
///   - 要异步 / 跨线程的消费者，**自己在监听者里往 channel 一丢**（TUI 与绑定就是这么干的）；
///   - ⚠ 监听者**不该阻塞**（它跑在回合的那条线程上，阻塞会把回合拖慢）；
///   - ⚠ 监听者**不该碰同一个 `Session`**（`aturn` 正持有 `&mut Session`，重入要么借用冲突要么死锁）。
///
/// `Clone` 很便宜（内部一个 `Arc`）——`Session` 内部就靠它避开借用冲突：发之前先 `self.bus.clone()`。
#[derive(Clone, Default)]
pub struct EventBus {
    inner: Arc<BusInner>,
}

/// 一个监听者：`(id, 回调)`——id 供退订，回调按**注册顺序**被同步调用（见 [`EventBus::emit`]）。
type Listener = (u64, Arc<dyn Fn(&Event) + Send + Sync>);

#[derive(Default)]
struct BusInner {
    next_id: AtomicU64,
    listeners: RwLock<Vec<Listener>>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("listeners", &self.listener_count())
            .finish()
    }
}

/// 一次订阅的凭据：**drop 掉即退订**。
pub struct Subscription {
    bus: Weak<BusInner>,
    id: u64,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let Some(bus) = self.bus.upgrade() else {
            return; // 总线已经没人持有了，本来就该一起没
        };
        let mut listeners = bus.listeners.write().unwrap_or_else(|e| e.into_inner());
        listeners.retain(|(id, _)| *id != self.id);
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个监听者（可以有多个，按**注册顺序**收到事件）。返回的凭据 drop 即退订。
    pub fn on(&self, listener: impl Fn(&Event) + Send + Sync + 'static) -> Subscription {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut listeners = self
            .inner
            .listeners
            .write()
            .unwrap_or_else(|e| e.into_inner());
        listeners.push((id, Arc::new(listener)));
        Subscription {
            bus: Arc::downgrade(&self.inner),
            id,
        }
    }

    /// 扇出给所有监听者。**不阻塞、无返回**（要结果就不该走事件）。
    ///
    /// 先**快照**监听者表再调用：监听者在自己里面 `on` / drop 订阅时不会和这里抢锁
    /// （同线程持读锁再取写锁就是死锁）。
    pub fn emit(&self, event: Event) {
        let listeners: Vec<_> = self
            .inner
            .listeners
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, listener)| Arc::clone(listener))
            .collect();
        for listener in listeners {
            listener(&event);
        }
    }

    /// 当前监听者数量（测试 / 诊断）。
    pub fn listener_count(&self) -> usize {
        self.inner
            .listeners
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// 扇出：多个监听者都收到；`Subscription` drop 掉就退订。
    #[test]
    fn bus_fans_out_and_drops_the_subscription() {
        let bus = EventBus::new();
        let got: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut subs = Vec::new();
        for tag in ["a", "b"] {
            subs.push(bus.on({
                let got = Arc::clone(&got);
                move |e: &Event| {
                    if let Event::Session(SessionEvent::TurnStart { input }) = e {
                        got.lock().unwrap().push(format!("{tag}:{input}"));
                    }
                }
            }));
        }
        assert_eq!(bus.listener_count(), 2);

        bus.emit(Event::Session(SessionEvent::TurnStart {
            input: "hi".into(),
        }));
        // 按**注册顺序**收到
        assert_eq!(got.lock().unwrap().as_slice(), ["a:hi", "b:hi"]);

        drop(subs.pop()); // 退订 b
        assert_eq!(bus.listener_count(), 1);
        bus.emit(Event::Session(SessionEvent::TurnStart {
            input: "again".into(),
        }));
        assert_eq!(got.lock().unwrap().len(), 3, "{:?}", got.lock().unwrap());
    }

    /// 会话事件的**线上形状**（绑定的 dict 键名就靠它 —— `#[serde(tag = "type")]` 那一套）。
    #[test]
    fn session_events_serialize_with_a_type_tag() {
        let start = serde_json::to_value(SessionEvent::Start {
            id: "s1".into(),
            resume: true,
        })
        .unwrap();
        assert_eq!(start["type"], "start");
        assert_eq!(start["id"], "s1");
        assert_eq!(start["resume"], true);

        let cleared = serde_json::to_value(SessionEvent::Cleared {
            archived: PathBuf::from("/tmp/w"),
            count: 3,
        })
        .unwrap();
        assert_eq!(cleared["type"], "cleared");
        assert_eq!(cleared["count"], 3);
    }

    /// 监听者在自己里面 `on(..)` 不该死锁（`emit` 先快照再调用，不持读锁去调）。
    #[test]
    fn subscribing_from_inside_a_listener_does_not_deadlock() {
        let bus = EventBus::new();
        let inside = bus.clone();
        // 新订阅得**留下来**（否则 `on` 的返回值一 drop 就退订了）
        let kept: Arc<Mutex<Vec<Subscription>>> = Arc::new(Mutex::new(Vec::new()));
        let _outer = bus.on({
            let kept = Arc::clone(&kept);
            move |_| {
                kept.lock().unwrap().push(inside.on(|_| {}));
            }
        });
        bus.emit(Event::Session(SessionEvent::Start {
            id: "x".into(),
            resume: false,
        }));
        assert_eq!(bus.listener_count(), 2);
        assert_eq!(kept.lock().unwrap().len(), 1);
    }
}
