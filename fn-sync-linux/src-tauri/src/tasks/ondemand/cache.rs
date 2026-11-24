//! 内容缓存 —— 本地持久缓存 + LRU 淘汰 + 脏文件元数据落盘。
//!
//! 目录布局：
//! ```text
//! <cache_root>/
//!   .meta.json        仅脏文件（未上传的本地修改），用于崩溃/重启后补传
//!   .tmp/<uuid>       下载/写入暂存
//!   <rel 路径原样>    缓存内容文件
//! ```

use anyhow::Result;
use dashmap::DashMap;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// 单个文件在缓存中的状态。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheState {
    /// 本地缓存文件大小
    pub size: u64,
    /// 有未上传的本地修改
    pub dirty: bool,
    /// 正在上传
    pub uploading: bool,
    /// 下载/最近一次上传时的远端 (size, mtim) 戳
    pub stamp: (u64, i64),
    /// 本地文件 mtime（秒）
    pub mtime: i64,
    /// 上一次访问时刻（时钟毫秒，LRU）
    pub last_access_ms: u64,
}

pub struct CacheMgr {
    root: PathBuf,
    limit: u64,
    entries: DashMap<String, CacheState>,
    /// 每个相对路径一把互斥锁：串行化 下载 / 淘汰 / 上传 对同一文件的竞争
    locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    clock_ms: AtomicU64,
}

impl CacheMgr {
    pub fn new(root: PathBuf, limit: u64) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(root.join(".tmp"))?;
        Ok(Self {
            root,
            limit,
            entries: DashMap::new(),
            locks: DashMap::new(),
            clock_ms: AtomicU64::new(0),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn now_ms(&self) -> u64 {
        self.clock_ms.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 校验并拼接相对路径；拒绝越界（.. 等）成分。
    pub fn sanitize_rel(rel: &str) -> Result<String> {
        let trimmed = rel.trim_matches('/');
        if trimmed.is_empty() {
            return Ok(String::new());
        }
        let p = Path::new(trimmed);
        for c in p.components() {
            match c {
                Component::Normal(_) => {}
                _ => anyhow::bail!("invalid relative path: {rel:?}"),
            }
        }
        Ok(trimmed.to_string())
    }

    pub fn file_path(&self, rel: &str) -> PathBuf {
        self.root.join(Self::sanitize_rel(rel).unwrap_or_default())
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join(".tmp")
    }

    pub fn tmp_path(&self) -> PathBuf {
        self.tmp_dir().join(uuid::Uuid::new_v4().to_string())
    }

    pub fn state(&self, rel: &str) -> Option<CacheState> {
        self.entries.get(rel).map(|s| *s)
    }

    /// 确保状态记录存在（写路径可能落在未缓存的占位文件上）。
    /// stamp 表示“本地内容对应的远端版本”，未下载过即为 (0,0)。
    pub fn ensure_state(&self, rel: &str, stamp: (u64, i64), mtime: i64) {
        if !self.entries.contains_key(rel) {
            self.set_state(
                rel,
                CacheState {
                    size: 0,
                    dirty: true,
                    uploading: false,
                    stamp,
                    mtime,
                    last_access_ms: self.now_ms(),
                },
            );
        }
    }

    pub fn set_state(&self, rel: &str, state: CacheState) {
        self.entries.insert(rel.to_string(), state);
    }

    pub fn update<F: FnOnce(&mut CacheState)>(&self, rel: &str, f: F) -> Option<CacheState> {
        let mut e = self.entries.get_mut(rel)?;
        f(&mut e);
        Some(*e)
    }

    pub fn remove(&self, rel: &str) {
        self.entries.remove(rel);
        self.locks.remove(rel);
    }

    pub fn downloaded(&self, rel: &str) -> bool {
        self.entries.contains_key(rel)
    }

    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// 所有脏文件的相对路径。
    pub fn dirty_entries(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.value().dirty)
            .map(|e| e.key().clone())
            .collect()
    }

    pub fn touch(&self, rel: &str) {
        let ms = self.now_ms();
        self.update(rel, |s| s.last_access_ms = ms);
    }

    pub fn lock_for(&self, rel: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .entry(rel.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// 当前缓存总字节（含脏文件）。
    pub fn total_size(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    /// 按 LRU 淘汰干净且未被引用的文件，直到总量 ≤ `limit`。
    /// 返回应删除缓存文件的相对路径列表（调用方负责删文件并回执）。
    pub fn plan_eviction(
        &self,
        pinned: &dyn Fn(&str) -> bool,
        in_use: &dyn Fn(&str) -> bool,
    ) -> Vec<String> {
        let mut total = self.total_size();
        if total <= self.limit {
            return Vec::new();
        }
        let now = self.now_ms();
        let mut candidates: Vec<(u64, String, u64)> = self
            .entries
            .iter()
            .filter(|e| {
                let s = e.value();
                !s.dirty && !s.uploading && !pinned(e.key()) && !in_use(e.key())
            })
            .map(|e| {
                // 访问时刻越久远越先淘汰；从未访问过的用反向时钟压到最后
                let age = if e.last_access_ms == 0 {
                    u64::MAX - total
                } else {
                    now.saturating_sub(e.last_access_ms)
                };
                (age, e.key().clone(), e.size)
            })
            .collect();
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        let mut evicted = Vec::new();
        for (_, rel, size) in candidates {
            if total <= self.limit {
                break;
            }
            self.entries.remove(&rel);
            self.locks.remove(&rel);
            total = total.saturating_sub(size);
            evicted.push(rel);
        }
        evicted
    }

    /// 把脏文件元数据原子落盘（崩溃后重启补传用）。
    pub fn persist_meta(&self) {
        let mut map = HashMap::new();
        for e in self.entries.iter() {
            if e.dirty {
                map.insert(
                    e.key().clone(),
                    json!({"size": e.size, "mtime": e.mtime, "stamp": [e.stamp.0, e.stamp.1]}),
                );
            }
        }
        let path = self.root.join(".meta.json");
        let tmp = self.root.join(".meta.json.tmp");
        let body = json!({ "dirty": map }).to_string();
        if std::fs::write(&tmp, body.as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// 启动时恢复脏文件状态（内容文件必须仍在磁盘上）。
    pub fn load_meta(&self) {
        let path = self.root.join(".meta.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return;
        };
        let Some(dirty) = value.get("dirty").and_then(|d| d.as_object()) else {
            return;
        };
        for (rel, meta) in dirty {
            if !Self::sanitize_rel(rel).map(|s| s == *rel).unwrap_or(false) {
                continue;
            }
            let file = self.root.join(rel);
            let Ok(file_meta) = std::fs::metadata(&file) else {
                continue;
            };
            let size = meta.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
            let mtime = meta.get("mtime").and_then(|v| v.as_i64()).unwrap_or(0);
            let stamp = meta
                .get("stamp")
                .and_then(|v| v.as_array())
                .map(|a| {
                    (
                        a.first().and_then(|v| v.as_u64()).unwrap_or(0),
                        a.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                    )
                })
                .unwrap_or((0, 0));
            self.entries.insert(
                rel.clone(),
                CacheState {
                    size: if file_meta.len() > 0 { file_meta.len() } else { size },
                    dirty: true,
                    uploading: false,
                    stamp,
                    mtime,
                    last_access_ms: 0,
                },
            );
        }
    }

    /// 清掉 tmp 残留。
    pub fn clean_tmp(&self) {
        if let Ok(rd) = std::fs::read_dir(self.tmp_dir()) {
            for entry in rd.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mgr() -> CacheMgr {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache");
        // TempDir 泄漏保住目录生命周期；进程结束后由 OS 清理
        std::mem::forget(tmp);
        CacheMgr::new(path, 100).unwrap()
    }

    #[test]
    fn sanitize_rejects_traversal() {
        assert_eq!(CacheMgr::sanitize_rel("a/b.txt").unwrap(), "a/b.txt");
        assert_eq!(CacheMgr::sanitize_rel("/a/b/").unwrap(), "a/b");
        assert!(CacheMgr::sanitize_rel("../etc").is_err());
        assert!(CacheMgr::sanitize_rel("a/../../b").is_err());
        assert!(CacheMgr::sanitize_rel("").unwrap().is_empty());
    }

    #[test]
    fn state_roundtrip_and_eviction() {
        let mgr = mgr();
        let ms = mgr.now_ms();
        mgr.set_state(
            "a.bin",
            CacheState {
                size: 60,
                dirty: false,
                uploading: false,
                stamp: (60, 1),
                mtime: 1,
                last_access_ms: ms,
            },
        );
        let ms2 = mgr.now_ms();
        mgr.set_state(
            "b.bin",
            CacheState {
                size: 60,
                dirty: false,
                uploading: false,
                stamp: (60, 1),
                mtime: 1,
                last_access_ms: ms2,
            },
        );
        assert_eq!(mgr.total_size(), 120);
        // 上限 100 → 淘汰最久未用的 a.bin
        let evicted = mgr.plan_eviction(&|_| false, &|_| false);
        assert_eq!(evicted, vec!["a.bin".to_string()]);
        assert_eq!(mgr.total_size(), 60);
    }

    #[test]
    fn eviction_respects_dirty_and_pinned() {
        let mgr = mgr();
        for (name, dirty) in [("d.bin", true), ("p.bin", false)] {
            mgr.set_state(
                name,
                CacheState {
                    size: 200,
                    dirty,
                    uploading: false,
                    stamp: (200, 1),
                    mtime: 1,
                    last_access_ms: mgr.now_ms(),
                },
            );
        }
        let pinned = |rel: &str| rel == "p.bin";
        let evicted = mgr.plan_eviction(&pinned, &|_| false);
        assert!(evicted.is_empty(), "dirty/pinned 都不能淘汰");
    }

    #[test]
    fn meta_persist_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let mgr = CacheMgr::new(root.clone(), 1000).unwrap();
        std::fs::write(root.join("c.bin"), b"payload").unwrap();
        mgr.set_state(
            "c.bin",
            CacheState {
                size: 7,
                dirty: true,
                uploading: false,
                stamp: (3, 11),
                mtime: 42,
                last_access_ms: 0,
            },
        );
        mgr.persist_meta();
        assert!(root.join(".meta.json").exists());

        let restored = CacheMgr::new(root.clone(), 1000).unwrap();
        restored.load_meta();
        let s = restored.state("c.bin").expect("dirty state restored");
        assert!(s.dirty);
        assert_eq!(s.mtime, 42);
        assert_eq!(s.stamp, (3, 11));
        assert_eq!(s.size, 7); // 文件存在且非空 → 用真实文件大小
    }

    #[test]
    fn load_meta_ignores_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let mgr = CacheMgr::new(root.clone(), 1000).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(".meta.json"),
            json!({"dirty": {"ghost.bin": {"size": 5, "mtime": 1, "stamp": [5, 1]}}}).to_string(),
        )
        .unwrap();
        mgr.load_meta();
        assert!(mgr.state("ghost.bin").is_none(), "文件不在就不恢复");
    }
}
