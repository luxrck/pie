//! `repl` 工具：一个**会话内持久**的 IPython —— 调用它就像往一个交互式 IPython 里敲代码，
//! 变量 / import / 定义跨调用保留。
//!
//! 与其它内置工具的唯一区别是有**会话级状态**（那个 Python 子进程）：状态挂在 `Session` 上的
//! `tools::SessionState` 槽里，本模块只管「取槽 → 懒启动 → 通信」。
//!
//! 进程 / 并发模型：
//!   - 子进程归一个 **owner task** 独占（`stdin` / `stdout` 都在它手里），本工具只经 `mpsc`
//!     请求 + `oneshot` 应答跟它说话——这样读帧**永远不会被 `select!` 取消**（`read_exact`
//!     不是 cancel-safe，被丢掉会留半帧、把后续帧全部错位）。
//!   - 同一会话内的 `repl` 调用用 `tokio::sync::Mutex` 串行化（状态共享，不能并发跑）。
//!   - 取消 / 超时：给**子进程组**发信号（`SIGINT` 保状态；停不住再 `SIGKILL`）。
//!
//! 协议见 `repl_driver.py`（「4 字节大端长度 + UTF-8」），本模块是它的对端。

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};

use crate::cancel::CANCEL_TEXT;
use crate::tools::{self, Tool, ToolCtx, ToolError, ToolOutput, ToolResult};

/// 驱动脚本正文（编译期嵌入；它自己用 IPython 的 `InteractiveShell`）。
const DRIVER: &str = include_str!("repl_driver.py");

/// 默认解释器：Unix `python3`，其它 `python`。
#[cfg(unix)]
const DEFAULT_PYTHON: &str = "python3";
#[cfg(not(unix))]
const DEFAULT_PYTHON: &str = "python";

/// 中断后等驱动回帧的宽限（超时路径用：及时停了就保住状态，停不住才杀）。
const INTERRUPT_GRACE: Duration = Duration::from_secs(3);

/// 信号：Unix 用 libc 的常量；其它平台只是占位（`signal` 在非 Unix 上是空操作）。
#[cfg(unix)]
const SIGINT: i32 = libc::SIGINT;
#[cfg(not(unix))]
const SIGINT: i32 = 2;
#[cfg(unix)]
const SIGKILL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const SIGKILL: i32 = 9;

// 会话内持久的 IPython：状态活在本会话的进程里；`pie -r` 恢复会话**不会**带回解释器内存
// （重新起一个干净的）——活进程没法跨进程重启携带。
/// 在当前会话里持久运行的 IPython 中执行一段 Python 代码（变量、import、定义跨调用保留）。
#[derive(Deserialize, JsonSchema)]
pub struct Repl {
    /// 要执行的 Python 代码（可多行；和往 IPython 里敲的一样，支持 `%` 魔法与 `!shell`）
    pub code: String,
    /// 超时秒数（可选，无默认）
    pub timeout: Option<i64>,
    /// 私有参数：解释器（默认 python3；IPython 装在别的 venv 就指过去；开头的 `~` 会展开成 $HOME）
    #[schemars(skip)]
    pub _python: Option<String>,
    /// 私有参数：输出行/字节上限（超限只留开头 + 落盘全文）
    #[schemars(skip)]
    pub _max_lines: Option<i64>,
    #[schemars(skip)]
    pub _max_bytes: Option<i64>,
    /// 私有参数：覆盖驱动脚本正文（测试用假驱动；正常别配）
    #[schemars(skip)]
    pub _driver: Option<String>,
}

/// 本会话的 REPL 状态（挂在 `SessionState` 槽里，一会话一份）。
#[derive(Default)]
struct ReplSession {
    proc: tokio::sync::Mutex<Option<ReplProc>>,
}

/// 一个活着的解释器：pid（发信号用）+ 请求通道（owner task 收）+ 存活标志。
struct ReplProc {
    pid: u32,
    tx: mpsc::Sender<Request>,
    alive: Arc<AtomicBool>,
}

/// 一次执行请求（owner task 按序处理）。
struct Request {
    code: String,
    reply: oneshot::Sender<Result<Reply, String>>,
}

/// 驱动回的一帧。
#[derive(Deserialize, Default)]
struct Reply {
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    error: Option<String>,
}

/// 一次执行的结果分类。
enum Outcome {
    Reply(Reply),
    Cancelled,
    TimedOut,
}

impl Tool for Repl {
    async fn call(self, ctx: ToolCtx) -> ToolResult {
        let Self {
            code,
            timeout,
            _python,
            _max_lines,
            _max_bytes,
            _driver,
        } = self;

        // 空代码不启动解释器（别为个空串把 IPython 拉起来）。
        if code.trim().is_empty() {
            return Ok(ToolOutput::text(""));
        }
        let mut python = _python.unwrap_or_else(|| DEFAULT_PYTHON.to_string());
        // `_python` 直接交给 `Command::new`，不会自己展开 `~`——这里只认 `~` / `~/…` → $HOME。
        if python == "~" || python.starts_with("~/") {
            if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
            {
                python = format!("{}{}", home.to_string_lossy(), &python[1..]);
            }
        }
        let driver = _driver.unwrap_or_else(|| DRIVER.to_string());

        // 取本会话的 REPL 槽；锁住整段交互（同一会话的 repl 调用串行）。
        let session = ctx.state.get_or_init(ReplSession::default);
        let mut guard = session.proc.lock().await;

        // 启动 / 断线重启；最多试两次（第一次失败多半是子进程刚自己退了）。
        let mut last: Option<String> = None;
        for attempt in 0..2 {
            let dead = guard
                .as_ref()
                .map(|p| !p.alive.load(Ordering::SeqCst))
                .unwrap_or(true);
            if dead {
                *guard = None;
                *guard = Some(ReplProc::spawn(&python, &driver).await?);
            }
            let proc = guard.as_mut().expect("刚确保过");
            match proc.run(code.clone(), &ctx, timeout).await {
                Ok(Outcome::Reply(reply)) => {
                    return Ok(format_reply(reply, &ctx, _max_lines, _max_bytes));
                }
                Ok(Outcome::Cancelled) => return Ok(ToolOutput::text(CANCEL_TEXT)),
                Ok(Outcome::TimedOut) => {
                    // 超时且宽限期内没停住 → 已 kill；下次调用会重启（状态丢失）。
                    return Ok(ToolOutput::text(format!(
                        "[repl] 执行超过 {}s 仍未停下，解释器已重启（之前的变量丢失）",
                        timeout.unwrap_or(0)
                    )));
                }
                Err(e) => {
                    last = Some(e);
                    *guard = None; // 下一轮重起
                    if attempt == 1 {
                        break;
                    }
                }
            }
        }
        Err(ToolError(format!(
            "[repl] {}",
            last.unwrap_or_else(|| "解释器不可用".to_string())
        )))
    }
}

impl ReplProc {
    /// 起一个解释器进程，把 `stdin` / `stdout` / `stderr` 交给 owner task。
    async fn spawn(python: &str, driver: &str) -> Result<ReplProc, ToolError> {
        let mut cmd = tokio::process::Command::new(python);
        cmd.arg("-u").arg("-c").arg(driver);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.kill_on_drop(true); // owner task 被丢弃（会话没了 / 回合中断）→ 别留孤儿
        #[cfg(unix)]
        cmd.process_group(0); // 独立进程组：中断/超时按组发信号
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError(format!("[repl] 启动解释器失败（{python}）: {e}")))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let pid = child.id().unwrap_or(0);
        let (tx, rx) = mpsc::channel::<Request>(1);
        let alive = Arc::new(AtomicBool::new(true));
        tokio::spawn(owner_loop(stdin, stdout, stderr, rx, alive.clone(), child));
        Ok(ReplProc { pid, tx, alive })
    }

    /// 发一次请求 + 等应答（race 取消 / 超时）。
    async fn run(
        &mut self,
        code: String,
        ctx: &ToolCtx,
        timeout: Option<i64>,
    ) -> Result<Outcome, String> {
        let (tx, mut rx) = oneshot::channel();
        if self.tx.send(Request { code, reply: tx }).await.is_err() {
            return Err("解释器进程已退出".to_string());
        }

        let cancel_wait = async {
            match ctx.cancel.clone() {
                Some(cancel) => cancel.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(cancel_wait);
        let timeout_wait = async {
            match timeout {
                Some(t) if t > 0 => tokio::time::sleep(Duration::from_secs(t as u64)).await,
                _ => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(timeout_wait);

        tokio::select! {
            res = &mut rx => match res {
                Ok(Ok(reply)) => Ok(Outcome::Reply(reply)),
                Ok(Err(e)) => Err(e),
                Err(_) => Err("解释器没有应答（进程可能已退出）".to_string()),
            },
            _ = &mut cancel_wait => {
                self.interrupt(); // SIGINT：解释器保住状态，下次还能接着用
                Ok(Outcome::Cancelled)
            }
            _ = &mut timeout_wait => {
                self.interrupt();
                match tokio::time::timeout(INTERRUPT_GRACE, &mut rx).await {
                    Ok(Ok(Ok(reply))) => Ok(Outcome::Reply(reply)), // 及时停了：状态还在
                    _ => {
                        self.kill(); // 停不住 → 杀（丢状态，下次重启）
                        Ok(Outcome::TimedOut)
                    }
                }
            }
        }
    }

    /// 给子进程**组**发信号（非 Unix 上是空操作）。
    fn signal(&self, sig: i32) {
        #[cfg(unix)]
        if self.pid != 0 {
            unsafe { libc::killpg(self.pid as libc::pid_t, sig) };
        }
        let _ = sig;
    }

    fn interrupt(&self) {
        self.signal(SIGINT);
    }

    fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        self.signal(SIGKILL);
    }
}

/// owner task：独占子进程的管道，按序「读请求 → 写帧 → 读帧 → 回答」。
///
/// 它**永不进 `select!`**，所以读写永远完整；协商取消靠外面给进程组发信号。
async fn owner_loop(
    mut stdin: ChildStdin,
    mut stdout: ChildStdout,
    mut stderr: ChildStderr,
    mut rx: mpsc::Receiver<Request>,
    alive: Arc<AtomicBool>,
    mut child: Child,
) {
    while let Some(req) = rx.recv().await {
        // 正常路径：写帧 → 读帧 → 回答；任何一步失败都带上子进程 stderr 再回。
        if let Err(e) = write_frame(&mut stdin, &req.code).await {
            let msg = with_stderr(format!("向解释器写入失败: {e}"), &mut stderr).await;
            let _ = req.reply.send(Err(msg));
            break;
        }
        match read_frame(&mut stdout).await {
            Ok(body) => {
                let reply = serde_json::from_slice::<Reply>(&body)
                    .map_err(|e| format!("解释器返回了非法 JSON: {e}"));
                let _ = req.reply.send(reply);
                continue; // 成功：处理下一个请求（别走到下面的失败分支）
            }
            Err(e) => {
                let msg = with_stderr(format!("读取解释器输出失败: {e}"), &mut stderr).await;
                let _ = req.reply.send(Err(msg));
                break;
            }
        }
    }
    alive.store(false, Ordering::SeqCst);
    let _ = child.start_kill();
    let _ = child.wait().await;
}

async fn write_frame(stdin: &mut ChildStdin, code: &str) -> std::io::Result<()> {
    let bytes = code.as_bytes();
    stdin.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    stdin.write_all(bytes).await?;
    stdin.flush().await
}

async fn read_frame(stdout: &mut ChildStdout) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stdout.read_exact(&mut len).await?;
    let n = u32::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    stdout.read_exact(&mut buf).await?;
    Ok(buf)
}

/// 子进程退出后把 stderr 读干净（给个 1s 上限，免得万一还活着卡住）。
async fn drain_stderr(stderr: &mut ChildStderr) -> String {
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(1), stderr.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).into_owned()
}

/// 失败信息补上子进程的 stderr（比如 `ModuleNotFoundError: IPython`），没有就不加。
async fn with_stderr(msg: String, stderr: &mut ChildStderr) -> String {
    let detail = drain_stderr(stderr).await;
    if detail.trim().is_empty() {
        msg
    } else {
        format!("{msg}；解释器 stderr：{}", detail.trim())
    }
}

/// 把一帧回执拼成工具输出：stdout / stderr / traceback 依次接上；超限则头部截断 + 落盘。
fn format_reply(
    reply: Reply,
    ctx: &ToolCtx,
    max_lines: Option<i64>,
    max_bytes: Option<i64>,
) -> ToolOutput {
    let mut body = String::new();
    for part in [
        reply.stdout.as_str(),
        reply.stderr.as_str(),
        reply.error.as_deref().unwrap_or(""),
    ] {
        if part.is_empty() {
            continue;
        }
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(part);
    }

    match tools::head_prefix(&body, max_lines, max_bytes) {
        None => ToolOutput::text(body),
        Some(head) => {
            // 单元格输出一旦被消费就没了（不可再生）→ 超限落盘 + 指针，与 `bash` 同款。
            let (spill, spill_txt) = match ctx.storage.store(crate::config::StoreType::Raw {
                prefix: "repl",
                body: &body,
            }) {
                Ok(path) => (Some(path.clone()), path.display().to_string()),
                Err(e) => (None, format!("(落盘失败: {e})")),
            };
            ToolOutput {
                text: tools::format_output(&[format!("[工具输出全文已保存: {spill_txt}]")], &head),
                spill,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::Cancel;
    use crate::config::Storage;
    use crate::tools::ToolRegistry;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    /// 假驱动：不依赖 IPython，只按协议回帧。`call#N` 里的 N 证明**进程被复用**。
    const FAKE: &str = r#"
import json, os, struct, sys, time
inp = os.fdopen(os.dup(sys.stdin.fileno()), "rb", buffering=0)
out = os.fdopen(os.dup(sys.stdout.fileno()), "wb", buffering=0)
count = 0
while True:
    head = inp.read(4)
    if len(head) < 4:
        break
    (size,) = struct.unpack(">I", head)
    code = inp.read(size).decode("utf-8")
    count += 1
    if code == "stall":
        time.sleep(10)
    if code == "stderr":
        payload = {"stdout": "", "stderr": "oops\n", "error": None}
    elif code == "error":
        payload = {"stdout": "", "stderr": "", "error": "ValueError: boom"}
    else:
        payload = {"stdout": "call#%d: %s\n" % (count, code), "stderr": "", "error": None}
    body = json.dumps(payload).encode("utf-8")
    out.write(struct.pack(">I", len(body)) + body); out.flush()
"#;

    /// 一启动就报错退出的假驱动（模拟「解释器里没装 IPython」）。
    const CRASH: &str = r#"
import sys
sys.stderr.write("ModuleNotFoundError: No module named 'IPython'\n")
sys.exit(3)
"#;

    /// 有 python3 才跑（本工具本来就要解释器；没有就跳过）。
    fn python3() -> Option<&'static str> {
        let ok = std::process::Command::new("python3")
            .args(["-c", "pass"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        ok.then_some("python3")
    }

    fn tempdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("pie-repl-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn registry() -> ToolRegistry {
        ToolRegistry::empty(HashMap::new()).with_tool::<Repl>("repl")
    }

    fn ctx_at(tmp: &Path) -> ToolCtx {
        ToolCtx {
            cancel: None,
            storage: Storage::at(tmp.to_path_buf()),
            state: Arc::new(crate::tools::SessionState::default()),
        }
    }

    async fn run_code(reg: &ToolRegistry, ctx: &ToolCtx, py: &str, code: &str) -> ToolOutput {
        let args = json!({"code": code, "_python": py, "_driver": FAKE});
        reg.dispatch("repl", &args, ctx.clone()).await.unwrap()
    }

    #[tokio::test]
    async fn state_persists_within_a_session_only() {
        let Some(py) = python3() else { return };
        let tmp = tempdir("state");
        let reg = registry();
        let ctx = ctx_at(&tmp);

        let a = run_code(&reg, &ctx, py, "hello").await;
        let b = run_code(&reg, &ctx, py, "world").await;
        assert!(a.text.starts_with("call#1:"), "{a:?}");
        assert!(
            b.text.starts_with("call#2:"),
            "同一会话应复用同一进程: {b:?}"
        );

        // 另一个会话（另一个 state）→ 全新进程
        let other = ctx_at(&tmp);
        let c = run_code(&reg, &other, py, "hello").await;
        assert!(c.text.starts_with("call#1:"), "新会话应是新进程: {c:?}");
    }

    #[tokio::test]
    async fn stderr_and_traceback_are_joined_into_the_body() {
        let Some(py) = python3() else { return };
        let reg = registry();
        let ctx = ctx_at(&tempdir("body"));

        let e = run_code(&reg, &ctx, py, "stderr").await;
        assert_eq!(e.text, "oops\n");
        let t = run_code(&reg, &ctx, py, "error").await;
        assert_eq!(t.text, "ValueError: boom");
        // 异常是 REPL 的正常输出，不是工具失败（dispatch 返回 Ok）
    }

    #[tokio::test]
    async fn overflow_truncates_head_and_spills_full_text() {
        let Some(py) = python3() else { return };
        let tmp = tempdir("spill");
        let reg = registry();
        let ctx = ctx_at(&tmp);

        let args = json!({
            "code": "l1\nl2\nl3\nl4", "_python": py, "_driver": FAKE, "_max_lines": 2
        });
        let out = reg.dispatch("repl", &args, ctx.clone()).await.unwrap();
        assert!(out.text.starts_with("[工具输出全文已保存:"), "{out:?}");
        assert!(
            out.text.contains("call#1: l1\nl2\n"),
            "只留开头两行: {out:?}"
        );
        let spill = out.spill.expect("应落盘");
        let full = std::fs::read_to_string(&spill).unwrap();
        assert!(full.contains("l4"), "落盘件应是全文: {full:?}");
        assert_eq!(spill.parent().unwrap(), tmp.join("context"));
    }

    #[tokio::test]
    async fn missing_interpreter_is_a_tool_error() {
        let reg = registry();
        let ctx = ctx_at(&tempdir("nointerp"));
        let args = json!({"code": "1", "_python": "definitely-not-an-interpreter-xyz"});
        let e = reg.dispatch("repl", &args, ctx).await.unwrap_err();
        assert!(e.0.contains("启动解释器失败"), "{:?}", e.0);
    }

    #[tokio::test]
    async fn child_crash_surfaces_its_stderr() {
        let Some(py) = python3() else { return };
        let reg = registry();
        let ctx = ctx_at(&tempdir("crash"));
        let args = json!({"code": "1", "_python": py, "_driver": CRASH});
        let e = reg.dispatch("repl", &args, ctx).await.unwrap_err();
        assert!(e.0.contains("IPython"), "应带上子进程 stderr: {:?}", e.0);
    }

    #[tokio::test]
    async fn cancel_returns_the_cancel_sentinel() {
        let Some(py) = python3() else { return };
        let reg = registry();
        let cancel = Cancel::new();
        let mut ctx = ctx_at(&tempdir("cancel"));
        ctx.cancel = Some(cancel.clone());

        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c.cancel();
        });
        let args = json!({"code": "stall", "_python": py, "_driver": FAKE});
        let out = reg.dispatch("repl", &args, ctx).await.unwrap();
        assert_eq!(out.text, CANCEL_TEXT);
    }

    #[tokio::test]
    async fn timeout_is_reported() {
        let Some(py) = python3() else { return };
        let reg = registry();
        let ctx = ctx_at(&tempdir("timeout"));
        let args = json!({"code": "stall", "_python": py, "_driver": FAKE, "timeout": 1});
        let out = reg.dispatch("repl", &args, ctx).await.unwrap();
        assert!(out.text.contains("超过 1s"), "{out:?}");
    }

    /// 对模型暴露的 schema：只有 `code`（必填）+ `timeout`，私有参数不进。
    #[test]
    fn schema_exposes_code_and_hides_private_params() {
        let spec = registry().specs().remove(0);
        assert_eq!(spec["function"]["name"], "repl");
        assert!(spec["function"]["description"]
            .as_str()
            .unwrap()
            .contains("IPython"));
        let params = &spec["function"]["parameters"];
        assert_eq!(params["required"], json!(["code"]));
        assert!(params["properties"].get("timeout").is_some());
        for hidden in ["_python", "_max_lines", "_max_bytes", "_driver"] {
            assert!(
                params["properties"].get(hidden).is_none(),
                "私有参数不该进 schema: {hidden}"
            );
        }
    }

    /// `_python` 开头的 `~` 会展开成 $HOME（从错误信息里能看出用的是展开后的绝对路径）。
    #[tokio::test]
    async fn tilde_in_python_path_is_expanded() {
        let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
        else {
            return;
        };
        let home = home.to_string_lossy();
        let reg = registry();
        let ctx = ctx_at(&tempdir("tilde"));
        let args = json!({"code": "1", "_python": "~/no-such-python-xyz"});
        let e = reg.dispatch("repl", &args, ctx).await.unwrap_err();
        assert!(
            e.0.contains(&format!("{home}/no-such-python-xyz")),
            "~ 未展开: {:?}",
            e.0
        );
    }
}
