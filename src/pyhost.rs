//! 长活 Python 宿主的**传输 + 进程生命周期**（`repl` 与 `pytool` 共用；将来的扩展宿主也走它）。
//!
//! 形状：一个子进程 + **一行一条 JSON**（一问一答、靠 `id` 配对、**应答可乱序**）：
//!
//! ```text
//! stdout 第一行  ← 清单（驱动自己给：`repl` 给 `{}`，`pytool` 给 {"tools":[…]}
//! stdin  每行    → {"id":N, …}
//! stdout 之后    ← {"id":N, …}
//! ```
//!
//! 几条踩过的规矩：
//!   - **首行永远是清单**：启动期就把「驱动起不来 / 卡在 import」暴露出来，而不是拖到第一次调用；
//!   - **读线程独占 stdout**：`read_line` 不是 cancel-safe，进了 `select!` 会留半行、把后续帧全部错位
//!     —— 所以调用方只等 `oneshot`，取消/超时**永远不需要碰读循环**；
//!   - **应答路由带「进程代」**：进程死了懒重启，重启后老调用的 `id` 可能和新调用撞上 → 只清自己那一代；
//!   - **起进程 = 独立进程组**（`process_group(0)`）+ `kill_on_drop`：取消按组杀（[`crate::cancel::kill_group`]），
//!     谁都不管它时也不留孤儿；
//!   - 子进程 stderr 留**尾巴**（出错时附在消息里，比如 `ModuleNotFoundError: IPython`），
//!     需要的话再逐行转成告警（`Spec::stderr_prefix`）。
//!
//! `repl` 早先自己实现过一套：一个 owner task 串行「读请求 → 写帧 → 读帧 → 应答」，理由只是绕
//! 「`read_exact` 不是 cancel-safe」。换成读线程 + `oneshot` 之后同样安全，还白拿「多请求在飞 +
//! 乱序应答」，而且取消/超时不必再靠「杀进程」把读循环逼出来。

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tokio::task::JoinHandle;

use crate::cancel::{self, Cancel};

/// 启动期等清单的上限：驱动卡在 import 时别把 `pie` 也拖住。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// 默认解释器：Unix `python3`，其它 `python`（`repl` / `pytool` 共用同一份）。
#[cfg(unix)]
pub const DEFAULT_PYTHON: &str = "python3";
#[cfg(not(unix))]
pub const DEFAULT_PYTHON: &str = "python";

/// 子进程 stderr 最多留几行（出错时附在消息里）。
const STDERR_TAIL: usize = 20;

/// 怎么起这个宿主。字段直接摆着，`new` 只给最常见的三件。
#[derive(Clone)]
pub struct Spec {
    /// 错误消息 / 告警里的名字：`[repl] …`。
    pub label: String,
    /// 解释器（调用方解析好：`[python] interpreter` → `envs/base` → `python3`，`~` 已展开）。
    pub python: String,
    /// 驱动正文（`-c` 传进去；官方的用 `include_str!` 嵌，测试传假驱动）。
    pub driver: String,
    /// 交给驱动的文件（`pytool` 的工具文件；`repl` 空）。
    pub files: Vec<PathBuf>,
    /// 额外环境变量（`PIE_TRANSCRIPT` 之类）。
    pub env: Vec<(String, String)>,
    /// `Some(prefix)` = 子进程 stderr 逐行转成 `[prefix] …` 告警；`None` = 只留尾巴。
    pub stderr_prefix: Option<String>,
    pub handshake_timeout: Duration,
}

impl Spec {
    pub fn new(label: &str, python: &str, driver: &str) -> Self {
        Self {
            label: label.to_string(),
            python: python.to_string(),
            driver: driver.to_string(),
            files: Vec::new(),
            env: Vec::new(),
            stderr_prefix: None,
            handshake_timeout: HANDSHAKE_TIMEOUT,
        }
    }
}

/// 取消 / 超时怎么打断这次请求。
#[derive(Clone, Copy, Debug)]
pub enum Interrupt {
    /// 给进程组发 `sig`（**尽量保住状态**）：超时路径再等 `grace` 里驱动自己回帧，及时回就算成功。
    Signal { sig: i32, grace: Duration },
    /// 直接按进程组 SIGKILL（丢状态，下次调用懒重启）。
    Kill,
}

/// 一次请求的结局。
#[derive(Debug)]
pub enum Outcome {
    Reply(Value),
    Cancelled,
    TimedOut,
}

/// `id` →（**进程代**，应答通道）。见模块头「应答路由带进程代」。
type Pending = Arc<Mutex<HashMap<u64, (u64, oneshot::Sender<Value>)>>>;

/// 一个长活 Python 宿主：有进程就写、没有就起、死了下次调用懒重启。
pub struct PyHost {
    spec: Spec,
    /// 写通道（`None` = 还没起 / 已死）。
    stdin: AsyncMutex<Option<ChildStdin>>,
    /// 当前进程的 pid（0 = 没有）——信号路径**只读它，不碰锁**（取消要能立刻发出去）。
    pid: AtomicU32,
    /// 当前进程还活着吗（读线程 EOF / 被杀时置 false）→ 下次请求懒重启。
    alive: Arc<AtomicBool>,
    pending: Pending,
    next_id: AtomicU64,
    generation: AtomicU64,
    /// 子进程 stderr 的尾巴（出错时附上）。
    stderr_tail: Arc<Mutex<Vec<String>>>,
}

impl PyHost {
    /// 起一个宿主：spawn + 读首行清单（原样返回，由调用方解释）。
    pub async fn start(spec: &Spec) -> Result<(Arc<Self>, Value), String> {
        let host = Arc::new(Self {
            spec: spec.clone(),
            stdin: AsyncMutex::new(None),
            pid: AtomicU32::new(0),
            alive: Arc::new(AtomicBool::new(false)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            generation: AtomicU64::new(0),
            stderr_tail: Arc::new(Mutex::new(Vec::new())),
        });
        let (manifest, stdin) = host.spawn().await?;
        *host.stdin.lock().await = Some(stdin);
        Ok((host, manifest))
    }

    /// 标签（错误消息里那个名字）。
    pub fn label(&self) -> &str {
        &self.spec.label
    }

    /// 起子进程 + 读首行清单，把剩下的 stdout 交给读线程；返回 `stdin` 让调用方自己存
    /// （不能由这儿去锁 `self.stdin`：`ensure` 正持着它）。
    async fn spawn(&self) -> Result<(Value, ChildStdin), String> {
        let spec = &self.spec;
        let mut cmd = tokio::process::Command::new(&spec.python);
        // `-X utf8`：中文经 stdin/stdout 进出不靠 locale；`-u`：别让 stdout 缓冲住一行应答
        cmd.arg("-X")
            .arg("utf8")
            .arg("-u")
            .arg("-c")
            .arg(&spec.driver);
        cmd.args(&spec.files);
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.kill_on_drop(true); // 谁都不管它时别留孤儿（配合读线程）
        #[cfg(unix)]
        cmd.process_group(0); // 独立进程组：取消/超时按组发信号
        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "[{}] 启动解释器失败（{}）: {e}（可用 [python] interpreter 指到别的解释器）",
                spec.label, spec.python
            )
        })?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        self.pid.store(child.id().unwrap_or(0), Ordering::SeqCst);
        self.alive.store(true, Ordering::SeqCst);

        let stderr_task = tokio::spawn(stderr_loop(
            stderr,
            Arc::clone(&self.stderr_tail),
            spec.stderr_prefix.clone(),
        ));

        let mut reader = BufReader::new(stdout);
        let mut first = String::new();
        let got = tokio::time::timeout(spec.handshake_timeout, reader.read_line(&mut first)).await;
        let manifest = match got {
            Err(_) => {
                let tail = self.drain_stderr(stderr_task).await;
                return Err(format!(
                    "[{}] 宿主 {}s 内没有给出清单（驱动是不是卡在 import 了？）{tail}",
                    spec.label,
                    spec.handshake_timeout.as_secs()
                ));
            }
            Ok(Err(e)) => {
                let tail = self.drain_stderr(stderr_task).await;
                return Err(format!("[{}] 读清单失败: {e}{tail}", spec.label));
            }
            Ok(Ok(0)) => {
                let tail = self.drain_stderr(stderr_task).await;
                return Err(format!("[{}] 宿主没有给出清单就退出了{tail}", spec.label));
            }
            Ok(Ok(_)) => match serde_json::from_str::<Value>(first.trim()) {
                Ok(value) => value,
                Err(e) => {
                    let tail = self.drain_stderr(stderr_task).await;
                    return Err(format!(
                        "[{}] 清单不是合法 JSON: {e}（原文: {}）{tail}",
                        spec.label,
                        first.trim()
                    ));
                }
            },
        };
        // 清单读完了才交棒：读线程接着用同一个 `BufReader`（它可能已经把后续字节缓冲进去了）。
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        tokio::spawn(reader_loop(
            reader,
            child,
            Arc::clone(&self.pending),
            Arc::clone(&self.alive),
            generation,
            spec.label.clone(),
        ));
        Ok((manifest, stdin))
    }

    /// 确保有一个活着的进程（懒重启；重启时清单丢掉——调用方早就用过了）。
    async fn ensure(&self) -> Result<(), String> {
        let mut guard = self.stdin.lock().await;
        if guard.is_none() || !self.alive.load(Ordering::SeqCst) {
            *guard = None;
            let (_, stdin) = self.spawn().await?;
            *guard = Some(stdin);
        }
        Ok(())
    }

    /// 发一次请求 + 等这一 `id` 的应答（race 取消 / 超时）。
    ///
    /// `payload` 必须是 JSON 对象：本模块会往里加 `id`。
    pub async fn request(
        &self,
        payload: Value,
        cancel: Option<&Cancel>,
        timeout: Option<Duration>,
        interrupt: Interrupt,
    ) -> Result<Outcome, String> {
        if !payload.is_object() {
            return Err(format!("[{}] 请求体必须是 JSON 对象", self.spec.label));
        }
        self.ensure().await?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = payload;
        msg["id"] = json!(id); // 上面查过是对象
        let mut line = serde_json::to_string(&msg)
            .map_err(|e| format!("[{}] 请求序列化失败: {e}", self.spec.label))?;
        line.push('\n');

        let (tx, mut rx) = oneshot::channel();
        {
            // 锁只护「挂上应答通道 + 写一行」，不等应答：应答可以乱序回来。
            let mut guard = self.stdin.lock().await;
            let Some(stdin) = guard.as_mut() else {
                return Err(format!("[{}] 宿主没起来", self.spec.label));
            };
            let generation = self.generation.load(Ordering::SeqCst);
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, (generation, tx));
            let written = async {
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await
            }
            .await;
            if let Err(e) = written {
                // 写不进去 = 管道断了（进程刚死）→ 标记待重启，本次调用算失败
                self.alive.store(false, Ordering::SeqCst);
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                return Err(format!(
                    "[{}] 写请求失败: {e}{}",
                    self.spec.label,
                    self.stderr_suffix()
                ));
            }
        }

        let cancel_wait = async {
            match cancel {
                Some(cancel) => cancel.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(cancel_wait);
        let timeout_wait = async {
            match timeout {
                Some(t) if !t.is_zero() => tokio::time::sleep(t).await,
                _ => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(timeout_wait);

        let outcome = tokio::select! {
            reply = &mut rx => match reply {
                Ok(value) => Ok(Outcome::Reply(value)),
                Err(_) => Err(format!(
                    "[{}] 宿主没有应答（进程可能已退出，下次调用会重启）",
                    self.spec.label
                )),
            },
            _ = &mut cancel_wait => {
                match interrupt {
                    Interrupt::Kill => self.kill_and_mark(),
                    Interrupt::Signal { sig, .. } => self.kill_group(sig),
                }
                Ok(Outcome::Cancelled)
            }
            _ = &mut timeout_wait => {
                match interrupt {
                    Interrupt::Kill => {
                        self.kill_and_mark();
                        Ok(Outcome::TimedOut)
                    }
                    Interrupt::Signal { sig, grace } => {
                        self.kill_group(sig); // 先商量（尽量保住状态）
                        match tokio::time::timeout(grace, &mut rx).await {
                            Ok(Ok(value)) => Ok(Outcome::Reply(value)),
                            _ => {
                                self.kill_and_mark();
                                Ok(Outcome::TimedOut)
                            }
                        }
                    }
                }
            }
        };
        // 这一条到此为止：迟到的应答（或根本没有）都由读线程丢掉。
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        outcome
    }

    /// 给当前进程**组**发信号（不碰锁：pid 是原子量，取消要能立刻发出去）。
    pub fn kill_group(&self, sig: i32) {
        cancel::kill_group(self.pid.load(Ordering::SeqCst), sig);
    }

    /// 杀掉 + 标记待重启：本次调用照旧返回「取消 / 超时」，下次调用会懒重启。
    fn kill_and_mark(&self) {
        self.alive.store(false, Ordering::SeqCst);
        self.kill_group(cancel::SIGKILL);
    }

    /// 给 stderr 读线程一点时间把尾巴收干净（子进程刚退出时它还在读），再拼成一句后缀。
    async fn drain_stderr(&self, task: JoinHandle<()>) -> String {
        let _ = tokio::time::timeout(Duration::from_millis(500), task).await;
        self.stderr_suffix()
    }

    /// 出错时附上的 stderr 尾巴（没有就是空串）。
    fn stderr_suffix(&self) -> String {
        let tail = self
            .stderr_tail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if tail.is_empty() {
            String::new()
        } else {
            format!("；宿主 stderr：{}", tail.join("\n").trim())
        }
    }
}

/// 读线程：按 `id` 把应答交还对应的调用；EOF（进程死了 / 被杀了）→ 丢掉**本代**在飞的调用。
async fn reader_loop(
    reader: BufReader<ChildStdout>,
    child: Child,
    pending: Pending,
    alive: Arc<AtomicBool>,
    generation: u64,
    label: String,
) {
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match serde_json::from_str::<Value>(&line) {
            Ok(value) => match value.get("id").and_then(Value::as_u64) {
                Some(id) => {
                    if let Some((_, tx)) = pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id)
                    {
                        let _ = tx.send(value);
                    }
                }
                // 没有 id 的行 = 宿主连请求都认不出来（协议外的东西）→ 当告警
                None => {
                    if let Some(err) = value.get("error").and_then(Value::as_str) {
                        crate::log::warn(format!("[{label}] {err}"));
                    }
                }
            },
            Err(e) => crate::log::warn(format!("[{label}] 应答不是合法 JSON: {e}（{line}）")),
        }
    }
    // 进程没了：把自己这一代还挂着的应答通道丢掉（丢弃 = 调用方拿到「没有应答」）。
    alive.store(false, Ordering::SeqCst);
    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, (gen_of, _)| *gen_of != generation);
    drop(child); // 回收（`kill_on_drop` 也在这里兜底杀掉）
}

/// stderr 读线程：留尾巴 + （可选）逐行转成告警。
async fn stderr_loop(stderr: ChildStderr, tail: Arc<Mutex<Vec<String>>>, prefix: Option<String>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(prefix) = &prefix {
            crate::log::warn(format!("[{prefix}] {line}"));
        }
        let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
        tail.push(line);
        if tail.len() > STDERR_TAIL {
            tail.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 有 python3 才跑（本模块的测试都要真解释器；没有就跳过，与 `repl` 的测试同款）。
    fn python3() -> Option<&'static str> {
        std::process::Command::new("python3")
            .args(["-c", "pass"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            .then_some("python3")
    }

    /// 假驱动：按 `delay` 毫秒延迟后回帧，把 `echo` 原样带回来。
    const ECHO: &str = r#"
import json, os, sys, time
inp = os.fdopen(os.dup(sys.stdin.fileno()), "r", encoding="utf-8", buffering=1)
out = os.fdopen(os.dup(sys.stdout.fileno()), "w", encoding="utf-8", buffering=1)
out.write(json.dumps({"ready": True}) + "\n"); out.flush()
while True:
    line = inp.readline()
    if not line:
        break
    req = json.loads(line)
    time.sleep(req.get("delay", 0) / 1000.0)
    out.write(json.dumps({"id": req["id"], "echo": req.get("echo"), "pid": os.getpid()}) + "\n")
    out.flush()
"#;

    async fn start(driver: &str) -> Arc<PyHost> {
        let py = python3().expect("调用方先查过");
        let spec = Spec::new("test", py, driver);
        PyHost::start(&spec).await.expect("宿主该起来").0
    }

    /// 驱动不吐清单就退出（比如缺 IPython）→ `start` 直接报错，并且**带上它的 stderr**。
    #[tokio::test]
    async fn handshake_failure_carries_stderr() {
        let Some(_py) = python3() else { return };
        let spec = Spec::new(
            "test",
            "python3",
            "import sys\nsys.stderr.write('boom: no IPython\\n')\nsys.exit(3)\n",
        );
        let err = match PyHost::start(&spec).await {
            Ok(_) => panic!("驱动没吐清单，不该起来"),
            Err(e) => e,
        };
        assert!(err.contains("没有给出清单"), "{err}");
        assert!(err.contains("no IPython"), "stderr 要带上：{err}");
    }

    /// 乱序应答按 `id` 路由：慢的先发、快的先回，两条各自拿到自己的结果。
    #[tokio::test]
    async fn replies_route_by_id_even_when_they_come_back_out_of_order() {
        let Some(_py) = python3() else { return };
        let host = start(ECHO).await;
        let slow = host.request(
            json!({"delay": 300, "echo": "slow"}),
            None,
            None,
            Interrupt::Kill,
        );
        let fast = host.request(
            json!({"delay": 10, "echo": "fast"}),
            None,
            None,
            Interrupt::Kill,
        );
        let (slow, fast) = tokio::join!(slow, fast);
        let echo = |outcome: Result<Outcome, String>| match outcome.expect("该有应答") {
            Outcome::Reply(value) => value["echo"].as_str().unwrap().to_string(),
            other => panic!("要有应答：{other:?}"),
        };
        assert_eq!(echo(slow), "slow");
        assert_eq!(echo(fast), "fast");
    }

    /// 进程没了 → 下次请求**懒重启**（新 pid），不用调用方操心。
    #[tokio::test]
    async fn a_dead_host_restarts_lazily() {
        let Some(_py) = python3() else { return };
        let host = start(ECHO).await;
        let first = host.request(json!({}), None, None, Interrupt::Kill).await;
        let pid = |outcome: &Result<Outcome, String>| match outcome {
            Ok(Outcome::Reply(value)) => value["pid"].as_u64().unwrap(),
            other => panic!("要有应答：{other:?}"),
        };
        let first_pid = pid(&first);

        host.kill_group(crate::cancel::SIGKILL);
        tokio::time::sleep(Duration::from_millis(200)).await; // 让读线程看到 EOF

        let second = host.request(json!({}), None, None, Interrupt::Kill).await;
        assert_ne!(pid(&second), first_pid, "该是重启出来的新进程");
    }

    /// 请求体必须是 JSON 对象（本模块要往里塞 `id`）。
    #[tokio::test]
    async fn non_object_payloads_are_rejected() {
        let Some(_py) = python3() else { return };
        let host = start(ECHO).await;
        let err = host
            .request(json!("裸字符串"), None, None, Interrupt::Kill)
            .await
            .unwrap_err();
        assert!(err.contains("必须是 JSON 对象"), "{err}");
    }
}
