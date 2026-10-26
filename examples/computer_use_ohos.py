"""`computer_use`：**读取**设备当前界面（纯感知层，不操作、不调模型）。

它只做一件不可替代的事：把 `uitest dumpLayout` 那棵 500 来节点 / 338KB 的树，压成几十条
**人能读、能直接照着敲**的行；动作由**主模型自己**用 `bash` 跑
`hdc shell "uitest uiInput …"` 完成（每行末尾已经渲染好参数片段，复制即用）。

## 为什么不做 sub-agent（2026-10-23 定稿）

原来的版本是「工具内部自己调 GUI 模型跑循环」。实测后确认没必要，理由：

1. **主模型本来就会**：一次真实任务（删「文档」下所有目录）就是主模型自己
   `dumpLayout → 读文本 → uitest` 干成的，全程没有专用 prompt；
2. 那份 GUI prompt 是两半：**JSON 协议**（只为工具解析服务）+ **质量约束**（主模型本来就有
   通用 agent 纪律）→ 单步驱动下两半都不需要；
3. 工具不调模型后，三笔债一起消失：嵌套请求**不进会话用量**（不用再自报 tokens）、
   `Esc` 取消**不用杀宿主**（一次 dump 是毫秒级无状态操作）、以及它随时可以搬回 Rust 内置。

## 感知层真正的价值（也只剩这些）

- **token 经济**：338KB 树 → ≤60 行清单（约 1.4k token），判据全是确定性的
  （`opacity=0`、`hitTest=None`、面积 <4px、`clip` 视口外、同 bounds 三胞胎去重、只留前台窗口）；
- **状态直读**：`Slider` 的值、`TextInput` 的 text/hint、`on`/`selected`/`focused`、
  `共 N 个文件，M 个文件夹` —— 判断"生效没有"靠这些，**不靠截图**（截图通道实测会滞后几秒）；
- **坐标权威**：`uitest dumpLayout` 的 `bounds` 中心（小字截图读不准，实测偏 1000+px）。

配置（`<数据根>/config.toml`，全部可选）：

    [python]
    interpreter = "/home/cc/.pie/envs/base/bin/python"   # 这个解释器里要装着 pie

    [tools.computer_use]
    _connect = "auto"        # 先 hdc tconn；"auto" = WSL 默认网关 + _relay_port
    _max_items = 60
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import tempfile
import time
from pathlib import Path

from pie import tool

DEFAULT_MAX_ITEMS = 60
DEFAULT_LAYOUT = "/data/local/tmp/pie-cu-layout.json"
DEFAULT_TIMEOUT_SECONDS = 60
DEFAULT_RELAY_PORT = 15555

# 框架占位标签（不是真内容）
NOISE_LABELS = {
    "DialogComp",
    "PadAboutDialogComp",
    "blankAreaFocus",
    "blankFocus",
    "label",
    "blank",
    "user",
}

_BOUNDS = re.compile(r"\[(-?\d+),(-?\d+)\]\[(-?\d+),(-?\d+)\]")
_COUNT = re.compile(r"共\s*\d+\s*个|已选择\s*\d+\s*项|项\s*已选")
_SHORTCUT_ONLY = re.compile(r"^(?:(?:Ctrl|Alt|Shift|Fn)\+[A-Za-z0-9]+|Delete|Enter|Esc|F\d+)$")
_CLOCK = re.compile(r"^(?:上午|下午|AM|PM)?\s*\d{1,2}:\d{2}$|^\d{1,2}月\d{1,2}日$")

# 状态栏里这些容器名没有语义（panel/method/center…），别写进清单
_GENERIC_ALIASES = {"panel", "method", "center", "arrowbutton", "sceneboard1", "sceneboard2"}

# 上一次观察（用来算「自上次以来变了什么」）。宿主是长活进程，所以跨调用记得住。
_LAST: dict[str, dict] = {}


# ---------------------------------------------------------------- 工具本体


@tool(
    name="computer_use",
    description=(
        "**读取**设备（鸿蒙模拟器 / 真机）当前界面：取一次 `uitest dumpLayout`，压成几十条元素清单。\n"
        "每条形如 `#27 「删除」 [delete] ⌨Delete ⇢ click 1509 1440` —— 末尾 `⇢` 后面就是\n"
        "`uitest uiInput …` 的参数，**复制到 bash 里执行即可操作界面**：\n"
        "`hdc shell \"uitest uiInput click 1509 1440\"`。\n"
        "**本工具只读，不做任何操作、也不调用模型**；动（点/滑/输入/按键）一律用 `bash` + uitest。\n"
        "可选 `query=\"关键词\"` 只列匹配的元素（本地检索，跨整棵树，不受清单条数限制）；\n"
        "`screenshot=true` 时额外截一张图并返回本地路径（可 `read` 看），**仅供布局/图标/canvas 参考**。\n"
        "\n"
        "**设备约定（都是实测踩出来的）**：\n"
        "- 坐标一律取自本清单（= dumpLayout 的 bounds 中心）；**别从截图目测坐标**——3120x2080 截图经服务端\n"
        "  预处理后小字读不准，实测偏 1000+px。\n"
        "- **状态判定看清单**（计数行、`val=`、`on`/`selected`、面包屑），别靠截图：截图通道（含 screenCap）\n"
        "  会滞后几秒，`snapshot_display` 更会给旧帧（已弃用）。\n"
        "- `hdc` 对设备侧命令失败**也常返回 0** → 判失败要同时看退出码和输出里的 `[Fail]`。\n"
        "- 多窗口：清单只列**前台**窗口 + dock；后台窗口的元素用 `query` 能找到（标「后台」），要先点 dock 切前台。\n"
        "- 键盘：`uitest uiInput keyEvent Back|Home|Power|<键码>`（Enter = 2054）；输入文字前先 click 聚焦，\n"
        "  回车提交后焦点会丢，要再输入就重新点一次。\n"
        "- 屏幕可能熄屏（清单会报截屏失败/无窗口）→ `hdc shell \"power-shell wakeup\"` 唤醒。\n"
        "- 点错了没反应？重新调一次本工具看「[变化]」那行——**完全没变化 = 那一下没生效**，换元素/先滚动。\n"
        "\n"
        "**返回**：头区（前台窗口 / 位置 / 计数 / 指纹 / 与上次相比的变化）+ 元素清单（≤`_max_items` 条）。"
    ),
)
def computer_use(
    query: str = "",
    screenshot: bool = False,
    max_items: int = DEFAULT_MAX_ITEMS,
    _hdc: str = "hdc",
    _target: str = "",
    _connect: str = "auto",
    _relay_port: int = DEFAULT_RELAY_PORT,
    _layout: str = DEFAULT_LAYOUT,
    _timeout_seconds: int = DEFAULT_TIMEOUT_SECONDS,
) -> str:
    """看一次设备界面。`query` = 只列匹配的元素；`screenshot` = 额外给一张截图路径。"""
    limit = max(8, min(int(max_items), 120))
    hdc = _Hdc(
        bin=_hdc or "hdc",
        target=_target or "",
        timeout=max(1, min(int(_timeout_seconds), 600)),
    )
    layout = (_layout or DEFAULT_LAYOUT).strip()
    _connect_device(hdc, _connect, int(_relay_port))

    started = time.perf_counter()
    tree = _Tree(hdc.layout(layout))
    took = time.perf_counter() - started

    shot_path = ""
    if screenshot:
        data, size = hdc.screenshot()
        shot_path = _save_shot(data)
    else:
        size = tree.screen_size()

    # ---- 头区
    items = tree.items(limit)
    hits = tree.find(query) if query.strip() else []
    head = [
        "[%s=%d, front=%s, fp=%s, took=%.1fs]"
        % (
            "hits" if query.strip() else "items",
            len(hits) if query.strip() else len(items),
            tree.front or "-",
            tree.fingerprint()["hash"][:8],
            took,
        )
    ]
    summary = tree.summary(size)
    if summary:
        head.append(summary)
    delta = _delta(_LAST.get(_cache_key(hdc)), tree)
    if delta:
        head.append(delta)
    _LAST[_cache_key(hdc)] = tree.fingerprint()
    if shot_path:
        head.append("[截图] %s（可能滞后几秒，仅供布局/图标/canvas 参考）" % shot_path)

    # ---- 清单
    if query.strip():
        if not hits:
            body = "没有匹配 %r 的元素（换个关键词，或去掉 query 看全量清单）" % query.strip()
        else:
            body = "".join(_item_line(i, node) for i, node in enumerate(hits[:limit], 1))
    else:
        body = tree.items_text(items, limit)
    return "\n".join(head) + "\n\n" + body


# ---------------------------------------------------------------- 设备侧


class _Hdc:
    """`hdc` 的唯一出口：argv 怎么拼、超时多久、失败怎么报，都在这一处。"""

    def __init__(self, bin: str, target: str, timeout: int) -> None:
        self.bin = bin
        self.target = target
        self.timeout = timeout

    def run(self, args: list[str]) -> str:
        cmd = [self.bin] + (["-t", self.target] if self.target else []) + args
        try:
            done = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=self.timeout,
                stdin=subprocess.DEVNULL,
            )
        except FileNotFoundError as error:
            raise RuntimeError("启动 %s 失败: %s（hdc 在 PATH 里吗？）" % (self.bin, error)) from error
        except subprocess.TimeoutExpired as error:
            raise RuntimeError("`hdc %s` 超过 %ds 没返回" % (" ".join(args), self.timeout)) from error
        text = (done.stdout or "") + (done.stderr or "")
        # hdc 对设备侧命令失败也常返回 0（只打一行 `[Fail]…`）→ 两条判据缺一不可
        if done.returncode != 0 or "[Fail]" in text:
            raise RuntimeError("`hdc %s` 失败: %s" % (" ".join(args), text.strip()))
        return text

    def _pull(self, remote: str, suffix: str) -> bytes:
        fd, local = tempfile.mkstemp(prefix="pie-cu-", suffix=suffix)
        os.close(fd)
        try:
            recv = self.run(["file", "recv", remote, local])
            if "FileTransfer finish" not in recv:
                raise RuntimeError("文件没拉回来（%s）: %s" % (remote, recv.strip()))
            return Path(local).read_bytes()
        finally:
            try:
                os.unlink(local)
            except OSError:
                pass

    def layout(self, remote: str) -> object:
        """取界面元素树（唯一权威的状态来源）。"""
        self.run(["shell", "uitest dumpLayout -i -p " + remote])
        raw = self._pull(remote, ".json").decode("utf-8", "replace")
        try:
            return json.loads(raw)
        except ValueError as error:
            raise RuntimeError("dumpLayout 不是合法 JSON: %s" % error) from error

    def screenshot(self) -> tuple[bytes, tuple[int, int] | None]:
        """截一张全尺寸图（**只走 `uitest screenCap`**：`snapshot_display` 会给旧帧）。"""
        out = self.run(["shell", "uitest screenCap /data/local/tmp/c.jpeg"])
        match = re.search(r"ScreenCap saved to\s+(\S+)", out)
        if not match:
            raise RuntimeError("screenCap 没给出落盘路径: %s" % out.strip())
        data = self._pull(match.group(1), ".png")
        return data, _image_size(data)


# ---------------------------------------------------------------- 树：解析 / 索引 / 渲染


class _Tree:
    """一次 `dumpLayout` 的解析结果：节点表（含父链）+ 派生信息 + 清单渲染。"""

    def __init__(self, raw: object) -> None:
        self.nodes: list[dict] = []
        self._walk(raw, None, "")
        self.by_id: dict[str, list[dict]] = {}
        for node in self.nodes:
            if node["id"]:
                self.by_id.setdefault(node["id"], []).append(node)
        for node in self.nodes:
            node["in_view"] = self._in_view(node)
        # 窗口按 dump 顺序排（最前的那个就是前台）。桌面拆成“全屏层”与“dock 条”。
        self.windows: list[tuple[str, tuple[int, int, int, int]]] = []
        for node in self.nodes:
            bundle = node["attrs"].get("bundleName")
            if bundle and node["box"] and all(b != bundle for b, _ in self.windows):
                self.windows.append((bundle, node["box"]))
        self.front = next((b for b, _ in self.windows if b != "com.ohos.sceneboard"), "")
        self.dock = next(
            (box for bundle, box in self.windows if bundle == "com.ohos.sceneboard" and box[1] > 1500),
            None,
        )

    # -------------------------------------------------------- 解析

    def _walk(self, raw: object, parent: dict | None, win: str) -> None:
        if isinstance(raw, list):
            for item in raw:
                self._walk(item, parent, win)
            return
        if not isinstance(raw, dict):
            return
        attrs = raw.get("attributes")
        node = _node(attrs, parent) if isinstance(attrs, dict) else None
        if node is not None:
            node["win"] = str(attrs.get("bundleName") or win)
            self.nodes.append(node)
        for child in raw.get("children") or []:
            self._walk(child, node or parent, node["win"] if node else win)

    def _in_view(self, node: dict) -> bool:
        """是否真的露在屏幕上：与最近 `clip` 祖先（没有就窗口）求交。"""
        box = node["box"]
        if box[2] - box[0] < 4 or box[3] - box[1] < 4:
            return False
        viewport = None
        parent = node["parent"]
        while parent is not None:
            if parent["attrs"].get("clip") == "true" and parent["box"]:
                viewport = parent["box"]
                break
            parent = parent["parent"]
        if viewport is None:
            viewport = self._window_box(node) or (0, 0, 100_000, 100_000)
        return _intersects(box, viewport)

    def _window_box(self, node: dict) -> tuple[int, int, int, int] | None:
        parent = node
        while parent is not None:
            if parent["attrs"].get("bundleName") and parent["box"]:
                return parent["box"]
            parent = parent["parent"]
        return None

    def _main_window_box(self) -> tuple[int, int, int, int] | None:
        """前台应用窗口的范围（桌面 `com.ohos.sceneboard` 不算）。"""
        for node in self.nodes:
            bundle = node["attrs"].get("bundleName")
            if bundle and bundle != "com.ohos.sceneboard" and node["box"]:
                return node["box"]
        for node in self.nodes:
            if node["attrs"].get("bundleName") and node["box"]:
                return node["box"]
        return None

    def _in_scope(self, node: dict) -> bool:
        """前台窗口 + dock 条；后台窗口的元素只在 `query` 里露面。"""
        if not self.front:
            return True
        if node["win"] == self.front:
            return True
        return bool(self.dock) and node["win"] == "com.ohos.sceneboard" and node["box"][1] >= self.dock[1]

    # -------------------------------------------------------- 对外信息

    def screen_size(self) -> tuple[int, int] | None:
        boxes = [n["box"] for n in self.nodes if n["attrs"].get("bundleName") and n["box"]]
        if not boxes:
            return None
        return max(b[2] for b in boxes), max(b[3] for b in boxes)

    def crumbs(self) -> list[str]:
        """顶部一排文字（标题栏 + 面包屑）——只给标签，不猜哪一个是标题。"""
        window = self._main_window_box()
        ceiling = (window[1] + 200) if window else 0
        cands = [
            n
            for n in self.nodes
            if n["label"] and n["in_view"] and n["box"][1] < ceiling and self._in_scope(n)
        ]
        if not cands:
            return []
        rows: dict[int, list[dict]] = {}
        for node in cands:
            rows.setdefault(node["box"][1] // 14, []).append(node)
        row = max(rows.values(), key=len)
        row.sort(key=lambda n: n["box"][0])
        out: list[str] = []
        for node in row:
            if node["label"] not in out:
                out.append(node["label"])
        return out[:6]

    def counts(self) -> list[str]:
        out: list[str] = []
        for node in self.nodes:
            if node["label"] and node["in_view"] and _COUNT.search(node["label"]) and self._in_scope(node):
                value = node["label"].strip("| \t")
                if value not in out:
                    out.append(value)
        return out

    def summary(self, size: tuple[int, int] | None) -> str:
        bits = []
        crumbs = self.crumbs()
        if crumbs:
            bits.append("位置：" + " › ".join(crumbs))
        counts = self.counts()
        if counts:
            bits.append("计数：" + "；".join(counts[:3]))
        if self.dock:
            bits.append("dock 可点（y≥%d）" % self.dock[1])
        if size:
            bits.append("屏幕：%dx%d" % (size[0], size[1]))
        return " ｜ ".join(bits)

    def fingerprint(self) -> dict:
        """界面指纹：模型据此判断"上一步到底有没有生效"（截图不可靠，见模块头）。"""
        texts = sorted({n["label"] for n in self.nodes if n["label"] and n["in_view"] and self._in_scope(n)})
        payload = "|".join([">".join(self.crumbs()), ";".join(self.counts()), "\n".join(texts)])
        return {
            "hash": hashlib.sha256(payload.encode("utf-8")).hexdigest(),
            "crumbs": self.crumbs(),
            "counts": self.counts(),
            "texts": texts,
        }

    # -------------------------------------------------------- 清单

    def items(self, limit: int = DEFAULT_MAX_ITEMS) -> list[dict]:
        """挑出"可定位"的元素：有名字 / 可点（带 id）/ 可读状态 / 可滚容器，且真的露在屏幕上。"""
        picked: dict[tuple, dict] = {}
        for node in self.nodes:
            if not node["in_view"] or not self._in_scope(node):
                continue
            container = node["scrollable"] and node["type"] in ("List", "Grid", "Scroll")
            if not (node["usable"] or container):
                continue
            if not (
                node["label"] or _alias(node["id"]) or container or node["type"] in ("Slider", "TextInput")
            ):
                continue  # 无字、无可用 id、也不是可滚容器：模型没法引用它
            if _SHORTCUT_ONLY.match(node["label"] or "") and not node["clickable"]:
                continue  # 菜单里单独渲染的快捷键提示（`Enter`/`Ctrl+X`）：不是元素
            b = node["box"]
            key = (node["label"], (b[0] // 8, b[1] // 8, b[2] // 8, b[3] // 8))
            previous = picked.get(key)
            if previous is None or _richness(node) > _richness(previous):
                picked[key] = node

        # 行内重复描述合并（菜单项常常三胞胎：MenuItem + 文本 + 快捷键）
        merged: list[dict] = []
        for node in sorted(picked.values(), key=lambda n: (n["box"][1], n["box"][0])):
            same = None
            for other in merged:
                if (
                    node["label"]
                    and other["label"] == node["label"]
                    and abs(other["center"][1] - node["center"][1]) < 14
                ):
                    same = other
                    break
            if same is None:
                merged.append(node)
            elif _richness(node) > _richness(same):
                merged[merged.index(same)] = node

        # 滑块 / 输入框自己没有名字（text 就是值/占位）：拿同一行左侧最近的标签当名字
        for node in merged:
            node["row_label"] = ""
            numeric = (node["label"] or "").replace(".", "").replace("-", "").isdigit()
            if node["type"] in ("Slider", "TextInput", "Toggle") and (numeric or not node["label"]):
                node["row_label"] = _row_label(self, node)

        # 同一标签重复 ≥3 次且都不可点（列表的类型/日期列）只留一条，否则一页就把额度吃满
        repeats: dict[str, int] = {}
        for node in merged:
            if node["label"] and not node["clickable"]:
                repeats[node["label"]] = repeats.get(node["label"], 0) + 1
        seen: dict[str, int] = {}
        kept: list[dict] = []
        for node in merged:
            label = node["label"]
            if label and repeats.get(label, 0) >= 3 and not node["clickable"]:
                seen[label] = seen.get(label, 0) + 1
                if seen[label] > 1:
                    continue
            kept.append(node)

        # 无标签的可点容器：如果它的范围里已经有一个带标签的元素（子文本/图标行），就别再列一遍
        labelled = [n["center"] for n in kept if n["label"]]
        kept = [
            n
            for n in kept
            if n["label"]
            or (n["scrollable"] and n["type"] in ("List", "Grid", "Scroll"))
            or not any(n["box"][0] <= cx <= n["box"][2] and n["box"][1] <= cy <= n["box"][3] for cx, cy in labelled)
        ]

        window = self._main_window_box()
        max_area = ((window[2] - window[0]) * (window[3] - window[1]) * 0.6) if window else 10**9
        top_strip = (window[1] + 120) if window else -1
        for node in kept:
            node["tap"] = _tap(node, max_area, top_strip)

        # 滑块刻度（同一行 6 个以上的纯数字）：只留当前选中的那个
        numbered: dict[int, list[dict]] = {}
        for node in kept:
            if (node["label"] or "").isdigit():
                numbered.setdefault(node["box"][1] // 24, []).append(node)
        drop_ticks = set()
        for group in numbered.values():
            if len(group) >= 6:
                keep = next((n for n in group if n["attrs"].get("selected") == "true"), group[0])
                drop_ticks.update(id(n) for n in group if n is not keep)
        kept = [n for n in kept if id(n) not in drop_ticks]

        # 无标签的可点节点常常三层嵌套（dock 图标就是这样）：只留最里层那个
        unlabeled = [n for n in kept if not n["label"] and n["clickable"]]
        drop = set()
        for outer in unlabeled:
            for inner in unlabeled:
                if inner is outer:
                    continue
                ib, ob = inner["box"], outer["box"]
                if (
                    ib[0] >= ob[0] and ib[1] >= ob[1] and ib[2] <= ob[2] and ib[3] <= ob[3]
                    and (ib[2] - ib[0]) * (ib[3] - ib[1]) < (ob[2] - ob[0]) * (ob[3] - ob[1])
                    and abs(inner["center"][0] - outer["center"][0]) <= 8
                    and abs(inner["center"][1] - outer["center"][1]) <= 8
                ):
                    drop.add(id(outer))
                    break
        kept = [n for n in kept if id(n) not in drop]

        kept.sort(key=lambda n: (not n["label"], n["box"][1] // 8, n["box"][0]))
        return kept[:limit]

    def items_text(self, items: list[dict], limit: int) -> str:
        lines = [_item_line(index, node) for index, node in enumerate(items, 1)]
        if len(items) >= limit:
            lines.append("（清单已截断：只列前 %d 条 → 用 query=关键词 查没列出的）\n" % limit)
        return "".join(lines)

    def find(self, keyword: str) -> list[dict]:
        """本地检索（不调模型）：在**整棵树**里按标签 / id / 类型模糊找。"""
        want = keyword.strip().lower()
        hits = []
        for node in self.nodes:
            if not node["usable"] or not node["in_view"]:
                continue
            hay = " ".join(x for x in (node["label"], node["id"], node["type"]) if x).lower()
            if want and want in hay:
                hits.append(node)
        # 前台优先，其次可见，再次有标签
        hits.sort(key=lambda n: (not self._in_scope(n), not n["in_view"], not n["label"], n["box"][1]))
        return hits[:12]


# ---------------------------------------------------------------- 节点与渲染


def _node(attrs: dict, parent: dict | None) -> dict | None:
    box = _bounds(attrs.get("bounds"))
    if box is None:
        return None
    label = _label(attrs)
    ident = str(attrs.get("id") or "").strip()
    node_type = str(attrs.get("type") or "")
    clickable = attrs.get("clickable") == "true"
    has_state = node_type in ("Slider", "TextInput", "Toggle", "Checkbox", "MenuItem", "ListItem")
    usable = _self_visible(attrs, box) and (bool(label) or clickable or has_state)
    return {
        "attrs": attrs,
        "parent": parent,
        "box": box,
        "center": ((box[0] + box[2]) // 2, (box[1] + box[3]) // 2),
        "label": label,
        "label_is_text": bool(attrs.get("originalText")),
        "id": ident,
        "type": node_type,
        "clickable": clickable,
        "long_clickable": attrs.get("longClickable") == "true",
        "scrollable": attrs.get("scrollable") == "true",
        "usable": usable,
    }


def _bounds(text: object) -> tuple[int, int, int, int] | None:
    match = _BOUNDS.match(str(text or ""))
    return tuple(int(g) for g in match.groups()) if match else None


def _intersects(a: tuple[int, int, int, int], b: tuple[int, int, int, int]) -> bool:
    return min(a[2], b[2]) > max(a[0], b[0]) and min(a[3], b[3]) > max(a[1], b[1])


def _self_visible(attrs: dict, box: tuple[int, int, int, int]) -> bool:
    if attrs.get("visible") == "false" or attrs.get("enabled") == "false":
        return False
    if str(attrs.get("opacity") or "") in ("0.000000", "0"):
        return False  # dock 图标阴影 / 滚动条 / 隐藏菜单项：visible 仍是 true
    if attrs.get("hitTestBehavior") == "HitTestMode.None":
        return False  # 完全不接收点击
    return box[2] - box[0] >= 4 and box[3] - box[1] >= 4


def _label(attrs: dict) -> str:
    """元素名：`originalText` 优先（干净）；退化到 `text` 的第一段；再退化到 `hint`。

    ⚠ 不用 `description`：实测 13~14 个图标按钮的 description 全是「按钮」，没有区分度。
    ⚠ 时钟/日期不是元素：滤掉，否则每分变化都会污染「自上次以来变了什么」。
    """
    raw_label = str(attrs.get("originalText") or "").strip()
    if _CLOCK.match(raw_label) or raw_label.startswith("resource://"):
        return ""
    value = str(attrs.get("originalText") or "").strip()
    if value and value not in NOISE_LABELS:
        return _clip(value.replace("\u202a", "").replace("\u202c", "").strip("| "), 40)
    raw = str(attrs.get("text") or "").strip()
    if raw:
        first = raw.split(",")[0].strip()
        if first and first not in NOISE_LABELS:
            return _clip(first.replace("\u202a", "").replace("\u202c", ""), 40)
    hint = str(attrs.get("hint") or "").strip()
    if hint and hint not in NOISE_LABELS:
        return _clip(hint, 40)
    return ""


def _shortcut(attrs: dict) -> str:
    """从 `text` 尾巴里抽快捷键（形如 `删除, 删除, Delete` / `打开, 打开, Enter`）。"""
    match = re.search(
        r",\s*((?:Ctrl|Alt|Shift|Fn)\+[A-Za-z0-9]+|Delete|Enter|Esc)\s*$",
        str(attrs.get("text") or ""),
    )
    return match.group(1) if match else ""


def _state(node: dict) -> str:
    attrs = node["attrs"]
    marks = []
    if attrs.get("checked") == "true":
        marks.append("on")
    if attrs.get("selected") == "true":
        marks.append("selected")
    if attrs.get("focused") == "true":
        marks.append("focused")
    if node["type"] in ("Slider", "TextInput", "SearchField"):
        value = str(attrs.get("text") or "").strip()
        if value:
            marks.append("val=%s" % _clip(value, 24))
    return ",".join(marks)


def _richness(node: dict) -> tuple:
    """同一块区域多个节点时，挑"信息更多"的那个排序键。"""
    return (bool(node["label"]), node["label_is_text"], bool(node["id"]), -len(node["id"]))


def _alias(ident: str, limit: int = 28) -> str:
    """id 的短别名：URI 取末段（并解码），其余取最后一段下划线后的部分；纯数字 id 无别名。"""
    bundle = re.search(r"com\.[a-z0-9_.]+", ident)
    if bundle:
        tail = bundle.group(0).rsplit(".", 1)[-1]
        if tail not in ("sceneboard",):  # dock 里的系统图标要靠后半段的 showDesktop/hoskey 区分
            return tail
    text = ident.split("/")[-1] or ident
    if "%" in text:
        try:
            from urllib.parse import unquote

            text = unquote(text)
        except Exception:
            pass
    if "_" in text and len(text) > limit // 2:
        text = text.rsplit("_", 1)[-1]
    if text.isdigit() or text in _GENERIC_ALIASES:
        return ""
    return _clip(text, limit)


def _row_label(tree: _Tree, node: dict) -> str:
    """同一行左侧最近的标签（给滑块 / 输入框当名字：它们自己的 text 是值/占位）。"""
    best = None
    for other in tree.nodes:
        if not other["label"] or not other["in_view"]:
            continue
        if abs(other["center"][1] - node["center"][1]) > 32:
            continue
        if other["box"][2] > node["box"][0]:
            continue
        if best is None or other["box"][2] > best["box"][2]:
            best = other
    return best["label"] if best else ""


def _item_line(index: int, node: dict) -> str:
    """一行清单：`#N 「标签」 [id] 状态 ⌨快捷键 ⇢ uitest 参数`（不可点的只给 @坐标）。"""
    bits = ["#%d" % index]
    label = node["label"]
    if node.get("row_label") and (not label or label.replace(".", "").isdigit()):
        label = node["row_label"]
    alias = _alias(node["id"], 26) if node["id"] else ""
    container = node["scrollable"] and node["type"] in ("List", "Grid", "Scroll")
    if label:
        bits.append("「%s」" % label)
    if alias and alias != label:
        bits.append("[%s]" % alias)
    if node["type"] in ("TextInput", "Slider", "Toggle"):
        bits.append("(%s)" % node["type"])
    state = _state(node)
    if state:
        bits.append(state)
    if container:
        box = node["box"]
        bits.append("⬍%s (%d,%d)-(%d,%d)" % (node["type"], box[0], box[1], box[2], box[3]))
        bits.append("⇢ " + _command(node))
        return " ".join(bits) + "\n"
    x, y = node["center"]
    bits.append("@%d,%d" % (x, y))
    tap = node.get("tap")
    if tap:
        shortcut = _shortcut(node["attrs"])
        if shortcut:
            bits.append("⌨%s" % shortcut)
        bits.append("⇢ %s %d %d" % (tap[0], tap[1][0], tap[1][1]))
    return " ".join(bits) + "\n"


def _tap(node: dict, max_area: float, top_strip: int = -1) -> tuple[str, tuple[int, int]] | None:
    """点这一条该用哪个坐标：自己可点就用自己，否则用最近的可点祖先（文字常常只是标签）。

    `max_area` 是"敢不敢用祖先"的上限：标题栏那种覆盖整个窗口的可点容器点了没意义。
    """

    def small(box: tuple[int, int, int, int]) -> bool:
        return (box[2] - box[0]) * (box[3] - box[1]) <= max_area

    # 祖先得长得像"一行"（高度和节点差不多）：标题栏那种覆盖整个窗口的可点容器点了没意义
    rows = {"Row", "GridItem", "ListItem", "MenuItem", "Flex", "Column", "Stack"}

    if node["clickable"] and node["box"] and small(node["box"]):
        return "click", node["center"]
    if node["long_clickable"] and node["box"] and small(node["box"]) and not node["clickable"]:
        return "longClick", node["center"]
    if node["box"][1] < top_strip:
        return None  # 标题栏 / 面包屑那一带：祖先通常是"拖窗口"的容器，别给它造点击
    height = node["box"][3] - node["box"][1]
    parent = node["parent"]
    while parent is not None:
        box = parent["box"]
        if (
            parent["clickable"]
            and box
            and small(box)
            and parent["type"] in rows
            and (box[3] - box[1]) <= max(48, height * 2.5)
        ):
            return "click", parent["center"]
        parent = parent["parent"]
    return None


def _command(node: dict) -> str:
    """把这一条渲染成 `uitest uiInput …` 的参数（复制到 bash 里就能执行）。"""
    if node["scrollable"] and node["type"] in ("List", "Grid", "Scroll"):
        x1, y1, x2, y2 = node["box"]
        cx = (x1 + x2) // 2
        travel = int((y2 - y1) * 0.35)
        return "swipe %d %d %d %d 300" % (cx, (y1 + y2) // 2 + travel, cx, (y1 + y2) // 2 - travel)
    x, y = node["center"]
    if node["long_clickable"] and not node["clickable"]:
        return "longClick %d %d" % (x, y)
    return "click %d %d" % (x, y)


def _delta(previous: dict | None, current: _Tree) -> str:
    """「自上次观察以来变了什么」——判断"上一步生效没有"靠它（不是截图）。"""
    if not previous:
        return ""
    now = current.fingerprint()
    lines = []
    if previous.get("crumbs") != now["crumbs"]:
        lines.append(
            "位置：%s → %s"
            % (" › ".join(previous.get("crumbs") or []) or "-", " › ".join(now["crumbs"]) or "-")
        )
    if previous.get("counts") != now["counts"]:
        lines.append(
            "计数：%s → %s"
            % ("；".join(previous.get("counts") or []) or "-", "；".join(now["counts"]) or "-")
        )
    gone = [t for t in previous.get("texts") or [] if t not in now["texts"]]
    new = [t for t in now["texts"] if t not in (previous.get("texts") or [])]
    if gone:
        lines.append("消失：" + "、".join(gone[:5]))
    if new:
        lines.append("新增：" + "、".join(new[:5]))
    if not lines:
        return "[变化] 与上次观察**完全一致**（上一步大概没生效）"
    return "[变化] " + "；".join(lines)


# ---------------------------------------------------------------- 杂项


def _connect_device(hdc: _Hdc, spec: str, relay_port: int) -> None:
    """连一次设备：`auto` = WSL 默认网关 + 中继端口；`host:port` 照写；空串 = 不管。"""
    spec = (spec or "").strip()
    if not spec:
        return
    if spec == "auto":
        gateway = _gateway()
        if not gateway:
            return
        spec = "%s:%d" % (gateway, relay_port)
    try:
        hdc.run(["tconn", spec])  # 已连上时只是一行回执
    except RuntimeError:
        pass  # 连不上别在这儿失败：后面的命令会把真原因报出来


def _gateway() -> str | None:
    """默认网关（WSL → Windows 主机）：`/proc/net/route` 里 Destination=0 那行的 Gateway（小端 hex）。"""
    try:
        with open("/proc/net/route", encoding="utf-8") as file:
            for line in file:
                fields = line.split()
                if len(fields) > 2 and fields[1] == "00000000":
                    raw = fields[2]
                    return ".".join(str(int(raw[i : i + 2], 16)) for i in (6, 4, 2, 0))
    except OSError:
        return None
    return None


def _cache_key(hdc: _Hdc) -> str:
    return "%s|%s" % (hdc.bin, hdc.target or "-")


def _image_size(data: bytes) -> tuple[int, int] | None:
    """PNG / JPEG 的尺寸（只读文件头）。"""
    try:
        if data[:8] == b"\x89PNG\r\n\x1a\n" and len(data) >= 24:
            return int.from_bytes(data[16:20], "big"), int.from_bytes(data[20:24], "big")
        if data[:2] == b"\xff\xd8":
            i = 2
            while i + 9 < len(data):
                if data[i] != 0xFF:
                    i += 1
                    continue
                marker = data[i + 1]
                length = int.from_bytes(data[i + 2 : i + 4], "big")
                if marker in (0xC0, 0xC1, 0xC2):
                    return int.from_bytes(data[i + 7 : i + 9], "big"), int.from_bytes(data[i + 5 : i + 7], "big")
                i += 2 + length
    except Exception:
        pass
    return None


def _clip(text: str, limit: int) -> str:
    return text if len(text) <= limit else text[:limit] + "…"


def _data_root() -> Path:
    """数据根（与 Rust 侧 `pie_dir` 同一套：`PIE_HOME` → `<cwd>/.pie` → `~/.pie`）。"""
    home = os.environ.get("PIE_HOME", "").strip()
    if home:
        return Path(home)
    local = Path.cwd() / ".pie"
    return local if local.is_dir() else Path.home() / ".pie"


def _save_shot(data: bytes) -> str:
    """截图落地（内容寻址，与 pie 自己的图片副本同目录 → `pie files gc` 认得它）。"""
    directory = (_data_root() / "files").resolve()
    directory.mkdir(parents=True, exist_ok=True)
    suffix = ".jpg" if data[:2] == b"\xff\xd8" else ".png"
    path = directory / ("img-%s%s" % (hashlib.sha256(data).hexdigest()[:16], suffix))
    if not path.exists():
        path.write_bytes(data)
    return str(path)


if __name__ == "__main__":
    # 调试用：`python computer_use.py <dumpLayout.json>` —— 只解析本地 dump，不连设备
    import sys

    if len(sys.argv) < 2:
        print("用法: python computer_use.py <dumpLayout.json>   # 打印摘要 + 元素清单")
    else:
        with open(sys.argv[1], encoding="utf-8") as file:
            tree = _Tree(json.load(file))
        print(tree.summary(tree.screen_size()))
        print(tree.items_text(tree.items(DEFAULT_MAX_ITEMS), DEFAULT_MAX_ITEMS), end="")
