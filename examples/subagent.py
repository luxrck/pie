"""`subagent` —— 带**自己记忆**的子代理（演示「子会话与主会话平级」）。

放进 `<数据根>/plugins/subagent.py` 即生效。

它做的事：

1. 每次调用拿父会话 id（`S.session_id`）→ 子会话 id 固定成 `<父 id>-<实例名>`，
   于是同一个父会话里的多次调用**接着同一份子历史**聊；
2. 子会话有自己的 system prompt、自己的工具白名单（**只有内置工具** —— 不含插件工具，
   所以不会套娃）、自己的压缩与用量；
3. 跑完 `save()`；下次 `pie -s <子 id>` 能把它打开看（打开即**接管**，从此是普通会话）。

⚠ 成本（都是有意接受的）：
- 子会话的 token **不**累加进父的 `/stat`（父的上下文里只多一段工具输出）；
- `Esc` 取消是**硬杀** pyplugin 宿主 → 子会话这一轮可能没 save（上一轮的记忆还在）；
- 同一时刻两次并发调用同一个子会话会撞 `RuntimeError("session 正忙")`：要么串行，要么
  用不同的 `_instance`（它们各自一份历史）。
"""

from __future__ import annotations

from pathlib import Path

import pie
from pie import SessionProxy, tool

DEFAULT_INSTANCE = "default"

SYSTEM = """你是主 agent 派出来的子代理，只负责把交给你的那一件事做完。

- 你有自己的上下文，看不到父对话；需要的信息要么在任务里，要么自己用工具查。
- 少说废话：最后一段就是你的交付物，主 agent 会原样读它。
- 别改与本任务无关的文件。"""


def _child_session(S: SessionProxy, instance: str) -> pie.Session:
    """子会话：存在就 load（记忆延续），否则新建并登记 `parent`。"""
    # 配置从磁盘来（父的运行时覆盖 —— `/model`、`/thinking` —— 带不过来；要一致就写进 config.toml）
    config = pie.Config.load()
    config.append_system_prompt = [SYSTEM]
    llm = pie.LlmClient(config)
    # **白名单**：只有内置工具。插件工具（含 subagent 自己）都不给 → 结构上不可能递归。
    tools = pie.Registry.builtins(config)

    child_id = f"{S.session_id}-{instance}"
    path = pie.session_dir(child_id)
    if Path(path, "messages.jsonl").exists():
        return pie.Session.load(path, config, llm, tools)
    return pie.Session.new(config, llm, tools, id=child_id, parent=S.session_id)


@tool(
    description="派一个带自己记忆的子代理去干活：它有独立上下文（不占你的），多次调用接着聊"
)
def subagent(
    S: SessionProxy,
    task: str,
    _instance: str = DEFAULT_INSTANCE,
) -> str:
    """`task` 要自包含（子代理看不到这段对话）；`_instance` 换一条独立记忆（默认 `default`）。"""
    child = _child_session(S, str(_instance))
    try:
        answer = child.turn(task)
    finally:
        # 跑挂了也存：已经发生的工具调用 / 中间结论不该丢
        child.save()
    return f"[子会话 {child.path}]\n{answer}"
