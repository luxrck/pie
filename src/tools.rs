//! 工具层：内置 read / edit / write / shell + 统一的「Headers\n\nBody」输出格式。
//!
//! 对齐 Python 版 `src/pie/tools.py` 的行为契约：
//!   - 返回文本统一为 `Headers\n\nBody`（headers 一行一个 `[...]`；body 为空则省略空行）；
//!   - 私有参数（下划线开头）不进 schema，由配置按工具名注入；
//!   - read 截断**不落盘**（内容可再生，按 offset 续读）；shell 截断**落盘**（stdout 不可再生）；
//!   - shell 必须独立进程组 + killpg（否则取消/超时会被孙进程持有的管道卡住）。

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

pub type ToolResult = Result<String, ToolError>;

/// 工具失败：回给模型看的一句话（不是 panic）。
#[derive(Debug)]
pub struct ToolError(pub String);

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ToolError {}

fn err<T>(msg: impl Into<String>) -> Result<T, ToolError> {
    Err(ToolError(msg.into()))
}

/// 工具分发返回的 future：装箱，好让注册表按名字动态分发。
///
/// 没有借用参数（参数是 `Value`、`self` 按值传）→ 是 `'static`，生命周期一下子简单了。
pub type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

/// **一个工具 = 一个结构体**：字段就是参数，doc 注释就是参数描述，`required` 由
/// 「非 `Option` 即必填」推导——这些全由 `#[derive(Deserialize, JsonSchema)]` 包办。
///
/// 返回 `impl Future + Send` 而不是写成 `async fn`：trait 里的 `async fn` **表达不出 `Send`**，
/// 而注册表要把工具 future 装箱成 `dyn Future + Send`（TUI 要 spawn 整个回合）。
/// **impl 里仍然可以写 `async fn`**，`Send` 只在 trait 里声明一次。
//
// ---------------------------------------------------------------- 工具上下文

/// 工具执行上下文：目前只装**取消信号**（将来放实时进度回调——Python 版的 `_on_progress` 就是走这条链）。
///
/// 按值传（内部是 `Arc`，`Clone` 便宜）：这样工具 future 仍是 `'static`，注册表里的函数指针不用改生命周期。
#[derive(Clone, Default)]
pub struct ToolCtx {
    pub cancel: Option<crate::cancel::Cancel>,
}

impl ToolCtx {
    pub fn with_cancel(cancel: crate::cancel::Cancel) -> Self {
        Self {
            cancel: Some(cancel),
        }
    }
}

pub trait Tool: serde::de::DeserializeOwned + schemars::JsonSchema + Send + Sync + 'static {
    fn call(self, ctx: ToolCtx) -> impl std::future::Future<Output = ToolResult> + Send;
}

/// 泛型适配函数：每个工具实例化一份，取函数指针即完成类型擦除（**不经过 `dyn Tool`**）。
fn erased<T: Tool>(args: Value, ctx: ToolCtx) -> BoxFuture<ToolResult> {
    Box::pin(async move {
        let args: T =
            serde_json::from_value(args).map_err(|e| ToolError(format!("参数解析失败: {e}")))?;
        args.call(ctx).await
    })
}

/// schemars 的 schema → 与 Python 版一致的精简 schema，返回 `(工具描述, parameters)`。
///
/// 要归一化的都是**实测出来的**差异：
///   - `$schema` / `title` 删掉（描述本来就在根上，待会儿提到 `function.description` 去）；
///   - `Option<T>` 不要 `["integer","null"]`（`option_add_null_type = false`）；
///   - `format: "int64"` 这类装饰字段删掉；
///   - 嵌套类型必须内联（`inline_subschemas = true`），否则出 `$ref` + `definitions`。
fn schema_of<T: schemars::JsonSchema>() -> (String, Value) {
    let mut settings = schemars::gen::SchemaSettings::draft07();
    settings.meta_schema = None;
    settings.inline_subschemas = true;
    settings.option_add_null_type = false;
    let mut schema = serde_json::to_value(
        schemars::gen::SchemaGenerator::new(settings)
            .root_schema_for::<T>()
            .schema,
    )
    .unwrap_or(Value::Null);
    strip_schema_noise(&mut schema);
    let description = schema
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or_default()
        .to_string();
    if let Some(obj) = schema.as_object_mut() {
        obj.remove("description");
        obj.remove("title");
    }
    (description, schema)
}

/// 递归剔掉 schemars 挂上去、而 Python 版没有的装饰字段：
///   - `format`（`int64` / `uint` …）；
///   - `minimum`：无符号整数会被自动加上 `minimum: 0`，而 Python 版没有；而且 `read` 的 `offset`
///     实际从 1 起（0 会被工具回错），写 0 反而误导。
fn strip_schema_noise(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("format");
            map.remove("minimum");
            map.values_mut().for_each(strip_schema_noise);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_schema_noise),
        _ => {}
    }
}

// ---------------------------------------------------------------- 跨工具共用
//
// 模块级只留这一件小事（四个工具都以它收尾）。其余辅助逻辑（图片嗅探 / 字节预算 / 头部截断 /
// 全文落盘 / 命令构造 / edit 诊断）一律并进对应工具的 `call`。

/// 统一「Headers\n\nBody」：headers 一行一个 `[...]`；body 非空时用空行分隔。
///
/// 四个工具都用它 → 留在这儿，「输出格式只有一处定义」。
/// 没有头区就直接给正文（`bash` 成功时就是这种）——否则会以空串 join 出一个多余的空行。
fn format_output(headers: &[String], body: &str) -> String {
    if headers.is_empty() {
        return body.to_string();
    }
    let head = headers.join("\n");
    if body.is_empty() {
        head
    } else {
        format!("{head}\n\n{body}")
    }
}

// ---------------------------------------------------------------- 工具实现
//
// 每个工具**自包含**：只服务它自己的逻辑都写在各自的 `impl Tool` 里，不再往模块级抛小函数。

/// 读取文件内容：文本按 UTF-8 全文或 offset（1 起）/ limit 分页读取。
// 结构体即参数：doc 注释 → 参数描述，非 `Option` 即必填，`#[schemars(skip)]` 的是私有参数。
// ⚠ 结构体 doc 的**首行**就是发给模型的工具描述（schemars 把 doc 挂到 schema 根上），
// 所以额外的说明必须写成普通注释，否则会被拼进 description。
#[derive(Deserialize, JsonSchema)]
pub struct Read {
    /// Path to the file to read (relative or absolute)
    pub path: String,
    /// Line number to start reading from (1-indexed); text files only, ignored for images
    pub offset: Option<usize>,
    /// Maximum number of lines to read; text files only, ignored for images
    pub limit: Option<usize>,
    /// 私有参数：由配置注入，不进 schema（容量/行号都是非负量 → 直接用 `usize`）
    #[schemars(skip)]
    pub _max_lines: Option<usize>,
    #[schemars(skip)]
    pub _max_bytes: Option<usize>,
    #[schemars(skip)]
    pub _max_image_bytes: Option<usize>,
}

impl Tool for Read {
    async fn call(self, _ctx: ToolCtx) -> ToolResult {
        // 嗅探只需文件开头这一小段（魔数 + 文件头里的尺寸，不解码全图）
        const READ_HEAD_BYTES: usize = 65_536;

        let Self {
            path,
            offset,
            limit,
            _max_lines,
            _max_bytes,
            _max_image_bytes,
        } = self;
        let p = Path::new(&path);
        if !p.exists() {
            return err(format!("文件不存在: {path}"));
        }
        let head = match std::fs::File::open(p).and_then(|mut f| {
            use std::io::Read;
            let mut buf = vec![0u8; READ_HEAD_BYTES];
            let n = f.read(&mut buf)?;
            buf.truncate(n);
            Ok(buf)
        }) {
            Ok(h) => h,
            Err(e) => return err(format!("无法读取 {path}: {e}")),
        };

        // —— 图片识别：魔数嗅探 + 读头部拿尺寸（**不解码全图**）——
        // 就地写在 call 里：只有 read 用得上（原来是模块级的 `probe_image`）。
        // 用 `image` crate 而不是手写解析——PNG/GIF/BMP 的头、JPEG 的 SOF 段扫描、WebP 的
        // VP8X/VP8/VP8L 三个变体，手写一遍就是几十行容易悄悄出错的位运算（progressive JPEG、
        // 带 EXIF 的 JPEG 都是坑）。实测两者在 Pillow 生成的真图上结果完全一致。
        // ⚠ `guess_format` 的魔数表比这里宽：**即使没开 tiff/ico 的 feature，它照样认出
        // TIFF/ICO**（只是解码器不可用）。所以必须过白名单——Python 版只把
        // PNG/JPEG/GIF/WebP/BMP 当图片，其余的仍走文本/二进制分支。
        let image_info = image::guess_format(&head).ok().and_then(|format| {
            let mime = match format {
                image::ImageFormat::Png => "image/png",
                image::ImageFormat::Jpeg => "image/jpeg",
                image::ImageFormat::Gif => "image/gif",
                image::ImageFormat::WebP => "image/webp",
                image::ImageFormat::Bmp => "image/bmp",
                _ => return None, // 魔数认得但不在白名单（TIFF/ICO…）→ 当文本/二进制文件
            };
            // 只读头部、不分配整张图；读不出来也只是少个 dim（尺寸仅供描述，不因它报错）
            let dim =
                image::ImageReader::with_format(std::io::Cursor::new(head.as_slice()), format)
                    .into_dimensions()
                    .ok()
                    .map(|(w, h)| (u64::from(w), u64::from(h)));
            Some((mime, dim))
        });
        if let Some((mime, dim)) = image_info {
            let size = p.metadata().map(|m| m.len()).unwrap_or(0);
            if let Some(max) = _max_image_bytes {
                if max > 0 && size > max as u64 {
                    return err(format!(
                        "图片过大（{size} 字节 > {max} 上限），无法内联发送给模型；请先压缩/裁剪该图片再读取"
                    ));
                }
            }
            let dim_txt = dim
                .map(|(w, h)| format!(", dim={w}x{h}"))
                .unwrap_or_default();
            return Ok(format!(
                "[图片已读取: path={path}, mime={mime}, size={size}{dim_txt}]"
            ));
        }

        let content = match std::fs::read_to_string(p) {
            Ok(c) => c,
            Err(_) => {
                let size = p.metadata().map(|m| m.len()).unwrap_or(0);
                return Ok(format!("[二进制文件，大小 {size} 字节，无法按文本读取]"));
            }
        };
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        if let Some(o) = offset {
            if o == 0 || o > total {
                return err(format!("offset 无效: {o}（文件共 {total} 行）"));
            }
        }
        if let Some(l) = limit {
            if l == 0 {
                return err(format!("limit 无效: {l}（必须为正整数）"));
            }
        }
        for (label, v) in [("_max_lines", _max_lines), ("_max_bytes", _max_bytes)] {
            if let Some(v) = v {
                if v == 0 {
                    return err(format!("{label} 无效: {v}（必须为正整数或 None）"));
                }
            }
        }
        let start = offset.unwrap_or(1).saturating_sub(1);
        let mut want = total - start;
        if let Some(l) = limit {
            want = want.min(l);
        }
        // 字节预算：从 start 起累计「行 + 1（换行）」字节不超过 `_max_bytes` 的行数（至少 1 行，
        // 保证单行超长也读得到）。这些行**不含**换行（`lines()` 已经去掉了），所以每行要 +1 才对得上
        // 磁盘上的实际大小——与 shell 那侧不同（它的行含 `\n`，见 `Shell::call`；Python 版两边也是这么分的）。
        let lines_by_bytes = |start: usize, max_bytes: usize| -> usize {
            let mut n = 0usize;
            let mut sz = 0usize;
            for line in lines.iter().skip(start) {
                let b = line.len() + 1;
                if sz + b > max_bytes && n > 0 {
                    break;
                }
                sz += b;
                n += 1;
            }
            n
        };
        let mut cap = want;
        if let Some(m) = _max_lines {
            cap = cap.min(m);
        }
        if let Some(m) = _max_bytes {
            cap = cap.min(lines_by_bytes(start, m));
        }
        let picked = &lines[start..(start + cap).min(lines.len())];
        let omitted = want - picked.len();

        let body = picked.join("\n");
        let mut headers = vec![format!(
            "[行 {}-{}，共 {total} 行]",
            start + 1,
            start + picked.len()
        )];
        if omitted > 0 {
            // 不落盘：read 的内容可再生（原文件还在），续读拿到的是完整内容。
            headers.push(format!(
                "[已截断：可用 offset={} 继续读]",
                start + picked.len() + 1
            ));
        }
        Ok(format_output(&headers, &body))
    }
}

/// `read` 读到的图片引用（从返回文本里的机器可读标记解析而来）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub path: String,
    pub mime: String,
    /// 原始字节数（用于跟磁盘上的文件对一下，看有没有变）。
    pub size: u64,
    pub width: Option<u64>,
    pub height: Option<u64>,
}

/// 从 `read` 的返回文本解析图片标记；非图片结果返回 None。
///
/// 标记是 read 自己写的（见 `Read::call`）：
/// `[图片已读取: path=<路径>, mime=<mime>, size=<字节>[, dim=<宽>x<高>]]`
/// 路径里不会出现 `,` 或 `]`（Python 的正则也是这么排的），所以手写解析足够，不用引 regex。
pub fn parse_image_marker(text: &str) -> Option<ImageRef> {
    const MARK: &str = "[图片已读取: ";
    let start = text.find(MARK)? + MARK.len();
    let rest = &text[start..];
    let end = rest.find(']')?;
    let mut path: Option<String> = None;
    let mut mime: Option<String> = None;
    let mut size: Option<u64> = None;
    let mut dim: Option<(u64, u64)> = None;
    for field in rest[..end].split(", ") {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        match key {
            "path" => path = Some(value.to_string()),
            "mime" => mime = Some(value.to_string()),
            "size" => size = value.parse::<u64>().ok(),
            "dim" => {
                if let Some((w, h)) = value.split_once('x') {
                    if let (Ok(w), Ok(h)) = (w.parse::<u64>(), h.parse::<u64>()) {
                        dim = Some((w, h));
                    }
                }
            }
            _ => {}
        }
    }
    Some(ImageRef {
        path: path?,
        mime: mime?,
        size: size?,
        width: dim.map(|d| d.0),
        height: dim.map(|d| d.1),
    })
}

// ⚠ 工具描述（`function.description`）**不能**只靠 doc 注释：schemars 会把 doc 里的单换行
// 合并成空格（`merge_description_lines`），而 Python 版这里是多行的 —— 用下面的
// `#[schemars(description = ...)]` 显式给。
#[derive(Deserialize, JsonSchema)]
#[schemars(
    description = "一次调用做多个精确替换：每个 oldText 必须**唯一**且互不重叠，且都按**同一份原文**匹配（不是逐条叠加）。\n编辑纪律（不做很容易白花一次往返）：\n- oldText 从刚 read 到的内容里**整段复制**（含缩进与空行）；\n- 同一次调用的多条 edits **互不依赖**：不能引用另一条 edit 的新文本（要级联就分两次调用）；\n- 任何一条报错，**整次调用什么都不写入**（原子）→ 重新 read 再改，不要接着用旧内容；\n- 改动较大或相邻，就用**一条** edit 覆盖整块，不要拆成多条挨着的 edits。"
)]
pub struct Edit {
    /// Path to the file to edit (relative or absolute)
    pub path: String,
    /// One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.
    pub edits: Vec<EditOp>,
}

// `edits` 数组的元素。**故意不加 doc 注释**：Python 版的 `items` 没有 description，
// 而 schemars 会把结构体 doc 填上去（加了就对不上）。
#[derive(Deserialize, JsonSchema)]
#[allow(non_snake_case)]
pub struct EditOp {
    /// Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call.
    pub oldText: String,
    /// Replacement text for this targeted edit.
    pub newText: String,
}

impl Tool for Edit {
    async fn call(self, _ctx: ToolCtx) -> ToolResult {
        let Self { path, edits } = self;
        let p = Path::new(&path);
        if !p.exists() {
            return err(format!("文件不存在: {path}"));
        }
        let content = match std::fs::read_to_string(p) {
            Ok(c) => c,
            Err(e) => return err(format!("无法读取 {path}: {e}")),
        };
        if edits.is_empty() {
            return err("edits 不能为空");
        }

        // 列表里回显的片段最多 120 字（超长的报错会把工具结果撑大）
        let clip = |s: &str| -> String {
            let t: String = s.chars().take(120).collect();
            if s.chars().count() > 120 {
                format!("{t}…")
            } else {
                t
            }
        };

        // —— 诊断：oldText 找不到时，给出「原文里最接近的位置 + 简版 diff」——
        // 拿 oldText 里最长的一行当锚（最可能是唯一标识），在原文里找字符级最相似的一行，
        // 再以它为基准取**同长窗口**逐行对齐输出。锚行相似度 < 50% 就不给（宁可不给，不给误导）。
        // 简版之处：用 LCS 近似 Python 的 `difflib.ratio()`，且窗口与 oldText 等长（不处理行
        // 插入/删除引起的位移，那种情况会多出几条 -/+ 而已）。
        let lcs_len = |a: &[char], b: &[char]| -> usize {
            let mut prev = vec![0usize; b.len() + 1];
            let mut cur = vec![0usize; b.len() + 1];
            for &ca in a {
                for (j, &cb) in b.iter().enumerate() {
                    cur[j + 1] = if ca == cb {
                        prev[j] + 1
                    } else {
                        cur[j].max(prev[j + 1])
                    };
                }
                std::mem::swap(&mut prev, &mut cur);
            }
            prev[b.len()]
        };
        let nearest_fragment = |content: &str, old: &str| -> String {
            let old_lines: Vec<&str> = old.lines().collect();
            let c_lines: Vec<&str> = content.lines().collect();
            if old_lines.is_empty() || c_lines.is_empty() {
                return String::new();
            }
            let anchor_idx = (0..old_lines.len())
                .max_by_key(|&k| old_lines[k].trim().chars().count())
                .unwrap_or(0);
            let anchor: Vec<char> = old_lines[anchor_idx].trim().chars().collect();
            if anchor.is_empty() {
                return String::new();
            }
            let mut best: Option<(usize, f64)> = None;
            for (i, line) in c_lines.iter().enumerate() {
                let cand: Vec<char> = line.trim().chars().collect();
                let total = anchor.len() + cand.len();
                let r = if total == 0 {
                    1.0
                } else {
                    2.0 * lcs_len(&anchor, &cand) as f64 / total as f64
                };
                if best.is_none_or(|(_, br)| r > br) {
                    best = Some((i, r));
                }
            }
            let Some((best_i, best_r)) = best else {
                return String::new();
            };
            if best_r < 0.5 {
                return String::new();
            }
            let start = best_i.saturating_sub(anchor_idx);
            let mut diff: Vec<String> = vec![
                "--- 你的 oldText".to_string(),
                format!("+++ 原文实际内容（第 {} 行起）", start + 1),
            ];
            for (k, old_line) in old_lines.iter().enumerate() {
                match c_lines.get(start + k) {
                    Some(new_line) if new_line == old_line => diff.push(format!(" {old_line}")),
                    Some(new_line) => {
                        diff.push(format!("-{old_line}"));
                        diff.push(format!("+{new_line}"));
                    }
                    None => diff.push(format!("-{old_line}")),
                }
            }
            const MAX_DIFF_LINES: usize = 16;
            let body = diff
                .iter()
                .take(MAX_DIFF_LINES)
                .map(|l| format!("    {l}"))
                .collect::<Vec<_>>()
                .join("\n");
            let more = if diff.len() > MAX_DIFF_LINES {
                format!("\n    …（diff 已截断，共 {} 行）", diff.len())
            } else {
                String::new()
            };
            format!(
                "原文里最接近的位置在第 {} 行（锚行相似度 {:.0}%）：\n{body}{more}",
                start + 1,
                best_r * 100.0
            )
        };

        let mut replacements: Vec<(usize, usize, String)> = Vec::new(); // (start, end, new)
        let mut labels: Vec<String> = Vec::new();
        let mut problems: Vec<String> = Vec::new();

        for (i, e) in edits.iter().enumerate() {
            let old = e.oldText.as_str();
            let new = e.newText.as_str();
            if old.is_empty() {
                problems.push(format!("edits[{i}].oldText 必须是非空字符串"));
                continue;
            }
            let matched = content.matches(old).count();
            if matched == 0 {
                let mut msg = vec![format!(
                    "edits[{i}].oldText 在 {path} 中找不到（区分大小写）：{:?}",
                    clip(old)
                )];
                // 诊断 1：引用了同一次调用里另一条 edit 的产物
                for (j, other) in edits.iter().enumerate() {
                    if j == i {
                        continue;
                    }
                    let other_new = other.newText.as_str();
                    if !other_new.is_empty() && other_new.contains(old) {
                        msg.push(format!(
                            "    提示：这段文本出现在 edits[{j}].newText 里 —— edits 是对**原文**一次性应用的，\
                             不能引用同一次调用中另一条 edit 产生的文本；请拆成两次调用。"
                        ));
                        break;
                    }
                }
                // 诊断 2：只差空白 / 缩进 / 换行（两边都去掉所有空白再比）
                let norm =
                    |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
                let (old_norm, content_norm) = (norm(old), norm(&content));
                if !old_norm.is_empty() && content_norm.contains(&old_norm) {
                    msg.push(
                        "    提示：忽略空白/换行后能匹配上 → 多半是缩进或空行与原文不完全一致（请从刚读到的内容里整段复制）。"
                            .to_string(),
                    );
                }
                // 诊断 3：原文里最接近的位置 + 简版 diff（一眼看出差在哪，不必再 read 一遍）
                let hint = nearest_fragment(&content, old);
                if !hint.is_empty() {
                    msg.push(format!("    {hint}"));
                }
                problems.push(msg.join("\n"));
                continue;
            }
            if matched > 1 {
                problems.push(format!(
                    "edits[{i}].oldText 在 {path} 中出现 {matched} 次，必须唯一：{:?}\n    \
                     提示：把 oldText 加长到能唯一确定位置（多带几行上下文），或与相邻改动合并成一个 edit。",
                    clip(old)
                ));
                continue;
            }
            let start = content.find(old).expect("matched>0");
            replacements.push((start, start + old.len(), new.to_string()));
            labels.push(format!("edits[{i}]"));
        }

        let mut order: Vec<usize> = (0..replacements.len()).collect();
        order.sort_by_key(|&k| replacements[k].0);
        for pair in order.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if replacements[b].0 < replacements[a].1 {
                problems.push(format!(
                    "edits 存在重叠：{} 与 {}（相邻/重叠改动请合并成一个 edit）",
                    labels[a], labels[b]
                ));
            }
        }

        if !problems.is_empty() {
            return err(format!(
                "edits 有 {} 处问题，**未写入任何内容**（本工具的 edits 对原文一次性应用）：\n{}",
                problems.len(),
                problems
                    .iter()
                    .map(|m| format!("- {m}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        let mut out = String::with_capacity(content.len());
        let mut pos = 0usize;
        for k in order {
            let (start, end, new) = &replacements[k];
            out.push_str(&content[pos..*start]);
            out.push_str(new);
            pos = *end;
        }
        out.push_str(&content[pos..]);
        if let Err(e) = std::fs::write(p, out) {
            return err(format!("写入失败 {path}: {e}"));
        }
        Ok(format_output(
            &[format!("[已替换 {} 处: {path}]", edits.len())],
            "",
        ))
    }
}

/// 把 content 写入 path，覆盖已有内容并自动创建父目录。
#[derive(Deserialize, JsonSchema)]
pub struct Writ {
    /// Path to the file to write (relative or absolute)
    pub path: String,
    /// Content to write to the file
    pub content: String,
}

impl Tool for Writ {
    async fn call(self, _ctx: ToolCtx) -> ToolResult {
        let Self { path, content } = self;
        let p = Path::new(&path);
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return err(format!("创建目录失败 {}: {e}", parent.display()));
                }
            }
        }
        match std::fs::write(p, &content) {
            Ok(()) => Ok(format_output(
                &[format!(
                    "[已写入 {path}（{} 字符，{} 行）]",
                    content.chars().count(),
                    content.matches('\n').count() + 1
                )],
                "",
            )),
            Err(e) => err(format!("写入失败 {path}: {e}")),
        }
    }
}

/// 执行 shell 命令，返回 stdout/stderr 与退出码（YOLO，无权限确认）。
#[derive(Deserialize, JsonSchema)]
pub struct Bash {
    /// Shell command to execute
    pub command: String,
    /// Timeout in seconds (optional, no default timeout)
    pub timeout: Option<i64>,
    /// 私有参数：由配置注入，不进 schema
    #[schemars(skip)]
    pub _max_lines: Option<i64>,
    #[schemars(skip)]
    pub _max_bytes: Option<i64>,
    /// 私有参数：受限模式（`--tools` 给了子命令白名单）时注入，command 首词必须命中
    #[schemars(skip)]
    pub _allow_cmds: Option<Vec<String>>,
}

impl Bash {
    /// 本工具实际用的 shell：Unix 是 `bash -c`，Windows 是 `cmd /C`。
    ///
    /// ⚠ **单一事实来源**：`call` 里起进程用它——不再各写一份字面量（写岔了就会对不上）。
    #[cfg(unix)]
    const SHELL: &'static str = "bash";
    #[cfg(not(unix))]
    const SHELL: &'static str = "cmd";

    /// 运行 shell 时传的选项：`bash -c <cmd>` / `cmd /C <cmd>`。
    #[cfg(unix)]
    const SHELL_FLAG: &'static str = "-c";
    #[cfg(not(unix))]
    const SHELL_FLAG: &'static str = "/C";
}

impl Tool for Bash {
    async fn call(self, ctx: ToolCtx) -> ToolResult {
        let Self {
            command,
            timeout,
            _max_lines,
            _max_bytes,
            _allow_cmds,
        } = self;
        // 受限模式（`--tools` 给了子命令白名单）：首词不在白名单里就直接回错，不启动进程
        if let Some(allow) = &_allow_cmds {
            let first = command.split_whitespace().next().unwrap_or("");
            if !allow.iter().any(|c| c == first) {
                let got = if first.is_empty() {
                    "(空命令)"
                } else {
                    first
                };
                return err(format!(
                    "[bash] 本次仅允许以这些命令开头: {}（收到: {got}）",
                    allow.join(", ")
                ));
            }
        }
        // 命令交给 `bash -c`（Windows 是 `cmd /C`）——名字与选项只住在上面的两个常量里，
        // 所以两个平台共用这一段（不再各写一份字面量）。（⚠ 与 Python 版不同：那边是 `sh -c`。）
        let mut cmd = {
            let mut c = tokio::process::Command::new(Self::SHELL);
            c.arg(Self::SHELL_FLAG).arg(&command);
            c
        };
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            cmd.process_group(0); // 独立进程组：取消/超时时 killpg 连子孙一起杀
                                  // stderr 合并进 stdout，保证输出顺序稳定（与 Python 版一致）
            unsafe {
                cmd.pre_exec(|| {
                    if libc::dup2(1, 2) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return err(format!("启动失败: {e}")),
        };
        let stdout = child.stdout.take().expect("piped");
        // tokio 的 id() 在进程已被回收后返回 None；0 表示“拿不到 pid”（不杀组）
        let pid = child.id().unwrap_or(0);
        let mut chunks: Vec<String> = Vec::new();

        // 逐块读 stdout（stderr 已 dup2 到同一管道）——用 read_until 保留原始字节，
        // 不做行重整（Python 版同样是「原样累积」，只在进度回调里剥掉换行）。
        let read_and_wait = async {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(stdout);
            let mut buf: Vec<u8> = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let text = String::from_utf8_lossy(&buf).into_owned();
                chunks.push(text);
            }
            child.wait().await
        };

        // 取消（Esc）：与超时同款处理——杀掉**整个进程组**，返回哨兵文本
        // （上层看到 `CANCEL_TEXT` 就收尾：补全未执行的 tool 消息 + 写一条终止 assistant 消息）。
        let cancel_waiter = async {
            match ctx.cancel.clone() {
                Some(cancel) => cancel.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(cancel_waiter);
        let kill_group = || {
            #[cfg(unix)]
            if pid != 0 {
                unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
            }
        };

        let status = match timeout {
            Some(t) if t > 0 => {
                let dur = std::time::Duration::from_secs(t as u64);
                tokio::select! {
                    waited = tokio::time::timeout(dur, read_and_wait) => match waited {
                        Ok(Ok(st)) => st,
                        Ok(Err(e)) => return err(format!("等待子进程失败: {e}")),
                        Err(_) => {
                            // 超时：杀**整个进程组**。只 kill 本体杀不掉持有管道写端的子孙进程，
                            // 而管道不 EOF 就会把等待卡到子孙自然退出（实测能卡满 timeout）。
                            kill_group();
                            return Ok(format!(
                                "[bash] 命令超过 {t}s 超时，可能仍在后台运行：{command}"
                            ));
                        }
                    },
                    _ = &mut cancel_waiter => {
                        kill_group();
                        return Ok(crate::cancel::CANCEL_TEXT.to_string());
                    }
                }
            }
            _ => tokio::select! {
                waited = read_and_wait => match waited {
                    Ok(st) => st,
                    Err(e) => return err(format!("等待子进程失败: {e}")),
                },
                _ = &mut cancel_waiter => {
                    kill_group();
                    return Ok(crate::cancel::CANCEL_TEXT.to_string());
                }
            },
        };

        // 退出码：被信号杀死时 Python 给负数，这里取 -1（信息量等价，都是「非正常退出」）
        let code = status.code().unwrap_or(-1);
        let out: String = chunks.concat();

        // —— 超限只保留**开头**连续段（未超限用原文，也不落盘）——
        // 取头部而不是尾部：`cat 大文件` 这类命令，开头才是你要看的那部分；被截掉的尾巴
        // 由下面的 `[工具输出全文已保存: …]` 指针给出。
        let head = if _max_lines.is_none() && _max_bytes.is_none() {
            None
        } else {
            let lines: Vec<&str> = out.split_inclusive('\n').collect();
            let mut cap = lines.len();
            if let Some(m) = _max_lines {
                cap = cap.min(m.max(0) as usize);
            }
            if let Some(m) = _max_bytes {
                // 字节预算：这些行**已含** `\n`，按实际字节累计即可（Python 的 `_tail_output` 同口径；
                // `read` 那边行不含换行，所以是 len+1）
                let mut sz: i64 = 0;
                let mut n = 0usize;
                for line in &lines {
                    let b = line.len() as i64;
                    if sz + b > m && n > 0 {
                        break;
                    }
                    sz += b;
                    n += 1;
                }
                cap = cap.min(n);
            }
            (cap < lines.len()).then(|| lines[..cap].concat())
        };

        // 退出码头：**只在非 0 时给**（成功就是成功，不给模型/前端添噪声）。要它的地方是
        // 「判成败」：Rust TUI 的 `history::tool_result_ok` 只看**第一行**，所以失败必须
        // 从这行就能认出来——判据是「`[exit=` 开头且不是 `[exit=0`」= 失败（没有任何头 = 成功）。
        // 三个字段挤在一行（2026-09-24 起）：这行只为「判成败」存在，一行就够，也少两行噪声。
        let mut headers: Vec<String> = Vec::new();
        if code != 0 {
            headers.push(format!(
                "[exit={code}, os={}, shell={}]",
                std::env::consts::OS,
                Self::SHELL
            ));
        }

        match head {
            None => Ok(format_output(&headers, &out)),
            Some(head) => {
                // shell 的 stdout 不可再生（进程结束就没了）→ 全文落盘 + 独立指针，指针是取回
                // 被截掉那部分的唯一途径。落盘走 context::write_raw（内容 hash 寻址，按内容去重）。
                let spill = crate::context::write_raw(&out, "bash")
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|e| format!("(落盘失败: {e})"));
                headers.push(format!("[工具输出全文已保存: {spill}]"));
                Ok(format_output(
                    &headers,
                    &head,
                ))
            }
        }
    }
}

// ---------------------------------------------------------------- 注册表

/// 动态注册的调用体：内置工具包的是 `erased::<T>`（fn 指针），**Python 工具**包的是
/// 一段回调（绑定侧往 Python 里再叫一次）。两条路共用同一条分发链。
pub type CallFn = std::sync::Arc<dyn Fn(Value, ToolCtx) -> BoxFuture<ToolResult> + Send + Sync>;

/// 注册表里的一条：名字 + 描述 + schema + 「JSON → 调用」的调用体（类型擦除的产物）。
///
/// `Clone` 是给嵌入方用的（Python 绑定要能拿一份副本建会话，见 `bindings/pie-py`）；
/// `Debug` 手写（`Box<dyn Fn>` 不是 `Debug`，打印成 `<call>` 就够）。
#[derive(Clone)]
pub struct Entry {
    /// 工具名（`String`：Python 工具的名字是运行时给的，不是 `&'static str`）。
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub call: CallFn,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("parameters", &self.parameters)
            .field("call", &"<call>")
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct ToolRegistry {
    entries: Vec<Entry>,
    defaults: HashMap<String, toml::Table>,
}

impl ToolRegistry {
    /// 空注册表，用 `.with_tool::<T>("名字")` 往里加。
    pub fn empty(defaults: HashMap<String, toml::Table>) -> Self {
        Self {
            entries: Vec::new(),
            defaults,
        }
    }

    /// **注册一个工具**：`.with_tool::<Read>("read")`。
    ///
    /// 传的是**类型**而不是值——因为「结构体即参数」，带字段的结构体在 Rust 里根本不是
    /// 一个值表达式（`Read` 这三个字写不出来）。名字手动给，和 Python 版一样显式。
    pub fn with_tool<T: Tool>(mut self, name: &str) -> Self {
        assert!(
            !self.entries.iter().any(|e| e.name == name),
            "工具已存在: {name}"
        );
        let (description, parameters) = schema_of::<T>();
        self.entries.push(Entry {
            name: name.to_string(),
            description,
            parameters,
            call: std::sync::Arc::new(erased::<T>),
        });
        self
    }

    /// **动态注册**（Python 工具走这条）：名字 / 描述 / schema 都是运行时给的，
    /// 调用体由调用方提供（绑定里就是「往 Python 里叫一次」那段）。
    ///
    /// 与 `with_tool` 同规矩：重名直接 panic（注册期编程错误，不是运行期数据问题）。
    pub fn with_dynamic(
        mut self,
        name: &str,
        description: &str,
        parameters: Value,
        call: CallFn,
    ) -> Self {
        assert!(
            !self.entries.iter().any(|e| e.name == name),
            "工具已存在: {name}"
        );
        self.entries.push(Entry {
            name: name.to_string(),
            description: description.to_string(),
            parameters,
            call,
        });
        self
    }

    /// 全量内置工具（对应 Python 版的 `register_builtins`）。
    pub fn new(defaults: HashMap<String, toml::Table>) -> Self {
        Self::empty(defaults)
            .with_tool::<Read>("read")
            .with_tool::<Edit>("edit")
            .with_tool::<Writ>("writ")
            .with_tool::<Bash>("bash")
    }

    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// 发给模型的 `tools` 数组（OpenAI 线上形状：`{"type": "function", "function": …}`）。
    ///
    /// 形状知识就放在这儿（与 Python 版一致：`Tool.definition()` 也是工具层自己出的），
    /// llm 层只管把整个数组塞进请求体的 `tools` 键。
    pub fn specs(&self) -> Vec<Value> {
        self.entries
            .iter()
            .map(|e| {
                json!({
                    "type": "function",
                    "function": {
                        "name": e.name,
                        "description": e.description,
                        "parameters": e.parameters,
                    }
                })
            })
            .collect()
    }

    /// 注入私有参数：配置里 `_` 开头的键塞进 args，由 serde 自己挑对应字段——
    /// 结构体里没有的键会被忽略，所以不需要再维护一张「私有参数名单」。
    pub fn inject_defaults(&self, name: &str, args: &mut Value) {
        let Some(values) = self.defaults.get(name) else {
            return;
        };
        let Some(map) = args.as_object_mut() else {
            return;
        };
        for (key, value) in values {
            if key.starts_with('_') && !map.contains_key(key) {
                map.insert(key.clone(), toml_to_json(value));
            }
        }
    }

    /// 按名字分发（解析 + 调用都在工具自己那边，注册表只管找）。
    pub async fn dispatch(&self, name: &str, args: &Value, ctx: ToolCtx) -> ToolResult {
        let Some(entry) = self.entries.iter().find(|e| e.name.as_str() == name) else {
            let names: Vec<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
            return err(format!("未知工具: {name}（可用: {}）", names.join(", ")));
        };
        let mut args = args.clone();
        self.inject_defaults(name, &mut args);
        (entry.call)(args, ctx).await
    }
}

/// 按 `--tools` 说明构建可用工具集（无 spec / 空串 → 默认全量）。
///
/// 解析优先级：内置工具名 > shell 子命令。
///   - 名字 ∈ 内置（read/edit/writ/bash）→ 启用该工具；
///   - 其他名字 → 收集成 shell 允许的子命令白名单，并隐式启用**受限 shell**；
///   - 未显式列 shell 且没有任何非内置名 → shell 工具禁用。
///
/// 示例：`"read"` → 仅 read；`"read,bash"` → read + 不限子命令的 bash；
/// `"read,ls,grep"` → read + 只允许 ls/grep 的受限 bash；`"ls,grep"` → 仅受限 bash。
pub fn tools_from_spec(spec: Option<&str>, defaults: HashMap<String, toml::Table>) -> ToolRegistry {
    const BUILTINS: [&str; 4] = ["read", "edit", "writ", "bash"];

    let mut enabled: Vec<&'static str> = Vec::new();
    let mut allow_cmds: Vec<String> = Vec::new();
    for token in spec.unwrap_or("").split(',').map(str::trim) {
        if token.is_empty() {
            continue;
        }
        match BUILTINS.iter().find(|b| **b == token) {
            Some(&name) => {
                if !enabled.contains(&name) {
                    enabled.push(name);
                }
            }
            None => {
                if !allow_cmds.iter().any(|c| c == token) {
                    allow_cmds.push(token.to_string());
                }
            }
        }
    }

    // 无 spec、或「四个内置全列且无白名单」→ 默认全量（与 Python 一致）
    if (enabled.is_empty() && allow_cmds.is_empty())
        || (allow_cmds.is_empty() && enabled.len() == BUILTINS.len())
    {
        return ToolRegistry::new(defaults);
    }

    let restricted = !allow_cmds.is_empty();
    let mut defaults = defaults;
    if restricted {
        // 白名单走「私有参数注入」这条路：Shell 里有 `_allow_cmds` 字段（→ `#[schemars(skip)]`，不进 schema）
        let mut table = defaults.remove("bash").unwrap_or_default();
        table.insert(
            "_allow_cmds".to_string(),
            toml::Value::Array(
                allow_cmds
                    .iter()
                    .cloned()
                    .map(toml::Value::String)
                    .collect(),
            ),
        );
        defaults.insert("bash".to_string(), table);
    }

    let mut reg = ToolRegistry::empty(defaults);
    for name in BUILTINS {
        if !(enabled.contains(&name) || (name == "bash" && restricted)) {
            continue;
        }
        reg = match name {
            "read" => reg.with_tool::<Read>("read"),
            "edit" => reg.with_tool::<Edit>("edit"),
            "writ" => reg.with_tool::<Writ>("writ"),
            _ => reg.with_tool::<Bash>("bash"),
        };
    }
    if restricted {
        // description 追加白名单，让模型事先知道边界、少试错（Python 同款）
        let allow_txt = allow_cmds.join(", ");
        if let Some(entry) = reg.entries.last_mut() {
            if entry.name == "bash" {
                entry
                    .description
                    .push_str(&format!("\n本次运行仅允许以这些命令开头: {allow_txt}"));
            }
        }
    }
    reg
}

fn toml_to_json(v: &toml::Value) -> Value {
    match v {
        toml::Value::Integer(i) => json!(i),
        toml::Value::Float(f) => json!(f),
        toml::Value::String(s) => json!(s),
        toml::Value::Boolean(b) => json!(b),
        toml::Value::Array(items) => Value::Array(items.iter().map(toml_to_json).collect()),
        other => json!(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("pie-rs-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn builtin() -> ToolRegistry {
        ToolRegistry::new(HashMap::new())
    }

    /// bash 失败时的唯一一行头（`impl Bash` 里那行格式的**测试侧镜像**）：
    /// os / shell 跟着平台走，所以按常量拼期望值，别把 `linux` / `bash` 写死。
    fn fail_header(code: i32) -> String {
        format!(
            "[exit={code}, os={}, shell={}]",
            std::env::consts::OS,
            Bash::SHELL
        )
    }

    #[test]
    fn parses_image_marker() {
        let text = "[图片已读取: path=/tmp/a b.png, mime=image/png, size=1234, dim=13x7]";
        let r = parse_image_marker(text).expect("应能解析");
        assert_eq!(r.path, "/tmp/a b.png"); // 路径里的空格不影响
        assert_eq!(r.mime, "image/png");
        assert_eq!(r.size, 1234);
        assert_eq!((r.width, r.height), (Some(13), Some(7)));
        // 没有 dim（读不出尺寸时）也能解
        let r = parse_image_marker("[图片已读取: path=x.jpg, mime=image/jpeg, size=9]").unwrap();
        assert_eq!((r.width, r.height), (None, None));
        // 非图片结果 / 文本里只是提到这个形状 → None
        assert!(parse_image_marker("[行 1-2，共 5 行]\n\nab").is_none());
        assert!(parse_image_marker("代码里写着 [图片已读取: 但没写全]").is_none());
    }

    #[test]
    fn format_output_omits_blank_line_when_body_empty() {
        assert_eq!(format_output(&["[exit=0]".into()], ""), "[exit=0]");
        assert_eq!(format_output(&["[exit=0]".into()], "hi"), "[exit=0]\n\nhi");
        // 没有头区 → 直接给正文（`bash` 成功时就是这种；别以空串 join 出多余空行）
        assert_eq!(format_output(&[], "hi"), "hi");
        assert_eq!(format_output(&[], ""), "");
    }

    /// 图片识别走 **read 的公开路径**（原 `probe_image` 已并进 `Read::call`，没有可单测的内部函数了）。
    #[tokio::test]
    async fn read_recognizes_real_image_formats() {
        use image::{ImageFormat, RgbImage};
        // 用 image crate 自己编码出真图（13x7）写进文件再 read —— 过的是真实文件头
        let img = RgbImage::from_pixel(13, 7, image::Rgb([200, 30, 90]));
        let reg = builtin();
        for (format, mime) in [
            (ImageFormat::Png, "image/png"),
            (ImageFormat::Jpeg, "image/jpeg"),
            (ImageFormat::Gif, "image/gif"),
            (ImageFormat::WebP, "image/webp"),
            (ImageFormat::Bmp, "image/bmp"),
        ] {
            let mut bytes = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut bytes), format)
                .expect("编码");
            let p = tmp(&format!("image-{format:?}"));
            std::fs::write(&p, &bytes).unwrap();
            let out = reg
                .dispatch(
                    "read",
                    &json!({"path": p.to_str().unwrap()}),
                    ToolCtx::default(),
                )
                .await
                .unwrap();
            assert!(out.starts_with("[图片已读取: path="), "{format:?}: {out}");
            assert!(out.contains(&format!("mime={mime}")), "{format:?}: {out}");
            assert!(
                out.contains(&format!("size={}", bytes.len())),
                "{format:?}: {out}"
            );
            assert!(out.contains("dim=13x7"), "{format:?}: {out}");
            let _ = std::fs::remove_file(&p);
        }
    }

    /// 非图片、以及魔数认得出但**不在白名单**的格式（TIFF/ICO）仍走文本/二进制分支。
    #[tokio::test]
    async fn read_keeps_off_list_formats_as_plain_files() {
        // TIFF/ICO 的魔数 `guess_format` 认得出（这正是必须显式过白名单的原因），但 Python 版不当图片
        assert!(
            image::guess_format(b"II*\0\x08\0\0\0").is_ok(),
            "guess_format 认得 TIFF"
        );
        // RIFF 但不是 WEBP（WAV/AVI）
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&[0u8; 4]);
        wav.extend_from_slice(b"WAVE");

        let reg = builtin();
        // ⚠ TIFF/ICO/WAV 的魔数全在 ASCII + 控制字符范围 → UTF-8 解得出来，**仍当文本读**（与 Python 版一致）；
        // 这里要断言的是「不被当成图片」。真正走二进制分支的是解不出 UTF-8 的字节。
        for (name, bytes, marker) in [
            ("text", b"hello world\n".to_vec(), "[行 1-1，共 1 行]"),
            ("tiff", b"II*\0\x08\0\0\0".to_vec(), "[行 1-1，共 1 行]"),
            (
                "ico",
                b"\x00\x00\x01\x00\x01\x00".to_vec(),
                "[行 1-1，共 1 行]",
            ),
            ("wav", wav, "[行 1-1，共 1 行]"),
            ("binary", vec![0xff, 0xfe, 0x00, 0x01], "[二进制文件"),
        ] {
            let p = tmp(&format!("plain-{name}"));
            std::fs::write(&p, &bytes).unwrap();
            let out = reg
                .dispatch(
                    "read",
                    &json!({"path": p.to_str().unwrap()}),
                    ToolCtx::default(),
                )
                .await
                .unwrap();
            assert!(!out.contains("图片已读取"), "{name}: {out}");
            assert!(out.starts_with(marker), "{name}: {out}");
            let _ = std::fs::remove_file(&p);
        }
    }

    // ---------------------------------------------------------------- 注册与分发

    /// 工具都通过注册表分发（等于把 serde 解析 + `#[schemars(skip)]` 注入 + `call` 整条链路跑一遍）。
    #[tokio::test]
    async fn read_paginates_and_reports_truncation() {
        let p = tmp("read.txt");
        std::fs::write(&p, "a\nb\nc\nd\ne\n").unwrap();
        let reg = builtin();
        let path = p.to_str().unwrap();

        let out = reg
            .dispatch(
                "read",
                &json!({"path": path, "limit": 2}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(out.starts_with("[行 1-2，共 5 行]"), "{out}");
        assert!(out.ends_with("a\nb"), "{out}");

        // limit 是模型显式分页（不算截断）；_max_lines（配置注入的私有参数）才给续读提示
        let out = reg
            .dispatch(
                "read",
                &json!({"path": path, "_max_lines": 2}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(out.contains("[已截断：可用 offset=3 继续读]"), "{out}");

        // 字节预算：行不含换行 → 每行 "a" 算 2 字节（+1）；5 字节装得下 2 行
        let out = reg
            .dispatch(
                "read",
                &json!({"path": path, "_max_bytes": 5}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(out.starts_with("[行 1-2，共 5 行]"), "{out}");
        assert!(out.contains("[已截断：可用 offset=3 继续读]"), "{out}");
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn read_errors_are_messages_not_panics() {
        let reg = builtin();
        let e = reg
            .dispatch(
                "read",
                &json!({"path": "/definitely/not/here"}),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        assert!(e.0.contains("文件不存在"), "{e}");

        // 缺必填参数：serde 的话直接回给模型
        let e = reg
            .dispatch("read", &json!({}), ToolCtx::default())
            .await
            .unwrap_err();
        assert!(e.0.contains("参数解析失败"), "{}", e.0);
        assert!(e.0.contains("missing field `path`"), "{}", e.0);
    }

    #[tokio::test]
    async fn edit_is_atomic_when_any_edit_fails() {
        let p = tmp("edit.txt");
        std::fs::write(&p, "alpha\nbeta\n").unwrap();
        let reg = builtin();
        let path = p.to_str().unwrap();

        // 第二条 oldText 找不到 → 整次不写入
        let e = reg
            .dispatch(
                "edit",
                &json!({"path": path, "edits": [
                    {"oldText": "alpha", "newText": "ALPHA"},
                    {"oldText": "nope", "newText": "x"}
                ]}),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        assert!(e.0.contains("未写入任何内容"), "{e}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "alpha\nbeta\n");

        // 正常替换（多条、按原文一次性应用）
        let out = reg
            .dispatch(
                "edit",
                &json!({"path": path, "edits": [
                    {"oldText": "alpha", "newText": "ALPHA"},
                    {"oldText": "beta", "newText": "BETA"}
                ]}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(out.contains("已替换 2 处"), "{out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "ALPHA\nBETA\n");

        // 重叠 → 报错且不写入
        let e = reg
            .dispatch(
                "edit",
                &json!({"path": path, "edits": [
                    {"oldText": "ALPHA\nBETA", "newText": "x"},
                    {"oldText": "ALPHA", "newText": "y"}
                ]}),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        assert!(e.0.contains("重叠"), "{e}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "ALPHA\nBETA\n");

        // 空 edits
        let e = reg
            .dispatch(
                "edit",
                &json!({"path": path, "edits": []}),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        assert!(e.0.contains("不能为空"), "{e}");
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn edit_rejects_non_unique_old_text() {
        let p = tmp("dup.txt");
        std::fs::write(&p, "x\nx\n").unwrap();
        let reg = builtin();
        let e = reg
            .dispatch(
                "edit",
                &json!({"path": p.to_str().unwrap(), "edits": [{"oldText": "x", "newText": "y"}]}),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        assert!(e.0.contains("出现 2 次"), "{e}");
        let _ = std::fs::remove_file(&p);
    }

    /// 找不到时给「最接近的位置 + 简版 diff」（缩进少两个空格的典型场景）。
    #[tokio::test]
    async fn edit_hints_nearest_fragment_with_diff() {
        let p = tmp("nearest.txt");
        std::fs::write(
            &p,
            "fn main() {\n    let a = 1;\n    println!(\"{a}\");\n}\n",
        )
        .unwrap();
        let reg = builtin();
        let e = reg
            .dispatch(
                "edit",
                &json!({"path": p.to_str().unwrap(), "edits": [
                    {"oldText": "  let a = 1;\n  println!(\"{a}\");", "newText": "x"}
                ]}),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        assert!(e.0.contains("找不到"), "{e}");
        // 锚行 = oldText 里最长的那行（println 那行）→ 定位到原文第 3 行，窗口从第 2 行起
        assert!(e.0.contains("原文里最接近的位置在第 2 行"), "{e}");
        assert!(e.0.contains("锚行相似度 100%"), "{e}");
        assert!(e.0.contains("--- 你的 oldText"), "{e}");
        assert!(e.0.contains("+    let a = 1;"), "{e}"); // 原文实际（四个空格）
        let _ = std::fs::remove_file(&p);
    }

    /// 内置工具的注册名与顺序（`writ` / `bash` 是用户点名改的名，别再改回去）。
    #[test]
    fn builtin_tool_names() {
        assert_eq!(builtin().names(), vec!["read", "edit", "writ", "bash"]);
    }

    // 自测工具（非内置）：验证 `.with_tool` 这条公开路径。
    /// 自测工具
    #[derive(Deserialize, JsonSchema)]
    struct Demo {
        /// 要回显的文本
        text: String,
        /// 可选计数
        count: Option<i64>,
        /// 私有参数，不进 schema
        #[schemars(skip)]
        hidden: Option<i64>,
    }

    impl Tool for Demo {
        async fn call(self, _ctx: ToolCtx) -> ToolResult {
            Ok(format!(
                "{}:{}:{}",
                self.text,
                self.count.unwrap_or(0),
                self.hidden.is_some()
            ))
        }
    }

    #[tokio::test]
    async fn with_tool_registers_and_dispatches() {
        let reg = ToolRegistry::empty(HashMap::new()).with_tool::<Demo>("demo");
        assert_eq!(reg.names(), vec!["demo"]);

        let spec = reg.specs().remove(0);
        assert_eq!(spec["function"]["name"], "demo");
        // 结构体 doc 注释 → function.description
        assert_eq!(spec["function"]["description"], "自测工具");
        // 非 Option 即必填
        assert_eq!(spec["function"]["parameters"]["required"], json!(["text"]));
        assert_eq!(
            spec["function"]["parameters"]["properties"]["count"]["type"],
            "integer"
        );
        // 私有参数不进 schema
        assert!(spec["function"]["parameters"]["properties"]
            .get("hidden")
            .is_none());

        // 分发：JSON → 结构体 → call
        let out = reg
            .dispatch(
                "demo",
                &json!({"text": "hi", "count": 2, "hidden": 1}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(out, "hi:2:true");

        // 未知工具：带上可用列表（对模型有用）
        let e = reg
            .dispatch("nope", &json!({}), ToolCtx::default())
            .await
            .unwrap_err();
        assert!(e.0.contains("未知工具"), "{}", e.0);
        assert!(e.0.contains("可用: demo"), "{}", e.0);
    }

    /// 重名注册是编程错误 → 直接 panic（Python 版 `register` 抛 ValueError）。
    #[test]
    #[should_panic(expected = "工具已存在")]
    fn with_tool_rejects_duplicate_name() {
        let _ = ToolRegistry::empty(HashMap::new())
            .with_tool::<Demo>("demo")
            .with_tool::<Demo>("demo");
    }

    // ---------------------------------------------------------------- --tools

    #[test]
    fn tools_from_spec_selects_and_restricts() {
        // 无 spec → 全量
        let all = tools_from_spec(None, HashMap::new());
        let mut names = all.names();
        names.sort();
        assert_eq!(names, vec!["bash", "edit", "read", "writ"]);

        // 四个内置全列且无白名单 → 也走全量（未受限）
        let reg = tools_from_spec(Some("read,edit,writ,bash"), HashMap::new());
        assert_eq!(reg.names(), vec!["read", "edit", "writ", "bash"]);

        // 仅 read → shell 禁用
        let reg = tools_from_spec(Some("read"), HashMap::new());
        assert_eq!(reg.names(), vec!["read"]);

        // 非内置名 → 隐式启用受限 shell；description 追加白名单；私有参数仍不进 schema
        let reg = tools_from_spec(Some("read, echo ,echo"), HashMap::new());
        assert_eq!(reg.names(), vec!["read", "bash"]);
        let shell = reg
            .specs()
            .into_iter()
            .find(|s| s["function"]["name"] == "bash")
            .expect("有 shell");
        let desc = shell["function"]["description"].as_str().unwrap();
        assert!(
            desc.contains("本次运行仅允许以这些命令开头: echo"),
            "{desc}"
        );
        assert!(shell["function"]["parameters"]["properties"]
            .get("_allow_cmds")
            .is_none());
    }

    #[tokio::test]
    async fn restricted_shell_rejects_commands_outside_allowlist() {
        let reg = tools_from_spec(Some("echo"), HashMap::new());
        assert_eq!(reg.names(), vec!["bash"]);
        // 白名单内 → 正常执行
        let out = reg
            .dispatch(
                "bash", &json!({"command": "echo hi"}), ToolCtx::default())
            .await
            .unwrap();
        assert!(out.contains("hi"), "{out}");
        // 白名单外（含空命令）→ 直接回给模型，不启动进程
        let e = reg
            .dispatch(
                "bash",
                &json!({"command": "rm -rf /tmp/nope"}),
                ToolCtx::default(),
            )
            .await
            .unwrap_err();
        assert!(e.0.contains("本次仅允许以这些命令开头: echo"), "{e}");
        assert!(e.0.contains("收到: rm"), "{e}");
        let e = reg
            .dispatch(
                "bash", &json!({"command": "  "}), ToolCtx::default())
            .await
            .unwrap_err();
        assert!(e.0.contains("收到: (空命令)"), "{e}");
    }

    #[test]
    fn inject_defaults_only_touches_underscore_keys() {
        let mut defaults: HashMap<String, toml::Table> = HashMap::new();
        let mut table = toml::Table::new();
        table.insert("_max_lines".into(), toml::Value::Integer(7));
        table.insert("path".into(), toml::Value::Integer(99)); // 非 `_` 开头：不该注入
        defaults.insert("read".into(), table);
        let reg = ToolRegistry::new(defaults);

        let mut args = json!({"path": "x"});
        reg.inject_defaults("read", &mut args);
        assert_eq!(args["_max_lines"], 7);
        assert_eq!(args["path"], "x"); // 显式传参不被覆盖，且非私有键不注入

        let mut args = json!({"path": "x", "_max_lines": 3});
        reg.inject_defaults("read", &mut args);
        assert_eq!(args["_max_lines"], 3); // 显式传参优先
    }

    // ---------------------------------------------------------------- shell

    #[tokio::test]
    async fn shell_merges_stderr_and_reports_exit() {
        let out = builtin()
            .dispatch(
                "bash",
                &json!({"command": "echo out; echo err 1>&2"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(out, "out\nerr\n", "成功：纯正文、没有退出码头：{out:?}");
        // stderr 已合并到 stdout（顺序稳定：同一个管道）
    }

    #[tokio::test]
    async fn shell_reports_nonzero_exit_code() {
        let out = builtin()
            .dispatch(
                "bash", &json!({"command": "exit 3"}), ToolCtx::default())
            .await
            .unwrap();
        // 没输出 → 结果就是那一行头
        assert_eq!(out, fail_header(3), "{out}");
    }

    /// 失败：只给一行头 `[exit=N, os=…, shell=…]`；成功：**只有结果**（连 `[exit=0]` 都没有）。
    #[tokio::test]
    async fn shell_exit_headers_only_on_failure() {
        let reg = builtin();
        let ok = reg
            .dispatch("bash", &json!({"command": "echo hi"}), ToolCtx::default())
            .await
            .unwrap();
        assert_eq!(ok, "hi\n", "成功：只有正文");

        let silent = reg
            .dispatch("bash", &json!({"command": "true"}), ToolCtx::default())
            .await
            .unwrap();
        assert_eq!(silent, "", "成功且没输出：结果为空（没有 `[exit=0]` 可给）");

        let headers = fail_header(3);
        let bad = reg
            .dispatch("bash", &json!({"command": "exit 3"}), ToolCtx::default())
            .await
            .unwrap();
        assert_eq!(bad, headers, "失败且没输出：只有一行头");

        let bad = reg
            .dispatch(
                "bash",
                &json!({"command": "echo boom; exit 3"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(bad, format!("{headers}\n\nboom\n"), "头 + 空行 + 正文");
        // 前端（Rust TUI 的 `history::tool_result_ok`）只拿**第一行**判成败：
        // `[exit=` 开头且不是 `[exit=0…` = 失败（成功根本没有头）
        let first = bad.lines().next().unwrap_or_default();
        assert_eq!(first, headers, "{bad}");
        assert!(
            first.starts_with("[exit=") && !first.starts_with("[exit=0"),
            "{bad}"
        );
    }

    /// 被信号干掉（`code()` 拿不到）→ `[exit=-1]`（与 Python 版同款：都是「非正常退出」）。
    #[cfg(unix)]
    #[tokio::test]
    async fn killed_shell_reports_minus_one() {
        let out = builtin()
            .dispatch(
                "bash",
                &json!({"command": "kill -TERM $$"}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(out.starts_with(&fail_header(-1)), "{out}");
    }

    #[tokio::test]
    async fn shell_truncates_head_and_spills_full_output() {
        let out = builtin()
            .dispatch(
                "bash",
                &json!({"command": "seq 1 200", "_max_lines": 5}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        // 超限 → 只留**开头** + 独立指针（shell 的 stdout 不可再生，是唯一该落盘的）
        assert!(out.contains("[工具输出全文已保存: "), "{out}");
        let body = out.split("\n\n").nth(1).expect("有 body");
        assert!(body.starts_with("1\n"), "{body}");
        assert!(body.contains("5\n") && !body.contains("6\n"), "{body}");

        // 但全文仍然落盘（经指针可取回）——「取头部」丢的只是可见性，不是信息
        let spill = out
            .lines()
            .find_map(|l| l.strip_prefix("[工具输出全文已保存: ")?.strip_suffix(']'))
            .expect("有落盘指针");
        let full = std::fs::read_to_string(spill).expect("落盘文件可读");
        assert!(
            full.starts_with("1\n") && full.contains("200\n"),
            "落盘内容不完整"
        );
    }

    /// 未超限不动原文、也不落盘；字节预算与 `read` 同一套口径。
    /// （原 `head_output` 的用例——它已并进 `Shell::call`，所以从 shell 的公开路径测。）
    #[tokio::test]
    async fn shell_truncation_uses_line_and_byte_budgets() {
        let reg = builtin();
        // 未超限 → 原文照回，没有落盘指针
        let out = reg
            .dispatch(
                "bash",
                &json!({"command": "seq 1 5", "_max_lines": 100}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(out, "1\n2\n3\n4\n5\n", "成功且未超限：纯正文，没有头也没有指针");

        // 字节预算：每行 "1\n" 实打实 2 字节 → 6 字节装 3 行（与 Python `_tail_output` 同口径）
        let out = reg
            .dispatch(
                "bash",
                &json!({"command": "seq 1 100", "_max_bytes": 6}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(out.contains("全文已保存"), "{out}");
        assert_eq!(out.split("\n\n").nth(1).unwrap(), "1\n2\n3\n", "{out}");
    }

    #[tokio::test]
    async fn shell_timeout_kills_the_whole_process_group() {
        // 后台子进程持有 stdout 管道写端：只杀 shell 本体的话，等管道 EOF 会卡满 30s。
        // 这条测试断言「超时路径真的没被卡住」。
        let started = std::time::Instant::now();
        let out = builtin()
            .dispatch(
                "bash",
                &json!({"command": "sleep 30 & wait", "timeout": 1}),
                ToolCtx::default(),
            )
            .await
            .unwrap();
        assert!(out.contains("超时"), "{out}");
        assert!(
            started.elapsed().as_secs() < 10,
            "超时路径被卡住了: {:?}",
            started.elapsed()
        );
    }
}
