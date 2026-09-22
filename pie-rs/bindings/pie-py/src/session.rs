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

use pie_rs::cancel::Cancel;
use pie_rs::context::CompactMode;
use pie_rs::session::{Session as CoreSession, TurnEvent};

use crate::config::PyConfig;
use crate::llm::PyLlmClient;
use crate::tools::PyToolRegistry;
use crate::{busy_error, llm_error, pie_error};

// ---------------------------------------------------------------- Cancel

/// 取消信号：给 `aturn(cancel=...)` 用，或从别的线程停住一个正在跑的回合。
#[pyclass(name = "Cancel", module = "pie_rs")]
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
        format!("<pie_rs.Cancel cancelled={}>", self.0.is_cancelled())
    }
}

// ---------------------------------------------------------------- Session

#[pyclass(name = "Session", module = "pie_rs")]
pub struct PySession {
    inner: std::sync::Arc<TokioMutex<CoreSession>>,
    /// 会话文件路径（构造后就固定；单拎出来是为了不为了读个路径去抢锁）。
    path: String,
    /// 当前回合的取消信号 —— `stop()` 靠它中断（回合结束清空）。
    current: StdMutex<Option<Cancel>>,
}

impl PySession {
    fn wrap(session: CoreSession) -> Self {
        Self {
            path: session.path.display().to_string(),
            inner: std::sync::Arc::new(TokioMutex::new(session)),
            current: StdMutex::new(None),
        }
    }

    /// 拿会话锁：**忙就报错，不排队**（排队会在「回调里调同一个 session」时死锁）。
    fn lock(&self) -> PyResult<tokio::sync::MutexGuard<'_, CoreSession>> {
        self.inner.try_lock().map_err(|_| busy_error())
    }

    fn build(
        cfg: &PyConfig,
        llm: &PyLlmClient,
        tools: &PyToolRegistry,
    ) -> (pie_rs::config::Config, pie_rs::llm::LlmClient, pie_rs::tools::ToolRegistry) {
        (cfg.inner.clone(), llm.inner.clone(), tools.inner.clone())
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
        let (cfg, llm, tools) = Self::build(config, llm, tools);
        Self::wrap(CoreSession::new(&cfg, id, llm, tools))
    }

    /// 临时会话：**不落盘、不写压缩 manifest**（一次性任务 / notebook / 服务用）。
    #[staticmethod]
    fn ephemeral(config: &PyConfig, llm: &PyLlmClient, tools: &PyToolRegistry) -> Self {
        let (cfg, llm, tools) = Self::build(config, llm, tools);
        Self::wrap(CoreSession::ephemeral(&cfg, llm, tools))
    }

    /// 从 JSONL 恢复（`path` 必须存在）。
    #[staticmethod]
    fn load(
        path: &str,
        config: &PyConfig,
        llm: &PyLlmClient,
        tools: &PyToolRegistry,
    ) -> PyResult<Self> {
        let (cfg, llm, tools) = Self::build(config, llm, tools);
        CoreSession::load(Path::new(path), &cfg, llm, tools)
            .map(Self::wrap)
            .map_err(pie_error)
    }

    /// 恢复最近的会话（同工作目录优先）——与 CLI 的 `-r` 同款。
    #[staticmethod]
    fn resume(config: &PyConfig, llm: &PyLlmClient, tools: &PyToolRegistry) -> PyResult<Self> {
        let (cfg, llm, tools) = Self::build(config, llm, tools);
        CoreSession::resume(&cfg, llm, tools)
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
    #[pyo3(signature = (input, on_event=None, cancel=None))]
    fn aturn(
        &self,
        py: Python<'_>,
        input: String,
        on_event: Option<Py<PyAny>>,
        cancel: Option<PyRef<'_, PyCancel>>,
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
                session.aturn(&input, &mut emit, &token).await
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

    fn __repr__(&self) -> String {
        format!("<pie_rs.Session path={} busy={}>", self.path, self.inner.try_lock().is_err())
    }
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
        TurnEvent::ToolCall {
            name,
            arguments,
            turn,
            step,
        } => {
            dict.set_item("type", "tool_call")?;
            dict.set_item("name", name)?;
            // `arguments` 给解析后的 dict（与 Python 版一致），另附原文备查
            dict.set_item("arguments", crate::json_to_py(py, &args_value(arguments))?)?;
            dict.set_item("arguments_raw", arguments)?;
            dict.set_item("turn", *turn)?;
            dict.set_item("step", *step)?;
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
