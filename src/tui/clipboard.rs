//! 剪贴板：把里面的**图片**变成路径插进输入框（`Ctrl+G`）。
//!
//! **普通文本不在这里处理**：文本粘贴交给终端自己的粘贴键（macOS `⌘V` / Linux
//! `Ctrl+Shift+V`）经 bracketed paste 走 `Event::Paste`。与 Python 一致——那边也只有
//! TextArea 的 `Ctrl+V` 会在「没有图片」时回退文本粘贴（`action_paste`），而
//! `Ctrl+G`（`action_paste_image`）**只认图片**，无图就提示。
//!
//! 两条来源，按这个顺序看（与 Python `clipboard.grab_image_path()` 同一语义）：
//!   1. **位图**（截图）→ PNG 编码后走 `session::store_blob` 落成本地副本（内容寻址
//!      `img-<sha256[:16]>`、0o600、写临时文件再 rename），返回**副本**路径 —— 与 `read`
//!      用的是同一份副本，所以粘贴后回车 `read` 这个路径是零复制的；PNG 编码用 `image`
//!      crate（Python 那边是 Pillow 干的活）。
//!   2. **文件列表**（Finder / 资源管理器里 `⌘C` 一个图片文件时剪贴板里是文件引用而非位图）
//!      → 只认图片后缀、且文件确实存在的第一个，返回**原路径、不复制**。
//!
//! `arboard` 在部分环境不可用（Termux/无 X11），拿不到剪贴板就当「没有内容」，不报错。
//!
//! **写入**（消息流框选 / 输入框拖选复制）用 [`Copier`]：句柄要在 `App` 里长活，
//! 每次写完就 drop 会在 Linux 上造成屏幕被 stderr 警告砸花 + 复制没生效（详见 `Copier`）。

use std::path::{Path, PathBuf};

use arboard::Clipboard;

use crate::session;

/// 剪贴板里的文件只认这些后缀（Python `clipboard._IMAGE_SUFFIXES` 同款，含多收的 TIFF）
/// —— 复制个 `.txt` 过来不算图片（会落回「剪贴板里没有图片」，不往输入框塞路径）。
const IMAGE_SUFFIXES: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"];

/// 读剪贴板里的图片：**先看位图**（截图场景），再看**文件列表**；都没有就 `None`
/// —— 文本不在这里兜底（那两个入口只负责图片）。
///
/// 返回的路径插进输入框，回车即普通 `read`：位图是新落盘的副本，文件列表是磁盘上原文件。
pub fn paste_image() -> Option<String> {
    let mut clipboard = Clipboard::new().ok()?;
    if let Ok(image) = clipboard.get_image() {
        if let Some(path) = store_image(&image) {
            return Some(path);
        }
    }
    // 没有位图：macOS 的 furl / Windows 的 CF_HDROP / Linux 的 URI 列表都从这里出来。
    if let Ok(paths) = clipboard.get().file_list() {
        if let Some(path) = image_from_paths(&paths) {
            return Some(path.display().to_string());
        }
    }
    None
}

/// 系统剪贴板的**长活**写入句柄（由 `App` 持有）。
///
/// 为什么不能每次 `Clipboard::new()` 写完就 drop：Linux（X11）下剪贴板内容的提供者就是本
/// 进程，写完 100ms 内 drop 有两个后果——
///   1. arboard 会往 **stderr** 打一行警告；TUI 期间是 raw mode + 交替屏，这行字节落在当前
///      光标处、而 ratatui 只重画变化的格子 → 屏幕被砸花且**再也修不回来**；
///   2. 剪贴板管理器可能来不及取走内容，复制其实没生效。
/// 所以持有一个进程级的长活句柄，每次写复用它。
#[derive(Default)]
pub struct Copier {
    slot: Option<Clipboard>,
}

impl Copier {
    /// 把文本写进系统剪贴板（消息流框选 / 输入框拖选共用）。
    /// `arboard` 不可用（无 X11 / Termux）时返回 `false`。
    pub fn copy(&mut self, text: &str) -> bool {
        if self.slot.is_none() {
            self.slot = Clipboard::new().ok();
        }
        match self.slot.as_mut() {
            Some(clipboard) => clipboard.set_text(text.to_string()).is_ok(),
            None => false,
        }
    }
}

/// 「剪贴板里没有图片」时按平台补一句怎么把图弄进剪贴板（对齐 Python `_NO_IMAGE_HINT`）。
pub fn no_image_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "（macOS：截图要按 ⌃⇧⌘4 才会进剪贴板；⇧⌘4 是存成文件——在 Finder 里 ⌘C 复制也行）"
    } else if cfg!(target_os = "linux") {
        "（Linux 下还需装 wl-clipboard 或 xclip）"
    } else {
        "（图片要先进系统剪贴板）"
    }
}

/// 文件列表 → 第一个「存在的图片文件」；**不复制**，直接用用户磁盘上那个文件
/// （与 Python `clipboard._from_file_list` 一致）。
fn image_from_paths(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|p| is_image_file(p)).cloned()
}

fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| IMAGE_SUFFIXES.contains(&ext.to_ascii_lowercase().as_str()))
        && path.is_file()
}

/// arboard 的 RGBA 位图 → PNG 字节 → 内容寻址落盘。
fn store_image(image: &arboard::ImageData<'_>) -> Option<String> {
    let rgba = image::RgbaImage::from_raw(
        u32::try_from(image.width).ok()?,
        u32::try_from(image.height).ok()?,
        image.bytes.to_vec(),
    )?;
    let mut png = std::io::Cursor::new(Vec::new());
    rgba.write_to(&mut png, image::ImageFormat::Png).ok()?;
    let (_hash_id, path) = session::store_blob(&png.into_inner(), "image/png").ok()?;
    Some(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真剪贴板不可控（CI/无 X11），这里只验证「图片 → PNG → 内容寻址落盘」这段纯逻辑。
    #[test]
    fn rgba_is_encoded_to_png_and_stored_content_addressed() {
        let _g = crate::config::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pie-paste-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("PIE_DIR", &dir);

        // 2x2 红色 RGBA
        let bytes = [255u8, 0, 0, 255].repeat(4);
        let data = arboard::ImageData {
            width: 2,
            height: 2,
            bytes: std::borrow::Cow::Owned(bytes),
        };
        let path = store_image(&data).expect("编码 + 落盘");
        assert!(path.contains("img-"), "{path}");
        assert!(std::path::Path::new(&path).exists());
        // 同内容再来一次 → 同一份副本（内容寻址）
        let again = store_image(&data).expect("再来一次");
        assert_eq!(path, again);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件列表：跳过「非图片」与「不存在」的项，取第一个真图片；扩展名大小写不敏感。
    #[test]
    fn file_list_keeps_first_existing_image() {
        let dir = std::env::temp_dir().join(format!("pie-paste-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let txt = dir.join("notes.txt");
        std::fs::write(&txt, b"x").unwrap();
        let upper = dir.join("Shot.JPEG");
        std::fs::write(&upper, b"x").unwrap();
        let png = dir.join("shot.png");
        std::fs::write(&png, b"x").unwrap();

        let paths = vec![
            txt,                     // 存在但不是图片
            dir.join("missing.png"), // 图片后缀但不存在
            upper.clone(),           // 大写扩展名也算
            png.clone(),             // 已经有更靠前的了，取不到
        ];
        assert_eq!(image_from_paths(&paths), Some(upper));
        // 一条都没有图片 → None（不再退回文本：复制个 .txt 不该往输入框里塞路径）
        assert_eq!(image_from_paths(&paths[..2]), None);
        assert_eq!(image_from_paths(&[]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

