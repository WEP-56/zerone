//! # Workspace:agent 的"工作目录视角"
//!
//! 所有工具都必须通过 Workspace 访问文件系统,而不是自己调 `std::fs`。
//! 现在它只做三件事:持有 root、解析路径、提供最基础的读写原语。
//!
//! 为什么值得单独一层?因为它是后续大量能力的天然挂载点:
//! - **Guardrails / 沙箱**:在 `resolve()` 里限制路径不得逃出 root;
//! - **Workspace Map**:扫描目录结构生成地图,喂给 ContextProvider;
//! - **虚拟文件系统**:测试时替换成内存实现;
//! - **审计**:在读写原语处统一记录 agent 动过哪些文件。
//!
//! 当前迁移基线尚未限制 workspace 外路径。这里是后续加入 canonical path、
//! symlink 与审批策略的统一边界；在策略落地前不能把它视为安全沙箱。

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

/// 单文件读取上限。防止模型 read 一个几百 MB 的日志把内存与上下文炸掉。
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

impl Workspace {
    pub fn new(root: PathBuf) -> Self {
        Workspace { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 把模型给的路径解析成绝对路径:
    /// - 相对路径 → 相对 root;
    /// - 绝对路径 → 原样使用(本版不做逃逸限制);
    /// - 顺手做词法归一化(消掉 `.` 与可消的 `..`),让显示与去重更干净。
    pub fn resolve(&self, given: &str) -> PathBuf {
        let p = Path::new(given);
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        normalize_lexically(&joined)
    }

    /// 显示用:能表示成相对 root 的就用相对形式,更短更易读。
    pub fn display(&self, p: &Path) -> String {
        match p.strip_prefix(&self.root) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
            _ => p.display().to_string(),
        }
    }

    // ---- 文件访问原语(工具层只允许用这些) ----

    /// 读文本文件。超过大小上限或非 UTF-8 会返回错误(信息面向模型,要可读)。
    pub fn read_text(&self, path: &Path) -> Result<String, String> {
        let meta = fs::metadata(path).map_err(|e| io_err("读取", path, &e))?;
        if !meta.is_file() {
            return Err(format!("{} 不是文件", path.display()));
        }
        if meta.len() > MAX_FILE_BYTES {
            return Err(format!(
                "文件过大({} 字节,上限 {}),请用 run_command 处理",
                meta.len(),
                MAX_FILE_BYTES
            ));
        }
        let bytes = fs::read(path).map_err(|e| io_err("读取", path, &e))?;
        String::from_utf8(bytes)
            .map_err(|_| format!("{} 不是 UTF-8 文本(可能是二进制文件)", path.display()))
    }

    /// 写文本文件,自动创建父目录。返回 (字节数, 是否覆盖了已有文件)。
    pub fn write_text(&self, path: &Path, content: &str) -> Result<(usize, bool), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| io_err("创建目录", parent, &e))?;
            }
        }
        let existed = path.exists();
        fs::write(path, content).map_err(|e| io_err("写入", path, &e))?;
        Ok((content.len(), existed))
    }

    pub fn metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::metadata(path)
    }

    /// 列目录,按"目录在前、名字排序"返回。
    pub fn read_dir_sorted(&self, path: &Path) -> Result<Vec<fs::DirEntry>, String> {
        let mut entries: Vec<fs::DirEntry> = fs::read_dir(path)
            .map_err(|e| io_err("列目录", path, &e))?
            .filter_map(|e| e.ok())
            .collect();
        entries.sort_by_key(|e| {
            let is_file = e.file_type().map(|t| !t.is_dir()).unwrap_or(true);
            (is_file, e.file_name().to_string_lossy().to_lowercase())
        });
        Ok(entries)
    }
}

fn io_err(action: &str, path: &Path, e: &io::Error) -> String {
    format!("{} {} 失败: {}", action, path.display(), e)
}

/// 词法归一化:只处理字符串层面的 `.` / `..`,不碰真实文件系统(不解析符号链接)。
fn normalize_lexically(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // 只在能弹出普通目录名时才消 `..`,否则原样保留
                if !matches!(
                    out.components().next_back(),
                    None | Some(Component::RootDir) | Some(Component::Prefix(_))
                ) {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_relative_and_dots() {
        let ws = Workspace::new(PathBuf::from(r"E:\proj"));
        assert_eq!(
            ws.resolve("src/main.rs"),
            PathBuf::from(r"E:\proj\src\main.rs")
        );
        assert_eq!(
            ws.resolve("./src/../a.txt"),
            PathBuf::from(r"E:\proj\a.txt")
        );
        assert_eq!(ws.resolve(r"D:\abs\x.txt"), PathBuf::from(r"D:\abs\x.txt"));
    }

    #[test]
    fn display_prefers_relative() {
        let ws = Workspace::new(PathBuf::from(r"E:\proj"));
        assert_eq!(ws.display(Path::new(r"E:\proj\src\a.rs")), r"src\a.rs");
        assert_eq!(ws.display(Path::new(r"D:\other\b.rs")), r"D:\other\b.rs");
    }
}
