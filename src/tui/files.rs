//! `@` 文件路径补全：cwd 文件索引（尊重 `.gitignore`）+ 片段匹配 + token 认读。
//!
//! 与 `/` 命令补全（`palette.rs`）的分工：那边候选表是**写死的**，这边是把 cwd 的文件树
//! **扫一遍**得来的；共同点是「候选 = `(插入文本, 说明)`」且面板渲染复用
//! `palette::panel_lines`。
//!
//! 三件事都在这里，各自的边界写清楚：
//!
//!   - [`Index::build`]：**同步**扫盘，调用方负责放 `spawn_blocking`（本仓在 9p 上 ~40ms、
//!     原生盘 18k 文件 ~55ms）——`App` 缓存它，只在「回合 / `!cmd` 结束后」标过期重建，
//!     **绝不能放在每次按键的路径上**。
//!   - [`Index::matches`]：纯内存匹配（每帧都要算，所以匹配用的字符串都在建索引时预处理好）。
//!   - [`token`] / [`PathSession`]：认「光标这一段的路径片段」的两种来源——`@` 打头（用户
//!     显式开局），或目录候选接受后留下来的锚点（`@` 已吃掉，接着往下钻）。都是纯函数，好测。
//!
//! 索引之外还有**三档锚点**（[`external`]）：`@..` / `@/` / `@~/` 分别从上级目录 / 根目录 /
//! 主目录列。索引只覆盖 cwd 子树，列不出它之外的目录 → 这三档走**实时 `read_dir`**（只列一层，
//! 不做全树模糊），也因此**不依赖索引**——打完就能弹，不必等第一次扫盘回来。

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

/// 索引条数上限：cwd 万一是巨型目录（比如 `~`），截断总比把内存吃光好。
pub const MAX_ENTRIES: usize = 20_000;

/// 匹配上限。面板一次只显示一屏（`palette::MAX_SHOWN` 行），但候选是可以 ↓ 一路翻的；
/// 200 条足够翻、也不至于每帧白算。
pub const MATCH_LIMIT: usize = 200;

/// 索引里的一条。
struct Entry {
    /// 相对 cwd 的路径，`/` 分隔（Windows 上也统一成 `/`，与 `read` 的入参口径一致）
    path: String,
    /// `path` 小写副本（「路径包含」那档匹配用；预先算好，免得每帧给 2 万条各分配一次）
    lower: String,
    /// basename 小写副本（主力匹配键）
    name: String,
    is_dir: bool,
}

/// 一份 cwd 文件索引（不可变；重建就是换一份新的）。
pub struct Index {
    root: PathBuf,
    entries: Vec<Entry>,
    /// 撞到 [`MAX_ENTRIES`] 截断了（面板给一句提示，别假装全都有）
    truncated: bool,
}

impl Index {
    /// 扫一遍 `root` 建索引（**同步**，见模块注释）。
    ///
    /// 忽略规则 = `.gitignore` / `.ignore`（含嵌套与 `!` 取反），外加点文件不收。
    pub fn build(root: &Path) -> Index {
        Self::build_capped(root, MAX_ENTRIES)
    }

    /// `build` 的实现（`cap` 抽成参数只为让截断那条用例别去建 2 万个文件）。
    fn build_capped(root: &Path, cap: usize) -> Index {
        let mut builder = WalkBuilder::new(root);
        builder
            // ⚠ 默认**只有 git 仓库里**才认 `.gitignore`（crate 的 `require_git` 默认开）。
            // 用户要的语义是「有 `.gitignore` 就按它排除」→ 关掉这个前提。
            .require_git(false)
            // 只认仓库里的忽略文件：个人的全局忽略（`core.excludesFile`）不该悄悄把
            // 某些文件从补全里藏起来。
            .git_global(false)
            // 点文件 / 点目录一概不收（ripgrep 的默认）——`.git` 也因此进不来。
            // 代价：`.env` / `.github/…` 补不出来（要的话把这里改成 false 并单独剪掉 `.git`）。
            .hidden(true);

        let mut entries: Vec<Entry> = Vec::new();
        let mut truncated = false;
        for entry in builder.build() {
            // 读不了的条目（权限…）跳过就行，别让一个目录把整棵树作废
            let Ok(entry) = entry else { continue };
            let Ok(rel) = entry.path().strip_prefix(root) else { continue };
            if rel.as_os_str().is_empty() {
                continue; // 根目录自己
            }
            if entries.len() >= cap {
                truncated = true;
                break;
            }
            let path = rel.to_string_lossy().replace('\\', "/");
            let name = path.rsplit('/').next().unwrap_or(&path).to_lowercase();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            entries.push(Entry {
                lower: path.to_lowercase(),
                name,
                path,
                is_dir,
            });
        }
        // 排序：面板里同排名候选的顺序稳定（`build` 的遍历顺序依赖 readdir）
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Index {
            root: root.to_path_buf(),
            entries,
            truncated,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// 给一段（`@` 之后的）片段找候选：`(插入文本, 说明)`。
    ///
    /// 三种模式：
    ///   - 片段含 `/`：**shell 式**——列「斜杠前那个目录」的**直接子项**，按名字前缀过滤。
    ///     一段段往下打路径时靠它（要更深就接着打 `/`）；
    ///   - 片段为空：只列 cwd 的直接子项（「输入 `@` 就弹面板」）；
    ///   - 其它：全树按 **basename** 模糊（前缀 > 包含 > 路径包含）。`@app` 直接命中
    ///     `src/tui/app.rs` —— 这才是 `@` 比手打路径省事的地方。
    pub fn matches(&self, fragment: &str, limit: usize) -> Vec<(String, String)> {
        let fragment = fragment.replace('\\', "/");
        let (dir, name) = match fragment.rfind('/') {
            Some(i) => (&fragment[..=i], &fragment[i + 1..]),
            None => ("", fragment.as_str()),
        };
        let name = name.to_lowercase();
        // 「列目录」还是「模糊找」：前者按字典序、后者按路径长度排（见下面 sort_by）
        let listing = !dir.is_empty() || name.is_empty();
        let mut hits: Vec<(u8, &Entry)> = Vec::new();
        for entry in &self.entries {
            let rank = if !dir.is_empty() {
                let Some(rest) = entry.path.strip_prefix(dir) else {
                    continue;
                };
                if rest.contains('/') {
                    continue; // 只列直接子项
                }
                if !rest.to_lowercase().starts_with(&name) {
                    continue;
                }
                0
            } else if name.is_empty() {
                if entry.path.contains('/') {
                    continue; // 空片段：只列 cwd 的直接子项
                }
                0
            } else if entry.name.starts_with(&name) {
                0
            } else if entry.name.contains(&name) {
                1
            } else if entry.lower.contains(&name) {
                2
            } else {
                continue;
            };
            hits.push((rank, entry));
        }
        hits.sort_by(|a, b| {
            a.0.cmp(&b.0).then_with(|| match listing {
                // 列目录（前缀模式 / 空片段）：按路径字典序，跟 `ls` 一个观感
                true => a.1.path.cmp(&b.1.path),
                // 模糊匹配：短的先（越靠近根通常越相关），同长度再字典序
                false => a
                    .1
                    .path
                    .len()
                    .cmp(&b.1.path.len())
                    .then_with(|| a.1.path.cmp(&b.1.path)),
            })
        });
        hits.truncate(limit);
        hits.into_iter()
            .map(|(_, e)| candidate(&e.path, e.is_dir))
            .collect()
    }
}

/// 候选的（插入文本, 说明）：目录尾部补 `/`（接着往下打的锚点），文件不带说明。
///
/// 插入的就是**能直接给 `read` 用的路径**：索引那套是相对 cwd 的完整路径，
/// `..` 是 `../…`，`/` 是绝对路径，`~` 已展开成主目录的绝对路径。
fn candidate(path: &str, is_dir: bool) -> (String, String) {
    if is_dir {
        (format!("{path}/"), "目录".to_string())
    } else {
        (path.to_string(), String::new())
    }
}

// ---------------------------------------------------------------- 索引之外的锚点

/// `@` 补全的候选入口：`..` / `/` / `~/` 三档**实时列目录**（见 [`external`]），其余走 `index`。
///
/// `index` 为 `None`（首次 `@` 那次后台扫盘还没回来）时，只有三档锚点有候选。
/// 三档要列的目录按 `index` 自己的 root 解析（与索引覆盖的范围一致），没索引才退回 `root`。
pub fn matches(
    fragment: &str,
    index: Option<&Index>,
    root: &Path,
    limit: usize,
) -> Vec<(String, String)> {
    let fragment = fragment.replace('\\', "/");
    let root = index.map(Index::root).unwrap_or(root);
    if let Some((dir, prefix, name)) = external(&fragment, root) {
        return list_dir(&dir, &prefix, &name, limit);
    }
    index.map(|i| i.matches(&fragment, limit)).unwrap_or_default()
}

/// `..` / `/` / `~/` 三档锚点 → `(要列的目录, 插入文本前缀, 名字前缀)`；不是这三档返回 `None`。
///
/// 索引只扫 cwd 子树，列不出它之外的目录 → 这三档改成实时 `read_dir`：
///   - `..` / `../…`：上级目录（插入文本保留相对写法，`read` 按相对路径能开）；
///   - `/` / `/…`：根目录（本来就是绝对路径）；
///   - `~` / `~/…`：主目录，**展开成绝对路径**——`read` 不认 `~`。
pub fn external(fragment: &str, root: &Path) -> Option<(PathBuf, String, String)> {
    let fragment = fragment.replace('\\', "/");
    let (dir, name) = split_dir(&fragment);
    let prefix = anchor_dir(dir)?;
    // 相对前缀（`../`）按 root 解析；绝对前缀 join 会直接覆盖 root
    Some((root.join(&prefix), prefix, name.to_lowercase()))
}

/// 拆出（目录段，**含尾斜杠**）+ 半截名字；整段就是锚点本身（`@..` / `@~`）时补成目录写法。
fn split_dir(fragment: &str) -> (&str, &str) {
    if let Some(i) = fragment.rfind('/') {
        return fragment.split_at(i + 1);
    }
    match fragment {
        ".." => ("../", ""),
        "~" => ("~/", ""),
        _ => ("", fragment),
    }
}

/// 目录段属于三档锚点吗？是就返回**展开后**的目录段（`~` → 主目录），否则 `None`。
fn anchor_dir(dir: &str) -> Option<String> {
    if dir.starts_with('/') || dir.starts_with("../") {
        return Some(dir.to_string());
    }
    let rest = dir.strip_prefix("~/")?;
    let home = crate::config::home_dir().to_string_lossy().replace('\\', "/");
    Some(format!("{home}/{rest}"))
}

/// 实时列**一层**目录（[`external`] 那三档的候选来源）：名字前缀过滤、字典序、取前 `limit` 条。
///
/// 点文件 / 点目录一概不收（与索引同口径）；读不了的目录（权限…）给空候选而不是报错。
/// 不算忽略规则（`.gitignore` 是索引那套的事）——要的就是「如实列出这一层有什么」。
fn list_dir(dir: &Path, prefix: &str, name: &str, limit: usize) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut hits: Vec<(String, bool)> = Vec::new();
    for entry in entries.flatten() {
        let child = entry.file_name().to_string_lossy().into_owned();
        if child.starts_with('.') || !child.to_lowercase().starts_with(name) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        hits.push((child, is_dir));
        if hits.len() >= MAX_ENTRIES {
            break; // 巨型目录：截断总比把内存吃光好（与索引同上限）
        }
    }
    hits.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    hits.truncate(limit);
    hits.into_iter()
        .map(|(child, is_dir)| candidate(&format!("{prefix}{child}"), is_dir))
        .collect()
}

/// 从「光标所在行 + 行内字符列」里认出光标处的 `@` token：`(替换起点列, 片段)`。
///
/// 起点列就是 `@` 自己那一列——**接受候选时连 `@` 一起替换掉**（用户要的：插进去的是干净
/// 路径，模型不会拿 `@src/a.rs` 去 read 失败）。
///
/// 三条边界：取光标前**最后一个** `@`（所以 `@@a` 认的是后一个）；`@` 与光标之间不能有
/// 空白（所以片段里不会有空格——带空格的路径补不了，已知限制）；`@` 前面不能紧跟着字母
/// 数字 / `.` / `-` / `_`（`a@b.com`、`foo@bar` 这种不留神就弹面板）。
pub fn token(line: &str, col: usize) -> Option<(usize, String)> {
    let before: Vec<char> = line.chars().take(col).collect();
    let at = before.iter().rposition(|c| *c == '@')?;
    if before[at + 1..].iter().any(|c| c.is_whitespace()) {
        return None;
    }
    // ⚠ 防误伤只认 **ASCII** 字母数字：中文句子里的「看下@src/a.rs」必须能弹面板
    // （`char::is_alphanumeric` 对汉字是真的，用它会把最常用的场景全挡掉）。
    if at > 0 && (before[at - 1].is_ascii_alphanumeric() || matches!(before[at - 1], '.' | '-' | '_'))
    {
        return None;
    }
    Some((at, before[at + 1..].iter().collect()))
}

/// 「路径补全会话」：**目录候选被 Tab 接受之后**留下来的锚点。
///
/// 为什么要它：接受 `@src` 之后输入框里变成 `src/`——`@` 已经被吃掉（用户要的语义），
/// 光靠 [`token`] 就再也认不出「这一段路径正在补全」，面板会直接收起、没法接着往下钻。
/// 所以接受目录时在这里记一笔（锚点列 + 刚插进去的那条路径），只要用户还在这条路径上
/// 编辑就让面板继续列下一层。
#[derive(Debug, Clone)]
pub struct PathSession {
    /// 这段路径从哪一列开始（与 [`token`] 的「替换起点列」同一个口径）
    anchor: (usize, usize),
    /// 上次落进输入框的路径（判断用户是不是还在这条路径上改）
    path: String,
}

impl PathSession {
    /// 接受候选时开/续一个会话（`from` = 刚被替换的那一段的起点，`inserted` = 插进去的文本）。
    pub fn new(from: (usize, usize), inserted: &str) -> PathSession {
        PathSession {
            anchor: from,
            path: inserted.to_string(),
        }
    }

    pub fn anchor(&self) -> (usize, usize) {
        self.anchor
    }

    /// 光标还落在这段路径里吗？在就给片段，否则 `None`（= 会话该结束了）。
    ///
    /// 四条：同一行、光标不早于锚点、这一段非空且没有空白（打了空格 = 去写别的了）、
    /// 而且它得和上次那条路径「同一条」——要么是它的前缀（用户删了几个字），
    /// 要么以它开头（用户接着往下打）。否则（比如全选后重打）会话就地结束，
    /// 不会在别的文本上莫名其妙弹面板。
    pub fn fragment(&self, line: &str, cursor: (usize, usize)) -> Option<String> {
        if cursor.0 != self.anchor.0 || cursor.1 < self.anchor.1 {
            return None;
        }
        let region: String = line
            .chars()
            .skip(self.anchor.1)
            .take(cursor.1 - self.anchor.1)
            .collect();
        if region.is_empty() || region.chars().any(char::is_whitespace) {
            return None;
        }
        if !self.path.starts_with(&region) && !region.starts_with(&self.path) {
            return None;
        }
        Some(region)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 临时目录（用例自己用 `name` 错开；跑完不删，`/tmp` 里留着看也方便）。
    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pie-rs-files-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("建父目录");
        }
        fs::write(path, body).expect("写文件");
    }

    fn paths(index: &Index) -> Vec<String> {
        index.entries.iter().map(|e| e.path.clone()).collect()
    }

    /// ⚠ 关键语义：**不在 git 仓库里也认 `.gitignore`**（crate 默认只在 git 仓库里生效）。
    /// 这个临时目录没有 `.git` —— 正是要踩这个坑。
    #[test]
    fn index_respects_gitignore_even_outside_a_git_repo() {
        let dir = tmp("index");
        write(&dir, ".gitignore", "build/\n*.log\n!keep.log\n");
        write(&dir, "build/out.o", "");
        write(&dir, "a.rs", "");
        write(&dir, "a.log", "");
        write(&dir, "keep.log", "");
        // 嵌套的 .gitignore 也要生效
        write(&dir, "sub/.gitignore", "gen/\n");
        write(&dir, "sub/gen/x.rs", "");
        write(&dir, "sub/ok.rs", "");
        // 点文件 / 点目录不收
        write(&dir, ".env", "");
        write(&dir, ".hidden/s.txt", "");

        let index = Index::build(&dir);
        let paths = paths(&index);
        assert!(paths.contains(&"a.rs".to_string()), "{paths:?}");
        assert!(paths.contains(&"sub/ok.rs".to_string()), "{paths:?}");
        assert!(paths.contains(&"keep.log".to_string()), "`!` 取反要生效：{paths:?}");
        assert!(!paths.contains(&"a.log".to_string()), "`*.log` 要排除：{paths:?}");
        assert!(
            !paths.iter().any(|p| p.starts_with("build/")),
            "`build/` 要排除：{paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.starts_with("sub/gen/")),
            "嵌套 .gitignore 要生效：{paths:?}"
        );
        assert!(!paths.contains(&".env".to_string()), "点文件不收：{paths:?}");
        assert!(
            !paths.iter().any(|p| p.contains(".hidden")),
            "点目录不收：{paths:?}"
        );
        assert!(!index.truncated());
        assert_eq!(index.root(), dir.as_path());
    }

    /// 三档锚点：`..` / `/` / `~/` 解析成（要列的目录, 插入文本前缀, 名字前缀）。
    ///
    /// 纯字符串解析——**不碰 env**（`HOME` 是进程级的，改它会把并行跑的用例拖下水），
    /// 所以主目录那档直接拿 `config::home_dir()` 当期望值。
    #[test]
    fn external_anchors_resolve_parent_root_and_home() {
        let root = Path::new("/tmp/pie-files-anchor/cwd"); // 不用真存在，只看解析
        let home = crate::config::home_dir().to_string_lossy().replace('\\', "/");

        // `..` / `../…`：相对 root（保留相对写法，`read` 照相对路径能开）
        assert_eq!(
            external("..", root),
            Some((root.join("../"), "../".to_string(), String::new()))
        );
        assert_eq!(
            external("../", root),
            Some((root.join("../"), "../".to_string(), String::new()))
        );
        assert_eq!(
            external("../../s", root),
            Some((root.join("../../"), "../../".to_string(), "s".to_string()))
        );
        // `/` / `/…`：绝对路径，join 直接覆盖 root
        assert_eq!(
            external("/", root),
            Some((PathBuf::from("/"), "/".to_string(), String::new()))
        );
        assert_eq!(
            external("/usr/Lo", root),
            Some((PathBuf::from("/usr"), "/usr/".to_string(), "lo".to_string()))
        );
        // `~` / `~/…`：展开成主目录（`read` 不认 `~`）
        assert_eq!(
            external("~", root),
            Some((
                crate::config::home_dir(),
                format!("{home}/"),
                String::new()
            ))
        );
        assert_eq!(
            external("~/Doc", root),
            Some((
                crate::config::home_dir(),
                format!("{home}/"),
                "doc".to_string()
            ))
        );
        // `~/Doc/`（带斜杠）= 名字输入完了，该列 `~/Doc` 这一层了
        assert_eq!(
            external("~/Doc/", root),
            Some((
                PathBuf::from(format!("{home}/Doc")),
                format!("{home}/Doc/"),
                String::new()
            ))
        );
        // 其余（含空片段）不归这三档 → 走索引
        assert!(external("src/", root).is_none());
        assert!(external("app", root).is_none());
        assert!(external("", root).is_none());
        assert!(external("./x", root).is_none());
    }

    /// 三档锚点实时列目录：一层、前缀过滤、点文件不收、目录带 `/`。
    /// 关键：**没索引也有候选**（不必等 `@` 那次扫盘回来）。
    #[test]
    fn external_anchors_list_directories_without_an_index() {
        let dir = tmp("external");
        write(&dir, "a.txt", "");
        write(&dir, "sub/b.txt", "");
        write(&dir, ".hidden", "");
        let cwd = dir.join("cwd");
        fs::create_dir_all(&cwd).expect("建 cwd");
        write(&dir, "cwd/inner.txt", "");

        let ins = |frag: &str| -> Vec<String> {
            matches(frag, None, &cwd, MATCH_LIMIT)
                .into_iter()
                .map(|(i, _)| i)
                .collect()
        };

        // `@..`：列上级（= dir）的直接子项，字典序、点文件不收
        assert_eq!(ins(".."), ["../a.txt", "../cwd/", "../sub/"]);
        assert_eq!(ins("../"), ["../a.txt", "../cwd/", "../sub/"]);
        // 前缀过滤（大小写不敏感）；目录候选带「目录」说明
        assert_eq!(ins("../S"), ["../sub/"]);
        assert_eq!(matches("../s", None, &cwd, MATCH_LIMIT)[0].1, "目录");
        // 再往下钻：`../cwd/` 列的就是 cwd 自己那一层
        assert_eq!(ins("../cwd/"), ["../cwd/inner.txt"]);
        // `@/`：真列根目录——至少能给候选，且都是绝对路径
        let root_items = matches("/", None, &cwd, MATCH_LIMIT);
        assert!(!root_items.is_empty(), "根目录不会空");
        assert!(root_items.iter().all(|(i, _)| i.starts_with('/')), "{root_items:?}");
        // `@~/`：展开成主目录（不断言有多少条，只断言前缀对）
        let home_pfx = format!("{}/", crate::config::home_dir().to_string_lossy().replace('\\', "/"));
        let home_items = matches("~/", None, &cwd, MATCH_LIMIT);
        assert!(
            home_items.iter().all(|(i, _)| i.starts_with(&home_pfx)),
            "{home_items:?}"
        );
        // 读不了的目录：空候选，不报错
        assert!(matches("~/../__pie_nope__/", None, &cwd, MATCH_LIMIT).is_empty());
        // 三档之外的片段没有索引 = 没候选（这一步本来就该等扫盘）
        assert!(ins("src").is_empty());
    }

    /// 普通相对路径仍然走索引（三档锚点不把索引那套挤掉）。
    #[test]
    fn index_still_serves_plain_relative_fragments() {
        let dir = tmp("both");
        write(&dir, "src/tui/app.rs", "");
        let index = Index::build(&dir);
        let ins = |frag: &str| -> Vec<String> {
            matches(frag, Some(&index), Path::new("/nope"), MATCH_LIMIT)
                .into_iter()
                .map(|(i, _)| i)
                .collect()
        };
        // 有索引：片段走了索引（root 也用索引自己的 root，与 `/nope` 无关）
        assert_eq!(ins("src/"), ["src/tui/"]);
        assert_eq!(ins("app"), ["src/tui/app.rs"]);
        // 三档锚点就算有索引也走实时列目录（`..` = tmp 的上级）
        assert!(ins("..").iter().all(|i| i.starts_with("../")), "{:?}", ins(".."));
    }

    /// 三种匹配模式：模糊 basename / shell 式目录前缀 / 空片段只列根目录。
    #[test]
    fn matches_fuzzy_basename_dir_prefix_and_root_listing() {
        let dir = tmp("match");
        write(&dir, "README.md", "");
        write(&dir, "src/tui/app.rs", "");
        write(&dir, "src/tui/palette.rs", "");
        write(&dir, "src/session.rs", "");
        write(&dir, "docs/guide.md", "");
        let index = Index::build(&dir);
        let ins = |frag: &str| -> Vec<String> {
            index
                .matches(frag, MATCH_LIMIT)
                .into_iter()
                .map(|(i, _)| i)
                .collect()
        };

        // 空片段：只列 cwd 直接子项（目录带 `/`）
        assert_eq!(ins(""), vec!["README.md", "docs/", "src/"]);
        // 模糊：basename 前缀优先，同档短路径优先
        assert_eq!(ins("app"), vec!["src/tui/app.rs"]);
        assert_eq!(ins("rs")[0], "src/session.rs", "basename 包含：{:?}", ins("rs"));
        assert!(ins("tui").contains(&"src/tui/".to_string()), "{:?}", ins("tui"));
        // 含 `/`：只列那个目录的直接子项（更深的不列），字典序
        assert_eq!(ins("src/"), vec!["src/session.rs", "src/tui/"]);
        assert_eq!(ins("src/t"), vec!["src/tui/"]);
        assert_eq!(ins("src/tui/a"), vec!["src/tui/app.rs"]);
        assert!(ins("src/nope").is_empty());
        // 大小写不敏感
        assert_eq!(ins("readme"), vec!["README.md"]);
        // 「路径包含」那档兜底：`pie-py` 这种只出现在中间的名字也能找到
        let dir2 = tmp("match-path");
        write(&dir2, "bindings/pie-py/setup.py", "");
        let hit: Vec<String> = Index::build(&dir2)
            .matches("pie-py", MATCH_LIMIT)
            .into_iter()
            .map(|(i, _)| i)
            .collect();
        assert!(hit.contains(&"bindings/pie-py/setup.py".to_string()), "{hit:?}");
        assert!(hit.contains(&"bindings/pie-py/".to_string()), "{hit:?}");
        // 目录候选带「目录」说明、插进去带 `/`；文件不带说明
        let (insert, desc) = index.matches("src/", MATCH_LIMIT)[1].clone();
        assert_eq!((insert.as_str(), desc.as_str()), ("src/tui/", "目录"));
        assert_eq!(index.matches("app", MATCH_LIMIT)[0].1, "");
    }

    /// 超上限就截断（只是别把内存吃光，`truncated` 让上层能说一句）。
    #[test]
    fn index_stops_at_the_entry_cap() {
        let dir = tmp("cap");
        for i in 0..5 {
            write(&dir, &format!("f{i}.txt"), "");
        }
        let index = Index::build_capped(&dir, 3);
        assert_eq!(index.len(), 3);
        assert!(index.truncated());
        // 没撞上限就不算截断
        assert!(!Index::build_capped(&dir, 5).truncated());
    }

    /// `@` token：起点是 `@` 那一列（接受时连它一起替换）。
    #[test]
    fn token_takes_the_last_at_before_the_cursor() {
        assert_eq!(token("@src", 4), Some((0, "src".to_string())));
        // 行中 / 行尾都行
        assert_eq!(
            token("看看 @src/tu 的实现", 9),
            Some((3, "src/t".to_string()))
        );
        // 光标前的最后一个 `@`
        assert_eq!(token("@a @b", 6), Some((3, "b".to_string())));
        // 空白之后就断掉（片段里不会有空格）
        assert_eq!(token("@a b", 4), None);
        // `@@a`：认后一个 `@`（片段 "a"）
        assert_eq!(token("@@a", 3), Some((1, "a".to_string())));
        // 邮箱 / 标识符：`@` 前面是 **ASCII** 字母数字或 . - _ 就不认
        assert_eq!(token("a@b.com", 7), None);
        assert_eq!(token("foo@bar", 7), None);
        // 汉字在 `@` 前不算「邮箱」：中文句子里跟着打 `@` 是最常用的姿势
        assert_eq!(token("看下@src", 7), Some((2, "src".to_string())));
        // 光标在 `@` 左边（还没打）
        assert_eq!(token("@a", 0), None);
        // 光秃秃的 `@`：片段为空 —— 这正是「输入 @ 就弹面板」那个入口
        assert_eq!(token("没有@", 5), Some((2, String::new())));
        assert_eq!(token("@", 1), Some((0, String::new())));
    }

    /// 「目录补全后接着往下钻」的锚点：只活在**那条路径**上（前缀或延续），其余一概结束。
    #[test]
    fn path_session_lives_only_on_that_same_path() {
        let session = PathSession::new((0, 3), "src/"); // 「看下 src/」里 `s` 在第 3 列
        assert_eq!(session.anchor(), (0, 3));
        let frag = |line: &str, col: usize| session.fragment(line, (0, col));

        // 接着往下打、或者删回前缀：都还在会话里（片段就是光标前那一段）
        assert_eq!(frag("看下 src/tu 的实现", 9).as_deref(), Some("src/tu"));
        assert_eq!(frag("看下 src/", 7).as_deref(), Some("src/"));
        assert_eq!(frag("看下 sr", 5).as_deref(), Some("sr"));
        assert_eq!(frag("看下 src/tui/ 里", 11).as_deref(), Some("src/tui/"));

        // 打了空白 = 去写别的了；全选重打 = 跟这条路径没关系了；空、光标跑到锚点前、换行
        assert_eq!(frag("看下 src/ 的实现", 8), None);
        assert_eq!(frag("hello", 5), None);
        assert_eq!(frag("看下 src/", 2), None);
        assert_eq!(session.fragment("看下 src/", (1, 7)), None);
        assert_eq!(frag("看下 src/", 3), None);
    }
}
