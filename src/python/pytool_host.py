# pie 的 Python 工具宿主：由二进制以 `python -X utf8 -u -c "<_tool.py 正文><本文件正文>" <工具文件…>`
# 启动（`rust` 侧见 `pytool.rs`）。所以本文件里**不能**出现模块级 docstring / `from __future__`
# 之类必须是「文件第一条语句」的东西——上面那段 `_tool.py` 的 `from __future__ import annotations`
# 才是整个程序的第一条语句（它也顺带作用于本段代码）。
#
# 协议：一行一个 JSON，一问一答；**应答可乱序**（调用并发跑在线程池里），靠 `id` 配对。
#   stdout 第一行  ← {"tools":[{name,description,parameters}…], "warnings":[…]}
#   stdin  每行    → {"id":N,"tool":"名字","arguments":{…}}
#   stdout 之后    ← {"id":N,"text":"…"} 或 {"id":N,"error":"…"}
#
# 上面用到的 `tool` / `Tool` 就是拼接在前面的 `_tool.py`（**与 Python 绑定同一份文件**：
# schema 生成只有一处实现）。
#
# 三条纪律：
#   - 工具文件 import 期抛异常 → 记进 `warnings` 跳过，不带走宿主；
#   - 工具 handler 抛异常 / 返回非 str → 变成 `error` 行（Rust 侧文本化回给模型，回合照跑）；
#   - handler 缺省什么都不写 stdout：**协议行走 dup 出来的原始 fd 1**，`sys.stdout` 已换成
#     stderr，用户工具里任何 `print` 都污染不了协议。

import concurrent.futures
import importlib.util
import json
import os
import sys
import threading
import types

# 协议输出：dup 一份**原始** stdout（后面就把 sys.stdout 换成 stderr）
_PROTOCOL = os.fdopen(os.dup(1), "w", encoding="utf-8", buffering=1)
sys.stdout = sys.stderr

# 用户工具文件写 `from pie import tool` —— 这里造个同名模块塞进 sys.modules，
# 于是**不需要装 Python 绑定**（`_tool.py` 是编译期嵌进来的）。
_pie = types.ModuleType("pie")
_pie.tool = tool  # noqa: F821 —— 来自上面拼接的 `_tool.py`
_pie.Tool = Tool  # noqa: F821
sys.modules["pie"] = _pie

# 并发度 = 一批 tool_calls 的条数（真并发只对 IO 型 handler 有效：GIL 会串起纯 CPU 的）。
_MAX_WORKERS = 8


def _emit(obj, lock=None):
    """往协议通道写一行；worker 线程里要拿锁（一次调用一条，别交错成半行）。"""
    line = json.dumps(obj, ensure_ascii=False) + "\n"
    if lock is None:
        _PROTOCOL.write(line)
    else:
        with lock:
            _PROTOCOL.write(line)


def _load(path, index):
    """按文件路径导入用户工具文件（同一路径的文件共用同一模块名，import 只跑一次）。"""
    name = "_pie_user_tools_%d" % index
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def _run(tool_obj, rid, kwargs, lock):
    """一次工具调用（跑在线程池里）。"""
    try:
        out = tool_obj.handler(**kwargs)
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
            if not isinstance(value, Tool):  # noqa: F821
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
