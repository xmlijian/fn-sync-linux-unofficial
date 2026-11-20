//! 任务管理器 — NAS 客户端连接池 + 任务 actor 生命周期。

use crate::service::nas_account::NasAccount;
use crate::traffic::TrafficCounter;
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

struct RunningTask {
    actor: Arc<crate::tasks::actor::TaskActor>,
    abort: tokio::task::AbortHandle,
    join: tokio::task::JoinHandle<()>,
    account_id: i64,
}

pub struct TaskManager {
    clients: DashMap<u64, Arc<crate::connect::nas_client::NasClient>>,
    account_clients: DashMap<i64, Arc<crate::connect::nas_client::NasClient>>,
    running: DashMap<i64, RunningTask>,
    traffic: TrafficCounter,
}

fn conn_config(
    address: &str,
    user: &str,
    password: &str,
) -> crate::connect::nas_client::NasConnConfig {
    crate::connect::nas_client::NasConnConfig {
        address: address.to_string(),
        user: user.to_string(),
        password: password.to_string(),
        device_name: "PcSync".into(),
        verify_tls: false,
    }
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            clients: DashMap::new(),
            account_clients: DashMap::new(),
            running: DashMap::new(),
            traffic: TrafficCounter::new(),
        }
    }

    pub fn traffic(&self) -> &TrafficCounter {
        &self.traffic
    }

    /// 建立到 NAS 的 WebSocket 客户端（未登录态，仅探测）。
    pub async fn create_client(&self, address: &str) -> Result<u64> {
        let client =
            crate::connect::nas_client::NasClient::open(conn_config(address, "", "")).await?;
        let conn_id = client.conn_id;
        self.clients.insert(conn_id, client);
        Ok(conn_id)
    }

    /// 用凭据建立并登录一个客户端。
    pub async fn create_authed_client(
        &self,
        address: &str,
        user: &str,
        password: &str,
    ) -> Result<Arc<crate::connect::nas_client::NasClient>> {
        let client =
            crate::connect::nas_client::NasClient::connect(conn_config(address, user, password))
                .await?;
        self.clients.insert(client.conn_id, client.clone());
        Ok(client)
    }

    pub async fn account_client(
        &self,
        acc: &NasAccount,
    ) -> Result<Arc<crate::connect::nas_client::NasClient>> {
        if let Some(client) = self.account_clients.get(&acc.id) {
            if client.is_connected() {
                return Ok(client.clone());
            }
        }
        self.account_clients.remove(&acc.id);
        let client =
            crate::connect::nas_client::NasClient::open(conn_config(&acc.address, "", "")).await?;
        client.restore_session(acc).await?;
        self.account_clients.insert(acc.id, client.clone());
        Ok(client)
    }

    pub fn delete_client(&self, conn_id: u64) {
        if let Some((_, client)) = self.clients.remove(&conn_id) {
            client.disconnect();
        }
        if let Some((_, client)) = self.account_clients.remove(&(conn_id as i64)) {
            client.disconnect();
        }
    }

    pub fn client(&self, conn_id: u64) -> Option<Arc<crate::connect::nas_client::NasClient>> {
        self.clients.get(&conn_id).map(|c| c.clone())
    }

    /// RPC 代理：通过已注册的连接发送 file.ls 等。
    pub async fn send_rpc(
        &self,
        conn_id: u64,
        message: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let client = self
            .clients
            .get(&conn_id)
            .map(|c| c.clone())
            .ok_or_else(|| anyhow!("client unavailable"))?;
        client.rpc(message).await
    }

    /// 探测 NAS 信息（名称/版本）。
    pub async fn probe_nas(&self, address: &str) -> Result<(String, String)> {
        let (host, ver) = crate::connect::nas_client::NasClient::probe(address).await?;
        let name = if host.is_empty() {
            format!("NAS ({ver})")
        } else {
            host
        };
        Ok((name, ver))
    }

    /// 启动同步任务。
    pub async fn start_task(
        &self,
        db: &sqlx::SqlitePool,
        task_id: i64,
        acc: &NasAccount,
    ) -> Result<()> {
        if self
            .running
            .get(&task_id)
            .map(|r| !r.abort.is_finished())
            .unwrap_or(false)
        {
            return Ok(());
        }
        self.running.remove(&task_id);
        let task = crate::service::tasks::get_task_by_id(db, task_id)
            .await?
            .ok_or_else(|| anyhow!("task not found"))?;
        let rule = crate::service::tasks::get_filter_rule_by_task_id(db, task_id).await?;
        let excluded = crate::service::tasks::get_folder_filter_paths(db, task_id).await?;
        let filter = crate::tasks::file_filter::FileFilter::from_db(rule.as_ref())
            .with_excluded_paths(excluded);

        let nas = self.account_client(acc).await?;

        let (local_tx, local_rx) = mpsc::channel(4096);

        let actor = Arc::new(crate::tasks::actor::TaskActor::new(
            task.clone(),
            nas,
            db.clone(),
            filter.clone(),
        )?);
        let watcher = crate::tasks::watch::LocalWatcher::start(
            std::path::PathBuf::from(&task.pc_path),
            filter.clone(),
            local_tx,
        )?;
        actor.set_saved_account(acc.clone());
        let paused = task.status == crate::tasks::actor::status::PAUSED;
        if paused {
            actor.pause();
        }
        crate::service::tasks::update_task_status(
            db,
            task_id,
            if paused {
                crate::tasks::actor::status::PAUSED
            } else {
                crate::tasks::actor::status::SYNCING
            },
        )
        .await?;

        let db_clone = db.clone();
        let actor_clone = actor.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = actor_clone.run(local_rx, watcher).await {
                tracing::error!(task_id, error = %e, "sync task stopped with error");
                let _ = crate::service::tasks::update_task_status(
                    &db_clone,
                    task_id,
                    crate::tasks::actor::status::FAILED,
                )
                .await;
            }
            let _ = db_clone;
        });
        self.running.insert(
            task_id,
            RunningTask {
                actor,
                abort: handle.abort_handle(),
                join: handle,
                account_id: acc.id,
            },
        );
        Ok(())
    }

    pub async fn stop_task(&self, task_id: i64) {
        if let Some((_, r)) = self.running.remove(&task_id) {
            r.abort.abort();
            let _ = r.join.await;
        }
    }

    pub fn pause_task(&self, task_id: i64) {
        if let Some(r) = self.running.get(&task_id) {
            r.actor.pause();
        }
    }

    pub fn resume_task(&self, task_id: i64) {
        if let Some(r) = self.running.get(&task_id) {
            r.actor.resume();
        }
    }

    pub fn is_running(&self, task_id: i64) -> bool {
        self.running
            .get(&task_id)
            .map(|r| !r.abort.is_finished())
            .unwrap_or(false)
    }

    pub fn task_count(&self) -> usize {
        self.running
            .iter()
            .filter(|r| !r.abort.is_finished())
            .count()
    }

    pub fn running_count(&self) -> usize {
        self.running
            .iter()
            .filter(|r| !r.abort.is_finished() && !r.actor.is_paused())
            .count()
    }

    pub async fn disconnect_account(&self, acc_id: i64) {
        if let Some((_, client)) = self.account_clients.remove(&acc_id) {
            client.disconnect();
        }
        let ids: Vec<i64> = self
            .running
            .iter()
            .filter(|e| e.account_id == acc_id)
            .map(|e| *e.key())
            .collect();
        for id in ids {
            self.stop_task(id).await;
        }
    }
}

impl Drop for TaskManager {
    fn drop(&mut self) {
        for task in self.running.iter() {
            task.abort.abort();
        }
    }
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod session_tests {
    #[tokio::test]
    #[ignore = "requires FN_SYNC_TEST_DB and FN_SYNC_TEST_ADDRESS for a live NAS"]
    async fn saved_account_restores_and_lists_directories() {
        let path = std::env::var("FN_SYNC_TEST_DB").expect("test database path");
        let address = std::env::var("FN_SYNC_TEST_ADDRESS").expect("NAS address");
        let db = crate::service::database::open_db(std::path::Path::new(&path))
            .await
            .unwrap();
        let account = crate::service::nas_account::get_all_nas_accounts(&db)
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.address == address)
            .expect("saved account");
        let manager = super::TaskManager::new();
        let client = manager.account_client(&account).await.unwrap();
        let reply = client
            .rpc(serde_json::json!({"req":"file.lsDir"}))
            .await
            .unwrap();
        assert!(
            reply.get("files").and_then(|v| v.as_array()).is_some(),
            "directory response missing files"
        );
        let cached = manager.account_client(&account).await.unwrap();
        assert!(std::sync::Arc::ptr_eq(&client, &cached));
        manager.delete_client(account.id as u64);
        let restored = manager.account_client(&account).await.unwrap();
        assert!(!std::sync::Arc::ptr_eq(&client, &restored));
        let reply = restored
            .rpc(serde_json::json!({"req":"appcgi.sysinfo.getMachineId"}))
            .await
            .unwrap();
        assert_eq!(reply.get("errno").and_then(|v| v.as_i64()).unwrap_or(0), 0);
    }
}

#[cfg(test)]
mod live_lifecycle_tests {
    use std::path::Path;
    async fn wait_contents(path: &Path, bytes: &[u8]) {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if std::fs::read(path).ok().as_deref() == Some(bytes) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("task failed to transfer expected file");
    }
    #[tokio::test]
    #[ignore = "requires isolated live NAS test root and saved credentials"]
    async fn task_manager_start_pause_resume_stop_and_recreate() {
        let source = crate::service::database::open_db(Path::new(
            &std::env::var("FN_SYNC_TEST_DB").unwrap(),
        ))
        .await
        .unwrap();
        let address = std::env::var("FN_SYNC_TEST_ADDRESS").unwrap();
        let original = crate::service::nas_account::get_all_nas_accounts(&source)
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.address == address)
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = crate::service::database::open_db(&temp.path().join("test.sqlite"))
            .await
            .unwrap();
        crate::service::database::migrate(&db).await.unwrap();
        let create = serde_json::from_value(serde_json::to_value(&original).unwrap()).unwrap();
        let account_id = crate::service::nas_account::add_nas_account(&db, &create)
            .await
            .unwrap();
        let acc = crate::service::nas_account::get_nas_account_by_id(&db, account_id)
            .await
            .unwrap()
            .unwrap();
        let manager = super::TaskManager::new();
        let nas = manager.account_client(&acc).await.unwrap();
        let parent = std::env::var("FN_SYNC_TEST_REMOTE_ROOT").unwrap();
        assert!(parent.contains("fn-sync-qa-"));
        let root = format!("{parent}/lifecycle-{}", uuid::Uuid::new_v4());
        nas.mkdir(&root).await.unwrap();
        nas.upload(
            &format!("{root}/state.txt"),
            b"first".to_vec(),
            Some(1700000000),
        )
        .await
        .unwrap();
        let local = temp.path().join("new-local-root");
        let task:crate::service::tasks::TaskCreate=serde_json::from_value(serde_json::json!({"name":"lifecycle","address":address,"conn_type":"ip","uuid":"1","identifier":"qa","mode":"downloadonly","nas_path":root,"pc_path":local,"nas_acc_id":account_id,"open_cloud":false})).unwrap();
        let id = crate::service::tasks::add_task(&db, &task).await.unwrap();
        manager.start_task(&db, id, &acc).await.unwrap();
        wait_contents(&local.join("state.txt"), b"first").await;
        assert!(manager.is_running(id));
        manager.pause_task(id);
        assert_eq!(manager.running_count(), 0);
        nas.upload(
            &format!("{root}/state.txt"),
            b"second-version".to_vec(),
            Some(1700000020),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert_eq!(std::fs::read(local.join("state.txt")).unwrap(), b"first");
        manager.resume_task(id);
        wait_contents(&local.join("state.txt"), b"second-version").await;
        nas.disconnect();
        let replacement = manager.account_client(&acc).await.unwrap();
        assert!(!std::sync::Arc::ptr_eq(&nas, &replacement));
        replacement
            .upload(
                &format!("{root}/state.txt"),
                b"network-recovered".to_vec(),
                Some(1700000030),
            )
            .await
            .unwrap();
        wait_contents(&local.join("state.txt"), b"network-recovered").await;
        manager.stop_task(id).await;
        assert!(!manager.is_running(id));
        replacement
            .upload(
                &format!("{root}/state.txt"),
                b"third-version".to_vec(),
                Some(1700000040),
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert_eq!(
            std::fs::read(local.join("state.txt")).unwrap(),
            b"network-recovered"
        );
        let restarted = super::TaskManager::new();
        restarted.start_task(&db, id, &acc).await.unwrap();
        wait_contents(&local.join("state.txt"), b"third-version").await;
        restarted.disconnect_account(account_id).await;
        assert!(!restarted.is_running(id));
        crate::service::tasks::delete_task(&db, id).await.unwrap();
        replacement.remove(vec![root]).await.unwrap();
    }
}
