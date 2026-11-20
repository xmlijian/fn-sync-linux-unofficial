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
    account_id: i64,
}

pub struct TaskManager {
    clients: DashMap<u64, Arc<crate::connect::nas_client::NasClient>>,
    account_clients: DashMap<i64, Arc<crate::connect::nas_client::NasClient>>,
    running: DashMap<i64, RunningTask>,
    traffic: TrafficCounter,
}

fn conn_config(address: &str, user: &str, password: &str) -> crate::connect::nas_client::NasConnConfig {
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
        let client = crate::connect::nas_client::NasClient::open(conn_config(address, "", "")).await?;
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
            crate::connect::nas_client::NasClient::connect(conn_config(address, user, password)).await?;
        self.clients.insert(client.conn_id, client.clone());
        Ok(client)
    }

    pub async fn account_client(&self, acc: &NasAccount) -> Result<Arc<crate::connect::nas_client::NasClient>> {
        if let Some(client) = self.account_clients.get(&acc.id) { return Ok(client.clone()); }
        let client = crate::connect::nas_client::NasClient::open(conn_config(&acc.address, "", "")).await?;
        client.restore_session(acc).await?;
        self.account_clients.insert(acc.id, client.clone());
        Ok(client)
    }

    pub fn delete_client(&self, conn_id: u64) {
        self.clients.remove(&conn_id);
        self.account_clients.remove(&(conn_id as i64));
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
        if self.running.get(&task_id).map(|r| !r.abort.is_finished()).unwrap_or(false) {
            return Ok(());
        }
        self.running.remove(&task_id);
        let task = crate::service::tasks::get_task_by_id(db, task_id)
            .await?
            .ok_or_else(|| anyhow!("task not found"))?;
        let rule = crate::service::tasks::get_filter_rule_by_task_id(db, task_id).await?;
        let filter = crate::tasks::file_filter::FileFilter::from_db(rule.as_ref());

        let nas = self
            .account_client(acc)
            .await?;

        let (local_tx, local_rx) = mpsc::channel(4096);
        let watcher = crate::tasks::watch::LocalWatcher::start(
            std::path::PathBuf::from(&task.pc_path),
            filter.clone(),
            local_tx,
        )?;

        let actor = Arc::new(crate::tasks::actor::TaskActor::new(
            task.clone(),
            nas,
            db.clone(),
            filter,
        )?);
        crate::service::tasks::update_task_status(db, task_id, crate::tasks::actor::status::SYNCING)
            .await?;

        let db_clone = db.clone();
        let actor_clone = actor.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = actor_clone.run(local_rx, watcher).await {
                tracing::error!(task_id, error = %e, "sync task stopped with error");
                let _ = crate::service::tasks::update_task_status(&db_clone, task_id, crate::tasks::actor::status::FAILED).await;
            }
            let _ = db_clone;
        });
        self.running.insert(task_id, RunningTask { actor, abort: handle.abort_handle(), account_id: acc.id });
        Ok(())
    }

    pub fn stop_task(&self, task_id: i64) {
        if let Some((_, r)) = self.running.remove(&task_id) {
            r.abort.abort();
        }
    }

    pub fn pause_task(&self, task_id: i64) {
        if let Some(r) = self.running.get(&task_id) {
            r.actor.pause();
        }
    }

    pub fn resume_task(&self, task_id: i64) {
        if let Some(r) = self.running.get(&task_id) { r.actor.resume(); }
    }

    pub fn is_running(&self, task_id: i64) -> bool {
        self.running.get(&task_id).map(|r| !r.abort.is_finished()).unwrap_or(false)
    }

    pub fn task_count(&self) -> usize {
        self.running.iter().filter(|r| !r.abort.is_finished()).count()
    }

    pub fn running_count(&self) -> usize {
        self.running.iter().filter(|r| !r.abort.is_finished()).count()
    }

    pub fn disconnect_account(&self, acc_id: i64) {
        self.account_clients.remove(&acc_id);
        let ids: Vec<i64> = self
            .running
            .iter()
            .filter(|e| e.account_id == acc_id)
            .map(|e| *e.key())
            .collect();
        for id in ids {
            self.stop_task(id);
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
        let db = crate::service::database::open_db(std::path::Path::new(&path)).await.unwrap();
        let account = crate::service::nas_account::get_all_nas_accounts(&db).await.unwrap()
            .into_iter().find(|a| a.address == address).expect("saved account");
        let manager = super::TaskManager::new();
        let client = manager.account_client(&account).await.unwrap();
        let reply = client.rpc(serde_json::json!({"req":"file.lsDir"})).await.unwrap();
        assert!(reply.get("files").and_then(|v| v.as_array()).is_some(), "directory response missing files");
        let cached = manager.account_client(&account).await.unwrap();
        assert!(std::sync::Arc::ptr_eq(&client, &cached));
        manager.delete_client(account.id as u64);
        let restored = manager.account_client(&account).await.unwrap();
        assert!(!std::sync::Arc::ptr_eq(&client, &restored));
        let reply = restored.rpc(serde_json::json!({"req":"appcgi.sysinfo.getMachineId"})).await.unwrap();
        assert_eq!(reply.get("errno").and_then(|v|v.as_i64()).unwrap_or(0), 0);
    }
}
