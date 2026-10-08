//! `pytool` —— **用 Python 写的工具**：起一个长活的 Python 宿主子进程，把它的工具注册进
//! [`ToolRegistry`]（与内置工具、Python 绑定注册的工具共用同一条分发链）。
//!
//! 为什么是子进程而不是嵌 CPython：不内嵌 → **零新依赖、默认构建行为不变**（没配
//! `[python] tools` 就什么都不发生），也不必为一个可选功能把 libpython 拉进每一次构建。
//!
//! 形状（协议见 `pytool_host.py`）：
//!   - 一个宿主进程装**所有** `[python] tools` 里列的文件（不是一文件一进程）；
//!   - 启动（`load`）时 spawn + 读第一行清单 → 逐个 `with_dynamic`；
//!   - 调用 = 写一行请求、等一行应答，靠 `id` 配对 → **一批 tool_calls 可以真并发**
//!     （并发度由 `Session` 那侧决定：`parallel_tools` 关掉时就只会有一个在飞）；
//!   - 取消（Esc）= `killpg` 杀进程组 → 所有在飞的调用一起失败 → **下次调用懒重启**；
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

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex as AsyncMutex, oneshot};

use crate::cancel::CANCEL_TEXT;
use crate::config::StoreType;
use crate::tools::{
    CallFn, ToolCtx, ToolError, ToolOutput, ToolRegistry, ToolResult, format_output, head_prefix,
};

/// 宿主脚本正文（编译期嵌入；见该文件的协议说明）。
const HOST: &str = include_str!("python/pytool_host.py");
/// `@pie.tool` 的实现正文——**直接复用绑定那一份文件**（schema 生成只有一处实现，不会分叉）。
const TOOL_PY: &str = include_str!("../bindings/pie-py/python/pie/_tool.py");

/// 默认解释器：Unix `python3`，其它 `python`（与 `repl` 同口径）。
#[cfg(unix)]
const DEFAULT_PYTHON: &str = "python3";
#[cfg(not(unix))]
const DEFAULT_PYTHON: &str = "python";

/// 启动期等清单的上限：工具文件在 import 期卡死时别把 `pie` 也拖住（超时就放弃这些工具）。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// 装载 Python 工具：起宿主、读清单、逐个注册。**任何失败都只告警**，原样返回 `registry`。
///
/// `entries` 是配置给的路径（`[python] tools` + `<root>/tools`；文件或目录；目录取其下 `*.py`，不递归）。
/// 空 → 直接返回（零开销，不 spawn）。
pub async fn load(
    registry: ToolRegistry,
    python: Option<&str>,
    entries: &[String],
) -> ToolRegistry {
    let files = files_of(entries);
    if files.is_empty() {
        return registry;
    }
    let python = python.unwrap_or(DEFAULT_PYTHON);
    let host = match Host::start(python, files).await {
        Ok(host) => Arc::new(host),
        Err(e) => {
            crate::log::warn(format!("[pytool] {e}"));
            return registry;
        }
    };
    for warning in &host.warnings {
        crate::log::warn(format!("[pytool] {warning}"));
    }

    let mut registry = registry;
    for spec in host.tools.clone() {
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
                Box::pin(async move { host.call(&name, args, ctx).await })
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

/// 应答路由表：`id` →（**进程代**，应答通道）。见 [`Host::pending`]。
type Pending = Arc<Mutex<HashMap<u64, (u64, oneshot::Sender<Reply>)>>>;

/// 一个长活 Python 宿主进程 + 「本会话所有在飞调用」的应答路由表。
struct Host {
    python: String,
    files: Vec<PathBuf>,
    /// 启动时拿到的清单与告警（重启不再重取：schema 已经注册进 registry 了）。
    tools: Vec<ToolSpec>,
    warnings: Vec<String>,
    /// 子进程：`None` = 还没起或已死（下次调用懒重启）。锁**只护「起进程 + 写一行」**，不等应答。
    proc: AsyncMutex<Option<Proc>>,
    /// `id` →（**进程代**，应答通道）：写请求前挂上、收到应答或出错时摘掉。
    /// 代是用来防串台的：读线程 EOF 时只清自己那一代（重启后新进程的调用可能已经挂在表里了）。
    pending: Pending,
    next_id: AtomicU64,
    /// 子进程代：每重启一次 +1。
    generation: AtomicU64,
}

struct Proc {
    stdin: ChildStdin,
    pid: u32,
    /// 本进程的代（应答路由用；见 `Host::pending`）。
    generation: u64,
    /// 读线程（stdout EOF / 被杀）置 false → 下次调用重启。
    alive: Arc<AtomicBool>,
}

/// 清单里的一条工具（宿主给的字段名与 OpenAI 线上形状对齐）。
#[derive(Clone, Deserialize)]
struct ToolSpec {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    parameters: Option<Value>,
}

/// 宿主的一行应答。
#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

impl Host {
    /// 起进程 + 读清单（第一行）+ 交给读线程。失败 → `Err(说明)`。
    async fn start(python: &str, files: Vec<PathBuf>) -> Result<Self, String> {
        let host = Self {
            python: python.to_string(),
            files,
            tools: Vec::new(),
            warnings: Vec::new(),
            proc: AsyncMutex::new(None),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            generation: AtomicU64::new(0),
        };
        let (proc, tools, warnings) = host.spawn().await?;
        Ok(Self {
            tools,
            warnings,
            proc: AsyncMutex::new(Some(proc)),
            ..host
        })
    }

    /// spawn 一个宿主进程，读第一行清单，把剩余的 stdout 交给读线程。
    async fn spawn(&self) -> Result<(Proc, Vec<ToolSpec>, Vec<String>), String> {
        // `_tool.py` 的 `from __future__ import annotations` 必须是程序第一条语句 → 它必须拼在最前
        let program = format!("{TOOL_PY}\n{HOST}");
        let mut cmd = tokio::process::Command::new(&self.python);
        // `-X utf8`：中文经 stdin/stdout 进出不能靠 locale；`-u`：别让 stdout 缓冲住一行应答
        cmd.arg("-X").arg("utf8").arg("-u").arg("-c").arg(&program);
        cmd.args(&self.files);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.kill_on_drop(true); // 谁都不管它时别留孤儿（配合下面的读线程）
        #[cfg(unix)]
        cmd.process_group(0); // 独立进程组：取消时按组杀
        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "启动解释器失败（{}）: {e}（可用 [python] interpreter 指到别的解释器）",
                self.python
            )
        })?;

        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let pid = child.id().unwrap_or(0);

        // stderr 是用户工具自己的输出（print / traceback）→ 走告警通道
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    crate::log::warn(format!("[pytool] {line}"));
                }
            }
        });

        let mut reader = BufReader::new(stdout);
        let mut first = String::new();
        let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, reader.read_line(&mut first)).await;
        match handshake {
            Err(_) => {
                return Err(format!(
                    "宿主 {}s 内没有给出工具清单（工具文件是不是在 import 期卡住了？）",
                    HANDSHAKE_TIMEOUT.as_secs()
                ));
            }
            Ok(Err(e)) => return Err(format!("读清单失败: {e}")),
            Ok(Ok(0)) => {
                return Err("宿主没有给出工具清单就退出了（看上面的 [pytool] 告警）".to_string());
            }
            Ok(Ok(_)) => {}
        }
        #[derive(Deserialize)]
        struct Handshake {
            #[serde(default)]
            tools: Vec<ToolSpec>,
            #[serde(default)]
            warnings: Vec<String>,
        }
        let handshake: Handshake = serde_json::from_str(&first)
            .map_err(|e| format!("清单不是合法 JSON: {e}（原文: {}）", first.trim()))?;

        let alive = Arc::new(AtomicBool::new(true));
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        tokio::spawn(reader_loop(
            reader,
            child,
            Arc::clone(&self.pending),
            Arc::clone(&alive),
            generation,
        ));
        Ok((
            Proc {
                stdin,
                pid,
                generation,
                alive,
            },
            handshake.tools,
            handshake.warnings,
        ))
    }

    /// 一次调用：写一行请求 → 等这一 `id` 的应答（race 取消）。
    async fn call(&self, tool: &str, args: Value, ctx: ToolCtx) -> ToolResult {
        // 截断阈值走与内置工具同一条路：`[tools.<名字>] _max_lines = N` 注入。
        // ⚠ 这两个是**宿主**的旋钮（与 `bash` 的 `_max_lines` 同义）→ 从 args 里摘掉再转发，
        // 否则没声明它们的 handler 会拿到一个多余的 kwarg 直接 TypeError。其余 `_` 开头的参数照旧
        // 转发给 handler（与 Python 绑定同款：用户在 handler 签名里声明 `_foo` 就收得到）。
        let mut args = args;
        let (max_lines, max_bytes) = match args.as_object_mut() {
            Some(map) => (
                map.remove("_max_lines").and_then(|v| v.as_i64()),
                map.remove("_max_bytes").and_then(|v| v.as_i64()),
            ),
            None => (None, None),
        };

        let mut guard = self.proc.lock().await;
        let dead = guard
            .as_ref()
            .map(|p| !p.alive.load(Ordering::SeqCst))
            .unwrap_or(true);
        if dead {
            *guard = None; // 先把死进程扔掉（读线程已经把它收走了）
            match self.spawn().await {
                Ok((proc, _, _)) => *guard = Some(proc),
                Err(e) => return Err(ToolError(format!("[pytool] {e}"))),
            }
        }
        let proc = guard.as_mut().expect("刚确保过");
        let pid = proc.pid;
        let generation = proc.generation;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, (generation, tx));
        let line = json!({"id": id, "tool": tool, "arguments": args}).to_string() + "\n";
        let written = async {
            proc.stdin.write_all(line.as_bytes()).await?;
            proc.stdin.flush().await
        }
        .await;
        if let Err(e) = written {
            // 写不进去 = 管道断了（进程刚死）→ 标记待重启，本次调用算失败
            proc.alive.store(false, Ordering::SeqCst);
            self.pending.lock().unwrap().remove(&id);
            return Err(ToolError(format!("[pytool] 写请求失败: {e}")));
        }
        drop(guard); // 应答可以乱序回来，锁不用捂到应答

        let cancel_wait = async {
            match ctx.cancel.clone() {
                Some(cancel) => cancel.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(cancel_wait);
        tokio::select! {
            reply = rx => match reply {
                Ok(reply) => reply.into_result(tool, &ctx, max_lines, max_bytes),
                Err(_) => Err(ToolError(
                    "[pytool] 宿主没有应答（进程已退出，下次调用会自动重启）".to_string()
                )),
            },
            _ = &mut cancel_wait => {
                // 杀掉整个进程组：在飞的（含本次）一起失败，下次调用懒重启。
                // 不做「中断单次调用但保住状态」——那要把中断协议搬进 Python，不值。
                self.pending.lock().unwrap().remove(&id);
                kill_group(pid);
                if let Some(p) = self.proc.lock().await.as_ref() {
                    p.alive.store(false, Ordering::SeqCst);
                }
                Ok(ToolOutput::text(CANCEL_TEXT))
            }
        }
    }
}

impl Reply {
    /// 应答 → 工具结果：`error` 文本化（`session` 会包成 `[工具错误] …`），正文过截断/落盘。
    fn into_result(
        self,
        tool: &str,
        ctx: &ToolCtx,
        max_lines: Option<i64>,
        max_bytes: Option<i64>,
    ) -> ToolResult {
        if let Some(error) = self.error {
            return Err(ToolError(error));
        }
        let text = self.text.unwrap_or_default();
        let Some(head) = head_prefix(&text, max_lines, max_bytes) else {
            return Ok(ToolOutput::text(text));
        };
        // 与 `bash` 同款：全文落盘 + 独立指针（`spill` 结构化带上，消息层据此设压缩）
        let (spill, spill_txt) = match ctx.storage.store(StoreType::Raw {
            prefix: tool,
            body: &text,
        }) {
            Ok(path) => (Some(path.clone()), path.display().to_string()),
            Err(e) => (None, format!("(落盘失败: {e})")),
        };
        Ok(ToolOutput {
            text: format_output(&[format!("[工具输出全文已保存: {spill_txt}]")], &head),
            spill,
            images: Vec::new(),
        })
    }
}

/// 读线程：按 `id` 把应答交还给对应的调用；EOF（进程死了 / 被杀了）→ 丢掉**本代**在飞的调用。
async fn reader_loop(
    reader: BufReader<ChildStdout>,
    child: Child,
    pending: Pending,
    alive: Arc<AtomicBool>,
    generation: u64,
) {
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match serde_json::from_str::<Reply>(&line) {
            Ok(reply) => match reply.id {
                Some(id) => {
                    if let Some((_, tx)) = pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(reply);
                    }
                }
                None => {
                    // 宿主连请求的 id 都没认出来（非法 JSON 之类）——只可能是告警
                    if let Some(error) = reply.error {
                        crate::log::warn(format!("[pytool] {error}"));
                    }
                }
            },
            Err(e) => crate::log::warn(format!("[pytool] 应答不是合法 JSON: {e}（{line}）")),
        }
    }
    // 进程没了：把**自己这一代**还挂着的应答通道丢掉（丢弃 = 调用方拿到「没有应答」）。
    // 只动自己这一代：重启出来的新进程可能已经把新调用挂进同一张表了。
    alive.store(false, Ordering::SeqCst);
    pending
        .lock()
        .unwrap()
        .retain(|_, (gen_of, _)| *gen_of != generation);
    drop(child); // 回收（kill_on_drop 也在这里兜底杀掉）
}

/// 给子进程**组**发 SIGKILL（非 Unix 上是空操作）——与 `bash` / `repl` 同款：
/// 只杀宿主会让它启动的孙进程变成孤儿并继续持有管道。
fn kill_group(pid: u32) {
    #[cfg(unix)]
    if pid != 0 {
        unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
    }
    let _ = pid;
}

/// 测试用真解释器 + 临时工具文件跑（没有 `python3` 就跳过，与 `repl` 的测试同款）。
#[cfg(test)]
mod tests {
    use super::*;

    fn python3() -> Option<&'static str> {
        let ok = std::process::Command::new("python3")
            .arg("-c")
            .arg("import json, concurrent.futures")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        ok.then_some("python3")
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

        fn entries(&self) -> Vec<String> {
            vec![self.dir.to_string_lossy().to_string()]
        }

        fn ctx(&self, cancel: crate::cancel::Cancel) -> ToolCtx {
            ToolCtx::with_cancel(
                cancel,
                crate::config::Storage::at(self.dir.join("storage")),
                Arc::new(crate::tools::SessionState::default()),
            )
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

    #[tokio::test]
    async fn registers_python_tools_and_parses_schema() {
        let Some(python) = python3() else { return };
        let fixture = Fixture::new("register", TOOLS);
        let registry = load(
            ToolRegistry::empty(HashMap::new()),
            Some(python),
            &fixture.entries(),
        )
        .await;

        let names = registry.names();
        for want in ["echo", "slow", "boom", "not_a_string", "die"] {
            assert!(names.contains(&want), "缺工具 {want}：{names:?}");
        }
        let spec = registry
            .specs()
            .into_iter()
            .find(|s| s["function"]["name"] == "echo")
            .expect("echo 的 schema");
        assert_eq!(spec["function"]["description"], "回显");
        assert_eq!(spec["function"]["parameters"]["required"], json!(["text"]));
        assert_eq!(
            spec["function"]["parameters"]["properties"]["text"]["type"],
            "string"
        );
    }

    #[tokio::test]
    async fn calls_tool_and_textualizes_errors() {
        let Some(python) = python3() else { return };
        let fixture = Fixture::new("call", TOOLS);
        let registry = load(
            ToolRegistry::empty(HashMap::new()),
            Some(python),
            &fixture.entries(),
        )
        .await;
        let ctx = fixture.ctx(crate::cancel::Cancel::new());

        let out = registry
            .dispatch("echo", &json!({"text": "你好"}), ctx.clone())
            .await
            .expect("echo 成功");
        assert_eq!(out.text, "echo:你好");

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
        let Some(python) = python3() else { return };
        let fixture = Fixture::new("parallel", TOOLS);
        let registry = load(
            ToolRegistry::empty(HashMap::new()),
            Some(python),
            &fixture.entries(),
        )
        .await;
        let ctx = fixture.ctx(crate::cancel::Cancel::new());

        let started = std::time::Instant::now();
        let args_a = json!({"seconds": 0.3, "marker": "a"});
        let args_b = json!({"seconds": 0.3, "marker": "b"});
        let (a, b) = tokio::join!(
            registry.dispatch("slow", &args_a, ctx.clone()),
            registry.dispatch("slow", &args_b, ctx.clone()),
        );
        let elapsed = started.elapsed();
        assert_eq!(a.unwrap().text, "slept:a");
        assert_eq!(b.unwrap().text, "slept:b");
        assert!(
            elapsed < Duration::from_millis(550),
            "两个 0.3s 的调用应当重叠跑（实测 {elapsed:?}）"
        );
    }

    /// 取消：`Esc` → 返回 CANCEL_TEXT 并杀掉宿主；**下次调用还能用**（懒重启）。
    #[tokio::test]
    async fn cancel_kills_and_next_call_restarts() {
        let Some(python) = python3() else { return };
        let fixture = Fixture::new("cancel", TOOLS);
        let registry = load(
            ToolRegistry::empty(HashMap::new()),
            Some(python),
            &fixture.entries(),
        )
        .await;

        let cancel = crate::cancel::Cancel::new();
        let cancelled = fixture.ctx(cancel.clone());
        cancel.cancel();
        let out = registry
            .dispatch("slow", &json!({"seconds": 5.0}), cancelled)
            .await
            .expect("取消不是错误");
        assert_eq!(out.text, CANCEL_TEXT);

        // 新的一轮：进程该被重启，工具照常可用
        let ctx = fixture.ctx(crate::cancel::Cancel::new());
        assert_eq!(
            registry
                .dispatch("echo", &json!({"text": "again"}), ctx)
                .await
                .unwrap()
                .text,
            "echo:again"
        );
    }

    /// 宿主进程猝死（工具里 `os._exit`）→ 本次调用报错、**下一次调用自动重启**。
    #[tokio::test]
    async fn dead_host_is_restarted_on_demand() {
        let Some(python) = python3() else { return };
        let fixture = Fixture::new("dead", TOOLS);
        let registry = load(
            ToolRegistry::empty(HashMap::new()),
            Some(python),
            &fixture.entries(),
        )
        .await;
        let ctx = fixture.ctx(crate::cancel::Cancel::new());

        let err = registry.dispatch("die", &json!({}), ctx.clone()).await;
        assert!(err.is_err(), "宿主死了，本次调用必须失败");

        assert_eq!(
            registry
                .dispatch("echo", &json!({"text": "back"}), ctx)
                .await
                .unwrap()
                .text,
            "echo:back"
        );
    }

    /// `[tools.<名字>] _max_lines` 是**宿主**的旋钮：不传给 handler（没声明它的工具不能因此报错），
    /// 超限 → 全文落盘 + 指针 + `spill`。
    #[tokio::test]
    async fn host_knob_truncates_and_spills() {
        let Some(python) = python3() else { return };
        let fixture = Fixture::new(
            "spill",
            "from pie import tool\n\n@tool()\ndef big() -> str:\n    return '\\n'.join('line %d' % i for i in range(100))\n",
        );
        let mut table = toml::Table::new();
        table.insert("_max_lines".to_string(), toml::Value::Integer(2));
        let mut defaults = HashMap::new();
        defaults.insert("big".to_string(), table);

        let registry = load(
            ToolRegistry::empty(defaults),
            Some(python),
            &fixture.entries(),
        )
        .await;
        let ctx = fixture.ctx(crate::cancel::Cancel::new());
        let out = registry
            .dispatch("big", &json!({}), ctx)
            .await
            .expect("big 成功");
        assert!(
            out.text.contains("[工具输出全文已保存: "),
            "超限要给指针：{}",
            out.text
        );
        assert!(
            out.text.contains("line 0\nline 1"),
            "只留开头：{}",
            out.text
        );
        assert!(!out.text.contains("line 9"), "尾巴被截掉：{}", out.text);

        // 全文在盘上（`spill` 是结构化给的，不用拿指针文本去嗅探）
        let spill = out.spill.expect("落盘路径要结构化带上");
        let full = std::fs::read_to_string(&spill).unwrap();
        assert!(full.contains("line 99"), "落盘的是全文");
    }

    /// 用户工具里的 `print` 不能污染协议（它走 stderr，工具结果照样正确）。
    #[tokio::test]
    async fn tool_prints_do_not_corrupt_protocol() {
        let Some(python) = python3() else { return };
        let fixture = Fixture::new("loud", TOOLS);
        let registry = load(
            ToolRegistry::empty(HashMap::new()),
            Some(python),
            &fixture.entries(),
        )
        .await;
        let ctx = fixture.ctx(crate::cancel::Cancel::new());
        assert_eq!(
            registry
                .dispatch("loud", &json!({}), ctx)
                .await
                .unwrap()
                .text,
            "ok"
        );
    }

    /// 起不来 / 文件不在 / 导入抛异常 / 重名 → 只告警，注册表原样（绝不 panic）。
    #[tokio::test]
    async fn failures_only_warn() {
        let fixture = Fixture::new("broken", "raise RuntimeError('导入期就炸')\n");
        // 解释器都不存在
        let before = ToolRegistry::new(HashMap::new());
        let after = load(
            before.clone(),
            Some("definitely-not-an-interpreter-xyz"),
            &fixture.entries(),
        )
        .await;
        assert_eq!(after.names(), before.names());

        // 导入期抛异常
        let before = ToolRegistry::new(HashMap::new());
        let after = load(before.clone(), python3(), &fixture.entries()).await;
        assert_eq!(after.names(), before.names());

        // 路径不存在
        let before = ToolRegistry::new(HashMap::new());
        let after = load(
            before.clone(),
            python3(),
            &["/definitely/not/here".to_string()],
        )
        .await;
        assert_eq!(after.names(), before.names());
    }

    /// 与内置工具重名 / 名字非法 → 跳过那条，不动已有注册表。
    #[tokio::test]
    async fn name_clash_is_skipped() {
        let Some(python) = python3() else { return };
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
        let before = ToolRegistry::new(HashMap::new());
        let after = load(before.clone(), Some(python), &fixture.entries()).await;
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
