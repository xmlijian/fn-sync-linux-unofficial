//! 同步任务 actor — 本地 inotify 事件 + 远端轮询比对。

use super::conflict;
use super::file_filter::FileFilter;
use super::watch::{LocalFileChange, LocalWatcher};
use crate::connect::nas_client::NasClient;
use crate::service::logs;
use crate::service::tasks::Task;
use anyhow::Result;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// 官方 TaskStatus（前端枚举一致）
pub mod status {
    pub const INIT: i64 = 1;
    pub const SYNCING: i64 = 2;
    pub const PAUSED: i64 = 3;
    pub const COMPLETED: i64 = 4;
    pub const FAILED: i64 = 5;
    pub const SEGMENT_COMPLETED: i64 = 6;
    pub const ERROR: i64 = 7;
    pub const IDLE: i64 = 8;
    pub const ACCOUNT_ERROR: i64 = 9;
    pub const FAILED_LOCAL_DIR: i64 = 10;
    pub const FAILED_NAS_DIR: i64 = 11;
    pub const VERSION_INCORRECT: i64 = 12;
    pub const NAS_NO_ACL_SUPPORT: i64 = 14;
    pub const NAS_NO_PERM: i64 = 15;
    pub const NAS_FILE_NOT_EXISTS: i64 = 16;
    pub const SECURITY_CODE_ERROR: i64 = 17;
}

pub use status as TaskStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    Bi,
    UpOnly,
    DownOnly,
}

impl SyncMode {
    pub fn from_db(mode: &str) -> Self {
        match mode {
            "uploadonly" => Self::UpOnly,
            "downloadonly" => Self::DownOnly,
            _ => Self::Bi,
        }
    }

    pub fn allows_upload(&self) -> bool {
        matches!(self, Self::Bi | Self::UpOnly)
    }

    pub fn allows_download(&self) -> bool {
        matches!(self, Self::Bi | Self::DownOnly)
    }
}

pub struct TaskActor {
    pub task: Task,
    mode: SyncMode,
    local_root: PathBuf,
    filter: FileFilter,
    nas: Mutex<Arc<NasClient>>,
    saved_account: Mutex<Option<crate::service::nas_account::NasAccount>>,
    remote_revisions: Mutex<HashMap<String, String>>,
    synced_hashes: Mutex<HashMap<String, String>>,
    db_pool: sqlx::SqlitePool,
    paused: Mutex<bool>,
    /// 远端最新状态：相对路径 → (大小, mtime)
    remote_state: Mutex<HashMap<String, (u64, i64)>>,
    local_state: Mutex<HashMap<String, (u64, i64, i64)>>,
}

impl TaskActor {
    pub fn new(
        task: Task,
        nas: Arc<NasClient>,
        db_pool: sqlx::SqlitePool,
        filter: FileFilter,
    ) -> Result<Self> {
        let local_root = PathBuf::from(&task.pc_path);
        if !local_root.exists() {
            std::fs::create_dir_all(&local_root).map_err(|e| {
                anyhow::anyhow!("failed to create sync dir {}: {e}", local_root.display())
            })?;
        }
        let local_root = std::fs::canonicalize(local_root)?;
        Ok(Self {
            mode: SyncMode::from_db(&task.mode),
            local_root,
            filter,
            nas: Mutex::new(nas),
            saved_account: Mutex::new(None),
            remote_revisions: Mutex::new(HashMap::new()),
            synced_hashes: Mutex::new(HashMap::new()),
            db_pool,
            task,
            paused: Mutex::new(false),
            remote_state: Mutex::new(HashMap::new()),
            local_state: Mutex::new(HashMap::new()),
        })
    }

    pub fn set_saved_account(&self, acc: crate::service::nas_account::NasAccount) {
        *self.saved_account.lock() = Some(acc);
    }

    async fn client(&self) -> Result<Arc<NasClient>> {
        let current = self.nas.lock().clone();
        if current.is_connected() {
            return Ok(current);
        }
        let account = self
            .saved_account
            .lock()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("saved NAS account unavailable"))?;
        let replacement = NasClient::open(current.config().clone()).await?;
        replacement.restore_session(&account).await?;
        replacement
            .persist_access_token(&self.db_pool, &account)
            .await?;
        let mut saved = account;
        saved.token = replacement.token.read().clone();
        *self.saved_account.lock() = Some(saved);
        *self.nas.lock() = replacement.clone();
        Ok(replacement)
    }

    fn safe_local_path(&self, rel: &str) -> Result<PathBuf> {
        let mut path = self.local_root.clone();
        for part in Path::new(rel).components() {
            if !matches!(part, std::path::Component::Normal(_)) {
                anyhow::bail!("invalid relative sync path");
            }
            path.push(part.as_os_str());
            if let Ok(meta) = std::fs::symlink_metadata(&path) {
                if meta.file_type().is_symlink() {
                    anyhow::bail!("symbolic link in sync destination");
                }
            }
        }
        Ok(path)
    }

    fn local_path(&self, rel: &str) -> PathBuf {
        self.local_root.join(rel.trim_start_matches('/'))
    }

    fn nas_root(&self) -> String {
        self.task.nas_path.trim_matches('/').to_string()
    }

    /// 启动 actor：先做一次全量远端拉取，然后本地事件 + 定时轮询。
    pub async fn run(
        self: Arc<Self>,
        mut local_rx: mpsc::Receiver<LocalFileChange>,
        _watcher: LocalWatcher,
    ) -> Result<()> {
        self.restore_snapshot().await?;
        self.reconcile().await?;
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                change = local_rx.recv() => {
                    let Some(change) = change else { break };
                    let _ = change;
                    if !*self.paused.lock() {
                        if let Err(e) = self.reconcile().await { self.log_exception(&e.to_string()).await; }
                    }
                }
                _ = ticker.tick() => {
                    if !*self.paused.lock() {
                        if let Err(e) = self.reconcile().await { self.log_exception(&e.to_string()).await; }
                    }
                }
            }
        }
        Ok(())
    }

    async fn restore_snapshot(&self) -> Result<()> {
        let key = format!("sync_snapshot_{}", self.task.id);
        if let Some(text) = logs::get_system_value(&self.db_pool, &key).await? {
            let data: serde_json::Value = serde_json::from_str(&text)?;
            if data["version"] == 2
                && data["uuid"] == self.task.uuid
                && data["mode"] == self.task.mode
            {
                *self.local_state.lock() = serde_json::from_value(data["local"].clone())?;
                *self.remote_state.lock() = serde_json::from_value(data["remote"].clone())?;
                *self.remote_revisions.lock() =
                    serde_json::from_value(data["revisions"].clone()).unwrap_or_default();
                *self.synced_hashes.lock() =
                    serde_json::from_value(data["hashes"].clone()).unwrap_or_default();
            }
        }
        Ok(())
    }

    async fn save_snapshot(&self) -> Result<()> {
        let data = serde_json::json!({"version":2,"uuid":self.task.uuid,"mode":self.task.mode,
            "local":self.local_state.lock().clone(),"remote":self.remote_state.lock().clone(),"revisions":self.remote_revisions.lock().clone(),"hashes":self.synced_hashes.lock().clone()});
        logs::set_system_value(
            &self.db_pool,
            &format!("sync_snapshot_{}", self.task.id),
            &data.to_string(),
        )
        .await?;
        Ok(())
    }

    /// 每一轮同步只把状态置一次 SYNCING，避免空轮询时界面在同步中/已完成之间跳动。
    async fn note_work(&self, started: &mut bool) {
        if *started {
            return;
        }
        *started = true;
        let _ =
            crate::service::tasks::update_task_status(&self.db_pool, self.task.id, status::SYNCING)
                .await;
    }

    async fn reconcile(&self) -> Result<()> {
        if *self.paused.lock() {
            return Ok(());
        }
        let mut started = false;
        if self.mode.allows_download() {
            self.poll_remote(&mut started).await?;
        }
        if self.mode.allows_upload() {
            self.sync_local(&mut started).await?;
        }
        self.save_snapshot().await?;
        if !*self.paused.lock() {
            let settled = crate::service::tasks::get_task_by_id(&self.db_pool, self.task.id)
                .await?
                .map(|t| t.status);
            if started || settled != Some(status::COMPLETED) {
                let _ = crate::service::tasks::update_task_status(
                    &self.db_pool,
                    self.task.id,
                    status::COMPLETED,
                )
                .await;
            }
        }
        Ok(())
    }

    fn local_stamp(path: &Path) -> Result<(u64, i64, i64)> {
        let meta = std::fs::metadata(path)?;
        if meta.is_dir() {
            return Ok((0, 0, 0));
        }
        let mt = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos() as i64;
        #[cfg(unix)]
        let changed = {
            use std::os::unix::fs::MetadataExt;
            meta.ctime()
                .saturating_mul(1_000_000_000)
                .saturating_add(meta.ctime_nsec())
        };
        #[cfg(not(unix))]
        let changed = mt;
        Ok((meta.len(), mt, changed))
    }

    async fn sync_local(&self, started: &mut bool) -> Result<()> {
        let paths = self.initial_sync_scan().await?;
        let previous = self.local_state.lock().clone();
        let mut now = HashMap::new();
        for rel in paths {
            if *self.paused.lock() {
                return Ok(());
            }
            let local = self.local_path(&rel);
            let stamp = Self::local_stamp(&local)?;
            let missing_directory = local.is_dir() && !self.remote_state.lock().contains_key(&rel);
            if previous.get(&rel) != Some(&stamp) || missing_directory {
                let is_dir = local.is_dir();
                self.note_work(started).await;
                self.handle_local(LocalFileChange {
                    event_type: "Modify".into(),
                    relative_path: rel.clone(),
                    old_relative_path: None,
                    is_dir,
                    file_size: Some(stamp.0),
                    fmtime: stamp.1,
                })
                .await?;
            }
            now.insert(rel, stamp);
        }
        let mut removed:Vec<_> = previous.keys().filter(|p|self.mode==SyncMode::Bi && !now.contains_key(*p) && matches!(std::fs::symlink_metadata(self.local_path(p)),Err(e) if e.kind()==std::io::ErrorKind::NotFound)).cloned().collect();
        removed.sort_by_key(|p| p.matches('/').count());
        let mut deleted_roots: Vec<String> = Vec::new();
        for rel in removed {
            if deleted_roots
                .iter()
                .any(|parent| rel.starts_with(&format!("{parent}/")))
            {
                continue;
            }
            let tracked = self.remote_state.lock().contains_key(&rel);
            if tracked {
                self.note_work(started).await;
                self.handle_local(LocalFileChange {
                    event_type: "Remove".into(),
                    relative_path: rel.clone(),
                    old_relative_path: None,
                    is_dir: false,
                    file_size: None,
                    fmtime: 0,
                })
                .await?;
                deleted_roots.push(rel);
            }
        }
        *self.local_state.lock() = now;
        Ok(())
    }

    async fn log_exception(&self, text: &str) {
        let _ =
            crate::service::tasks::update_task_status(&self.db_pool, self.task.id, status::ERROR)
                .await;
        let _ = logs::add_log(
            &self.db_pool,
            logs::NewLog {
                task_id: self.task.id,
                is_dir: false,
                name: "",
                path: "",
                sync_folder: &self.task.name,
                action_code: logs::ACTION_EXCEPTION,
                status: logs::STATUS_FAILED,
                failed_code: 0,
                failed_text: Some(text),
                payload: None,
            },
        )
        .await;
    }

    /// 本地变更 → NAS。
    async fn handle_local(&self, change: LocalFileChange) -> Result<()> {
        let nas = self.client().await?;
        let remote = format!("{}/{}", self.nas_root(), change.relative_path);
        match change.event_type.as_str() {
            "Create" | "Modify" => {
                let local = self.local_path(&change.relative_path);
                if change.is_dir || local.is_dir() {
                    nas.mkdir(&remote).await?;
                    self.remote_state
                        .lock()
                        .insert(change.relative_path.clone(), (0, 0));
                } else if local.exists() {
                    let data = std::fs::read(&local)?;
                    let mtime = std::fs::metadata(&local)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64);
                    let hash = content_hash(&data);
                    nas.upload(&remote, data, mtime).await?;
                    self.synced_hashes
                        .lock()
                        .insert(change.relative_path.clone(), hash);
                    self.remote_state.lock().insert(
                        change.relative_path.clone(),
                        (change.file_size.unwrap_or(0), mtime.unwrap_or_default()),
                    );
                    let _ = logs::add_log(
                        &self.db_pool,
                        logs::NewLog {
                            task_id: self.task.id,
                            is_dir: false,
                            name: &file_name(&change.relative_path),
                            path: &change.relative_path,
                            sync_folder: &self.task.name,
                            action_code: logs::ACTION_UPLOAD,
                            status: logs::STATUS_SUCCESS,
                            failed_code: 0,
                            failed_text: None,
                            payload: None,
                        },
                    )
                    .await;
                }
            }
            "Remove" => {
                nas.remove(vec![remote]).await?;
                self.remote_state.lock().remove(&change.relative_path);
                self.synced_hashes.lock().remove(&change.relative_path);
                let _ = logs::add_log(
                    &self.db_pool,
                    logs::NewLog {
                        task_id: self.task.id,
                        is_dir: change.is_dir,
                        name: &file_name(&change.relative_path),
                        path: &change.relative_path,
                        sync_folder: &self.task.name,
                        action_code: logs::ACTION_DELETE,
                        status: logs::STATUS_SUCCESS,
                        failed_code: 0,
                        failed_text: None,
                        payload: None,
                    },
                )
                .await;
            }
            "Rename" => {
                let local = self.local_path(&change.relative_path);
                if local.exists() {
                    // 新路径存在 → 当作新建上传
                    if local.is_dir() {
                        nas.mkdir(&remote).await?;
                    } else {
                        let data = std::fs::read(&local)?;
                        nas.upload(&remote, data, None).await?;
                    }
                } else {
                    nas.remove(vec![remote]).await?;
                    self.remote_state.lock().remove(&change.relative_path);
                    self.synced_hashes.lock().remove(&change.relative_path);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// 拉取远端目录树，下载新增/变更，清理远端已删除且本地未改动的文件。
    async fn poll_remote(&self, started: &mut bool) -> Result<()> {
        let root = self.nas_root();
        let nas = self.client().await?;
        let tree = nas.remote_tree_filtered(&root, Some(&self.filter)).await?;
        let mut now: HashMap<String, (u64, i64)> = HashMap::new();

        let previous = self.remote_state.lock().clone();
        let previous_revisions = self.remote_revisions.lock().clone();
        let mut revisions = HashMap::new();
        for (rel, entry) in tree.iter() {
            if *self.paused.lock() {
                return Ok(());
            }
            if !self.filter.should_sync(
                rel,
                if entry.is_dir() {
                    None
                } else {
                    Some(entry.size)
                },
            ) {
                continue;
            }
            let local = self.safe_local_path(rel)?;
            revisions.insert(rel.clone(), entry.revision.clone());
            if entry.is_dir() {
                if self.mode == SyncMode::Bi
                    && !local.exists()
                    && self.local_state.lock().contains_key(rel)
                {
                    now.insert(rel.clone(), (0, 0));
                    continue;
                }
                if !local.exists() {
                    std::fs::create_dir_all(&local)?;
                }
                self.local_state
                    .lock()
                    .insert(rel.clone(), Self::local_stamp(&local)?);
                now.insert(rel.clone(), (0, 0));
                continue;
            }
            now.insert(rel.clone(), (entry.size, entry.mtim));

            let meta = std::fs::metadata(&local).ok();
            // NAS 文件接口只返回秒级时间。对刚写入的文件复核内容，避免同一秒同大小修改漏检。
            let now_seconds = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;
            let recent = entry.mtim >= now_seconds - 5 || entry.btim >= now_seconds - 5;
            let mut remote_changed = recent
                || previous.get(rel) != Some(&(entry.size, entry.mtim))
                || previous_revisions.get(rel) != Some(&entry.revision);
            let mut downloaded = None;
            if remote_changed {
                let bytes = nas.download(&format!("{root}/{rel}")).await?;
                remote_changed = self.synced_hashes.lock().get(rel) != Some(&content_hash(&bytes));
                downloaded = Some(bytes);
            }
            let local_stamp = Self::local_stamp(&local).ok();
            let local_changed = local_stamp.as_ref() != self.local_state.lock().get(rel);
            if self.mode == SyncMode::Bi
                && meta.is_none()
                && previous.contains_key(rel)
                && !remote_changed
                && self.local_state.lock().contains_key(rel)
            {
                continue;
            }
            let need = meta.is_none()
                || remote_changed
                || (self.mode == SyncMode::DownOnly && local_changed);
            if need {
                self.note_work(started).await;
                let remote = format!("{root}/{rel}");
                let result = if let Some(bytes) = downloaded {
                    Ok(bytes)
                } else {
                    nas.download(&remote).await
                };
                match result {
                    Ok(bytes) => {
                        let local = self.safe_local_path(rel)?;
                        if let Some(parent) = local.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        let current_changed = Self::local_stamp(&local).ok().as_ref()
                            != self.local_state.lock().get(rel);
                        if self.mode == SyncMode::Bi
                            && local.exists()
                            && current_changed
                            && std::fs::read(&local)? != bytes
                        {
                            conflict::resolve_conflict_keep_both(&local)?;
                        }
                        let temp_dir = self.safe_local_path(".download")?;
                        std::fs::create_dir_all(&temp_dir)?;
                        let tmp = temp_dir.join(uuid::Uuid::new_v4().to_string());
                        std::fs::write(&tmp, &bytes)?;
                        std::fs::rename(&tmp, &local)?;
                        self.local_state
                            .lock()
                            .insert(rel.clone(), Self::local_stamp(&local)?);
                        self.synced_hashes
                            .lock()
                            .insert(rel.clone(), content_hash(&bytes));
                        let _ = logs::add_log(
                            &self.db_pool,
                            logs::NewLog {
                                task_id: self.task.id,
                                is_dir: false,
                                name: &file_name(rel),
                                path: rel,
                                sync_folder: &self.task.name,
                                action_code: logs::ACTION_DOWNLOAD,
                                status: logs::STATUS_SUCCESS,
                                failed_code: 0,
                                failed_text: None,
                                payload: None,
                            },
                        )
                        .await;
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("download {rel} failed: {e}"));
                    }
                }
            }
        }

        // 远端已删除 → 删除本地（仅当本地未被改动）
        let mut deleted_any = false;
        {
            let mut prev = self.remote_state.lock();
            let mut removed: Vec<String> = prev
                .keys()
                .filter(|k| !now.contains_key(*k))
                .cloned()
                .collect();
            removed.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));
            for rel in removed.into_iter().filter(|_| self.mode == SyncMode::Bi) {
                let local = self.safe_local_path(&rel)?;
                let prior_size = if local.is_dir() {
                    None
                } else {
                    prev.get(&rel).map(|(size, _)| *size)
                };
                if !self.filter.should_sync(&rel, prior_size) {
                    continue;
                }
                if let Some((size, _)) = prev.get(&rel) {
                    let _ = size;
                    let same = Self::local_stamp(&local).ok().as_ref()
                        == self.local_state.lock().get(&rel);
                    if same {
                        if local.is_dir() {
                            let _ = std::fs::remove_dir(&local);
                        } else {
                            let _ = std::fs::remove_file(&local);
                        }
                        deleted_any = true;
                        self.local_state.lock().remove(&rel);
                    }
                }
            }
            *prev = now;
        }
        *self.remote_revisions.lock() = revisions;
        if deleted_any {
            self.note_work(started).await;
        }
        Ok(())
    }

    pub fn filter(&self) -> &FileFilter {
        &self.filter
    }

    pub async fn initial_sync_scan(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        Self::walk(&self.local_root, &self.local_root, &self.filter, &mut out)?;
        Ok(out)
    }

    fn walk(root: &Path, dir: &Path, filter: &FileFilter, out: &mut Vec<String>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_symlink() {
                continue;
            }
            let rel = match path.strip_prefix(root) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            if rel.is_empty() {
                continue;
            }
            let is_dir = path.is_dir();
            let size = if is_dir {
                None
            } else {
                std::fs::metadata(&path).ok().map(|m| m.len())
            };
            if !filter.should_sync(&rel, size) {
                continue;
            }
            out.push(rel.clone());
            if is_dir {
                Self::walk(root, &path, filter, out)?;
            }
        }
        Ok(())
    }

    pub fn pause(&self) {
        *self.paused.lock() = true;
    }

    pub fn resume(&self) {
        *self.paused.lock() = false;
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.lock()
    }
}

fn content_hash(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn file_name(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_mode_parse() {
        assert_eq!(SyncMode::from_db("bisynchronous"), SyncMode::Bi);
        assert_eq!(SyncMode::from_db("uploadonly"), SyncMode::UpOnly);
        assert_eq!(SyncMode::from_db("downloadonly"), SyncMode::DownOnly);
        assert!(SyncMode::UpOnly.allows_upload());
        assert!(!SyncMode::UpOnly.allows_download());
    }

    #[test]
    fn local_path_join() {
        let p = PathBuf::from("/tmp/sync");
        assert_eq!(p.join("a/b.txt"), PathBuf::from("/tmp/sync/a/b.txt"));
    }
}

#[cfg(test)]
mod live_mode_tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires isolated FN_SYNC_TEST_REMOTE_ROOT and saved session"]
    async fn three_modes_transfer_pause_resume_and_deletions() {
        let live_db = crate::service::database::open_db(Path::new(
            &std::env::var("FN_SYNC_TEST_DB").unwrap(),
        ))
        .await
        .unwrap();
        let address = std::env::var("FN_SYNC_TEST_ADDRESS").unwrap();
        let acc = crate::service::nas_account::get_all_nas_accounts(&live_db)
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.address == address)
            .unwrap();
        let manager = crate::task_manager::TaskManager::new();
        let nas = manager.account_client(&live_db, &acc).await.unwrap();
        let remote_root = std::env::var("FN_SYNC_TEST_REMOTE_ROOT").unwrap();
        assert!(remote_root.contains("fn-sync-qa-"));
        for mode in ["bisynchronous", "uploadonly", "downloadonly"] {
            let root = format!("{remote_root}/{mode}-{}", uuid::Uuid::new_v4());
            nas.mkdir(&root).await.unwrap();
            let temp = tempfile::tempdir().unwrap();
            let local = temp.path().join("local");
            std::fs::create_dir(&local).unwrap();
            let db = crate::service::database::open_db(&temp.path().join("test.sqlite"))
                .await
                .unwrap();
            crate::service::database::migrate(&db).await.unwrap();
            let task = Task {
                id: 1,
                nas_acc_id: acc.id,
                name: mode.into(),
                address: address.clone(),
                conn_type: "ip".into(),
                uuid: "1".into(),
                identifier: "qa".into(),
                mode: mode.into(),
                nas_path: root.clone(),
                pc_path: local.to_string_lossy().into(),
                status: 1,
                is_init_sync: 0,
                open_cloud: 0,
                created_at: 0,
                updated_at: 0,
            };
            let actor = TaskActor::new(
                task.clone(),
                nas.clone(),
                db.clone(),
                FileFilter::from_db(None).with_excluded_paths(vec!["excluded".into()]),
            )
            .unwrap();
            std::fs::write(local.join("local.txt"), b"local-original").unwrap();
            std::fs::create_dir_all(local.join("excluded")).unwrap();
            std::fs::write(local.join("excluded/local.txt"), b"excluded-local").unwrap();
            nas.mkdir(&format!("{root}/excluded")).await.unwrap();
            nas.upload(
                &format!("{root}/excluded/remote.txt"),
                b"excluded-remote".to_vec(),
                None,
            )
            .await
            .unwrap();
            std::fs::create_dir_all(local.join("local-sub")).unwrap();
            std::fs::write(local.join("local-sub/inner.txt"), b"nested-local").unwrap();
            nas.mkdir(&format!("{root}/remote-sub")).await.unwrap();
            nas.upload(
                &format!("{root}/remote-sub/inner.txt"),
                b"nested-remote".to_vec(),
                Some(1700000000),
            )
            .await
            .unwrap();
            if mode == "bisynchronous" {
                std::fs::write(local.join("remote.txt"), b"local-conflict").unwrap();
            }
            nas.upload(
                &format!("{root}/remote.txt"),
                b"remote-original".to_vec(),
                Some(1700000000),
            )
            .await
            .unwrap();
            eprintln!("START {mode}: initial reconciliation");
            actor.reconcile().await.expect("initial reconciliation");
            eprintln!("DONE {mode}: initial reconciliation");
            let up = mode != "downloadonly";
            let down = mode != "uploadonly";
            assert!(
                !local.join("excluded/remote.txt").exists(),
                "excluded directory downloaded"
            );
            assert!(
                !nas.ls(&format!("{root}/excluded"))
                    .await
                    .unwrap()
                    .iter()
                    .any(|e| e.name == "local.txt"),
                "excluded directory uploaded"
            );
            assert_eq!(
                local.join("remote.txt").exists(),
                down,
                "{mode}: initial download"
            );
            assert_eq!(
                local.join("remote-sub/inner.txt").exists(),
                down,
                "{mode}: nested download"
            );
            if up {
                assert_eq!(
                    nas.download(&format!("{root}/local-sub/inner.txt"))
                        .await
                        .unwrap(),
                    b"nested-local"
                );
            }
            if mode == "bisynchronous" {
                assert!(
                    std::fs::read_dir(&local)
                        .unwrap()
                        .flatten()
                        .any(|e| e.file_name().to_string_lossy().contains("Conflicted Copy")),
                    "local conflict must be preserved before download"
                );
            }
            assert_eq!(
                nas.ls(&root)
                    .await
                    .unwrap()
                    .iter()
                    .any(|e| e.name == "local.txt"),
                up,
                "{mode}: initial upload"
            );
            actor.pause();
            std::fs::write(local.join("paused.txt"), b"paused-edit").unwrap();
            actor.reconcile().await.unwrap();
            assert!(
                !nas.ls(&root)
                    .await
                    .unwrap()
                    .iter()
                    .any(|e| e.name == "paused.txt"),
                "{mode}: paused upload"
            );
            actor.resume();
            actor.reconcile().await.expect("resume reconciliation");
            assert_eq!(
                nas.ls(&root)
                    .await
                    .unwrap()
                    .iter()
                    .any(|e| e.name == "paused.txt"),
                up,
                "{mode}: resumed upload"
            );
            if down {
                nas.upload(
                    &format!("{root}/remote.txt"),
                    b"remote-modified".to_vec(),
                    Some(1700000000),
                )
                .await
                .unwrap();
                actor.reconcile().await.expect("remote change");
                assert_eq!(
                    std::fs::read(local.join("remote.txt")).unwrap(),
                    b"remote-modified",
                    "{mode}: same-size remote update"
                );
                nas.remove(vec![format!("{root}/remote.txt")])
                    .await
                    .unwrap();
                actor.reconcile().await.expect("remote deletion");
                assert_eq!(
                    local.join("remote.txt").exists(),
                    mode == "downloadonly",
                    "{mode}: source deletion follows UI retention contract"
                );
            }
            if up {
                let preserved_time = std::fs::metadata(local.join("local.txt"))
                    .unwrap()
                    .modified()
                    .unwrap();
                std::fs::write(local.join("local.txt"), b"local-modified").unwrap();
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(local.join("local.txt"))
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(preserved_time))
                    .unwrap();
                actor.reconcile().await.expect("local change");
                assert_eq!(
                    nas.download(&format!("{root}/local.txt")).await.unwrap(),
                    b"local-modified",
                    "{mode}: local update"
                );
                if mode == "bisynchronous" {
                    std::fs::remove_file(local.join("local.txt")).unwrap();
                    nas.upload(
                        &format!("{root}/local.txt"),
                        b"NAS edit wins over deletion".to_vec(),
                        Some(1700000050),
                    )
                    .await
                    .unwrap();
                    actor
                        .reconcile()
                        .await
                        .expect("delete versus remote edit conflict");
                    assert_eq!(
                        std::fs::read(local.join("local.txt")).unwrap(),
                        b"NAS edit wins over deletion"
                    );
                }
                std::fs::remove_file(local.join("local.txt")).unwrap();
                actor.reconcile().await.expect("local deletion");
                assert_eq!(
                    nas.ls(&root)
                        .await
                        .unwrap()
                        .iter()
                        .any(|e| e.name == "local.txt"),
                    mode == "uploadonly",
                    "{mode}: source deletion follows UI retention contract"
                );
            }
            if mode == "bisynchronous" {
                std::fs::write(local.join("offline.txt"), b"offline").unwrap();
                actor.reconcile().await.unwrap();
                std::fs::remove_file(local.join("offline.txt")).unwrap();
                let restored = TaskActor::new(
                    task.clone(),
                    nas.clone(),
                    db.clone(),
                    FileFilter::from_db(None).with_excluded_paths(vec!["excluded".into()]),
                )
                .unwrap();
                restored.restore_snapshot().await.unwrap();
                restored
                    .reconcile()
                    .await
                    .expect("offline deletion restore");
                assert!(
                    !nas.ls(&root)
                        .await
                        .unwrap()
                        .iter()
                        .any(|e| e.name == "offline.txt"),
                    "restart resurrected deleted file"
                );
                std::fs::create_dir_all(local.join("filter-change")).unwrap();
                std::fs::write(local.join("filter-change/keep.bak"), b"keep-me").unwrap();
                restored.reconcile().await.unwrap();
                let filtered = TaskActor::new(
                    task.clone(),
                    nas.clone(),
                    db.clone(),
                    FileFilter::from_db(None)
                        .with_excluded_paths(vec!["excluded".into(), "filter-change".into()]),
                )
                .unwrap();
                filtered.restore_snapshot().await.unwrap();
                filtered.reconcile().await.expect("filter change reload");
                assert!(
                    local.join("filter-change/keep.bak").exists(),
                    "filter change deleted existing local file"
                );
                assert_eq!(
                    nas.download(&format!("{root}/filter-change/keep.bak"))
                        .await
                        .unwrap(),
                    b"keep-me"
                );
                restored.reconcile().await.unwrap();
                let mut extension_rule = crate::service::tasks::FilterRuleInput::default_rule();
                extension_rule.ignore_extensions.push("bak".into());
                let extension_filter = TaskActor::new(
                    task.clone(),
                    nas.clone(),
                    db.clone(),
                    FileFilter::from_rule(&extension_rule)
                        .with_excluded_paths(vec!["excluded".into()]),
                )
                .unwrap();
                extension_filter.restore_snapshot().await.unwrap();
                extension_filter
                    .reconcile()
                    .await
                    .expect("extension filter change");
                assert!(
                    local.join("filter-change/keep.bak").exists(),
                    "extension filter deleted local file"
                );
                restored.reconcile().await.unwrap();
                let mut size_rule = crate::service::tasks::FilterRuleInput::default_rule();
                size_rule.enable_size_lower = 1;
                size_rule.size_lower_bound = Some(5000);
                let size_filter = TaskActor::new(
                    task.clone(),
                    nas.clone(),
                    db.clone(),
                    FileFilter::from_rule(&size_rule).with_excluded_paths(vec!["excluded".into()]),
                )
                .unwrap();
                size_filter.restore_snapshot().await.unwrap();
                size_filter.reconcile().await.expect("size filter change");
                assert!(
                    local.join("filter-change/keep.bak").exists(),
                    "size filter deleted local file"
                );
                let outside = temp.path().join("outside");
                std::fs::create_dir(&outside).unwrap();
                std::os::unix::fs::symlink(&outside, local.join("link")).unwrap();
                nas.mkdir(&format!("{root}/link")).await.unwrap();
                nas.upload(&format!("{root}/link/probe.txt"), b"probe".to_vec(), None)
                    .await
                    .unwrap();
                assert!(
                    restored.reconcile().await.is_err(),
                    "symlink destination accepted"
                );
                assert!(
                    !outside.join("probe.txt").exists(),
                    "download escaped sync root"
                );
                std::fs::remove_file(local.join("link")).unwrap();
                std::fs::remove_dir(local.join(".download")).unwrap();
                std::os::unix::fs::symlink(&outside, local.join(".download")).unwrap();
                assert!(
                    restored.reconcile().await.is_err(),
                    "symlink staging directory accepted"
                );
                assert_eq!(
                    std::fs::read_dir(&outside).unwrap().count(),
                    0,
                    "staging escaped sync root"
                );
                std::fs::remove_file(local.join(".download")).unwrap();
            }
            nas.remove(vec![root]).await.unwrap();
            eprintln!("PASS {mode}: initial files, pause/resume, changes and deletions");
        }
    }
}
