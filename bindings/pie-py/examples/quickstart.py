"""pie Python 绑定的示例：一次性问答 → 会话 + 事件流 → 自定义工具 → 取消 → asyncio → 历史。

先装好绑定（见仓内 ``docs/python-bindings.md`` §6）：

    cd bindings/pie-py
    VIRTUAL_ENV=$PWD/.venv uv pip install maturin pytest
    VIRTUAL_ENV=$PWD/.venv .venv/bin/maturin develop     # 编 Rust 扩展（改 Rust 后要重跑）
    .venv/bin/python examples/quickstart.py

模型 / 地址 / key 走 ``~/.pie/config.toml``（也可用 ``OPENAI_BASE_URL`` / ``OPENAI_API_KEY``
覆盖，与 CLI 同口径）。⚠ 配置缺省时 ``Config()`` 的 ``api_key`` 是本部署写死的真实 key，
下面第 1 段就真的会出网——不想联网就把 base_url 指向本地假端点（见 ``tests/test_bindings.py``）。

本脚本与 ``pie`` CLI 共用同一份 ``~/.pie/``：会话 JSONL / 压缩记录 / 图片副本两边都能互读。
"""

from __future__ import annotations

import asyncio
import threading
import time

import pie


# ---------------------------------------------------------------- 1. 一次性问答（无会话）

def one_shot() -> None:
    """``pie.run`` = 建临时会话跑一回合：无会话、不落盘，只把最终答复给你。"""
    answer = pie.run("用一句话解释什么是 agent harness")
    print("[one_shot]", answer)


# ---------------------------------------------------------------- 2. 会话 + 事件流

def chat_with_events() -> None:
    cfg = pie.Config.load()          # 读 ~/.pie/config.toml（文件不在 → 默认值）
    cfg.model = "deepseek-flash"     # 只改内存、不写盘；要留就 cfg.save()
    cfg.reasoning_effort = "high"

    llm = pie.LlmClient(cfg)
    tools = pie.ToolRegistry.builtins(cfg)   # read / edit / writ / bash

    # new() 落盘；.ephemeral() 临时（服务 / notebook 用）；.load(path, …) / .resume(…) 续前话
    session = pie.Session.new(cfg, llm, tools)

    def on_event(ev: pie.TurnEvent) -> None:
        # ⚠ 回调里别碰同一个 session，否则抛 RuntimeError("session 正忙")；别的对象随便用。
        kind = ev["type"]
        if kind == "content_delta":
            print(ev["text"], end="", flush=True)
        elif kind == "reasoning_delta":
            print(f"\033[2m{ev['text']}\033[0m", end="", flush=True)
        elif kind == "tool_call":
            print(f"\n→ {ev['name']}({ev['arguments']})")   # arguments 已解析成 dict
        elif kind == "tool_result":
            print(f"← {ev['text'][:200]}…")                 # ⚠ text 不截断，要少显示自己切

    answer = session.aturn("看看当前目录，挑个文件读 10 行再总结", on_event=on_event)
    print("\n[答复]", answer)
    print("[用量]\n" + session.usage_report())
    session.save()                    # 落到 ~/.pie/sessions/chat-<时间戳>.jsonl


# ---------------------------------------------------------------- 3. 自定义 Python 工具

def with_python_tool() -> None:
    cfg = pie.Config.load()
    llm = pie.LlmClient(cfg)
    tools = pie.ToolRegistry.builtins(cfg)

    # 装饰器：类型注解自动导出 JSON schema（str/int/float/bool/list/dict/Optional）
    @pie.tool(name="word_count", description="统计一段文本的字符数")
    def word_count(text: str) -> str:
        """text: 要统计的文本"""
        return f"{len(text)} 个字符"

    tools.register(word_count)

    # 也可以直接给零件（适合按配置 / 注解动态生成工具）；handler 必须同步、返回 str，
    # 抛异常会被文本化成 "[工具错误] …" 回给模型，不打断回合。
    tools.register(
        name="now",
        description="返回当前 Unix 时间戳",
        parameters={"type": "object", "properties": {}},
        handler=lambda: str(int(time.time())),
    )

    session = pie.Session.ephemeral(cfg, llm, tools)   # 临时会话：不落盘
    print("[tool]", session.aturn("用 word_count 数一下 '你好世界' 有几个字符"))


# ---------------------------------------------------------------- 4. 从别的线程取消

def cancellable() -> None:
    cfg = pie.Config.load()
    llm = pie.LlmClient(cfg)
    tools = pie.ToolRegistry.builtins(cfg)
    session = pie.Session.ephemeral(cfg, llm, tools)

    token = pie.Cancel()
    result: list[str] = []

    def run() -> None:
        # aturn 期间释放 GIL → 这个线程在等模型，主线程照样能动
        result.append(session.aturn("写一篇一万字的长文", cancel=token))

    thread = threading.Thread(target=run)
    thread.start()
    time.sleep(2.0)
    token.cancel()                    # 等价：session.stop()（从别处停住正在跑的回合）
    thread.join()
    print("[cancelled]", result[0])   # 取消时返回的是 CANCEL_TEXT，不是半截答复


# ---------------------------------------------------------------- 5. asyncio 入口（M5）

async def async_entry() -> None:
    cfg = pie.Config.load()
    llm = pie.LlmClient(cfg)
    tools = pie.ToolRegistry.builtins(cfg)
    session = pie.Session.ephemeral(cfg, llm, tools)

    # 事件走 asyncio.Queue：先 create_task，再 async for（队列在 aturn 里就建好了）
    task = asyncio.create_task(session.aturn("列一下当前目录"))
    async for ev in session.events():
        print("[async]", ev["type"])
    print("[async 答复]", await task)

    # task.cancel() 也真能停住（Python 侧 glue 捕 CancelledError 后桥到 session.stop()）：
    # task = asyncio.create_task(session.aturn("写一篇长文"))
    # await asyncio.sleep(1.0)
    # task.cancel()


# ---------------------------------------------------------------- 6. 历史会话 / 恢复

def history_and_resume() -> None:
    for row in pie.list_sessions(limit=5):   # 键名同 CLI `pie sessions --json`
        print(f"{row['id']}  {row['turns']} 轮  {row['first_query'][:40]}")

    cfg = pie.Config.load()
    llm = pie.LlmClient(cfg)
    tools = pie.ToolRegistry.builtins(cfg)

    # 续最近一次会话（等价 CLI 的 `pie -r`）；指定文件用 Session.load(path, …)
    session = pie.Session.resume(cfg, llm, tools)
    print("[resume]", session.aturn("接着上次说"))
    session.save()

    # 手动触发一次整窗口归档（自动压缩只做工具级 / 轮次级）
    # session.compact("turns")     # 模式："auto" | "tools" | "turns"
    # session.clear_window()       # 把当前窗口归档成窗口块后开新窗口


def main() -> None:
    one_shot()
    chat_with_events()
    with_python_tool()
    cancellable()
    asyncio.run(async_entry())
    history_and_resume()


if __name__ == "__main__":
    main()
