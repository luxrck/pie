# pie 的 Python 工具宿主：由二进制以 `python -X utf8 -u -c "<本文件正文>" <工具文件…>` 启动
# （`rust` 侧见 `pytool.rs`）。
#
# 协议：一行一个 JSON，一问一答；**应答可乱序**（调用并发跑在线程池里），靠 `id` 配对。
#   stdout 第一行  ← {"tools":[{name,description,parameters}…], "warnings":[…]}
#   stdin  每行    → {"id":N,"tool":"名字","arguments":{…}}
#   stdout 之后    ← {"id":N,"text":"…"} 或 {"id":N,"error":"…"}
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
import json
import os
import sys
import threading

# 协议输出：dup 一份**原始** stdout（后面就把 sys.stdout 换成 stderr）
_PROTOCOL = os.fdopen(os.dup(1), "w", encoding="utf-8", buffering=1)
sys.stdout = sys.stderr

# 真绑定（见文件头）：装不上就整个宿主起不来 —— 把原因写清楚（会进 pie 的告警）。
try:
    from pie import Tool, tool
except ImportError:
    print(
        "pytool 需要 Python 绑定 `pie`（工具里的 @tool 就是它提供的）：请把它装进 %s"
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
threading.Thread(target=_LOOP.run_forever, daemon=True, name="pie-pytool-loop").start()


def _emit(obj, lock=None):
    """往协议通道写一行；worker 线程里要拿锁（一次调用一条，别交错成半行）。"""
    line = json.dumps(obj, ensure_ascii=False) + "\n"
    if lock is None:
        _PROTOCOL.write(line)
    else:
        with lock:
            _PROTOCOL.write(line)


def _load(path, index):
    """按文件路径导入用户工具文件（同一路径的文件共用同一模块名，import 只跑一次）。

    执行前把 `tool` / `Tool` 注进这个模块的 globals → 工具文件不写 `from pie import tool`
    也能用 `@tool()`；写了的走真绑定，是同一个类。
    """
    name = "_pie_user_tools_%d" % index
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    module.__dict__["tool"] = tool
    module.__dict__["Tool"] = Tool
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def _run(tool_obj, rid, kwargs, lock):
    """一次工具调用（跑在线程池里；async handler 丢给后台 loop 跑完再交回来）。"""
    try:
        out = tool_obj.handler(**kwargs)
        if inspect.isawaitable(out):  # async handler（或者同步 handler 返回了协程）
            out = asyncio.run_coroutine_threadsafe(out, _LOOP).result()
    except BaseException as e:  # 含 sys.exit()：工具怎么炸都不该带走宿主
        _emit({"id": rid, "error": "%s: %s" % (type(e).__name__, e)}, lock)
        return
    if not isinstance(out, str):
        _emit(
            {
                "id": rid,
                "error": "工具 %s 必须返回 str，拿到 %s" % (tool_obj.name, type(out).__name__),
            },
            lock,
        )
        return
    _emit({"id": rid, "text": out}, lock)


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
            "warnings": warnings,
        }
    )

    pool = concurrent.futures.ThreadPoolExecutor(max_workers=_MAX_WORKERS)
    lock = threading.Lock()
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
        pool.submit(_run, tool_obj, rid, kwargs, lock)


_main()
