//! 会话层：多轮对话（历史 + JSONL 持久化 + 用量）**加上一回合的 agent 循环**。
//!
//! 回合循环原来在独立的 `loop.rs`（`run_turn`），已并进 `Session::aturn`：两者本来就是一件事的
//! 两半，分开只会让每次调用在两模块之间穿 7 个参数（其中 4 个还是 `self` 的字段）。
//! 一次性模式改走 `Session::ephemeral()`（不落盘、压缩事件只在内存）。
//!
//! **持久化契约**：
//!   - 一行一条 JSON：首行 `__meta__`（usage / windows / cwd / title / files / compaction_events），之后每行一条消息；
//!   - 恢复时**丢弃文件里的 system 消息**，按当前 SYSTEM.md / AGENTS.md / MEMORY.md 重建
//!     （提示词会变，历史里那份旧 system 没有意义）；
//!   - 会话目录是 `~/.pie/sessions/`，resume 按 mtime 选最新、「同 cwd 优先」（读 meta 里的 `cwd`；
//!     旧会话没这个键就当不匹配）；未知字段 serde 直接忽略。
//!
//! 文件名用 `chat-<unix 秒>-<微秒>.jsonl`：没有日期库，Unix 时间戳一样能做到「可排序 + 微秒级不撞车」。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use futures_util::stream::{self, StreamExt};

use crate::cancel::{CANCEL_TEXT, Cancel};
use crate::config::{self, Config};
use crate::context;
use crate::llm::{
    self, Compaction, Content, LlmClient, LlmError, LlmResult, Message, RequestOptions,
    StreamChunk, ToolCall, UsageTracker,
};
use crate::tools::{self, Attachment, ToolOutput, ToolRegistry};

/// 模型请求失败时补进历史的那条 assistant 消息的前缀（与 `cancel::CANCEL_TEXT` 同款用途：
/// 让「回合没产出」这件事在历史里留下一条**能认出来**的 assistant 消息，而不是留个悬空提问）。
pub(crate) const ERROR_TURN_PREFIX: &str = "[请求失败] ";

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

/// 解释器跑过的单个代码块（写进 `__meta__.repl_blocks`）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReplBlock {
    /// 工具调用的 id（判「它还在不在当前对话里」就靠它）。
    pub id: String,
    /// 那次 `repl` 的 `code` 参数（逐字；过长的按字符截断并标一句）。
    pub code: String,
}

/// `repl_blocks` 最多留这么多个（超出丢最老的）——它同时限住 `__meta__` 与那节 system prompt。
const REPL_BLOCKS_MAX: usize = 24;
/// 单个 cell 存进日志/显示的字符上限（超了截断 + 提示看原文）。
const REPL_BLOCK_MAX_CHARS: usize = 2000;

#[derive(Debug)]
pub struct Session {
    /// 会话 JSONL 自身的路径（也是 `save` 的默认目标）。
    pub path: PathBuf,
    /// 配置快照（压缩水位 / 压缩参数（含保护窗口）/ 数据目录都从它取）。
    pub config: Config,
    /// 模型后端（会话自己持有）。
    pub llm: LlmClient,
    /// 工具集（`--tools` 裁剪过的注册表就从这里进来）。
    pub tools: ToolRegistry,
    /// 会话级工具状态（同一 Session 的所有工具调用共享；`repl` 的 IPython 子进程就挂这儿）。
    pub tool_state: std::sync::Arc<crate::tools::SessionState>,
    /// 压缩事件流水（每条落盘成 `{kind, ts, path, hash, summary?}`，`kind` 就是类型标签；
    /// 旧会话里的键是 `raw_path` / `raw_hash`，靠 `CompactEvent` 的 serde alias 读回）。
    ///
    /// 以前单独 append 到 `~/.pie/context/<会话名>.manifest.jsonl`；现在**只住内存**，
    /// `save()` 时作为 `__meta__.compaction_events` 一起落盘——于是它跟消息里的 `compaction` 指针
    /// **同一趟车**，不会出现「账记了、指针没记」（`ephemeral` 会话不 `save`，自然也就不落盘）。
    pub compaction_events: Vec<context::CompactEvent>,
    /// 解释器跑过的代码块（写进 `__meta__.repl_blocks`）。
    ///
    /// 为什么得单独存一份：压缩会把 `tool_calls` 从消息里删掉（轮次级把整段 span 换成摘要、
    /// 会话级把整窗口归档）—— 等要把代码摆进 system prompt 那节时，消息里已经查不到了。
    /// 所以它**独立于压缩**维护（每轮开头扫一遍消息里新增的 `repl` 调用、按 call id 去重）。
    /// 而 system prompt 那节的内容是由它派生的：**只列 id 已不在 `messages` 里的那些** ——
    /// 两次压缩之间逐字不变（缓存照旧命中），只有真被压走东西的那一刻才变。
    pub repl_blocks: Vec<ReplBlock>,
    /// 完整历史（`messages[0]` 是当前 system prompt）。
    pub messages: Vec<Message>,
    /// 用量：token 是最近一次上报值，`calls` 累计（见 `UsageTracker`）。
    pub usage: UsageTracker,
    /// 历史窗口块（会话级压缩产出，写进 `__meta__.windows`）。
    pub windows: Vec<PathBuf>,
    /// 图片 id 表：`hash_id` → 上传记录（写进 `__meta__.files`；见 `llm::ImageStore`）。
    pub files: HashMap<String, Value>,
    /// 会话标题 = 首个用户输入的首行（写进 `__meta__.title`）。
    pub title: Option<String>,
    /// 用户消息条数（载入时按历史重算）。
    pub turn_count: usize,
    /// 启动时拉取的可用模型 id（`/model` 的候选；**不持久化**）。
    pub available_models: Option<Vec<String>>,
}

/// 「思考」计时：实时视图里那行 `• Thought for 3.4s` 的来源。
///
/// 口径与 TUI 的 `settle_thought` **一致**：**首个 reasoning 增量**起算、**首个正文增量**停下
/// （中途插进来的工具调用不算停）。量到的毫秒数写进本回合的 assistant 消息（`thought_ms`，
/// 只进会话文件），退出再 `pie -r` 回来才还原得出这一行——`reasoning_content` 只说"思考过"，
/// 没说是多久。非流式没有增量事件 → 量不到（实时视图那边同样不显示这行）。
#[derive(Default)]
struct ThoughtClock {
    started: Option<std::time::Instant>,
    ms: Option<u64>,
}

impl ThoughtClock {
    fn on(&mut self, ev: &TurnEvent) {
        match ev {
            TurnEvent::Reasoning(_) => {
                self.started.get_or_insert_with(std::time::Instant::now);
            }
            TurnEvent::AssistantText(_) => {
                if let Some(start) = self.started.take() {
                    self.ms = Some(start.elapsed().as_millis() as u64);
                }
            }
            _ => {}
        }
    }

    /// 取走本次量到的时长（写完那条 assistant 消息后就清空，下一个 model call 重新量）。
    fn take_ms(&mut self) -> Option<u64> {
        self.ms.take()
    }
}

impl Session {
    /// 新建会话：`id` 给了就用它（纯名字 → `~/.pie/sessions/<id>.jsonl`，带目录/绝对路径 → 原样），
    /// 否则按时间戳新建文件。
    ///
    /// `llm` / `tools` 由调用方给：
    /// 外面已经建好的客户端 /（可能被 `--tools` 裁剪过的）工具集直接收进来。
    pub fn new(config: &Config, id: Option<&str>, llm: LlmClient, tools: ToolRegistry) -> Self {
        Self::at(
            resolve_path(&config.storage.sessions(), id),
            config,
            llm,
            tools,
        )
    }

    /// 临时会话（`pie "任务"` 用）：不落盘（别调 `save`，压缩事件也就不会落盘），其余完全一样。
    ///
    /// 既然回合循环已经并在 `Session` 上，
    /// 就用“不记账的 Session”表达同一件事。
    pub fn ephemeral(config: &Config, llm: LlmClient, tools: ToolRegistry) -> Self {
        Self::at(
            config
                .storage
                .sessions()
                .join(format!("ephemeral-{}.jsonl", timestamp())),
            config,
            llm,
            tools,
        )
    }

    /// 拉取端点可用模型 id 并缓存（`/model` 的候选列表；失败不动旧缓存）。
    ///
    /// ⚠ 暂时只有单测在读它：入口是交互层（`/model`），一次性 CLI 用 `--models`。
    #[allow(dead_code)]
    pub async fn fetch_models(&mut self) -> Result<Vec<String>, LlmError> {
        let models = self.llm.list_models().await?;
        self.available_models = Some(models.clone());
        Ok(models)
    }

    /// 切模型（`/model <id>`）：改配置 + 同步客户端实例，并把配置写回文件。
    /// 返回一句提示（写不进去就说明“仅本次生效”）。
    ///
    /// ⚠ 暂时只有单测在读它：入口是交互层的 `/model` 命令。
    #[allow(dead_code)]
    pub fn set_model(&mut self, name: &str) -> String {
        self.config.model = name.to_string();
        self.llm.model = name.to_string();
        self.persist_config()
    }

    /// 切思考深度（`/thinking <none|low|high|max>`）：同上。
    #[allow(dead_code)] // 入口是交互层的 `/thinking`
    pub fn set_reasoning_effort(&mut self, level: &str) -> String {
        self.config.reasoning_effort = level.to_string();
        self.llm
            .set_reasoning_effort(self.config.normalized_reasoning_effort());
        self.persist_config()
    }

    fn persist_config(&self) -> String {
        match self.config.save() {
            Ok(_) => "已写入配置".to_string(),
            Err(e) => format!("配置写入失败: {e}（仅本次会话生效）"),
        }
    }

    /// 从 JSONL 恢复（`path` 必须存在）。
    pub fn load(
        path: &Path,
        config: &Config,
        llm: LlmClient,
        tools: ToolRegistry,
    ) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("读取会话失败 {}: {e}", path.display()))?;
        let mut session = Self::at(path.to_path_buf(), config, llm, tools);
        let mut restored: Vec<Message> = Vec::new();
        for (n, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line)
                .map_err(|e| format!("会话文件第 {} 行不是合法 JSON: {e}", n + 1))?;
            if value
                .get("__meta__")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                if let Some(u) = value.get("usage") {
                    session.usage = serde_json::from_value(u.clone()).unwrap_or_default();
                }
                session.title = value
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                // 窗口块列表（旧键 `fs` 是更早的写法，一并兼容）
                let windows = value.get("windows").or_else(|| value.get("fs"));
                session.windows = windows
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(PathBuf::from)
                            .collect()
                    })
                    .unwrap_or_default();
                // 压缩事件流水（与消息里的 `compaction` 指针同车落盘）
                // 逐条解析：单条坏（手改过？）只丢那条，不让整份流水变空
                session.compaction_events = value
                    .get("compaction_events")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| serde_json::from_value(e.clone()).ok())
                            .collect()
                    })
                    .unwrap_or_default();
                // 图片 id 表（hash_id → 上传记录）
                session.files = value
                    .get("files")
                    .and_then(Value::as_object)
                    .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                // 解释器代码块日志（与那节 system prompt 同源；坏条目单独丢）
                session.repl_blocks = value
                    .get("repl_blocks")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|b| serde_json::from_value(b.clone()).ok())
                            .collect()
                    })
                    .unwrap_or_default();
                continue;
            }
            // 旧 system（提示词 / 窗口摘要）丢掉：按当前提示词重建
            if value.get("role").and_then(Value::as_str) == Some("system") {
                continue;
            }
            let msg: Message = serde_json::from_value(value)
                .map_err(|e| format!("会话文件第 {} 行不是合法消息: {e}", n + 1))?;
            restored.push(msg);
        }
        // 防御：带 tool_calls 却没有 reasoning_content 的 assistant 消息补空串
        // （thinking 模式回放这种历史会被 DeepSeek 判 400）
        for m in &mut restored {
            if m.tool_calls.is_some() && m.reasoning_content.is_none() {
                m.reasoning_content = Some(String::new());
            }
        }
        // 用户轮数：`synthetic` 的（注入的图片消息，role 也是 user）不算轮次
        session.turn_count = restored
            .iter()
            .filter(|m| m.role == "user" && !m.synthetic)
            .count();
        if session.title.is_none() {
            // 旧文件没有 title：从首个用户消息补（不立刻写回，下次 save 就带上了）
            session.title =
                restored
                    .iter()
                    .find(|m| m.role == "user")
                    .and_then(|m| match &m.content {
                        Some(Content::Text(t)) => first_line(t),
                        _ => None,
                    });
        }
        // 文件里的旧 system（提示词 / 窗口摘要）都已丢掉 → 按 `windows` 列表**重建**窗口摘要：
        // 摘要文本随 `head/tail` 配置与提示词变化，重建比留着旧的更准。
        let window_summaries = session.window_summary_messages();
        session.messages.extend(window_summaries);
        session.messages.extend(restored);
        Ok(session)
    }

    /// 按 `self.windows` 生成历史窗口的摘要 system 消息（`compaction.session` 关掉就不生成）。
    fn window_summary_messages(&self) -> Vec<Message> {
        let Some(session_config) = self
            .config
            .compaction
            .as_ref()
            .and_then(|c| c.session.as_ref())
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for block in &self.windows {
            if !block.exists() {
                continue; // 窗口块没了（被 gc 清掉？）→ 不硬塞死链
            }
            let mut msg = Message::system(context::build_window_summary(
                block,
                session_config.head,
                session_config.tail,
            ));
            msg.compaction = Some(Compaction::session(block.as_path()));
            out.push(msg);
        }
        out
    }

    /// 恢复最近的会话：优先**当前工作目录**下最新的，没有则全局最新。
    pub fn resume(config: &Config, llm: LlmClient, tools: ToolRegistry) -> Result<Self, String> {
        let cwd = std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string());
        Self::resume_in(
            &config.storage.sessions(),
            cwd.as_deref(),
            config,
            llm,
            tools,
        )
    }

    /// `resume` 的本体（目录可注入，便于测试）。
    pub fn resume_in(
        dir: &Path,
        cwd: Option<&str>,
        config: &Config,
        llm: LlmClient,
        tools: ToolRegistry,
    ) -> Result<Self, String> {
        let path =
            latest_in(dir, cwd).ok_or_else(|| format!("{} 中没有历史会话", dir.display()))?;
        Self::load(&path, config, llm, tools)
    }

    /// 追加一条用户消息（不含模型调用）。
    pub fn push_user(&mut self, text: &str) {
        if self.title.is_none() {
            self.title = first_line(text);
        }
        self.messages.push(Message::user(text));
        self.turn_count += 1;
    }

    /// 追加一条 **assistant** 消息（纯文本、无 `tool_calls`）——给嵌入方补历史用。
    ///
    /// 典型场景（synthetic 那套）：模型这一轮什么也没改，就在历史里补一条 assistant 再补一条
    /// user 提醒，然后**接着跑**下一个 `aturn`。计 `turn_count` 只数用户消息，这里不动它。
    pub fn push_assistant(&mut self, text: &str) {
        self.messages.push(Message::assistant(text));
    }

    /// 模型请求失败（外部因素：网络 / 服务端错 / 协议错）→ 往历史里补一条 assistant 消息
    /// （内容 = 错误信息）。
    ///
    /// 不补的话 `push_user` 那条 user 就成了**没有回答的悬空提问**：历史里出现连续两条 user、
    /// resume 回放也莫名其妙（实测这种历史 API 收，但语义上是脏的）。与取消同款——
    /// 取消补 `CANCEL_TEXT`，失败补 `[请求失败] <错误>`；调用方仍然拿到 `Err`（界面照旧报错），
    /// 只是历史里这对 user/assistant 始终成对。
    fn push_error_turn(&mut self, err: &LlmError) {
        self.messages
            .push(Message::assistant(format!("{ERROR_TURN_PREFIX}{err}")));
    }

    /// 跑一个完整回合：追加用户消息 → 反复「问模型 → 执行工具」→ 返回最终答复。
    /// **不落盘会话**（由调用方 `save`）；压缩事件攒在 `compaction_events`，随 `save` 一起落盘。
    ///
    /// 回合语义：工具失败文本化后照常回传、达到 `max_steps` 就把
    /// 最后一段 assistant 文本当答复（不额外追加消息）、`tool` 结果与 `tool_call_id` 严格配对。
    ///
    /// 按次（不在配置里）的执行旋钮：
    ///   - `max_steps`：单回合最多问几次模型；`None` = 不限。
    ///   - `stream`：`None` = 默认（客户端都实现了 `stream()` → 流式）；`Some(false)` 强制一次性
    ///     `complete()`（`on_event` 不再有增量，只推一次 `Answer`）。
    ///   - `parallel_tools`：同一批 `tool_calls` 是否**并发**执行；`None` = 跟随 `config.parallel_tools`
    ///     （默认 true）。工具共享可变状态时必须 `Some(false)`（改为按模型返回顺序串行）。
    ///   - `request_options`：本回合模型请求的**按次覆盖**（见 [`RequestOptions`]）——思考深度
    ///     （`None` = 用客户端自身的 / 配置值；空串 / `none` = 关闭思考）与 `response_format`
    ///     （`Text` 默认 = 不发该字段；`JsonObject` = 要求合法 JSON，⚠ 还得自己在 prompt 里交代）。
    ///     **只影响本回合的请求**：不改配置、不写回文件。
    ///
    /// `cancel` 触发时（TUI 的 `Esc`）：
    ///   - 模型请求中的取消 → 直接收尾（不追加 assistant 消息）；
    ///   - 工具执行中的取消 → shell 会杀掉整个进程组，**未执行的 `tool_calls` 补 `CANCEL_TEXT` 的
    ///     tool 消息**（保证每个 `tool_call_id` 都有配对结果、API 序列合法）；
    ///   - 两种收尾都往历史里写一条 `CANCEL_TEXT` 的 assistant 消息，并把它作为本轮答复返回。
    ///
    /// ⚠ 参数 8 个（clippy 会念）是故意的：全是**按次**旋钮 —— `--max-steps` / `--no-stream` /
    /// 按次请求覆盖都不进 `Config`（见 AGENTS.md「执行旋钮按次传」），打包成结构体只是把字段挪个地方。
    #[allow(clippy::too_many_arguments)]
    pub async fn aturn(
        &mut self,
        input: &str,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancel: &Cancel,
        max_steps: Option<usize>,
        stream: Option<bool>,
        parallel_tools: Option<bool>,
        request_options: RequestOptions<'_>,
    ) -> Result<String, LlmError> {
        // 每轮开头：先收解释器代码块（压缩会把 `tool_calls` 删掉，这是最后的收集机会），
        // 再校准 system prompt 末尾那节（cwd 可能被 `/cd` 改过）——都必须先于任何请求
        self.collect_repl_blocks();
        self.refresh_runtime_state();
        self.push_user(input);
        // 转录快照（解释器的 `history()` 读它）——每轮重写，于是会话进行中也能查到历史
        self.write_transcript();
        // 压缩事件先攒在本地（`self.messages` 这会儿正被借出去跑回合）→ 回合末并进字段
        let mut compacted: Vec<context::CompactEvent> = Vec::new();

        // 配置值拷出来（不长期借 `self.config`：后面还要 `&mut self` 做注入/降级）
        // `stream: None` = 用默认（客户端都实现了流式）
        let use_stream = stream.unwrap_or(true);
        // `parallel_tools: None` = 跟随配置
        let use_parallel = parallel_tools.unwrap_or(self.config.parallel_tools);
        let mut steps = 0usize;
        let mut answer: Option<String> = None;
        let mut done = false;
        // 「思考」计时：见 [`ThoughtClock`]（量到的毫秒数会写进本回合的 assistant 消息）
        let mut clock = ThoughtClock::default();
        let mut cancelled = false;
        loop {
            // 取消检查点（每步开头；请求/工具内部的 race 另算）
            if cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            if let Some(max) = max_steps
                && steps >= max
            {
                // 达到步数上限：不再问模型，把历史里最后一段非空 assistant 文本当最终答复
                answer = self.messages.iter().rev().find_map(|m| match &m.content {
                    Some(Content::Text(t)) if m.role == "assistant" && !t.trim().is_empty() => {
                        Some(t.clone())
                    }
                    _ => None,
                });
                if !use_stream {
                    on_event(TurnEvent::Answer(answer.clone().unwrap_or_default()));
                }
                done = true;
                break;
            }
            steps += 1;

            // 请求前：拿上一次 API 上报的水位判一次（本次会话还没发过请求 → 不压）
            let (stats, mut events) =
                context::maybe_compact(&mut self.messages, &self.config, self.usage.prompt_tokens);
            compacted.append(&mut events);
            self.refresh_after_compaction(&stats);

            let specs = self.tools.specs();
            let called = match self
                .model_call(
                    &specs,
                    on_event,
                    &mut clock,
                    cancel,
                    use_stream,
                    request_options,
                )
                .await
            {
                Ok(option) => option,
                Err(e) => {
                    // file_id 失效（服务端删了 / 中途换了 key）：把历史里的图片块降级成文本占位、
                    // 记录标失效（下次同图重传），再试一次——不然整个回合 400 报废。
                    if e.is_stale_file_error() && self.downgrade_file_blocks() {
                        crate::log::warn(format!(
                            "[warn] file_id 已失效，已把历史里的图片降级为占位文本并重试：{e}"
                        ));
                        match self
                            .model_call(
                                &specs,
                                on_event,
                                &mut clock,
                                cancel,
                                use_stream,
                                request_options,
                            )
                            .await
                        {
                            Ok(option) => option,
                            Err(e) => {
                                self.push_error_turn(&e);
                                return Err(e);
                            }
                        }
                    } else if e.is_context_overflow() {
                        // 水位判据失手（本次会话还没上报过 / 单条输入就撑满窗口）：
                        // **无视水位**强压一次（工具级 + 轮次级）再发——手动 `compact` 正是不看水位那条路。
                        let (stats, mut events) = context::compact(
                            &mut self.messages,
                            &self.config,
                            context::CompactMode::Auto,
                        );
                        compacted.append(&mut events);
                        if stats.tools == 0 && stats.turns == 0 {
                            // 工具级 / 轮次级都压不动了 → **不自动归档**：换窗口是用户的动作，
                            // 自动做会让模型在回合中途莫名「失忆」（当前轮的工作记忆只剩摘要）
                            crate::log::warn(
                                "[warn] 上下文超限且已无可压：可以 `/clear` 开新窗口，或调小单条工具输出上限",
                            );
                            self.push_error_turn(&e);
                            return Err(e);
                        }
                        crate::log::warn(format!(
                            "[warn] 上下文超限，已强制压缩（工具级 {} 条 / 轮次级 {} 轮）并重试一次",
                            stats.tools, stats.turns
                        ));
                        match self
                            .model_call(
                                &specs,
                                on_event,
                                &mut clock,
                                cancel,
                                use_stream,
                                request_options,
                            )
                            .await
                        {
                            Ok(option) => option,
                            Err(e) => {
                                self.push_error_turn(&e);
                                return Err(e);
                            }
                        }
                    } else {
                        // 放弃前把错误写进历史（否则这条 user 就没人应答了，见 `push_error_turn`）
                        self.push_error_turn(&e);
                        return Err(e);
                    }
                }
            };
            let Some(result) = called else {
                cancelled = true; // 模型请求被取消：不 push 任何消息
                break;
            };

            let tool_calls = result.tool_calls.clone();
            self.usage.record(&result.usage);
            // 请求后：provider 上报的 `prompt_tokens` 是最准的水位（上下文只增不减），拿它再判一次
            let (stats, mut events) =
                context::maybe_compact(&mut self.messages, &self.config, self.usage.prompt_tokens);
            compacted.append(&mut events);
            self.refresh_after_compaction(&stats);
            self.messages.push(Message {
                role: "assistant".into(),
                content: result
                    .content
                    .as_deref()
                    .map(|c| Content::Text(c.to_string())),
                tool_calls: (!tool_calls.is_empty()).then(|| tool_calls.clone()),
                reasoning_content: result.reasoning_content.clone(),
                thought_ms: clock.take_ms(), // 本次「思考」时长（None = 这轮模型没给 reasoning）
                ..Default::default()
            });

            if tool_calls.is_empty() {
                answer = result.content;
                if !use_stream {
                    // 非流式：正文从没被推过 → 用 answer 事件交出去（流式下已增量推过）
                    on_event(TurnEvent::Answer(answer.clone().unwrap_or_default()));
                }
                done = true;
                break;
            }

            // —— 一批工具调用：并发（默认）或按模型返回顺序串行
            let outcomes = self
                .tool_call(&tool_calls, use_parallel, cancel, on_event)
                .await;
            // 全部收尾后**按原顺序**回填 ToolMessage（并发/串行都是这个顺序 → 历史扁平序列一致，
            // compaction 的 step 批次 / keep_last_steps 认定不受影响）
            let mut interrupted = false;
            // `read` 读到图的那几条：记下**消息下标**，循环外统一上传并挂进那条 tool 消息
            //（上传是 async；拿不到 `file_id` 就什么都不改 = 天然降级）
            let mut pending_attachments: Vec<(usize, Attachment)> = Vec::new();
            for (call, outcome) in tool_calls.iter().zip(outcomes) {
                let out = match outcome {
                    Some(out) => out,
                    // 被取消：每个 tool_call_id 都要有配对的 tool 消息，否则回放这段历史时
                    // API 会拒）；结果事件已在 `tool_call` 里推过
                    None => {
                        interrupted = true;
                        ToolOutput::text(CANCEL_TEXT)
                    }
                };
                let name = call.function.name.as_str();
                // 工具自带落盘（bash / repl / pytool 超限）→ 消息**进历史前**就带上原文指针（工具级），
                // 并同步一条压缩流水（gc 靠它保住那份落盘原文）。
                // ⚠ 只有真落了盘才设：`compact_tools` 靠 `compaction.is_some()` 跳过「已压过」的
                // 消息，没落盘却设一段元数据会让工具级压缩永远压不动（曾经就是无条件标）。
                let mut msg = Message::tool_result(&call.id, name, out.to_text());
                if let Some(path) = &out.body.spill {
                    compacted.push(context::CompactEvent::tool(name, path));
                    msg.compaction = Some(Compaction::tool(path));
                }
                let msg_index = self.messages.len();
                self.messages.push(msg);
                // 要送给模型的图（`Attachment::Model`）——先记下标，循环外挂（见 `attach_read_images`）
                if let Some(att) = out
                    .attachments
                    .iter()
                    .find(|a| matches!(a, Attachment::Model { .. }))
                {
                    pending_attachments.push((msg_index, att.clone()));
                }
                // `repl` 出的图登记进 `__meta__.files`（本地副本保命 + `calls` 供 resume 配对）
                self.record_repl_images(call, &out);
            }
            if interrupted {
                cancelled = true;
                break;
            }
            // 本批 `read` 读到的图：挂到**那条 tool 消息**的 content 上（`text` + `file` 两个 part）
            self.attach_read_images(&pending_attachments).await;
        }

        // 会话级压缩（level 3）产出的窗口块 → 登记进 `self.windows`
        self.windows
            .extend(compacted.iter().filter_map(|e| match e {
                context::CompactEvent::Session { path, .. } => Some(path.clone()),
                _ => None,
            }));
        self.compaction_events.append(&mut compacted);

        if cancelled {
            // 历史里留一条终止消息，并把它当本轮答复
            self.messages.push(Message::assistant(CANCEL_TEXT));
            if !use_stream {
                on_event(TurnEvent::Answer(CANCEL_TEXT.to_string()));
            }
            return Ok(CANCEL_TEXT.to_string());
        }
        debug_assert!(done, "回合既没正常结束也没取消");
        Ok(answer.unwrap_or_default())
    }

    /// 一次模型调用（流式 / 非流式两条路），增量经 `on_event` 推；`stream` / `request_options` 由 `aturn`
    /// 按次传进来。
    /// 一次模型调用；`Ok(None)` = 请求期间被取消（不 push 任何消息，收尾由 `aturn` 统一做）。
    async fn model_call(
        &self,
        specs: &[Value],
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        clock: &mut ThoughtClock,
        cancel: &Cancel,
        stream: bool,
        request_options: RequestOptions<'_>,
    ) -> Result<Option<LlmResult>, LlmError> {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let call = async {
            if stream {
                self.llm
                    .stream(
                        &self.messages,
                        specs,
                        request_options,
                        |chunk| match chunk {
                            // 先喂计时器（它是 `thought_ms` 的唯一来源），再交给调用方
                            StreamChunk::Content(d) => {
                                let ev = TurnEvent::AssistantText(d);
                                clock.on(&ev);
                                on_event(ev);
                            }
                            StreamChunk::Reasoning(d) => {
                                let ev = TurnEvent::Reasoning(d);
                                clock.on(&ev);
                                on_event(ev);
                            }
                            StreamChunk::ToolCall { .. } => {}
                        },
                    )
                    .await
            } else {
                self.llm
                    .complete(&self.messages, specs, request_options)
                    .await
            }
        };
        tokio::select! {
            result = call => result.map(Some),
            _ = cancel.cancelled() => Ok(None),
        }
    }

    /// 把本批 `read` 读到的图挂到**那条 tool 消息**的 content 上：
    /// `[{type:text, 原正文}, {type:file, file_id}]`。
    ///
    /// **只走 Files API**：拿不到 `file_id`（未开启 / 模型不支持 / 上传失败）就**什么都不改**——
    /// 消息保持纯文本，正文里那行 `[图片已读取: …]` 仍在，模型知道有这张图；本地副本与记录也会留下，
    /// 下次同图直接命中不再重传。**不回退内联 base64**。
    ///
    /// 为什么挂 tool 消息而不是另推一条 user 消息：官方文档里 tool 消息的 content 就是
    /// `string | content parts`（text / image_url / file 三档，2026-10-08 逐档实测过——见
    /// docs/CHANGELOG.md 那条探测），图本来就是**工具产出**的，挂在工具结果上语义更正，
    /// 还省掉一条消息（而且 `synthetic` 那条特殊路径再也不用走）。
    async fn attach_read_images(&mut self, pending: &[(usize, Attachment)]) {
        for (idx, att) in pending {
            let Attachment::Model {
                path, mime, size, ..
            } = att
            else {
                continue;
            };
            let Ok(data) = std::fs::read(path) else {
                continue; // 文件没了 → 正文那行说明仍在，不硬塞
            };
            if *size > 0 && data.len() as u64 != *size {
                continue; // 文件被换过（大小对不上）
            }
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let src = path.to_string_lossy().into_owned();
            let file_id = self.ensure_image_file(&data, mime, &filename, &src).await;
            let Some(file_id) = file_id else {
                continue;
            };
            // 把那条第 tool 消息的纯文本正文换成 parts：文本 + 文件（拿不到就维持纯文本）
            let Some(msg) = self.messages.get_mut(*idx) else {
                continue;
            };
            let Some(Content::Text(text)) = msg.content.take() else {
                continue;
            };
            msg.content = Some(Content::Parts(vec![
                json!({"type": "text", "text": text}),
                json!({"type": "file", "file_id": file_id}),
            ]));
        }
    }

    /// 执行**一批** `tool_call`（`model_call` 的姊妹：那边问模型，这边跑工具）。
    ///
    /// `parallel` = 同一批是否并发：
    ///   - `true`：它们同时开跑，谁先跑完谁先出结果（工具共享可变状态时**不能**用）；
    ///   - `false`：按模型返回顺序一个个来（`buffer_unordered(1)` = 严格串行）。
    ///
    /// 两条路的**事件形状一致**：先把整批 `tool_call` 发出去（并发时事件只能先统一发；串行
    /// 也保持同序，嵌入方不用分情况处理），再按「谁先跑完谁先发」推 `tool_result`——被取消的
    /// 那条推 `CANCEL_TEXT`。
    ///
    /// 返回与 `calls` **等长同序**的文本（`None` = 该调用被取消）——回填消息时
    /// 才能保证历史扁平序列与串行一致（compaction 的 step 批次 / `keep_last_steps` 都看它），
    /// 由调用方统一补 `CANCEL_TEXT`。
    ///
    /// 工具输出的文本**原样**交出去（不在这里截断）：要少显示是展示层（TUI / CLI）的事，
    /// 要少回传给模型是工具自己配容量上限（`shell` / `read`）的事。
    ///
    /// 参数非法 JSON **不执行工具**、只把原文回给模型；单个工具失败文本化后照常返回，
    /// 不拖累同批其他工具。
    async fn tool_call(
        &self,
        calls: &[ToolCall],
        parallel: bool,
        cancel: &Cancel,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Vec<Option<ToolOutput>> {
        for call in calls {
            on_event(TurnEvent::ToolCall {
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
            });
        }
        // ⚠ `on_event` **不能**进 future：`&mut dyn FnMut` 没实现 `Sync`，一旦被 future 捕获，
        // `aturn` 的 future 就不再是 `Send`，TUI 那边的 `tokio::spawn` 直接编不过。
        // 所以 future 只负责算出结果，事件在下面这个循环里「完成的当下」推。
        //
        // ⚠ future 必须在 `for` 里造（而不是 `map(|(i, call)| async move {…})`）：
        // 闭包参数的生命周期会变成 HRTB，撞上 rustc 的已知限制（#100013，报在调用方
        // `tokio::spawn` 上一头雾水）。`for` 循环里的绑定没有这层问题。
        let registry = &self.tools;
        let mut futures = Vec::with_capacity(calls.len());
        for (index, call) in calls.iter().enumerate() {
            futures.push(async move {
                let outcome = if cancel.is_cancelled() {
                    None // 还没轮到就取消了
                } else {
                    match serde_json::from_str::<Value>(&call.function.arguments) {
                        // 比 serde 的英文报错有用：把原文回给模型（上限 500 字）
                        Err(_) => Some(ToolOutput::text(format!(
                            "[参数解析失败] 模型返回了非法 JSON: {}",
                            call.function
                                .arguments
                                .chars()
                                .take(500)
                                .collect::<String>()
                        ))),
                        Ok(args) => {
                            // 取消信号与数据目录随 ctx 进工具层（shell 会在等待时 race 它、
                            // 也可能要把 stdout 落盘）；**单个工具失败不拖累其他工具**——
                            // 任何异常都文本化后回传模型，让它自己修。
                            let ctx = tools::ToolCtx::with_cancel(
                                cancel.clone(),
                                self.config.storage.clone(),
                                self.tool_state.clone(),
                            )
                            .with_transcript(self.transcript_path())
                            // 工具名 → `ToolBody::fit` 的落盘前缀（`ctx.name`）
                            .named(&call.function.name);
                            let out = match registry.dispatch(&call.function.name, &args, ctx).await
                            {
                                Ok(out) => out,
                                Err(e) => ToolOutput::text(format!("[工具错误] {e}")),
                            };
                            (out.to_text() != CANCEL_TEXT).then_some(out) // shell 被杀 → 哨兵 → 算取消
                        }
                    }
                };
                (index, outcome)
            });
        }
        // 并发度 = 整批（并发）或 1（串行）；`buffer_unordered` 只决定「同时 poll 几个」，
        // 所以串行时后面的 future 根本不会被 poll —— 与手写一个个 await 等价。
        let limit = if parallel { calls.len().max(1) } else { 1 };
        let mut stream = stream::iter(futures).buffer_unordered(limit);

        let mut outcomes: Vec<Option<ToolOutput>> = vec![None; calls.len()];
        while let Some((index, outcome)) = stream.next().await {
            // 文本**原样**推给嵌入方（不在这里截断）：要少显示是展示层的事（TUI 按行截、
            // CLI 只取首行），要少回传给模型是工具自己配容量上限的事。
            let finished = outcome
                .clone()
                .unwrap_or_else(|| ToolOutput::text(CANCEL_TEXT));
            on_event(TurnEvent::ToolResult {
                name: calls[index].function.name.clone(),
                content: finished.to_text().clone(),
                arguments: calls[index].function.arguments.clone(),
                images: finished.canvas_images(),
            });
            outcomes[index] = outcome;
        }
        outcomes
    }

    /// 把历史里的 `file` 块就地换成文本占位，并把对应记录标失效（下次同图重传）。
    ///
    /// ⚠ 降级时不回退内联 base64 —— 代价是这次
    /// 请求里模型看不到那张图（但回合能继续跑，比整个请求 400 报废强），本地副本还在。
    fn downgrade_file_blocks(&mut self) -> bool {
        let mut changed = false;
        let mut stale_ids: Vec<String> = Vec::new();
        for msg in &mut self.messages {
            let Some(Content::Parts(parts)) = &mut msg.content else {
                continue;
            };
            for part in parts.iter_mut() {
                if part.get("type").and_then(Value::as_str) != Some("file") {
                    continue;
                }
                if let Some(id) = part.get("file_id").and_then(Value::as_str) {
                    stale_ids.push(id.to_string());
                }
                *part = json!({
                    "type": "text",
                    "text": "[图片已失效（file_id 不再可用），需要时重新 read 该文件]",
                });
                changed = true;
            }
        }
        if changed {
            for id in &stale_ids {
                self.invalidate_file(id);
            }
        }
        changed
    }

    /// 登记 `repl` 产出的图：写进 `__meta__.files`（`local` 保命 + `calls` 供 resume 配对）。
    ///
    /// 为什么必须登记：图落地后是 `files/img-<hash>.png`，而 `files gc` 的判据是「未被任何会话
    /// 的 `__meta__.files[*].local` 引用 + 放置超过 `GC_PROTECT_HOURS`」——不登记就活不过一天。
    /// `calls` 则是 `Repl::from_messages` 把图配回那次调用的唯一线索（`hash → entry` 本身没有
    /// 调用维度）。
    ///
    /// 与 `read` 那条路（[`Self::ensure_image_file`]）共用同一张表（同内容 = 同 hash），所以
    /// **按字段合并**：已有的 `file_id` / `base_url` 等照旧留着，只补自己这几项。
    fn record_repl_images(&mut self, call: &ToolCall, out: &ToolOutput) {
        // 只认 `Canvas` 那批（去向在产出时就定了，不必再看工具名）
        for att in &out.attachments {
            let Attachment::Canvas { path } = att else {
                continue;
            };
            if !path.is_file() {
                continue;
            }
            let hash_id = config::name_of(path);
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let entry = self
                .files
                .entry(hash_id.clone())
                .or_insert_with(|| json!({}));
            entry["hash_id"] = json!(hash_id);
            entry["size"] = json!(size);
            entry["mime"] = json!("image/png");
            entry["local"] = json!(path.display().to_string());
            if entry.get("filename").is_none() {
                entry["filename"] = json!(filename);
            }
            if entry.get("src").is_none() {
                entry["src"] = json!("repl");
            }
            // `calls`：产出过这张图的调用（去重追加）——resume 就靠它把图配回画布那条记录
            let mut calls = entry
                .get("calls")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if !calls.iter().any(|v| v.as_str() == Some(call.id.as_str())) {
                calls.push(json!(call.id));
            }
            entry["calls"] = json!(calls);
        }
    }

    /// 保证这张图有一个可用的 `file_id`：先落本地副本 → 命中可用记录就复用，否则上传。
    ///
    /// 未开启（`files_api = false` / 模型不支持）或上传失败 → `None`，调用方就不注入图片
    /// （**不回退内联 base64**）。记录就地写进 `self.files`（下次 `save` 带上）。
    async fn ensure_image_file(
        &mut self,
        data: &[u8],
        mime: &str,
        filename: &str,
        src: &str,
    ) -> Option<String> {
        let (enabled, base_url, key_fp, ttl_days) = {
            let config = &self.config;
            (
                config.files_api && llm::model_supports_files(&config.model),
                config.base_url.trim_end_matches('/').to_string(),
                llm::key_fingerprint(&config.api_key),
                config.files_ttl_days as i64,
            )
        };
        if !enabled {
            return None;
        }
        let local = match self
            .config
            .storage
            .store(config::StoreType::Blob { data, mime })
        {
            Ok(path) => path,
            Err(e) => {
                crate::log::warn(format!("[warn] 图片本地副本写入失败: {e}"));
                return None;
            }
        };
        // 文件名主干就是 id（`img-<hash>`）——扫描 `files/` 时也是这么反推的
        let image_hash = config::name_of(&local);
        if let Some(entry) = self.files.get(&image_hash)
            && entry_is_usable(entry, &base_url, &key_fp)
        {
            return entry
                .get("file_id")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        let uploaded = match self
            .llm
            .upload_file(data.to_vec(), filename, mime, ttl_days)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                crate::log::warn(format!("[warn] 图片上传失败（不回退内联 base64）: {e}"));
                return None;
            }
        };
        // **按字段合并**：同 hash 可能已被 `repl` 那条路登记过（`calls` / `src`）——整条覆盖
        // 会把那些抹掉（下次 `repl` 出的同张图就配不回画布了）。这里只动上传相关的字段。
        let entry = self
            .files
            .entry(image_hash.clone())
            .or_insert_with(|| json!({}));
        entry["hash_id"] = json!(image_hash);
        entry["size"] = json!(data.len());
        entry["mime"] = json!(mime);
        entry["filename"] = json!(filename);
        entry["src"] = json!(src);
        entry["local"] = json!(local.display().to_string());
        entry["file_id"] = json!(uploaded.id);
        entry["base_url"] = json!(base_url);
        entry["key_fp"] = json!(key_fp);
        entry["uploaded_at"] = json!(config::now().as_secs() as i64);
        entry["expires_at"] = json!(uploaded.expires_at);
        Some(uploaded.id)
    }

    /// 把某个 `file_id` 标成失效（服务端删了 / 换了 key）：下次同图重新上传。
    fn invalidate_file(&mut self, file_id: &str) {
        for entry in self.files.values_mut() {
            if entry.get("file_id").and_then(Value::as_str) == Some(file_id) {
                entry["expires_at"] = json!(0);
            }
        }
    }

    /// 整文件重写会话（历史不长，简单可靠）。
    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .map_err(|e| format!("创建会话目录失败 {}: {e}", parent.display()))?;
        }
        let mut meta = json!({
            "__meta__": true,
            "usage": self.usage,
            "windows": self
                .windows
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
            "cwd": std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        });
        if let Some(t) = &self.title {
            meta["title"] = json!(t);
        }
        if !self.files.is_empty() {
            // 空表不写：别把每个会话文件都撑起来
            meta["files"] = json!(self.files);
        }
        if !self.compaction_events.is_empty() {
            meta["compaction_events"] = json!(self.compaction_events);
        }
        if !self.repl_blocks.is_empty() {
            // 空表不写：没跑过 repl 的会话不该多一个键
            meta["repl_blocks"] = json!(self.repl_blocks);
        }
        let mut out = String::new();
        out.push_str(&json_line(&meta)?);
        out.push('\n');
        for m in &self.messages {
            out.push_str(&json_line(m)?);
            out.push('\n');
        }
        fs::write(&self.path, out).map_err(|e| format!("写入会话失败 {}: {e}", self.path.display()))
    }

    /// 恢复 / 新建后的概览（CLI 提示用）。
    pub fn summary(&self) -> String {
        format!("{} 轮历史，{} 次请求", self.turn_count, self.usage.calls)
    }

    /// 手动压缩（`/compact`）：不看水位，按 `mode` 压；轮次级一路压到不能再压。
    /// **不含会话级**（整窗口归档是 `/clear` 的事）。
    pub fn compact(&mut self, mode: context::CompactMode) -> context::CompactStats {
        let (stats, events) = context::compact(&mut self.messages, &self.config, mode);
        self.compaction_events.extend(events);
        stats
    }

    /// 完整转录：按消息顺序**展开压缩指针**——工具级还原落盘全文，轮次级 / 会话级还原原始消息序列。
    ///
    /// 给 TUI resume 回放用：会话里存的是「摘要 + 指针」，照着回放看着没头没尾；展开才是当初界面上
    /// 真正出现过的内容。落盘文件不在（被 `context gc` 收走）
    /// 就退回压缩形式本身。
    ///
    /// 不再把压缩流水里「已不在消息中」的原文追加到末尾——那些原文要么仍在消息
    /// 里（各自展开）、要么属于已归档的窗口块（走 `windows` 重建的摘要消息），另追加一遍只会让回放
    /// 顺序错乱。
    pub fn full_history(&self) -> Vec<Message> {
        let mut out: Vec<Message> = Vec::new();
        for m in &self.messages {
            let raw = m
                .compaction
                .as_ref()
                .and_then(Compaction::path)
                .map(PathBuf::from)
                .filter(|p| p.exists());
            match (m.compaction.as_ref(), raw) {
                // 轮次级 / 会话级：落盘的是整段原文（JSON 数组或 JSONL）
                (Some(Compaction::Turn { .. } | Compaction::Session { .. }), Some(path)) => {
                    let mut raws: Vec<Message> = context::load_window_dicts(&path)
                        .into_iter()
                        .filter_map(|v| serde_json::from_value::<Message>(v).ok())
                        .collect();
                    if matches!(m.compaction, Some(Compaction::Turn { .. }))
                        && raws.first().is_some_and(|r| r.role == "user")
                    {
                        raws.remove(0); // 轮次级：user 留在外层，落盘的是它后面的过程
                    }
                    out.extend(raws);
                }
                // 工具级：落盘的是被截断的那份输出全文（纯文本，不是消息）。
                //
                // 头区（`[exit=N]` / 指针行）不在落盘件里，从压缩后的消息里补回来——否则回放的
                // bash 行看不到退出码，会被当成成功（回放里失败的命令显示 〼 且不带正文）。
                (Some(Compaction::Tool { .. }), Some(path)) if m.role == "tool" => {
                    let pointer = m.content_text();
                    let headers = pointer.split_once("\n\n").map_or(
                        pointer.as_str(), // 只有头区（没空行）时整段都是头
                        |(head, _)| head,
                    );
                    let full = std::fs::read_to_string(&path).unwrap_or_else(|_| pointer.clone());
                    let text = if headers.is_empty() || full.starts_with(headers) {
                        full
                    } else {
                        format!("{headers}\n\n{full}")
                    };
                    out.push(Message::tool_result(
                        m.tool_call_id.clone().unwrap_or_default(),
                        m.tool_name.clone().unwrap_or_default(),
                        text,
                    ));
                }
                _ => out.push(m.clone()),
            }
        }
        out
    }

    // `messages[0]` 的两件维护工作：这里的「运行时状态」与下面的 `/clear` 重建。

    /// 每轮开头校准 `messages[0]` 末尾的「运行时状态」节（cwd 之类）。
    ///
    /// 把最后一次出现的标题之前那段原样留下、尾部重拼 —— 幂等：cwd 没变时结果逐字相同，
    /// 服务端提示词前缀缓存照旧命中（所以不必先比较再赋值）。
    ///
    /// 为什么住 `messages[0]` 而不是历史里的一条 system：见 [`config::runtime_state`]。
    fn refresh_runtime_state(&mut self) {
        // 那节现在有两块：cwd（每轮现取）+ 解释器跑过的代码（**只列已被压缩带走的**）
        let state = config::runtime_state(&self.repl_section());
        let Some(prompt) = self.messages.first_mut() else {
            return;
        };
        let text = prompt.content_text();
        let base = match text.rfind(config::RUNTIME_STATE_HEADING) {
            Some(at) => &text[..at], // 含标题前面那两个空行（原来怎么拼就怎么留）
            None => text.as_str(),   // 旧会话 / 手改过（没节）→ 直接接在末尾
        };
        *prompt = Message::system(format!("{base}{state}"));
    }

    /// 压缩之后把刚消失的代码块补进那节（每次 `maybe_compact` 后调）。
    ///
    /// 全 0 说明什么都没压 → **一个字节都别动**（否则会白打掉前缀缓存）。
    fn refresh_after_compaction(&mut self, stats: &context::CompactStats) {
        if stats.tools > 0 || stats.turns > 0 {
            self.refresh_runtime_state();
        }
    }

    /// 每轮开头把消息里**还没记过**的 `repl` 代码块收进 `repl_blocks`。
    ///
    /// 幂等（按 call id 去重，重复扫不会重复记）。当前这轮不在压缩的受害范围里
    /// （轮次级只压「已完成」的轮，会话级只由用户手动 `/clear`）→ 下一轮再收也来得及。
    fn collect_repl_blocks(&mut self) {
        let seen: HashSet<&str> = self.repl_blocks.iter().map(|b| b.id.as_str()).collect();
        let mut fresh: Vec<ReplBlock> = Vec::new();
        for m in &self.messages {
            for call in m.tool_calls.iter().flatten() {
                if call.function.name != "repl" || seen.contains(call.id.as_str()) {
                    continue;
                }
                let Some(code) = serde_json::from_str::<Value>(&call.function.arguments)
                    .ok()
                    .and_then(|v| v.get("code").and_then(Value::as_str).map(str::to_string))
                else {
                    continue;
                };
                let mut code = code;
                if code.chars().count() > REPL_BLOCK_MAX_CHARS {
                    code = code.chars().take(REPL_BLOCK_MAX_CHARS).collect::<String>();
                    code.push_str("\n# …（本 cell 过长，完整见原文）");
                }
                fresh.push(ReplBlock {
                    id: call.id.clone(),
                    code,
                });
            }
        }
        if fresh.is_empty() {
            return;
        }
        self.repl_blocks.extend(fresh);
        if self.repl_blocks.len() > REPL_BLOCKS_MAX {
            let cut = self.repl_blocks.len() - REPL_BLOCKS_MAX;
            self.repl_blocks.drain(..cut); // 丢最老的
        }
    }

    /// 那节里列什么：**日志里 id 已经不在 `messages` 里的那些** cell（= 被压缩带走的）。
    ///
    /// 于是它有两条白拿的性质：两次压缩之间消息没变 → 返回值**逐字相同**（缓存不受影响）；
    /// 刚被压走东西的那一刻才变 —— 而那正是缓存已经全废的时刻。
    fn repl_section(&self) -> String {
        let live: HashSet<&str> = self
            .messages
            .iter()
            .flat_map(|m| m.tool_calls.iter().flatten())
            .map(|c| c.id.as_str())
            .collect();
        let gone: Vec<&str> = self
            .repl_blocks
            .iter()
            .filter(|b| !live.contains(b.id.as_str()))
            .map(|b| b.code.as_str())
            .collect();
        if gone.is_empty() {
            return String::new();
        }
        // 字符预算跟窗口成比例：小窗口的模型不该被这节挤爆（大窗口够放完 24 个 cell）。
        let mut budget = (self.config.context_budget() / 32).max(2_000);
        let mut start = gone.len();
        for i in (0..gone.len()).rev() {
            let n = gone[i].chars().count();
            if budget < n && start != gone.len() {
                break; // 至少放最近一个；再多放不下了就停
            }
            budget = budget.saturating_sub(n);
            start = i;
        }
        let mut out = format!(
            "解释器里跑过的代码（列了 {} 个 cell，**只列已被压缩带走的** —— 最近跑的那几个 cell 还在上面的对话里）：\n```python\n",
            gone.len() - start
        );
        for (i, code) in gone[start..].iter().enumerate() {
            out.push_str(&format!("# cell {}\n{code}\n\n", start + i + 1));
        }
        out.push_str("```");
        if start > 0 {
            out.push_str(&format!(
                "\n（更早的 {start} 个不列出来了；逐条转录/输出用 `history()`，或看 `[轮次原文已保存: …]` 的原文）"
            ));
        }
        out
    }

    /// 转录快照的路径：**临时目录** + 会话路径的 hash。
    ///
    /// 放临时目录是为了不占数据目录（一次性子 agent 也会写它，不该在 `sessions/` 里留垃圾）；
    /// 带 hash 是为了并行会话互不打架（父 agent 的界面与它派生的子 agent 各读各的）。
    fn transcript_path(&self) -> PathBuf {
        let id = config::hash_id(self.path.to_string_lossy().as_bytes());
        std::env::temp_dir().join(format!("pie-transcript-{id}.jsonl"))
    }

    /// 把当前（**压缩态**）转录写一份快照给解释器 —— `repl` 的 `history()` 读它。
    ///
    /// 为什么不让解释器直接读会话文件：那个文件**只在退出 / `/save` 时**落盘，会话进行中
    /// 它根本还不存在（一次性会话更是永远不落盘）。每轮开头重写一次的代价只是一次内存
    /// 序列化，换来「解释器里随时拿得到完整转录」。写盘走「临时文件 + 改名」，
    /// 读到的一定是完整的一份（不撞上半截 JSON）。
    fn write_transcript(&self) {
        let path = self.transcript_path();
        let mut body = String::new();
        for m in &self.messages {
            match json_line(m) {
                Ok(line) => {
                    body.push_str(&line);
                    body.push('\n');
                }
                Err(e) => crate::log::warn(format!("[转录快照] 序列化失败: {e}")),
            }
        }
        let tmp = path.with_extension("tmp");
        let done = std::fs::write(&tmp, body.as_bytes()).and_then(|_| std::fs::rename(&tmp, &path));
        if let Err(e) = done {
            crate::log::warn(format!("[转录快照] 写入失败 {}: {e}", path.display()));
        }
    }

    /// 只留 system prompt（`/clear` 的一半：换窗口）。
    #[allow(dead_code)] // 入口是交互层的 `/clear`
    pub fn reset(&mut self) {
        self.messages.truncate(1);
    }

    /// `/clear`：把当前窗口（system 与**既有窗口摘要**除外）整块写进 `~/.pie/windows/` 落盘，开新窗口。
    ///
    /// 新窗口 = 重建的 system prompt + **所有**窗口块的「摘要 + 指针」（旧摘要原地留着，
    /// 末尾接上刚归档那一块的新摘要）：可见上下文立刻瘦下来，原文仍在 `~/.pie/windows/`
    /// 可经指针回查（`full_history` / resume 都能展开）。
    ///
    /// 这就是**第三级（会话级）压缩**，只是**只由用户手动触发**（自动压缩只做工具级 / 轮次级）：
    /// 换窗口会让当前轮的工作记忆只剩摘要，自动做会让模型莫名「失忆」。
    ///
    /// 返回归档后手上的窗口块**总数**（调用方拿去提示）；落盘失败则原样返回、**不动窗口**
    ///（宁可不清，也不能把历史弄丢）。
    pub fn clear_window(&mut self) -> std::io::Result<usize> {
        // `[compaction.session]` 没配就用默认值（head/tail 只影响摘要保留几轮）
        let session_config = self
            .config
            .compaction
            .as_ref()
            .and_then(|c| c.session.clone())
            .unwrap_or_default();
        if let Some((path, event)) =
            context::compact_session(&mut self.messages, &session_config, &self.config.storage)?
        {
            self.compaction_events.push(event);
            self.windows.push(path);
        }
        // `/clear` 的特性（兜底没有）：system prompt 顺便重建一次（记忆变化要反映进来；
        // cwd 那节每轮也会被 `refresh_runtime_state` 重拼）
        self.messages[0] = Message::system(config::build_system_prompt(
            &self.config,
            self.config.system_prompt.as_deref(),
            &self.config.append_system_prompt,
        ));
        Ok(self.windows.len())
    }

    /// `/stat` 的报告文本：
    /// 上下文占用（API 上报）/ 水位 / 输入预算 / 各角色条数 / 压缩事件 / API 用量。
    pub fn usage_report(&self) -> String {
        let limit = self.config.context_budget();
        let reserved = match self.config.reserved_tokens {
            Some(n) => config::thousands(n as i64),
            None => "服务端默认".to_string(),
        };
        // 各角色**条数**（不是 token：不做估算，token 只有服务端上报那一个来源）
        let mut roles: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
        for m in &self.messages {
            *roles.entry(m.role.clone()).or_default() += 1;
        }
        let roles_txt = roles
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" | ");

        let mut parts: Vec<String> = Vec::new();
        if self.path.exists() {
            parts.push(format!("会话文件：{}", self.path.display()));
        }
        match self.usage.prompt_tokens {
            Some(reported) => {
                let pct = if limit > 0 {
                    reported as f64 * 100.0 / limit as f64
                } else {
                    0.0
                };
                parts.push(format!(
                    "当前上下文占用（API 上报）：{} / {} tokens ({pct:.1}%)",
                    config::thousands(reported),
                    config::thousands(limit as i64)
                ));
            }
            None => parts.push("当前上下文占用：尚无 API 上报（本次会话还没发过请求）".to_string()),
        }
        parts.push(format!(
            "软阈值 {} ({:.0}%)",
            config::thousands(self.config.soft_limit() as i64),
            self.config.soft_ratio() * 100.0
        ));
        parts.push(format!(
            "输入预算 {} = 上下文窗口 {} − 输出预留 {reserved}",
            config::thousands(limit as i64),
            config::thousands(self.config.context_window as i64)
        ));
        parts.push(format!("各角色条数：{roles_txt}"));

        let mut counts = [0i64; 3];
        let mut evicted: i64 = 0;
        for e in &self.compaction_events {
            match e.level() {
                1 => counts[0] += 1,
                2 => counts[1] += 1,
                3 => counts[2] += 1,
                _ => {}
            }
            if let Ok(meta) = std::fs::metadata(e.raw_path()) {
                evicted += meta.len() as i64;
            }
        }
        parts.push(format!(
            "上下文压缩：{} / {} / {} (工具级 / 轮次级 / 会话级)",
            counts[0], counts[1], counts[2]
        ));
        parts.push(format!(
            "本会话已压缩 {} 次 (当前为压缩视图)，落盘原文 {} 字节 (可经指针恢复)",
            self.compaction_events.len(),
            config::thousands(evicted)
        ));
        parts.push(format!(
            "API 用量：\n{}",
            serde_json::to_string_pretty(&self.usage).unwrap_or_default()
        ));
        parts.join("\n")
    }

    fn at(path: PathBuf, config: &Config, llm: LlmClient, tools: ToolRegistry) -> Self {
        Self {
            path,
            config: config.clone(),
            llm,
            tools,
            tool_state: std::sync::Arc::new(crate::tools::SessionState::default()),
            compaction_events: Vec::new(),
            messages: vec![Message::system(config::build_system_prompt(
                config,
                config.system_prompt.as_deref(),
                &config.append_system_prompt,
            ))],
            usage: UsageTracker::default(),
            windows: Vec::new(),
            repl_blocks: Vec::new(),
            files: HashMap::new(),
            title: None,
            turn_count: 0,
            available_models: None,
        }
    }
}

/// 一条 JSONL 行（落盘共用）：`serde_json` 序列化 + **转义行分隔类控制符**（见下）。
fn json_line<T: serde::Serialize>(value: &T) -> Result<String, String> {
    let json = serde_json::to_string(value).map_err(|e| e.to_string())?;
    Ok(escape_control_chars(json))
}

/// 把 JSON 文本里**裸的** C1 控制符（U+0080–U+009F）与 U+2028/U+2029 换成 `\uXXXX`。
///
/// `serde_json` 只转义 C0（U+0000–U+001F），C1 与两个 Unicode 行分隔符在 JSON 里合法、会原样落盘；
/// 但按「Unicode 行边界」切行的读者（`splitlines()` 那一类、部分编辑器与
/// 日志工具）会把 **U+0085 当换行** → 一条消息被劈成两半、整份 JSONL 读不出来（工具输出里出现这些
/// 字节一点不稀奇：转义序列 dump、二进制预览）。落盘前转义掉，读回来还是同一个字符。
///
/// 替换在整个 JSON 文本上做是安全的：结构部分是纯 ASCII，这些字符只可能出现在字符串字面量里。
fn escape_control_chars(json: String) -> String {
    fn needs_escape(c: char) -> bool {
        matches!(c, '\u{80}'..='\u{9f}' | '\u{2028}' | '\u{2029}')
    }
    if !json.chars().any(needs_escape) {
        return json; // 绝大多数消息没有 → 不重写
    }
    let mut out = String::with_capacity(json.len() + 8);
    for c in json.chars() {
        if needs_escape(c) {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------- 图片文件管理
//
// 本地那一侧的事都在这里（`llm.rs` 只放 Files API 协议）：内容寻址副本、`__meta__.files`
// 记录表、本地副本的 GC 清单。目录本身住 `config::Storage`（`sessions()` /
// `files()`）——磁盘布局一处可见。

/// 记录还能不能直接用：同一 key / base_url + 未过期（`expires_at` 缺失 = 服务端永久保留）。
pub fn entry_is_usable(entry: &Value, base_url: &str, key_fp: &str) -> bool {
    if entry.get("file_id").and_then(Value::as_str).is_none() {
        return false;
    }
    if entry.get("base_url").and_then(Value::as_str) != Some(base_url) {
        return false;
    }
    if entry.get("key_fp").and_then(Value::as_str) != Some(key_fp) {
        return false;
    }
    let Some(expires_at) = entry.get("expires_at").and_then(Value::as_f64) else {
        return true; // 未记有效期 = 永久
    };
    config::now().as_secs_f64() < expires_at - 60.0 // 留 1 分钟余量，别卡在过期边缘
}

/// `id` 解析：None → 时间戳文件名；纯名字 → `<sessions>/<name>.jsonl`；带目录/绝对路径 → 原样。
fn resolve_path(dir: &Path, id: Option<&str>) -> PathBuf {
    match id {
        None => dir.join(format!("chat-{}.jsonl", timestamp())),
        Some(id) => {
            let p = Path::new(id);
            let is_path = p.is_absolute() || p.parent().is_some_and(|d| !d.as_os_str().is_empty());
            if is_path {
                p.to_path_buf()
            } else {
                let stem = p
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                dir.join(format!("{stem}.jsonl"))
            }
        }
    }
}

/// `<unix 秒>-<微秒 6 位>`：可排序、同秒不撞车。
fn timestamp() -> String {
    let now = config::now();
    format!("{}-{:06}", now.as_secs(), now.subsec_micros())
}

/// 目录里最新的会话文件；`cwd` 匹配的优先（没有 cwd 的旧文件算不匹配）。
fn latest_in(dir: &Path, cwd: Option<&str>) -> Option<PathBuf> {
    let mut files: Vec<(SystemTime, PathBuf)> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| {
            let path = e.path();
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, path))
        })
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0)); // mtime 降序
    if let Some(cwd) = cwd
        && let Some((_, path)) = files
            .iter()
            .find(|(_, p)| meta_cwd(p).as_deref() == Some(cwd))
    {
        return Some(path.clone());
    }
    files.first().map(|(_, p)| p.clone())
}

/// 读会话文件 meta 行里的 `cwd`（旧会话没有该键 → None）。
fn meta_cwd(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let first = text.lines().find(|l| !l.trim().is_empty())?;
    let value: Value = serde_json::from_str(first).ok()?;
    value.get("cwd").and_then(Value::as_str).map(str::to_string)
}

fn first_line(text: &str) -> Option<String> {
    let line = text.trim().lines().next().unwrap_or("").trim();
    (!line.is_empty()).then(|| line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{FunctionCall, Usage};

    /// 测试用数据目录：跟随进程级 `PIE_DIR`（`pie_dir_tmp` + `env_lock` 那套）。
    fn storage() -> config::Storage {
        config::Storage::default()
    }

    /// 落一张图片副本 → 路径（`StoreType::Blob` 的薄包装，只为测试读起来短）。
    fn blob(data: &[u8], mime: &str) -> PathBuf {
        storage()
            .store(config::StoreType::Blob { data, mime })
            .unwrap()
    }

    /// 落一段压缩原文 → 路径（`StoreType::Raw` 的薄包装，仅供测试）。
    fn raw(body: &str, prefix: &str) -> PathBuf {
        storage()
            .store(config::StoreType::Raw { prefix, body })
            .unwrap()
    }

    /// 改进程级 `PIE_DIR` 的用例共用 `config::ENV_LOCK`（跟 `context` 的测试串行化）。
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::config::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn pie_dir_tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pie-img-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::config::set_env("PIE_DIR", &dir);
        dir
    }

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("pie-session-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn usage(prompt: i64) -> Usage {
        Usage {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(2),
            total_tokens: Some(prompt + 2),
            ..Default::default()
        }
    }

    fn llm() -> LlmClient {
        LlmClient::new(&Config::default()).expect("client")
    }

    fn tools() -> ToolRegistry {
        ToolRegistry::new(Default::default())
    }

    /// 跑一批工具的会话（不落盘、不碰 `~/.pie`）：`tool_call` 只需要一个 `&Session`。
    fn session() -> Session {
        Session::ephemeral(&Config::default(), llm(), tools())
    }

    // ---------------------------------------------------------------- 一批工具的并发/串行

    fn shell_call(id: &str, command: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "bash".into(),
                arguments: format!("{{\"command\": {command:?}}}"),
            },
        }
    }

    /// `tool_call` 的结果 → 可断言的文本（`None` = 被取消）。
    fn texts(outcomes: &[Option<ToolOutput>]) -> Vec<String> {
        outcomes
            .iter()
            .map(|o| {
                o.clone()
                    .map(|o| o.to_text())
                    .unwrap_or_else(|| "[cancelled]".into())
            })
            .collect()
    }

    fn event_kinds(events: &[TurnEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                TurnEvent::ToolCall { .. } => "call",
                TurnEvent::ToolResult { .. } => "result",
                _ => "other",
            })
            .collect()
    }

    /// 并发执行：三个工具（两个 0.6s + 一个 0.05s）总耗时 ≈ 最慢那个（串行要 1.25s+）。
    /// 同时验证：结果**按调用顺序**返回（快的先跑完也不抢位），
    /// 事件形状是「先全部 tool_call、再按完成顺序 tool_result」。
    #[test]
    fn parallel_batch_overlaps_and_keeps_call_order() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let calls = vec![
                shell_call("c1", "sleep 0.6; echo one"),
                shell_call("c2", "sleep 0.6; echo two"),
                shell_call("c3", "sleep 0.05; echo three"),
            ];
            let mut events: Vec<TurnEvent> = Vec::new();
            let started = std::time::Instant::now();
            {
                let mut sink = |e: TurnEvent| events.push(e);
                let outcomes = session()
                    .tool_call(&calls, true, &Cancel::new(), &mut sink)
                    .await;
                // 结果与入参等长同序：慢的那两条仍占下标 0 / 1
                let texts = texts(&outcomes);
                assert!(texts[0].contains("one"), "{texts:?}");
                assert!(texts[1].contains("two"), "{texts:?}");
                assert!(texts[2].contains("three"), "{texts:?}");
            }
            let elapsed = started.elapsed().as_secs_f64();
            assert!(
                elapsed < 1.1,
                "三个工具应当重叠执行（串行要 1.25s+），实际 {elapsed:.2}s"
            );

            assert_eq!(
                event_kinds(&events),
                ["call", "call", "call", "result", "result", "result"]
            );
            // 完成顺序：最快的（three）先出结果 —— 证明真的并发，而不是串行跑完再补事件
            match &events[3] {
                TurnEvent::ToolResult { content, .. } => {
                    assert!(content.contains("three"), "{content}")
                }
                other => panic!("第 4 个事件该是结果：{other:?}"),
            }
        });
    }

    /// 串行：同样的两个 0.6s 工具按调用顺序一个个跑（总耗时是两个之和），
    /// 结果事件也按调用顺序。
    #[test]
    fn serial_batch_runs_in_call_order() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let calls = vec![
                shell_call("c1", "sleep 0.6; echo one"),
                shell_call("c2", "sleep 0.6; echo two"),
            ];
            let mut events: Vec<TurnEvent> = Vec::new();
            let started = std::time::Instant::now();
            {
                let mut sink = |e: TurnEvent| events.push(e);
                session()
                    .tool_call(&calls, false, &Cancel::new(), &mut sink)
                    .await;
            }
            let elapsed = started.elapsed().as_secs_f64();
            assert!(elapsed > 1.1, "串行总耗时是两个之和，实际 {elapsed:.2}s");
            assert_eq!(event_kinds(&events), ["call", "call", "result", "result"]);
            match &events[2] {
                TurnEvent::ToolResult { content, .. } => {
                    assert!(content.contains("one"), "{content}")
                }
                other => panic!("第 3 个事件该是结果：{other:?}"),
            }
        });
    }

    /// 参数非法 JSON：**不执行工具**，把原文回给模型（但 `tool_call` / `tool_result` 事件照发）。
    #[test]
    fn invalid_arguments_are_reported_without_running_the_tool() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let calls = vec![ToolCall {
                id: "c1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "bash".into(),
                    arguments: "{不是 JSON".into(),
                },
            }];
            let mut events: Vec<TurnEvent> = Vec::new();
            let mut sink = |e: TurnEvent| events.push(e);
            let outcomes = session()
                .tool_call(&calls, true, &Cancel::new(), &mut sink)
                .await;
            let text = texts(&outcomes)[0].clone();
            assert!(text.starts_with("[参数解析失败]"), "{text}");
            assert!(!text.contains("[exit="), "没执行工具，不该有退出码：{text}");
            assert_eq!(event_kinds(&events), ["call", "result"]);
        });
    }

    /// `/clear`：把当前窗口写进 `~/.pie/windows/`、只留「system + 各窗口块的摘要/指针」，
    /// 而且这段历史**能经 `full_history` 原样展开回来**（归档不能丢信息）。
    #[test]
    fn clear_window_archives_and_expands_back() {
        let _g = env_lock();
        let dir = pie_dir_tmp("clear");
        let mut s = Session::at(dir.join("s.jsonl"), &Config::default(), llm(), tools());
        s.push_user("第一问");
        s.messages.push(Message {
            role: "assistant".into(),
            content: Some(Content::Text("第一答".into())),
            ..Default::default()
        });
        s.push_user("第二问");

        assert_eq!(s.clear_window().expect("归档成功"), 1, "一个窗口块");

        // 新窗口 = system（重建）+ 窗口摘要（会话级，带指针）
        assert_eq!(s.messages.len(), 2, "{:?}", s.messages.len());
        assert_eq!(s.messages[0].role, "system");
        assert!(s.messages[0].compaction.is_none());
        assert!(matches!(
            s.messages[1].compaction,
            Some(Compaction::Session { .. })
        ));
        let block = PathBuf::from(s.messages[1].compaction.as_ref().unwrap().path().unwrap());
        assert!(block.starts_with(storage().windows()), "{block:?}");
        assert!(block.exists(), "{block:?}");
        // 块里是**原文**（三条都在），且文件名最后一段就是内容 hash
        let raw = std::fs::read_to_string(&block).unwrap();
        for needle in ["第一问", "第一答", "第二问"] {
            assert!(raw.contains(needle), "块里丢了 {needle}：{raw}");
        }
        assert_eq!(
            block.file_name().unwrap().to_string_lossy(),
            format!("window-{}", config::hash_of(&block)),
            "窗口块名 = `window-<hash>`（与 `context/`、`files/` 同一套命名）：{block:?}"
        );
        // 内容寻址：同一段内容再归档一次 → 还是那个文件（已存在则不动）
        assert_eq!(
            storage().store(config::StoreType::Window(&raw)).unwrap(),
            block,
            "同内容同路径"
        );
        // 摘要有窗口指针标记 + 首尾轮次
        let text = s.messages[1].content_text();
        assert!(text.contains("[历史窗口:"), "{text}");
        assert!(text.contains("第一问"), "{text}");

        // 压缩流水里记了一条会话级事件（就在 `Session.compaction_events`，随 save 落盘）
        let events: Vec<&context::CompactEvent> = s
            .compaction_events
            .iter()
            .filter(|e| matches!(e, context::CompactEvent::Session { .. }))
            .collect();
        assert_eq!(events.len(), 1, "{:?}", s.compaction_events);
        let (hash, path) = match events[0] {
            context::CompactEvent::Session { hash, path, .. } => (hash, path),
            other => panic!("期望会话级事件：{other:?}"),
        };
        // `raw_hash` 是从落盘路径反推的（`store` 只返回路径）→ 必须等于文件名里那段
        assert_eq!(
            hash,
            &config::hash_of(&block),
            "raw_hash 要跟文件名一致：{:?}",
            s.compaction_events
        );
        assert_eq!(path, &block);

        // save → load：流水跟着 `__meta__` 一起往返（与消息里的指针同一趟车）
        s.save().expect("save");
        // 落盘形状：类型看 `kind` 标签，不再写冗余的 `level`
        let saved = std::fs::read_to_string(&s.path).unwrap();
        let meta = saved.lines().next().unwrap_or_default();
        assert!(meta.contains("\"kind\":\"session\""), "{meta}");
        assert!(!meta.contains("\"level\""), "{meta}");
        let back = Session::load(&s.path, &s.config, s.llm.clone(), s.tools.clone()).expect("load");
        assert_eq!(back.compaction_events.len(), s.compaction_events.len());
        assert_eq!(back.compaction_events[0].raw_path(), events[0].raw_path());

        // 归档的信息一条不少（展开回原样）
        let full = s.full_history();
        let roles: Vec<&str> = full.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"], "{roles:?}");

        // 第二次 /clear：新窗口里没东西可归档 → 块数不变
        assert_eq!(s.clear_window().expect("再来一次"), 1);
        assert_eq!(s.windows.len(), 1);
    }

    /// 工具输出**原样**推给嵌入方：Session 不在这里截断——少显示是展示层的事（TUI 按
    /// `TOOL_BODY_LINES` 截、CLI 只取首行），
    /// 少回传给模型是工具自己配容量上限的事。
    #[test]
    fn tool_result_event_keeps_the_full_output() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            // `seq 1 300` → 1000+ 字，远超 500
            let calls = vec![shell_call("c1", "seq 1 300")];
            let mut events: Vec<TurnEvent> = Vec::new();
            let mut sink = |e: TurnEvent| events.push(e);
            let outcomes = session()
                .tool_call(&calls, true, &Cancel::new(), &mut sink)
                .await;
            let text = outcomes[0].clone().expect("跑成功了");
            assert!(
                text.to_text().len() > 500,
                "工具输出本来就很长：{} 字",
                text.to_text().len()
            );
            match &events[1] {
                TurnEvent::ToolResult { content, .. } => {
                    assert_eq!(content, &text.to_text(), "事件里的文本要原样（不截断）")
                }
                other => panic!("第 2 个事件该是结果：{other:?}"),
            }
        });
    }

    /// 整批开始前就已被取消：一条都不执行（全 `None`），但仍各推一条 `CANCEL_TEXT` 结果事件。
    #[test]
    fn cancelled_batch_skips_every_tool() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let calls = vec![shell_call("c1", "echo hi"), shell_call("c2", "echo hi")];
            let cancel = Cancel::new();
            cancel.cancel();
            let mut events: Vec<TurnEvent> = Vec::new();
            let mut sink = |e: TurnEvent| events.push(e);
            let outcomes = session().tool_call(&calls, true, &cancel, &mut sink).await;
            assert!(outcomes.iter().all(|o| o.is_none()));
            // 被取消的也要各推一条 `CANCEL_TEXT` 结果事件（每个 `tool_call_id` 都要有交代）
            assert_eq!(event_kinds(&events), ["call", "call", "result", "result"]);
            for event in &events[2..] {
                match event {
                    TurnEvent::ToolResult { content, .. } => {
                        assert_eq!(content.as_str(), CANCEL_TEXT)
                    }
                    other => panic!("第 3/4 个事件该是结果：{other:?}"),
                }
            }
        });
    }

    /// save → load 往返：消息（去掉旧 system 后重建）、title、turn_count、usage 都要活下来。
    #[test]
    fn downgrade_replaces_file_blocks_and_invalidates() {
        let config = Config::default();
        let path = tmp("downgrade.jsonl");
        let mut s = Session::new(&config, Some(path.to_str().unwrap()), llm(), tools());
        // 历史里两条消息各带一个 file 块 + 一个普通 text 块
        s.messages.push(Message {
            role: "user".into(),
            content: Some(Content::Parts(vec![
                json!({"type": "text", "text": "看看这张图"}),
                json!({"type": "file", "file_id": "file-abc"}),
            ])),
            synthetic: true,
            ..Default::default()
        });
        s.messages.push(Message {
            role: "user".into(),
            content: Some(Content::Parts(vec![
                json!({"type": "file", "file_id": "file-xyz"}),
            ])),
            synthetic: true,
            ..Default::default()
        });
        s.files.insert(
            "img-1".into(),
            json!({"file_id": "file-abc", "expires_at": 9_999_999_999i64}),
        );

        assert!(s.downgrade_file_blocks());
        // file 块 → 文本占位；text 块原样（skip(1) 跳过开头的 system prompt）
        for msg in s.messages.iter().skip(1) {
            let Some(Content::Parts(parts)) = &msg.content else {
                panic!("仍是 parts")
            };
            assert!(
                parts.iter().all(|p| p["type"] != "file"),
                "file 块应已换成占位"
            );
        }
        // 记录被标失效 → 下次同图重传
        assert_eq!(s.files["img-1"]["expires_at"], json!(0));
        // 再调一次：已经没 file 块了 → 没改动
        assert!(!s.downgrade_file_blocks());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn blob_is_content_addressed_and_0600() {
        let _g = env_lock();
        let dir = pie_dir_tmp("blob");
        let p1 = blob(b"same-bytes", "image/png");
        let p2 = blob(b"same-bytes", "image/png");
        assert_eq!(p1, p2, "内容寻址：同内容落到同一份副本");
        // 文件名主干就是 id：`img-<sha256[:16]>`（图片那一档留扩展名，是给用户双击用的）
        let stem = p1.file_stem().unwrap().to_string_lossy().into_owned();
        assert!(stem.starts_with("img-") && stem.len() == 20, "{stem}");
        assert!(p1.to_string_lossy().ends_with(".png"), "{p1:?}");
        assert_eq!(std::fs::read(&p1).unwrap(), b"same-bytes");
        assert_ne!(
            stem,
            blob(b"other", "image/png")
                .file_stem()
                .unwrap()
                .to_string_lossy(),
            "不同内容不同 id"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p1).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "图是用户数据，副本给 0o600");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entry_usability_depends_on_key_base_url_and_expiry() {
        let base = "https://api.deepseek.com";
        let fp = llm::key_fingerprint("sk-test");
        let ok = json!({"file_id": "f1", "base_url": base, "key_fp": fp});
        assert!(entry_is_usable(&ok, base, &fp));
        assert!(!entry_is_usable(
            &ok,
            base,
            &llm::key_fingerprint("sk-other")
        )); // 换 key
        assert!(!entry_is_usable(&ok, "https://other", &fp)); // 换 base_url
        assert!(!entry_is_usable(&json!({}), base, &fp)); // 没有 file_id
        let soon = config::now().as_secs_f64() + 30.0; // 不足 1 分钟余量
        assert!(!entry_is_usable(
            &json!({"file_id": "f1", "base_url": base, "key_fp": fp, "expires_at": soon}),
            base,
            &fp
        ));
        let later = config::now().as_secs_f64() + 3600.0;
        assert!(entry_is_usable(
            &json!({"file_id": "f1", "base_url": base, "key_fp": fp, "expires_at": later}),
            base,
            &fp
        ));
    }

    /// `repl` 出的图登记两条命脉：`local`（`files gc` 才认得“还被引用”）+ `calls`（`-r` 才配回画布）。
    #[test]
    fn repl_images_are_registered_for_gc_and_resume() {
        let dir = std::env::temp_dir().join(format!("pie-repl-img-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = Config {
            storage: config::Storage::at(&dir),
            ..Default::default()
        };
        let mut s = Session::new(&config, Some("s.jsonl"), llm(), tools());
        // `repl.rs` 转存后的持久副本（`files/img-<hash>.png`）
        let image = config
            .storage
            .store(config::StoreType::Blob {
                data: b"png-bytes",
                mime: "image/png",
            })
            .unwrap();
        let call = |id: &str| ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "repl".into(),
                arguments: "{}".into(),
            },
        };
        let out = || {
            let mut o = ToolOutput::text("");
            o.attachments = vec![Attachment::Canvas {
                path: image.clone(),
            }];
            o
        };
        s.record_repl_images(&call("c1"), &out());

        let hash = config::name_of(&image);
        assert_eq!(s.files[&hash]["local"], json!(image.display().to_string()));
        assert_eq!(
            s.files[&hash]["calls"],
            json!(["c1"]),
            "记下那图是哪次调用产的"
        );
        assert_eq!(s.files[&hash]["src"], json!("repl"));
        assert!(
            s.files[&hash].get("file_id").is_none(),
            "本地图不上传，没有 file_id"
        );

        // 同一次调用重复产出 → 不重复记；另一次调用再产出 → 追加
        s.record_repl_images(&call("c1"), &out());
        s.record_repl_images(&call("c2"), &out());
        assert_eq!(s.files[&hash]["calls"], json!(["c1", "c2"]));

        // 落盘后 `files gc`：副本刚落的、又没人引用——没有这条登记就会被当成垃圾回收。
        // 保护窗设 0（即“过了”，只看引用）→ 能留下就说明 `local` 生效了。
        s.save().unwrap();
        let garbage = crate::cli::collect_file_garbage(&config.storage, 0);
        assert!(
            !garbage.contains(&image),
            "登记过 `local` 就该留住：{garbage:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 图拿不到 `file_id`（这儿用 `files_api = false`，**不联网**）→ 那条 tool 消息**什么都不改**：
    /// 仍是纯文本，正文那行 `[图片已读取: …]` 还在（模型知道有图）—— 这就是天然降级。
    ///
    /// 用 `block_on` 而不是 `#[tokio::test]`：与旁边那个用例同款写法（不过这里没持环境锁）。
    #[test]
    fn attach_read_images_keeps_plain_text_when_files_api_is_off() {
        let _g = env_lock();
        let dir = std::env::temp_dir().join(format!("pie-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("x.png");
        std::fs::write(&png, b"fake-png").unwrap();

        let config = Config {
            files_api: false,
            ..Default::default()
        };
        let mut s = Session::ephemeral(&config, llm(), tools());
        s.messages.push(Message::tool_result(
            "call_1",
            "read",
            "[图片已读取: path=…, mime=image/png, size=8]",
        ));
        let att = Attachment::Model {
            path: png.clone(),
            mime: "image/png".into(),
            size: 8,
            dim: Some((13, 7)),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(s.attach_read_images(&[(0, att)]));

        assert!(
            matches!(s.messages[0].content, Some(Content::Text(_))),
            "拿不到 `file_id` 就该保持纯文本：{:?}",
            s.messages[0].content
        );
        assert!(s.files.is_empty(), "没上传就不该留记录");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没开 `files_api`（或模型不支持）时不注入、也不落副本（更不碰网络）。
    ///
    /// 用 `block_on` 而不是 `#[tokio::test]`：环境锁的 guard 不能跨 await 持有。
    #[test]
    fn ensure_image_file_is_none_when_disabled() {
        let _g = env_lock();
        let dir = pie_dir_tmp("disabled");
        let config = Config {
            files_api: false,
            ..Default::default()
        };
        let mut s = Session::new(
            &config,
            Some(dir.join("s.jsonl").to_str().unwrap()),
            llm(),
            tools(),
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(
            rt.block_on(s.ensure_image_file(b"bytes", "image/png", "x.png", "/tmp/x.png"))
                .is_none()
        );
        assert!(s.files.is_empty());
        assert!(
            !storage().files().exists(),
            "不开这个功能就没必要多存一份副本"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 窗口摘要在 `load` 时要**重建**回来：文件里的旧 system 被丢掉，但 `meta.windows`
    /// 记着窗口块，按当前 `head/tail` 重新生成摘要 system 消息。
    /// （不重建的话 resume 后模型就看不到被归档的历史了。）
    #[test]
    fn load_rebuilds_window_summaries() {
        let _g = env_lock();
        let dir = pie_dir_tmp("win");
        std::fs::create_dir_all(storage().context()).unwrap();
        let block = storage().context().join("session-abc.txt");
        let raw = json!([
            {"role": "user", "content": "旧问题"},
            {"role": "assistant", "content": "旧答复"}
        ]);
        std::fs::write(&block, serde_json::to_string(&raw).unwrap()).unwrap();
        let path = dir.join("s.jsonl");
        let meta = json!({
            "__meta__": true,
            "windows": [block.display().to_string()],
            "usage": {"calls": 1}
        });
        std::fs::write(
            &path,
            format!(
                "{meta}\n{}\n",
                json!({"role": "user", "content": "当前问题"})
            ),
        )
        .unwrap();

        let s = Session::load(&path, &Config::default(), llm(), tools()).expect("load");
        assert_eq!(s.messages.len(), 3, "system prompt + 窗口摘要 + 真实消息");
        assert_eq!(s.messages[0].role, "system");
        assert!(s.messages[0].compaction.is_none(), "第一条是当前提示词");
        assert!(
            matches!(s.messages[1].compaction, Some(Compaction::Session { .. })),
            "窗口摘要要重建回来"
        );
        let text = s.messages[1].content_text();
        assert!(text.starts_with(context::WINDOW_SUMMARY_MARKER), "{text}");
        assert!(text.contains("旧问题") && text.contains("旧答复"), "{text}");
        assert_eq!(s.messages[2].role, "user");
        assert_eq!(s.turn_count, 1, "window 摘要不是轮次");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TUI resume 回放用：`full_history()` 按消息顺序展开压缩指针。
    #[test]
    fn full_history_expands_compaction_pointers() {
        let _g = env_lock();
        let dir = pie_dir_tmp("full-history");
        std::fs::create_dir_all(storage().context()).unwrap();
        let config = Config::default();
        let mut s = Session::ephemeral(&config, llm(), tools());

        // 工具级：落盘的是被截断的那份输出全文（纯文本，不是消息）
        let full_text = "line1\nline2\nline3\n";
        let spill = raw(full_text, "tool");
        let mut tool = Message::tool_result(
            "call_1",
            "bash",
            format!(
                "[exit=0]\n\n[工具输出全文已保存: {}]\n\nline1",
                spill.display()
            ),
        );
        tool.compaction = Some(Compaction::tool(&spill));
        s.messages.push(tool);

        // 轮次级：落盘的是整段原文（JSON 数组，首条是 user）
        let raws = [
            Message::user("旧问题"),
            Message {
                role: "assistant".into(),
                content: Some(Content::Text("旧答复".into())),
                ..Default::default()
            },
        ];
        let blob = serde_json::to_string(&Value::Array(
            raws.iter()
                .map(|m| serde_json::to_value(m).unwrap())
                .collect(),
        ))
        .unwrap();
        let turn = raw(&blob, "turn");
        let mut summary =
            Message::assistant(format!("[轮次原文已保存: {}]\n\n旧答复", turn.display()));
        summary.compaction = Some(Compaction::turn(Some(&turn)));
        s.messages.push(summary);

        let full = s.full_history();
        let expanded = full
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call_1"))
            .expect("工具级消息还在");
        let text = expanded.content_text();
        assert!(text.ends_with(full_text), "工具级展开成落盘全文：{text}");
        assert!(text.starts_with("[exit=0]"), "头区（退出码）要保住：{text}");
        assert!(
            full.iter()
                .any(|m| m.role == "assistant" && m.content_text() == "旧答复"),
            "轮次级展开成原文序列：{full:?}"
        );
        assert!(
            !full
                .iter()
                .any(|m| matches!(m.compaction, Some(Compaction::Turn { .. }))),
            "指针消息都展开完了"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 落盘文件不在了（被 `context gc` 收走）→ 退回压缩形式本身，不装成功。
    #[test]
    fn full_history_falls_back_when_raw_is_gone() {
        let _g = env_lock();
        let dir = pie_dir_tmp("full-history-gone");
        std::fs::create_dir_all(storage().context()).unwrap();
        let config = Config::default();
        let mut s = Session::ephemeral(&config, llm(), tools());

        let turn = raw("[]", "turn");
        let mut summary =
            Message::assistant(format!("[轮次原文已保存: {}]\n\n旧答复", turn.display()));
        summary.compaction = Some(Compaction::turn(Some(&turn)));
        s.messages.push(summary);
        std::fs::remove_file(&turn).unwrap();

        let full = s.full_history();
        assert!(
            full.iter()
                .any(|m| m.content_text().contains("[轮次原文已保存")),
            "退回压缩形式：{full:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 转录快照：每轮**重写**（不是追加），解释器的 `history()` 读它。
    ///
    /// 为什么不能让它读会话文件：那个文件只在退出 / `/save` 时落盘，会话进行中根本不存在。
    #[test]
    fn transcript_snapshot_is_rewritten_for_each_turn() {
        let mut s = Session::new(&Config::default(), Some("s.jsonl"), llm(), tools());
        s.push_user("第一问");
        s.write_transcript();
        let snap = s.transcript_path();
        let body = std::fs::read_to_string(&snap).unwrap();
        assert!(body.contains("第一问"), "{body:?}");
        assert_eq!(body.lines().count(), 2, "system + user 各一行：{body:?}");

        s.messages.push(Message::assistant("第一答"));
        s.push_user("第二问");
        s.write_transcript();
        let body2 = std::fs::read_to_string(&snap).unwrap();
        assert_eq!(body2.lines().count(), 4, "重写而不是追加：{body2:?}");
        assert!(
            body2.contains("第二问") && body2.contains("第一答"),
            "{body2:?}"
        );
        let _ = std::fs::remove_file(&snap); // 落在临时目录，测完就收
    }

    /// 解释器代码块：**收进日志独立于压缩**（压缩会把 `tool_calls` 删掉），而那节**只列
    /// 已经从对话里消失的** —— 于是两次压缩之间那节逐字不变（缓存），压走之后才补上。
    #[test]
    fn repl_blocks_are_collected_and_listed_only_after_compaction() {
        let dir = std::env::temp_dir().join(format!("pie-rs-blocks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config {
            storage: crate::config::Storage::at(&dir),
            ..Config::default()
        };
        let repl_call = |id: &str, code: &str| ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: crate::llm::FunctionCall {
                name: "repl".into(),
                arguments: json!({"code": code}).to_string(),
            },
        };
        let mut s = Session::new(&config, Some("blocks.jsonl"), llm(), tools());
        s.messages.push(Message {
            role: "assistant".into(),
            tool_calls: Some(vec![
                repl_call("c1", "df = 1"),
                repl_call("c2", "df.head()"),
            ]),
            ..Default::default()
        });

        // 1) 采集：独立于压缩，只按 call id 去重（重复扫不重复记）
        s.collect_repl_blocks();
        s.collect_repl_blocks();
        assert_eq!(s.repl_blocks.len(), 2, "{:?}", s.repl_blocks);
        assert_eq!(s.repl_blocks[0].code, "df = 1");

        // 2) 还在对话里 → 那节不该列它（避重复）
        s.refresh_runtime_state();
        let live = s.messages[0].content_text();
        assert!(
            !live.contains("df.head()"),
            "还在对话里就不该列进那节：{live}"
        );
        assert!(live.ends_with(&config::runtime_state("")));

        // 3) 什么都没压 → 重拼逐字相同（前缀缓存不受影响）
        s.refresh_runtime_state();
        assert_eq!(s.messages[0].content_text(), live, "没压东西就不该动那节");
        let base = live[..live.rfind(config::RUNTIME_STATE_HEADING).unwrap()].to_string();

        // 4) 模拟轮次级压缩：那两条 tool_calls 被换成一条摘要
        s.messages[1] = Message::assistant("[轮次原文已保存: …]\n\n...[中间过程省略]...\n好了");
        s.refresh_runtime_state();
        let after = s.messages[0].content_text();
        assert!(after.contains("# cell 1\ndf = 1"), "{after}");
        assert!(after.contains("# cell 2\ndf.head()"), "{after}");
        assert!(after.contains("只列已被压缩带走的"), "{after}");
        assert!(
            after.starts_with(&base),
            "标题前面那段不该变（缓存前缀照旧）：{after}"
        );

        // 5) 日志随 `__meta__` 落盘 → `-r` 之后那节能重建
        s.save().unwrap();
        let mut loaded = Session::load(&s.path, &config, llm(), tools()).unwrap();
        assert_eq!(loaded.repl_blocks.len(), 2, "{:?}", loaded.repl_blocks);
        loaded.refresh_runtime_state();
        assert!(
            loaded.messages[0].content_text().contains("df.head()"),
            "载入后那节要能重建：{}",
            loaded.messages[0].content_text()
        );
    }

    /// 日志有上限：超出丢最老的（不能让它变成第二份无限长的历史）。
    #[test]
    fn repl_blocks_are_capped_and_drop_the_oldest() {
        let mut s = Session::new(&Config::default(), Some("cap.jsonl"), llm(), tools());
        for i in 0..(REPL_BLOCKS_MAX + 3) {
            s.messages.push(Message {
                role: "assistant".into(),
                tool_calls: Some(vec![ToolCall {
                    id: format!("c{i}"),
                    kind: "function".into(),
                    function: crate::llm::FunctionCall {
                        name: "repl".into(),
                        arguments: json!({"code": format!("x = {i}")}).to_string(),
                    },
                }]),
                ..Default::default()
            });
        }
        s.collect_repl_blocks();
        assert_eq!(s.repl_blocks.len(), REPL_BLOCKS_MAX);
        assert_eq!(s.repl_blocks[0].code, "x = 3", "丢最老的三个");
        assert_eq!(s.repl_blocks[REPL_BLOCKS_MAX - 1].code, "x = 26");
    }

    /// 「运行时状态」节活在 `messages[0]` 末尾：每轮重拼、**幂等**（cwd 没变就逐字相同 →
    /// 提示词前缀缓存不失效），cwd 变了只换尾部那节。
    ///
    /// 它不进历史 → 轮次级压缩碰不到它：这正是「把状态变更插成一条 system 消息」那个做法
    /// 的老病（span 就是「两个 user 之间的一切」）的解药。
    #[test]
    fn runtime_state_is_refreshed_in_place_on_the_system_prompt() {
        let _g = env_lock(); // cwd 是进程级的 → 与其它环境类用例串行
        let saved = std::env::current_dir().unwrap();
        let dir = std::env::temp_dir().join(format!("pie-rs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config {
            storage: crate::config::Storage::at(&dir),
            ..Config::default()
        };
        let mut s = Session::ephemeral(&config, llm(), tools());

        let before = s.messages[0].content_text();
        let heading = config::RUNTIME_STATE_HEADING;
        // ⚠ 别数绝对次数：提示词正文里就可能提到这个标题（`prompts/system.md` 就写了）——
        // 判据是「那一节在**末尾**」，而且重拼不会凭空多出几节。
        assert!(
            before.ends_with(&config::runtime_state("")), // 还没跑过 repl → 那节只有 cwd
            "建会话时那节就该在末尾：{before}"
        );
        let sections = before.matches(heading).count();
        let base = before[..before.rfind(heading).unwrap()].to_string();

        // 幂等：故意连搅两次，逐字相同（不堆出第二节）
        s.refresh_runtime_state();
        s.refresh_runtime_state();
        assert_eq!(s.messages[0].content_text(), before, "cwd 没变就该逐字相同");

        // 轮到新 cwd：下一轮反映进去，而且**只有尾部那节变**（前面那段原样 → 缓存前缀照旧）
        let moved = std::env::temp_dir().join(format!("pie-rs-moved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&moved);
        std::fs::create_dir_all(&moved).unwrap();
        let moved = moved.canonicalize().unwrap();
        std::env::set_current_dir(&moved).unwrap();
        s.refresh_runtime_state();
        let after = s.messages[0].content_text();
        assert!(after.starts_with(&base), "标题前面的那段不该变：{after}");
        assert_eq!(
            after.matches(heading).count(),
            sections,
            "重拼不该多出几节：{after}"
        );
        assert!(
            after.contains(&moved.display().to_string()),
            "新 cwd 要写进去：{after}"
        );

        // 轮次级压缩压的是「两个 user 之间」→ system prompt 那节不受影响
        s.messages.push(Message::user("q1"));
        s.messages.push(Message::assistant("a1"));
        s.messages.push(Message::user("q2"));
        s.messages.push(Message::assistant("a2"));
        let (stats, _) = context::maybe_compact(&mut s.messages, &config, Some(9_999_999));
        assert!(stats.turns > 0, "这一轮该被压掉");
        assert!(
            s.messages[0]
                .content_text()
                .contains(&moved.display().to_string()),
            "压缩后 cwd 还在：{}",
            s.messages[0].content_text()
        );

        std::env::set_current_dir(&saved).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&moved);
    }

    /// `/model`、`/thinking`：改配置 + 同步客户端实例 + 写回文件。
    #[test]
    fn set_model_and_thinking_persist_config() {
        let _g = env_lock();
        let dir = pie_dir_tmp("setmodel");
        let path = dir.join("config.toml");
        let config = Config {
            config_file: Some(path.clone()),
            ..Default::default()
        };
        let mut s = Session::new(
            &config,
            Some(dir.join("s.jsonl").to_str().unwrap()),
            llm(),
            tools(),
        );
        assert_eq!(s.set_model("deepseek-v4-pro"), "已写入配置");
        assert_eq!(s.config.model, "deepseek-v4-pro");
        assert_eq!(s.llm.model, "deepseek-v4-pro", "客户端实例要同步");
        assert_eq!(s.set_reasoning_effort("none"), "已写入配置");
        // 落盘可见：重新读一遍配置文件
        let back = Config::load(Some(&path)).expect("load");
        assert_eq!(back.model, "deepseek-v4-pro");
        assert_eq!(back.reasoning_effort, "none");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/stat` 报告：有会话文件行、有千分位、带用量 JSON。
    #[test]
    fn usage_report_mentions_budget_and_usage() {
        let _g = env_lock();
        let dir = pie_dir_tmp("stat");
        let path = dir.join("s.jsonl");
        let config = Config {
            context_window: 100_000,
            reserved_tokens: Some(4_000),
            ..Default::default()
        };
        let mut s = Session::new(&config, Some(path.to_str().unwrap()), llm(), tools());
        s.push_user("hi");
        s.usage.record(&Usage {
            prompt_tokens: Some(1_234),
            ..Default::default()
        });
        let report = s.usage_report();
        assert!(report.contains("1,234"), "千分位：{report}");
        assert!(report.contains("输入预算"), "{report}");
        assert!(report.contains("上下文压缩：0 / 0 / 0"), "{report}");
        assert!(report.contains("API 用量"), "{report}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trips_through_jsonl() {
        let config = Config::default();
        let path = tmp("rt.jsonl");
        let mut s = Session::new(&config, Some(path.to_str().unwrap()), llm(), tools());
        assert_eq!(s.path, path);
        s.push_user("第一轮：读一下 Cargo.toml");
        s.messages.push(Message {
            role: "assistant".into(),
            content: Some(Content::Text("好的".into())),
            ..Default::default()
        });
        s.usage.record(&usage(10));
        s.save().expect("save");

        let back = Session::load(&path, &config, llm(), tools()).expect("load");
        assert_eq!(back.turn_count, 1);
        assert_eq!(back.title.as_deref(), Some("第一轮：读一下 Cargo.toml"));
        assert_eq!(back.usage.calls, 1);
        assert_eq!(back.usage.prompt_tokens, Some(10));
        assert_eq!(back.messages.len(), 3); // 重建的 system + user + assistant
        assert_eq!(back.messages[0].role, "system");
        assert_eq!(back.messages[1].role, "user");
        assert_eq!(back.messages[2].role, "assistant");
        let _ = std::fs::remove_file(&path);
    }

    /// 压缩事件的读回：旧键名（`raw_path` / `raw_hash` / 冗余 `level`）与新键名（`path` / `hash`）
    /// 都能读（前者靠 `#[serde(alias)]`）；类型看 `kind`，坏的那条（未知 `kind`）只丢它自己。
    #[test]
    fn legacy_and_current_event_keys_both_load() {
        let config = Config::default();
        let path = tmp("legacy-events.jsonl");
        let old_path = PathBuf::from("/tmp/pie-legacy/context/bash-0123456789abcdef");
        let new_path = PathBuf::from("/tmp/pie-legacy/context/bash-fedcba9876543210");
        let meta = json!({
            "__meta__": true,
            "compaction_events": [
                {"ts": 1, "level": 1, "kind": "tool", "tool": "bash",
                 "raw_path": old_path.display().to_string(), "raw_hash": "0123456789abcdef"},
                {"ts": 2, "level": 3, "kind": "session",
                 "raw_path": "/tmp/pie-legacy/context/session-ff", "raw_hash": "ff", "summary": "旧摘要"},
                {"ts": 3, "kind": "tool", "tool": "bash",
                 "path": new_path.display().to_string(), "hash": "fedcba9876543210"},
                {"ts": 4, "kind": "some-future-kind"},
            ]
        });
        std::fs::write(&path, format!("{meta}\n")).expect("write");

        let s = Session::load(&path, &config, llm(), tools()).expect("load");
        assert_eq!(
            s.compaction_events.len(),
            3,
            "未知 kind 只丢那条：{:?}",
            s.compaction_events
        );
        assert_eq!(s.compaction_events[0].level(), 1);
        assert_eq!(
            s.compaction_events[0].raw_path(),
            old_path.as_path(),
            "旧键 raw_path"
        );
        match &s.compaction_events[1] {
            context::CompactEvent::Session { summary, .. } => assert_eq!(summary, "旧摘要"),
            other => panic!("期望会话级事件：{other:?}"),
        }
        assert_eq!(
            s.compaction_events[2].raw_path(),
            new_path.as_path(),
            "新键 path"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 落盘把 C1 控制符 / U+2028 转义掉：否则按「Unicode 行边界」切行的读者（`splitlines()`）
    /// 会把 U+0085 当换行 → 一条消息被劈成两半、整份 JSONL 读不出来。
    #[test]
    fn save_escapes_control_chars_that_break_line_readers() {
        let config = Config::default();
        let path = tmp("c1.jsonl");
        let mut s = Session::at(path.clone(), &config, llm(), tools());
        let payload = "a\u{85}b\u{2028}c\u{9f}d";
        s.messages
            .push(Message::tool_result("call_1", "bash", payload));
        s.save().expect("save");

        let text = std::fs::read_to_string(&path).expect("read");
        for c in text.chars() {
            assert!(
                !matches!(c, '\u{80}'..='\u{9f}' | '\u{2028}' | '\u{2029}'),
                "还有没转义的字符: U+{:04X}",
                c as u32
            );
        }
        assert!(text.contains(r"\u0085"), "{text}");
        // 剩下的换行只有真正的行分隔 → 就是「splitlines 类读者也切不开」
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{lines:?}"); // meta + system + tool
        for line in &lines {
            serde_json::from_str::<Value>(line).expect("每行都是合法 JSON");
        }
        // 转义对语义透明：读回来还是原来的字符
        let back = Session::load(&path, &config, llm(), tools()).expect("load");
        let Some(Content::Text(back_text)) = back.messages.last().unwrap().content.as_ref() else {
            panic!("最后一条该是 tool 文本消息");
        };
        assert_eq!(back_text, payload);
        let _ = std::fs::remove_file(&path);
    }

    /// 模型请求失败（外部因素）→ 历史里补一条带错误信息的 assistant 消息，**不留悬空提问**。
    #[test]
    fn failed_model_call_leaves_an_error_assistant_message() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            // 故意给个 reqwest 解析不了的地址：请求立刻失败、不联网、不重试
            let config = Config {
                base_url: "不是地址".into(),
                max_retries: 0,
                ..Config::default()
            };
            let mut s =
                Session::ephemeral(&config, LlmClient::new(&config).expect("client"), tools());
            let err = s
                .aturn(
                    "看一下这个 bug",
                    &mut |_| {},
                    &Cancel::new(),
                    None,
                    Some(false),
                    None,
                    RequestOptions::default(),
                )
                .await
                .expect_err("请求必定失败");
            let last = s.messages.last().expect("有消息");
            assert_eq!(last.role, "assistant", "失败后历史末尾该是一条 assistant");
            let Some(Content::Text(text)) = last.content.as_ref() else {
                panic!("错误回合该是纯文本 assistant");
            };
            assert!(text.starts_with(ERROR_TURN_PREFIX), "{text}");
            assert!(text.contains(&err.to_string()), "{text} / {err}");
            // user 后面跟着 assistant，不再是连续两条 user
            let roles: Vec<&str> = s.messages.iter().map(|m| m.role.as_str()).collect();
            assert_eq!(roles, vec!["system", "user", "assistant"], "{roles:?}");
        });
    }

    /// 用量是「最近一次上报值 + calls 累计」（不求和）。
    #[test]
    fn usage_tracker_overwrites_tokens_but_counts_calls() {
        let mut t = UsageTracker::default();
        t.record(&usage(10));
        t.record(&usage(30));
        assert_eq!(t.prompt_tokens, Some(30));
        assert_eq!(t.calls, 2);
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains(r#""calls":2"#), "{json}");
        assert!(json.contains(r#""prompt_tokens":30"#), "{json}");
    }

    /// **向后兼容**：旧版写下的会话（含未知字段 / 旧 system / null token）必须能读。
    #[test]
    fn loads_legacy_written_session() {
        let config = Config::default();
        let path = tmp("py.jsonl");
        let jsonl = [
            r#"{"__meta__":true,"usage":{"prompt_tokens":123,"completion_tokens":null,"total_tokens":null,"prompt_cache_hit_tokens":null,"prompt_cache_miss_tokens":null,"reasoning_tokens":null,"calls":3},"windows":[],"cwd":"/tmp","title":"旧会话"}"#,
            r#"{"role":"system","content":"SENTINEL-OLD-SYSTEM","compaction":{"kind":"session","path":"/x"}}"#,
            r#"{"role":"user","content":"你好","synthetic":false}"#,
            r#"{"role":"assistant","content":"在的","tool_calls":[{"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"pwd\"}"}}]}"#,
            r#"{"role":"tool","content":"[exit=0]\n\n/tmp","tool_call_id":"call_1"}"#,
        ]
        .join("\n");
        std::fs::write(&path, format!("{jsonl}\n")).unwrap();

        let s = Session::load(&path, &config, llm(), tools()).expect("load");
        assert_eq!(s.title.as_deref(), Some("旧会话"));
        assert_eq!(s.usage.calls, 3);
        assert_eq!(s.usage.prompt_tokens, Some(123));
        assert_eq!(s.turn_count, 1);
        assert_eq!(s.messages.len(), 4); // 重建的 system + user + assistant + tool
        assert!(
            !matches!(&s.messages[0].content, Some(Content::Text(t)) if t.contains("SENTINEL-OLD-SYSTEM")),
            "旧 system 应被丢弃，换成当前提示词"
        );
        // 带 tool_calls 但缺 reasoning_content → 补空串（否则 thinking 模式回放报 400）
        assert_eq!(s.messages[2].reasoning_content.as_deref(), Some(""));
        let _ = std::fs::remove_file(&path);
    }

    /// resume：同 cwd 优先；都不匹配时取 mtime 最新的。
    #[test]
    fn resume_prefers_same_cwd_then_latest() {
        let config = Config::default();
        let dir = std::env::temp_dir().join(format!("pie-sessdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, cwd: &str, title: &str| {
            let path = dir.join(name);
            let meta = format!(
                r#"{{"__meta__":true,"usage":{{"calls":1}},"windows":[],"cwd":"{cwd}","title":"{title}"}}"#
            );
            std::fs::write(
                &path,
                format!("{meta}\n{}\n", r#"{"role":"user","content":"hi"}"#),
            )
            .unwrap();
            path
        };
        let same_cwd = write("a.jsonl", "/work/dir", "same-cwd");
        std::thread::sleep(std::time::Duration::from_millis(20)); // 拉开 mtime
        let other_cwd = write("b.jsonl", "/elsewhere", "other-cwd");

        // cwd 匹配优先（哪怕它更旧）
        let s = Session::resume_in(&dir, Some("/work/dir"), &config, llm(), tools()).unwrap();
        assert_eq!(s.title.as_deref(), Some("same-cwd"));
        assert_eq!(s.path, same_cwd);
        // 都不匹配 → 取 mtime 最新
        let s = Session::resume_in(&dir, Some("/nope"), &config, llm(), tools()).unwrap();
        assert_eq!(s.title.as_deref(), Some("other-cwd"));
        assert_eq!(s.path, other_cwd);
        // 空目录 → 报错不 panic
        let empty = std::env::temp_dir().join(format!("pie-sessempty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        assert!(
            Session::resume_in(&empty, None, &config, llm(), tools())
                .unwrap_err()
                .contains("没有历史会话")
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }
}
