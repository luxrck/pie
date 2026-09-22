"""Textual TUI 主题：集中管理界面配色与图标（按 role / 按工具 / 按执行结果），支持按名字切换。

两类内容：
- **展示数据**（纯常量 + 查表，不依赖 Textual / Rich）：颜色常量 + 图标常量 + 注册表；
- **终端背景明暗探测**（`detect_dark_background`，只用标准库）：OSC 11 查询 → COLORFGBG → None，
  供主题族自动选深/浅变体。

界面结构与样式生成（build_css / _box）保留在 tui.py，便于与控件布局绑定。

主题分两层：
- **主题族**（THEME_FAMILIES，如 `catppuccin`）：一个族 = 深色 + 浅色两个变体，取主题时按
  终端背景明暗（detect_dark_background）自动选一个——这就是「一套主题同时适应
  深色/浅色终端」；
- **具体变体**（THEMES，如 `catppuccin-mocha` / `catppuccin-latte`）：固定明暗，不探测。
"""

from __future__ import annotations

import os
import re
import select
import time
from dataclasses import dataclass, field
from functools import lru_cache

# 内部模块：配色/图标数据 + 终端背景探测 + 主题查表（config.py 取默认族名、tui.py 取 palette 与 CSS）。
__all__: list[str] = []


try:  # pragma: no cover - 非 Unix 平台没有 termios
    import termios
except ImportError:  # pragma: no cover
    termios = None  # type: ignore[assignment]


# ---- 终端背景明暗探测（OSC 11 → COLORFGBG → None）----
# 只用标准库；任何不确定都返回 None，由调用方回退到默认变体（探不到按深色）。

# OSC 11 响应：``ESC ] 11 ; rgb:RRRR/GGGG/BBBB ST``（分量 2 或 4 位十六进制；部分终端带 alpha）
_OSC11_RE = re.compile(
    r"\x1b\]11;rgba?:([0-9a-fA-F]{2,4})/([0-9a-fA-F]{2,4})/([0-9a-fA-F]{2,4})"
)


def parse_osc11(reply: str) -> bool | None:
    """解析 OSC 11 响应里的背景色 → 背景是否深色；解析不了返回 None。"""
    match = _OSC11_RE.search(reply)
    if match is None:
        return None
    channels = [int(part, 16) / (16 ** len(part) - 1) for part in match.groups()]
    luma = 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2]
    return luma < 0.5


def parse_colorfgbg(value: str | None) -> bool | None:
    """解析 COLORFGBG（``"fg;bg"``，颜色索引 0-15）→ 背景是否深色；解析不了返回 None。

    背景索引 0-7 是暗色、8-15 是亮色（xterm/rxvt/WezTerm 等会设置这个变量）。
    """
    if not value:
        return None
    parts = value.split(";")
    try:
        background = int(parts[-1])
    except (ValueError, IndexError):
        return None
    return background < 8


def query_osc11(timeout: float = 0.2) -> str | None:
    """向控制终端发 OSC 11 查询并读回响应；无控制终端 / 超时 / 出错返回 None。

    直接读写 ``/dev/tty``（不碰 stdin/stdout），所以在管道的 shell 里也能工作；
    读前临时关掉规范模式与回显，读完恢复。响应超时是必要的——不支持该查询的终端
    不会有任何回复，不能无限等。
    """
    if termios is None:  # pragma: no cover - 非 Unix
        return None
    try:
        fd = os.open("/dev/tty", os.O_RDWR | os.O_NOCTTY)
    except OSError:
        return None
    original = None
    try:
        original = termios.tcgetattr(fd)
        raw = termios.tcgetattr(fd)
        raw[3] &= ~(termios.ICANON | termios.ECHO)
        raw[6][termios.VMIN] = 0
        raw[6][termios.VTIME] = 0
        termios.tcsetattr(fd, termios.TCSANOW, raw)
        os.write(fd, b"\x1b]11;?\x1b\\")
        buf = b""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], max(0.0, deadline - time.monotonic()))
            if not ready:
                break
            chunk = os.read(fd, 256)
            if not chunk:
                break
            buf += chunk
            if buf.endswith(b"\x1b\\") or buf.endswith(b"\x07"):
                break
        return buf.decode("utf-8", "replace") or None
    except termios.error:  # type: ignore[union-attr]
        return None
    finally:
        if original is not None:
            try:
                termios.tcsetattr(fd, termios.TCSADRAIN, original)  # type: ignore[union-attr]
            except termios.error:  # type: ignore[union-attr]  # pragma: no cover
                pass
        os.close(fd)


@lru_cache(maxsize=1)
def detect_dark_background() -> bool | None:
    """探测终端背景是否深色：OSC 11 查询 → COLORFGBG → None（未知）。

    进程内只探测一次（结果缓存）：OSC 查询最多阻塞 timeout，别在每次取主题时都做。
    """
    dark = parse_osc11(query_osc11() or "")
    if dark is not None:
        return dark
    return parse_colorfgbg(os.environ.get("COLORFGBG"))


# ---- 主题数据与查表 ----

# 默认主题名（config.theme 缺省值）：主题族 → 按终端明暗自适应
DEFAULT_THEME_NAME = "catppuccin"


@dataclass(frozen=True)
class Theme:
    """TUI 界面配色 + 图标（按 role + 按工具名覆盖 + 按执行结果）。

    字段按语义分组：背景/正文、边框（默认/暗/强调）、次级文本、滚动条、
    各 role 边框与图标、按工具名的图标覆盖、简洁模式结果标记、发送按钮 busy 态。
    颜色值为 CSS 颜色字符串（hex / rgba / transparent）；图标为盒子标题前缀（"" = 无图标）。
    """

    name: str

    # 背景 / 正文
    screen_bg: str       # 窗口背景（transparent = 露出终端底色）
    log_bg: str          # 消息流背景
    body_text: str       # 正文文本色
    # 代码（Markdown 行内代码 / 代码块）的样式覆盖：**完整的 Rich 样式串**
    # （如 "#4c4f69 on #e6e9ef" 或 "bold cyan"）；空串 = 不覆盖（沿用 Rich
    # 默认的 `cyan on black`）。只给浅色变体用——Rich 默认给代码配了硬编码黑底，
    # 在浅色终端里像一块墨；深色变体用默认就正好。
    markdown_code: str
    # 代码块（围栏）的语法高亮主题（pygments 主题名），经 `Markdown(code_theme=...)` 传入。
    # 高亮 token 自带背景色、会盖过 `markdown.code_block`，所以浅色变体必须配浅色主题
    # （现为 solarized-light）——否则仍是一块 monokai 的 `#272822`。
    code_theme: str

    # 边框
    border: str          # 输入框 / 补全面板 / 发送按钮 默认边框
    border_dim: str      # 日志区边框（比 border 更暗）
    accent: str          # 焦点 / 选中 / 发送按钮 hover 边框
    accent_text: str     # 选中框内文字（accent 的前景）

    # 次级文本（状态栏 / meta / stream / 滚动条滑块 / 发送按钮默认文字）
    muted: str           # 次级文本（比正文暗）
    faint: str           # 最弱文本（占位符 / 系统框）

    # 滚动条（轨道带 alpha，与透明背景 alpha 混合成半透明灰）
    scrollbar_track: str
    scrollbar_track_hover: str

    # 各 role 边框色
    role_user: str
    role_assistant: str
    role_tool_call: str
    role_tool_result: str
    role_error: str
    role_system: str

    # 各 role 图标（盒子标题前缀；"" = 不显示图标）。
    # icon_ok / icon_error / icon_cancelled 是「执行结果」的字形：盒子模式下分作
    # role=tool_result / error / cancelled 的标题图标，简洁模式下同一套字形放行首
    # （见 lean_mark）——同一个字形在两处表示同一个意思，不再各配一份。
    icon_user: str
    icon_assistant: str
    icon_tool_call: str
    icon_ok: str          # 成功（role tool_result）
    icon_error: str       # 失败/出错（role error）
    icon_cancelled: str   # 被 /stop 终止（role cancelled；既非成功也非失败）
    icon_system: str

    # 按工具名覆盖图标（可选）：tool_icons 用于**工具调用**框（工具名 → 图标，如
    # {"shell": "❯"}），未列出的回退 icon_tool_call；tool_result_icons 是对结果框状态
    # 字形（icon_ok/error/cancelled）的可选覆盖（仅当“结果框仍想带工具自己的图标”时才配）。
    # hash=False：dict 不可哈希，不让它们参与 __hash__（Theme 仍可按值比较）。
    tool_icons: dict[str, str] = field(hash=False)
    tool_result_icons: dict[str, str] = field(hash=False)

    # 发送按钮 busy（处理中 = 停止）态
    busy_border: str
    busy_border_hover: str
    busy_text: str

    def markdown_styles(self) -> dict[str, str]:
        """最小 Markdown 覆盖：只改带硬编码黑底的代码（`markdown_code` 为空则不覆盖）。

        其余 markdown.* 元素（标题/引用/表格/链接）保持 Rich 默认的 ANSI 具名色，由浅色终端
        自行映射；不接管它们就不会与终端脱节。注意：**带语言标注的代码块（fence）底色由
        `Markdown(code_theme=...)` 的高亮主题 token 自带，本覆盖治不到它**。
        """
        if not self.markdown_code:
            return {}
        return {"markdown.code": self.markdown_code, "markdown.code_block": self.markdown_code}

    def role_border(self, role: str) -> str:
        """按 role 取边框色：未知 role 回退 system 灰。

        cancelled（被 /stop 终止）与 system 同色：既不是成功也不是失败，低调灰即可。
        """
        return {
            "user": self.role_user,
            "assistant": self.role_assistant,
            "tool_call": self.role_tool_call,
            "tool_result": self.role_tool_result,
            "error": self.role_error,
            "cancelled": self.role_system,
            "system": self.role_system,
        }.get(role, self.role_system)

    def role_icon(self, role: str) -> str:
        """按 role 取标题图标：未知 role 回退 system（无图标）。

        role=tool_result / error / cancelled 的三个字形就是「执行结果」标记
        （icon_ok / icon_error / icon_cancelled），盒子标题与简洁模式行首共用同一套。
        """
        return {
            "user": self.icon_user,
            "assistant": self.icon_assistant,
            "tool_call": self.icon_tool_call,
            "tool_result": self.icon_ok,
            "error": self.icon_error,
            "cancelled": self.icon_cancelled,
            "system": self.icon_system,
        }.get(role, self.icon_system)

    def tool_icon(self, name: str) -> str:
        """按工具名取**调用**图标：命中 tool_icons 则用它（值为 "" = 该工具不显示图标），
        否则回退 role 默认（icon_tool_call）。name 为空或未配置的工具都走回退。
        """
        icon = self.tool_icons.get(name or "")
        return self.role_icon("tool_call") if icon is None else icon

    def result_icon(self, role: str, name: str = "") -> str:
        """工具**结果**框的图标：按 role 取状态字形（tool_result → icon_ok、error →
        icon_error、cancelled → icon_cancelled、system → 无图标）。

        role 由 tui 按执行结果判定（退出码 / 失败前缀 / 取消）。主题若按工具名配了
        tool_result_icons（"" = 不要图标）则以它为准。
        """
        icon = self.tool_result_icons.get(name or "")
        return self.role_icon(role) if icon is None else icon

    def lean_mark(self, status: str) -> str:
        """简洁模式单行工具记录行首的状态标记：status ∈ ok / fail / cancelled（未知回退 ok）。

        与结果的 role 图标是同一套字形（icon_ok / icon_error / icon_cancelled）：主题只提供
        **字形**，取哪个 status 由 tui 侧按执行结果判定。
        """
        return {
            "ok": self.icon_ok,
            "fail": self.icon_error,
            "cancelled": self.icon_cancelled,
        }.get(status, self.icon_ok)


# 图标（字形不随明暗变化，各变体共用）
_SHARED_ICONS: dict[str, object] = dict(
    icon_user="▎",              # 你
    icon_assistant="▎",         # pie 回复
    icon_tool_call="✽",         # 工具调用
    icon_ok="✓",               # 成功的工具结果；简洁模式的成功标记
    icon_error="✗",            # 出错框（含失败的工具结果）；简洁模式的失败标记
    icon_cancelled="■",         # 被 /stop 终止（role cancelled）；简洁模式同一字形
    icon_system="",             # 命令反馈/系统提示（无图标）
    # 按工具名覆盖图标：tool_icons = 调用框（一眼看出调的是哪个工具）；
    # tool_result_icons = 结果框对状态字形（✅/❌/⏹）的可选覆盖，默认留空。
    tool_icons={"read": "✧", "edit": "✽", "write": "✦", "shell": "✛"},
    tool_result_icons={},
)

# catppuccin mocha（深色变体）：recover 历史配色（背景透明 + 各 role 区分色）
CATPPUCCIN_MOCHA = Theme(
    name="catppuccin-mocha",
    screen_bg="transparent",
    log_bg="transparent",
    body_text="#cdd6f4",            # text
    markdown_code="",                # 深色变体不覆盖（Rich 默认的黑底在这里正好）
    code_theme="monokai",           # 代码块高亮：Rich 默认就是它（#272822 底）
    border="#45475a",               # surface0
    border_dim="#313244",           # surface1（日志边框比输入框更暗）
    accent="#89b4fa",               # blue
    accent_text="#06121f",          # 选中文字（深蓝黑）
    muted="#a6adc8",                # overlay0（状态栏/stream/滚动条滑块）
    faint="#585b70",                # overlay3（占位符/系统框）
    scrollbar_track="rgba(108, 112, 134, 0.35)",
    scrollbar_track_hover="rgba(108, 112, 134, 0.5)",
    role_user="#5b78a6",            # 你（深蓝）
    role_assistant="#0f766e",       # pie 回复（深青）
    role_tool_call="#9c4916",       # 工具调用（橙棕）
    role_tool_result="#805b45",     # 工具结果（棕）
    role_error="#f38ba8",           # 出错（红）
    role_system="#585b70",          # 命令反馈/系统提示（低调灰）
    **_SHARED_ICONS,
    busy_border="#f38ba8",          # busy 边框（亮红）
    busy_border_hover="#ffb4c8",    # busy hover 边框（更亮）
    busy_text="#f38ba8",            # busy 文字
)


# catppuccin latte（浅色变体）：同一套语义字段，换成浅色背景下的可读配色
CATPPUCCIN_LATTE = Theme(
    name="catppuccin-latte",
    screen_bg="transparent",
    log_bg="transparent",
    body_text="#4c4f69",            # text
    markdown_code="bold cyan",       # 浅色变体：去掉黑底、只留青色粗体字
    code_theme="solarized-light",   # 代码块高亮：暖白底 #fdf6e3（换掉 monokai 的黑块）
    border="#9ca0b0",               # overlay0（浅色下边框要够看得见）
    border_dim="#ccd0da",           # surface0（日志边框比输入框更淡）
    accent="#1e66f5",               # blue
    accent_text="#ffffff",          # 选中文字（蓝底白字）
    muted="#7c7f93",                # overlay2（状态栏/stream/滚动条滑块）
    faint="#9ca0b0",                # overlay0（占位符/系统框）
    scrollbar_track="rgba(124, 127, 147, 0.35)",
    scrollbar_track_hover="rgba(124, 127, 147, 0.5)",
    role_user="#1e66f5",            # 你（蓝）
    role_assistant="#0f766e",       # pie 回复（深青，白底可读）
    role_tool_call="#9c4916",       # 工具调用（橙棕）
    role_tool_result="#805b45",     # 工具结果（棕）
    role_error="#d20f39",           # 出错（红）
    role_system="#8c8fa1",          # 命令反馈/系统提示（低调灰）
    **_SHARED_ICONS,
    busy_border="#d20f39",          # busy 边框（红）
    busy_border_hover="#e64553",    # busy hover 边框（更亮）
    busy_text="#d20f39",            # busy 文字
)


# 主题族：一个族 = (深色变体, 浅色变体)，取用时按终端背景明暗自动选
THEME_FAMILIES: dict[str, tuple[Theme, Theme]] = {
    "catppuccin": (CATPPUCCIN_MOCHA, CATPPUCCIN_LATTE),
}

THEMES: dict[str, Theme] = {
    CATPPUCCIN_MOCHA.name: CATPPUCCIN_MOCHA,
    CATPPUCCIN_LATTE.name: CATPPUCCIN_LATTE,
}


def _match_family(key: str) -> tuple[Theme, Theme] | None:
    if key in THEME_FAMILIES:
        return THEME_FAMILIES[key]
    lower = key.lower()
    for name, family in THEME_FAMILIES.items():
        if name.lower() == lower:
            return family
    return None


def _match_theme(key: str) -> Theme | None:
    if key in THEMES:
        return THEMES[key]
    lower = key.lower()
    for name, theme in THEMES.items():
        if name.lower() == lower:
            return theme
    return None


def get_theme(name: str | None = None, dark: bool | None = None) -> Theme:
    """按名字取主题（支持「族名」与「具体变体名」两种），未知/为空时回退默认。

    - **族名**（如 ``catppuccin``）：按 `dark` 选深/浅变体；`dark` 为 None 时探测终端背景
      （`detect_dark_background`），探测不到按深色处理——这就是「一套主题适应深色/浅色终端」。
    - **具体变体名**（如 ``catppuccin-mocha``）：固定返回，`dark` 不参与。

    名字匹配忽略首尾空白与大小写；未知主题名不打断界面启动。
    """
    key = str(name).strip() if name else DEFAULT_THEME_NAME
    family = _match_family(key)
    if family is not None:
        dark_variant, light_variant = family
        if dark is None:
            dark = detect_dark_background()
        return light_variant if dark is False else dark_variant
    theme = _match_theme(key)
    if theme is not None:
        return theme
    return CATPPUCCIN_MOCHA


def build_css(palette: Theme) -> str:
    """由主题（palette）生成 PieApp 的 Textual CSS：布局 + 配色，无硬编码颜色。

    颜色全部来自 Theme；布局/滚动条配置固定。运行时在 __init__ 注入到 self.CSS，
    Textual 在 load 阶段读取的是实例属性 self.CSS（而非类级 CSS），因此可按所选主题动态生成。
    """
    # 滚动条统一样式：窄（1 cell）+ 半透明灰轨道 + 亮灰滑块，替换默认的 2 cell 黑底蓝条。
    # 轨道用带 alpha 的灰：ScrollBar 渲染时若背景 alpha<1 会与父级背景（沿 transparent
    # 链最终是终端默认背景色）alpha 混合 → 半透明灰透出终端底色；不要用 transparent（纯透明轨道会隐形）。
    scrollbar = f"""scrollbar-size: 0 1;
    scrollbar-background: {palette.scrollbar_track};
    scrollbar-background-hover: {palette.scrollbar_track_hover};
    scrollbar-background-active: {palette.scrollbar_track_hover};
    scrollbar-color: {palette.muted};
    scrollbar-color-hover: {palette.body_text};
    scrollbar-color-active: {palette.body_text};"""
    return f"""
Screen {{ layout: vertical; background: {palette.screen_bg}; }}
#log {{
    height: 1fr;
    border: round {palette.border_dim};
    padding: 0 1;
    background: {palette.log_bg};
    {scrollbar}
}}
#stream {{
    height: auto;
    max-height: 4;
    color: {palette.muted};
    padding: 0 1;
    display: none;
    border: round {palette.border_dim};
}}
#assistant-stream {{
    height: auto;
    max-height: 12;
    display: none;
    border: round {palette.border_dim};
    padding: 0 1;
    {scrollbar}
}}
#assistant-panel {{
    width: 100%;
}}
#foot-bar {{
    height: auto;
}}
#meta, #status {{
    height: auto;
    color: {palette.muted};
    /*padding: 0 1;*/
}}
/* #meta 撑满剩余宽度，把 #status 顶到最右；两者同处 #foot-bar 一行 */
#meta {{
    width: 1fr;
    text-wrap: nowrap;
    text-overflow: ellipsis;
}}
#status {{
    width: auto;
    text-align: right;
}}
#input-bar {{
    height: auto;
}}
CommandPalette {{
    height: auto;
    /*max-height: 6;*/
    border: round {palette.border};
    background: transparent;
    color: {palette.body_text};
    padding: 0 1;
    display: none;
}}
#input {{
    width: 1fr;
    height: auto;
    min-height: 5;
    max-height: 5;
    background: {palette.screen_bg};
    color: {palette.body_text};
    border: round {palette.border};
    /* 输入框（TextArea = ScrollView）自带滚动条，默认是 Textual 的 2 cell 黑底蓝条，
       与 #log / #assistant-stream 不一致 → 用同一份 scrollbar 样式（TextArea 的
       ScrollBar 子控件读的是**父控件**的 scrollbar-* 样式，所以写在 #input 上即可）。 */
    {scrollbar}
    & .text-area--placeholder {{
        color: {palette.faint};
    }}
    /* 选中高亮与 #log 鼠标框选（SelectableRichLog._selection_style）统一：
       accent 底 + accent_text 字。TextArea 在 ansi 主题下（App 用 ansi-dark）
       有一条 `&:ansi .text-area--selection {{ background: transparent; text-style: reverse; }}`；
       #input 的 ID 规则虽然更具体、能盖住 background/color，但**盖不住 text-style**
       （该规则没声明 text-style 时，低优先级的 reverse 仍会叠加）→ 选中呈反色，
       与 #log 的框选视觉相反。所以必须显式写 text-style: none 清掉 reverse。 */
    & .text-area--selection {{
        background: {palette.accent};
        color: {palette.accent_text};
        text-style: none;
    }}
}}
#input:focus {{
    border: round {palette.accent};
}}
#input.shell-mode, #input.shell-mode:focus {{
    border: round {palette.role_tool_call};
}}
#send-btn {{
    min-width: 0;
    padding: 1;
    text-align: center;
    background: transparent;
    border: round {palette.border};
    color: {palette.muted};
}}
#send-btn:hover {{
    background: transparent;
    border: round {palette.accent};
    color: {palette.body_text};
}}
#send-btn.busy {{
    background: transparent;
    border: round {palette.busy_border};
    color: {palette.busy_text};
}}
#send-btn.busy:hover {{
    background: transparent;
    border: round {palette.busy_border_hover};
    color: {palette.busy_border_hover};
}}
Header {{ background: {palette.screen_bg}; color: {palette.body_text}; }}
Footer {{ background: {palette.screen_bg}; color: {palette.body_text}; }}
"""


