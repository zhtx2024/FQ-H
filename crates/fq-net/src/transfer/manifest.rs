//! 传输清单构建与不可信路径消毒。
//!
//! # 安全要点
//!
//! 清单里的 `path` 字段来自**不可信的对端**。接收方必须先用
//! [`sanitize_entry_path`] 消毒,才能拼接到本地下载目录 —— 否则一个
//! `../../../Users/x/Startup/foo.bat` 就能写任意文件。
//!
//! 消毒规则(全部拒绝):
//! * 绝对路径、反斜杠、冒号(Windows 盘符/NTFS 流)、NUL
//! * 空组件、`.`、`..`
//! * 超长组件(>255 字节)、过深嵌套(>64 层)
//! * Windows 保留设备名(CON/PRN/AUX/NUL/COM1-9/LPT1-9,含带扩展名形式)

use std::path::{Path, PathBuf};

use fq_proto::{FileEntry, FileKind, FileManifest};

use crate::error::{Error, Result};

/// 单个路径组件的最大字节数。
pub const MAX_COMPONENT_LEN: usize = 255;
/// 路径最大层数。
pub const MAX_PATH_COMPONENTS: usize = 64;

/// 消毒清单条目路径,返回安全的相对路径组件。
pub fn sanitize_entry_path(raw: &str) -> Result<Vec<String>> {
    if raw.is_empty() {
        return Err(Error::Protocol("清单路径为空".into()));
    }
    if raw.starts_with('/') || raw.starts_with('\\') {
        return Err(Error::Protocol(format!("拒绝绝对路径: {raw:?}")));
    }
    if raw.contains('\\') {
        return Err(Error::Protocol(format!("拒绝反斜杠分隔的路径: {raw:?}")));
    }
    if raw.contains(':') {
        return Err(Error::Protocol(format!("拒绝包含冒号的路径: {raw:?}")));
    }
    if raw.contains('\0') {
        return Err(Error::Protocol("路径包含 NUL".into()));
    }

    let mut parts = Vec::new();
    for component in raw.split('/') {
        if component.is_empty() {
            return Err(Error::Protocol(format!("路径包含空组件: {raw:?}")));
        }
        if component == "." || component == ".." {
            return Err(Error::Protocol(format!("拒绝相对路径组件: {raw:?}")));
        }
        if component.len() > MAX_COMPONENT_LEN {
            return Err(Error::Protocol(format!("路径组件过长: {component:?}")));
        }
        if is_windows_reserved(component) {
            return Err(Error::Protocol(format!("拒绝 Windows 保留设备名: {component:?}")));
        }
        parts.push(component.to_string());
    }
    if parts.len() > MAX_PATH_COMPONENTS {
        return Err(Error::Protocol(format!("路径嵌套过深({} 层)", parts.len())));
    }
    Ok(parts)
}

/// Windows 保留设备名(不区分大小写,含带扩展名的形式,如 `CON.txt`)。
fn is_windows_reserved(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component).to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL"
            | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
            | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
    )
}

/// 构建结果:清单 + 与 entries 一一对应的本地源路径(目录条目对应目录本身)。
#[derive(Debug)]
pub struct ManifestSource {
    /// 传输清单。
    pub manifest: FileManifest,
    /// 与 `manifest.entries` 对齐的本地绝对路径。
    pub sources: Vec<PathBuf>,
}

/// 遍历本地路径(单文件或目录)构建传输清单。
///
/// 哈希不在此时计算 —— 发送时流式计算,避免大文件二次读取。
/// 符号链接直接拒绝(不跟随、不传输),防止清单内容与实际读取不一致。
pub async fn build_manifest(root: &Path) -> Result<ManifestSource> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || build_manifest_blocking(&root))
        .await
        .map_err(|e| Error::Protocol(format!("清单构建任务失败: {e}")))?
}

fn build_manifest_blocking(root: &Path) -> Result<ManifestSource> {
    let meta = std::fs::symlink_metadata(root)
        .map_err(|e| Error::Protocol(format!("读取 {}: {e}", root.display())))?;
    if meta.is_symlink() {
        return Err(Error::Protocol("不支持发送符号链接".into()));
    }

    let root_name = root
        .file_name()
        .ok_or_else(|| Error::Protocol("根路径没有文件名".into()))?
        .to_string_lossy()
        .to_string();
    // 根名本身也必须是安全组件(接收端会用它建目录/文件)
    sanitize_entry_path(&root_name)?;

    let mut entries = Vec::new();
    let mut sources = Vec::new();
    if meta.is_file() {
        entries.push(file_entry(&root_name, &meta));
        sources.push(root.to_path_buf());
    } else if meta.is_dir() {
        // 根目录自身也是一个条目(接收方据此创建根目录;空目录也能保留)
        entries.push(dir_entry(&root_name, &meta));
        sources.push(root.to_path_buf());
        walk_dir(root, &root_name, &mut entries, &mut sources)?;
    } else {
        return Err(Error::Protocol("不支持的对象类型".into()));
    }

    // 确定性顺序(父目录天然排在子路径前),便于双方对账与测试
    let mut paired: Vec<_> = entries.into_iter().zip(sources).collect();
    paired.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    let total_bytes = paired.iter().map(|(entry, _)| entry.size).sum();
    let (entries, sources): (Vec<FileEntry>, Vec<PathBuf>) = paired.into_iter().unzip();

    Ok(ManifestSource {
        manifest: FileManifest {
            root_name,
            total_bytes,
            entries,
        },
        sources,
    })
}

fn walk_dir(
    dir: &Path,
    prefix: &str,
    entries: &mut Vec<FileEntry>,
    sources: &mut Vec<PathBuf>,
) -> Result<()> {
    let children = std::fs::read_dir(dir)
        .map_err(|e| Error::Protocol(format!("读取目录 {}: {e}", dir.display())))?;
    for child in children {
        let child = child.map_err(|e| Error::Protocol(format!("枚举目录失败: {e}")))?;
        let name = child.file_name().to_string_lossy().to_string();
        sanitize_entry_path(&name)?;
        let rel = format!("{prefix}/{name}");
        let path = child.path();
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| Error::Protocol(format!("读取 {}: {e}", path.display())))?;
        if meta.is_symlink() {
            return Err(Error::Protocol(format!("不支持符号链接: {path:?}")));
        } else if meta.is_dir() {
            entries.push(dir_entry(&rel, &meta));
            sources.push(path.clone());
            walk_dir(&path, &rel, entries, sources)?;
        } else if meta.is_file() {
            entries.push(file_entry(&rel, &meta));
            sources.push(path);
        } else {
            return Err(Error::Protocol(format!("不支持的对象类型: {path:?}")));
        }
    }
    Ok(())
}

fn file_entry(rel: &str, meta: &std::fs::Metadata) -> FileEntry {
    FileEntry {
        path: rel.to_string(),
        size: meta.len(),
        mtime_ms: mtime_ms_of(meta),
        kind: FileKind::File,
        sha256: None,
    }
}

fn dir_entry(rel: &str, meta: &std::fs::Metadata) -> FileEntry {
    FileEntry {
        path: rel.to_string(),
        size: 0,
        mtime_ms: mtime_ms_of(meta),
        kind: FileKind::Dir,
        sha256: None,
    }
}

fn mtime_ms_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn normal_relative_paths_pass() {
        assert_eq!(
            sanitize_entry_path("a.txt").unwrap(),
            vec!["a.txt".to_string()]
        );
        assert_eq!(
            sanitize_entry_path("设计稿/子目录/封面.png").unwrap(),
            vec!["设计稿".to_string(), "子目录".to_string(), "封面.png".to_string()]
        );
    }

    #[test]
    fn traversal_attempts_are_rejected() {
        for evil in [
            "../escape.txt",
            "a/../../escape.txt",
            "..",
            ".",
            "a/./b",
            "a//b",
            "",
            "/etc/passwd",
            "\\windows\\system32",
            "C:\\boot.ini",
            "a/b:c",
            "a\0b",
        ] {
            assert!(
                sanitize_entry_path(evil).is_err(),
                "必须拒绝路径穿越尝试: {evil:?}"
            );
        }
    }

    #[test]
    fn windows_reserved_device_names_are_rejected() {
        for evil in ["CON", "con.txt", "PRN.log", "AUX", "NUL.dat", "COM1", "lpt9.old"] {
            assert!(
                sanitize_entry_path(evil).is_err(),
                "必须拒绝保留设备名: {evil:?}"
            );
        }
        // 普通名字不受影响
        assert!(sanitize_entry_path("contact.txt").is_ok());
        assert!(sanitize_entry_path("console-style.md").is_ok());
    }

    #[test]
    fn depth_and_length_limits_are_enforced() {
        let too_deep = vec!["d"; MAX_PATH_COMPONENTS + 1].join("/");
        assert!(sanitize_entry_path(&too_deep).is_err());
        let too_long = "a".repeat(MAX_COMPONENT_LEN + 1);
        assert!(sanitize_entry_path(&too_long).is_err());
        let deep_ok = vec!["d"; MAX_PATH_COMPONENTS].join("/");
        assert!(sanitize_entry_path(&deep_ok).is_ok());
    }

    #[tokio::test]
    async fn manifest_of_single_file() {
        let dir = std::env::temp_dir().join(format!("fq-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");
        std::fs::write(&file, b"hello").unwrap();

        let built = build_manifest(&file).await.unwrap();
        assert_eq!(built.manifest.root_name, "hello.txt");
        assert_eq!(built.manifest.total_bytes, 5);
        assert_eq!(built.manifest.entries.len(), 1);
        assert_eq!(built.manifest.entries[0].path, "hello.txt");
        assert_eq!(built.manifest.entries[0].size, 5);
        assert!(built.manifest.entries[0].is_file());
        assert_eq!(built.sources, vec![file.clone()]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn manifest_of_directory_preserves_tree() {
        let dir = std::env::temp_dir().join(format!("fq-manifest-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("root/子目录")).unwrap();
        std::fs::write(dir.join("root/a.txt"), b"aaa").unwrap();
        std::fs::write(dir.join("root/子目录/b.txt"), b"bb").unwrap();

        let built = build_manifest(&dir.join("root")).await.unwrap();
        assert_eq!(built.manifest.root_name, "root");
        let paths: Vec<&str> = built.manifest.entries.iter().map(|e| e.path.as_str()).collect();
        // 根目录与全部子项都在,父目录天然排在子路径前,空目录保留,字典序确定
        assert_eq!(
            paths,
            vec!["root", "root/a.txt", "root/子目录", "root/子目录/b.txt"]
        );
        assert_eq!(built.manifest.total_bytes, 5);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn symlink_is_refused() {
        let dir = std::env::temp_dir().join(format!("fq-manifest-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("real.txt"), b"x").unwrap();
        #[cfg(windows)]
        let link = std::os::windows::fs::symlink_file(dir.join("real.txt"), dir.join("link.txt"));
        #[cfg(not(windows))]
        let link = std::os::unix::fs::symlink(dir.join("real.txt"), dir.join("link.txt"));
        link.unwrap();

        let result = build_manifest(&dir.join("link.txt")).await;
        assert!(result.is_err(), "符号链接必须被拒绝");

        let whole = build_manifest(&dir).await;
        assert!(whole.is_err(), "目录中包含符号链接也必须被拒绝");

        std::fs::remove_dir_all(&dir).ok();
    }
}
