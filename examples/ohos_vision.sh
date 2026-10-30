#!/bin/sh
# `ohos_vision`（**原图版 / v11**）：截一张设备全屏原图；可选叠一层带刻度的比例网格（JPEG）。
#
# 参数走**命令行**（与 shplugin 的 `--键=值` 契约一致），`--help` 看全部。
# 环境变量 `PIE_ARG_*` 仍然认（老宿主 / 老配置的兜底），优先级低于 argv。
#
# ── 历史 ──
# v12（2026-10-27，用户指定）：`--grid_step` **默认值改成 0.01**（每 1% 一格，100×100）；
#    要细网格就叠，不要就显式 `--grid_step=0`（关网格也省掉 matplotlib ~3s）。
# v11（2026-10-27，用户指定）：网格图也出 **JPEG**（不再 PNG）；刻度步长**固定每 5 格**，
#    去掉一切自动放稀/加密；删掉 `--grid_every` 参数。
# v10（2026-10-27，用户指定）：网格改由 **matplotlib** 画（`grid_step>0` 就要 matplotlib + Pillow）——
#    刻度**四周都标**（上下 x、左右 y），**数字落在坐标轴外面**（红框内才是屏幕，不遮内容）；
#    minor 细虚线、major 实线，tick marks 由 matplotlib 自己摆。代价：多 ~3s 启动。
# v9（2026-10-27）：细虚线打底 + 每 5 格一条实线 + 图外刻度（当时用 PIL 手画）；
#    标签挤不下就放稀、只剩 0~1 个就加密到每格——这条步长规则沿用至今。
# v8（2026-10-27，用户指定）：参数从 `PIE_ARG_*` 环境变量改成命令行参数，补 `--help`；
#    旧脚本里的 env 读取保留为兜底，不再是主通道。
# v7（2026-10-27）：新增 `--grid_step`（0~1，0 = 无网格）→ 叠半透明红线网格。**只有网格
#    生效时**产物才转 PNG（JPEG 会把细线糊成色晕），且需要 Pillow。
# v6（2026-10-27）：截图引擎默认从 `uitest screenCap`（PNG）换成 `snapshot_display -t jpeg`
#    —— 设备侧 ~0.4s、端到端 ~0.5s、**实时**（改画面后 md5 立即变）；产物 **JPEG / RGB 三
#    通道 / 3120x2080 / ~280KB**。要无损像素级分析用 `--_engine=screencap`。
#
# 输出 = 头区 `header! { … }`（`image` 是协议字段：宿主上传后模型直接看图，不必再 `read`；
#        有网格时另有 `grid_step`（红框内 = 屏幕本身））
#        + 空行 + 正文（相对比例换算提示）。
# 落盘 `PIE_HOME/files/img-<hash>.<jpeg|png>`（内容寻址，`pie files gc` 认得）。
# 坐标一律用**相对比例**（左上 0,0 → 右下 1,1）：图与屏幕 1:1 同尺寸，但模型看到的是上游
# 缩放过的视图，图上像素**不能**当屏幕坐标。
#
# schema! {
#   "description": [
#     "**读取**设备（鸿蒙模拟器 / 真机）当前屏幕：截一张**全屏原图**（最高分辨率，不裁剪/不缩放/不加边框）发给模型。",
#     "可选 `grid_step` 在图上叠半透明红线网格（**默认已开：0.01，每 1% 一格**；`0` = 关掉）。"
#   ],
#   "parameters": {
#     "type": "object",
#     "properties": {
#       "delay_ms": { "type": "integer", "description": "连拍两张之间的等待毫秒，默认 0 = 单拍（snapshot_display 实时，无需双拍）；>0 时连拍两张并取较新、顺带检测画面是否在变" },
#       "grid_step": { "type": "number", "description": "网格间隔（相对比例，0~1）：默认 0.01 = 每 1% 一格（100×100）；0.1 = 每 10% 一格；0 = 无网格。网格用 matplotlib 画：细虚线打底 + 每 5 格一条实线，刻度数字在坐标轴外、四周都标（不遮内容）；>0 时需要 matplotlib + Pillow" }
#     }
#   },
#   "timeout_seconds": 120
# }
#
# 契约（docs/sh-plugins.md）：stdout = 正文；stderr = 失败原因；exit ≠ 0 = 失败。
# 私有参数（下划线开头）是站点配置：`--_hdc` / `--_target` / `--_connect` / `--_relay_port` /
# `--_py` / `--_file` / `--_timeout_seconds` / `--_engine`。
#
# 依赖：`hdc`（宿主）、`snapshot_display`（设备，主路径）、`uitest`（设备，仅 `--_engine=screencap`）、
#       `sh`、`sed`、`sha256sum`、`$PY`（默认 /home/cc/.pie/envs/base/bin/python：熄屏检测用 Pillow、
#       `--grid_step>0` 时用 matplotlib；缺 Pillow 也照跑（只是不出熄屏提示），缺 matplotlib 时
#       `--grid_step` 只能为 0）。

set -u

# ════════════════════════════════════════════════════════════════════ 默认值
# 每个参数：先取 `PIE_ARG_*`（老宿主兼容），下面 argv 解析再覆盖。
default_hdc=/home/cc/ohos/linux/x86_64/sdk/default/openharmony/toolchains/hdc

delay_ms=${PIE_ARG_delay_ms:-0}
grid_step=${PIE_ARG_grid_step:-0.01}      # 默认叠 1% 网格（用户指定，v12）；0 = 关
engine=${PIE_ARG__engine:-snapshot}          # snapshot | screencap
hdc_bin=${PIE_ARG__hdc:-$default_hdc}
target=${PIE_ARG__target:-}
connect=${PIE_ARG__connect:-auto}            # auto | none | host:port
relay_port=${PIE_ARG__relay_port:-15555}
py=${PIE_ARG__py:-/home/cc/.pie/envs/base/bin/python}
local_png=${PIE_ARG__file:-}
hdc_timeout=${PIE_ARG__timeout_seconds:-60}

# ════════════════════════════════════════════════════════════════════ 输出 / 退出
usage() {
    cat <<'EOF'
ohos_vision —— 截一张鸿蒙设备的全屏原图（可选叠比例网格）

用法：sh ohos_vision.sh [--键=值 …]

参数
  --delay_ms=MS        连拍两张并取较新的一张；>0 还能探出画面是否在变（默认 0 = 单拍）
  --grid_step=X        叠网格，X ∈ (0,1]：0.1 = 每 10% 一格（默认 0.01 = 每 1% 一格）
                       --grid_step=0 关掉网格（也省掉 matplotlib 启动时间）
                       用 matplotlib 画：刻度在图外、四周都标，
                       细虚线打底 + 每 5 格一条实线并标刻度；>0 时需要 matplotlib + Pillow
  -h, --help           显示本帮助

站点参数（下划线开头是「配置」，模型看不见）
  --_engine=MODE       snapshot（默认，JPEG 实时）| screencap（PNG 无损但慢）
  --_file=PATH         不截图，直接用本地图（叠网格 / 探尺寸很快）
  --_hdc=PATH          hdc 可执行文件
  --_target=ID         多设备时指定目标
  --_connect=SPEC      auto（默认，走网关中继）| none | host:port
  --_relay_port=PORT   中继端口（默认 15555）
  --_py=PATH           python（Pillow：熄屏检测 / 网格绘制）
  --_timeout_seconds=N hdc 单次调用超时（默认 60）

环境
  PIE_HOME             数据根：产物落 <PIE_HOME>/files/（必需）
  PIE_ARG_*            老宿主的参数通道，argv 优先

示例
  PIE_HOME=/home/cc/ohos_pc sh ohos_vision.sh                  # 默认 grid_step=0.01
  PIE_HOME=/home/cc/ohos_pc sh ohos_vision.sh --grid_step=0.1
  PIE_HOME=/home/cc/ohos_pc sh ohos_vision.sh --grid_step 0.2 --delay_ms 300
  PIE_HOME=/home/cc/ohos_pc sh ohos_vision.sh --_file=/tmp/shot.png --grid_step=0.25
EOF
}

die() {
    printf '[错误] %s\n' "$*" >&2
    printf '（用法：%s --help）\n' "${0##*/}" >&2
    exit 1
}

# ════════════════════════════════════════════════════════════════════ 参数解析
set_opt() {
    case $1 in
        delay_ms) delay_ms=$2 ;;
        grid_step) grid_step=$2 ;;
        _engine) engine=$2 ;;
        _hdc) hdc_bin=$2 ;;
        _target) target=$2 ;;
        _connect) connect=$2 ;;
        _relay_port) relay_port=$2 ;;
        _py) py=$2 ;;
        _file) local_png=$2 ;;
        _timeout_seconds) hdc_timeout=$2 ;;
        *) die "未知参数：--$1" ;;
    esac
}

while [ $# -gt 0 ]; do
    case $1 in
        -h | --help | help)
            usage
            exit 0
            ;;
        --*=*)
            key=${1%%=*}
            value=${1#*=}
            shift
            ;;
        --*)
            key=$1
            shift
            [ $# -gt 0 ] || die "$key 需要一个值"
            value=$1
            shift
            ;;
        *)
            die "不认识的位置参数：$1（参数写成 --键=值）"
            ;;
    esac
    set_opt "${key#--}" "$value"
done

# ── 校验
case $delay_ms in
    '' | *[!0-9]*) die "delay_ms 必须是非负整数，收到：$delay_ms" ;;
esac
case $grid_step in
    '' | *[!0-9.]* | *.*.*) die "grid_step 必须是 0~1 之间的数（0 = 无网格），收到：$grid_step" ;;
    0 | 1 | 0.* | 1.0*) ;;
    *) die "grid_step 必须在 0~1 之间（0 = 无网格），收到：$grid_step" ;;
esac
case $engine in
    snapshot | screencap) ;;
    *) die "engine 只能是 snapshot 或 screencap，收到：$engine" ;;
esac
case $relay_port in
    '' | *[!0-9]*) die "relay_port 必须是数字，收到：$relay_port" ;;
esac
case $hdc_timeout in
    '' | *[!0-9]*) die "timeout_seconds 必须是数字，收到：$hdc_timeout" ;;
esac

# `0` / `0.0` / `0.00` 都是「关」
case $grid_step in
    0 | 0.0 | 0.00 | 0.000) grid_step=0 ;;
esac

# ════════════════════════════════════════════════════════════════════ 路径 / 时间
home_dir=${PIE_HOME:-${TMPDIR:-/tmp}}
state_dir=${PIE_STATE_DIR:-$home_dir/state/ohos_vision}
files_dir=$home_dir/files
mkdir -p "$state_dir" "$files_dir" 2>/dev/null || true

# ⚠ 这台机器上 `date +%s%3N` 实际返回纳秒（19 位），统一当纳秒用
now_ns() {
    _n=$(date +%s%N 2>/dev/null)
    case $_n in '' | *[!0-9]*) _n=$(date +%s)000000000 ;; esac
    printf '%s' "$_n"
}

# ════════════════════════════════════════════════════════════════════ hdc
hdc_run() {
    _desc=$1
    shift
    if [ -n "$target" ]; then
        set -- -t "$target" "$@"
    fi
    if command -v timeout >/dev/null 2>&1; then
        _out=$(timeout "$hdc_timeout" "$hdc_bin" "$@" 2>&1) || die "hdc $_desc 失败: $_out"
    else
        _out=$("$hdc_bin" "$@" 2>&1) || die "hdc $_desc 失败: $_out"
    fi
    case $_out in
        *'[Fail]'*) die "hdc $_desc 失败: $_out" ;;
    esac
    printf '%s' "$_out"
}

# hdc 对设备侧失败也常返回 0（只打 `[Fail]…`）→ 判失败一律看输出。
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

# WSL 够不到 Windows 的 127.0.0.1:5555 → 走网关上的中继（`--_connect=host:port` 可覆盖）。
connect_device() {
    [ -n "$connect" ] || return 0
    case $connect in
        auto)
            _gw=$(gateway)
            [ -n "$_gw" ] || return 0
            _spec="$_gw:$relay_port"
            ;;
        none) return 0 ;;
        *) _spec=$connect ;;
    esac
    "$hdc_bin" tconn "$_spec" >/dev/null 2>&1 || true
}

rm_remote() {
    if [ -n "$target" ]; then
        "$hdc_bin" -t "$target" shell rm -f "$1" >/dev/null 2>&1 || true
    else
        "$hdc_bin" shell rm -f "$1" >/dev/null 2>&1 || true
    fi
}

# ════════════════════════════════════════════════════════════════════ 截图
# 主路径 `snapshot_display -t jpeg`（实时、设备侧 ~0.4s）；`screencap` 回退到 `uitest screenCap`
# 的 PNG（无损但慢，且可能给旧帧）。回显：本地图片路径。
shot_snapshot() {
    _dev="/data/local/tmp/pie-snap-$$-$(now_ns).jpeg"
    _out=$(hdc_run snapshot shell "snapshot_display -t jpeg -f $_dev")
    case $_out in
        *success*) ;;
        *) die "snapshot_display 没成功：$_out" ;;
    esac
    _local="$state_dir/raw-$$-$(now_ns).jpeg"
    hdc_run 'file recv' file recv "$_dev" "$_local" >/dev/null
    rm_remote "$_dev"
    [ -s "$_local" ] || die "截图拉回失败：$_dev"
    printf '%s' "$_local"
}

# 设备**不认后缀**：给什么名字都存成 screenCap_<num>.png，回执里才是真实路径。
shot_screencap() {
    _out=$(hdc_run screenCap shell "uitest screenCap /data/local/tmp/pie-v8-shot.png")
    _p=$(printf '%s\n' "$_out" | sed -n 's/.*ScreenCap saved to \(.*\.png\).*/\1/p' | tail -n 1)
    [ -n "$_p" ] || _p=$(printf '%s\n' "$_out" | grep -o '/data/local/tmp/[^ ]*\.png' | tail -n 1)
    [ -n "$_p" ] || die "screenCap 没给出落盘路径：$_out"
    _local="$state_dir/raw-$$-$(now_ns).png"
    hdc_run 'file recv' file recv "$_p" "$_local" >/dev/null
    [ -s "$_local" ] || die "截图拉回失败：$_p"
    printf '%s' "$_local"
}

shot_once() {
    case $engine in
        screencap) shot_screencap ;;
        *) shot_snapshot ;;
    esac
}

# ════════════════════════════════════════════════════════════════════ 探测 + 网格
# 一次 python 调用干三件事：出尺寸、判熄屏、（grid_step>0 时）把红线叠到图上。
# 回显 `键=值` 三行（screen / blank / grid，grid 为 `-` 表示没画），shell 侧零解析。
probe_and_overlay() {
    _src=$1
    _out=$2
    "$py" - "$_src" "$grid_step" "$_out" 2>/dev/null <<'PY'
import sys

try:
    import matplotlib

    matplotlib.use('Agg')                       # 无显示环境（WSL / 服务器）也能出图
    import matplotlib.pyplot as plt
    from matplotlib.ticker import FixedLocator, FuncFormatter, NullFormatter
    from PIL import Image
except Exception:                               # 没 matplotlib / Pillow：让 shell 报清楚
    print('nompl')
    sys.exit(7)

path, step, out = sys.argv[1], sys.argv[2], sys.argv[3]

im = Image.open(path).convert('RGB')
w, h = im.size
print('screen=%dx%d' % (w, h))
lum = im.convert('L').resize((24, 24))
print('blank=%d' % (1 if sum(lum.tobytes()) / 576.0 < 6 else 0))

g = float(step)
if g <= 0 or not out:
    print('grid=-')
    sys.exit(0)

# ── 网格：细虚线打底（minor），每 N 格一条实线（major）+ 刻度。
# matplotlib 负责「刻度画在图外」这套布局：数字在坐标轴外侧，绝不压内容。
cells = max(1, int(round(1.0 / g)))             # 每轴格数
dpi = 100
font_size = max(9.0, min(w, h) / dpi * 1.05)    # pt ≈ 31px @3120x2080

# 刻度步长：固定「每 5 格」一条实线 + 一个刻度，不做任何自动调整
major = 5

major_vals = [k * g for k in range(1, cells) if k % major == 0]
minor_vals = [k * g for k in range(1, cells) if k % major]

fig = plt.figure(figsize=(w / dpi, h / dpi), dpi=dpi)
ax = fig.add_axes([0, 0, 1, 1])                 # 图像区正好等于原图尺寸（1:1，不缩放）
ax.imshow(im, extent=[0, 1, 1, 0], aspect='auto', interpolation='nearest')
ax.set_xlim(0, 1)
ax.set_ylim(1, 0)

ax.xaxis.set_major_locator(FixedLocator(major_vals))
ax.xaxis.set_minor_locator(FixedLocator(minor_vals))
ax.yaxis.set_major_locator(FixedLocator(major_vals))
ax.yaxis.set_minor_locator(FixedLocator(minor_vals))

value = FuncFormatter(lambda v, _pos: '{:g}'.format(round(v, 4)))
ax.xaxis.set_major_formatter(value)
ax.yaxis.set_major_formatter(value)
ax.xaxis.set_minor_formatter(NullFormatter())
ax.yaxis.set_minor_formatter(NullFormatter())

# 刻度四周都写：上下给 x（左右各一份），左右给 y（上下各一份）
ax.tick_params(axis='x', which='major', top=True, bottom=True,
               labeltop=True, labelbottom=True, length=10, width=2,
               color='firebrick', labelcolor='darkred',
               labelsize=font_size, pad=6)
ax.tick_params(axis='x', which='minor', top=True, bottom=True,
               length=5, width=1.2, color='firebrick')
ax.tick_params(axis='y', which='major', left=True, right=True,
               labelleft=True, labelright=True, length=10, width=2,
               color='firebrick', labelcolor='darkred',
               labelsize=font_size, pad=6)
ax.tick_params(axis='y', which='minor', left=True, right=True,
               length=5, width=1.2, color='firebrick')

ax.grid(which='major', color='red', alpha=0.85, linewidth=2.2)
ax.grid(which='minor', color='red', alpha=0.35, linewidth=1.2, linestyle=(0, (7, 5)))
ax.set_axisbelow(False)                         # 线压在图上（图是 artist，默认会在下面）

for spine in ax.spines.values():
    spine.set_color('firebrick')
    spine.set_linewidth(1.5)

fig.savefig(out, dpi=dpi, bbox_inches='tight', pad_inches=0.12, facecolor='white',
            format='jpeg', pil_kwargs={'quality': 95})
plt.close(fig)

print('grid=%s' % out)
print('major=%d' % major)

PY
}

# ════════════════════════════════════════════════════════════════════ 主流程
unstable=0
if [ -n "$local_png" ]; then
    # 用现成的本地图：不截屏，只做探测 / 叠网格
    [ -s "$local_png" ] || die "本地图不存在：$local_png"
    raw=$local_png
else
    connect_device
    raw=$(shot_once)
    if [ "$delay_ms" -gt 0 ]; then
        # 双拍只是用来探「画面在动」：snapshot_display 本身就实时
        sleep_s=$(printf '%d.%03d' $((delay_ms / 1000)) $((delay_ms % 1000)))
        sleep "$sleep_s" 2>/dev/null || sleep 1
        raw2=$(shot_once)
        if cmp -s "$raw" "$raw2"; then
            rm -f "$raw2"
        else
            unstable=1
            rm -f "$raw"
            raw=$raw2
        fi
    fi
fi

screen=
blank=0
grid_shot=
grid_tmp=
grid_major=
if [ "$grid_step" != 0 ]; then
    [ -x "$py" ] || die "grid_step=$grid_step 需要 matplotlib + Pillow：$py 不可用 → 用 --grid_step=0"
    grid_tmp="$state_dir/grid-$$-$(now_ns).jpeg"
fi

if [ -x "$py" ]; then
    while IFS='=' read -r _k _v; do
        case $_k in
            screen) screen=$_v ;;
            blank) blank=$_v ;;
            grid) [ "$_v" = '-' ] || grid_shot=$_v ;;
            major) grid_major=$_v ;;
            nompl) die "grid_step=$grid_step 需要 matplotlib：$py 里没装上" ;;
        esac
    done <<EOF
$(probe_and_overlay "$raw" "$grid_tmp")
EOF
    if [ "$grid_step" != 0 ] && [ -z "$grid_shot" ]; then
        die "grid_step=$grid_step 网格没画出来（Pillow 缺失或绘图失败：$py）"
    fi
fi
case $blank in '' | *[!0-9]*) blank=0 ;; esac
[ -n "$screen" ] || screen=3120x2080   # 设备分辨率兜底：恒为整屏

# ════════════════════════════════════════════════════════════════════ 落盘
# 内容寻址：同一屏幕同一参数 → 同一路径（重复调用不涨盘）。
save() {
    _src=$1
    _ext=$2
    _h=$(sha256sum "$_src" | cut -c1-16)
    _dst=$files_dir/img-$_h.$_ext
    if [ -f "$_dst" ]; then
        rm -f "$_src" 2>/dev/null || true
    else
        mv "$_src" "$_dst" 2>/dev/null || cp "$_src" "$_dst"
    fi
    printf '%s' "$_dst"
}

if [ -n "$grid_shot" ]; then
    shot=$(save "$grid_shot" jpeg)
    if [ "$local_png" = "$raw" ]; then :; else rm -f "$raw" 2>/dev/null || true; fi
else
    case $raw in
        *.png | *.PNG) shot_ext=png ;;
        *) shot_ext=$(printf '%s' "${raw##*.}") ;;
    esac
    shot=$(save "$raw" "$shot_ext")
fi
[ -s "$shot" ] || die "图没落盘：$shot"

# ════════════════════════════════════════════════════════════════════ 输出
# 头区：`image` 是协议字段（宿主上传 → 模型直接看图）；`grid_step` 只在真的叠了网格时给。
if [ -n "$grid_shot" ]; then
    printf 'header! { "screen": "%s", "image": "%s", "grid_step": %s }\n\n' \
        "$screen" "$shot" "$grid_step"
else
    printf 'header! { "screen": "%s", "image": "%s" }\n\n' "$screen" "$shot"
fi

printf '你无法确定输入图片是否经过缩放，因此坐标一律用**相对比例**（左上 0,0 → 右下 1,1，取目标中心）表示，之后再根据 Screen 分辨率换算成绝对坐标。\n'
if [ -n "$grid_shot" ]; then
    printf '（网格图在屏幕外面加了一圈留白放刻度：**红框内才是屏幕本身**（%s），坐标按框内估；细虚线 = 每 %s 一格，实线 + 框外数字 = 每 %s 格一条）\n' \
        "$screen" "$grid_step" "$grid_major"
fi
[ "$unstable" = 1 ] && printf '（画面刚变过，已取最新一张）\n'
[ "$blank" = 1 ] && printf '（图几乎全黑：可能熄屏 → hdc shell "power-shell wakeup" 后重截）\n'

exit 0
