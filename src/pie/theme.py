"""Textual TUI 主题：集中管理界面配色，支持按名字切换。

只做「配色数据」：theme.py 不依赖 Textual / Rich，纯粹是颜色常量 + 注册表。
界面结构与样式生成（build_css）保留在 tui.py，便于与控件布局绑定。

当前内置 catppuccin-mocha（还原历史配色的默认主题）；
后续扩展新主题只需在 THEMES 里加一项 Theme。
"""

from __future__ import annotations

from dataclasses import dataclass

# 默认主题名（config.theme 缺省值）
DEFAULT_THEME_NAME = "catppuccin-mocha"


@dataclass(frozen=True)
class Theme:
    """TUI 界面配色。

    字段按语义分组：背景/正文、边框（默认/暗/强调）、次级文本、滚动条、
    各 role 边框、发送按钮 busy 态。颜色值为 CSS 颜色字符串（hex / rgba / transparent）。
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
