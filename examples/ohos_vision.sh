#!/bin/sh
# `ohos_vision`：**截全屏 + 叠坐标网格** —— 把设备屏幕变成一张「照着念坐标就能点」的图。
#
# schema! {
#   "description": [
#     "**读取**设备（鸿蒙模拟器 / 真机）当前屏幕：截一张**全屏**图（最高分辨率、不缩放、不裁剪），",
#     "叠一层**坐标网格**后返回本地路径（用 `read` 看图）。",
#     "坐标极简：**网格线上的数字就是屏幕坐标**（图尺寸 = 屏幕尺寸，如 3120x2080），照着念出来直接",
#     "`hdc shell \"uitest uiInput click <x> <y>\"` —— 不用估位置、不用换算缩放。",
#     "网格：细线每 `grid_step`（默认 200）像素，每 5 格一条粗线并标数值；数字沿上/中/下与左/中/右重复，",
#     "屏幕上任何位置都能就近读到刻度。",
#     "- `delay_ms`（默认 900）：连拍两张取较新那张（截图通道会滞后几秒）；`delay_ms=0` 只拍一张、更快。",
#     "- `grid_step=0`：不叠网格，返回**纯净原图**（网格挡住小字 / 配色时用）。",
#     "- 图几乎全黑 = 熄屏 → `hdc shell \"power-shell wakeup\"` 后重截。",
#     "- 点完没反应：重截一张看画面变了没；完全没变化 = 那一下没生效（换位置或先滚动）。",
#     "**本工具只读**：不点、不输入、不调模型；动一律用 `bash` + `uitest`。"
#   ],
#   "parameters": {
#     "type": "object",
#     "properties": {
#       "delay_ms": { "type": "integer", "description": "连拍两张之间的等待毫秒，默认 900；0 = 只拍一张" },
#       "grid_step": { "type": "integer", "description": "网格步长（像素），默认 200；0 = 不叠网格、返回原图" }
#     }
#   },
#   "timeout_seconds": 120
# }
#
# 契约见 docs/sh-plugins.md：
#   模型给的参数走命令行（`--delay_ms=900`）；站点配置走环境变量（`PIE_CFG_hdc` / `PIE_CFG_target` /
#   `PIE_CFG_connect` / `PIE_CFG_relay_port` / `PIE_CFG_timeout_seconds`）；stdout = 正文（这里就是
#   一行本地路径 + 一行提示）、stderr = 失败原因、exit ≠ 0 = 失败。
#   产物落 `$PIE_HOME/files/img-<hash>.jpeg`（内容寻址：`pie files gc` 认得它、`read` 能直接看）。
#
# 依赖：宿主侧 `hdc`（或 hdc.exe）、`sh`、`sha256sum`（或 macOS 的 `shasum`）、可选 `timeout`；
#       设备侧 `uitest`。叠网格要一个**带 Pillow 的 python**：先试 `$PIE_CFG_python`，再试
#       `$PIE_HOME/envs/base/bin/python3` （`pie setup` 建的解释器，Pillow 就在里面）、`~/.pie/envs/base/bin/python3`
#       （PIE_HOME 没设时的手动运行）、`python3` / `python`；一个都不成 → **退化成原图**并告警（绝不因此失败）。

set -eu

fail() {
    printf '%s\n' "$*" >&2
    exit 1
}

# ---------------------------------------------------------------- 参数

delay_ms=900
grid_step=200
for arg in "$@"; do
    case $arg in
        --delay_ms=*) delay_ms=${arg#*=} ;;
        --grid_step=*) grid_step=${arg#*=} ;;
        --) ;;
        -*) fail "未知参数：$arg" ;;
        *) fail "多余的参数：${arg}（本工具不吃位置参数）" ;;
    esac
done
case "$delay_ms" in '' | *[!0-9]*) delay_ms=900 ;; esac
case "$grid_step" in '' | *[!0-9]*) grid_step=200 ;; esac
# 太小的步长会把图糊成一片；1..19 归一到 20（0 专门表示「不叠网格」）
if [ "$grid_step" -gt 0 ] && [ "$grid_step" -lt 20 ]; then grid_step=20; fi

hdc_bin=${PIE_CFG_hdc:-hdc}
target=${PIE_CFG_target:-}
connect=${PIE_CFG_connect:-auto}
relay_port=${PIE_CFG_relay_port:-15555}
hdc_timeout=${PIE_CFG_timeout_seconds:-60}

home_dir=${PIE_HOME:-${TMPDIR:-/tmp}}
mkdir -p "$home_dir/files" 2>/dev/null || true

# **一次调用内的临时件**（拉回来的原图、连拍的旧那张）：用完就删。
# 本工具**没有任何跨调用状态**（每次都是「现在截一张」）→ 所以它不占 `$PIE_HOME`，
# 临时件就放系统临时目录；唯一留下来的东西是最终那张图（内容寻址进 `$PIE_HOME/files/`）。
work=$(mktemp -d 2>/dev/null || true)
[ -n "$work" ] || {
    work=${TMPDIR:-/tmp}/pie-ohos-vision-$$
    mkdir -p "$work"
}
trap 'rm -rf "$work"' EXIT INT TERM

# sha256 的十六进制摘要：GNU 是 `sha256sum`，macOS 只有 `shasum -a 256`
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1"
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1"
    else
        fail '需要 sha256sum（或 macOS 的 shasum）来算内容寻址的 id'
    fi
}

# ---------------------------------------------------------------- 设备侧

case "$hdc_bin" in
    *.exe | *.EXE) win_hdc=1 ;;  # DevEco 的 hdc.exe 在 WSL 里跑：本地路径要 Windows 形式
    *) win_hdc=0 ;;
esac

# 本地路径 → 交给 hdc 的形式
local_path() {
    if [ "$win_hdc" = 1 ] && command -v wslpath >/dev/null 2>&1; then
        wslpath -w "$1"
    else
        printf '%s' "$1"
    fi
}

# `hdc` 的唯一出口：argv 怎么拼、超时多久、失败怎么判，都在这一处。
# ⚠ hdc 对设备侧命令失败**也常返回 0**（只打一行 `[Fail]…`）→ 两条判据缺一不可。
hdc_run() {
    _desc=$1
    shift
    if [ -n "$target" ]; then
        set -- -t "$target" "$@"
    fi
    if command -v timeout >/dev/null 2>&1; then
        _out=$(timeout "$hdc_timeout" "$hdc_bin" "$@" 2>&1) || fail "hdc $_desc 失败: $_out"
    else
        _out=$("$hdc_bin" "$@" 2>&1) || fail "hdc $_desc 失败: $_out"
    fi
    case "$_out" in
        *'[Fail]'*) fail "hdc $_desc 失败: $_out" ;;
    esac
    printf '%s' "$_out"
}

# 默认网关（WSL → Windows 主机）：/proc/net/route 里 Destination=0 那行的 Gateway（小端 hex）。
gateway() {
    [ -r /proc/net/route ] || return 0
    while read -r _iface _dest _hex _rest; do
        [ "$_dest" = 00000000 ] || continue
        _b1=${_hex#??????}
        _t=${_hex%??}
        _b2=${_t#????}
        _t=${_t%??}
        _b3=${_t#??}
        _b4=${_hex%??????}
        printf '%d.%d.%d.%d' "$((0x$_b1))" "$((0x$_b2))" "$((0x$_b3))" "$((0x$_b4))"
        return 0
    done </proc/net/route
}

# `auto` = WSL 默认网关 + 中继端口；`host:port` 照写；空串 = 不管。
connect_device() {
    [ -n "$connect" ] || return 0
    _spec=$connect
    if [ "$_spec" = auto ]; then
        _gw=$(gateway)
        [ -n "$_gw" ] || return 0
        _spec="$_gw:$relay_port"
    fi
    # 连不上别在这儿失败（已连上时只是一行回执）；后面的命令会报出真原因
    "$hdc_bin" tconn "$_spec" >/dev/null 2>&1 || true
}

# 截一张 → 打印本地路径（`$1` = 作用在本地临时件上的后缀）
# ⚠ `uitest screenCap` 只认 `-p <路径>`：少了 `-p` 会**忽略**路径、写到自己生成的文件名
# （`screenCap_<n>.png`）→ 后面 recv 那个固定路径必然 no such file。
shoot() {
    hdc_run "screenCap" shell 'uitest screenCap -p /data/local/tmp/c.jpeg' >/dev/null
    _local="$work/shot$1.jpeg"
    _recv=$(hdc_run "file recv" file recv /data/local/tmp/c.jpeg "$(local_path "$_local")")
    case "$_recv" in
        *'FileTransfer finish'*) ;;
        *) fail "截图没拉回来: $_recv" ;;
    esac
    [ -s "$_local" ] || fail "截图是空的（熄屏？先 hdc shell \"power-shell wakeup\"）"
    printf '%s' "$_local"
}

# 叠坐标网格：把 `$1` 画上网格存成 `$2`（`$3` = 步长）。
# 一个带 Pillow 的 python 都找不到就返回非 0（调用方退化成原图，**绝不因此失败**）。
gridify() {
    _script="$work/grid.py"
    cat >"$_script" <<'PY'
import sys

from PIL import Image, ImageDraw, ImageFont

src, dst, step = sys.argv[1], sys.argv[2], int(sys.argv[3])
im = Image.open(src).convert("RGB")
width, height = im.size
major = step * 5

font = None
for path in (
    "/System/Library/Fonts/Helvetica.ttc",
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "C:/Windows/Fonts/arial.ttf",
):
    try:
        font = ImageFont.truetype(path, 44)
        break
    except Exception:
        continue
if font is None:
    try:
        font = ImageFont.load_default(size=44)
    except TypeError:
        font = ImageFont.load_default()

# 图内只留**极淡**的辅助线（真正读数靠四周的刻度），不挡内容
over = Image.new("RGBA", im.size, (0, 0, 0, 0))
draw = ImageDraw.Draw(over)
for x in range(0, width + 1, step):
    thick = x % major == 0
    draw.line([(x, 0), (x, height)], fill=(255, 0, 0, 105 if thick else 45), width=2 if thick else 1)
for y in range(0, height + 1, step):
    thick = y % major == 0
    draw.line([(0, y), (width, y)], fill=(0, 90, 255, 105 if thick else 45), width=2 if thick else 1)

# 四周留白：刻度 + 数值全放图外，数值仍是**屏幕坐标**（=图内像素坐标）
probe = ImageDraw.Draw(Image.new("RGB", (1, 1)))
text_w = int(max(probe.textlength(str(width), font=font), probe.textlength(str(height), font=font)))
text_h = int(font.getbbox("0")[3] - font.getbbox("0")[1])
tick = 22
pad_x = text_w + tick + 14
pad_y = text_h + tick + 12

canvas = Image.new("RGB", (width + 2 * pad_x, height + 2 * pad_y), (248, 248, 248))
canvas.paste(Image.alpha_composite(im.convert("RGBA"), over).convert("RGB"), (pad_x, pad_y))
draw = ImageDraw.Draw(canvas)
ox, oy = pad_x, pad_y
draw.rectangle([ox - 1, oy - 1, ox + width, oy + height], outline=(160, 160, 160), width=2)


def label(text, x, y, fill, align="left"):
    tw = draw.textlength(text, font=font)
    if align == "center":
        x -= tw / 2
    elif align == "right":
        x -= tw
    draw.text((x, y), text, fill=fill, font=font)


red, blue = (200, 0, 0), (0, 60, 200)
# 步长太小时相邻标签会撞在一起 → 刻度照画，数字隔几格才标一次
label_every = max(1, (text_w + 14 + step - 1) // step)

for x in range(0, width + 1, step):
    thick = x % major == 0
    length = tick if thick else tick // 2
    wide = 3 if thick else 1
    px = ox + x
    draw.line([(px, oy), (px, oy - length)], fill=red, width=wide)
    draw.line([(px, oy + height), (px, oy + height + length)], fill=red, width=wide)
    if (x // step) % label_every == 0:
        label(str(x), px, oy - length - text_h - 8, red, "center")
        label(str(x), px, oy + height + length + 8, red, "center")

for y in range(0, height + 1, step):
    thick = y % major == 0
    length = tick if thick else tick // 2
    wide = 3 if thick else 1
    py = oy + y
    draw.line([(ox, py), (ox - length, py)], fill=blue, width=wide)
    draw.line([(ox + width, py), (ox + width + length, py)], fill=blue, width=wide)
    if (y // step) % label_every == 0:
        label(str(y), ox - length - 8, py - text_h / 2, blue, "right")
        label(str(y), ox + width + length + 8, py - text_h / 2, blue, "left")

canvas.save(dst, "PNG")
PY
    for _py in "${PIE_CFG_python:-}" \
        "$home_dir/envs/base/bin/python3" "$home_dir/envs/base/bin/python" \
        "${HOME:-}/.pie/envs/base/bin/python3" "${HOME:-}/.pie/envs/base/bin/python" \
        python3 python; do
        [ -n "$_py" ] || continue
        if ! command -v "$_py" >/dev/null 2>&1 && [ ! -x "$_py" ]; then continue; fi
        "$_py" -c 'import PIL' >/dev/null 2>&1 || continue
        if "$_py" "$_script" "$1" "$2" "$3" >/dev/null 2>&1 && [ -s "$2" ]; then
            return 0
        fi
    done
    return 1
}

# ---------------------------------------------------------------- 抓一张（或连拍两张取新的）

connect_device

shot=$(shoot .1)
if [ "$delay_ms" -gt 0 ]; then
    # 截图通道会滞后几秒 → 等一会儿再拍一张，用第二张（界面多半已经变了）
    if command -v sleep >/dev/null 2>&1; then
        sleep "$((delay_ms / 1000)).$(printf '%03d' "$((delay_ms % 1000))")" 2>/dev/null ||
            sleep "$(((delay_ms + 999) / 1000))"
    fi
    newer=$(shoot .2)
    rm -f "$shot" 2>/dev/null || true
    shot=$newer
fi

# 叠坐标网格（`grid_step=0` 或没有带 Pillow 的 python → 跳过，原图照发）
gridded=0
if [ "$grid_step" -gt 0 ]; then
    if gridify "$shot" "$work/grid.png" "$grid_step"; then
        gridded=1
        shot="$work/grid.png"
    else
        printf '[ohos_vision] 没找到带 Pillow 的 python（可用 PIE_CFG_python 指定；或 uv pip install --python ~/.pie/envs/base/bin/python pillow），本次返回未叠网格的原图\n' >&2
    fi
fi

# 内容寻址：同内容的图只落一份（`pie files gc` 认这个位置，`read` 能直接看）
final="$home_dir/files/img-$(sha256 "$shot" | cut -c1-16).png"  # 设备给的是 PNG，不看扩展名
if [ ! -f "$final" ]; then mv "$shot" "$final"; else rm -f "$shot"; fi

# ---------------------------------------------------------------- 输出

size=$(wc -c <"$final" | sed 's/ //g')
printf '%s\n' "$final"
if [ "$gridded" = 1 ]; then
    printf '[截图] %s 字节，已叠坐标网格（细线 %spx / 粗线 %spx，线上数字 = 屏幕坐标；直接 uitest uiInput click <x> <y>）\n' \
        "$size" "$grid_step" "$((grid_step * 5))"
else
    printf '[截图] %s 字节（未叠网格；图上像素坐标 = 屏幕坐标）\n' "$size"
fi
