"""命令行入口：参照 tau 的命令行设计。

  pie [OPTIONS] [PROMPT]          交互 TUI（PROMPT 作为首条消息）或 print 一次性执行
  pie -p [PROMPT]                 非交互一次性执行（子 agent / 管道）
  pie -r | pie resume             恢复当前目录下最近的会话
  pie --session <id> [PROMPT]     恢复指定会话
  pie sessions                    列出历史会话
  pie files                       图片上传件维护（list/gc，各带 --all 走云端）
  pie setup                       交互式配置模型与网络参数
  pie context                     上下文压缩维护（info/verify/gc）
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import json
import os
import sys
import tempfile
from dataclasses import asdict
from datetime import datetime
from pathlib import Path
from typing import Any, Awaitable, Callable

from . import aio
from .session import Session
from .config import (
    CONFIG_FILE,
    PIE_DIR,
    REASONING_LEVELS,
    REASONING_NONE,
    Config,
    _prompt,
    build_system_prompt,
    ensure_config,
    parse_reserved_tokens,
)
from .context import CONTEXT_DIR, collect_context_garbage, referenced_raw_paths
from .files import (
    FILES_DIR,
    GC_PROTECT_HOURS,
    collect_file_garbage,
    iter_session_files,
    list_remote_files,
    purge_remote_files,
)
from .input import read_input
from .llm import OpenAILLM
from .tools import default_tools, parse_image_marker, tools_from_spec

try:
    from importlib.metadata import version as _pkg_version

    __version__ = _pkg_version("pie")
except Exception:
    __version__ = "0.1.0"

THINKING_LEVELS = ("off", "minimal", "low", "medium", "high", "xhigh", "max")

MAIN_EPILOG = """\
示例：
  pie                             新对话（TUI；非 TTY 从 stdin 读取任务一次性执行）
  pie "帮我看看这个项目"           交互 TUI，把该消息作为第一条输入
  pie -p "列出 /tmp 下的文件"      非交互一次性执行（子 agent 模式）
  cat 任务.txt | pie -p           任务从 stdin 读取
  pie -r                          恢复最近会话
  pie -r -p "继续刚才的任务"       在最近会话上非交互执行一条消息
  pie --session 20260831-103224    恢复指定会话
  pie -m deepseek-v4-flash "..."  本次运行指定模型（不持久化）
  pie --tools read,ls,grep "..."   限制工具：read + shell（仅 ls/grep）
  pie --tools read "..."           仅 read 工具（shell 禁用）
"""

CHAT_HELP = """\
命令：
  /exit, /quit   退出
  /reset         清空对话历史（保留 system prompt 与记忆）
  /clear         当前窗口写入 windows 归档，开新窗口
  /compact       手动压缩：/compact tools（工具级）、/compact turns（轮次级）、/compact（两者）
  /save [文件]   保存会话（不带参数则保存到当前会话文件）
  /thinking      查看思考深度（/thinking <none|low|high|max> 切换）
  /model         查看当前模型与可用列表（/model <id> 切换，重启仍生效）
  /status        查看当前 token 使用情况
  /help          显示本帮助
"""


def self_check() -> None:
    """不调用模型，验证内置工具与自动生成的 schema 可用（开发用）。"""
    reg = default_tools()
    with tempfile.TemporaryDirectory() as d:
        p = Path(d) / "sub" / "hello.txt"
        print(reg.dispatch("write", {"path": str(p), "content": "hello world\n"}))
        print(reg.dispatch("read", {"path": str(p)}))
        print(reg.dispatch("edit", {"path": str(p), "edits": [{"oldText": "hello", "newText": "hi"}]}))
        print(reg.dispatch("read", {"path": str(p)}))
        print(reg.dispatch("shell", {"command": f"echo yolo && wc -c {p}"}))
        # read 图片：1x1 PNG → 返回机器可读图片标记，parse_image_marker 可解析
        img = Path(d) / "pixel.png"
        img.write_bytes(base64.b64decode(_PNG_1X1_B64))
        img_out = reg.dispatch("read", {"path": str(img)})
        print(img_out)
        ref = parse_image_marker(img_out)
        print(f"图片标记解析: path={ref.path} mime={ref.mime} size={ref.size} dim={ref.width}x{ref.height}")
    print("\n自动生成的工具定义（read 示例）:")
    print(json.dumps(reg.definitions()[0], ensure_ascii=False, indent=2))
    print("self-check OK")


# 1x1 透明 PNG（read 图片分支的 self_check 用例）
_PNG_1X1_B64 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA"
    "60e6kgAAAABJRU5ErkJggg=="
)


def _safe_save(session: Session) -> None:
    """保存会话；目录不可写时只警告，不让 REPL 崩溃。"""
    try:
        session.save()
    except OSError as e:
        print(f"[warn] 会话保存失败（跳过）: {e}", file=sys.stderr)


def _read_cli_text(text_or_path: str) -> str:
    """--system-prompt / --append-system-prompt 的 TEXT_OR_PATH：存在则读文件，否则当字面文本。"""
    p = Path(text_or_path)
    if len(text_or_path) < 4096 and p.is_file():
        try:
            return p.read_text(encoding="utf-8")
        except OSError:
            pass
    return text_or_path


def _resolve_session(session_id: str) -> Path:
    """按 id / 文件名 / 路径解析会话文件。"""
    p = Path(session_id)
    if p.is_file():
        return p
    sessions_dir = PIE_DIR / "sessions"
    name = p.name if p.suffix == ".jsonl" else f"{p.name}.jsonl"
    for candidate in (sessions_dir / name, sessions_dir / f"chat-{name}"):
        if candidate.is_file():
            return candidate
    hits = sorted(
        (f for f in sessions_dir.glob("*.jsonl") if session_id in f.stem),
        key=lambda f: f.stat().st_mtime,
        reverse=True,
    )
    if hits:
        return hits[0]
    raise FileNotFoundError(f"找不到会话: {session_id}（~/.pie/sessions/ 中）")


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="pie",
        description="pie — 极简 agent harness（参照 tau 的命令行设计）",
        epilog=MAIN_EPILOG,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "prompt",
        nargs="?",
        help="初始消息：交互模式作为第一条消息进入 TUI；print/非 TTY 模式作为一次性任务（省略时从 stdin 读取）",
    )
    parser.add_argument("-p", "--print", action="store_true", help="非交互一次性执行（子 agent 模式），输出最终答案后退出")
    parser.add_argument(
        "--mode",
        choices=("text", "json", "transcript"),
        default="text",
        help="print 模式的输出格式（默认 text）",
    )
    parser.add_argument("-m", "--model", metavar="NAME", help="本次运行的模型（覆盖配置，不持久化）")
    parser.add_argument(
        "-t",
        "--thinking",
        choices=THINKING_LEVELS,
        metavar="LEVEL",
        help="本次思考强度（off/minimal/low/medium/high/xhigh/max，覆盖配置，不持久化）",
    )
    parser.add_argument(
        "--reserved-tokens",
        "--max-tokens",  # 旧名保留为别名（它就是 API 的 max_tokens）
        dest="reserved_tokens",
        metavar="N",
        help="每次请求为输出预留的 token（即 API 的 max_tokens；例：131072 / 128k / auto；"
        "覆盖配置，不持久化。默认 256000，auto = 不发送该参数、用服务端默认：DeepSeek 思考模式 64K、上限 384K）",
    )
    parser.add_argument("-c", "--config", metavar="FILE", help="指定配置文件（默认 ~/.pie/config.toml）")
    parser.add_argument("-r", "--resume", action="store_true", help="恢复当前目录下最近的会话")
    parser.add_argument("--cwd", metavar="PATH", help="内置工具的工作目录（默认当前目录）")
    parser.add_argument("--session", metavar="ID", help="恢复指定会话（id / 文件名 / 路径）")
    parser.add_argument("--session-id", metavar="ID", help="为新会话指定精确 id（print 模式会保存到该文件）")
    parser.add_argument(
        "--tools",
        metavar="SPEC",
        help=(
            "限制可用工具（仅本次运行）。内置工具名 read/edit/write/shell 直接启用；"
            "非内置名视为 shell 允许的子命令白名单（并隐式启用受限 shell）。"
            "例：--tools read,ls,grep（read + shell 仅 ls/grep）；--tools read（仅 read）。"
            "默认启用全部工具。"
        ),
    )
    parser.add_argument(
        "--system-prompt",
        metavar="TEXT_OR_PATH",
        help="替换默认 SYSTEM.md 基础提示（字面文本或 UTF-8 文件）",
    )
    parser.add_argument(
        "--append-system-prompt",
        metavar="TEXT_OR_PATH",
        action="append",
        default=[],
        help="追加到 system prompt（可重复）",
    )
    parser.add_argument(
        "--auto-compact-threshold",
        type=int,
        metavar="TOKENS",
        help="上下文 token 估算超过该值即自动压缩（覆盖软阈值，不持久化）",
    )
    parser.add_argument("--timeout-seconds", type=float, metavar="SECONDS", help="HTTP 超时（默认 60.0）")
    parser.add_argument(
        "--max-retries",
        type=int,
        metavar="N",
        help="请求重试次数（默认 2；只重试连接/超时/408/409/429/5xx）",
    )
    parser.add_argument(
        "--max-retry-delay-seconds",
        type=float,
        metavar="SECONDS",
        help="重试等待上限秒数（默认 1.0；实际等待 = max(1.0, random(0, 该值))）",
    )
    parser.add_argument("-v", "--version", action="version", version=f"pie {__version__}")
    return parser


def _run(args: argparse.Namespace) -> int:
    cfg = ensure_config(config_file=args.config)
    # 运行时覆盖（不持久化）
    if args.model:
        cfg.model = args.model
    if args.thinking:
        # -t/--thinking 的 off 是给人看的名词，API 只认 none（其余 minimal/medium/xhigh 服务端兼容接受）
        cfg.reasoning_effort = (
            REASONING_NONE if args.thinking == "off" else args.thinking
        )
    if args.reserved_tokens is not None:
        cfg.reserved_tokens = parse_reserved_tokens(args.reserved_tokens)
    if args.timeout_seconds is not None:
        cfg.timeout_seconds = args.timeout_seconds
    if args.max_retries is not None:
        cfg.max_retries = args.max_retries
    if args.max_retry_delay_seconds is not None:
        cfg.max_retry_delay_seconds = args.max_retry_delay_seconds
    if args.auto_compact_threshold is not None:
        cfg.auto_compact_threshold = args.auto_compact_threshold
    if args.cwd:
        os.chdir(args.cwd)

    # print 模式：-p 强制；否则非 TTY 自动进入（子 agent / 管道）
    print_mode = bool(args.print) or not sys.stdin.isatty()

    system_override = _read_cli_text(args.system_prompt) if args.system_prompt else None
    appends = [_read_cli_text(t) for t in args.append_system_prompt]

    resume_requested = bool(args.resume or args.session)
    registry = tools_from_spec(args.tools)
    try:
        if args.session:
            session = Session.load(_resolve_session(args.session), config=cfg, tools=registry)
        elif resume_requested:
            session = Session.resume(config=cfg, tools=registry)
        else:
            session = Session.new(config=cfg, tools=registry, session_id=args.session_id)
    except FileNotFoundError as e:
        print(f"恢复失败: {e}", file=sys.stderr)
        return 1

    if system_override is not None or appends:
        session.messages.messages[0].content = build_system_prompt(
            cfg, system_prompt=system_override, append_prompts=appends
        )

    if print_mode:
        save = bool(args.resume or args.session or args.session_id)
        return _print_main(session, args, save=save)
    return _interactive_main(session, args.prompt, resumed=resume_requested)


def _print_main(session: Session, args: argparse.Namespace, save: bool) -> int:
    """非交互一次性执行：在 session 上跑一条消息，按 --mode 输出。"""
    prompt = args.prompt
    if prompt is None:
        prompt = sys.stdin.read().strip()
    if not prompt:
        print("请提供任务描述，或通过 stdin 传入", file=sys.stderr)
        return 2
    if len(prompt) > 40_000:
        print(
            f"[warn] 任务长达 {len(prompt)} 字符，建议改用 stdin 传入，避免命令行参数长度限制",
            file=sys.stderr,
        )
    cfg = session.config
    cfg.verbose = False  # stdout 只输出结果，便于被 shell 捕获
    try:
        answer = session.turn(prompt)
    except Exception as e:
        print(f"运行失败: {type(e).__name__}: {e}", file=sys.stderr)
        return 1
    if save:
        _safe_save(session)
    if args.mode == "json":
        print(
            json.dumps(
                {
                    "answer": answer,
                    "session": str(session.path),
                    "turns": session.turn_count,
                    "usage": asdict(session.usage),
                },
                ensure_ascii=False,
            )
        )
    elif args.mode == "transcript":
        print(json.dumps(session.full_history(), ensure_ascii=False, indent=1))
    else:
        print(answer)
    return 0


def _interactive_main(session: Session, initial_prompt: str | None, resumed: bool) -> int:
    """交互模式：TTY 走 Textual TUI，否则回退 readline。"""
    cfg = session.config
    if sys.stdin.isatty():
        try:
            from .tui import run_tui

            run_tui(session, initial_prompt)
            return 0
        except Exception as exc:  # Textual 初始化失败等 → 回退
            print(
                f"[warn] TUI 启动失败，已回退 readline 模式: {type(exc).__name__}: {exc}",
                file=sys.stderr,
            )

    print(
        f"pie：{'恢复会话 ' + str(session.path) if resumed else '新对话'}"
        "（/help 查看命令，/exit 退出，Ctrl-D 也可）",
        file=sys.stderr,
    )
    # readline 回退模式（非 TTY / TUI 启动失败）：启动时同步拉取可用模型列表
    # （/model 查看与切换用）；TTY 走 TUI 时由 PieApp.on_mount 后台拉取，不在此阻塞。
    try:
        aio.run(session.fetch_models(timeout=8))
    except Exception as e:
        print(f"[warn] 获取可用模型列表失败: {e}（/model <id> 仍可直接切换）", file=sys.stderr)
    if initial_prompt:
        try:
            answer = session.turn(initial_prompt)
        except Exception as e:
            print(f"运行失败: {type(e).__name__}: {e}", file=sys.stderr)
        else:
            print(answer)
            _safe_save(session)
    try:
        while True:
            try:
                line = read_input(">>> ", history_file=PIE_DIR / "history.txt")
            except EOFError:
                print(file=sys.stderr)
                break
            line = line.strip()
            if not line:
                continue
            if line.startswith("/"):
                cmd, _, arg = line.partition(" ")
                if cmd in ("/exit", "/quit"):
                    break
                if cmd == "/help":
                    print(CHAT_HELP)
                elif cmd == "/reset":
                    session.reset()
                    print("已清空历史（保留 system prompt 与记忆）")
                elif cmd == "/clear":
                    session.clear_window()
                    print(f"已切换新窗口（归档 {len(session.windows)} 个历史窗口块，文件在 ~/.pie/windows/）")
                elif cmd == "/compact":
                    mode = arg.strip() or "auto"
                    if mode not in ("auto", "tools", "turns"):
                        print(f"未知压缩模式: {mode}（/compact [tools|turns]）")
                    else:
                        stats = session.compact(mode=mode)
                        if stats.get("skipped"):
                            print(f"未压缩：{stats['skipped']}")
                        else:
                            print(
                                f"压缩完成：节省约 {stats['saved_tokens']:,} tokens"
                                f"（tools={stats['tools']}，turns={stats['turns']}）"
                            )
                        _safe_save(session)
                elif cmd == "/save":
                    if arg.strip():
                        session.path = Path(arg.strip())
                    _safe_save(session)
                    if session.path is not None:
                        print(f"会话已保存: {session.path}")
                elif cmd == "/status":
                    print(session.usage_report())
                elif cmd == "/thinking":
                    level = arg.strip().lower()
                    if not level:
                        print(
                            f"当前思考深度: {session.config.reasoning_effort}"
                            f"（可选: {' / '.join(REASONING_LEVELS)}）"
                        )
                    elif level not in REASONING_LEVELS:
                        print(f"未知思考级别: {level}（可选: {' / '.join(REASONING_LEVELS)}）")
                    else:
                        note = session.set_reasoning_effort(level)
                        print(f"思考深度: {level}（{note}，重启后仍生效）")
                elif cmd == "/model":
                    name = arg.strip()
                    avail = session.available_models
                    if not name:
                        cur = session.config.model
                        print(f"当前: {cur}")
                        if avail:
                            for m in avail:
                                print(f"  {m}  ←" if m == cur else f"  {m}")
                        else:
                            print("（可用模型列表未获取到：/model <id> 直接切换，或 /model refresh 重新拉取）")
                    elif name == "refresh":
                        try:
                            models = aio.run(session.fetch_models(timeout=8))
                            print(f"已获取可用模型 {len(models)} 个（/model 查看）")
                        except Exception as e:
                            print(f"获取失败: {e}")
                    elif avail and name not in avail:  # avail 为空（=列表未获取到）时不拦，允许手动指定
                        print(f"未知模型: {name}（/model 查看可用 {len(avail)} 个；/model refresh 重新拉取）")
                    else:
                        note = session.set_model(name)
                        print(f"模型已切换: {name}（{note}），下个请求生效")
                else:
                    print(f"未知命令: {cmd}（/help 查看）")
                continue
            try:
                answer = session.turn(line)
            except Exception as e:  # 单条消息失败不退出
                print(f"运行失败: {type(e).__name__}: {e}", file=sys.stderr)
                continue
            print(answer)
            _safe_save(session)
            if cfg.verbose:
                print(f"[saved] {session.path}", file=sys.stderr)
    finally:
        if len(session.messages) > 1:
            _safe_save(session)
    return 0


def sessions_main(argv: list[str]) -> int:
    """pie sessions：列出历史会话。"""
    parser = argparse.ArgumentParser(prog="pie sessions", description="列出历史会话")
    parser.add_argument("-l", "--limit", type=int, default=20, help="最多列出 N 个（默认 20）")
    parser.add_argument("--all", action="store_true", help="列出全部会话（忽略 --limit）")
    parser.add_argument("--json", action="store_true", help="输出 JSON")
    args = parser.parse_args(argv)
    sessions_dir = PIE_DIR / "sessions"
    files = sorted(
        (f for f in sessions_dir.glob("*.jsonl") if f.is_file()),
        key=lambda f: f.stat().st_mtime,
        reverse=True,
    )
    if not args.all:
        files = files[: max(0, args.limit)]
    rows: list[dict[str, Any]] = []
    for f in files:
        turns = api_calls = 0
        first_query = ""
        try:
            for line in f.read_text(encoding="utf-8").splitlines():
                if not line.strip():
                    continue
                d = json.loads(line)
                if d.get("__meta__"):
                    api_calls = d.get("usage", {}).get("calls", 0)
                    first_query = d.get("title") or first_query
                elif d.get("role") == "user" and not d.get("synthetic"):  # 图片消息不算用户轮
                    turns += 1
                    if not first_query and isinstance(d.get("content"), str):
                        first_query = d["content"].strip()
        except (OSError, json.JSONDecodeError):
            pass
        rows.append(
            {
                "id": f.stem,
                "file": str(f),
                "mtime": f.stat().st_mtime,
                "size": f.stat().st_size,
                "turns": turns,
                "api_calls": api_calls,
                "first_query": first_query,
            }
        )
    if args.json:
        print(json.dumps(rows, ensure_ascii=False, indent=1))
        return 0
    if not rows:
        print("暂无会话（~/.pie/sessions/）")
        return 0
    for r in rows:
        print(
            f"{r['id']}  turns={r['turns']}  api_calls={r['api_calls']}  "
            f"{datetime.fromtimestamp(r['mtime']):%Y-%m-%d %H:%M}"
        )
        q = r["first_query"]
        if q:
            disp = q.replace("\n", " ").replace("\r", " ")
            disp = disp[:80] + ("…" if len(disp) > 80 else "")
            print(f"    ↳ {disp}")
        else:
            print("    ↳ (无用户消息)")
    return 0


def setup_main(argv: list[str]) -> int:
    """pie setup：交互式配置模型与网络参数（写入 ~/.pie/config.toml）。"""
    parser = argparse.ArgumentParser(prog="pie setup", description="配置模型与网络参数（写入配置文件）")
    parser.add_argument("-c", "--config", metavar="FILE", help="指定配置文件（默认 ~/.pie/config.toml）")
    args = parser.parse_args(argv)
    path = Path(args.config) if args.config else CONFIG_FILE
    cfg = Config.load(path) if path.exists() else Config()
    print(f"配置将写入 {path}（直接回车保留当前值）", file=sys.stderr)
    cfg.model = _prompt("模型名", cfg.model)
    cfg.base_url = _prompt("API 地址（OpenAI 兼容）", cfg.base_url)
    cfg.api_key = _prompt("API key", cfg.api_key)
    cfg.timeout_seconds = float(_prompt("HTTP 超时秒数", str(cfg.timeout_seconds)))
    cfg.max_retries = int(_prompt("重试次数", str(cfg.max_retries)))
    cfg.save(path)
    print(f"已写入 {path}", file=sys.stderr)
    return 0


def context_main(argv: list[str]) -> int:
    """上下文压缩维护：info / verify / gc。"""
    parser = argparse.ArgumentParser(prog="pie context", description="上下文压缩维护工具")
    sub = parser.add_subparsers(dest="action", required=True)
    sub.add_parser("info", help="列出所有压缩事件")
    sub.add_parser("verify", help="校验 manifest 引用的原文文件是否存在")
    gc = sub.add_parser("gc", help="列出 / 删除未被引用的 context 文件")
    gc.add_argument("--delete", action="store_true", help="真正删除未引用文件")
    args = parser.parse_args(argv)

    if not CONTEXT_DIR.exists():
        print("暂无压缩记录（~/.pie/context 不存在）")
        return 0

    if args.action == "info":
        for manifest in sorted(CONTEXT_DIR.glob("*.manifest.jsonl")):
            print(f"# {manifest.name}")
            for line in manifest.read_text(encoding="utf-8").splitlines():
                if line.strip():
                    print(" ", line)
        return 0

    if args.action == "verify":
        missing: list[str] = [str(p) for p in referenced_raw_paths() if not p.exists()]
        if missing:
            print(f"缺失 {len(missing)} 个原文文件:")
            for p in missing:
                print(" ", p)
            return 1
        print("OK：所有 manifest 引用的原文文件都在")
        return 0

    garbage = collect_context_garbage()
    print(f"未引用文件 {len(garbage)} 个:")
    for p in garbage:
        print(" ", p)
    if args.delete:
        for p in garbage:
            p.unlink(missing_ok=True)
        print(f"已删除 {len(garbage)} 个文件")
    return 0


def files_main(argv: list[str]) -> int:
    """图片上传件维护：list（各会话记的图片；`--all` 改列云端）/ gc（本地副本回收；`--all` 另清空云端）。

    上传结果按会话存在 `__meta__.files`（不做全局缓存），所以这里只是把会话里的记录
    读出来看看；本地副本 `~/.pie/files/` 是跨会话共享的，回收靠一次无状态扫描。
    服务端那份默认不主动动：它由上传时的 `expires_after`（默认 30 天）自行过期。
    两个 `--all` 都直接调 Files API（见文件下部，都以「远端是本账号全局的」为前提）：
    `list --all` 把云端那份列出来看看，`gc --all` 把云端那份立刻清空。

    gc 的判据是「未被任何会话引用 **且** 已放了超过 GC_PROTECT_HOURS 小时」：
    后者是给「刚粘贴进 files/、还没来得及 read」的图留的保护窗口（见 clipboard.py）。
    """
    parser = argparse.ArgumentParser(prog="pie files", description="图片上传件维护工具")
    sub = parser.add_subparsers(dest="action", required=True)
    ls = sub.add_parser("list", help="列出各会话记录的图片（本地副本 + file_id + 过期时间）；加 --all 改列云端")
    ls.add_argument("--all", action="store_true", help="调 Files API 列出服务端本账号的全部上传件")
    ls.add_argument("-c", "--config", metavar="FILE", help="指定配置文件（默认 ~/.pie/config.toml）")
    gc = sub.add_parser(
        "gc", help=f"本地副本回收（未被引用且超过 {GC_PROTECT_HOURS} 小时）；加 --all 另清空云端"
    )
    gc.add_argument("--delete", action="store_true", help="真正删除未引用副本")
    gc.add_argument(
        "--all", action="store_true", help="调 Files API 删除服务端本账号的全部上传件（云端缓存清零）"
    )
    gc.add_argument("-c", "--config", metavar="FILE", help="指定配置文件（默认 ~/.pie/config.toml）")
    args = parser.parse_args(argv)

    if args.action == "list":
        if args.all:
            return _files_remote_list(Path(args.config) if args.config else None)
        rows = list(iter_session_files())
        if not rows:
            print(f"暂无图片记录（会话 __meta__.files 为空；副本目录 {FILES_DIR}）")
            return 0
        for session_file, image_hash, entry in rows:
            expires_at = entry.get("expires_at")
            expires_txt = (
                datetime.fromtimestamp(expires_at).strftime("%Y-%m-%d %H:%M")
                if expires_at
                else "永久"
            )
            print(
                f"{image_hash}  {int(entry.get('size') or 0):>10,} B  {entry.get('mime') or '?'}  "
                f"{Path(str(entry.get('local') or '')).name}"
            )
            print(
                f"    file_id={entry.get('file_id')}  过期={expires_txt}  "
                f"源={entry.get('src')}"
            )
            print(f"    会话={session_file.stem}")
        return 0

    garbage = collect_file_garbage()
    print(f"可回收的本地副本 {len(garbage)} 个（未被任何会话引用、且已放置超过 {GC_PROTECT_HOURS} 小时）:")
    for path in garbage:
        print(" ", path)
    if args.delete:
        for path in garbage:
            path.unlink(missing_ok=True)
        print(f"已删除 {len(garbage)} 个文件")
    if args.all:
        return _files_gc_remote(Path(args.config) if args.config else None)
    return 0


def _fmt_ts(value: Any, default: str = "?") -> str:
    """unix 秒 → `%Y-%m-%d %H:%M`；空值/坏值给 `default`（服务端字段缺失时别崩）。"""
    try:
        if not value:
            return default
        return datetime.fromtimestamp(float(value)).strftime("%Y-%m-%d %H:%M")
    except (TypeError, ValueError, OSError, OverflowError):
        return default


def _files_api_call(
    config_file: Path | None, note: str, work: Callable[[Any], Awaitable[Any]]
) -> tuple[Any, str | None]:
    """按配置建 Files API 客户端 → `aio.run(work(client))` → 用完关闭。

    返回 `(结果, 错误信息)`：没配 api_key、或调用本身抛异常时结果是 `None`、错误是一句
    人话（调用方打印到 stderr 并返回 1 —— 云端没列出来/没清干净不能假装成功）。
    这是不可逆操作，所以先把「打到哪个账号」写清楚（多套配置 / 默认 key 时看得出来）。
    """
    cfg = Config.load(config_file)
    if not cfg.api_key:
        return None, "未配置 api_key，无法调用 Files API（先跑 pie setup）"
    print(f"Files API: {cfg.base_url}  key=…{cfg.api_key[-4:]}  （{note}）")
    backend = OpenAILLM(
        api_key=cfg.api_key,
        base_url=cfg.base_url,
        timeout=cfg.timeout_seconds,
        max_retries=cfg.max_retries,
        max_retry_delay_seconds=cfg.max_retry_delay_seconds,
    )

    async def _call():
        client = backend.files_client()
        try:
            return await work(client)
        finally:
            with contextlib.suppress(Exception):
                await client.close()

    try:
        return aio.run(_call()), None
    except Exception as e:  # 网络 / 鉴权挂了就到此为止
        return None, f"调用 Files API 失败: {type(e).__name__}: {e}"


def _local_file_index() -> dict[str, list[str]]:
    """`file_id` → 记着它的会话名（读各会话 `__meta__.files`），给 `list --all` 标注用。"""
    index: dict[str, list[str]] = {}
    for session_file, _image_hash, entry in iter_session_files():
        file_id = entry.get("file_id")
        if file_id:
            index.setdefault(str(file_id), []).append(session_file.stem)
    return index


def _files_remote_list(config_file: Path | None = None) -> int:
    """`pie files list --all`：列出服务端本账号的全部上传件（顺带标出哪个会话记着它）。"""
    rows, error = _files_api_call(config_file, "列出本账号下的全部上传件", list_remote_files)
    if error is not None:
        print(error, file=sys.stderr)
        return 1
    if not rows:
        print("服务端没有上传件（云端为空）")
        return 0
    local = _local_file_index()
    print(f"服务端上传件 {len(rows)} 个（云端那份；本地记录见不带 --all 的 `pie files list`）:")
    for info in rows:
        sessions = local.get(str(info["id"]))
        print(f"{info['id']}  {int(info.get('bytes') or 0):>10,} B  {info.get('filename') or '?'}")
        print(
            f"    上传={_fmt_ts(info.get('created_at'))}  "
            f"过期={_fmt_ts(info.get('expires_at'), default='永久')}  "
            f"会话={'、'.join(sessions) if sessions else '未记录'}"
        )
    return 0


def _files_gc_remote(config_file: Path | None = None) -> int:
    """`pie files gc --all`：调 Files API 清空服务端上传件（本地副本 / 会话记录不动）。"""
    result, error = _files_api_call(
        config_file, "被删的是本账号下的全部上传件", purge_remote_files
    )
    if error is not None:
        print(error, file=sys.stderr)
        return 1
    deleted, failed = result
    for info in deleted:
        size_txt = f"{int(info['bytes']):,} B" if info.get("bytes") else "?"
        name = info.get("filename") or "?"
        print(
            f"  已删除 {info['id']}  {name:<24} {size_txt:>12}  "
            f"上传={_fmt_ts(info.get('created_at'))}"
        )
    print(f"服务端上传件：已删除 {len(deleted)} 个")
    for file_id, err in failed.items():
        print(f"  删除失败 {file_id}: {err}", file=sys.stderr)
    if failed:
        print(f"{len(failed)} 个删除失败（见 stderr）", file=sys.stderr)
    return 1 if failed else 0


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv and argv[0] == "resume":
        argv = ["-r", *argv[1:]]
    elif argv and argv[0] == "sessions":
        return sessions_main(argv[1:])
    elif argv and argv[0] == "files":
        return files_main(argv[1:])
    elif argv and argv[0] == "setup":
        return setup_main(argv[1:])
    elif argv and argv[0] == "context":
        return context_main(argv[1:])
    return _run(_parser().parse_args(argv))





