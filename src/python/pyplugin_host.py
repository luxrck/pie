# pie 的 Python 工具宿主：由二进制以 `python -X utf8 -u -c "<本文件正文>" <工具文件…>` 启动
# （`rust` 侧见 `pyplugin.rs`）。
#
# 协议：一行一个 JSON，一问一答；**应答可乱序**（并发跑在线程池里），靠 `id` 配对。
#   stdout 第一行  ← {"tools":[…], "extensions":{"subscribe":["tool_call",…]}, "warnings":[…]}
#   stdin  每行    → 工具调用：{"id":N,"tool":"名字","arguments":{…}}
#                   事件    ：{"event":{…},"ctx":{…}}          （只发一行：观察，不等回话）
#                             {"id":N,"event":{…},"ctx":{…}}   （要回话：可否决的 kind，如 tool_call）
#   stdout 之后    ← {"id":N,"text":"…"} / {"id":N,"error":"…"}
#                   事件回话：{"id":N,"deny":"理由"|null}      （null = 通过）
#
# **反方向**（插件要改当前实例，`S.set_model(…)` 那种）——两个方向各用各的键，不撞车：
#   stdin  每行    → 动作    ：{"call":N,"session":"…","method":"…","args":{…}}
#   stdout 之后    ← 动作回执：{"call_reply":N,"result":{"ok":true,"effective":"now"|"next_step"}}
#
# 插件（一个文件可以只写 @tool、只写 session.on(…)，或者都写）：
#   def no_rm(ev, S):            # ev = 事件 dict（形状与 Python 绑定那套完全一致）
#       if ev["tool"] == "bash" and "rm " in ev["arguments"]["command"]:
#           return "策略拒绝：检测到 rm"     # 返回一句理由 = **否决**（只有 tool_call 有这档语义）
#       return None                       # None = 通过
#   session.on("tool_call", no_rm)
# `S` 是「当前会话」的代理：属性 = 事实（`session_id` / `path` / `model` / `cwd`），
# 方法 = 动作（`S.set_model(…)` / `S.cancel()` …）—— 一次远程调用，回执里的 `effective` 说清是
# 当场生效（`cancel`）还是排到下一个 apply point（其余）。
#
# **用真绑定，不嵌副本**：`tool` / `Tool` 直接 `from pie import …`，所以**那个解释器里得装着
# Python 绑定 `pie`**（工具文件里的 `@tool` 就是它提供的）；装不上整个宿主起不来（这里打印原因，
# Rust 侧再报一句「没有任何工具」）。好处是只有一份实现：与绑定永远同版本、不会分叉。
#
# 为了让工具文件少写一行，加载每个文件前把 `tool` / `Tool` **注进它的 globals** —— `@tool()`
# 直接用就行；写 `from pie import tool` 也一样（本来就是同一个类）。
#
# 三条纪律：
#   - 工具文件 import 期抛异常 → 记进 `warnings` 跳过，不带走宿主；
#   - 工具 handler 抛异常 / 返回非 str → 变成 `error` 行（Rust 侧文本化回给模型，回合照跑）；
#   - handler 缺省什么都不写 stdout：**协议行走 dup 出来的原始 fd 1**，`sys.stdout` 已换成
#     stderr，用户工具里任何 `print` 都污染不了协议。
#
# 同步/异步都能装：**async handler 跑在一个后台 loop 上**（`_LOOP`，进程级一份 → 跨调用共享
# 连接池，一个 handler 内部还能 `asyncio.gather` 并发），同步 handler 照旧跑在线程池里。

import asyncio
import concurrent.futures
import importlib.util
import inspect
import itertools
import json
import os
import sys
import threading
import types

# 协议输出：dup 一份**原始** stdout（后面就把 sys.stdout 换成 stderr）
_PROTOCOL = os.fdopen(os.dup(1), "w", encoding="utf-8", buffering=1)
sys.stdout = sys.stderr

# 真绑定（见文件头）：装不上就整个宿主起不来 —— 把原因写清楚（会进 pie 的告警）。
try:
    from pie import Tool, tool
except ImportError:
    print(
        "pyplugin 需要 Python 绑定 `pie`（工具里的 @tool 就是它提供的）：请把它装进 %s"
        % sys.executable,
        file=sys.stderr,
    )
    raise

# 并发度 = 一批 tool_calls 的条数（真并发只对 IO 型 handler 有效：GIL 会串起纯 CPU 的）。
# ⚠ async handler 也占一个 worker（线程在 `run_coroutine_threadsafe(...).result()` 上等），
# 所以这里就是「一批同时最多几条」的上限。
_MAX_WORKERS = 8

# async handler 的后台 loop（进程级一份）：跑在一个 daemon 线程里，用
# `run_coroutine_threadsafe` 把协程递过去。共享一份 loop = 跨调用复用 client / 连接池。
# ⚠ async handler 里别干重的 CPU 活（那是单线程，会把别的 async 调用一起卡住）。
_LOOP = asyncio.new_event_loop()
threading.Thread(target=_LOOP.run_forever, daemon=True, name="pie-pyplugin-loop").start()


class _Proxy:
    """插件看到的「当前会话」：属性 = **事实**（Rust 侧发的那份 `ctx`），方法 = **动作**。

    动作 = 一次远程调用（Rust 侧执行）。立即档（`cancel`）当场生效；其余排到回合的下一个
    apply point —— 回执里的 `effective` 说清是哪档，插件别拿 `"next_step"` 当「已生效」。
    方法白名单就是会话自己那批公开操作（不另造 API）。
    """

    def __init__(self, ctx, state=None, readonly=False):
        self.__dict__.update(ctx or {})
        self.__dict__["_session"] = (ctx or {}).get("session_id", "")
        # `state` 是**本会话 + 本工具**的 dict（形状由工具自己定）；事件里给只读视图。
        self.__dict__["state"] = (
            types.MappingProxyType(state or {}) if readonly else dict(state or {})
        )

    def _act(self, method, **args):
        return _call(self._session, method, args)

    def cancel(self):
        """停掉当前回合（立即生效）。"""
        return self._act("cancel")

    def set_model(self, name):
        """换模型（下一个 apply point 生效，即下一步请求起）。"""
        return self._act("set_model", name=name)

    def set_reasoning_effort(self, level):
        """换思考档位（下一个 apply point 生效）。"""
        return self._act("set_reasoning_effort", level=level)

    def compact(self, mode="auto"):
        """手动压缩；`mode` = `auto` | `tools` | `turns`。"""
        return self._act("compact", mode=mode)

    def clear_window(self):
        """归档当前窗口（等于 `/clear`）。"""
        return self._act("clear_window")

    def reset(self):
        """清空历史（只留 system prompt）。"""
        return self._act("reset")


class _Session:
    """插件注册事件订阅的地方（`session.on(kind, fn)`）。

    装载期就被注进每个插件文件的 globals（和 `tool` / `Tool` 一样），
    所以插件文件里直接写 `session.on(...)` 就行。
    """

    def __init__(self):
        self.handlers = {}  # kind → [fn]

    def on(self, kind, fn):
        self.handlers.setdefault(kind, []).append(fn)
        return fn


SESSION = _Session()


# 写协议通道的锁：worker 线程 / 后台 loop / `_call` 都可能同时写（一条一条，别交错成半行）。
_EMIT_LOCK = threading.Lock()


# 动作（插件改当前实例）的往返：`{"call":N,…}` → `{"call_reply":N,"result":{…}}`。
_CALL_ID = itertools.count(1)
_CALL_LOCK = threading.Lock()
_CALLS = {}  # id → {"event": Event, "result": …}
CALL_TIMEOUT = 10.0


def _emit(obj, lock=None):
    """往协议通道写一行（默认用模块级那把锁）。"""
    line = json.dumps(obj, ensure_ascii=False) + "\n"
    with (lock or _EMIT_LOCK):
        _PROTOCOL.write(line)


def _call(session, method, args):
    """发一次动作给 Rust 侧，等回执（worker 线程里阻塞等；主线程负责把回执塞回来）。"""
    cid = next(_CALL_ID)
    slot = {"event": threading.Event(), "result": None}
    with _CALL_LOCK:
        _CALLS[cid] = slot
    _emit({"call": cid, "session": session, "method": method, "args": args or {}})
    if not slot["event"].wait(CALL_TIMEOUT):
        with _CALL_LOCK:
            _CALLS.pop(cid, None)
        raise TimeoutError("动作 %s 没有回执（%.0fs 超时）" % (method, CALL_TIMEOUT))
    return slot["result"]


def _load(path, index):
    """按文件路径导入用户工具文件（同一路径的文件共用同一模块名，import 只跑一次）。

    执行前把 `tool` / `Tool` / `session` 注进这个模块的 globals → 工具文件不写
    `from pie import tool` 也能用 `@tool()`；写了的走真绑定，是同一个类。
    """
    name = "_pie_user_tools_%d" % index
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    module.__dict__["tool"] = tool
    module.__dict__["session"] = SESSION
    module.__dict__["Tool"] = Tool
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


_RESULT_KEYS = {"body", "header", "state"}


def _normalize_result(out):
    """工具返回值 → `(body, header, state)`。

    两种形状：

    - `str`：**原样**当工具输出（作者自己排 `header! { … }` + 空行 + 正文）；
    - `dict`：`{"body": str, "header": {…}, "state": {…}}` —— 三个键都可选，
      头区由 Rust 侧渲染（与手写的那种文本逐字相同），`state` 是这次调用结束时的最终值。
      不认识的键直接报错（`{"bdy": …}` 静默忽略只会让人查半天）。
    """
    if isinstance(out, str):
        return out, None, None
    if not isinstance(out, dict):
        raise TypeError(
            "工具必须返回 str 或 dict（{'body','header','state'}），拿到 %s" % type(out).__name__
        )
    unknown = set(out) - _RESULT_KEYS
    if unknown:
        raise TypeError(
            "返回 dict 里有不认识的键：%s（只认 body / header / state）" % sorted(unknown)
        )
    body = out.get("body", "")
    if not isinstance(body, str):
        raise TypeError("body 必须是 str，拿到 %s" % type(body).__name__)
    header = out.get("header")
    if header is not None and not isinstance(header, dict):
        raise TypeError("header 必须是 dict，拿到 %s" % type(header).__name__)
    state = out.get("state")
    if state is not None and not isinstance(state, dict):
        raise TypeError("state 必须是 dict，拿到 %s" % type(state).__name__)
    return body, header, state


def _run(tool_obj, rid, kwargs, lock, session, state):
    """一次工具调用（跑在线程池里；async handler 丢给后台 loop 跑完再交回来）。

    `state` 是**本会话 + 本工具**的那份 dict（上次调用结束时交回去的）：声明了
    `S: SessionProxy` 的工具拿到它、改它，**这次调用返回时整份交回**（Rust 侧写进
    `meta["state"][<工具名>]`）。没声明 `S` 的老工具一点都感觉不到。
    """
    proxy = _Proxy({"session_id": session}, state) if tool_obj.wants_s else None
    try:
        out = (
            tool_obj.handler(proxy, **kwargs)
            if proxy is not None
            else tool_obj.handler(**kwargs)
        )
        if inspect.isawaitable(out):  # async handler（或者同步 handler 返回了协程）
            out = asyncio.run_coroutine_threadsafe(out, _LOOP).result()
    except BaseException as e:  # 含 sys.exit()：工具怎么炸都不该带走宿主
        _emit({"id": rid, "error": "%s: %s" % (type(e).__name__, e)}, lock)
        return
    try:
        body, header, state_out = _normalize_result(out)
    except TypeError as e:  # 返回形状不对：当成工具错误交给模型，别带崩宿主
        _emit({"id": rid, "error": "工具 %s: %s" % (tool_obj.name, e)}, lock)
        return
    reply = {"id": rid, "text": body}
    if header is not None:
        reply["header"] = header
    # `state` 的优先级：**返回值里给的赢**（那是作者显式声明的最终值）；没给就看 `S.state`
    if state_out is not None:
        reply["state"] = state_out
    elif proxy is not None:
        reply["state"] = proxy.state
    _emit(reply, lock)


def _run_event(event, ctx, rid, lock):
    """处理一个事件：跑订了这一类的插件函数；有一个返回非空字符串就是**否决理由**（取第一个）。

    `rid` 不是 None = 对方在等回话（可否决的 kind），那就必须回一行；否则只跑不答。
    """
    reason = None
    kind = event.get("type") if isinstance(event, dict) else None
    state = (ctx or {}).get("state") or {}
    for fn in SESSION.handlers.get(kind, []):
        try:
            # ⚠ 签名是 `fn(S, event)`：`S` 永远在第一位（与工具一致）；事件里 `S.state` 只读。
            out = fn(_Proxy(ctx, state, readonly=True), event)
        except BaseException as e:  # 插件自己出错：告警 + 当通过（fail-open）
            print("插件处理 %s 时抛异常: %s: %s" % (kind, type(e).__name__, e), file=sys.stderr)
            continue
        if isinstance(out, str) and out and reason is None:
            reason = out
    if rid is not None:
        _emit({"id": rid, "deny": reason}, lock)


def _main():
    found = {}
    warnings = []
    for index, path in enumerate(sys.argv[1:]):
        directory = os.path.dirname(os.path.abspath(path))
        if directory not in sys.path:
            sys.path.insert(0, directory)  # 工具文件同目录的兄弟模块能 import
        try:
            module = _load(path, index)
        except BaseException as e:
            warnings.append("%s 导入失败: %s: %s" % (path, type(e).__name__, e))
            continue
        for value in list(vars(module).values()):
            if not isinstance(value, Tool):
                continue
            if value.name in found:
                warnings.append("工具重名，跳过: %s（%s）" % (value.name, path))
                continue
            found[value.name] = value
    _emit(
        {
            "tools": [
                {"name": t.name, "description": t.description, "parameters": t.parameters}
                for t in found.values()
            ],
            "extensions": {"subscribe": sorted(SESSION.handlers)},
            "warnings": warnings,
        }
    )

    pool = concurrent.futures.ThreadPoolExecutor(max_workers=_MAX_WORKERS)
    lock = _EMIT_LOCK  # 全局一把：`_call` 发的动作帧也别与别的行交错
    while True:
        # 用 `readline()` 而不是 `for line in sys.stdin`：后者带读前缓冲，一半请求可能被捂在缓冲区里
        line = sys.stdin.readline()
        if not line:  # stdin 关了 = pie 不要我们了（连卡死的 handler 一起带走）
            _PROTOCOL.flush()
            os._exit(0)
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
            if "call_reply" in request:  # 动作回执 → 交还给等着的 worker
                with _CALL_LOCK:
                    slot = _CALLS.pop(request["call_reply"], None)
                if slot is not None:
                    slot["result"] = request.get("result")
                    slot["event"].set()
                continue
            if "event" in request:  # 事件（工具与扩展共用一条通道、共用一套 id）
                pool.submit(
                    _run_event,
                    request["event"],
                    request.get("ctx"),
                    request.get("id"),  # 没有 id = 不用回话（观察类）
                    lock,
                )
                continue
            rid = request["id"]
            tool_obj = found[request["tool"]]
            kwargs = request.get("arguments") or {}
            if not isinstance(kwargs, dict):
                raise TypeError("arguments 必须是 JSON 对象")
        except BaseException as e:
            # 连 id 都认不出来时只能给 id=null：Rust 侧记一条告警（不会错配给别的调用）
            _emit(
                {
                    "id": None,
                    "error": "宿主收到非法请求: %s: %s（%s）" % (type(e).__name__, e, line[:200]),
                },
                lock,
            )
            continue
        pool.submit(
            _run,
            tool_obj,
            rid,
            kwargs,
            lock,
            request.get("session"),
            request.get("state"),
        )


_main()
