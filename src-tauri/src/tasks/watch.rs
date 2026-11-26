//! 本地文件系统 watch（inotify 递归）— 官方 fnotify/watch 的 Linux 实现。
//! 过滤内置规则后发出本地变更事件，与 NAS 事件在 task actor 合并。

use crate::tasks::file_filter::FileFilter;
use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalFileChange {
    pub event_type: String, // Create | Modify | Remove | Rename
    pub relative_path: String,
    pub old_relative_path: Option<String>, // Rename 来源
    pub is_dir: bool,
    pub file_size: Option<u64>,
    pub fmtime: i64,
}

pub struct LocalWatcher {
    _watcher: RecommendedWatcher,
}

impl LocalWatcher {
    /// 递归 watch sync_root，将过滤后的事件发到 tx。
    pub fn start(
        root: PathBuf,
        filter: FileFilter,
        tx: mpsc::Sender<LocalFileChange>,
    ) -> Result<Self> {
        let (notify_tx, notify_rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = notify_tx.send(res);
        })?;
        watcher.watch(&root, RecursiveMode::Recursive)?;
        // 事件循环（独立线程 → tokio channel）
        std::thread::spawn(move || {
            while let Ok(res) = notify_rx.recv() {
                let Ok(ev) = res else { continue };
                for path in ev.paths {
                    let Ok(rel) = path.strip_prefix(&root) else {
                        continue;
                    };
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    if rel_str.is_empty() {
                        continue;
                    }
                    let is_dir = path.is_dir();
                    let (event_type, old) = match ev.kind {
                        notify::EventKind::Create(_) => ("Create", None),
                        notify::EventKind::Modify(notify::event::ModifyKind::Name(
                            notify::event::RenameMode::Both,
                        )) => ("Rename", None),
                        notify::EventKind::Modify(notify::event::ModifyKind::Name(_)) => {
                            ("Rename", None)
                        }
                        notify::EventKind::Modify(_) => ("Modify", None),
                        notify::EventKind::Remove(_) => ("Remove", None),
                        _ => continue,
                    };
                    let file_size = if is_dir {
                        None
                    } else {
                        std::fs::metadata(&path).ok().map(|m| m.len())
                    };
                    let fmtime = std::fs::metadata(&path)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    if event_type != "Remove" {
                        // 存在的路径按过滤规则判定（含大小）
                        if !filter.should_sync(&rel_str, file_size) {
                            continue;
                        }
                    } else if !filter.should_watch_dir(rel_str.rsplit('/').nth(1).unwrap_or(""))
                        && rel_str.contains('/')
                    {
                        // 父目录被过滤时忽略删除事件
                        continue;
                    }
                    let change = LocalFileChange {
                        event_type: event_type.to_string(),
                        relative_path: rel_str.clone(),
                        old_relative_path: old.map(str::to_string),
                        is_dir,
                        file_size,
                        fmtime,
                    };
                    if tx.blocking_send(change).is_err() {
                        return;
                    }
                }
            }
        });
        Ok(Self { _watcher: watcher })
    }
}

/// 元数据探针（moka 缓存的替代：轻量 LRU）。
pub struct MetaCache {
    inner: lru::LruCache<String, (i64, u64)>, // path -> (mtime_ms, size)
    cap: usize,
}

impl MetaCache {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: lru::LruCache::new(std::num::NonZeroUsize::new(cap.max(1)).unwrap()),
            cap,
        }
    }

    pub fn get_mtime_size(&mut self, path: &Path) -> Option<(i64, u64)> {
        let key = path.to_string_lossy().to_string();
        if let Some(v) = self.inner.get(&key) {
            return Some(*v);
        }
        let md = std::fs::metadata(path).ok()?;
        let mtime = md
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as i64;
        let v = (mtime, md.len());
        if self.cap > 0 {
            self.inner.put(key, v);
        }
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_cache_reads_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, b"hello").unwrap();
        let mut cache = MetaCache::new(10);
        let (mtime, size) = cache.get_mtime_size(&f).unwrap();
        assert_eq!(size, 5);
        assert!(mtime > 0);
    }
}
