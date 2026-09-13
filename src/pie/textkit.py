"""显示层文本处理：CJK 友好断行 + 控制符/ANSI 转义清洗。

都是纯函数、不依赖 Textual，便于单独测试（tests/test_tui.py）。

CJK 断行需要 `install_cjk_wrap()` 显式安装一次（TUI 导入 tui.py 时调用）。要装两处，
因为显示层有两条独立的排版链路：

- `#log` / 盒子 / Markdown 等 Rich 渲染 → `Text.wrap()` 在调用时从 `rich.text` 模块全局取
  `divide_line`，替换那个模块属性即可。
- `#input`（TextArea）→ 不走 Rich 排版，而是 `WrappedDocument` 调 `textual._wrap.compute_wrap_offsets`，
  得单独替换 `_wrapped_document` 模块里这个名字。
"""

from __future__ import annotations

import re
from typing import Iterator

from rich.cells import cell_len, chop_cells
from rich.text import Text

# ---- CJK 友好断行：把 Rich 的词级换行换成「全角字也可断」----
#
# Rich 只在空白处断行（rich.text.divide_line → rich._wrap.divide_line 用 `\s*\S+\s*`
# 分词）：中文长句没有空格 → 整段被挪到下一行、上一行一大片留白（宽 74 实测只用 42 格）。
# 这里把全角字（cell_len == 2）拆成单字 token，非全角串仍按词（**不在英文单词内部断**），
# 于是「英文词尽量不断、中文可逐字折」。纯 ASCII 文本交回 Rich 原实现（逐字节不变）。
# 注：零宽断点（U+200B）行不通——Python 的 `\s` 不匹配它，Rich 的分词认不出来。

try:  # Rich 内部实现，只用于纯 ASCII 快路径
    from rich._wrap import divide_line as _rich_divide_line
except Exception:  # pragma: no cover - Rich 换实现时退回自研断行
    _rich_divide_line = None


def _is_wide(ch: str) -> bool:
    """全角字（中文/日文全角标点/假名等占两格）——作为可断点。"""
    return cell_len(ch) == 2


def _wrap_tokens(text: str) -> Iterator[tuple[int, str]]:
    """切断行 token：单个全角字 / 一段非全角非空白；尾部空白贴在该 token 上。

    yield (start, token)，token 含其后空白（语义对齐 rich._wrap.words：断点落在 token 开头，
    尾部空白留在上一行）。
    """
    i, n = 0, len(text)
    while i < n:
        start = i
        while i < n and text[i].isspace():
            i += 1
        if i >= n:  # 行尾空白
            yield (start, text[start:])
            return
        if _is_wide(text[i]):
            i += 1
        else:
            while i < n and not text[i].isspace() and not _is_wide(text[i]):
                i += 1
        while i < n and text[i].isspace():
            i += 1
        yield (start, text[start:i])


def cjk_divide_line(text: str, width: int, fold: bool = True) -> list[int]:
    """CJK 友好版 divide_line（同契约：返回断点的字符下标列表）。

    与 Rich 原版只有分词不同：全角字各自成一个 token，于是「放不下就换行」对全角字
    等价于逐字折行（英文词仍是整词挪到下一行）；比整行宽的 token 硬折（chop_cells）。
    """
    if width < 1 or not text:
        return []
    if _rich_divide_line is not None and not any(_is_wide(ch) for ch in text):
        return _rich_divide_line(text, width, fold)
    breaks: list[int] = []
    cell_offset = 0  # 当前行已占单元格
    for start, token in _wrap_tokens(text):
        length = cell_len(token.rstrip())  # 尾部空白不计入“词宽”（同 Rich）
        if width - cell_offset >= length:
            cell_offset += cell_len(token)
        elif length > width:  # token 比整行还宽 → 硬折
            if not fold:
                if cell_offset:
                    breaks.append(start)
                cell_offset = cell_len(token)
                continue
            offset = start
            chunks = chop_cells(token, width)
            for index, chunk in enumerate(chunks):
                if offset:
                    breaks.append(offset)
                if index == len(chunks) - 1:
                    cell_offset = cell_len(chunk)
                else:
                    offset += len(chunk)
        elif cell_offset and start:
            breaks.append(start)
            cell_offset = cell_len(token)
    # 防御：宽度极小时 chop_cells 可能产出空块 → 去掉重复/越界断点
    return [
        offset
        for index, offset in enumerate(breaks)
        if 0 < offset < len(text) and (index == 0 or offset > breaks[index - 1])
    ]


# ---- Textual TextArea（#input 输入框）的断行 ----
#
# TextArea 的换行由 WrappedDocument 负责，分词同样用 `\S+\s*|\s+`（textual._wrap.compute_wrap_offsets），
# 于是「无空格的中文长串」被当成一个不可断的词：只要它比**行尾剩余空间**宽，就整段挪到下一行，
# 上一行留下一大片空白（实测 width=30 时 "把 #log " 之后只剩 8 格就换行）。
# 这里补一个同契约的实现（字符下标断点），行为与 cjk_divide_line 一致。

_textual_orig_wrap_offsets = None
"""安装时捕获的 Textual 原实现：含 \t 的行交回它（tab 展开宽度依赖列位置）。"""


def cjk_compute_wrap_offsets(
    text: str,
    width: int,
    tab_size: int = 4,
    fold: bool = True,
    precomputed_tab_sections: list[tuple[str, int]] | None = None,
) -> list[int]:
    """CJK 友好版 compute_wrap_offsets（TextArea 用；契约与 textual._wrap 同）。

    含制表符的行直接交回 Textual 原实现：tab 宽度随列位置变化，原实现会用调用方预计算的
    `precomputed_tab_sections`，本实现不重算。
    """
    if _textual_orig_wrap_offsets is not None and "\t" in text:
        return _textual_orig_wrap_offsets(
            text, width, tab_size, fold, precomputed_tab_sections
        )
    return cjk_divide_line(text, width, fold)


def install_cjk_wrap() -> bool:
    """把 Rich 与 Textual TextArea 的断行都换成 CJK 友好实现（接口不在则静默跳过）。

    Rich：Text.wrap() 在调用时从 rich.text 模块全局取 divide_line，改这个模块属性即可
    让所有 Rich 渲染（盒子正文、Markdown、状态栏…）都用上 —— 即 #log 区域。
    Textual：WrappedDocument 在模块里按全局名调 compute_wrap_offsets，改 _wrapped_document
    的同名属性即可 —— 即 #input 输入框。
    只在 TUI 模块导入时安装。
    """
    global _textual_orig_wrap_offsets
    try:
        import rich.text as rich_text
    except Exception:  # pragma: no cover
        return False
    if rich_text.divide_line is not cjk_divide_line:
        rich_text.divide_line = cjk_divide_line  # type: ignore[assignment]
    try:
        from textual.document import _wrapped_document as wrapped_document
    except Exception:  # pragma: no cover - 没有 Textual 时只装 Rich 那份（CLI 模式）
        return True
    if wrapped_document.compute_wrap_offsets is not cjk_compute_wrap_offsets:
        if _textual_orig_wrap_offsets is None:
            _textual_orig_wrap_offsets = wrapped_document.compute_wrap_offsets
        wrapped_document.compute_wrap_offsets = cjk_compute_wrap_offsets  # type: ignore[assignment]
    return True


# ---- 显示层文本清洗：控制符 / ANSI 转义 ----
# 工具输出（典型：shell 的 `ls --color`）会带 ANSI 转义序列。这些字节在终端上不可见，
# 但 Rich 排版时会把它们算进文本宽度 → Panel 的右边框被推到实际内容之外（超出日志区
# 宽度后换行），盒子画歪。这里统一在显示前清洗：SGR 颜色序列交给 Text.from_ansi 解成
# Rich 样式（保留 `ls --color` 的配色，宽度只按可见文本算），其余转义（OSC / 光标移动 /
# 单字符转义）与 C0 控制符（保留 \n \t）直接剔除。
_SGR_RE = re.compile(r"\x1b\[[0-9;]*m")
_ESC_RE = re.compile(
    r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)?"  # OSC ... BEL / ST
    r"|\x1b[()#%][0-9A-Za-z]"  # 字符集/私有选择 ESC ( B / ESC ) 0 / ESC # 8 / ESC % G
    r"|\x1b\[[0-?]*[ -/]*[@-~]"  # CSI（含 SGR，由回调决定保留与否）
    r"|\x1b.?"  # 其余转义（ESC M / ESC 7 …，含末尾孤立的 ESC）
)
_CTRL_RE = re.compile(r"[\x00-\x08\x0b-\x0c\x0e-\x1a\x1c-\x1f\x7f]")  # 除 \n \t \x1b


def strip_escapes(text: str, *, keep_sgr: bool = True) -> str:
    """剔除控制符与转义序列；keep_sgr=True 时保留 SGR 颜色序列（交 from_ansi 解码）。

    回车单独处理：`\r\n` 归一成 `\n`、孤立 `\r`（进度条回写）剔除——Rich 不按回车的
    覆写语义排版，留着会与终端实际显示错位。

    顺序要紧：先剔转义序列再剔控制符——OSC 以 BEL(\x07) 结尾，先删 BEL 会让
    `\x1b]...` 的匹配吞掉其后全部文本（实测丢内容）。
    """
    text = text.replace("\r\n", "\n").replace("\r", "")
    text = _ESC_RE.sub(
        lambda m: m.group(0) if keep_sgr and _SGR_RE.fullmatch(m.group(0)) else "", text
    )
    return _CTRL_RE.sub("", text)


def rich_text(text: str, style: str = "") -> Text:
    """显示层外部文本 → Rich Text：SGR 解码成样式，不可见字节不再参与宽度计算。"""
    text = strip_escapes(text)
    if "\x1b" in text:
        return Text.from_ansi(text, style=style)
    return Text(text, style=style)

