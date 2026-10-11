//! `repl` 工具：一个**会话内持久**的 IPython —— 调用它就像往一个交互式 IPython 里敲代码，
//! 变量 / import / 定义跨调用保留。
//!
//! 与其它内置工具的唯一区别是有**会话级状态**（那个 Python 子进程）：状态挂在 `Session` 上的
//! `state::SessionState` 槽里，本模块只管「取槽 → 懒启动 → 通信」。
//!
//! 进程 / 并发模型：
//!   - 一个**会话内持久**的 IPython 子进程，传输与进程生命周期全交给 [`crate::pyhost`]
//!     （一行一条 JSON、`id` 配对、读线程不受取消影响）；
//!   - 同一会话内的 `repl` 调用用 `tokio::sync::Mutex` 串行化（状态共享，不能并发跑）；
//!   - 取消 / 超时：给**子进程组**发 `SIGINT`（保状态），超时路径再等 [`INTERRUPT_GRACE`]
//!     看它能不能自己停下来，停不住才 `SIGKILL`（丢状态，下次调用重启）。
//!
//! 协议见 `repl_driver.py`（一行一条 JSON，**首行是清单** `{}`；见 `pyhost` 的模块头）。

use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex as AsyncMutex;

use crate::cancel::{CANCEL_TEXT, SIGINT};
use crate::pyhost::{DEFAULT_PYTHON, Interrupt, Outcome, PyHost, Spec};
use crate::tools::{
    Attachment, Limits, Tool, ToolBody, ToolCtx, ToolError, ToolOutput, ToolResult,
};

/// 驱动脚本正文（编译期嵌入；它自己用 IPython 的 `InteractiveShell`）。
const DRIVER: &str = include_str!("python/repl_driver.py");

/// 中断后等驱动回帧的宽限（超时路径用：及时停了就保住状态，停不住才杀）。
const INTERRUPT_GRACE: Duration = Duration::from_secs(3);

// 会话内持久的 IPython：状态活在本会话的进程里；`pie -r` 恢复会话**不会**带回解释器内存
// （重新起一个干净的）——活进程没法跨进程重启携带。
// 工具描述用 `#[schemars(description = …)]` 显式给（doc 注释不行：schemars 会把单换行
// 合并成空格，而这里要多行）—— 跟 `Edit` 上面那条注释同一个理由。
// `[解释器]` 头区那行与 `history()` 的来龙去脉见 AGENTS.md。
#[derive(Deserialize, JsonSchema)]
#[schemars(
    description = "会话内**持久**的 Python 解释器（IPython）：变量 / import / 定义跨调用保留，本会话后面每次调用都在同一个进程里。\n**要跑 Python 就优先用它**，别用 `bash` 反复 `python3 -c`（那每次都是新进程，状态不会留）：\n- 分几段写、边看边改（探查数据、调参数、试算法）：import 与读数据做一次，后面反复掏；\n- 有贵的准备（加载大文件 / 大表、连服务、起子进程）→ 做一次留着，别每段重来；\n- 要画图：matplotlib 的 figure 会随结果回传、在界面上直接显示 —— 画图只能用 repl；\n- 一次性 shell 命令（grep / 看目录 / 构建 / 测试）用 `bash` 更直接。\n结果头区总有一行 `[解释器] …`（当前有哪些名字 + 本次新增）：写代码前以它为准，别按记忆猜；要逐条转录与输出，调 `history()`。"
)]
pub struct Repl {
    /// 要执行的 Python 代码（可多行；和往 IPython 里敲的一样，支持 `%` 魔法与 `!shell`）
    pub code: String,
    /// 超时秒数（可选，无默认）
    pub timeout: Option<i64>,
    /// 私有参数：解释器（缺省 `[python] interpreter`，没配则 `python3` / Windows `python`；
    /// IPython 装在别的 venv 就指过去；开头的 `~` 会展开成 $HOME）
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
///
/// 传输/进程生命周期全交给 [`crate::pyhost`]；这里只留「本会话那个宿主句柄」
/// + 一把串行锁（解释器状态共享，同一会话的 `repl` 调用不能并发跑）。
#[derive(Default)]
struct ReplSession {
    host: AsyncMutex<Option<Arc<PyHost>>>,
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
    /// 本次执行产出的图（driver 把 matplotlib 的 figure 存成 PNG 后的**绝对路径**）。
    /// 只给界面用（模型看不到）——走结构化通道，不从正文里嗅探。
    #[serde(default)]
    images: Vec<String>,
    /// 解释器状态：driver 报的 `state`（见 `Namespace`）。`None` = **没上报**（假驱动/老驱动）
    /// —— 与「上报了、是空的」是两回事：前者不加头区那行，后者要加（“空”正是模型最该知道的事）。
    #[serde(default)]
    state: Option<Namespace>,
}

/// 解释器里现在有什么（driver 的 `_namespace` 口径）。
#[derive(Deserialize, Default)]
struct Namespace {
    /// 当前命名空间（非下划线、非 IPython 注入的），driver 已截断到 32。
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    total: usize,
    /// 本次执行新增的名字。
    #[serde(default)]
    defined: Vec<String>,
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
        let python =
            crate::config::expand_tilde(&_python.unwrap_or_else(|| DEFAULT_PYTHON.to_string()));
        let mut spec = Spec::new(
            "repl",
            &python,
            &_driver.unwrap_or_else(|| DRIVER.to_string()),
        );
        if let Some(path) = &ctx.transcript {
            // 解释器的 `history()` 读这份转录快照（每轮开头由 `Session` 重写）
            spec.env
                .push(("PIE_TRANSCRIPT".to_string(), path.display().to_string()));
        }
        let payload = json!({"code": code});
        // 超时按秒给；`0` / 负数 = 不限时（与旧行为一致）
        let deadline = timeout
            .filter(|t| *t > 0)
            .map(|t| Duration::from_secs(t as u64));
        // 取消/超时都先 `SIGINT`：解释器能把 `KeyboardInterrupt` 变成一帧回执，状态保住。
        let interrupt = Interrupt::Signal {
            sig: SIGINT,
            grace: INTERRUPT_GRACE,
        };

        // 取本会话的 REPL 槽；锁住整段交互（同一会话的 repl 调用串行）。
        let session = ctx.state.get_or_init(ReplSession::default);
        let mut guard = session.host.lock().await;

        // 启动 / 断线重启；最多试两次（第一次失败多半是子进程刚自己退了）。
        let mut last: Option<String> = None;
        for attempt in 0..2 {
            let host = match guard.clone() {
                Some(host) => host,
                None => match PyHost::start(&spec).await {
                    Ok((host, _manifest)) => {
                        *guard = Some(Arc::clone(&host));
                        host
                    }
                    Err(e) => {
                        last = Some(e);
                        if attempt == 0 {
                            continue;
                        }
                        break;
                    }
                },
            };
            match host
                .request(payload.clone(), ctx.cancel.as_ref(), deadline, interrupt)
                .await
            {
                Ok(Outcome::Reply(value)) => match serde_json::from_value::<Reply>(value) {
                    Ok(reply) => return Ok(format_reply(reply, &ctx, _max_lines, _max_bytes)),
                    Err(e) => last = Some(format!("[repl] 解释器返回了非法 JSON: {e}")),
                },
                Ok(Outcome::Cancelled) => return Ok(ToolOutput::text(CANCEL_TEXT)),
                Ok(Outcome::TimedOut) => {
                    // 宽限期内没停住 → 已杀进程组；下次调用会重启（状态丢失）。
                    return Ok(ToolOutput::text(format!(
                        "[repl] 执行超过 {}s 仍未停下，解释器已重启（之前的变量丢失）",
                        timeout.unwrap_or(0)
                    )));
                }
                Err(e) => last = Some(e),
            }
            *guard = None; // 下一轮重起
        }
        Err(ToolError(
            last.unwrap_or_else(|| "[repl] 解释器不可用".to_string()),
        ))
    }
}

/// 把一帧回执拼成工具输出：stdout / stderr / traceback 依次接上；超限则头部截断 + 落盘。
fn format_reply(
    reply: Reply,
    ctx: &ToolCtx,
    max_lines: Option<i64>,
    max_bytes: Option<i64>,
) -> ToolOutput {
    // 图像附件：driver 出的是**临时** PNG（一小时后它自己会清），这里转存进内容寻址的本地副本
    // 目录（`files/img-<hash>.png`）——会话 `__meta__.files` 记的 `local` 才立得住，`files gc`
    // 也才认得出它还被引用（见 `session::record_repl_images`）。读不到字节的（已被删掉的
    // 临时图）直接跳过。
    // 去向是 `Canvas`：**只给界面**（TUI 画布），模型看不到——模型自己知道它画了什么。
    let attachments: Vec<Attachment> = reply
        .images
        .iter()
        .filter_map(|s| std::fs::read(s).ok())
        .filter_map(|data| {
            match ctx.storage.store(crate::config::StoreType::Blob {
                data: &data,
                mime: "image/png",
            }) {
                Ok(path) => Some(Attachment::Canvas { path }),
                Err(e) => {
                    crate::log::warn(format!("[warn] repl 图副本写入失败: {e}"));
                    None
                }
            }
        })
        .collect();
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

    // 单元格输出一旦被消费就没了（不可再生）→ 超限把**全文**落盘、正文只留头段预览
    // （截断/落盘/指针的规矩都在 `ToolBody::fit` 里一处；落盘前缀取 `ctx.name`）。
    ToolOutput {
        headers: headers(&reply),
        body: ToolBody::fit(
            ctx,
            &body,
            Limits {
                lines: max_lines,
                bytes: max_bytes,
            },
        ),
        attachments,
    }
}

/// 输出头区：解释器状态那一行（`[解释器] 共 3 个：df, f, math（本次新增：df）`）。
///
/// 为什么每次调用都报（包括“空”）：模型看不到解释器内部，压缩之后它连自己写过的代码块都没了
/// —— “里面有什么”得由每次执行自己带回来，而不能靠模型另发一次询问（白搭一次往返）。
/// 为什么放**头区**：`head_prefix` 保留的是开头，超限截断/落盘时它得跟着走；
/// 而“输出很长”恰恰是最需要它的场合。
fn headers(reply: &Reply) -> Vec<String> {
    let Some(state) = &reply.state else {
        return Vec::new(); // driver 没上报（假驱动）—— 别凭空说“空”
    };
    let mut line = if state.total == 0 {
        "[解释器] 空".to_string()
    } else {
        let mut line = format!("[解释器] 共 {} 个：{}", state.total, state.names.join(", "));
        if state.total > state.names.len() {
            line.push('…');
        }
        line
    };
    if !state.defined.is_empty() {
        line.push_str(&format!("（本次新增：{}）", state.defined.join(", ")));
    }
    vec![line]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::Cancel;
    use crate::config::Storage;
    use crate::tools::Registry;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    /// 假驱动：不依赖 IPython，只按协议回帧。`call#N` 里的 N 证明**进程被复用**。
    const FAKE: &str = r#"
import json, os, sys, time
inp = os.fdopen(os.dup(sys.stdin.fileno()), "r", encoding="utf-8", buffering=1)
out = os.fdopen(os.dup(sys.stdout.fileno()), "w", encoding="utf-8", buffering=1)
out.write(json.dumps({"ready": True}) + "\n"); out.flush()
count = 0
while True:
    line = inp.readline()
    if not line:
        break
    request = json.loads(line)
    rid = request.get("id")
    code = request.get("code", "")
    count += 1
    if code == "stall":
        time.sleep(10)
    if code == "stderr":
        payload = {"stdout": "", "stderr": "oops\n", "error": None}
    elif code == "error":
        payload = {"stdout": "", "stderr": "", "error": "ValueError: boom"}
    elif code.startswith("ns:"):
        # 上报解释器状态 + 一段足够长的输出（测截断时状态行还在不在头区）
        payload = {"stdout": "".join("line%d\n" % i for i in range(1, 20)),
                   "stderr": "", "error": None,
                   "state": {"names": ["df", "f", "math"], "total": 3, "defined": ["df"]}}
    else:
        payload = {"stdout": "call#%d: %s\n" % (count, code), "stderr": "", "error": None}
    payload["id"] = rid
    out.write(json.dumps(payload) + "\n"); out.flush()
"#;

    /// 一启动就报错退出的假驱动（模拟「解释器里没装 IPython」）。
    const CRASH: &str = r#"
import sys
sys.stderr.write("ModuleNotFoundError: No module named 'IPython'\n")
sys.exit(3)
"#;

    /// 把「输入 code」当成一个文件路径回帧的假驱动：用它验证**图从临时目录转存进 `files/`**
    /// （不依赖 IPython / matplotlib）。
    const FAKE_IMAGES: &str = r#"
import json, os, sys
inp = os.fdopen(os.dup(sys.stdin.fileno()), "r", encoding="utf-8", buffering=1)
out = os.fdopen(os.dup(sys.stdout.fileno()), "w", encoding="utf-8", buffering=1)
out.write(json.dumps({"ready": True}) + "\n"); out.flush()
while True:
    line = inp.readline()
    if not line:
        break
    request = json.loads(line)
    path = str(request.get("code", "")).strip()
    images = [path] if os.path.isfile(path) else []
    payload = {"id": request.get("id"), "stdout": "", "stderr": "", "error": None, "images": images}
    out.write(json.dumps(payload) + "\n"); out.flush()
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

    fn registry() -> Registry {
        Registry::empty(HashMap::new()).with_tool::<Repl>("repl")
    }

    fn ctx_at(tmp: &Path) -> ToolCtx {
        ToolCtx {
            cancel: None,
            storage: Storage::at(tmp.to_path_buf()),
            transcript: None,
            state: Arc::new(crate::state::SessionState::default()),
            name: "repl".to_string(),
        }
    }

    async fn run_code(reg: &Registry, ctx: &ToolCtx, py: &str, code: &str) -> ToolOutput {
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
        assert!(a.to_text().starts_with("call#1:"), "{a:?}");
        assert!(
            b.to_text().starts_with("call#2:"),
            "同一会话应复用同一进程: {b:?}"
        );

        // 另一个会话（另一个 state）→ 全新进程
        let other = ctx_at(&tmp);
        let c = run_code(&reg, &other, py, "hello").await;
        assert!(
            c.to_text().starts_with("call#1:"),
            "新会话应是新进程: {c:?}"
        );
    }

    #[tokio::test]
    async fn stderr_and_traceback_are_joined_into_the_body() {
        let Some(py) = python3() else { return };
        let reg = registry();
        let ctx = ctx_at(&tempdir("body"));

        let e = run_code(&reg, &ctx, py, "stderr").await;
        assert_eq!(e.to_text(), "oops\n");
        let t = run_code(&reg, &ctx, py, "error").await;
        assert_eq!(t.to_text(), "ValueError: boom");
        // 异常是 REPL 的正常输出，不是工具失败（dispatch 返回 Ok）
    }

    /// 解释器状态行：模型压缩后看不到代码块，就靠这一行知道解释器里有什么。
    #[tokio::test]
    async fn namespace_digest_lands_in_the_header() {
        let Some(py) = python3() else { return };
        let reg = registry();
        let ctx = ctx_at(&tempdir("ns"));

        let out = run_code(&reg, &ctx, py, "ns:any").await;
        assert!(
            out.to_text()
                .starts_with("[解释器] 共 3 个：df, f, math（本次新增：df）\n\nline1\n"),
            "{out:?}"
        );
        // 没上报状态的驱动（上面的 FAKE）不该凭空多出这一行
        let plain = run_code(&reg, &ctx, py, "hello").await;
        assert!(plain.to_text().starts_with("call#"), "{plain:?}");
    }

    /// 输出超限落盘时状态行**得留在头区**（`head_prefix` 保留开头）——不然最需要它的场合恰好丢掉。
    #[tokio::test]
    async fn namespace_digest_survives_truncation() {
        let Some(py) = python3() else { return };
        let tmp = tempdir("ns-spill");
        let reg = registry();
        let ctx = ctx_at(&tmp);

        let args = json!({
            "code": "ns:long", "_python": py, "_driver": FAKE, "_max_lines": 2
        });
        let out = reg.dispatch("repl", &args, ctx.clone()).await.unwrap();
        assert!(out.to_text().starts_with("[解释器] 共 3 个："), "{out:?}");
        assert!(
            out.to_text().contains("[工具输出全文已保存: "),
            "指针也在头区：{out:?}"
        );
        assert!(
            out.to_text().contains("line1\nline2\n"),
            "只留开头两行：{out:?}"
        );
        assert!(out.body.spill.is_some());
    }

    /// 真驱动要 IPython（系统 `python3` 通常没有）→ 优先 `pie setup` 建的 `~/.pie/envs/base`
    /// （旧的手工约定 `~/.pie/repl` 仍然认），再退到 `python3`。
    fn python_with_ipython() -> Option<String> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let candidates = [".pie/envs/base/bin/python", ".pie/repl/bin/python"];
        candidates
            .into_iter()
            .filter_map(|rel| {
                home.clone()
                    .map(|h| h.join(rel).to_string_lossy().into_owned())
            })
            .chain(["python3".to_string()])
            .find(|py| {
                std::process::Command::new(py)
                    .args(["-c", "import IPython"])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            })
    }

    /// 跑**真驱动**（不传 `_driver`）——`history()` 那条路只有真解释器才有。
    async fn run_code_real(reg: &Registry, ctx: &ToolCtx, py: &str, code: &str) -> ToolOutput {
        let args = json!({"code": code, "_python": py});
        reg.dispatch("repl", &args, ctx.clone()).await.unwrap()
    }

    /// `history()`：解释器里能拿到**本会话的完整转录**，压缩指针默认展开。
    #[tokio::test]
    async fn history_reads_the_transcript_and_expands_compaction() {
        let Some(py) = python_with_ipython() else {
            return;
        };
        let tmp = tempdir("history");
        // 轮次归档：被压掉的那一轮原文（整轮的代码 + 输出）
        let archive = tmp.join("turn-abc");
        std::fs::write(
            &archive,
            serde_json::to_string_pretty(&json!([
                {"role": "assistant", "content": null,
                 "tool_calls": [{"id": "c1", "type": "function", "function":
                                 {"name": "repl", "arguments": "{\"code\": \"df = 42\"}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "42"}
            ]))
            .unwrap(),
        )
        .unwrap();
        // 快照（真跑时由 `Session::write_transcript` 每轮重写；这里是手造的）
        let snapshot = tmp.join("transcript.jsonl");
        std::fs::write(
            &snapshot,
            [
                json!({"role": "user", "content": "第一问"}).to_string(),
                json!({"role": "assistant", "content": "[轮次原文已保存: …]\n\n...[中间过程省略]...",
                       "compaction": {"kind": "turn", "path": archive}})
                .to_string(),
                json!({"role": "user", "content": "第二问"}).to_string(),
                json!({"role": "tool", "tool_name": "bash", "content": "<head>",
                       "compaction": {"kind": "tool", "path": "/tmp/全文.txt"}})
                .to_string(),
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();

        let reg = registry();
        let ctx = ToolCtx {
            transcript: Some(snapshot),
            ..ctx_at(&tmp)
        };
        let out = run_code_real(
            &reg,
            &ctx,
            &py,
            "print([(e['role'], e['turn']) for e in history()])\n\
             print(len(history(expand=False)), len(history(turn=0)))\n\
             print(history()[1]['tool_calls'][0]['function']['arguments'])\n\
             print(history()[4]['full_output_path'])\n",
        )
        .await;

        // 展开后：user(0) + 归档里的 assistant(0)/tool(0) + user(1) + tool(1)
        assert!(
            out.to_text()
                .contains("[('user', 0), ('assistant', 0), ('tool', 0), ('user', 1), ('tool', 1)]"),
            "{out:?}"
        );
        assert!(
            out.to_text().contains("4 3"),
            "expand=False 是压缩态（4 条）、turn=0 只那一轮（3 条）：{out:?}"
        );
        assert!(
            out.to_text().contains(r#"{"code": "df = 42"}"#),
            "归档里的代码要能被查到：{out:?}"
        );
        assert!(
            out.to_text().contains("/tmp/全文.txt"),
            "落盘全文的路径要带出来：{out:?}"
        );
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
        assert!(out.to_text().starts_with("[工具输出全文已保存:"), "{out:?}");
        assert!(
            out.to_text().contains("call#1: l1\nl2\n"),
            "只留开头两行: {out:?}"
        );
        let spill = out.body.spill.expect("应落盘");
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
        assert_eq!(out.to_text(), CANCEL_TEXT);
    }

    #[tokio::test]
    async fn timeout_is_reported() {
        let Some(py) = python3() else { return };
        let reg = registry();
        let ctx = ctx_at(&tempdir("timeout"));
        let args = json!({"code": "stall", "_python": py, "_driver": FAKE, "timeout": 1});
        let out = reg.dispatch("repl", &args, ctx).await.unwrap();
        assert!(out.to_text().contains("超过 1s"), "{out:?}");
    }

    /// driver 出的图是**临时** PNG → 工具层转存成 `files/img-<hash>.png`（会话才留得住它）。
    #[tokio::test]
    async fn driver_images_land_in_the_local_store() {
        let Some(py) = python3() else { return };
        let tmp = tempdir("images");
        let reg = registry();
        let ctx = ctx_at(&tmp);
        // 模拟 driver 落的临时图（内容不要求是真 PNG——工具层只读字节转存）
        let src = tmp.join("fig-tmp.png");
        std::fs::write(&src, b"fake-png-bytes").unwrap();

        let args = json!({
            "code": src.display().to_string(), "_python": py, "_driver": FAKE_IMAGES
        });
        let out = reg.dispatch("repl", &args, ctx).await.unwrap();
        assert_eq!(out.attachments.len(), 1, "{out:?}");
        let crate::tools::Attachment::Canvas { path: stored } = &out.attachments[0] else {
            panic!("repl 的图只给界面：{:?}", out.attachments[0]);
        };
        assert_eq!(
            stored.parent().unwrap(),
            tmp.join("files"),
            "转存进 files/：{stored:?}"
        );
        assert!(stored.to_string_lossy().ends_with(".png"), "{stored:?}");
        assert_eq!(std::fs::read(stored).unwrap(), b"fake-png-bytes");
    }

    /// 对模型暴露的 schema：只有 `code`（必填）+ `timeout`，私有参数不进。
    #[test]
    fn schema_exposes_code_and_hides_private_params() {
        let spec = registry().tool_specs().remove(0);
        assert_eq!(spec["function"]["name"], "repl");
        assert!(
            spec["function"]["description"]
                .as_str()
                .unwrap()
                .contains("IPython")
        );
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
