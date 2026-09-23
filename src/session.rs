//! 会话层：多轮对话（历史 + JSONL 持久化 + 用量）**加上一回合的 agent 循环**。
//!
//! 回合循环原来在独立的 `loop.rs`（`run_turn`），已并进 `Session::aturn`：两者本来就是一件事的
//! 两半，分开只会让每次调用在两模块之间穿 7 个参数（其中 4 个还是 `self` 的字段）。
//! ⚠ 与 Python 版的结构差异：那边 `loop.py` 仍是独立一层（`loop.aturn` 被 `Session.aturn` 调，
//! `loop.run` 供一次性模式复用）；这边一次性模式改走 `Session::ephemeral()`（不落盘、不写 manifest）。
//!
//! **持久化契约**（与 Python 版同格式，两边写下的文件可以互相读）：
//!   - 一行一条 JSON：首行 `__meta__`（usage / windows / cwd / title），之后每行一条消息；
//!   - 恢复时**丢弃文件里的 system 消息**，按当前 SYSTEM.md / AGENTS.md / MEMORY.md 重建
//!     （提示词会变，历史里那份旧 system 没有意义）；
//!   - 会话目录与 Python 版同一处，resume 按 mtime 选最新、「同 cwd 优先」（读 meta 里的 `cwd`；
//!     旧会话没这个键就当不匹配）；未知字段（Python 的 `synthetic` / `compress_level`…）serde 直接忽略。
//!
//! 文件名用 `chat-<unix 秒>-<微秒>.jsonl`（Python 用本地时间 `%Y%m%d-%H%M%S-%f`）：没有日期库，
//! Unix 时间戳一样能做到「可排序 + 微秒级不撞车」；两边都按 mtime 排序，命名不同不影响互相 resume。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value};

use futures_util::stream::{self, StreamExt};

use crate::cancel::{Cancel, CANCEL_TEXT};
use crate::config::{self, Config};
use crate::context;
use crate::llm::{
    self, Content, LlmClient, LlmError, LlmResult, Message, StreamChunk, ToolCall, UsageTracker,
};
use crate::tools::{self, ToolRegistry};
use sha2::{Digest, Sha256};

/// 回合过程中推给调用方的事件（TUI / CLI 边跑边渲染用）。
///
/// 与 Python 版 `loop.aturn` 的 `on_event` dict 一一对应（那边是 `{"type": …}` 裸 dict，
/// 这边是枚举）；`arguments` 传**原始 JSON 字符串**，由消费者自己解析出「这次调的是哪个
/// 文件 / 命令」的摘要（Python 那边直接给解析后的 dict）。
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// 正文增量
    AssistantText(String),
    /// 思考增量
    Reasoning(String),
    /// 即将执行某个工具
    ToolCall {
        name: String,
        arguments: String,
    },
    /// 工具执行完毕（按真实完成顺序推；`content` 是**原样**文本，不再截断——
    /// 少显示是展示层的事，见 `Session::tool_call`）
    ToolResult {
        name: String,
        content: String,
        arguments: String,
    },
    /// 最终答复。
    ///
    /// **只在非流式（`aturn(stream = Some(false))`）时推**：流式下正文已经通过 `AssistantText` 增量
    /// 推过了，再推一次消费者会重复显示（Python 是流式也推 `answer`，靠 TUI 覆盖面板绕过）。
    Answer(String),
}
#[derive(Debug)]
pub struct Session {
    /// 会话 JSONL 自身的路径（也是 `save` 的默认目标）。
    pub path: PathBuf,
    /// 配置快照（压缩水位 / `keep_last_steps` / `compaction` 都从它取）。
    pub config: Config,
    /// 模型后端（Python `Session.llm` 同款：会话自己拿揰着）。
    pub llm: LlmClient,
    /// 工具集（Python `Session.tools` 同款；`--tools` 裁剪过的注册表就从这里进来）。
    pub tools: ToolRegistry,
    /// 压缩 manifest（`~/.pie/context/<会话名>.manifest.jsonl`）；`None` = 不记账（`ephemeral`）。
    pub manifest: Option<PathBuf>,
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

impl Session {
    /// 新建会话：`id` 给了就用它（纯名字 → `~/.pie/sessions/<id>.jsonl`，带目录/绝对路径 → 原样），
    /// 否则按时间戳新建文件。
    ///
    /// `llm` / `tools` 由调用方给（Python `Session.new(config=…, llm=…, tools=…)` 同款）：
    /// 外面已经建好的客户端 /（可能被 `--tools` 裁剪过的）工具集直接收进来。
    pub fn new(config: &Config, id: Option<&str>, llm: LlmClient, tools: ToolRegistry) -> Self {
        Self::at(resolve_path(id), config, llm, tools)
    }

    /// 临时会话（`pie-rs "任务"` 用）：不落盘（别调 `save`）、不写 manifest，其余完全一样。
    ///
    /// Python 那边一次性模式走独立的 `loop.run()`；这边既然回合循环已经并在 `Session` 上，
    /// 就用“不记账的 Session”表达同一件事。
    pub fn ephemeral(config: &Config, llm: LlmClient, tools: ToolRegistry) -> Self {
        let mut session = Self::at(
            sessions_dir().join(format!("ephemeral-{}.jsonl", timestamp())),
            config,
            llm,
            tools,
        );
        session.manifest = None;
        session
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
    /// 返回一句提示（写不进去就说明“仅本次生效”）——与 Python `set_model` 同款。
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
                // 窗口块列表（旧键 `fs` 是 Python 改名前的写法，一并兼容）
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
                // 图片 id 表（hash_id → 上传记录）
                session.files = value
                    .get("files")
                    .and_then(Value::as_object)
                    .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
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
        // （Python 版同款；thinking 模式回放这种历史会被 DeepSeek 判 400）
        for m in &mut restored {
            if m.tool_calls.is_some() && m.reasoning_content.is_none() {
                m.reasoning_content = Some(String::new());
            }
        }
        // 用户轮数：`synthetic` 的（注入的图片消息，role 也是 user）不算轮次（Python 同款）
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
        // 摘要文本随 `head/tail` 配置与提示词变化，重建比留着旧的更准（Python 同款）。
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
            let raw = std::fs::read_to_string(block).unwrap_or_default();
            let mut msg = Message::system(context::build_window_summary(
                block,
                session_config.head,
                session_config.tail,
            ));
            msg.compress_level = 3;
            msg.raw_path = Some(block.display().to_string());
            msg.raw_hash = block
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.rsplit('-').next())
                .map(str::to_string);
            msg.raw_len = Some(raw.chars().count() as i64);
            msg.raw_tokens = Some(raw.chars().count() as i64 / 4);
            out.push(msg);
        }
        out
    }

    /// 恢复最近的会话：优先**当前工作目录**下最新的，没有则全局最新。
    pub fn resume(config: &Config, llm: LlmClient, tools: ToolRegistry) -> Result<Self, String> {
        let cwd = std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string());
        Self::resume_in(&sessions_dir(), cwd.as_deref(), config, llm, tools)
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
        self.messages.push(Message {
            role: "assistant".into(),
            content: Some(Content::Text(text.to_string())),
            ..Default::default()
        });
    }

    /// 跑一个完整回合：追加用户消息 → 反复「问模型 → 执行工具」→ 返回最终答复。
    /// **不落盘会话**（由调用方 `save`）；压缩事件写进 manifest（`ephemeral` 会话不写）。
    ///
    /// 回合语义（对齐 Python `loop.aturn`）：工具失败文本化后照常回传、达到 `max_steps` 就把
    /// 最后一段 assistant 文本当答复（不额外追加消息）、`tool` 结果与 `tool_call_id` 严格配对。
    ///
    /// 两个**按次**的执行旋钮（不在配置里，对齐 Python 把 `loop.aturn` 的形参）：
    ///   - `max_steps`：单回合最多问几次模型；`None` = 不限。
    ///   - `stream`：`None` = 默认（客户端都实现了 `stream()` → 流式）；`Some(false)` 强制一次性
    ///     `complete()`（`on_event` 不再有增量，只推一次 `Answer`）。
    ///   - `parallel_tools`：同一批 `tool_calls` 是否**并发**执行；`None` = 跟随 `config.parallel_tools`
    ///     （默认 true）。工具共享可变状态时必须 `Some(false)`（改为按模型返回顺序串行）。
    ///
    /// `cancel` 触发时（TUI 的 `Esc`）：
    ///   - 模型请求中的取消 → 直接收尾（不追加 assistant 消息）；
    ///   - 工具执行中的取消 → shell 会杀掉整个进程组，**未执行的 `tool_calls` 补 `CANCEL_TEXT` 的
    ///     tool 消息**（保证每个 `tool_call_id` 都有配对结果、API 序列合法，Python `_cancel_tools` 同款）；
    ///   - 两种收尾都往历史里写一条 `CANCEL_TEXT` 的 assistant 消息，并把它作为本轮答复返回。
    pub async fn aturn(
        &mut self,
        input: &str,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancel: &Cancel,
        max_steps: Option<usize>,
        stream: Option<bool>,
        parallel_tools: Option<bool>,
    ) -> Result<String, LlmError> {
        self.push_user(input);
        let manifest = self.manifest.clone();
        // 会话级压缩的窗口块：从 on_compact 事件（level=3）里收集，跑完再登记进 self.windows
        let mut new_windows: Vec<PathBuf> = Vec::new();
        let mut on_compact = |entry: Value| {
            if entry.get("level").and_then(Value::as_u64) == Some(3) {
                if let Some(path) = entry.get("raw_path").and_then(Value::as_str) {
                    new_windows.push(PathBuf::from(path));
                }
            }
            record_compact(manifest.as_deref(), &entry);
        };

        // 配置值拷出来（不长期借 `self.config`：后面还要 `&mut self` 做注入/降级）
        // `stream: None` = 用默认（客户端都实现了流式）；对齐 Python `stream is not False` 的判据
        let use_stream = stream.unwrap_or(true);
        // `parallel_tools: None` = 跟随配置（Python `config.parallel_tools if x is None else x` 同义）
        let use_parallel = parallel_tools.unwrap_or(self.config.parallel_tools);
        let mut steps = 0usize;
        let mut answer: Option<String> = None;
        let mut done = false;
        let mut cancelled = false;
        loop {
            // 取消检查点（每步开头；请求/工具内部的 race 另算）
            if cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            if let Some(max) = max_steps {
                if steps >= max {
                    // 达到步数上限：不再问模型，把历史里最后一段 assistant 文本当最终答复
                    answer = last_assistant_text(&self.messages);
                    if !use_stream {
                        on_event(TurnEvent::Answer(answer.clone().unwrap_or_default()));
                    }
                    done = true;
                    break;
                }
            }
            steps += 1;

            // 请求前：按**估算**水位压一次（`config.compaction` 未配置就什么都不做）
            context::maybe_compact(
                &mut self.messages,
                &self.config,
                None,
                Some(&mut on_compact as &mut dyn FnMut(Value)),
            );

            let specs = self.tools.specs();
            let called = match self.model_call(&specs, on_event, cancel, use_stream).await {
                Ok(option) => option,
                Err(e) => {
                    // file_id 失效（服务端删了 / 中途换了 key）：把历史里的图片块降级成文本占位、
                    // 记录标失效（下次同图重传），再试一次——不然整个回合 400 报废。
                    if e.is_stale_file_error() && self.downgrade_file_blocks() {
                        crate::log::warn(format!(
                            "[warn] file_id 已失效，已把历史里的图片降级为占位文本并重试：{e}"
                        ));
                        self.model_call(&specs, on_event, cancel, use_stream)
                            .await?
                    } else {
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
            // 请求后：provider 上报的 `prompt_tokens` 是最准的水位（上下文只增不减），拿它再压一次
            if let Some(prompt_tokens) = self.usage.prompt_tokens {
                context::maybe_compact(
                    &mut self.messages,
                    &self.config,
                    Some(prompt_tokens),
                    Some(&mut on_compact as &mut dyn FnMut(Value)),
                );
            }
            self.messages.push(Message {
                role: "assistant".into(),
                content: result
                    .content
                    .as_deref()
                    .map(|c| Content::Text(c.to_string())),
                tool_calls: (!tool_calls.is_empty()).then(|| tool_calls.clone()),
                reasoning_content: result.reasoning_content.clone(),
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
            let mut batch_results: Vec<String> = Vec::with_capacity(tool_calls.len());
            let mut interrupted = false;
            for (call, outcome) in tool_calls.iter().zip(outcomes) {
                let text = match outcome {
                    Some(text) => text,
                    // 被取消：每个 tool_call_id 都要有配对的 tool 消息，否则回放这段历史时
                    // API 会拒（Python `_cancel_tools` 同款）；结果事件已在 `tool_call` 里推过
                    None => {
                        interrupted = true;
                        CANCEL_TEXT.to_string()
                    }
                };
                let mut msg = Message::tool_result(&call.id, &call.function.name, text.clone());
                if call.function.name == "bash" {
                    // bash 超限自带落盘：把 spill 指针同步成压缩元数据 + manifest 事件
                    //（只 bash 会落盘 → 按工具名 gate；不然 read 回来的源码字面量会被误判）
                    if let Some(entry) =
                        context::mark_tool_spill(&mut msg, &call.function.name, &text)
                    {
                        on_compact(entry);
                    }
                }
                self.messages.push(msg);
                batch_results.push(text);
            }
            if interrupted {
                cancelled = true;
                break;
            }
            // 工具结果全部回填后，把本批 `read` 读到的图片作为多模态 user 消息注入
            //（紧随结果之后；图片消息是 synthetic → 不是轮次边界，不影响压缩/轮数）
            self.inject_read_images(&tool_calls, &batch_results).await;
        }

        drop(on_compact); // 显式收尾：下面要 move `new_windows`
        self.windows.extend(new_windows);

        if cancelled {
            // 历史里留一条终止消息（Python `_cancel_turn` 同款），并把它当本轮答复
            self.messages.push(Message {
                role: "assistant".into(),
                content: Some(Content::Text(CANCEL_TEXT.to_string())),
                ..Default::default()
            });
            if !use_stream {
                on_event(TurnEvent::Answer(CANCEL_TEXT.to_string()));
            }
            return Ok(CANCEL_TEXT.to_string());
        }
        debug_assert!(done, "回合既没正常结束也没取消");
        Ok(answer.unwrap_or_default())
    }

    /// 一次模型调用（流式 / 非流式两条路），增量经 `on_event` 推；`stream` 由 `aturn` 按次传进来。
    /// 一次模型调用；`Ok(None)` = 请求期间被取消（不 push 任何消息，收尾由 `aturn` 统一做）。
    async fn model_call(
        &self,
        specs: &[Value],
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancel: &Cancel,
        stream: bool,
    ) -> Result<Option<LlmResult>, LlmError> {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let call = async {
            if stream {
                self.llm
                    .stream(&self.messages, specs, |chunk| match chunk {
                        StreamChunk::Content(d) => on_event(TurnEvent::AssistantText(d)),
                        StreamChunk::Reasoning(d) => on_event(TurnEvent::Reasoning(d)),
                        StreamChunk::ToolCall { .. } => {}
                    })
                    .await
            } else {
                self.llm.complete(&self.messages, specs).await
            }
        };
        tokio::select! {
            result = call => result.map(Some),
            _ = cancel.cancelled() => Ok(None),
        }
    }

    /// 把本批 `read` 工具读到的图片作为多模态 user 消息注入（下一轮请求模型就能看到图）。
    ///
    /// **只走 Files API**：拿不到 `file_id`（未开启 / 模型不支持 / 上传失败）就不注入——
    /// 与 Python 有意不同（那边回退内联 base64）。标记文本仍在工具结果里，模型知道有这张图；
    /// 本地副本与记录也会留下，下次同图直接命中不再重传。
    async fn inject_read_images(&mut self, calls: &[ToolCall], results: &[String]) {
        for (call, text) in calls.iter().zip(results.iter()) {
            if call.function.name != "read" || text.is_empty() {
                continue;
            }
            let Some(image) = tools::parse_image_marker(text) else {
                continue; // 不是读图
            };
            let Ok(data) = std::fs::read(&image.path) else {
                continue; // 文件没了 → 标记仍在工具结果里，不硬塞
            };
            if image.size > 0 && data.len() as u64 != image.size {
                continue; // 文件被换过（大小对不上）
            }
            let filename = Path::new(&image.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let file_id = self
                .ensure_image_file(&data, &image.mime, &filename, &image.path)
                .await;
            let Some(file_id) = file_id else {
                continue;
            };
            let dim = match (image.width, image.height) {
                (Some(w), Some(h)) => format!("{w}x{h} "),
                _ => String::new(),
            };
            let parts = json!([
                {
                    "type": "text",
                    "text": format!(
                        "[图片（由 read 工具读取，非用户输入）: {} {dim}{} 字节 {}]",
                        image.path,
                        data.len(),
                        image.mime
                    )
                },
                {"type": "file", "file_id": file_id},
            ]);
            self.messages.push(Message {
                role: "user".into(),
                content: Some(Content::Parts(
                    parts.as_array().cloned().unwrap_or_default(),
                )),
                synthetic: true,
                ..Default::default()
            });
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
    /// 那条推 `CANCEL_TEXT`（Python `_cancel_tools` 同款）。
    ///
    /// 返回与 `calls` **等长同序**的文本（`None` = 该调用被取消）——回填消息时
    /// 才能保证历史扁平序列与串行一致（compaction 的 step 批次 / `keep_last_steps` 都看它），
    /// 由调用方统一补 `CANCEL_TEXT`。
    ///
    /// 工具输出的文本**原样**交出去（不在这里截断）：要少显示是展示层（TUI / CLI）的事，
    /// 要少回传给模型是工具自己配容量上限（`shell` / `read`）的事。
    ///
    /// 参数非法 JSON **不执行工具**、只把原文回给模型；单个工具失败文本化后照常返回，
    /// 不拖累同批其他工具（对齐 Python `loop._run_tool_call`）。
    async fn tool_call(
        &self,
        calls: &[ToolCall],
        parallel: bool,
        cancel: &Cancel,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Vec<Option<String>> {
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
                        Err(_) => Some(format!(
                            "[参数解析失败] 模型返回了非法 JSON: {}",
                            call.function.arguments.chars().take(500).collect::<String>()
                        )),
                        Ok(args) => {
                            let text = dispatch_tool(registry, call, &args, cancel).await;
                            (text != CANCEL_TEXT).then_some(text) // shell 被杀 → 哨兵 → 算取消
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

        let mut outcomes: Vec<Option<String>> = vec![None; calls.len()];
        while let Some((index, outcome)) = stream.next().await {
            // 文本**原样**推给嵌入方（不在这里截断）：要少显示是展示层的事（TUI 按行截、
            // CLI 只取首行），要少回传给模型是工具自己配容量上限的事。
            on_event(TurnEvent::ToolResult {
                name: calls[index].function.name.clone(),
                content: outcome.clone().unwrap_or_else(|| CANCEL_TEXT.to_string()),
                arguments: calls[index].function.arguments.clone(),
            });
            outcomes[index] = outcome;
        }
        outcomes
    }

    /// 把历史里的 `file` 块就地换成文本占位，并把对应记录标失效（下次同图重传）。
    ///
    /// ⚠ 与 Python 有意不同：那边降级成**内联 base64**；这边不回退 base64 —— 代价是这次
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

    /// 保证这张图有一个可用的 `file_id`：先落本地副本 → 命中可用记录就复用，否则上传。
    ///
    /// 未开启（`files_api = false` / 模型不支持）或上传失败 → `None`，调用方就不注入图片
    /// （**不回退内联 base64**，与 Python 有意不同）。记录就地写进 `self.files`（下次 `save` 带上）。
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
        let (image_hash, local) = match store_blob(data, mime) {
            Ok(v) => v,
            Err(e) => {
                crate::log::warn(format!("[warn] 图片本地副本写入失败: {e}"));
                return None;
            }
        };
        if let Some(entry) = self.files.get(&image_hash) {
            if entry_is_usable(entry, &base_url, &key_fp) {
                return entry
                    .get("file_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
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
        self.files.insert(
            image_hash.clone(),
            json!({
                "hash_id": image_hash,
                "size": data.len(),
                "mime": mime,
                "filename": filename,
                "src": src,
                "local": local.display().to_string(),
                "file_id": uploaded.id,
                "base_url": base_url,
                "key_fp": key_fp,
                "uploaded_at": config::now().as_secs() as i64,
                "expires_at": uploaded.expires_at,
            }),
        );
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
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("创建会话目录失败 {}: {e}", parent.display()))?;
            }
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
            // 空表不写：别把每个会话文件都撑起来（Python 同款）
            meta["files"] = json!(self.files);
        }
        let mut out = String::new();
        out.push_str(&serde_json::to_string(&meta).map_err(|e| e.to_string())?);
        out.push('\n');
        for m in &self.messages {
            out.push_str(&serde_json::to_string(m).map_err(|e| e.to_string())?);
            out.push('\n');
        }
        fs::write(&self.path, out).map_err(|e| format!("写入会话失败 {}: {e}", self.path.display()))
    }

    /// 恢复 / 新建后的概览（CLI 提示用）。
    pub fn summary(&self) -> String {
        format!("{} 轮历史，{} 次请求", self.turn_count, self.usage.calls)
    }

    /// 手动压缩（`/compact`）：不看水位，按 `mode` 压；轮次级一路压到不能再压。
    /// **不含会话级**（整窗口归档是 `/clear` 的事，与 Python 一致）。
    #[allow(dead_code)] // 入口是交互层的 `/compact`
    pub fn compact(&mut self, mode: context::CompactMode) -> context::CompactStats {
        let manifest = self.manifest.clone();
        context::compact(
            &mut self.messages,
            &self.config,
            mode,
            Some(&mut |entry: Value| record_compact(manifest.as_deref(), &entry)),
        )
    }

    /// 本会话的压缩事件（读 manifest；没有就空）——`/stat` 与调试用。
    pub fn compression_history(&self) -> Vec<Value> {
        let Some(manifest) = &self.manifest else {
            return Vec::new();
        };
        let Ok(text) = std::fs::read_to_string(manifest) else {
            return Vec::new();
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    }

    /// 完整转录：按消息顺序**展开压缩指针**——工具级还原落盘全文，轮次级 / 会话级还原原始消息序列。
    ///
    /// 给 TUI resume 回放用：会话里存的是「摘要 + 指针」，照着回放看着没头没尾；展开才是当初界面上
    /// 真正出现过的内容（对齐 Python `Session.full_history`）。落盘文件不在（被 `context gc` 收走）
    /// 就退回压缩形式本身。
    ///
    /// 与 Python 的差异：不再把 manifest 里「已不在消息中」的原文追加到末尾——那些原文要么仍在消息
    /// 里（各自展开）、要么属于已归档的窗口块（走 `windows` 重建的摘要消息），另追加一遍只会让回放
    /// 顺序错乱。
    pub fn full_history(&self) -> Vec<Message> {
        let mut out: Vec<Message> = Vec::new();
        for m in &self.messages {
            let raw = m
                .raw_path
                .as_deref()
                .map(PathBuf::from)
                .filter(|p| p.exists());
            match (m.compress_level, raw) {
                // 轮次级 / 会话级：落盘的是整段原文（JSON 数组或 JSONL）
                (2 | 3, Some(path)) => {
                    let mut raws: Vec<Message> = context::load_window_dicts(&path)
                        .into_iter()
                        .filter_map(|v| serde_json::from_value::<Message>(v).ok())
                        .collect();
                    if m.compress_level == 2 && raws.first().is_some_and(|r| r.role == "user") {
                        raws.remove(0); // 轮次级：user 留在外层，落盘的是它后面的过程
                    }
                    out.extend(raws);
                }
                // 工具级：落盘的是被截断的那份输出全文（纯文本，不是消息）。
                //
                // 头区（`[exit=N]` / 指针行）不在落盘件里，从压缩后的消息里补回来——否则回放的
                // shell 行看不到退出码，会被当成成功（Python 版没补，算它的瑕疵：回放里失败的命令
                // 显示 〼 且不带正文）。
                (1, Some(path)) if m.role == "tool" => {
                    let pointer = context::content_text(m.content.as_ref());
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

    /// 只留 system prompt（`/clear` 的一半：换窗口）。
    #[allow(dead_code)] // 入口是交互层的 `/clear`
    pub fn reset(&mut self) {
        self.messages.truncate(1);
    }

    /// `/clear`：把当前窗口（system 与**既有窗口摘要**除外）写成一块**窗口块**落盘，开新窗口。
    ///
    /// 新窗口 = 当前 system prompt + **所有**窗口块的「摘要 + 指针」（不只最新一块，与 Python
    /// `Session.clear_window()` 同款）：可见上下文立刻瘦下来，原文仍在 `~/.pie/windows/`
    /// 可经指针回查（`full_history` / resume 都能展开）。
    ///
    /// 返回归档后手上的窗口块**总数**（调用方拿去提示）；落盘失败则原样返回、**不动窗口**
    ///（宁可不清，也不能把历史弄丢）。
    pub fn clear_window(&mut self) -> Result<usize, String> {
        // 既有窗口摘要不再入档：它们在 `self.windows` 里、由 `window_summary_messages` 统一重建
        //（否则会「摘要的摘要」层层嵌套，旧窗口的信息反而从可见上下文里掉出去）
        let old: Vec<Value> = self
            .messages
            .iter()
            .skip(1)
            .filter(|m| !(m.role == "system" && m.compress_level == 3))
            .map(|m| json!(m))
            .collect();
        if !old.is_empty() {
            let raw: String = old.iter().map(|d| format!("{d}\n")).collect();
            let path = context::write_window_block(&raw)
                .map_err(|e| format!("写窗口块失败: {e}"))?;
            let (head, tail) = self.window_sizes();
            // 与上下文压缩同一条 manifest（`pie context info` / `/stat` 看的就是它）
            record_compact(
                self.manifest.as_deref(),
                &json!({
                    "ts": config::now().as_secs() as i64,
                    "level": 3,
                    "kind": "session",
                    "raw_path": path.display().to_string(),
                    "raw_hash": context::content_hash(&raw),
                    "summary": context::summarize_turns(&old, head, tail)
                        .chars()
                        .take(200)
                        .collect::<String>(),
                }),
            );
            self.windows.push(path);
        }
        // 新窗口：只留「当前 system prompt + 各窗口块的摘要/指针」（提示词顺便重建一次）
        self.messages = std::iter::once(Message::system(config::build_system_prompt(
            &self.config,
            self.config.system_prompt.as_deref(),
            &self.config.append_system_prompt,
        )))
        .chain(self.window_summary_messages())
        .collect();
        Ok(self.windows.len())
    }

    /// 窗口块摘要的 head/tail（会话级压缩没配就用默认值，与 Python 同款）。
    fn window_sizes(&self) -> (usize, usize) {
        match self
            .config
            .compaction
            .as_ref()
            .and_then(|c| c.session.as_ref())
        {
            Some(sc) => (sc.head, sc.tail),
            None => {
                let d = config::SessionCompaction::default();
                (d.head, d.tail)
            }
        }
    }

    /// `/stat` 的报告文本（对齐 Python `usage_report`）：
    /// 上下文占用 / 水位 / 输入预算 / 各角色估算 / 压缩事件 / API 用量。
    pub fn usage_report(&self) -> String {
        let (total, label) = match self.usage.prompt_tokens {
            Some(reported) => (reported, "当前上下文占用（API 上报）："),
            None => (
                context::messages_tokens(&self.messages),
                "当前上下文占用（估算）：",
            ),
        };
        let limit = self.config.context_budget();
        let reserved = match self.config.reserved_tokens {
            Some(n) => config::thousands(n as i64),
            None => "服务端默认".to_string(),
        };
        let pct = if limit > 0 {
            total as f64 * 100.0 / limit as f64
        } else {
            0.0
        };
        let mut roles: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
        for m in &self.messages {
            *roles.entry(m.role.clone()).or_default() += context::message_tokens(m);
        }
        let roles_txt = roles
            .iter()
            .map(|(k, v)| format!("{k} {}", config::thousands(*v)))
            .collect::<Vec<_>>()
            .join(" | ");

        let mut parts: Vec<String> = Vec::new();
        if self.path.exists() {
            parts.push(format!("会话文件：{}", self.path.display()));
        }
        parts.push(format!(
            "{label}{} / {} tokens ({pct:.1}%)",
            config::thousands(total),
            config::thousands(limit as i64)
        ));
        parts.push(format!(
            "软阈值 {} ({:.0}%) | 目标水位 {} ({:.0}%)",
            config::thousands(self.config.soft_limit() as i64),
            self.config.soft_ratio() * 100.0,
            config::thousands(self.config.target_limit() as i64),
            self.config.target_ratio() * 100.0
        ));
        parts.push(format!(
            "输入预算 {} = 上下文窗口 {} − 输出预留 {reserved}",
            config::thousands(limit as i64),
            config::thousands(self.config.context_window as i64)
        ));
        parts.push(format!("各角色占用（估算）：{roles_txt}"));

        let events = self.compression_history();
        let mut counts = [0i64; 3];
        let mut evicted: i64 = 0;
        for e in &events {
            match e.get("level").and_then(Value::as_i64) {
                Some(1) => counts[0] += 1,
                Some(2) => counts[1] += 1,
                Some(3) => counts[2] += 1,
                _ => {}
            }
            if let Some(raw) = e.get("raw_path").and_then(Value::as_str) {
                if let Ok(meta) = std::fs::metadata(raw) {
                    evicted += meta.len() as i64 / 4;
                }
            }
        }
        parts.push(format!(
            "上下文压缩：{} / {} / {} (工具级 / 轮次级 / 会话级)",
            counts[0], counts[1], counts[2]
        ));
        parts.push(format!(
            "本会话已压缩 {} 次 (当前为压缩视图)，落盘原文约 {} tokens (可经指针恢复)",
            events.len(),
            config::thousands(evicted)
        ));
        parts.push(format!(
            "API 用量：\n{}",
            serde_json::to_string_pretty(&self.usage).unwrap_or_default()
        ));
        parts.join("\n")
    }

    fn at(path: PathBuf, config: &Config, llm: LlmClient, tools: ToolRegistry) -> Self {
        let manifest = Some(manifest_path_of(&path));
        Self {
            path,
            config: config.clone(),
            llm,
            tools,
            manifest,
            messages: vec![Message::system(config::build_system_prompt(
                config,
                config.system_prompt.as_deref(),
                &config.append_system_prompt,
            ))],
            usage: UsageTracker::default(),
            windows: Vec::new(),
            files: HashMap::new(),
            title: None,
            turn_count: 0,
            available_models: None,
        }
    }
}

/// 压缩 manifest 路径：`~/.pie/context/<会话名>.manifest.jsonl`（只记不读）。
fn manifest_path_of(session_path: &Path) -> PathBuf {
    let stem = session_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    context::context_dir().join(format!("{stem}.manifest.jsonl"))
}

/// 单个工具失败不拖累其他工具：任何异常都文本化后回传模型，让它自己修。
async fn dispatch_tool(
    registry: &ToolRegistry,
    call: &ToolCall,
    args: &Value,
    cancel: &Cancel,
) -> String {
    // 取消信号随 ctx 传进工具层（shell 会在等待时 race 它、杀掉整个进程组）
    let ctx = tools::ToolCtx::with_cancel(cancel.clone());
    match registry.dispatch(&call.function.name, args, ctx).await {
        Ok(text) => text,
        Err(e) => format!("[工具错误] {e}"),
    }
}

/// 历史里最后一段非空 assistant 文本（达到 `max_steps` 时拿来当「最终答复」）。
fn last_assistant_text(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|m| {
        if m.role != "assistant" {
            return None;
        }
        match &m.content {
            Some(Content::Text(t)) if !t.trim().is_empty() => Some(t.clone()),
            _ => None,
        }
    })
}

/// 历史会话概览（`pie-rs sessions` 用）。
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub path: PathBuf,
    /// 文件 mtime（unix 秒）——列表按它降序。
    pub mtime: i64,
    pub size: u64,
    /// 真实用户消息数（`synthetic` 的图片消息不算）。
    pub turns: usize,
    /// `__meta__.usage.calls`（累计 API 请求数）。
    pub api_calls: i64,
    /// 标题（`__meta__.title`），没有就用首个用户消息。
    pub first_query: String,
}

/// 列出历史会话（按 mtime 降序；`limit = None` = 全部）。
pub fn list_sessions(limit: Option<usize>) -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir(sessions_dir()) else {
        return Vec::new();
    };
    let mut files: Vec<(i64, PathBuf, u64)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| {
            let meta = p.metadata().ok()?;
            let mtime = meta
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs() as i64;
            Some((mtime, p, meta.len()))
        })
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    if let Some(limit) = limit {
        files.truncate(limit);
    }
    files
        .into_iter()
        .map(|(mtime, path, size)| {
            let mut turns = 0usize;
            let mut api_calls = 0i64;
            let mut first_query = String::new();
            if let Ok(text) = std::fs::read_to_string(&path) {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    let Ok(v) = serde_json::from_str::<Value>(line) else {
                        continue;
                    };
                    if v.get("__meta__").is_some() {
                        api_calls = v
                            .get("usage")
                            .and_then(|u| u.get("calls"))
                            .and_then(Value::as_i64)
                            .unwrap_or(0);
                        if let Some(t) = v.get("title").and_then(Value::as_str) {
                            first_query = t.to_string();
                        }
                    } else if v.get("role").and_then(Value::as_str) == Some("user")
                        && !v.get("synthetic").and_then(Value::as_bool).unwrap_or(false)
                    {
                        turns += 1;
                        if first_query.is_empty() {
                            if let Some(c) = v.get("content").and_then(Value::as_str) {
                                first_query = c.trim().to_string();
                            }
                        }
                    }
                }
            }
            SessionInfo {
                id: file_stem(&path),
                path,
                mtime,
                size,
                turns,
                api_calls,
                first_query,
            }
        })
        .collect()
}

/// 压缩事件 → manifest（`ephemeral` 会话没有 manifest 就跳过）。
fn record_compact(manifest: Option<&Path>, entry: &Value) {
    if let Some(manifest) = manifest {
        if let Err(e) = context::write_manifest(manifest, entry) {
            crate::log::warn(format!("[context] manifest 写入失败: {e}"));
        }
    }
}

/// 会话目录：`~/.pie/sessions/`（`PIE_DIR` 可重定向）。
pub fn sessions_dir() -> PathBuf {
    config::pie_dir().join("sessions")
}

// ---------------------------------------------------------------- 图片文件管理
//
// 本地那一侧的事都在这里（`llm.rs` 只放 Files API 协议）：内容寻址副本、`__meta__.files`
// 记录表、本地副本的 GC 清单。字段名与 Python 版 `files.py` 逐字对齐。

/// 本地副本目录：`~/.pie/files/`（与 `context/` 分开：那边是压缩落盘的文本）。
pub fn files_dir() -> PathBuf {
    config::pie_dir().join("files")
}

/// 本地副本的 GC 保护窗口（小时）：比这新的未引用副本一概先留着。
///
/// 理由：“未被引用”不等于“没人用”—— 刚粘进 `files/` 的图在它被某次 `read` 登记进
/// `__meta__.files` 之前没有任何引用，但路径可能正躺在输入框 / 某条命令里。
pub const GC_PROTECT_HOURS: u64 = 24;

/// 图片内容 id：`img-<sha256[:16]>`（与 `context` 的 `turn-<hash>` 同形状）。
pub fn image_hash_id(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("img-{}", &hex::encode(hasher.finalize())[..16])
}

/// 把图片复制进 `~/.pie/files/`（内容寻址、幂等），返回 `(hash_id, 副本路径)`。
///
/// 先写临时文件再 rename（避免读到别人写了一半的副本）；权限 0o600（图是用户数据）。
pub fn store_blob(data: &[u8], mime: &str) -> std::io::Result<(String, PathBuf)> {
    let image_hash = image_hash_id(data);
    let ext = match mime {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/bmp" => ".bmp",
        _ => "",
    };
    let dir = files_dir();
    let path = dir.join(format!("{image_hash}{ext}"));
    if path.exists() {
        return Ok((image_hash, path));
    }
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path)?;
    Ok((image_hash, path))
}

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

/// 遍历所有会话记录里的图片条目：`(会话文件, hash_id, 条目)`。
pub fn iter_session_files(sessions_dir: &Path) -> Vec<(PathBuf, String, Value)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return out;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        // meta 是第一行，读完就够
        let Some(first) = text.lines().find(|l| !l.trim().is_empty()) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<Value>(first) else {
            continue;
        };
        if !meta
            .get("__meta__")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let Some(map) = meta.get("files").and_then(Value::as_object) else {
            continue;
        };
        for (hash, entry) in map {
            if entry.is_object() {
                out.push((file.clone(), hash.clone(), entry.clone()));
            }
        }
    }
    out
}

/// `file_id` → 记着它的会话名（`files list --all` 标“谁传的”用）。
pub fn file_id_index() -> HashMap<String, Vec<String>> {
    let mut index: HashMap<String, Vec<String>> = HashMap::new();
    for (file, _, entry) in iter_session_files(&sessions_dir()) {
        if let Some(id) = entry.get("file_id").and_then(Value::as_str) {
            index
                .entry(id.to_string())
                .or_default()
                .push(file_stem(&file));
        }
    }
    index
}

/// `~/.pie/files/` 下没有被任何会话引用、且已经放了 `protect_hours` 小时的副本。
///
/// 副本是**跳会话共享**的（同一内容一个文件），所以“删会话”不会自动删副本 —— 回收靠这次
/// 无状态扫描；服务端那份由上传时的 `expires_after`（默认 30 天）自行过期。
pub fn collect_file_garbage(protect_hours: u64) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(files_dir()) else {
        return Vec::new();
    };
    let referenced: std::collections::HashSet<PathBuf> = iter_session_files(&sessions_dir())
        .into_iter()
        .filter_map(|(_, _, e)| e.get("local").and_then(Value::as_str).map(PathBuf::from))
        .collect();
    let now = config::now();
    let mut garbage: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            !p.file_name()
                .map(|n| n.to_string_lossy().starts_with('.'))
                .unwrap_or(false)
        })
        .filter(|p| !referenced.contains(p))
        .filter(|p| {
            // 保护窗口内 → 留着
            p.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|mtime| now.saturating_sub(mtime).as_secs() >= protect_hours * 3600)
        })
        .collect();
    garbage.sort();
    garbage
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `id` 解析：None → 时间戳文件名；纯名字 → `<sessions>/<name>.jsonl`；带目录/绝对路径 → 原样。
fn resolve_path(id: Option<&str>) -> PathBuf {
    let dir = sessions_dir();
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
    if let Some(cwd) = cwd {
        if let Some((_, path)) = files
            .iter()
            .find(|(_, p)| meta_cwd(p).as_deref() == Some(cwd))
        {
            return Some(path.clone());
        }
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

    /// 改进程级 `PIE_DIR` 的用例共用 `config::ENV_LOCK`（跟 `context` 的测试串行化）。
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::config::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn pie_dir_tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pie-rs-img-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("PIE_DIR", &dir);
        dir
    }

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("pie-rs-session-{}-{name}", std::process::id()));
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
    fn texts(outcomes: &[Option<String>]) -> Vec<String> {
        outcomes
            .iter()
            .map(|o| o.clone().unwrap_or_else(|| "[cancelled]".into()))
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
                TurnEvent::ToolResult { content, .. } => assert!(content.contains("three"), "{content}"),
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
                TurnEvent::ToolResult { content, .. } => assert!(content.contains("one"), "{content}"),
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

        // 新窗口 = system（重建）+ 窗口摘要（level 3，带指针）
        assert_eq!(s.messages.len(), 2, "{:?}", s.messages.len());
        assert_eq!(s.messages[0].role, "system");
        assert_eq!(s.messages[0].compress_level, 0);
        assert_eq!(s.messages[1].compress_level, 3);
        let block = PathBuf::from(s.messages[1].raw_path.clone().expect("带指针"));
        assert!(block.starts_with(context::windows_dir()), "{block:?}");
        assert!(block.exists(), "{block:?}");
        // 块里是**原文**（三条都在），且文件名最后一段就是内容 hash
        let raw = std::fs::read_to_string(&block).unwrap();
        for needle in ["第一问", "第一答", "第二问"] {
            assert!(raw.contains(needle), "块里丢了 {needle}：{raw}");
        }
        assert!(
            block.file_stem().unwrap().to_string_lossy().ends_with(&context::content_hash(&raw)),
            "hash 要放文件名最后一段：{block:?}"
        );
        // 摘要有窗口指针标记 + 首尾轮次
        let text = context::content_text(s.messages[1].content.as_ref());
        assert!(text.contains("[历史窗口:"), "{text}");
        assert!(text.contains("第一问"), "{text}");

        // manifest 里记了一条 level=3 的会话级事件
        let manifest = manifest_path_of(&s.path);
        let entries: Vec<Value> = std::fs::read_to_string(&manifest)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(
            entries.iter().any(|e| e["level"] == 3 && e["kind"] == "session"),
            "{entries:?}"
        );

        // 归档的信息一条不少（展开回原样）
        let full = s.full_history();
        let roles: Vec<&str> = full.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"], "{roles:?}");

        // 第二次 /clear：新窗口里没东西可归档 → 块数不变
        assert_eq!(s.clear_window().expect("再来一次"), 1);
        assert_eq!(s.windows.len(), 1);
    }


    /// 工具输出**原样**推给嵌入方：Session 不在这里截断（与 Python `clip_output(text, 500)`
    /// 有意不同）——少显示是展示层的事（TUI 按 `TOOL_BODY_LINES` 截、CLI 只取首行），
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
            assert!(text.len() > 500, "工具输出本来就很长：{} 字", text.len());
            match &events[1] {
                TurnEvent::ToolResult { content, .. } => {
                    assert_eq!(content, &text, "事件里的文本要原样（不截断）")
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
            let outcomes = session()
                .tool_call(&calls, true, &cancel, &mut sink)
                .await;
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
        let (h1, p1) = store_blob(b"same-bytes", "image/png").unwrap();
        let (h2, p2) = store_blob(b"same-bytes", "image/png").unwrap();
        assert_eq!(h1, h2, "同内容同 id");
        assert_eq!(p1, p2, "幂等：同一份副本");
        assert!(h1.starts_with("img-") && h1.len() == 20, "{h1}");
        assert!(p1.to_string_lossy().ends_with(".png"), "{p1:?}");
        assert_eq!(std::fs::read(&p1).unwrap(), b"same-bytes");
        assert_ne!(image_hash_id(b"other"), h1);
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

    #[test]
    fn garbage_needs_no_reference_and_past_protect_window() {
        let _g = env_lock();
        let dir = pie_dir_tmp("gc");
        std::fs::create_dir_all(sessions_dir()).unwrap();
        let (_, referenced) = store_blob(b"referenced", "image/png").unwrap();
        let (_, orphan_old) = store_blob(b"orphan-old", "image/png").unwrap();
        let (_, orphan_fresh) = store_blob(b"orphan-fresh", "image/png").unwrap();
        // 把孤儿副本的 mtime 拨到 2 天前（超出 24h 保护窗口）
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&orphan_old)
            .unwrap()
            .set_modified(old)
            .unwrap();
        // 一个会话的 meta 引用 referenced
        let meta = json!({
            "__meta__": true,
            "files": {"img-x": {"file_id": "f1", "local": referenced.display().to_string()}}
        });
        std::fs::write(sessions_dir().join("s.jsonl"), format!("{meta}\n")).unwrap();

        // 会话记录（`files list` 的数据源）读得出来，file_id 索引也建得出
        let rows = iter_session_files(&sessions_dir());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "img-x");
        assert!(file_id_index().contains_key("f1"));

        let garbage = collect_file_garbage(GC_PROTECT_HOURS);
        assert!(garbage.contains(&orphan_old), "{garbage:?}");
        assert!(!garbage.contains(&referenced), "被会话引用 → 不能收");
        assert!(!garbage.contains(&orphan_fresh), "保护窗口内 → 不能收");
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
        assert!(rt
            .block_on(s.ensure_image_file(b"bytes", "image/png", "x.png", "/tmp/x.png"))
            .is_none());
        assert!(s.files.is_empty());
        assert!(!files_dir().exists(), "不开这个功能就没必要多存一份副本");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 窗口摘要在 `load` 时要**重建**回来：文件里的旧 system 被丢掉，但 `meta.windows`
    /// 记着窗口块，按当前 `head/tail` 重新生成摘要 system 消息（Python 同款）。
    /// （不重建的话 resume 后模型就看不到被归档的历史了。）
    #[test]
    fn load_rebuilds_window_summaries() {
        let _g = env_lock();
        let dir = pie_dir_tmp("win");
        std::fs::create_dir_all(context::context_dir()).unwrap();
        let block = context::context_dir().join("session-abc.txt");
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
        assert_eq!(s.messages[0].compress_level, 0, "第一条是当前提示词");
        assert_eq!(s.messages[1].compress_level, 3, "窗口摘要要重建回来");
        let text = context::content_text(s.messages[1].content.as_ref());
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
        std::fs::create_dir_all(context::context_dir()).unwrap();
        let config = Config::default();
        let mut s = Session::ephemeral(&config, llm(), tools());

        // 工具级：落盘的是被截断的那份输出全文（纯文本，不是消息）
        let full_text = "line1\nline2\nline3\n";
        let spill = context::write_raw(full_text, "tool").unwrap();
        let mut tool = Message::tool_result(
            "call_1",
            "bash",
            format!(
                "[exit=0]\n\n[工具输出全文已保存: {}]\n\nline1",
                spill.display()
            ),
        );
        tool.compress_level = 1;
        tool.raw_path = Some(spill.display().to_string());
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
        let blob = serde_json::to_string(
            &Value::Array(
                raws.iter()
                    .map(|m| serde_json::to_value(m).unwrap())
                    .collect(),
            ),
        )
        .unwrap();
        let turn = context::write_raw(&blob, "turn").unwrap();
        let mut summary = Message {
            role: "assistant".into(),
            content: Some(Content::Text(format!(
                "[轮次原文已保存: {}]\n\n旧答复",
                turn.display()
            ))),
            ..Default::default()
        };
        summary.compress_level = 2;
        summary.raw_path = Some(turn.display().to_string());
        s.messages.push(summary);

        let full = s.full_history();
        let expanded = full
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call_1"))
            .expect("工具级消息还在");
        let text = context::content_text(expanded.content.as_ref());
        assert!(text.ends_with(full_text), "工具级展开成落盘全文：{text}");
        assert!(text.starts_with("[exit=0]"), "头区（退出码）要保住：{text}");
        assert!(
            full.iter().any(|m| m.role == "assistant"
                && context::content_text(m.content.as_ref()) == "旧答复"),
            "轮次级展开成原文序列：{full:?}"
        );
        assert!(
            !full.iter().any(|m| m.compress_level >= 2),
            "指针消息都展开完了"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 落盘文件不在了（被 `context gc` 收走）→ 退回压缩形式本身，不装成功。
    #[test]
    fn full_history_falls_back_when_raw_is_gone() {
        let _g = env_lock();
        let dir = pie_dir_tmp("full-history-gone");
        std::fs::create_dir_all(context::context_dir()).unwrap();
        let config = Config::default();
        let mut s = Session::ephemeral(&config, llm(), tools());

        let turn = context::write_raw("[]", "turn").unwrap();
        let mut summary = Message {
            role: "assistant".into(),
            content: Some(Content::Text(format!(
                "[轮次原文已保存: {}]\n\n旧答复",
                turn.display()
            ))),
            ..Default::default()
        };
        summary.compress_level = 2;
        summary.raw_path = Some(turn.display().to_string());
        s.messages.push(summary);
        std::fs::remove_file(&turn).unwrap();

        let full = s.full_history();
        assert!(
            full.iter().any(|m| context::content_text(m.content.as_ref())
                .contains("[轮次原文已保存")),
            "退回压缩形式：{full:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
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

    /// 用量是「最近一次上报值 + calls 累计」（不求和），与 Python 版一致。
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

    /// **跨版本兼容**：Python 版写下的会话（含未知字段 / 旧 system / null token）必须能读。
    #[test]
    fn loads_python_written_session() {
        let config = Config::default();
        let path = tmp("py.jsonl");
        let jsonl = [
            r#"{"__meta__":true,"usage":{"prompt_tokens":123,"completion_tokens":null,"total_tokens":null,"prompt_cache_hit_tokens":null,"prompt_cache_miss_tokens":null,"reasoning_tokens":null,"calls":3},"windows":[],"cwd":"/tmp","title":"旧会话"}"#,
            r#"{"role":"system","content":"SENTINEL-OLD-SYSTEM","compress_level":3,"raw_path":"/x"}"#,
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
        let dir = std::env::temp_dir().join(format!("pie-rs-sessdir-{}", std::process::id()));
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
        let empty = std::env::temp_dir().join(format!("pie-rs-sessempty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        assert!(Session::resume_in(&empty, None, &config, llm(), tools())
            .unwrap_err()
            .contains("没有历史会话"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }
}
