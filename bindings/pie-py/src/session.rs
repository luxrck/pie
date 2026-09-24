//! `Session` / `Cancel`：会话与回合循环。
//!
//! # 回合怎么跑（这是整个绑定最要紧的一段）
//!
//! `Session::aturn` 是 async 且要推事件。为了**不在 tokio 线程里跑 Python 代码**（那需要跨线程
//! attach GIL、回调抛异常还要跨 FFI 边界处理），这里：
//!
//! 1. 把整个回合 `spawn` 到进程级 runtime 上，`on_event` 只往 `mpsc` 推 `TurnEvent`；
//! 2. **调用线程**（持有 GIL）循环 `py.detach(|| rx.recv())` —— 等事件时释放 GIL，
//!    拿到事件后回 GIL 调用户回调；
//! 3. 所有 sender drop（回合结束）→ `recv` 返回 `Err` → 收尾 `block_on(handle)` 取最终答复。
//!
//! 代价与约定：
//!   - 会话被回合**独占**（`tokio::sync::Mutex`）：回合期间别的调用拿到 `RuntimeError("session 正忙")`，
//!     而不是排队等死锁 —— 尤其**回调里不要碰同一个 Session**。
//!   - 回调抛异常 → 记下来，等回合跑完再抛回去（不会把回合打断在半路，历史也不会写坏）。
//!   - 想中途停：别的线程调 `s.stop()`，或外部持一个 `Cancel` 传进来。

use std::path::Path;
use std::sync::mpsc;
use std::sync::Mutex as StdMutex;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use serde_json::Value;
use tokio::sync::Mutex as TokioMutex;

use pie::cancel::Cancel;
use pie::context::CompactMode;
use pie::session::{Session as CoreSession, TurnEvent};

use crate::config::PyConfig;
use crate::llm::PyLlmClient;
use crate::tools::PyToolRegistry;
use crate::{busy_error, llm_error, pie_error};

// ---------------------------------------------------------------- Cancel

/// 取消信号：给 `aturn(cancel=...)` 用，或从别的线程停住一个正在跑的回合。
#[pyclass(name = "Cancel", module = "pie")]
pub struct PyCancel(pub(crate) Cancel);

#[pymethods]
impl PyCancel {
    #[new]
    fn new() -> Self {
        Self(Cancel::new())
    }

    /// 请求停止（幂等）：模型请求会断开、shell 会杀掉整个进程组。
    fn cancel(&self) {
        self.0.cancel();
    }

    #[getter]
    fn cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    fn __repr__(&self) -> String {
        format!("<pie.Cancel cancelled={}>", self.0.is_cancelled())
    }
}

// ---------------------------------------------------------------- Session

#[pyclass(name = "Session", module = "pie")]
pub struct PySession {
    inner: std::sync::Arc<TokioMutex<CoreSession>>,
    /// 会话文件路径（构造后就固定；单拎出来是为了不为了读个路径去抢锁）。
    path: String,
    /// 当前回合的取消信号 —— `stop()` 靠它中断（回合结束清空）。
    current: StdMutex<Option<Cancel>>,
    /// `asyncio` 入口登记的事件队列（`aturn_async` 用；`events()` 读它）。
    #[cfg(feature = "asyncio")]
    events_queue: StdMutex<Option<Py<PyAny>>>,
}

impl PySession {
    pub(crate) fn wrap(session: CoreSession) -> Self {
        Self {
            path: session.path.display().to_string(),
            inner: std::sync::Arc::new(TokioMutex::new(session)),
            current: StdMutex::new(None),
            #[cfg(feature = "asyncio")]
            events_queue: StdMutex::new(None),
        }
    }

    /// 拿会话锁：**忙就报错，不排队**（排队会在「回调里调同一个 session」时死锁）。
    fn lock(&self) -> PyResult<tokio::sync::MutexGuard<'_, CoreSession>> {
        self.inner.try_lock().map_err(|_| busy_error())
    }

    fn build(
        config: &PyConfig,
        llm: &PyLlmClient,
        tools: &PyToolRegistry,
    ) -> (pie::config::Config, pie::llm::LlmClient, pie::tools::ToolRegistry) {
        (config.inner.clone(), llm.inner.clone(), tools.inner.clone())
    }
}

#[pymethods]
impl PySession {
    /// 新建会话：`id` 给纯名字 → `~/.pie/sessions/<id>.jsonl`（带目录 / 绝对路径则原样）。
    #[staticmethod]
    #[pyo3(signature = (config, llm, tools, id=None))]
    fn new(
        config: &PyConfig,
        llm: &PyLlmClient,
        tools: &PyToolRegistry,
        id: Option<&str>,
    ) -> Self {
        let (core_config, llm, tools) = Self::build(config, llm, tools);
        Self::wrap(CoreSession::new(&core_config, id, llm, tools))
    }

    /// 临时会话：**不落盘、不写压缩 manifest**（一次性任务 / notebook / 服务用）。
    #[staticmethod]
    fn ephemeral(config: &PyConfig, llm: &PyLlmClient, tools: &PyToolRegistry) -> Self {
        let (core_config, llm, tools) = Self::build(config, llm, tools);
        Self::wrap(CoreSession::ephemeral(&core_config, llm, tools))
    }

    /// 从 JSONL 恢复（`path` 必须存在）。
    #[staticmethod]
    fn load(
        path: &str,
        config: &PyConfig,
        llm: &PyLlmClient,
        tools: &PyToolRegistry,
    ) -> PyResult<Self> {
        let (core_config, llm, tools) = Self::build(config, llm, tools);
        CoreSession::load(Path::new(path), &core_config, llm, tools)
            .map(Self::wrap)
            .map_err(pie_error)
    }

    /// 恢复最近的会话（同工作目录优先）——与 CLI 的 `-r` 同款。
    #[staticmethod]
    fn resume(config: &PyConfig, llm: &PyLlmClient, tools: &PyToolRegistry) -> PyResult<Self> {
        let (core_config, llm, tools) = Self::build(config, llm, tools);
        CoreSession::resume(&core_config, llm, tools)
            .map(Self::wrap)
            .map_err(pie_error)
    }

    // ------------------------------------------------------------ 读

    #[getter]
    fn path(&self) -> &str {
        &self.path
    }

    /// 历史（**dict 列表**，字段名与 JSONL / Python 版 `to_dict()` 一致，含压缩元数据）。
    #[getter]
    fn messages(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let guard = self.lock()?;
        crate::to_py(py, &guard.messages)
    }

    /// 完整转录：把压缩指针（工具级落盘全文、轮次级/会话级摘要）展开成原始消息。
    #[getter]
    fn full_history(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let guard = self.lock()?;
        crate::to_py(py, &guard.full_history())
    }

    /// 用量（`prompt_tokens` / `completion_tokens` / `calls` …；token 是最近一次上报值，`calls` 累加）。
    #[getter]
    fn usage(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let guard = self.lock()?;
        crate::to_py(py, &guard.usage)
    }

    #[getter]
    fn title(&self) -> PyResult<Option<String>> {
        Ok(self.lock()?.title.clone())
    }

    #[getter]
    fn turn_count(&self) -> PyResult<usize> {
        Ok(self.lock()?.turn_count)
    }

    /// 一行摘要（`n 轮历史，m 次请求`）。
    fn summary(&self) -> PyResult<String> {
        Ok(self.lock()?.summary())
    }

    /// `/stat` 那段文本报告。
    fn usage_report(&self) -> PyResult<String> {
        Ok(self.lock()?.usage_report())
    }

    /// `/clear`：把当前窗口归档成**窗口块**（`~/.pie/windows/`）后开新窗口，
    /// 返回手上的窗口块总数。历史不丢——原文留在块里，可经指针回查（`full_history` 能展开）。
    fn clear_window(&self) -> PyResult<usize> {
        self.lock()?.clear_window().map_err(pie_error)
    }

    /// 当前配置的**快照**（`pie.Config`）。
    ///
    /// ⚠ 是副本：改它**不影响**会话（会话持有自己那份）。要改会话的配置用
    /// `set_model()` / `set_reasoning_effort()`，或建会话时把配置传进去。
    #[getter]
    fn config(&self) -> PyResult<crate::config::PyConfig> {
        Ok(crate::config::PyConfig {
            inner: self.lock()?.config.clone(),
        })
    }

    /// 切模型：改配置 + 同步客户端实例 + **写回配置文件**；返回一句提示（与 Python `set_model` 同款）。
    ///
    /// ⚠ 会写盘（写到 `Config.config_file`，没设就落 `~/.pie/config.toml`）。
    fn set_model(&self, name: &str) -> PyResult<String> {
        Ok(self.lock()?.set_model(name))
    }

    /// 切思考深度（`level` 与 `Config.reasoning_effort` 同口径：`none`/`low`/`high`/`max`…）；
    /// 同样会写回配置文件。返回一句提示。
    fn set_reasoning_effort(&self, level: &str) -> PyResult<String> {
        Ok(self.lock()?.set_reasoning_effort(level))
    }

    /// 追加一条 **assistant** 消息（纯文本、无工具调用）——给嵌入方补历史用。
    ///
    /// 例：模型这一轮什么都没改，就在历史里补一条 assistant 再跑一个 `aturn`（user 提醒），
    /// 让对话读起来是一次真实往来。
    fn push_assistant(&self, text: &str) -> PyResult<()> {
        self.lock()?.push_assistant(text);
        Ok(())
    }

    /// **API 形状**的消息（去掉压缩元数据）——存档 / 喂给别的模型用；
    /// `messages` 给的是原始 dict（含 `compress_level` / `raw_path` 那些）。
    #[getter]
    fn api_messages(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let guard = self.lock()?;
        let values: Vec<Value> = guard.messages.iter().map(|m| m.to_api()).collect();
        crate::json_to_py(py, &Value::Array(values))
    }

    /// 压缩历史（manifest 里的每条事件，dict 列表）。
    fn compression_history(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let guard = self.lock()?;
        crate::to_py(py, &guard.compression_history())
    }

    // ------------------------------------------------------------ 写

    /// 落盘（`~/.pie/sessions/<名字>.jsonl`，与 CLI / Python 版同格式）。
    fn save(&self) -> PyResult<()> {
        self.lock()?.save().map_err(pie_error)
    }

    /// 清空历史（保留会话文件本身）。
    fn reset(&self) -> PyResult<()> {
        self.lock()?.reset();
        Ok(())
    }

    /// 手动压缩：`mode` ∈ `auto`（工具级+轮次级）/ `tools` / `turns`。返回统计 dict。
    #[pyo3(signature = (mode="auto"))]
    fn compact(&self, py: Python<'_>, mode: &str) -> PyResult<Py<PyAny>> {
        let mode = match mode {
            "auto" => CompactMode::Auto,
            "tools" => CompactMode::Tools,
            "turns" => CompactMode::Turns,
            other => {
                return Err(PyValueError::new_err(format!(
                    "未知压缩模式: {other}（可用 auto / tools / turns）"
                )))
            }
        };
        let mut guard = self.lock()?;
        let stats = guard.compact(mode);
        // `CompactStats` 没实现 Serialize（核心侧只内部用）→ 在这儿手工拼 dict。
        let dict = PyDict::new(py);
        dict.set_item("saved_tokens", stats.saved_tokens)?;
        dict.set_item("turns", stats.turns)?;
        dict.set_item("tools", stats.tools)?;
        dict.set_item("session", stats.session)?;
        dict.set_item("skipped", stats.skipped)?;
        Ok(dict.into_any().unbind())
    }

    /// 请求停止当前回合（别的线程调也行）。返回是否真的发出了取消。
    fn stop(&self) -> bool {
        let token = self.current.lock().ok().and_then(|slot| slot.clone());
        match token {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }

    /// 跑一个完整回合，返回最终答复。**阻塞**到回合结束（模型请求期间不持 GIL）。
    ///
    /// `on_event(event: dict)` 边跑边收事件（`content_delta` / `reasoning_delta` / `tool_call` /
    /// `tool_result` / `answer`，形状与纯 Python 版 `loop.aturn` 的 `on_event` 一致）；
    /// 事件在**调用线程**上回调（所以回调里能安全地 print / 更新自己的状态）。
    ///
    /// `max_steps=None` = 不限步数；`stream=None` = 默认流式（`False` 则一次性 complete，
    /// 只推一次 `answer`）；`parallel_tools=None` = 跟随 `Config.parallel_tools`（同一批
    /// tool_calls 是否并发执行）。三个都与纯 Python 版 `loop.aturn` 的同名形参同义（**不是**配置项）。
    ///
    /// 模型请求失败（外部原因）抛 `pie.LlmError`，但**历史里已经补了一条 `[请求失败] <错误>` 的
    /// assistant 消息**（不让那条 user 成为没人应答的提问）；取消则返回 `用户手动终止`。
    #[pyo3(signature = (input, on_event=None, cancel=None, max_steps=None, stream=None, parallel_tools=None))]
    pub(crate) fn aturn(
        &self,
        py: Python<'_>,
        input: String,
        on_event: Option<Py<PyAny>>,
        cancel: Option<PyRef<'_, PyCancel>>,
        max_steps: Option<usize>,
        stream: Option<bool>,
        parallel_tools: Option<bool>,
    ) -> PyResult<String> {
        // ① 独占会话：拿不到锁立刻报错（别排队等）
        let guard = self.inner.clone().try_lock_owned().map_err(|_| busy_error())?;

        // ② 取消信号：外部给了就用它，否则自己造一个并登记，供 `s.stop()` 用
        let token = cancel.map(|c| c.0.clone()).unwrap_or_else(Cancel::new);
        if let Ok(mut slot) = self.current.lock() {
            *slot = Some(token.clone());
        }

        // ③ 回合扔到 runtime 上跑，事件只进 channel（tokio 线程里不碰 Python）
        let (tx, rx) = mpsc::channel::<TurnEvent>();
        let handle = crate::runtime().spawn(async move {
            let mut session = guard;
            let result = {
                let mut emit = move |event: TurnEvent| {
                    // 接收端没了（调用方已放弃）就静默丢弃，别把回合搞崩
                    let _ = tx.send(event);
                };
                session
                    .aturn(&input, &mut emit, &token, max_steps, stream, parallel_tools)
                    .await
            };
            result
        });

        // ④ 调用线程 pump：等事件时释放 GIL，拿到事件回 GIL 调回调
        //
        // ⚠ `rx` 只能**按值**进闭包再带出来：`py.detach` 的闭包要 `Send`，而
        // `mpsc::Receiver` 是 `Send + !Sync` —— 按引用捕获就需要 `Sync`，编译不过。
        let mut rx = rx;
        let mut callback_error: Option<PyErr> = None;
        loop {
            let (next_rx, event) = py.detach(move || {
                let event = rx.recv();
                (rx, event)
            });
            rx = next_rx;
            let Ok(event) = event else {
                break; // 所有 sender 已 drop = 回合结束
            };
            let Some(callback) = &on_event else { continue };
            match event_to_py(py, &event).and_then(|obj| callback.call1(py, (obj,))) {
                Ok(_) => {}
                Err(e) => {
                    // 回调自己抛了：记账后继续把回合收完（半路丢掉会写坏历史），最后再抛
                    callback_error = Some(e);
                    break;
                }
            }
        }
        let outcome = py.detach(|| crate::runtime().block_on(handle));
        if let Ok(mut slot) = self.current.lock() {
            *slot = None;
        }

        if let Some(e) = callback_error {
            return Err(e);
        }
        match outcome {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(e)) => Err(llm_error(py, e)),
            Err(join) => Err(pie_error(format!("回合任务异常退出: {join}"))),
        }
    }

    // ------------------------------------------------------------ asyncio（M5）
    //
    // 形状：`aturn_async()` 返回一个**可 await** 的对象，事件走 `async for ev in s.events()`。
    // 三处与同步版不同（规划 §5.2 的三个坑），都由下面这套接口兜住：
    //   ① 事件 → `asyncio.Queue`（从 tokio 线程 `loop.call_soon_threadsafe(queue.put_nowait, ev)`
    //      —— 这一步由 Python 侧组的 `sink` 干，本模块只负责「在 tokio 线程上叫它」）；
    //   ② `task.cancel()` → Python 侧那层 glue 捕获 `CancelledError` 后调 `s.stop()`
    //      （见 `pie/_async.py`；tokio 任务不会因为 Python 侧取消而自己停）；
    //   ③ runtime 用 `pyo3_async_runtimes::tokio` 的**进程级** runtime，不随 loop 生死。

    /// 异步回合的底层口：把回合扔到 runtime 上跑，返回可 await 的对象。
    ///
    /// `sink(payload)` 在 **tokio 线程**上被调用（持 GIL）：事件是 dict，回合结束收到 `None`。
    /// ⚠ 它必须只做「把东西丢给 asyncio」（`pie/_async.py` 里就是 `call_soon_threadsafe` +
    /// `queue.put_nowait`）—— **别碰 Session**（回合正占着锁）。
    #[cfg(feature = "asyncio")]
    #[pyo3(signature = (input, sink, cancel=None, max_steps=None, stream=None, parallel_tools=None))]
    fn turn_future(
        &self,
        py: Python<'_>,
        input: String,
        sink: Py<PyAny>,
        cancel: Option<Py<PyAny>>,
        max_steps: Option<usize>,
        stream: Option<bool>,
        parallel_tools: Option<bool>,
    ) -> PyResult<Py<PyAny>> {
        // ① 独占会话 + 取消信号（与同步 `aturn` 同一套）
        let guard = self.inner.clone().try_lock_owned().map_err(|_| busy_error())?;
        let token = match &cancel {
            Some(obj) => obj
                .bind(py)
                .extract::<PyRef<'_, PyCancel>>()
                .map_err(|_| pie_error("cancel= 只接受 pie.Cancel（或 None）"))?
                .0
                .clone(),
            None => Cancel::new(),
        };
        if let Ok(mut slot) = self.current.lock() {
            *slot = Some(token.clone());
        }

        let sink_for_events = sink.clone_ref(py);
        let sink_end = sink;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut session = guard;
            let mut emit = move |event: TurnEvent| {
                // 事件：只会「往 asyncio 队列里丢」，失败就当没发生（别把回合弄崩）
                let _ = Python::attach(|py| -> PyResult<()> {
                    let obj = event_to_py(py, &event)?;
                    sink_for_events.bind(py).call1((obj,))?;
                    Ok(())
                });
            };
            let result = session
                .aturn(&input, &mut emit, &token, max_steps, stream, parallel_tools)
                .await;
            // 收尾：告诉 Python 侧「事件到头了」——`events()` 的迭代器据此 StopAsyncIteration
            let _ = Python::attach(|py| -> PyResult<()> {
                sink_end.bind(py).call1((py.None(),))?;
                Ok(())
            });
            result.map_err(|e| Python::attach(|py| llm_error(py, e)))
        })
        .map(|obj| obj.unbind())
    }

    /// 登记事件队列（`aturn_async` 自己调；`events()` 读它）。
    #[cfg(feature = "asyncio")]
    fn _set_events_queue(&self, queue: Py<PyAny>) {
        self.store_events_queue(queue);
    }

    /// `async for ev in session.events()` 用的（内部就是 `pie._async._Events(queue)`）。
    ///
    /// 没跑过 `aturn_async` 就报错——事件流是**按回合**的，没有常驻队列。
    #[cfg(feature = "asyncio")]
    fn events(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let queue = self
            .events_queue
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|q| q.clone_ref(py)))
            .ok_or_else(|| {
                busy_error_or(
                    "还没有事件队列：先调 session.aturn_async(...)（事件流是按回合的）",
                )
            })?;
        let helper = py.import("pie._async")?.getattr("_Events")?;
        Ok(helper.call1((queue,))?.unbind())
    }

    /// `await` 版回合（M5）：返回一个可 await 的对象，返回值同 `aturn`。
    ///
    /// 事件走 `async for ev in session.events()`；`task.cancel()` / `session.stop()` 都能真停住
    /// （前者靠 Python 侧的 glue 把 `CancelledError` 桥到 `stop()`）。
    #[cfg(feature = "asyncio")]
    #[pyo3(signature = (input, cancel=None, max_steps=None, stream=None, parallel_tools=None))]
    fn aturn_async(
        slf: PyRef<'_, Self>,
        py: Python<'_>,
        input: String,
        cancel: Option<Py<PyAny>>,
        max_steps: Option<usize>,
        stream: Option<bool>,
        parallel_tools: Option<bool>,
    ) -> PyResult<Py<PyAny>> {
        // 队列**在这里就建好并登记**：这样 `s.aturn_async(...)` 返回后 `s.events()` 立刻能用
        //（不用先 await 一下让协程跑起来）。胶水（哨兵 / CancelledError → stop）在 Python 侧。
        let asyncio = py.import("asyncio")?;
        let queue = asyncio.getattr("Queue")?.call0()?;
        slf.store_events_queue(queue.clone().unbind());

        let helper = py.import("pie._async")?.getattr("aturn_async")?;
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item("queue", queue)?;
        kwargs.set_item("cancel", cancel)?;
        kwargs.set_item("max_steps", max_steps)?;
        kwargs.set_item("stream", stream)?;
        kwargs.set_item("parallel_tools", parallel_tools)?;
        Ok(helper.call((slf, input), Some(&kwargs))?.unbind())
    }

    fn __repr__(&self) -> String {
        format!("<pie.Session path={} busy={}>", self.path, self.inner.try_lock().is_err())
    }
}

/// Rust 侧内部：把事件队列塞进槽位（`_set_events_queue` 与 `aturn_async` 共用）。
///
/// ⚠ 放在 `#[pymethods]` **外面**：它不该是 Python 方法（暴露出去只会污染接口面，
/// 存根对拍用例也会红）。
#[cfg(feature = "asyncio")]
impl PySession {
    fn store_events_queue(&self, queue: Py<PyAny>) {
        if let Ok(mut slot) = self.events_queue.lock() {
            *slot = Some(queue);
        }
    }
}

/// 通用一点的报错（`events()` 之类「接口用错」的场景）。
#[cfg(feature = "asyncio")]
fn busy_error_or(msg: &str) -> PyErr {
    pyo3::exceptions::PyRuntimeError::new_err(msg.to_string())
}

/// `TurnEvent` → dict（键名与纯 Python 版 `loop.aturn` 的 `on_event` dict 对齐）。
fn event_to_py(py: Python<'_>, event: &TurnEvent) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    match event {
        TurnEvent::AssistantText(text) => {
            dict.set_item("type", "content_delta")?;
            dict.set_item("text", text)?;
        }
        TurnEvent::Reasoning(text) => {
            dict.set_item("type", "reasoning_delta")?;
            dict.set_item("text", text)?;
        }
        TurnEvent::ToolCall { name, arguments } => {
            dict.set_item("type", "tool_call")?;
            dict.set_item("name", name)?;
            // `arguments` 给解析后的 dict（与 Python 版一致），另附原文备查
            dict.set_item("arguments", crate::json_to_py(py, &args_value(arguments))?)?;
            dict.set_item("arguments_raw", arguments)?;
        }
        TurnEvent::ToolResult {
            name,
            content,
            arguments,
        } => {
            dict.set_item("type", "tool_result")?;
            dict.set_item("name", name)?;
            dict.set_item("text", content)?;
            dict.set_item("arguments", crate::json_to_py(py, &args_value(arguments))?)?;
        }
        TurnEvent::Answer(text) => {
            dict.set_item("type", "answer")?;
            dict.set_item("text", text)?;
        }
    }
    Ok(dict.into_any().unbind())
}

/// 工具参数是模型给的 JSON 字符串：能解析就用解析结果，否则退回 `{"raw": ...}`（Python 同款）。
fn args_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| {
        let mut map = serde_json::Map::new();
        map.insert(
            "raw".to_string(),
            Value::String(raw.chars().take(200).collect()),
        );
        Value::Object(map)
    })
}
