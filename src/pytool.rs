//! `pytool` —— **用 Python 写的工具**：起一个长活的 Python 宿主子进程，把它的工具注册进
//! [`Registry`]（与内置工具、Python 绑定注册的工具共用同一条分发链）。
//!
//! 为什么是子进程而不是嵌 CPython：不内嵌 → **零新依赖、默认构建行为不变**（没配
//! `[python] plugins` 就什么都不发生），也不必为一个可选功能把 libpython 拉进每一次构建。
//!
//! 形状（**传输与进程生命周期全交给 [`crate::pyhost`]**：一行一条 JSON、`id` 配对、首行清单、
//! 懒重启、按进程组杀 —— 本模块只管「清单 → 注册」与「调用 → 工具结果」）：
//!   - 一个宿主进程装**所有** `[python] plugins` 里列的文件（不是一文件一进程）；
//!   - 启动（`load`）时 start + 读首行清单 → 逐个 `with_dynamic`；
//!   - 调用 = 写一行请求、等一行应答，靠 `id` 配对 → **一批 tool_calls 可以真并发**
//!     （并发度由 `Session` 那侧决定：`parallel_tools` 关掉时就只会有一个在飞）；
//!   - **同步 / 异步 handler 都能装**：同步的跑在宿主的线程池里，`async def` 跑在宿主那个
//!     后台 loop 上（跨调用共享连接池）；本侧只管「写一行等一行」，两种都一样；
//!   - **宿主用真绑定**（`from pie import tool`）：那个解释器里得装着 `pie`（工具里的 `@tool` 就是它
//!     提供的）。不内嵌副本 → 实现只有一处、不会与绑定分叉；代价是 pytool 不再「零安装」。
//!   - 取消（Esc）= 按进程组 `SIGKILL`（`pyhost` 的 `Interrupt::Kill`）→ 在飞的调用一起失败
//!     → **下次调用懒重启**；
//!   - 任何失败（起不来 / 导入错 / 重名 / 名字非法）都只告警、不阻断会话。
//!
//! 与 `repl` 的关系：**共用解释器（同一个 venv）但各是各的进程**——`repl` 的 `_python` 默认
//! 取 `[python] interpreter`（见 `Config::tool_defaults`），于是「工具里 import 得到的包，
//! repl 里也 import 得到」。状态不互通（进程不同），这是有意的：`repl` 的命名空间是模型可写的，
//! 工具不该和它共享。
//!
//! 两个已知边界（都是「进程长活」的必然结果）：
//!   - **cwd 固定为启动时的工作目录**（宿主在会话开始前就起了；TUI 里 `/cd` 不会改它的 cwd，
//!     而内置工具会跟着变）——要跟 cwd 走的工具请自己用绝对路径；
//!   - 用户在**会话中途改工具文件**不会生效（schema 与 handler 都在启动时装好了），重启 `pie` 才行。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::cancel::CANCEL_TEXT;
use crate::pyhost::{DEFAULT_PYTHON, Interrupt, Outcome, PyHost, Spec};
use crate::tools::{
    CallFn, ExtensionHook, Limits, Registry, ToolBody, ToolCtx, ToolError, ToolOutput, ToolResult,
};

/// 宿主脚本正文（编译期嵌入；见该文件的协议说明）。
///
/// ⚠ 宿主**依赖真绑定**（`from pie import tool`）—— 不再内嵌 `_tool.py` / `_llm.py` 的副本：
/// 实现只有绑定那一处，不会分叉（用户拍板：宁可装真包，也不要两份代码）。
const HOST: &str = include_str!("python/pytool_host.py");

/// 装载 Python 工具：起宿主、读清单、逐个注册。**任何失败都只告警**，原样返回 `registry`。
///
/// 解释器与工具文件都从 `config` 取（`[python] interpreter` → `envs/base` → `python3`；
/// `[python] plugins` + 约定目录 `<root>/plugins`）。文件列表为空 → 直接返回（零开销，不 spawn）。
pub async fn load(registry: Registry, config: &crate::config::Config) -> Registry {
    let files = files_of(&config.python_plugin_entries());
    if files.is_empty() {
        return registry;
    }
    let python = config
        .python_interpreter()
        .unwrap_or_else(|| DEFAULT_PYTHON.to_string());
    let mut spec = Spec::new("pytool", &python, HOST);
    spec.files = files;
    // 子进程 stderr（工具里的 print / traceback）→ 逐行告警；尾巴也会附在出错的消息里
    spec.stderr_prefix = Some("pytool".to_string());
    let (host, manifest) = match PyHost::start(&spec).await {
        Ok(ok) => ok,
        Err(e) => {
            // `PyHost` 的消息已经带 `[pytool]` 前缀
            crate::log::warn(e);
            return registry;
        }
    };
    let handshake: Handshake = match serde_json::from_value(manifest) {
        Ok(handshake) => handshake,
        Err(e) => {
            crate::log::warn(format!("[pytool] 清单不是预期的形状: {e}"));
            return registry;
        }
    };
    for warning in &handshake.warnings {
        crate::log::warn(format!("[pytool] {warning}"));
    }
    // 插件报的事件订阅：每个会话建好后会把它挂到自己的事件总线上（见 `Session::attach_extensions`）。
    let hook = (!handshake.extensions.subscribe.is_empty()).then(|| ExtensionHook {
        subscribe: handshake.extensions.subscribe.clone(),
        on_event: {
            let host = Arc::clone(&host);
            Arc::new(move |event: &crate::event::Event, ctx: &Value| deliver(&host, event, ctx))
        },
    });
    let mut registry = registry.with_extensions(hook.into_iter().collect());
    for spec in handshake.tools {
        if !valid_tool_name(&spec.name) {
            crate::log::warn(format!(
                "[pytool] 跳过非法工具名 {:?}：只能用 [A-Za-z0-9_-]、长度 1..=64（API 的 function.name 要求）",
                spec.name
            ));
            continue;
        }
        // `with_dynamic` 撞重名会 panic（那是 Rust 侧注册的编程错误）——外部来的名字必须自己挡
        if registry.names().contains(&spec.name.as_str()) {
            crate::log::warn(format!(
                "[pytool] 跳过重名工具 {}（与已有工具同名）",
                spec.name
            ));
            continue;
        }
        let call: CallFn = {
            let host = Arc::clone(&host);
            let name = spec.name.clone();
            Arc::new(move |args, ctx| {
                let host = Arc::clone(&host);
                let name = name.clone();
                Box::pin(async move { call(&host, &name, args, ctx).await })
            })
        };
        registry = registry.with_dynamic(
            &spec.name,
            &spec.description,
            // 没给 schema 就按「无参数对象」发（模型至少知道有这么个工具）
            spec.parameters
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            call,
        );
    }
    registry
}

/// 一个事件交给宿主：
///
/// - **可否决的 kind**（`tool_call`）：带上 `id` 并**等回话** → 宿主回的 `deny` 就是否决理由；
/// - 其余 kind：只写一行就走（不等，返回值没人用）。
///
/// ⚠ 它是**同步**回调（`EventBus::ask` / `send` 在调用方线程上跑），所以要 `block_in_place`
/// 把当前 worker 让出去再 `block_on`（多线程 runtime 才行；当前线程 runtime 用了会 panic）。
/// 起不起 runtime 的场合（纯同步调用）→ 直接放行 + 告警，别把调用方带崩。
fn deliver(host: &PyHost, event: &crate::event::Event, ctx: &Value) -> Result<(), String> {
    let value = match crate::event::to_wire(event) {
        Ok(value) => value,
        Err(e) => {
            crate::log::warn(format!("[pytool] 事件序列化失败: {e}"));
            return Ok(());
        }
    };
    let veto = VETO_KINDS.contains(&event.name());
    // 同步回调里等异步请求：得 `block_in_place` 把当前 worker 让出去（`Handle::block_on` 自己
    // 在 runtime 里用会 panic）。⚠ 它只在**多线程 runtime** 上可用 —— `pie` 的 CLI/TUI 与绑定都是
    // 多线程（`Runtime::new()` / `new_multi_thread()`），单线程 runtime 下只能放弃投递（放行 + 告警）。
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        crate::log::warn(format!(
            "[pytool] 没有 runtime，{} 事件没发给插件",
            event.name()
        ));
        return Ok(());
    };
    if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
        crate::log::warn(format!(
            "[pytool] 单线程 runtime 下不投递事件（{}），这次当通过",
            event.name()
        ));
        return Ok(());
    }
    let payload = json!({"event": value, "ctx": ctx});
    let outcome = tokio::task::block_in_place(|| {
        handle.block_on(async {
            if veto {
                host.request(payload, None, Some(VETO_TIMEOUT), Interrupt::Kill)
                    .await
            } else {
                host.notify(payload)
                    .await
                    .map(|()| Outcome::Reply(Value::Null))
            }
        })
    });
    match outcome {
        Ok(Outcome::Reply(reply)) => {
            let deny = reply
                .get("deny")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if veto && !deny.is_empty() {
                return Err(deny);
            }
            Ok(())
        }
        // 取消 / 超时 → 放行（fail-open）；宿主没了 → 告警 + 放行
        Ok(_) => Ok(()),
        Err(e) => {
            crate::log::warn(e);
            Ok(())
        }
    }
}

/// 一次工具调用：把参数转给宿主，把应答转成工具结果。
///
/// 截断阈值走与内置工具同一条路：`[tools.<名字>] _max_lines = N` 注入。
/// ⚠ 这两个是**宿主**的旋钮（与 `bash` 的 `_max_lines` 同义）→ 从 args 里摘掉再转发，
/// 否则没声明它们的 handler 会拿到一个多余的 kwarg 直接 TypeError。其余 `_` 开头的参数照旧
/// 转发给 handler（与 Python 绑定同款：用户在 handler 签名里声明 `_foo` 就收得到）。
async fn call(host: &PyHost, tool: &str, args: Value, ctx: ToolCtx) -> ToolResult {
    let mut args = args;
    let (max_lines, max_bytes) = match args.as_object_mut() {
        Some(map) => (
            map.remove("_max_lines").and_then(|v| v.as_i64()),
            map.remove("_max_bytes").and_then(|v| v.as_i64()),
        ),
        None => (None, None),
    };
    let payload = json!({"tool": tool, "arguments": args});
    match host
        .request(payload, ctx.cancel.as_ref(), None, Interrupt::Kill)
        .await
    {
        Ok(Outcome::Reply(value)) => match serde_json::from_value::<Reply>(value) {
            Ok(reply) => reply.into_result(&ctx, max_lines, max_bytes),
            Err(e) => Err(ToolError(format!("[pytool] 应答不是预期的形状: {e}"))),
        },
        // 取消 = 杀掉整个进程组（在飞的含本次一起失败，下次调用懒重启）——
        // 不做「中断单次调用但保住状态」，那要把中断协议搬进 Python，不值。
        Ok(Outcome::Cancelled) => Ok(ToolOutput::text(CANCEL_TEXT)),
        // pytool 没设超时，这条路上不该出现
        Ok(Outcome::TimedOut) => Err(ToolError("[pytool] 宿主超时".to_string())),
        Err(e) => Err(ToolError(e)),
    }
}

/// 展开路径：`~` → `$HOME`；目录取其下 `*.py`（不递归、排过序）；不存在 → 告警。
fn files_of(entries: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in entries {
        let path = PathBuf::from(crate::config::expand_tilde(entry));
        match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => match std::fs::read_dir(&path) {
                Ok(dir) => {
                    let mut found: Vec<PathBuf> = dir
                        .filter_map(Result::ok)
                        .map(|e| e.path())
                        .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "py"))
                        .collect();
                    found.sort();
                    if found.is_empty() {
                        crate::log::warn(format!("[pytool] 目录里没有 *.py：{}", path.display()));
                    }
                    out.append(&mut found);
                }
                Err(e) => crate::log::warn(format!("[pytool] 读不了目录 {}: {e}", path.display())),
            },
            Ok(_) => out.push(path),
            Err(e) => crate::log::warn(format!("[pytool] 找不到 {}: {e}", path.display())),
        }
    }
    out
}

/// 工具名得是 `[A-Za-z0-9_-]{1,64}`（OpenAI 兼容端点对 `function.name` 的硬要求；
/// 与 Python 绑定 `register` 的那道校验同规矩）。
fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

// ---------------------------------------------------------------- 宿主

/// 清单里的一条工具（宿主给的字段名与 OpenAI 线上形状对齐）。
#[derive(Clone, Deserialize)]
struct ToolSpec {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    parameters: Option<Value>,
}

/// 宿主首行清单（`pytool_host.py` 给的形状）。
#[derive(Default, Deserialize)]
struct Handshake {
    #[serde(default)]
    tools: Vec<ToolSpec>,
    /// 插件声明的事件订阅（`{"subscribe": ["tool_call", …]}`；没写 = 不订阅）。
    #[serde(default)]
    extensions: Extensions,
    #[serde(default)]
    warnings: Vec<String>,
}

/// 清单里的 `extensions` 段。
#[derive(Default, Deserialize)]
struct Extensions {
    #[serde(default)]
    subscribe: Vec<String>,
}

/// 可否决的 kind（「动作前」的事件）——只有它要等回话；其余只发一行就走。
const VETO_KINDS: [&str; 1] = ["tool_call"];

/// 等宿主表态的上限：超时就**放行 + 告警**。
///
/// 一个卡住的插件不该把工具全卡死（fail-open）；真要 fail-closed 得另说（见 `docs/extension.md`）。
const VETO_TIMEOUT: Duration = Duration::from_secs(5);

/// 宿主的一行应答。
///
/// ⚠ `id` 不在这里：它由 [`crate::pyhost`] 用来配对（`serde` 会忽略多出来的键）。
#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

impl Reply {
    /// 应答 → 工具结果：`error` 文本化（`session` 会包成 `[工具错误] …`），正文过截断/落盘。
    fn into_result(
        self,
        ctx: &ToolCtx,
        max_lines: Option<i64>,
        max_bytes: Option<i64>,
    ) -> ToolResult {
        if let Some(error) = self.error {
            return Err(ToolError(error));
        }
        let text = self.text.unwrap_or_default();
        // 与 `bash` / `repl` 同款：单元格输出一旦被消费就没了（不可再生）→ 超限把**全文**落盘、
        // 正文只留头段预览（截断/落盘/指针的规矩都在 `ToolBody::fit` 里一处；前缀取 `ctx.name`）。
        Ok(ToolOutput {
            headers: Vec::new(),
            body: ToolBody::fit(
                ctx,
                &text,
                Limits {
                    lines: max_lines,
                    bytes: max_bytes,
                },
            ),
            attachments: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::process::Stdio;

    use crate::session::Session;
    use std::time::Duration;

    /// 一个**插件**（不是工具）：只订阅 `tool_call`，含 `rm ` 就否决。
    /// 理由里带上 `S.session_id` —— 证明帧里的 `ctx`（会话身份）也到位了。
    const PLUGIN: &str = r#"
def no_rm(ev, S):
    if ev["name"] == "bash" and "rm " in ev["arguments"]["command"]:
        return "策略拒绝：检测到 rm（来自插件 %s）" % S.session_id
    return None

session.on("tool_call", no_rm)
"#;

    /// 宿主**依赖真绑定**（工具里 `from pie import tool`），所以测试解释器得能 `import pie`：
    /// 先试绑定自己的 venv（本仓开发态就是它），再退 `python3`（用户自己往里装过）。
    /// 都没有 → `None`，那几条用例直接跳过（不在 CI 上装绑定也能跑整个套件）。
    fn host_python() -> Option<&'static str> {
        const BINDING_PY: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/bindings/pie-py/.venv/bin/python"
        );
        [BINDING_PY, "python3"].into_iter().find(|python| {
            std::process::Command::new(python)
                .arg("-c")
                .arg("import json, concurrent.futures, pie")
                .stderr(Stdio::null()) // 探测失败别往测试输出里喷 traceback
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
    }

    /// 临时工具文件 + 注册表（没有 python3 就跳过——本机 Linux/macOS 一般都有）。
    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str, body: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("pie-pytool-test-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("tools.py"), body).unwrap();
            Self { dir }
        }

        /// 测试用配置：解释器 + 只装这个目录里的工具。
        fn config(&self, python: &str) -> crate::config::Config {
            crate::config::Config {
                python: crate::config::PythonConfig {
                    interpreter: Some(python.to_string()),
                    plugins: vec![self.dir.to_string_lossy().into_owned()],
                },
                storage: crate::config::Storage::at(self.dir.join("storage")),
                ..Default::default()
            }
        }

        fn ctx(&self, cancel: crate::cancel::Cancel) -> ToolCtx {
            ToolCtx::with_cancel(
                cancel,
                crate::config::Storage::at(self.dir.join("storage")),
                Arc::new(crate::state::SessionState::default()),
            )
            .named("pytool")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const TOOLS: &str = r#"
from pie import tool
import os
import time

@tool(description="回显")
def echo(text: str) -> str:
    return "echo:" + text

@tool
def bare(text: str) -> str:
    return "bare:" + text

@tool()
def slow(seconds: float, marker: str = "x") -> str:
    time.sleep(seconds)
    return "slept:" + marker

@tool()
def boom() -> str:
    raise ValueError("nope")

@tool()
def not_a_string() -> str:
    return 42

@tool()
def die() -> str:
    os._exit(3)

@tool()
def loud() -> str:
    print("工具里的 print")
    return "ok"
"#;

    /// **插件（独立进程）能拦下工具**：清单报 `extensions.subscribe` → `Session` 把订阅挂到自己
    /// 的事件总线上 → `tool_call` 前那一下 `ask` 把事件发给插件、拿回否决理由。
    ///
    /// `#[tokio::test(flavor = "multi_thread")]`：宿主桥在同步回调里要 `block_in_place` 等回话，
    /// 单线程 runtime 上用不了（生产里 CLI/TUI 与绑定都是多线程）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_plugin_can_veto_a_tool_call() {
        let Some(python) = host_python() else { return };
        let fixture = Fixture::new("veto", PLUGIN);
        let config = fixture.config(python);
        let registry = load(Registry::empty(HashMap::new()), &config).await;

        // ① 清单里的订阅被读到
        assert_eq!(registry.extensions().len(), 1, "该有一个订阅声明");
        assert_eq!(registry.extensions()[0].subscribe, ["tool_call"]);

        // ② 会话建好 → 订阅挂上 → `ask`（= 工具执行前那一下）拿到插件的否决理由
        let llm = crate::llm::LlmClient::new(&config).expect("client");
        let session = Session::new(&config, Some("veto.jsonl"), llm, registry);
        let call = |command: &str| {
            crate::event::Event::Turn(crate::session::TurnEvent::ToolCall {
                name: "bash".into(),
                arguments: json!({"command": command}).to_string(),
            })
        };
        let denials = session.bus_handle().ask(&call("rm -rf /tmp/x"));
        assert_eq!(denials.len(), 1, "{denials:?}");
        assert!(denials[0].contains("检测到 rm"), "{denials:?}");
        assert!(
            denials[0].contains("来自插件"),
            "理由里该带会话身份（S.session_id）：{denials:?}"
        );

        // ③ 不危险的命令 → 插件返回 None → 放行
        assert!(
            session.bus_handle().ask(&call("echo hi")).is_empty(),
            "没否决就该是空集"
        );
    }

    #[tokio::test]
    async fn registers_python_tools_and_parses_schema() {
        let Some(python) = host_python() else { return };
        let fixture = Fixture::new("register", TOOLS);
        let registry = load(Registry::empty(HashMap::new()), &fixture.config(python)).await;

        let names = registry.names();
        for want in ["echo", "bare", "slow", "boom", "not_a_string", "die"] {
            assert!(names.contains(&want), "缺工具 {want}：{names:?}");
        }
        let spec = registry
            .tool_specs()
            .into_iter()
            .find(|s| s["function"]["name"] == "echo")
            .expect("echo 的 schema");
        assert_eq!(spec["function"]["description"], "回显");
        assert_eq!(spec["function"]["parameters"]["required"], json!(["text"]));
        assert_eq!(
            spec["function"]["parameters"]["properties"]["text"]["type"],
            "string"
        );
        // `@tool` **裸用**（不带括号）也得注册：曾经它会返回 `decorate` 而不是 `Tool`，
        // 于是工具静默消失（描述也退到函数名）
        let bare = registry
            .tool_specs()
            .into_iter()
            .find(|s| s["function"]["name"] == "bare")
            .expect("裸用 `@tool` 的 schema");
        assert_eq!(bare["function"]["description"], "bare");
        assert_eq!(bare["function"]["parameters"]["required"], json!(["text"]));
    }

    #[tokio::test]
    async fn calls_tool_and_textualizes_errors() {
        let Some(python) = host_python() else { return };
        let fixture = Fixture::new("call", TOOLS);
        let registry = load(Registry::empty(HashMap::new()), &fixture.config(python)).await;
        let ctx = fixture.ctx(crate::cancel::Cancel::new());

        let out = registry
            .dispatch("echo", &json!({"text": "你好"}), ctx.clone())
            .await
            .expect("echo 成功");
        assert_eq!(out.to_text(), "echo:你好");

        // 中文来回：`-X utf8` 没生效的话这里会炸/乱码
        let err = registry
            .dispatch("boom", &json!({}), ctx.clone())
            .await
            .expect_err("boom 失败");
        assert!(err.0.contains("ValueError"), "异常要文本化：{}", err.0);

        let err = registry
            .dispatch("not_a_string", &json!({}), ctx.clone())
            .await
            .expect_err("返回值必须 str");
        assert!(err.0.contains("必须返回 str"), "{}", err.0);
    }

    /// 一批调用并发跑：两个各睡 0.3s 的工具，总耗时远小于 0.6s（串行就会 ≥0.6s）。
    #[tokio::test]
    async fn parallel_calls_do_not_queue() {
        let Some(python) = host_python() else { return };
        let fixture = Fixture::new("parallel", TOOLS);
        let registry = load(Registry::empty(HashMap::new()), &fixture.config(python)).await;
        let ctx = fixture.ctx(crate::cancel::Cancel::new());

        let started = std::time::Instant::now();
        let args_a = json!({"seconds": 0.3, "marker": "a"});
        let args_b = json!({"seconds": 0.3, "marker": "b"});
        let (a, b) = tokio::join!(
            registry.dispatch("slow", &args_a, ctx.clone()),
            registry.dispatch("slow", &args_b, ctx.clone()),
        );
        let elapsed = started.elapsed();
        assert_eq!(a.unwrap().to_text(), "slept:a");
        assert_eq!(b.unwrap().to_text(), "slept:b");
        assert!(
            elapsed < Duration::from_millis(550),
            "两个 0.3s 的调用应当重叠跑（实测 {elapsed:?}）"
        );
    }

    /// async handler：整批跑在宿主的**同一个后台 loop** 上（跨调用复用连接池、批内还能重叠），
    /// 协程里抛异常照样文本化，宿主不重启。
    #[tokio::test]
    async fn async_handlers_share_one_background_loop() {
        const BODY: &str = r#"
import asyncio
from pie import tool

@tool()
async def loopid() -> str:
    return "loop=%d" % id(asyncio.get_running_loop())

@tool()
async def slow(seconds: float, marker: str) -> str:
    await asyncio.sleep(seconds)
    return "slept:" + marker

@tool()
async def boom() -> str:
    raise RuntimeError("协程里炸了")
"#;
        let Some(python) = host_python() else { return };
        let fixture = Fixture::new("async", BODY);
        let registry = load(Registry::empty(HashMap::new()), &fixture.config(python)).await;
        let ctx = fixture.ctx(crate::cancel::Cancel::new());

        let first = registry
            .dispatch("loopid", &json!({}), ctx.clone())
            .await
            .expect("loopid 成功");
        let second = registry
            .dispatch("loopid", &json!({}), ctx.clone())
            .await
            .expect("loopid 成功");
        assert_eq!(
            first.to_text(),
            second.to_text(),
            "两次调用得在同一个 loop 上"
        );

        let started = std::time::Instant::now();
        let args_a = json!({"seconds": 0.3, "marker": "a"});
        let args_b = json!({"seconds": 0.3, "marker": "b"});
        let (a, b) = tokio::join!(
            registry.dispatch("slow", &args_a, ctx.clone()),
            registry.dispatch("slow", &args_b, ctx.clone()),
        );
        assert_eq!(a.unwrap().to_text(), "slept:a");
        assert_eq!(b.unwrap().to_text(), "slept:b");
        assert!(
            started.elapsed() < Duration::from_millis(550),
            "两个 0.3s 的异步调用应当重叠跑（实测 {:?}）",
            started.elapsed()
        );

        let err = registry
            .dispatch("boom", &json!({}), ctx.clone())
            .await
            .expect_err("协程抛异常要文本化");
        assert!(err.0.contains("协程里炸了"), "{}", err.0);
        let after = registry
            .dispatch("loopid", &json!({}), ctx)
            .await
            .expect("宿主还活着");
        assert_eq!(
            after.to_text(),
            first.to_text(),
            "同一个 loop（宿主没被重启）"
        );
    }

    /// `[tools.<名字>]` 里 `_` 开头的键会注入到 Python 工具的 handler —— 与内置工具同一条
    /// `Registry::inject_defaults`（按**注册名**查表，跟工具是怎么注册的无关）。
    #[tokio::test]
    async fn config_defaults_reach_python_handlers() {
        const BODY: &str = r#"
from pie import tool

@tool()
def peek(text: str, _secret: str = "没注入") -> str:
    return "%s|%s" % (text, _secret)

@tool()
def tight(text: str) -> str:
    return text
"#;
        let Some(python) = host_python() else { return };
        let fixture = Fixture::new("defaults", BODY);
        let with_secret = |name: &str| {
            let mut table = toml::Table::new();
            table.insert(
                "_secret".to_string(),
                toml::Value::String("来自配置".to_string()),
            );
            let mut defaults: HashMap<String, toml::Table> = HashMap::new();
            defaults.insert(name.to_string(), table);
            defaults
        };
        let ctx = fixture.ctx(crate::cancel::Cancel::new());

        let registry = load(
            Registry::empty(with_secret("peek")),
            &fixture.config(python),
        )
        .await;
        let out = registry
            .dispatch("peek", &json!({"text": "hi"}), ctx.clone())
            .await
            .expect("注入的 `_secret` 该被 handler 收到");
        assert_eq!(out.to_text(), "hi|来自配置");

        // 显式传参优先（`inject_defaults` 只塞「还没有的键」）
        let out = registry
            .dispatch(
                "peek",
                &json!({"text": "hi", "_secret": "显式"}),
                ctx.clone(),
            )
            .await
            .expect("显式参数该压过配置");
        assert_eq!(out.to_text(), "hi|显式");

        // 签名里没声明那个 `_` 键 → 宿主照样当 kwarg 传下去 → TypeError（不是静默忽略）
        let registry = load(
            Registry::empty(with_secret("tight")),
            &fixture.config(python),
        )
        .await;
        let err = registry
            .dispatch("tight", &json!({"text": "hi"}), ctx)
            .await
            .expect_err("没声明的注入键会炸");
        assert!(
            err.0.contains("unexpected keyword argument"),
            "该是 Python 的 TypeError：{}",
            err.0
        );
    }

    /// 工具文件**不用 import** 也能用 `@tool`：宿主把 `tool` / `Tool` 注进了每个被加载模块的 globals。
    #[tokio::test]
    async fn tool_names_are_injected_into_module_globals() {
        const BODY: &str = r#"
@tool(description="连 import 都不用写")
def ping(text: str) -> str:
    return "pong:" + text
"#;
        let Some(python) = host_python() else { return };
        let fixture = Fixture::new("inject", BODY);
        let registry = load(Registry::empty(HashMap::new()), &fixture.config(python)).await;
        let ctx = fixture.ctx(crate::cancel::Cancel::new());
        let out = registry
            .dispatch("ping", &json!({"text": "hi"}), ctx)
            .await
            .expect("ping 成功");
        assert_eq!(out.to_text(), "pong:hi");
    }

    /// 取消：`Esc` → 返回 CANCEL_TEXT 并杀掉宿主；**下次调用还能用**（懒重启）。
    #[tokio::test]
    async fn cancel_kills_and_next_call_restarts() {
        let Some(python) = host_python() else { return };
        let fixture = Fixture::new("cancel", TOOLS);
        let registry = load(Registry::empty(HashMap::new()), &fixture.config(python)).await;

        let cancel = crate::cancel::Cancel::new();
        let cancelled = fixture.ctx(cancel.clone());
        cancel.cancel();
        let out = registry
            .dispatch("slow", &json!({"seconds": 5.0}), cancelled)
            .await
            .expect("取消不是错误");
        assert_eq!(out.to_text(), CANCEL_TEXT);

        // 新的一轮：进程该被重启，工具照常可用
        let ctx = fixture.ctx(crate::cancel::Cancel::new());
        assert_eq!(
            registry
                .dispatch("echo", &json!({"text": "again"}), ctx)
                .await
                .unwrap()
                .to_text(),
            "echo:again"
        );
    }

    /// 宿主进程猝死（工具里 `os._exit`）→ 本次调用报错、**下一次调用自动重启**。
    #[tokio::test]
    async fn dead_host_is_restarted_on_demand() {
        let Some(python) = host_python() else { return };
        let fixture = Fixture::new("dead", TOOLS);
        let registry = load(Registry::empty(HashMap::new()), &fixture.config(python)).await;
        let ctx = fixture.ctx(crate::cancel::Cancel::new());

        let err = registry.dispatch("die", &json!({}), ctx.clone()).await;
        assert!(err.is_err(), "宿主死了，本次调用必须失败");

        assert_eq!(
            registry
                .dispatch("echo", &json!({"text": "back"}), ctx)
                .await
                .unwrap()
                .to_text(),
            "echo:back"
        );
    }

    /// `[tools.<名字>] _max_lines` 是**宿主**的旋钮：不传给 handler（没声明它的工具不能因此报错），
    /// 超限 → 全文落盘 + 指针 + `spill`。
    #[tokio::test]
    async fn host_knob_truncates_and_spills() {
        let Some(python) = host_python() else { return };
        let fixture = Fixture::new(
            "spill",
            "from pie import tool\n\n@tool()\ndef big() -> str:\n    return '\\n'.join('line %d' % i for i in range(100))\n",
        );
        let mut table = toml::Table::new();
        table.insert("_max_lines".to_string(), toml::Value::Integer(2));
        let mut defaults = HashMap::new();
        defaults.insert("big".to_string(), table);

        let registry = load(Registry::empty(defaults), &fixture.config(python)).await;
        let ctx = fixture.ctx(crate::cancel::Cancel::new());
        let out = registry
            .dispatch("big", &json!({}), ctx)
            .await
            .expect("big 成功");
        assert!(
            out.to_text().contains("[工具输出全文已保存: "),
            "超限要给指针：{}",
            out.to_text()
        );
        assert!(
            out.to_text().contains("line 0\nline 1"),
            "只留开头：{}",
            out.to_text()
        );
        assert!(
            !out.to_text().contains("line 9"),
            "尾巴被截掉：{}",
            out.to_text()
        );

        // 全文在盘上（`spill` 是结构化给的，不用拿指针文本去嗅探）
        let spill = out.body.spill.expect("落盘路径要结构化带上");
        let full = std::fs::read_to_string(&spill).unwrap();
        assert!(full.contains("line 99"), "落盘的是全文");
    }

    /// 用户工具里的 `print` 不能污染协议（它走 stderr，工具结果照样正确）。
    #[tokio::test]
    async fn tool_prints_do_not_corrupt_protocol() {
        let Some(python) = host_python() else { return };
        let fixture = Fixture::new("loud", TOOLS);
        let registry = load(Registry::empty(HashMap::new()), &fixture.config(python)).await;
        let ctx = fixture.ctx(crate::cancel::Cancel::new());
        assert_eq!(
            registry
                .dispatch("loud", &json!({}), ctx)
                .await
                .unwrap()
                .to_text(),
            "ok"
        );
    }

    /// 起不来 / 文件不在 / 导入抛异常 / 重名 → 只告警，注册表原样（绝不 panic）。
    #[tokio::test]
    async fn failures_only_warn() {
        let fixture = Fixture::new("broken", "raise RuntimeError('导入期就炸')\n");
        // 解释器都不存在
        let before = Registry::new(HashMap::new());
        let after = load(
            before.clone(),
            &fixture.config("definitely-not-an-interpreter-xyz"),
        )
        .await;
        assert_eq!(after.names(), before.names());

        // 导入期抛异常
        let Some(python) = host_python() else { return };
        let before = Registry::new(HashMap::new());
        let after = load(before.clone(), &fixture.config(python)).await;
        assert_eq!(after.names(), before.names());

        // 路径不存在
        let mut nowhere = fixture.config(python);
        nowhere.python.plugins = vec!["/definitely/not/here".to_string()];
        let before = Registry::new(HashMap::new());
        let after = load(before.clone(), &nowhere).await;
        assert_eq!(after.names(), before.names());
    }

    /// 与内置工具重名 / 名字非法 → 跳过那条，不动已有注册表。
    #[tokio::test]
    async fn name_clash_is_skipped() {
        let Some(python) = host_python() else { return };
        let fixture = Fixture::new(
            "clash",
            r#"
from pie import tool

@tool()
def read(path: str) -> str:
    return "劫持 read"

@tool(name="坏名字")
def bad() -> str:
    return "x"

@tool()
def fine() -> str:
    return "ok"
"#,
        );
        let before = Registry::new(HashMap::new());
        let after = load(before.clone(), &fixture.config(python)).await;
        let names = after.names();
        assert_eq!(
            names.iter().filter(|n| **n == "read").count(),
            1,
            "内置 read 不能被顶掉：{names:?}"
        );
        assert!(names.contains(&"fine"));
        assert!(!names.contains(&"坏名字"));
    }

    /// 目录展开：只取 `*.py`、不递归、排过序；`~` 会展开。
    #[test]
    fn files_of_expands_directories_and_tilde() {
        let dir = std::env::temp_dir().join(format!("pie-pytool-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("b.py"), "").unwrap();
        std::fs::write(dir.join("a.py"), "").unwrap();
        std::fs::write(dir.join("c.txt"), "").unwrap();

        let files = files_of(&[dir.to_string_lossy().to_string()]);
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, ["a.py", "b.py"], "只取 *.py、排过序、不递归");

        // 单个文件原样收下
        let one = files_of(&[dir.join("c.txt").to_string_lossy().to_string()]);
        assert_eq!(one.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_names_must_match_api_rules() {
        assert!(valid_tool_name("echo"));
        assert!(valid_tool_name("word_count-2"));
        assert!(!valid_tool_name(""));
        assert!(!valid_tool_name("有中文"));
        assert!(!valid_tool_name("a.b"));
        assert!(!valid_tool_name(&"x".repeat(65)));
    }
}
