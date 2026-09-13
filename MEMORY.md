# MEMORY.md — 持久记忆

本文件是项目的持久记忆：记录用户偏好、关键决策和踩过的坑。agent 在每次会话开始时读取，并可在运行中用 edit/write 更新。

保持简洁：只记跨会话仍然有效的事实，不要记临时状态。

## 用户偏好

- 信奉 YOLO：不要权限确认，工具直接执行。
- 偏好极简、可扩展的实现。
- 不要自动更新版本号（pyproject/uv.lock 等），除非用户明确要求；提交时不要自拟版本号或改动版本号。
- `max_seq_len = 128_0000`（128 万）是**刻意设定**，不是手误——已确认过，不要再当成笔误反复确认。
- `Config.max_tokens` 默认 **256000**（256k，按 k=1000 换算），不要改回 None/不发送。

## 打包 / 还原

打包
```bash
find . -type f \( -name '*.py' -o -name '*.toml' -o -name '*.md' \) \
   -not -path './.git/*' \
   -not -path './.venv/*' \
   -not -path '*/__pycache__/*' \
   -not -path './dist/*' \
   -print0 | sort -z | while IFS= read -r -d '' f; do
     printf '===== %s =====\n' "$f"
     cat "$f"
     printf '\n'
   done > pie-export.txt
```

还原
```bash
#!/usr/bin/env bash
# restore.sh — 从 pie-export.txt 还原项目文件
# 用法: bash restore.sh [导出文件，默认 pie-export.txt]
# 格式: 每个文件段以 "===== <相对路径> =====" 开始，以 "===== END <路径> =====" 结束（兼容无 END 的旧格式）
set -u

SRC="${1:-pie-export.txt}"
if [ ! -f "$SRC" ]; then
  echo "找不到导出文件: $SRC" >&2
  exit 1
fi

out=""
while IFS= read -r line || [ -n "$line" ]; do
  if [[ "$line" == "===== "* ]]; then
    if [[ "$line" == "===== END "* ]]; then
      out=""                      # END 标记：当前文件结束
    else
      path="${line#===== }"       # 去掉前缀 "===== "
      path="${path% =====}"       # 去掉后缀 " ====="
      if [ -n "$path" ]; then
        mkdir -p "$(dirname -- "$path")"
        : > "$path"               # 创建 / 清空目标文件
        out="$path"
      fi
    fi
    continue
  fi
  if [ -n "$out" ]; then
    printf '%s\n' "$line" >> "$out"
  fi
done < "$SRC"

echo "还原完成: $SRC"
```

## 关键决策

- 2026-08-28：harness 采用 OpenAI 兼容接口，默认模型 deepseek-v4-flash（reasoning_effort=high），默认 API https://api.deepseek.com/，配置只从 ~/.pie/config.toml 读取。
- 2026-08-28：包名为 pie，入口 python -m pie；内置工具仅 read / edit / write / shell，扩展用 @tool() + ToolRegistry。
- 2026-08-28：配置持久化到 ~/.pie/config.toml（旧 config.json 自动迁移），记忆文件为 MEMORY.md 与 ~/.pie/memory.md。
- 2026-08-28：CLI 为 pie（新对话）/ pie resume（恢复最近会话）/ pie [PROMPT]（一次性子 agent，不写 sessions）；Session 支持多轮对话与 JSONL 会话持久化；支持全局安装（uv tool install . --editable），任意目录可运行。
- 2026-08-29：上下文压缩文件用内容 sha256 前 16 位命名（不做 turn-range）；摘要子 agent 实现为 shell 调 pie 一次性模式；CLI 新增 -c/--config。
- 2026-08-29：三级上下文压缩（工具级 eager spill / 轮次级 / 会话级），token 计数用 API usage.prompt_tokens；单条消息压缩级别只升不降（0→1→2→3）。
- 2026-08-29：read 返回全文不截断、支持 offset（1 起）/ limit 分页；edit 为 edits 数组（oldText 唯一、互不重叠、按原文非增量应用），对齐 pi-agent。
- 2026-08-29：shell 工具不内部截断，全文交回 harness 统一做 1 级压缩；spill 按工具区分：read 默认不落盘（>100K），shell 等按 8K。
- 2026-08-29：压缩事件写会话 manifest（~/.pie/context/<session>-manifest.jsonl），maybe_compact 返回节省 token 统计，Session 提供 compression_history / verify_context / raw_history；`pie context info/verify/gc` 维护命令。
- 2026-08-29：摘要为规则式：工具=head+tail，轮次=user+最终输出，会话级=指针+保护区域 verbatim。
- 2026-08-31：重构——摘要只保留规则式；移除子 agent 摘要器（_make_subagent_summarizer）与 LLM 摘要模式（parse_summary_output / remember_facts / 滚式 / compress_summarizer / subagent_timeout / remember_facts / subagent_config_file / ensure_subagent_config）。
- 2026-08-29：/usage 显示当前上下文占用（估算+百分比）、压缩次数与落盘原文量、API 上报与累计；UsageTracker 经会话文件 __meta__ 跨 resume 恢复。
- 2026-08-31：/usage 改名 /stat；usage_report 新增“会话文件：<path>”行（仅当会话文件已保存存在时显示），TUI 状态栏过滤该行保持首/尾摘要。
- 2026-08-31：/stat 改名 /status（命令字符串、帮助文本、补全候选、README 同步更新）。
- 2026-08-29：DeepSeek thinking 400 真根因 = 会话级压缩在轮次进行中 pair 提取，产生 user→纯文本 assistant→tool_calls→tool 非法序列；修复：进行中轮次不 pair 提取（turn_in_progress）、pair 仅当轮次以 assistant 结尾时提取、Session.load 自动修复已损坏序列。
- 2026-08-29：tool_call 参数膨胀（write 大 content + thinking reasoning 大）是上下文主要消耗源，决策：留给自动压缩处理，不单独改工具。
- 2026-08-29：新增 keep_last_steps（当前轮次内必须完整保留的最近 step 批次数，默认 3）与 compress_current_turn（默认 true）：进行中的轮次超限时压缩较早 step 批次（整批落盘 + manifest kind=step），解决长工具循环单轮撑爆上下文的问题。
- 2026-08-31：语义收敛——只保留 keep_last_steps（默认 5）决定保护窗口（最近 N 个 step 批次所在轮次，跨轮次滚动，当前轮恒在窗口内）；移除 keep_last_turns 与 compress_current_turn；纯文本会话（无 step 批次）保护全部、不压缩。
- 2026-08-31：三级压缩开关合并为 compaction（bool）：true 时工具/轮次/会话三级全部启用；旧键 compress_tools/compress_turns/compress_session 自动迁移（取三者 AND，新键优先）。
- 2026-08-31：compaction 改为嵌套结构 [compaction] enabled + [compaction.tool] head/tail（工具级压缩按行保留 head+tail，行数不足则不压）；旧扁平 compaction=true 与更早三键自动迁移。
- 2026-08-31：修复轮次级/会话级被饿死——保护窗口收窄为“只保护当前轮”，已完成轮次不再受 keep_last_steps 窗口保护（跨轮次滚动语义取消），轮次级/会话级恢复工作；keep_last_steps 只负责当前轮内最近 N 批 verbatim。
- 2026-08-31：上下文管理重构为容器模型——AgentMessage（整场对话）/ ModelMessage（一轮内 assistant+tool）/ 叶子 System/User/Assistant/ToolMessage；compact(tools|turns|session) 为容器方法，支持切片与拼接；tokens() 用 provider 基线 + 压缩比例估算；compaction.session 默认 false（/clear 切换窗口）；fs 为历史窗口块列表（~/.pie/windows/，GC 不碰）；会话文件新格式，不兼容旧文件。
- 2026-08-31：会话 meta 不再记录 manifest 路径——消息自带 raw_path 自描述，manifest 降级为按文件名推导的可选审计日志；verify_context 以消息字段 + fs 为准。
- 2026-08-31：新增 Textual TUI（src/pie/tui.py，pi/tau 风格）——真实终端下 chat/resume 走图形界面，工具日志经 complete_turn 的 on_event 回调实时展示；非 TTY 回退 readline。
- 2026-08-31：移除 compress_max_turns_per_event（LLM 摘要时代遗留的每事件封顶）；规则式下轮次级一次压到目标水位或无可压轮次，压不完再升级会话级。
- 2026-08-31：移除 spill_threshold_chars / read_spill_threshold_chars 与 harness 级工具级压缩——shell 新增 limit 参数（默认 200 行，超限全文落盘 + 指针 + 最后 limit 行），read 用 offset/limit 分页；shell 落盘指针在 loop 里写入 manifest（kind=tool）。
- 2026-08-31：移除 use_memory 配置——SYSTEM.md / AGENTS.md / MEMORY.md 存在即加载，不再有跳过开关。
- 2026-08-31：修复 /save 自定义路径的 manifest 关联——__meta__ 记录 manifest 路径，load 优先使用；新增 Session.full_history() 按 manifest 展开压缩内容重建完整转录（压缩视图 vs 完整历史的差异是设计，原始数据始终在 step/turn/session-*.txt）。
- 2026-08-31：TUI 新增 shell 模式——输入以 `!` 开头时输入框边框变 tool_call 橙色（#9c4916，CSS 类 shell-mode 切换），提交后 `!` 后内容直接 subprocess 执行（shell=True，120s 超时，超 200 行截断显示前 100 后 50），结果输出到 log 但不经过 LLM、不进会话上下文（不写 messages、不 save）。
- 2026-09-01：清理第一类死代码——删除 cli.py 的 chat_main/resume_main/once_main 兼容入口（main() 已用 argv[0] 分发 + _run 替代）与 --new-session 死参数；删除 Session.to_messages / maybe_compact / verify_context / raw_history（无调用方，被 messages.to_api() / context.maybe_compact / referenced_raw_paths() / full_history() 取代）；删除 tui.py 的 BORDER_* 兼容别名（_box 直接用 ROLE_BORDERS）；README 同步去掉 verify_context/raw_history 引用。
- 2026-09-01：新增手动取消（TUI `/stop`）——发出消息后输入框不再 disabled（等待期间可继续输入）；loop 层 `complete_turn` / `Session.turn` 新增 `cancel_event`（threading.Event）参数，`_run_cancellable` 在子线程跑阻塞调用并 poll 取消标志（50ms 粒度）；模型请求被取消 → 回合终止、历史写 AssistantMessage("用户手动终止")；工具执行被取消 → 该工具（及未执行工具）结果填充 ToolMessage("用户手动终止")，再写 AssistantMessage("用户手动终止")，保证 tool_call_id 一一对应（API 序列合法）；TUI shell 模式（!）改 Popen 轮询，/stop kill 子进程；处理中提交普通消息提示“正在处理中，输入 /stop 可取消”，取消收尾时 join 等待避免新旧回合并发写历史。
- 2026-09-01：TUI 输入框右侧按钮（`#send-btn`）两用：空闲“▶ 发送”点击提交输入框内容（等价回车），处理中变“■ 停止”点击等价 `/stop`；实现 = `#input-bar` Horizontal 布局（输入框 `width: 1fr` + 按钮 `width: 8; height: 100%` 等高），按钮 `min-width: 0; padding: 0 1` 收窄，`can_focus=False` 不抢焦点；状态在 `_submit/_run_shell`（进入 busy）与 `_finish_turn/_fail_turn/_show_shell_result`（退出）切换。
- 2026-09-01（修订）：按钮从右下角悬浮改为输入框右侧等高——用户嫌悬浮覆盖丑；`layers/overlay/dock/offset-y` 全部移除，`_place_send_button` 删除；Textual `pilot.click` 的 offset 是像素偏移（非百分比），headless 测试需等布局稳定或显式传中心 offset。
- 2026-09-01（再修订）：用户最终决定**移除 send-btn**——输入框恢复为直接 yield（去掉 Horizontal 容器），`_update_send_button` / `on_button_pressed` / CSS 规则全部删除，功能回退到纯 `/stop` 文本命令 + 回车提交。
- 2026-09-01（最终）：用户决定**恢复 send-btn**（加回来）——恢复为输入框右侧按钮：`Horizontal(id="input-bar")` 容器 + `Button("▶")`，label 纯符号（空闲 `▶` / busy `■`），CSS 用 `min-width: 0; padding: 1; text-align: center`（用户此前手动改窄过），`can_focus=False`。
- 2026-09-01（再修订）：send-btn 样式改为**透明背景、只以 border 为边界**——Textual 默认 background 覆盖整个 widget 区域（含 border 之下），border 字符其实是画在背景色块上的前景描边，视觉上“border 在 background 内部”；改 `background: transparent` 后 border 即按钮唯一边界，内部露出下层背景；hover/busy 改用边框颜色区分：空闲 border `#45475a` / hover `#89b4fa`，busy border `#f38ba8` / busy:hover `#ffb4c8`（文字颜色同步）。
- 2026-09-01（再再修订）：**busy 时恢复暗红背景**（用户明确要求，允许 busy 状态下 border 在背景内部）——空闲仍透明背景+border 边界；busy 未 hover 背景 `#3a1d1d` + 边框 `#f38ba8`，busy:hover 背景 `#5a2d2d` + 边框 `#ffb4c8`，文字 `#f38ba8`。
- 2026-09-01（再再再修订）：**取消 busy 暗红背景，暗红挪到 border**——所有状态背景统一透明（border 即边界）；busy 边框 `#3a1d1d`（暗红，即原背景色）/ busy:hover 边框 `#5a2d2d`（提亮暗红），文字 `#f38ba8`；注意 `#3a1d1d` 与屏幕背景 `#1a1a24` 对比度低，边框偏暗（如需可见可调亮）。
- 2026-09-01（再再再再修订）：**busy 边框提亮为亮红**——用户反馈暗红边框不醒目；busy 边框 `#f38ba8`（error 色）/ busy:hover 边框 `#ffb4c8`（更亮），文字同步，背景仍透明。
- 2026-08-31：修正工具级/step 级语义——工具级压缩只压缩 tool 返回文本（内容落盘成指针，消息保留，绝不删除）；当前轮 step 压缩改为内容级（spill_turn_tool_results），整批删除的 compress_step_batches 已移除；stats 字段 spilled 改名为 tools。
- 2026-08-31：压缩指针写入消息自身字段（Message.raw_path / raw_hash）——消息自描述，full_history() 按消息顺序精确重建；referenced_raw_paths() 同时扫 manifest 与会话消息字段，GC/verify 不再依赖文件名 stem 关联。
- 2026-09-02：TUI 样式整理成 theme——新增 src/pie/theme.py（Theme 纯数据 + THEMES 注册表 + get_theme），Config 新增 theme 键（默认 catppuccin-mocha）持久化到 config.toml；PieApp 实例持有 self.palette=get_theme(config.theme)，CSS 经 build_css(palette) 在 __init__ 注入实例属性 self.CSS（Textual load 阶段读实例属性，可按主题动态生成）；tui.py 不再有硬编码颜色常量，_box/选中高亮/补全面板均走 palette。注意：RichLog 已有只读 property selection_style，选中样式需用私有名 self._selection_style；选中的高亮样式需在 compose 里传给 SelectableRichLog(selection_style=Style(...))。
- 2026-09-02：resume 含图片消息的会话时 TUI 崩溃（on_mount → full_history）——message_raw_path() 对多模态 content（list，如 ImageMessage）直接 re.search 抛 TypeError；修复为统一先经 content_text() 归一（图片 part 不参与指针匹配）再扫压缩指针。教训：扫描消息 content 的代码必须兼容 str / list 两种形态。

## 项目演进总结（优化改进一览）

- 架构：单文件 → 分层包（tools / llm / loop / config / chat / context / cli），src 布局，可全局安装（uv tool install --editable）；@tool() 按签名自动生成 schema，LLM 协议可注入，扩展点明确。
- CLI 与交互：pie（新对话）/ pie resume（恢复最近）/ pie [PROMPT]（一次性子 agent，不写 sessions）/ pie context info|verify|gc；-c/--config 指定配置文件；prompt_toolkit Unicode 安全输入 + 历史；任意目录运行；无 max_turns（长任务不设步数上限）。
- 工具：read 全文不截断 + offset/limit 分页；edit edits 数组（oldText 唯一、防重叠、按原文非增量）；shell 全文返回不丢数据；spill 按工具区分（read 是工作集默认不落盘）。
- 上下文压缩：轮次级 user+最终输出 / 当前轮 step 批次 / 会话级 指针+保护区域 verbatim（工具级落盘由 shell limit 与 read 分页承担）；保护区域由 keep_last_steps 唯一决定（最近 N 个 step 批次所在轮次），摘要全部规则式零模型调用；内容 hash 落盘、指针链、压缩级别只升不降、软阈值 80% / 目标水位 55% 迟滞；manifest 索引 + verify/gc；压缩统计（节省 token）。
- 模型层：reasoning_effort 透传；reasoning_content 捕获/持久化/原样回传（thinking 模式 400 已修）；采集 provider usage（prompt/completion）+ UsageTracker 跨 resume 累计；API 异常打印请求诊断。
- 可观测性：/usage 显示当前上下文占用（估算+百分比）、预算水位、各角色占比、压缩次数与落盘原文量、最近上报、累计用量；调试日志 [t{x}s{y}]（用户轮次 × 工具步骤）。
- 决策备忘：子 agent = shell 调 pie 一次性模式（argv 只传短指令）；tool_call 参数膨胀留给自动压缩。
- 待办：/usage 主行改用 provider 上报值（chars/4 对中文低估）；write 参数与 reasoning 纳入压缩统计口径。

## 问题与解决（踩坑记录）

### 注解与类型

- `from __future__ import annotations` 会把注解变成字符串，@tool() 拿到的是 "str" 而非 str → 用 get_type_hints(fn) 解析真实类型。
- `str | None` 的 get_origin 在 Python 3.13 返回 types.UnionType、3.14 返回 typing.Union（两者合一）→ 判断要写 `origin is Union or origin is types.UnionType`；全局 uv tool 环境的 Python 可能和项目 .venv 不同（本项目：工具 3.13 / .venv 3.14），跨版本改动要两个环境都实测。

### 输入与终端

- input() 在部分终端（WSL/mintty）退格按字节删除，删中文会截断成非法 UTF-8 → 改用 prompt_toolkit 行编辑（非 TTY 回退 input()），附带获得 ~/.pie/history.txt 输入历史。
- 命令行参数有长度上限（Linux 单参数 128KiB / macOS 256KiB / Windows 32KiB），长任务走 stdin 或文件指针，不要塞进 argv。
- 2026-09-13：**CJK 断行要装两处**。显示层有两条独立排版链路：① Rich（`#log`/盒子/Markdown）走 `rich.text.divide_line`（`install_cjk_wrap()` 替换模块属性即可）；② `#input` 的 TextArea 不走 Rich，而是 `textual.document._wrapped_document` 调 `textual._wrap.compute_wrap_offsets`——只修前者时输入框仍按「无空格的整串算一个词」换行：中文长串只要比**行尾剩余空间**宽就整段挪到下一行，上一行留大片空白（实测 width=30 时 `把 #log ` 之后只剩 8 格就换行）。两者分词都是 `\S+\s*|\s+`，所以修法一样：把全角字（`cell_len==2`）拆成单字 token。patch 的是 from-import 后那个模块的全局名，不能改 `textual._wrap` 本体。含 `\t` 的行回退原实现（tab 展开宽度依赖列位置、调用方会预计算 `precomputed_tab_sections`）。

### 打包与部署

- uv_build 默认要求 src 布局（报 Expected a Python module at src/pie/__init__.py）→ 包移到 src/pie/。
- uv 在沙箱里缓存目录只读导致 sync/run 失败 → 用 UV_CACHE_DIR 指向可写目录（环境问题，非代码）。

### 会话与文件

- 会话文件名秒级时间戳同秒碰撞会互相覆盖、resume 错乱 → 文件名加微秒 %f。
- /save 自定义路径时 manifest 按文件名 stem 查找可能错位（已知边缘问题，待修）。

### 上下文压缩

- 2026-09-03：工具级压缩误报修复——`_finalize_tool_message` 先前对所有工具结果调用 `extract_spill_path` 全局搜 `[...全文已保存: ...]`；而 `read` 读取源码时输出常含该格式字面量（如 `[工具输出全文已保存: {path}]`、`{spill}`、`{write_raw(content, 'tool')}`），被 `_SPILL_RE` 误匹配 → 伪造 raw_path/raw_hash、compress_level=1 并写假 level=1 manifest，导致离软阈值很远时也报“工具压缩”。修复：① 仅 `call.name=='shell'` 时提取（该机制本就为 shell 落盘设计，read/edit/write 落盘指针在 content 里自描述、`message_raw_path` 可恢复）；② `extract_spill_path` 加路径存在校验（真 spill 指针必指向刚落盘的文件）。
- keep_last_steps=0（旧键 keep_last_turns/keep_last_k_turns 已移除）：user_idx[m] 越界 IndexError（compress_session 必崩、maybe_compact 触发时崩），轮次级还会把进行中的当前轮压掉 → 统一 clamp max(1, keep)，0 等价 1（当前轮必须保留）。
- 会话级压缩在轮次进行中 pair 提取“最终输出”，产生 user→纯文本 assistant→assistant(tool_calls)→tool 非法序列，被 DeepSeek 400 拒绝（reasoning_content 缺失/空串都不是根因）→ 进行中轮次不 pair 提取、pair 仅当轮次以 assistant 结尾时提取、Session.load 自动修复。
- shell 工具内部 clip_output 截断后丢弃全文（信息丢失）→ 工具返回全文，落盘统一在 harness 边界做（eager spill）。
- 压缩只统计 content，reasoning_content 和 tool_call 参数不计入，超大 thinking 消息可能漏过统计（与用量低估相关，见待办）。

### API / 模型（DeepSeek thinking）

- thinking 模式要求 assistant tool_calls 消息回传时必须带 reasoning_content，缺失报 400 → LLMResult/Message 捕获并持久化，to_api 对 tool_calls 消息恒带该字段（空串兜底）。
- 空串 reasoning_content 曾被 truthy 判断省略 → 改为 is not None / 恒带字段；None（原始响应没有该字段）才省略。
- API 异常时打印请求诊断（消息数 / tool_calls 消息数 / 缺 reasoning 数），便于下次直接定位。
- **max_tokens（2026-09-13 查官方文档核实）**：`Config.max_tokens` 默认 **256000**（用户指定；上限 384K/393216），None = 不发送该参数，由服务端默认——DeepSeek：非思考 8K / 思考模式 64K / `reasoning_effort=max` 时 128K。只有 `--max-tokens auto` 能临时回到“不发送”。**不做 /maxtokens 会话命令**（用户明确不要）。注意 **max_tokens 含思考 token**，给小了会只输出思考、正文为空。验收手段：不传时用 `max_tokens=10**9` 探边界，服务端 400 会回「valid range of max_tokens is [1, 393216]」。
- `-t/--thinking` 的合法取值曾与配置层不一致：CLI 的 `THINKING_LEVELS` 有 `off`，但 `reasoning_effort` 在 API 侧只认 `none/low/high/max`（`minimal`→low、`medium/xhigh`→high 是服务端兼容），直接发 `off` 会被 400 拒（unknown variant）→ `_run` 里把 `off` 归一成 `REASONING_NONE`。

### 配置清理

- save_history 是死配置（chat 无条件保存、one-shot 强制关闭）、resolve_config 的 OPENAI_* env 覆盖是 CLI 参数时代遗留、OpenAILLM 构造器还有 env 回退 → 全部移除，配置只从 ~/.pie/config.toml 读取（保留 PIE_DIR / PIE_CONFIG_FILE 路径重定向）。

### 用量统计

- resume 后 /usage 累计显示 0（UsageTracker 不持久化）→ 会话 JSONL 首行 __meta__ 保存用量，load 时恢复。

### 已知问题 / 待办

- 当前上下文估算用 chars/4 对中文严重低估（实例：估算 1,792 vs provider 上报 30,609）→ /usage 应优先展示 provider 上报值（last_prompt_tokens 已采集，主行待改）。
- write(content=全文) 参数 + thinking reasoning 体积大是上下文主要消耗源，且不在 spill 统计口径内；决策：留给自动压缩处理。
