//! 冲突文件处理 — 官方行为：重命名 "原名 - Conflicted Copy %Y-%m-%d %H-%M-%S"。

use anyhow::Result;
use std::path::{Path, PathBuf};

/// 官方格式串恢复：" - Conflicted Copy "，时间 "%Y-%m-%d %H-%M-%S"
pub const CONFLICT_SUFFIX: &str = " - Conflicted Copy ";
pub const CONFLICT_TIME_FORMAT: &str = "%Y-%m-%d %H-%M-%S";

/// 生成本地冲突文件路径。磁盘存在同名冲突文件时追加序号。
pub fn conflict_path(original: &Path) -> PathBuf {
    let ts = chrono::Local::now().format(CONFLICT_TIME_FORMAT);
    let dir = original.parent().unwrap_or_else(|| Path::new("."));
    let _stem = original
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = original
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    // 官方格式："原名 - Conflicted Copy 时间.扩展名"（原名含扩展名，时间 %Y-%m-%d %H-%M-%S）
    let full = original
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut p = dir.join(format!("{full}{CONFLICT_SUFFIX}{ts}{ext}"));
    let mut n = 1;
    while p.exists() {
        p = dir.join(format!("{full}{CONFLICT_SUFFIX}{ts} ({n}){ext}"));
        n += 1;
    }
    p
}

/// 冲突处理：将本地文件重命名为 Conflicted Copy，保留双方修改。
/// NAS 版本稍后由同步引擎重新下载。
pub fn resolve_conflict_keep_both(local: &Path) -> Result<PathBuf> {
    if !local.exists() {
        anyhow::bail!("conflict base path has no filename");
    }
    let target = conflict_path(local);
    std::fs::rename(local, &target)
        .map_err(|e| anyhow::anyhow!("conflict failed to rename: {e}"))?;
    Ok(target)
}

/// 路径是否位于同步根内（官方 validate_conflict_base_path 语义）。
pub fn validate_conflict_base_path(sync_root: &Path, path: &Path) -> Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("conflict base path must be absolute");
    }
    if path.file_name().is_none() {
        anyhow::bail!("conflict base path has no filename");
    }
    if !path.starts_with(sync_root) {
        anyhow::bail!("conflict base path is outside sync root");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_path_format() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("photo.jpg");
        std::fs::write(&f, b"x").unwrap();
        let p = conflict_path(&f);
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("photo.jpg - Conflicted Copy "));
        // 2026-11-20 09-41-00 形态
        let suffix = &name["photo.jpg - Conflicted Copy ".len()..];
        let ts = suffix.split(".jpg").next().unwrap();
        assert_eq!(ts.len(), 19);
        assert_eq!(ts.chars().nth(4), Some('-'));
        assert_eq!(ts.chars().nth(7), Some('-'));
        assert_eq!(ts.chars().nth(10), Some(' '));
        assert_eq!(ts.chars().nth(13), Some('-'));
    }

    #[test]
    fn keep_both_renames() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, b"local version").unwrap();
        let target = resolve_conflict_keep_both(&f).unwrap();
        assert!(!f.exists());
        assert!(target.exists());
        assert!(std::fs::read(&target).unwrap() == b"local version");
    }

    #[test]
    fn validate_paths() {
        let root = Path::new("/tmp/syncroot");
        assert!(validate_conflict_base_path(root, Path::new("/tmp/syncroot/a/b.txt")).is_ok());
        assert!(validate_conflict_base_path(root, Path::new("/etc/passwd")).is_err());
        assert!(validate_conflict_base_path(root, Path::new("relative.txt")).is_err());
    }
}
