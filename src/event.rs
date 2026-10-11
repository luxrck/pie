//! 事件词汇表：**harness 说出去的话**（也是「要跨到 Python 的东西」的唯一事实来源）。
//!
//! 这里放两样东西：**事件词汇表**（层无关的纯数据）与**事件出口**（[`EventBus`]）。
//!
//! 词汇表 —— 从 `session` / `llm` / `context` 三处搬来的三类事件：
//!   - [`TurnEvent`]    语义：回合 → 消费者（TUI / CLI / 绑定）
//!   - [`ChunkEvent`]  传输增量：模型层内部（`stream()` 的增量，被翻成 `TurnEvent`）
//!   - [`CompactEvent`] 压缩记账：随会话落盘（`__meta__.compaction_events`）
//!
//! **故意不在这里的东西**：
//!   - `tui::UiEvent` —— 它引用 `LlmError` / `Balance` / `Snapshot` / `Status` / `files::Index`，
//!     搬进来会把 TUI 依赖拖进核心、破坏 `tui` feature 隔离；
//!   - `llm::Retry` —— 它是控制动词（重试怎么走），不是「发生了什么」；
//!   - `log::Kind` / `log::Notice` —— 带外诊断，是**进程内**的告警出口，不走事件总线。
//!
//! 各原模块用 `pub use crate::event::…` 再导出，所以调用点（`pie::session::TurnEvent`、
//! `crate::llm::ChunkEvent` …）路径不变。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- 语义（回合 → 消费者）

/// 回合过程中推给调用方的事件（TUI / CLI / 绑定 / 扩展宿主边跑边消费）。
///
/// **一份线上词汇表**：`#[serde(tag = "type", rename_all = "snake_case")]` 让变体名逐字变成
/// 线上名（`ContentDelta` → `content_delta`）—— 所以**没有** per-variant rename，
/// `Event::name()` 与 serde 的 `type` 自然一致（契约测试钉住）。
/// 订阅时用的那个名字就是 `type` 的值。`arguments` 是**原始 JSON 字符串**，由消费者自己解析摘要。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnEvent {
    /// 正文增量
    ContentDelta { text: String },
    /// 思考增量
    ReasoningDelta { text: String },
    /// 即将执行某个工具
    ToolCall { name: String, arguments: String },
    /// 工具执行完毕（按真实完成顺序推；`content` 是**原样**文本，不再截断——
    /// 少显示是展示层的事，见 `Session::tool_call`）
    ToolResult {
        name: String,
        /// 工具的输出正文（**原样**，不截断）——线上键名就是 `text`（与绑定那套 dict 一致）。
        text: String,
        arguments: String,
        /// 工具产出的**图像**（本地文件路径）——只给界面用（模型看不到）。
        images: Vec<PathBuf>,
    },
    /// 最终答复。
    ///
    /// **只在非流式（`aturn(stream = Some(false))`）时推**：流式下正文已经通过 `ContentDelta` 增量
    /// 推过了，再推一次消费者会重复显示。
    Answer { text: String },
}

impl TurnEvent {
    /// 线上名（= `type` 的值）。
    pub fn name(&self) -> &'static str {
        match self {
            TurnEvent::ContentDelta { .. } => "content_delta",
            TurnEvent::ReasoningDelta { .. } => "reasoning_delta",
            TurnEvent::ToolCall { .. } => "tool_call",
            TurnEvent::ToolResult { .. } => "tool_result",
            TurnEvent::Answer { .. } => "answer",
        }
    }
}

// ---------------------------------------------------------------- 传输增量（模型层内）

/// 流式增量。`done` 不在这里——最终结果由 `stream()` 直接返回。
///
/// ⚠ 变体名一律带 `Delta`：这里的每一条都是**片段**，不是成品。
/// 尤其是 `ToolCallDelta.arguments` 是「**本次收到的片段**」（要拼接），
/// 而 [`TurnEvent::ToolCall`] 的 `arguments` 是**完整 JSON 串**（直接能 `from_str`）——
/// 两个同名反义的字段曾经是个坑，靠名字分开。
#[derive(Clone, Debug)]
pub enum ChunkEvent {
    ReasoningDelta(String),
    ContentDelta(String),
    ToolCallDelta {
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

impl SessionEvent {
    /// 线上名（= `type` 的值）。
    pub fn name(&self) -> &'static str {
        match self {
            SessionEvent::TurnStart { .. } => "turn_start",
            SessionEvent::TurnDone { .. } => "turn_done",
            SessionEvent::Compacted { .. } => "compacted",
            SessionEvent::Cleared { .. } => "cleared",
            SessionEvent::ModelChanged { .. } => "model_changed",
            SessionEvent::CwdChanged { .. } => "cwd_changed",
            SessionEvent::Start { .. } => "start",
        }
    }
}

/// **对外观察的统一信封**：所有事件都从这一个出口出去。
///
/// `#[serde(untagged)]`：序列化时**摊平**成内层（`{"type":"tool_call",…}`）—— 与绑定那套 dict、
/// 与宿主协议同形（不会多一层 `{"type":"turn",…}`）。
/// 订阅时按 [`Event::name`] 过滤。
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Event {
    /// 回合内的事件（模型说话、工具活动）。
    Turn(TurnEvent),
    /// 会话生命周期。
    Session(SessionEvent),
}

impl Event {
    /// 所有事件名（= 线上 `type` = 订阅用的 kind）。唯一事实来源是下面 `name()` 里那些字面量
    /// —— 这里再列一遍是给**校验**用（订阅时拼错了就告警），契约测试钉住两者一致。
    pub const NAMES: [&'static str; 12] = [
        "content_delta",
        "reasoning_delta",
        "tool_call",
        "tool_result",
        "answer",
        "turn_start",
        "turn_done",
        "compacted",
        "cleared",
        "model_changed",
        "cwd_changed",
        "start",
    ];

    /// 事件名（= 序列化出来的 `type` = 订阅时用的那个 kind）。
    pub fn name(&self) -> &'static str {
        match self {
            Event::Turn(event) => event.name(),
            Event::Session(event) => event.name(),
        }
    }
}

/// 事件的**线上形状**——唯一出口。
///
/// serde 给规范形状（`#[serde(tag = "type", rename_all = "snake_case")]`），这里只补一处
/// Python 侧顺手的：`tool_call` / `tool_result` 的 `arguments` 给**解析后的对象**
/// （`tool_call` 另附 `arguments_raw` 原文）。**绑定的 dict 与宿主协议的事件帧都吃它** ——
/// 两个 Python 面因此逐字同形（插件里写的 `ev["arguments"]["command"]` 在绑定里也一样）。
pub fn to_wire(event: &Event) -> Result<serde_json::Value, serde_json::Error> {
    let mut value = serde_json::to_value(event)?;
    if let Some(map) = value.as_object_mut() {
        let kind = map
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let parsed = kind == "tool_call" || kind == "tool_result";
        if parsed
            && let Some(raw) = map
                .get("arguments")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        {
            map.insert("arguments".into(), arguments_value(&raw));
            if kind == "tool_call" {
                map.insert("arguments_raw".into(), serde_json::Value::String(raw));
            }
        }
    }
    Ok(value)
}

/// 工具参数是模型给的 JSON 字符串：能解析就用解析结果，否则退回 `{"raw": …}`（前 200 字）。
fn arguments_value(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| {
        let mut map = serde_json::Map::new();
        map.insert(
            "raw".to_string(),
            serde_json::Value::String(raw.chars().take(200).collect()),
        );
        serde_json::Value::Object(map)
    })
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

/// 事件出口：**发射器 + 订阅**。`Session` 全程持有它，任何位置（含 `aturn` 之外）都能 `send`。
///
/// **默认同步**（`non_blocking` 为 `None` / `Some(false)`）：就在调用方那条线程上按注册顺序逐个调监听者
/// ——所以：
///   - 要异步 / 跨线程的消费者，**自己在监听者里往 channel 一丢**（TUI 与绑定就是这么干的），
///     或直接把 `non_blocking` 给 `Some(true)`（那就走后台派发线程）；
///   - ⚠ 监听者**不该阻塞**（它跑在回合的那条线程上，阻塞会把回合拖慢）；
///   - ⚠ 监听者**不该碰同一个 `Session`**（`aturn` 正持有 `&mut Session`，重入要么借用冲突要么死锁）。
///
/// `Clone` 很便宜（内部一个 `Arc`）——`Session` 内部就靠它避开借用冲突：发之前先 `self.bus.clone()`。
#[derive(Clone, Default)]
pub struct EventBus {
    inner: Arc<BusInner>,
}

/// 一个监听者：`(id, 事件名过滤, 回调)`——id 供退订；过滤 `None` = 全部；
/// 回调按**注册顺序**被调用（见 [`EventBus::send`]）。
///
/// 回调返回 `Result<(), String>`：`Ok(())` = 通过；`Err(理由)` = **否决**——
/// 只对「动作前」的事件（现在只有 `tool_call`）有含义，见 [`EventBus::ask`]。
type Listener = (
    u64,
    Option<String>,
    Arc<dyn Fn(&Event) -> Result<(), String> + Send + Sync>,
);

#[derive(Default)]
struct BusInner {
    next_id: AtomicU64,
    /// 监听者表（同步派发时先快照再调，见 [`EventBus::dispatch`]）。
    listeners: RwLock<Vec<Listener>>,
    /// 异步派发的入口：**第一次 `send(.., Some(false))` 才起线程**（没人用异步就不起）。
    queue: Mutex<Option<std::sync::mpsc::Sender<Event>>>,
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
        listeners.retain(|(id, _, _)| *id != self.id);
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个监听者（可以有多个，按**注册顺序**收到事件）。返回的凭据 drop 即退订。
    /// 注册一个监听者（按**注册顺序**收到事件）。返回的凭据 drop 即退订。
    ///
    /// `name`：`"*"` = 全部；其余 = 只要这一类（值就是 [`Event::name`] / 线上 `type`）。
    /// 拼错了（不在 [`Event::NAMES`] 里）会告警——那一类永远不会有事件，多半是手误。
    ///
    /// `listener` 返回 `Ok(())` 通过 / `Err(理由)` 否决（只对「动作前」的事件有含义，见 [`EventBus::ask`]）；
    /// 纯观察者写 `Ok(())` 就行。
    pub fn on(
        &self,
        name: &str,
        listener: impl Fn(&Event) -> Result<(), String> + Send + Sync + 'static,
    ) -> Subscription {
        let filter = if name == "*" {
            None
        } else {
            if !Event::NAMES.contains(&name) {
                crate::log::warn(format!("事件名拼错了？订阅了一个不存在的事件：{name}"));
            }
            Some(name.to_string())
        };
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut listeners = self
            .inner
            .listeners
            .write()
            .unwrap_or_else(|e| e.into_inner());
        listeners.push((id, filter, Arc::new(listener)));
        Subscription {
            bus: Arc::downgrade(&self.inner),
            id,
        }
    }

    /// 扇出给所有监听者（**不等回话**）。要回话用 [`EventBus::ask`]。
    ///
    /// `non_blocking`：`None` / `Some(false)` = **同步**（默认：就在调用方这条线程上、按注册顺序
    /// 逐个调完才返回）；`Some(true)` = 丢进后台派发线程、立刻返回（观察者慢也拖不住回合）。
    ///
    /// ⚠ 两种模式混用时，同一个监听者**可能被并发调用**（同步那条在调用方线程、异步那条在派发
    /// 线程上），跨路径的相对顺序也不保证（各自路径内才是 FIFO）——监听者该自己保证可重入。
    pub fn send(&self, event: Event, non_blocking: Option<bool>) {
        if non_blocking.unwrap_or(false) {
            self.enqueue(event);
            return;
        }
        let denials = self.dispatch(&event);
        if !denials.is_empty() {
            // `send` 这条路是「陈述句」（回合边界、正文增量…），没有「动作前」的语义 ——
            // 监听者在这儿返回的理由没人能用，说一声，别让人以为拦住了什么。
            crate::log::warn(format!(
                "{} 事件上有订阅者给了否决理由，但这类事件没有「动作前」语义，已忽略：{}",
                event.name(),
                denials.join("；")
            ));
        }
    }

    /// 派发**并收集回话**：返回所有否决理由（空 = 通过），按**注册顺序**。
    ///
    /// 只有「动作前」的事件该走这条（现在只有 `tool_call`）——调用方拿理由去决定做不做。
    /// **不走异步派发**（要回话就不能丢后台），也不短路（理由要全，见 `docs/extension.md`）。
    pub fn ask(&self, event: &Event) -> Vec<String> {
        self.dispatch(event)
    }

    /// 同步派发（`send` 的默认路径；派发线程也走它）。返回所有否决理由。
    ///
    /// 先**快照**监听者表再调用：监听者在自己里面 `on` / drop 订阅时不会和这里抢锁
    /// （同线程持读锁再取写锁就是死锁）。
    fn dispatch(&self, event: &Event) -> Vec<String> {
        let name = event.name();
        let listeners: Vec<_> = self
            .inner
            .listeners
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, filter, _)| filter.as_deref().is_none_or(|want| want == name))
            .map(|(_, _, listener)| Arc::clone(listener))
            .collect();
        let mut denials = Vec::new();
        for listener in listeners {
            if let Err(reason) = listener(event) {
                denials.push(reason);
            }
        }
        denials
    }

    /// 异步派发：把事件交给后台线程（懒起、一个总线一个），**立刻返回**。
    ///
    /// 队列是**无界** `mpsc`：观察者慢只会让事件积压，既拖不住回合、也**不吃背压**。
    /// 「有界 + 满则丢 + 丢弃计数」等真接上远端观察者（`docs/extension.md` 的观察轴）再加。
    fn enqueue(&self, event: Event) {
        let mut slot = self.inner.queue.lock().unwrap_or_else(|e| e.into_inner());
        let sender = slot.get_or_insert_with(|| {
            let (tx, rx) = std::sync::mpsc::channel::<Event>();
            // 线程里只持 `Weak`：总线没人要了 → `Sender`（住在 `BusInner` 里）随之 drop
            // → `recv` 断开 → 线程自己退出，不会把 `BusInner` 吊着不死。
            let inner = Arc::downgrade(&self.inner);
            let _ = std::thread::Builder::new()
                .name("pie-events".to_string())
                .spawn(move || {
                    while let Ok(event) = rx.recv() {
                        let Some(inner) = inner.upgrade() else { break };
                        // 后台派发不问回话（没人等）→ 理由丢掉
                        EventBus { inner }.dispatch(&event);
                    }
                });
            tx
        });
        // 线程起不来 / 通道已断 → 直接丢：观察者少收一条不该影响回合。
        let _ = sender.send(event);
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
            subs.push(bus.on("*", {
                let got = Arc::clone(&got);
                move |e: &Event| {
                    if let Event::Session(SessionEvent::TurnStart { input }) = e {
                        got.lock().unwrap().push(format!("{tag}:{input}"));
                    }
                    Ok(())
                }
            }));
        }
        assert_eq!(bus.listener_count(), 2);

        bus.send(
            Event::Session(SessionEvent::TurnStart { input: "hi".into() }),
            None,
        );
        // 按**注册顺序**收到
        assert_eq!(got.lock().unwrap().as_slice(), ["a:hi", "b:hi"]);

        drop(subs.pop()); // 退订 b
        assert_eq!(bus.listener_count(), 1);
        bus.send(
            Event::Session(SessionEvent::TurnStart {
                input: "again".into(),
            }),
            None,
        );
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

    /// 监听者在自己里面 `on(..)` 不该死锁（`send` 先快照再调用，不持读锁去调）。
    #[test]
    fn subscribing_from_inside_a_listener_does_not_deadlock() {
        let bus = EventBus::new();
        let inside = bus.clone();
        // 新订阅得**留下来**（否则 `on` 的返回值一 drop 就退订了）
        let kept: Arc<Mutex<Vec<Subscription>>> = Arc::new(Mutex::new(Vec::new()));
        let _outer = bus.on("*", {
            let kept = Arc::clone(&kept);
            move |_| {
                kept.lock().unwrap().push(inside.on("*", |_| Ok(())));
                Ok(())
            }
        });
        bus.send(
            Event::Session(SessionEvent::Start {
                id: "x".into(),
                resume: false,
            }),
            None,
        );
        assert_eq!(bus.listener_count(), 2);
        assert_eq!(kept.lock().unwrap().len(), 1);
    }

    /// `Some(true)`：`send` 立刻返回，事件随后仍**按序**到达（后台派发线程里叫监听者）。
    #[test]
    fn non_blocking_send_delivers_off_thread_and_in_order() {
        let bus = EventBus::new();
        let got: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        let _sub = bus.on("*", {
            let got = Arc::clone(&got);
            move |e: &Event| {
                if let Event::Session(SessionEvent::Cleared { count, .. }) = e {
                    got.lock().unwrap().push(*count);
                }
                Ok(())
            }
        });
        for n in 0..64usize {
            bus.send(
                Event::Session(SessionEvent::Cleared {
                    archived: PathBuf::from("/tmp/w"),
                    count: n,
                }),
                Some(true),
            );
        }
        // 等后台线程追平（有上限，免得坏了就挂住）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while got.lock().unwrap().len() < 64 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let got = got.lock().unwrap().clone();
        assert_eq!(got, (0..64usize).collect::<Vec<_>>(), "少了或乱了：{got:?}");
    }

    /// 只订一个事件名：别的名字不进来（宿主 / 绑定靠这个关掉不需要的流量）。
    #[test]
    fn subscription_filters_by_name() {
        let bus = EventBus::new();
        let got: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let _sub = bus.on("tool_call", {
            let got = Arc::clone(&got);
            move |e: &Event| {
                got.lock().unwrap().push(e.name());
                Ok(())
            }
        });
        bus.send(
            Event::Turn(TurnEvent::ContentDelta { text: "x".into() }),
            None,
        );
        bus.send(
            Event::Turn(TurnEvent::ToolCall {
                name: "read".into(),
                arguments: "{}".into(),
            }),
            None,
        );
        assert_eq!(got.lock().unwrap().as_slice(), ["tool_call"]);
    }

    /// 契约：[`Event::name`] 必须与 serde 出来的 `type` **逐字一致**，且恰好覆盖 [`Event::NAMES`]。
    ///
    /// 词表只有一处定义（`name()` 里那些字面量）；这条是那张网——改名 / 加变体时忘了同步就会红。
    #[test]
    fn event_names_match_the_wire() {
        let samples = vec![
            Event::Turn(TurnEvent::ContentDelta { text: "x".into() }),
            Event::Turn(TurnEvent::ReasoningDelta { text: "x".into() }),
            Event::Turn(TurnEvent::ToolCall {
                name: "read".into(),
                arguments: "{}".into(),
            }),
            Event::Turn(TurnEvent::ToolResult {
                name: "read".into(),
                text: "ok".into(),
                arguments: "{}".into(),
                images: Vec::new(),
            }),
            Event::Turn(TurnEvent::Answer { text: "x".into() }),
            Event::Session(SessionEvent::TurnStart { input: "x".into() }),
            Event::Session(SessionEvent::TurnDone {
                answer: "x".into(),
                error: None,
                elapsed_ms: 1,
            }),
            Event::Session(SessionEvent::Compacted {
                level: 1,
                path: PathBuf::from("/tmp/c"),
                hash: "h".into(),
            }),
            Event::Session(SessionEvent::Cleared {
                archived: PathBuf::from("/tmp/w"),
                count: 1,
            }),
            Event::Session(SessionEvent::ModelChanged {
                previous: "a".into(),
                current: "b".into(),
            }),
            Event::Session(SessionEvent::CwdChanged {
                to: PathBuf::from("/tmp"),
            }),
            Event::Session(SessionEvent::Start {
                id: "s".into(),
                resume: false,
            }),
        ];
        let mut names: Vec<&str> = Vec::new();
        for ev in &samples {
            let wire = serde_json::to_value(ev).expect("序列化");
            assert_eq!(wire["type"], ev.name(), "{ev:?}");
            names.push(ev.name());
        }
        names.sort_unstable();
        names.dedup();
        let mut known = Event::NAMES.to_vec();
        known.sort_unstable();
        assert_eq!(names, known, "name() 与 Event::NAMES 要对得上，且一个不漏");
    }
}
