"""Textual TUI 主题：集中管理界面配色与图标（按 role / 按工具），支持按名字切换。

只做「展示数据」：theme.py 不依赖 Textual / Rich，纯粹是颜色常量 + 图标常量 + 注册表。
界面结构与样式生成（build_css / _box）保留在 tui.py，便于与控件布局绑定。

当前内置 catppuccin-mocha（还原历史配色的默认主题）；
后续扩展新主题只需在 THEMES 里加一项 Theme。
"""

from __future__ import annotations

from dataclasses import dataclass, field

# 默认主题名（config.theme 缺省值）
DEFAULT_THEME_NAME = "catppuccin-mocha"


@dataclass(frozen=True)
class Theme:
    """TUI 界面配色 + 图标（按 role + 按工具名覆盖）。

    字段按语义分组：背景/正文、边框（默认/暗/强调）、次级文本、滚动条、
    各 role 边框与图标、按工具名的图标覆盖、发送按钮 busy 态。
    颜色值为 CSS 颜色字符串（hex / rgba / transparent）；图标为盒子标题前缀（"" = 无图标）。
    """

    name: str

    # 背景 / 正文
    screen_bg: str       # 窗口背景（transparent = 露出终端底色）
    log_bg: str          # 消息流背景
    body_text: str       # 正文文本色

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

    # 各 role 图标（盒子标题前缀；"" = 不显示图标）
    icon_user: str
    icon_assistant: str
    icon_tool_call: str
    icon_tool_result: str
    icon_error: str
    icon_system: str

    # 按工具名覆盖图标（可选）：tool_icons 用于工具调用框、tool_result_icons 用于
    # 工具结果框，如 {"shell": "$", "read": "📄"}。未列出的工具回退对应 role 的
    # 默认图标（icon_tool_call / icon_tool_result）。分两张表是因为同一工具会以
    # 「调用」「结果」两种形态出现，两类图标互不覆盖（保留 ⚙ vs ↳ 的形态区分）。
    # hash=False：dict 不可哈希，不让它们参与 __hash__（Theme 仍可按值比较）。
    tool_icons: dict[str, str] = field(hash=False)
    tool_result_icons: dict[str, str] = field(hash=False)

    # 发送按钮 busy（处理中 = 停止）态
    busy_border: str
    busy_border_hover: str
    busy_text: str

    def role_border(self, role: str) -> str:
        """按 role 取边框色：未知 role 回退 system 灰。"""
        return {
            "user": self.role_user,
            "assistant": self.role_assistant,
            "tool_call": self.role_tool_call,
            "tool_result": self.role_tool_result,
            "error": self.role_error,
            "system": self.role_system,
        }.get(role, self.role_system)

    def role_icon(self, role: str) -> str:
        """按 role 取标题图标：未知 role 回退 system（无图标）。

        图标与边框色语义不同，可分开指定——典型：工具结果框失败时边框染红
        （role="error"），图标仍用 tool_result 的 ↳（见 tui._tool_result_box）。
        """
        return {
            "user": self.icon_user,
            "assistant": self.icon_assistant,
            "tool_call": self.icon_tool_call,
            "tool_result": self.icon_tool_result,
            "error": self.icon_error,
            "system": self.icon_system,
        }.get(role, self.icon_system)

    def tool_icon(self, name: str, *, result: bool = False) -> str:
        """按工具名取图标：命中 tool_icons（调用）/ tool_result_icons（结果）则用它，
        否则回退对应 role 的默认图标（icon_tool_call / icon_tool_result）；
        name 为空或未配置的工具都走回退。

        命中且值为 "" 表示该工具刻意不显示图标（未命中才回退，二者不同）。
        """
        table = self.tool_result_icons if result else self.tool_icons
        icon = table.get(name)
        if icon is None:
            icon = self.role_icon("tool_result" if result else "tool_call")
        return icon


# catppuccin mocha 变体：recover 历史配色（背景透明 + 各 role 区分色）
CATPPUCCIN_MOCHA = Theme(
    name="catppuccin-mocha",
    screen_bg="transparent",
    log_bg="transparent",
    body_text="#cdd6f4",            # text
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
    icon_user="▎",              # 你
    icon_assistant="▎",         # pie 回复
    icon_tool_call="⚙",         # 工具调用
    icon_tool_result="↳",       # 工具结果
    icon_error="✗",             # 出错
    icon_system="",             # 命令反馈/系统提示（无图标）
    # 按工具名的图标覆盖（未配置的工具 → 上面的 role 图标 ⚙ / ↳；值为 "" = 该工具不显示图标）
    # 调用框用工具图标（一眼看出调的是哪个工具）；结果框保持 ↳（图标编码「形态」：
    # ↳ = 上一个框的产出，工具名仍在标题里），故默认不配 tool_result_icons。
    tool_icons={"read": "✧", "edit": "⟱", "write": "✦", "shell": "❯"},
    tool_result_icons={},
    busy_border="#f38ba8",          # busy 边框（亮红）
    busy_border_hover="#ffb4c8",    # busy hover 边框（更亮）
    busy_text="#f38ba8",            # busy 文字
)


THEMES: dict[str, Theme] = {
    CATPPUCCIN_MOCHA.name: CATPPUCCIN_MOCHA,
}


def get_theme(name: str | None) -> Theme:
    """按名字取主题；名字不存在或为空时回退默认主题（catppuccin-mocha）。

    归一化：忽略首尾空白与大小写（连字符/下划线原样匹配），
    匹配不到也回退默认，不让未知主题名打断界面启动。
    """
    if not name:
        return CATPPUCCIN_MOCHA
    key = str(name).strip()
    if key in THEMES:
        return THEMES[key]
    # 大小写不敏感兜底
    lower = key.lower()
    for theme_name, theme in THEMES.items():
        if theme_name.lower() == lower:
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

