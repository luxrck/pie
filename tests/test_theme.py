"""主题层回归测试：终端明暗探测 + 主题族自适应 + Markdown 样式来自主题（无 pytest 依赖）。

跑法（任一）：

    uv run python tests/test_theme.py
    pytest tests/test_theme.py        # 装了 pytest 也能直接跑

覆盖两块：

- **探测解析**（纯函数）：OSC 11 响应、COLORFGBG 的解析与异常输入兜底；
- **主题族**：族名按 `dark` 选深/浅变体；具体变体名固定明暗；未知/为空回退默认。
"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from pie.theme import (
    CATPPUCCIN_LATTE,
    CATPPUCCIN_MOCHA,
    DEFAULT_THEME_NAME,
    get_theme,
    parse_colorfgbg,
    parse_osc11,
)


def test_parse_osc11() -> None:
    """OSC 11 响应 → 明暗：4 位/2 位分量、带 alpha、异常输入都要稳。"""
    assert parse_osc11("\x1b]11;rgb:0000/0000/0000\x1b\\") is True
    assert parse_osc11("\x1b]11;rgb:ffff/ffff/ffff\x1b\\") is False
    assert parse_osc11("\x1b]11;rgb:1e1e/1e1e/2e2e\x07") is True  # catppuccin base（深）
    assert parse_osc11("\x1b]11;rgb:ef/ef/f5\x1b\\") is False  # 2 位分量（浅）
    assert parse_osc11("\x1b]11;rgba:ffff/ffff/ffff/ffff\x1b\\") is False  # 带 alpha 后缀
    assert parse_osc11("") is None
    assert parse_osc11("garbage") is None


def test_parse_colorfgbg() -> None:
    """COLORFGBG（``fg;bg`` 索引）：背景索引 < 8 视为深色。"""
    assert parse_colorfgbg("0;15") is False  # 浅色终端（WezTerm 常见）
    assert parse_colorfgbg("15;0") is True
    assert parse_colorfgbg("0") is True
    assert parse_colorfgbg("7;7") is True
    assert parse_colorfgbg("15;8") is False
    assert parse_colorfgbg("") is None
    assert parse_colorfgbg(None) is None
    assert parse_colorfgbg("abc") is None


def test_theme_family_picks_variant_by_dark() -> None:
    """族名按 dark 选变体；具体变体名固定；未知/为空回退默认。"""
    assert get_theme("catppuccin", dark=True) is CATPPUCCIN_MOCHA
    assert get_theme("catppuccin", dark=False) is CATPPUCCIN_LATTE
    assert get_theme("Catppuccin", dark=False) is CATPPUCCIN_LATTE  # 大小写不敏感
    assert get_theme(" catppuccin ", dark=True) is CATPPUCCIN_MOCHA
    # 具体变体名不受 dark 影响（要固定明暗就用它们）
    assert get_theme("catppuccin-mocha", dark=False) is CATPPUCCIN_MOCHA
    assert get_theme("catppuccin-latte", dark=True) is CATPPUCCIN_LATTE
    # 为空 / 未知 → 默认（族名 → 深色变体）
    assert DEFAULT_THEME_NAME == "catppuccin"
    assert get_theme(None, dark=False) is CATPPUCCIN_LATTE
    assert get_theme(None, dark=True) is CATPPUCCIN_MOCHA
    assert get_theme("nope", dark=True) is CATPPUCCIN_MOCHA


def test_variants_differ_and_share_icons() -> None:
    """深/浅变体除配色外结构一致：图标字形相同、关键配色不同。"""
    assert CATPPUCCIN_MOCHA.name != CATPPUCCIN_LATTE.name
    assert CATPPUCCIN_MOCHA.body_text != CATPPUCCIN_LATTE.body_text
    assert CATPPUCCIN_MOCHA.accent != CATPPUCCIN_LATTE.accent
    assert CATPPUCCIN_MOCHA.border != CATPPUCCIN_LATTE.border
    assert CATPPUCCIN_MOCHA.icon_ok == CATPPUCCIN_LATTE.icon_ok
    assert CATPPUCCIN_MOCHA.tool_icons == CATPPUCCIN_LATTE.tool_icons
    # role 边框色共享（语义色，不随明暗变）
    assert CATPPUCCIN_MOCHA.role_border("tool_call") == CATPPUCCIN_LATTE.role_border("tool_call")


def test_markdown_styles_are_minimal() -> None:
    """Markdown 覆盖只针对代码两键、且只给浅色变体（深色变体不接管）。"""
    assert CATPPUCCIN_MOCHA.markdown_styles() == {}
    assert CATPPUCCIN_MOCHA.markdown_code == ""
    assert CATPPUCCIN_LATTE.markdown_code
    styles = CATPPUCCIN_LATTE.markdown_styles()
    assert set(styles) == {"markdown.code", "markdown.code_block"}
    # 值原样使用（不再自动加粗等）
    for style in styles.values():
        assert style == CATPPUCCIN_LATTE.markdown_code


def test_code_theme_follows_variant() -> None:
    """代码块高亮主题：深色变体照抄 Rich 默认 monokai，浅色变体换成浅色主题。"""
    from pygments.styles import get_style_by_name

    def luma(hex_color: str) -> float:
        r, g, b = (int(hex_color[i : i + 2], 16) / 255 for i in (1, 3, 5))
        return 0.2126 * r + 0.7152 * g + 0.0722 * b

    assert CATPPUCCIN_MOCHA.code_theme == "monokai"  # Rich 的默认值
    assert CATPPUCCIN_LATTE.code_theme != CATPPUCCIN_MOCHA.code_theme
    mocha_bg = get_style_by_name(CATPPUCCIN_MOCHA.code_theme).background_color
    latte_bg = get_style_by_name(CATPPUCCIN_LATTE.code_theme).background_color
    assert luma(mocha_bg) < 0.3, mocha_bg
    assert luma(latte_bg) > 0.7, latte_bg


def _main() -> int:
    tests = [(n, f) for n, f in sorted(globals().items()) if n.startswith("test_") and callable(f)]
    failed = []
    for name, func in tests:
        try:
            func()
        except AssertionError as exc:
            failed.append((name, exc))
            print(f"[FAIL] {name}: {exc}")
        else:
            print(f"[PASS] {name}")
    print(f"\n{len(tests) - len(failed)}/{len(tests)} 通过")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(_main())
