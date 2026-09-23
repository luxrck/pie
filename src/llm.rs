//! 模型层：基于 reqwest 的 OpenAI 兼容客户端 —— **不依赖任何 OpenAI SDK**。
//!
//! 对齐 Python 版 `src/pie/llm.py` 的语义，但把 SDK 那一层整个换掉：
//!   - 端点就 4 个：`POST /chat/completions`（普通 + SSE 流式）、`GET /models`、`GET /user/balance`
//!     （查询余额，DeepSeek 扩展）与 Files API；
//!   - 重试自己实现（SDK 自带那套早关了）：408/409/429/5xx + 传输层异常可重试，
//!     等待优先服务端 `Retry-After`，否则随机退避 + 1s 下限；
//!   - 流式**只在还没吐出过任何增量时**才重试（吐过了重来会重复内容）。
//!
//! Rust 侧顺带白拿的好处（Python 版要专门绕的坑，这里根本不存在）：
//!   - 读到 SSE `[DONE]` 直接 break 不会留下挂起的异步生成器，也就没有
//!     `generator didn't stop after athrow()` 那类收尾噪音（Python 版为此写了 `aio.py`）；
//!   - 一个 `reqwest::Client` 就是一个连接池，不需要「按事件循环缓存 client」的
//!     WeakKeyDictionary hack；
//!   - 错误是自己定义的类型，`retryable()` 不用靠 `type(exc).__module__` 嗅探。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::Config;

// ---------------------------------------------------------------- 消息（线上格式）

/// 消息正文：纯文本，或多模态 parts（图片等）。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<Value>),
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct FunctionCall {
    pub name: String,
    /// JSON 字符串，由 agent 循环负责解析。
    pub arguments: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

/// 一条会话消息。字段全部可缺省，构造走下面的便捷方法。
///
/// 带压缩元数据（`compress_level` / `raw_*`）：这些字段**只进会话文件，不进 API 请求体**
/// （发给模型前统一过 `to_api()`，与 Python 版 `Message.to_api()` 同规则）。字段名与 Python 版
/// `to_dict()` 逐字对齐，所以两边写下的会话文件可以互相读（Rust 不写 Python 那个 `cls`——
/// 没有类层级，Python 侧读不到 cls 时本来就按 `role` 回退）。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// thinking 模式的思考内容：**必须原样回传**，否则 DeepSeek 报 400。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// 工具名（tool 消息）：工具级压缩落盘时当文件名前缀。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// 压缩级别：0=原始 1=工具级 2=轮次级 3=会话级（**只升不降**）。
    pub compress_level: u8,
    /// 压缩落盘的原文件（自描述指针）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_hash: Option<String>,
    /// 压缩前的原始文本长度 / token 估算（消息已被改写，靠它按比例复原估算）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_len: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_tokens: Option<i64>,
    /// 非用户输入注入的消息（如图片）：不构成轮次边界、不计轮数。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub synthetic: bool,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(Content::Text(text.into())),
            ..Default::default()
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(Content::Text(text.into())),
            ..Default::default()
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            role: "tool".into(),
            content: Some(Content::Text(text.into())),
            tool_call_id: Some(tool_call_id.into()),
            tool_name: Some(tool_name.into()),
            ..Default::default()
        }
    }

    /// 转成发给模型的消息（**剥掉压缩元数据**，与 Python 版 `Message.to_api()` 同规则）：
    ///   - `tool`：只要 `tool_call_id` + `content`（空则 `""`）；
    ///   - `assistant` 带 `tool_calls`：**必须带 `reasoning_content`**（空串兜底），thinking 模式缺了报 400；
    ///   - `assistant` 有思考内容：带上；
    ///   - 其余：只有 `role` + `content`（多模态 parts 原样透传）。
    pub fn to_api(&self) -> Value {
        if self.role == "tool" {
            return json!({
                "role": "tool",
                "tool_call_id": self.tool_call_id.clone().unwrap_or_default(),
                "content": match &self.content {
                    Some(Content::Text(t)) => t.clone(),
                    _ => String::new(),
                },
            });
        }
        if self.role == "assistant" {
            if let Some(calls) = &self.tool_calls {
                return json!({
                    "role": "assistant",
                    "content": self.content,
                    "tool_calls": calls,
                    "reasoning_content": self.reasoning_content.clone().unwrap_or_default(),
                });
            }
            if self.reasoning_content.is_some() {
                return json!({
                    "role": "assistant",
                    "content": self.content,
                    "reasoning_content": self.reasoning_content,
                });
            }
        }
        json!({"role": self.role, "content": self.content})
    }
}

// ---------------------------------------------------------------- 结果与用量

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Usage {
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub prompt_cache_hit_tokens: Option<i64>,
    pub prompt_cache_miss_tokens: Option<i64>,
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct CompletionTokensDetails {
    pub reasoning_tokens: Option<i64>,
}

impl Usage {
    pub fn reasoning_tokens(&self) -> Option<i64> {
        self.completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens)
    }
}

/// 会话级用量：token 字段是**最近一次** API 上报的值（**不累计求和**，跟 Python 一致——
/// 上下文是累积的，最近一次的 `prompt_tokens` 就是当前上下文大小），只有 `calls` 累加。
///
/// 字段名与 Python 版 `UsageTracker` 逐字对齐，所以两边写下的 `__meta__.usage` 可以互相读。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageTracker {
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub prompt_cache_hit_tokens: Option<i64>,
    pub prompt_cache_miss_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub calls: i64,
}

impl UsageTracker {
    /// 记一次 API 上报：token 字段覆盖，`calls` 加一。
    pub fn record(&mut self, u: &Usage) {
        self.prompt_tokens = u.prompt_tokens;
        self.completion_tokens = u.completion_tokens;
        self.total_tokens = u.total_tokens;
        self.prompt_cache_hit_tokens = u.prompt_cache_hit_tokens;
        self.prompt_cache_miss_tokens = u.prompt_cache_miss_tokens;
        self.reasoning_tokens = u.reasoning_tokens();
        self.calls += 1;
    }
}

#[derive(Clone, Debug, Default)]
pub struct LlmResult {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub reasoning_content: Option<String>,
}

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

// ---------------------------------------------------------------- 错误

#[derive(Debug)]
pub enum LlmError {
    /// 传输层（连接 / 超时 / 读 body 中断）—— 一律可重试。
    Transport(reqwest::Error),
    /// 服务端返回了非 2xx：保留 status + 原文 + `Retry-After`。
    /// 原文必须留着：`is_stale_file_error` 靠它做字符串匹配。
    Api {
        status: u16,
        body: String,
        retry_after: Option<f64>,
    },
    /// 我们自己解析不了（协议错），重试也没用。
    Protocol(String),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Transport(e) => write!(f, "连接失败: {e}"),
            LlmError::Api { status, body, .. } => {
                let body = body.trim();
                let body: String = body.chars().take(500).collect();
                write!(f, "HTTP {status}: {body}")
            }
            LlmError::Protocol(m) => write!(f, "协议错误: {m}"),
        }
    }
}

impl std::error::Error for LlmError {}

/// `file_id` 失效/不属于本账号的报错——触发「降级成内联 + 重传」自愈。
const STALE_FILE_HINTS: [&str; 2] = ["do not exist or are not created", "file_ids do not exist"];

impl LlmError {
    pub fn status(&self) -> Option<u16> {
        match self {
            LlmError::Api { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// 值不值得重试：408/409/429/5xx 与传输层异常；其余 4xx 立刻抛。
    pub fn retryable(&self) -> bool {
        match self {
            LlmError::Transport(_) => true,
            LlmError::Api { status, .. } => {
                *status == 408 || *status == 409 || *status == 429 || *status >= 500
            }
            LlmError::Protocol(_) => false,
        }
    }

    pub fn is_stale_file_error(&self) -> bool {
        match self {
            LlmError::Api {
                status: 400, body, ..
            } => STALE_FILE_HINTS.iter().any(|h| body.contains(h)),
            _ => false,
        }
    }

    /// 日志里的一行摘要（对齐 Python 的 `[retry] …（异常摘要）`）。
    fn brief(&self) -> String {
        match self {
            LlmError::Transport(e) => {
                let mut cur: &dyn std::error::Error = e;
                let mut parts = Vec::new();
                for _ in 0..2 {
                    parts.push(one_line(&cur.to_string()));
                    match cur.source() {
                        Some(s) => cur = s,
                        None => break,
                    }
                }
                format!("{}: {}", type_prefix(e), parts.join(" ↳ "))
            }
            other => {
                let mut s = one_line(&other.to_string());
                s.truncate(240);
                s
            }
        }
    }
}

fn type_prefix(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "超时"
    } else if e.is_connect() {
        "连接"
    } else if e.is_decode() {
        "解码"
    } else {
        "传输"
    }
}

fn one_line(s: &str) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.chars().take(240).collect()
}

// ---------------------------------------------------------------- 客户端

#[derive(Clone, Debug)]
pub struct LlmClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    pub model: String,
    reasoning_effort: Option<String>,
    max_tokens: Option<i64>,
    max_retries: u32,
    max_retry_delay: f64,
}

/// 重试等待的上限：再长也不把一个回合卡死。
const RETRY_AFTER_MAX: f64 = 60.0;

/// 一次失败的处置（交给 `with_retry` 的判定闭包回答）。
#[derive(Debug, PartialEq, Eq)]
enum Retry {
    /// 退避后重试（占一次重试额度）
    Backoff,
    /// 立刻重试：不占额度、不退避——只给「重发本身就能解决」的情形（如摘掉 `stream_options`）
    Now,
    /// 放弃，把错误原样抛出去
    Give,
}

/// 默认判定：还留着额度且这个错误值得重试 → 退避重试；否则放弃。
fn default_retry(max_retries: u32, attempt: u32, e: &LlmError) -> Retry {
    if attempt < max_retries && e.retryable() {
        Retry::Backoff
    } else {
        Retry::Give
    }
}

impl LlmClient {
    pub fn new(config: &Config) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs_f64(config.timeout_seconds.max(1.0)))
            .user_agent(concat!("pie-rs/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(LlmError::Transport)?;
        Ok(Self {
            http,
            api_key: config.api_key.clone(),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            reasoning_effort: config.normalized_reasoning_effort().map(str::to_string),
            max_tokens: config.reserved_tokens.map(|n| n as i64),
            max_retries: config.max_retries as u32,
            max_retry_delay: config.max_retry_delay_seconds,
        })
    }

    /// 切换思考深度（`/thinking`）：同步客户端实例（配置写回由会话层负责）。
    /// `None` = 关闭思考（不发 `reasoning_effort`，改发 `thinking: disabled`）。
    pub fn set_reasoning_effort(&mut self, level: Option<&str>) {
        self.reasoning_effort = level.map(str::to_string);
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path)
    }

    /// 请求体。`reasoning_effort` 为 `None` 时改发 `thinking: {type: disabled}`（关闭思考）。
    fn request_body(
        &self,
        messages: &[Message],
        tools: &[Value],
        stream: bool,
        with_usage: bool,
    ) -> Value {
        let mut body = json!({
            "model": self.model,
            // 只发协议字段：压缩元数据（compress_level / raw_*）不能进请求体
            "messages": messages.iter().map(Message::to_api).collect::<Vec<_>>(),
        });
        let map = body.as_object_mut().expect("object");
        if !tools.is_empty() {
            map.insert("tools".into(), Value::Array(tools.to_vec()));
        }
        match &self.reasoning_effort {
            Some(e) => {
                map.insert("reasoning_effort".into(), json!(e));
            }
            None => {
                map.insert("thinking".into(), json!({"type": "disabled"}));
            }
        }
        if let Some(mt) = self.max_tokens {
            map.insert("max_tokens".into(), json!(mt));
        }
        if stream {
            map.insert("stream".into(), json!(true));
            if with_usage {
                map.insert("stream_options".into(), json!({"include_usage": true}));
            }
        }
        body
    }

    /// **重试驱动器**（全文件唯一的重试循环）：跑 `op`，失败后由 `decide` 决定怎么办。
    ///
    /// 总共最多 `1 + max_retries` 次退避重试（`Retry::Now` 不占额度，见 `Retry` 的说明）。
    /// 三个调用点（`complete` / `list_models` / `stream`）差异都在各自那个 `decide` 里；
    /// `op` 每次调用都重新发一遍请求（流式在闭包里重建自己的累积状态）。
    async fn with_retry<T, F, Fut, D>(
        &self,
        what: &str,
        mut op: F,
        mut decide: D,
    ) -> Result<T, LlmError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, LlmError>>,
        D: FnMut(u32, &LlmError) -> Retry,
    {
        let mut attempt = 0u32;
        let result = loop {
            match op().await {
                Ok(v) => break Ok(v),
                Err(e) => match decide(attempt, &e) {
                    Retry::Give => break Err(e),
                    Retry::Now => {}
                    Retry::Backoff => {
                        attempt += 1;
                        self.sleep_before_retry(attempt, what, &e).await;
                    }
                },
            }
        };
        // 这串重试结束了（成功或放弃）：让界面把那个进度块撤掉（没重试过就是空操作）
        crate::log::progress_done(what);
        result
    }

    /// 非流式完整请求（一次尝试的完整流程就写在 `with_retry` 的闭包里）。
    pub async fn complete(
        &self,
        messages: &[Message],
        tools: &[Value],
    ) -> Result<LlmResult, LlmError> {
        self.with_retry(
            "请求",
            || async move {
                let body = self.request_body(messages, tools, false, false);
                let resp = self
                    .http
                    .post(self.url("chat/completions"))
                    .bearer_auth(&self.api_key)
                    .json(&body)
                    .send()
                    .await
                    .map_err(LlmError::Transport)?;
                let resp = check_status(resp).await?;
                let parsed: ChatResponse = resp.json().await.map_err(LlmError::Transport)?;
                let choice = parsed
                    .choices
                    .into_iter()
                    .next()
                    .ok_or_else(|| LlmError::Protocol("响应里没有 choices".into()))?;
                Ok(LlmResult {
                    content: choice.message.content,
                    tool_calls: choice.message.tool_calls.unwrap_or_default(),
                    usage: parsed.usage.unwrap_or_default(),
                    reasoning_content: choice.message.reasoning_content,
                })
            },
            |attempt, e| default_retry(self.max_retries, attempt, e),
        )
        .await
    }

    /// 流式请求：增量交给 `on_chunk`，最终结果作为返回值。
    ///
    /// 两条流式专属规则塞在 `decide` 里（其余走默认判定）：**只在还没吐出过任何增量时**才重试；
    /// 端点以 400 拒 `stream_options` 时摘掉该参数重来一次（不占重试额度）。
    /// SSE 解析（按行切、`[DONE]` 就地收工）就在闭包里的 `async` 块中，不再单开一层 `_once`。
    pub async fn stream<F>(
        &self,
        messages: &[Message],
        tools: &[Value],
        on_chunk: F,
    ) -> Result<LlmResult, LlmError>
    where
        F: FnMut(StreamChunk) + Send,
    {
        // 请求闭包与判定闭包都要读写这两个开关；`on_chunk` 还要跨多次尝试复用。
        //
        // ⚠ 这里**不能**用 `Cell` / `RefCell`：`&Cell<T>` / `&RefCell<T>` 都不是 `Send`
        //（它们不是 Sync），一旦这个 future 被 `tokio::spawn`（TUI 就是这么干的）就会
        // “future is not Send”。`AtomicBool`（Sync）+ `Mutex`（Sync）才是能在闭包间共享的。
        let with_usage = AtomicBool::new(true);
        let emitted = AtomicBool::new(false);
        let on_chunk = std::sync::Mutex::new(on_chunk);
        self.with_retry(
            "流式请求",
            || {
                emitted.store(false, Ordering::Relaxed); // 每次尝试一份干净的累积状态
                let mut state = StreamState::default();
                let with_usage = with_usage.load(Ordering::Relaxed);
                let (on_chunk, emitted) = (&on_chunk, &emitted);
                async move {
                    let body = self.request_body(messages, tools, true, with_usage);
                    let resp = self
                        .http
                        .post(self.url("chat/completions"))
                        .bearer_auth(&self.api_key)
                        .json(&body)
                        .send()
                        .await
                        .map_err(LlmError::Transport)?;
                    let resp = check_status(resp).await?;

                    let mut stream = resp.bytes_stream();
                    let mut buf: Vec<u8> = Vec::new();
                    while let Some(item) = stream.next().await {
                        let bytes = item.map_err(LlmError::Transport)?;
                        buf.extend_from_slice(&bytes);
                        // 按行切；最后一段可能是不完整的行，留在 buf 里等下一个 chunk
                        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                            let mut line: Vec<u8> = buf.drain(..=pos).collect();
                            line.pop(); // '\n'
                            if line.last() == Some(&b'\r') {
                                line.pop();
                            }
                            let line = String::from_utf8_lossy(&line);
                            let line = line.trim();
                            if line.is_empty() || line.starts_with(':') {
                                continue; // 空行 / keepalive 注释
                            }
                            let Some(payload) = line.strip_prefix("data:") else {
                                continue;
                            };
                            let payload = payload.trim();
                            if payload == "[DONE]" {
                                // 直接 return 是安全的：reqwest 的 body 被 drop 即关连接，
                                // 不存在 Python 那边挂起的异步生成器收尾问题。
                                return Ok(state.finish());
                            }
                            let chunk: StreamChunkWire =
                                serde_json::from_str(payload).map_err(|e| {
                                    LlmError::Protocol(format!("流式块解析失败: {e} — {payload}"))
                                })?;
                            state.absorb(chunk, &mut *on_chunk.lock().unwrap(), emitted);
                        }
                    }
                    Ok(state.finish())
                }
            },
            |attempt, e| {
                if emitted.load(Ordering::Relaxed) {
                    Retry::Give // 已吐过增量 → 重来会重复内容，原样往上抛
                } else if with_usage.load(Ordering::Relaxed) && e.status() == Some(400) {
                    with_usage.store(false, Ordering::Relaxed); // 摘掉 stream_options 重来，不占重试额度
                    Retry::Now
                } else {
                    default_retry(self.max_retries, attempt, e)
                }
            },
        )
        .await
    }

    /// 拉取端点可用模型 id（`GET /models`），已排序。
    pub async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        self.with_retry(
            "模型列表请求",
            || async move {
                let resp = self
                    .http
                    .get(self.url("models"))
                    .bearer_auth(&self.api_key)
                    .send()
                    .await
                    .map_err(LlmError::Transport)?;
                let resp = check_status(resp).await?;
                let parsed: ModelsResponse = resp.json().await.map_err(LlmError::Transport)?;
                let mut ids: Vec<String> = parsed.data.into_iter().map(|m| m.id).collect();
                ids.sort();
                Ok(ids)
            },
            |attempt, e| default_retry(self.max_retries, attempt, e),
        )
        .await
    }

    /// 查询账号余额（`GET /user/balance`，DeepSeek 扩展，文档见
    /// <https://api-docs.deepseek.com/zh-cn/api/get-user-balance/>）。
    ///
    /// 返回的是**原样**结构（可能多币种）：要显示成一行是展示层的事（见 `tui::status::balance_text`）。
    /// 非 OpenAI 兼容端点大多没这个接口（一般回 404）——调用方该当成「拿不到」，不当错误报。
    pub async fn fetch_balance(&self) -> Result<Balance, LlmError> {
        self.with_retry(
            "余额查询",
            || async move {
                let resp = self
                    .http
                    .get(self.url("user/balance"))
                    .bearer_auth(&self.api_key)
                    .send()
                    .await
                    .map_err(LlmError::Transport)?;
                let resp = check_status(resp).await?;
                resp.json::<Balance>().await.map_err(LlmError::Transport)
            },
            |attempt, e| default_retry(self.max_retries, attempt, e),
        )
        .await
    }

    async fn sleep_before_retry(&self, attempt: u32, what: &str, e: &LlmError) {
        let delay = retry_delay(self.max_retry_delay, e.retry_after());
        // 进度（不是告警）：TUI 侧按 `what` 就地刷新同一个块（每次重试一行太吵）；
        // 按 `what` 分组，并行的两条流程（回合请求 / 拉模型列表）各占一块
        crate::log::progress(
            what,
            format!(
                "[retry] {what}失败（{}），{delay:.1}s 后第 {attempt}/{} 次重试",
            e.brief(),
            self.max_retries
        ));
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;
    }
}

// ---------------------------------------------------------------- Files API（图片上传件）
//
// 与 Python 版 `src/pie/files.py` 同一套契约（本地副本命名、`__meta__.files` 条目字段、
// `expires_after` 语义），只是按用户要求**不再单独开一个 files.rs**，全放在这里。
//
// 与 Python 的一处**有意差异**：那边上传失败（或模型不支持）会**回退内联 base64**，
// 这边只走 Files API —— 拿不到 `file_id` 就不注入图片（read 的标记文本仍在工具结果里），
// 不做 base64 回退。

/// 服务端上传件（`GET /files` 的条目）：字段缺失给 None，别让展示层崩。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FileObject {
    pub id: String,
    pub filename: Option<String>,
    pub bytes: Option<i64>,
    pub created_at: Option<i64>,
    pub expires_at: Option<i64>,
}

/// 支持 `file` 内容块的模型（与 Python `FILES_API_MODELS` 同名单）。
pub const FILES_API_MODELS: [&str; 2] = ["deepseek-flash", "deepseek-v4-flash-vision-exp"];

/// 服务端允许的上传有效期上限（天）。
pub const TTL_MAX_DAYS: i64 = 30;

/// 当前模型是否支持 `file` 内容块（不支持就别白传）。
pub fn model_supports_files(model: &str) -> bool {
    !model.is_empty() && FILES_API_MODELS.iter().any(|name| model.contains(name))
}

/// API key 指纹（sha256 前 8 位）：判断缓存的 `file_id` 是否还属于当前 key。
pub fn key_fingerprint(api_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(api_key.as_bytes());
    hex::encode(hasher.finalize())[..8].to_string()
}
impl LlmClient {
    /// 上传一张图（multipart），返回服务端 `FileObject`。
    ///
    /// `expires_after` 是 DeepSeek 扩展字段，用**方括号展开**的两个表单字段发：
    /// `expires_after[anchor]=created_at`、`expires_after[seconds]=N`——
    /// 这是 Python SDK 的 `_serialize_multipartform`（`stringify_items(array_format="brackets")`）
    /// 干的事，也是**服务端唯一认的编码**：实测把 JSON 串（无论 part 是不是 application/json）
    /// 当单个字段发，响应里 `expires_at` 都是 `null`（= 服务端当永久件收下了）。
    pub async fn upload_file(
        &self,
        data: Vec<u8>,
        filename: &str,
        mime: &str,
        ttl_days: i64,
    ) -> Result<FileObject, LlmError> {
        let filename = filename.to_string();
        let mime = mime.to_string();
        self.with_retry(
            "图片上传",
            || {
                let data = data.clone();
                let filename = filename.clone();
                let mime = mime.clone();
                async move {
                    let part = reqwest::multipart::Part::bytes(data)
                        .file_name(filename)
                        .mime_str(&mime)
                        .map_err(|e| LlmError::Protocol(format!("图片 mime 非法: {e}")))?;
                    let mut form = reqwest::multipart::Form::new()
                        .part("file", part)
                        .text("purpose", "user_data");
                    let seconds = ttl_days.clamp(0, TTL_MAX_DAYS) * 86_400;
                    if seconds > 0 {
                        form = form
                            .text("expires_after[anchor]", "created_at")
                            .text("expires_after[seconds]", seconds.to_string());
                    }
                    let resp = self
                        .http
                        .post(self.url("files"))
                        .bearer_auth(&self.api_key)
                        .multipart(form)
                        .send()
                        .await
                        .map_err(LlmError::Transport)?;
                    let resp = check_status(resp).await?;
                    resp.json::<FileObject>().await.map_err(LlmError::Transport)
                }
            },
            |attempt, e| default_retry(self.max_retries, attempt, e),
        )
        .await
    }

    /// 列出**本账号**的全部上传件（`GET /files`，自动翻页）。
    pub async fn list_files(&self) -> Result<Vec<FileObject>, LlmError> {
        let mut out: Vec<FileObject> = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut query: Vec<(&str, String)> = vec![("limit", "100".to_string())];
            if let Some(after) = &after {
                query.push(("after", after.clone()));
            }
            let resp = self
                .http
                .get(self.url("files"))
                .bearer_auth(&self.api_key)
                .query(&query)
                .send()
                .await
                .map_err(LlmError::Transport)?;
            let resp = check_status(resp).await?;
            let page: FileListPage = resp.json().await.map_err(LlmError::Transport)?;
            let last = page.data.last().map(|f| f.id.clone());
            out.extend(page.data);
            match (page.has_more, last) {
                (true, Some(id)) if !id.is_empty() => after = Some(id),
                _ => return Ok(out),
            }
        }
    }

    /// 删除一个上传件（`DELETE /files/{id}`）。
    pub async fn delete_file(&self, id: &str) -> Result<(), LlmError> {
        let resp = self
            .http
            .delete(self.url(&format!("files/{id}")))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(LlmError::Transport)?;
        let _ = check_status(resp).await?;
        Ok(())
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FileListPage {
    data: Vec<FileObject>,
    has_more: bool,
}

impl LlmError {
    fn retry_after(&self) -> Option<f64> {
        match self {
            LlmError::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// 非 2xx → `LlmError::Api`（带上 status / 原文 / `Retry-After`）。
async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, LlmError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<f64>().ok());
    let body = resp.text().await.unwrap_or_default();
    Err(LlmError::Api {
        status: status.as_u16(),
        body,
        retry_after,
    })
}

/// 本次重试前等待的秒数：**优先服务端 `Retry-After`**，否则 `max(1.0, random(0, max_delay))`。
///
/// 429/503 常带 `Retry-After`（服务端说多久后再来，比瞎退避准），夹在 `[1.0, 60.0]`：
/// 太短的限流等待没意义，太长会把一个回合卡死。没有该头时随机退避避免同时撞回来。
pub fn retry_delay(max_delay_seconds: f64, retry_after: Option<f64>) -> f64 {
    if let Some(ra) = retry_after {
        return RETRY_AFTER_MAX.min(1.0f64.max(ra));
    }
    // 不引 rand 依赖：拿当前时间的小数部分当 jitter 源足够（目的只是打散同时重试）
    let nanos = crate::config::now().subsec_nanos() as f64 / 1e9;
    1.0f64.max(nanos * max_delay_seconds.max(0.0))
}

// ---------------------------------------------------------------- 流式状态累积

#[derive(Default)]
struct StreamState {
    content: String,
    reasoning: String,
    usage: Usage,
    /// 按 index 归并 tool_call 增量（BTreeMap 保证顺序 = index 顺序）。
    tool_calls: BTreeMap<usize, ToolCall>,
}

impl StreamState {
    fn absorb<F>(&mut self, chunk: StreamChunkWire, on_chunk: &mut F, emitted: &AtomicBool)
    where
        F: FnMut(StreamChunk),
    {
        if let Some(u) = chunk.usage {
            self.usage = u;
        }
        let Some(choice) = chunk.choices.into_iter().next() else {
            return;
        };
        let delta = choice.delta;
        if let Some(r) = delta.reasoning_content.filter(|s| !s.is_empty()) {
            self.reasoning.push_str(&r);
            emitted.store(true, Ordering::Relaxed);
            on_chunk(StreamChunk::Reasoning(r));
        }
        if let Some(c) = delta.content.filter(|s| !s.is_empty()) {
            self.content.push_str(&c);
            emitted.store(true, Ordering::Relaxed);
            on_chunk(StreamChunk::Content(c));
        }
        for tc in delta.tool_calls.unwrap_or_default() {
            let index = tc.index.unwrap_or(0) as usize;
            let (name, arguments) = match tc.function {
                Some(f) => (f.name.unwrap_or_default(), f.arguments.unwrap_or_default()),
                None => (String::new(), String::new()),
            };
            let slot = self.tool_calls.entry(index).or_default();
            if let Some(id) = tc.id.filter(|s| !s.is_empty()) {
                slot.id = id;
            }
            if !name.is_empty() {
                slot.function.name = name;
            }
            slot.function.arguments.push_str(&arguments);
            emitted.store(true, Ordering::Relaxed);
            on_chunk(StreamChunk::ToolCall {
                index,
                id: slot.id.clone(),
                name: slot.function.name.clone(),
                arguments,
            });
        }
    }

    fn finish(mut self) -> LlmResult {
        for slot in self.tool_calls.values_mut() {
            if slot.kind.is_empty() {
                slot.kind = "function".into();
            }
        }
        LlmResult {
            content: (!self.content.is_empty()).then_some(self.content),
            reasoning_content: (!self.reasoning.is_empty()).then_some(self.reasoning),
            tool_calls: self.tool_calls.into_values().collect(),
            usage: self.usage,
        }
    }
}

// ---------------------------------------------------------------- 线上结构

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ResponseMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,
    reasoning_content: Option<String>,
}

#[derive(Deserialize)]
struct StreamChunkWire {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StreamDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<StreamToolCall>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StreamToolCall {
    index: Option<i64>,
    id: Option<String>,
    function: Option<StreamFunction>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StreamFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

/// 账号余额（`GET /user/balance` 的原样响应）。
///
/// 服务端把金额写成**字符串**（`"110.00"`，避免浮点误差）——这里也不动它，
/// 要算要说都交给展示层。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Balance {
    /// 当前账户是否有余额可供 API 调用
    #[serde(default)]
    pub is_available: bool,
    #[serde(default)]
    pub balance_infos: Vec<BalanceInfo>,
}

/// 一个币种的余额明细。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BalanceInfo {
    /// `CNY` / `USD`
    pub currency: String,
    /// 总的可用余额（含赠金 + 充值）
    pub total_balance: String,
    /// 未过期的赠金余额
    #[serde(default)]
    pub granted_balance: String,
    /// 充值余额
    #[serde(default)]
    pub topped_up_balance: String,
}

#[derive(Deserialize)]
struct ModelEntry {
    #[serde(default)]
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    // 单线程的测试里用 `Cell` 计数就行（`Send` 约束只对 spawn 出去的 future 有意义）
    use std::cell::Cell;

    #[test]
    fn retry_delay_prefers_retry_after_and_clamps() {
        assert_eq!(retry_delay(3.0, Some(5.0)), 5.0);
        assert_eq!(retry_delay(3.0, Some(0.2)), 1.0); // 下限 1s
        assert_eq!(retry_delay(3.0, Some(600.0)), 60.0); // 上限 60s
        let d = retry_delay(0.0, None);
        assert_eq!(d, 1.0); // 无 Retry-After 时 1s 下限
    }

    #[test]
    fn retryable_matches_python_semantics() {
        let api = |s: u16| LlmError::Api {
            status: s,
            body: String::new(),
            retry_after: None,
        };
        assert!(api(408).retryable());
        assert!(api(409).retryable());
        assert!(api(429).retryable());
        assert!(api(500).retryable());
        assert!(!api(400).retryable());
        assert!(!api(401).retryable());
        assert!(!LlmError::Protocol("x".into()).retryable());
    }

    /// 默认判定：额度 + 可重试性（真正的等待/重试行为在 `retry_driver_*`）。
    #[test]
    fn default_retry_respects_budget_and_retryability() {
        let api = |s: u16| LlmError::Api {
            status: s,
            body: String::new(),
            retry_after: None,
        };
        assert_eq!(default_retry(2, 0, &api(429)), Retry::Backoff);
        assert_eq!(default_retry(2, 1, &api(503)), Retry::Backoff);
        assert_eq!(default_retry(2, 2, &api(429)), Retry::Give); // 额度用尽
        assert_eq!(default_retry(2, 0, &api(401)), Retry::Give); // 不可重试的 4xx
        assert_eq!(default_retry(0, 0, &api(500)), Retry::Give); // 配置成不重试
        assert_eq!(
            default_retry(2, 0, &LlmError::Protocol("x".into())),
            Retry::Give
        );
    }

    /// 驱动器自身：`Retry::Now` 立刻重来（不吃额度、不退避），`Retry::Give` 只尝试一次。
    #[tokio::test]
    async fn retry_driver_honours_now_and_give() {
        let c = LlmClient::new(&Config::default()).unwrap();
        let bad = || LlmError::Api {
            status: 400,
            body: String::new(),
            retry_after: None,
        };

        let calls = Cell::new(0);
        let ok: Result<u32, LlmError> = c
            .with_retry(
                "请求",
                || {
                    calls.set(calls.get() + 1);
                    async {
                        if calls.get() < 3 {
                            Err(bad())
                        } else {
                            Ok(7)
                        }
                    }
                },
                |_, _| Retry::Now,
            )
            .await;
        assert_eq!(ok.unwrap(), 7);
        assert_eq!(calls.get(), 3);

        let calls = Cell::new(0);
        let e = c
            .with_retry(
                "请求",
                || {
                    calls.set(calls.get() + 1);
                    async { Err::<u32, _>(LlmError::Protocol("x".into())) }
                },
                |_, _| Retry::Give,
            )
            .await
            .unwrap_err();
        assert!(matches!(e, LlmError::Protocol(_)));
        assert_eq!(calls.get(), 1);
    }

    /// `GET /user/balance` 的响应形状：直接拿官方文档的例子当基准（金额是**字符串**）。
    #[test]
    fn balance_response_parses_the_documented_example() {
        let body = r#"{
            "is_available": true,
            "balance_infos": [
                {
                    "currency": "CNY",
                    "total_balance": "110.00",
                    "granted_balance": "10.00",
                    "topped_up_balance": "100.00"
                }
            ]
        }"#;
        let parsed: Balance = serde_json::from_str(body).expect("文档例子要能解");
        assert!(parsed.is_available);
        assert_eq!(parsed.balance_infos.len(), 1);
        let info = &parsed.balance_infos[0];
        assert_eq!(info.currency, "CNY");
        assert_eq!(info.total_balance, "110.00");
        assert_eq!(info.granted_balance, "10.00");
        assert_eq!(info.topped_up_balance, "100.00");

        // 字段缺失也不能崩（非 DeepSeek 端点 / 老版本可能少字段）
        let lenient: Balance = serde_json::from_str("{\"balance_infos\":[{\"currency\":\"USD\",\"total_balance\":\"1.00\"}]}")
            .expect("缺字段按默认值");
        assert!(!lenient.is_available);
        assert_eq!(lenient.balance_infos[0].granted_balance, "");
    }

    #[test]
    fn stale_file_error_is_recognized_from_body() {
        let e = LlmError::Api {
            status: 400,
            body: r#"{"error":{"message":"the following file_ids do not exist or are not created under your account"}}"#.into(),
            retry_after: None,
        };
        assert!(e.is_stale_file_error());
        let other = LlmError::Api {
            status: 400,
            body: "bad request".into(),
            retry_after: None,
        };
        assert!(!other.is_stale_file_error());
    }

    #[test]
    fn request_body_shape() {
        let mut config = Config::default();
        config.reasoning_effort = "high".into();
        let c = LlmClient::new(&config).unwrap();
        let b = c.request_body(&[Message::user("hi")], &[], false, false);
        assert_eq!(b["reasoning_effort"], "high");
        assert!(b.get("thinking").is_none());
        assert_eq!(b["max_tokens"], 128_000);

        config.reasoning_effort = "none".into();
        let c = LlmClient::new(&config).unwrap();
        let b = c.request_body(&[Message::user("hi")], &[], true, true);
        assert!(b.get("reasoning_effort").is_none());
        assert_eq!(b["thinking"]["type"], "disabled");
        assert_eq!(b["stream"], true);
        assert_eq!(b["stream_options"]["include_usage"], true);
    }

    #[test]
    fn usage_parses_deepseek_fields() {
        let u: Usage = serde_json::from_value(json!({
            "prompt_tokens": 10,
            "completion_tokens": 20,
            "total_tokens": 30,
            "prompt_cache_hit_tokens": 5,
            "prompt_cache_miss_tokens": 5,
            "completion_tokens_details": {"reasoning_tokens": 7}
        }))
        .unwrap();
        assert_eq!(u.prompt_tokens, Some(10));
        assert_eq!(u.prompt_cache_hit_tokens, Some(5));
        assert_eq!(u.reasoning_tokens(), Some(7));
    }

    #[test]
    fn stream_state_merges_tool_call_deltas_by_index() {
        let mut state = StreamState::default();
        let emitted = AtomicBool::new(false);
        let mut seen = Vec::new();
        let mut feed = |v: Value| {
            let chunk: StreamChunkWire = serde_json::from_value(v).unwrap();
            state.absorb(chunk, &mut |c| seen.push(c), &emitted);
        };
        feed(
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"pa"}}]}}]}),
        );
        feed(
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]}}]}),
        );
        feed(json!({"choices":[{"delta":{"content":"done"}}],"usage":{"prompt_tokens":1}}));
        let result = state.finish();
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].function.name, "read");
        assert_eq!(result.tool_calls[0].function.arguments, r#"{"path":"a"}"#);
        assert_eq!(result.tool_calls[0].kind, "function");
        assert_eq!(result.content.as_deref(), Some("done"));
        assert_eq!(result.usage.prompt_tokens, Some(1));
        assert!(emitted.load(Ordering::Relaxed));
        assert_eq!(seen.len(), 3);
    }
}
