//! 任务管理器 — NAS 客户端连接池 + 任务 actor / 按需同步挂载生命周期。

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

/// 运行中的任务：普通同步 actor，或按需同步 FUSE 挂载（open_cloud=1）。
enum RunningKind {
    Sync(RunningTask),
    OnDemand {
        mount: crate::tasks::ondemand::OnDemandMount,
        account_id: i64,
    },
}

impl RunningKind {
    fn account_id(&self) -> i64 {
        match self {
            Self::Sync(t) => t.account_id,
            Self::OnDemand { account_id, .. } => *account_id,
        }
    }

    fn is_active(&self) -> bool {
        match self {
            Self::Sync(t) => !t.abort.is_finished(),
            Self::OnDemand { mount, .. } => mount.is_active(),
        }
    }
}

pub struct TaskManager {
    clients: DashMap<u64, Arc<crate::connect::nas_client::NasClient>>,
    /// 按账号共享的会话缓存。**同一进程内每个账号只允许一个会话**：
    /// fnOS 的 tokenLogin 会作废旧 token，若按需挂载与普通同步各建一个，
    /// 两边会互相把对方顶下线，形成无限互踢。
    account_clients: Arc<DashMap<i64, Arc<crate::connect::nas_client::NasClient>>>,
    /// 按账号的建连闸门：并发请求只让一个真正建连。
    account_locks: Arc<DashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    running: DashMap<i64, RunningKind>,
    failed_stops: DashMap<i64, std::path::PathBuf>,
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

/// 用账号信息建立并认证一个 NAS 连接（不需要密码，走保存的会话）。
/// fnid 账号每次建连都重新解析中继地址。
///
/// 整体限时：`connect_async` 本身没有超时，网络/代理卡住时会让调用方
/// 无限期挂起（按需挂载因此整棵树卡死）。
pub async fn open_account_client(
    db: &sqlx::SqlitePool,
    acc: &NasAccount,
) -> Result<Arc<crate::connect::nas_client::NasClient>> {
    let attempt = async {
        let address = crate::connect::address::resolve_target(&acc.address, &acc.conn_type)
            .await?
            .address;
        let client =
            crate::connect::nas_client::NasClient::open(conn_config(&address, "", "")).await?;
        client.restore_session(acc).await?;
        client.persist_access_token(db, acc).await?;
        Ok::<_, anyhow::Error>(client)
    };
    match tokio::time::timeout(std::time::Duration::from_secs(25), attempt).await {
        Ok(r) => r,
        Err(_) => Err(anyhow!("NAS connect/auth timed out")),
    }
}

/// 按账号取一个**共享**的已认证会话：活着就复用，死了才重建（单飞）。
/// 保证同一进程内同一账号只有一个会话，避免 tokenLogin 互踢。
async fn shared_account_client(
    clients: &Arc<DashMap<i64, Arc<crate::connect::nas_client::NasClient>>>,
    locks: &Arc<DashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    db: &sqlx::SqlitePool,
    acc: &NasAccount,
) -> Result<Arc<crate::connect::nas_client::NasClient>> {
    if let Some(client) = clients.get(&acc.id) {
        if client.is_connected() {
            return Ok(client.clone());
        }
    }
    let lock = locks
        .entry(acc.id)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _guard = match tokio::time::timeout(std::time::Duration::from_secs(30), lock.lock()).await {
        Ok(g) => g,
        Err(_) => return Err(anyhow!("timed out waiting for NAS connect gate")),
    };
    // 双检：排队期间可能已被别人建好。
    if let Some(client) = clients.get(&acc.id) {
        if client.is_connected() {
            return Ok(client.clone());
        }
    }
    clients.remove(&acc.id);
    let client = open_account_client(db, acc).await?;
    clients.insert(acc.id, client.clone());
    Ok(client)
}

/// 按需挂载的重连工厂：复用按账号共享的会话（与普通同步同一个），
/// 这样不会因为重复 tokenLogin 把对方顶下线。
fn ondemand_connector(
    clients: Arc<DashMap<i64, Arc<crate::connect::nas_client::NasClient>>>,
    locks: Arc<DashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    db: sqlx::SqlitePool,
    acc: NasAccount,
) -> crate::tasks::ondemand::remote::SessionConnector {
    use crate::tasks::ondemand::remote::RemoteSession;
    Arc::new(move || {
        let clients = clients.clone();
        let locks = locks.clone();
        let db = db.clone();
        let acc = acc.clone();
        Box::pin(async move {
            let latest = crate::service::nas_account::get_nas_account_by_id(&db, acc.id)
                .await?
                .unwrap_or(acc);
            let client = shared_account_client(&clients, &locks, &db, &latest).await?;
            let session: Arc<dyn RemoteSession> = client;
            Ok(session)
        })
    })
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            clients: DashMap::new(),
            account_clients: Arc::new(DashMap::new()),
            account_locks: Arc::new(DashMap::new()),
            running: DashMap::new(),
            failed_stops: DashMap::new(),
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
        db: &sqlx::SqlitePool,
        acc: &NasAccount,
    ) -> Result<Arc<crate::connect::nas_client::NasClient>> {
        shared_account_client(&self.account_clients, &self.account_locks, db, acc).await
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

    /// 启动同步任务（open_cloud=1 时启动按需同步 FUSE 挂载）。
    pub async fn start_task(
        &self,
        db: &sqlx::SqlitePool,
        task_id: i64,
        acc: &NasAccount,
    ) -> Result<()> {
        if self
            .running
            .get(&task_id)
            .map(|r| r.is_active())
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

        let nas = self.account_client(db, acc).await?;

        // 兜底：按需同步只对“双向同步”有意义。若数据库里残留非法组合
        // （如外部直接改库），归一化为普通同步，避免“仅上传”的任务反而开始下载。
        if task.open_cloud == 1 && task.mode != crate::service::tasks::ON_DEMAND_MODE {
            tracing::warn!(
                task_id,
                mode = %task.mode,
                "on-demand sync requires bisynchronous mode; falling back to regular sync"
            );
            let _ = crate::service::tasks::update_task(
                db,
                task_id,
                &crate::service::tasks::TaskUpdate {
                    filter_rule: None,
                    name: None,
                    mode: None,
                    open_cloud: Some(0),
                },
            )
            .await;
        } else if task.open_cloud == 1 {
            return self.start_ondemand(db, task, acc, nas, filter).await;
        }

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
            RunningKind::Sync(RunningTask {
                actor,
                abort: handle.abort_handle(),
                join: handle,
                account_id: acc.id,
            }),
        );
        Ok(())
    }

    /// 启动按需同步：pc_path 即挂载点，目录实时映射 NAS 内容。
    async fn start_ondemand(
        &self,
        db: &sqlx::SqlitePool,
        task: crate::service::tasks::Task,
        acc: &NasAccount,
        nas: Arc<crate::connect::nas_client::NasClient>,
        filter: crate::tasks::file_filter::FileFilter,
    ) -> Result<()> {
        let cache_root = crate::platform::app_data_dir()?
            .join("ondemand")
            .join(task.id.to_string());
        let cfg = crate::tasks::ondemand::OnDemandConfig {
            task_id: task.id,
            task_name: task.name.clone(),
            mountpoint: std::path::PathBuf::from(&task.pc_path),
            nas_root: task.nas_path.trim_matches('/').to_string(),
            cache_root,
            cache_limit_bytes: crate::tasks::ondemand::cache_limit_from_settings(db).await,
        };
        let task_id = task.id;
        let db_owned = db.clone();
        // 按需挂载必须自己负责重连：会话被服务端作废（errno 4224 等）时
        // NasClient 会自断，而挂载没有 TaskActor 替它重连。用 ReconnectingRemote
        // 包一层，重连走与普通同步相同的 restore_session（longToken，无需密码）。
        let connector = ondemand_connector(
            self.account_clients.clone(),
            self.account_locks.clone(),
            db.clone(),
            acc.clone(),
        );
        let nas: Arc<dyn crate::tasks::ondemand::remote::RemoteSession> = nas;
        let remote = crate::tasks::ondemand::remote::ReconnectingRemote::new(Some(nas), connector);
        let remote: Arc<dyn crate::tasks::ondemand::remote::RemoteOps> = remote;
        let mount = tokio::task::spawn_blocking(move || {
            crate::tasks::ondemand::OnDemandMount::start(cfg, remote, db_owned, filter)
        })
        .await
        .map_err(|e| anyhow!("on-demand mount task failed: {e}"))?;
        let mount = match mount {
            Ok(m) => m,
            Err(e) => {
                let _ = crate::service::tasks::update_task_status(
                    db,
                    task_id,
                    crate::tasks::actor::status::FAILED,
                )
                .await;
                return Err(e);
            }
        };
        crate::service::tasks::update_task_status(
            db,
            task_id,
            crate::tasks::actor::status::COMPLETED,
        )
        .await?;
        self.running.insert(
            task_id,
            RunningKind::OnDemand {
                mount,
                account_id: acc.id,
            },
        );
        tracing::info!(task_id, "on-demand task started");
        Ok(())
    }

    pub async fn stop_task(&self, task_id: i64) -> Result<()> {
        if let Some(path) = self.failed_stops.get(&task_id) {
            if crate::tasks::ondemand::is_fuse_mount(path.value()) {
                return Err(anyhow!("上次停止未完成，挂载仍在使用中"));
            }
        }
        self.failed_stops.remove(&task_id);
        if let Some((_, kind)) = self.running.remove(&task_id) {
            match kind {
                RunningKind::Sync(r) => {
                    r.abort.abort();
                    let _ = r.join.await;
                }
                RunningKind::OnDemand { mount, .. } => {
                    // 卸载可能阻塞到派发线程退出；放到阻塞线程池
                    let path = mount.mountpoint().to_path_buf();
                    let stopped = tokio::task::spawn_blocking(move || mount.stop()).await;
                    let result = match stopped {
                        Ok(result) => result,
                        Err(error) => Err(anyhow!("mount shutdown failed: {error}")),
                    };
                    if let Err(error) = result {
                        self.failed_stops.insert(task_id, path);
                        return Err(error);
                    }
                }
            }
        }
        Ok(())
    }

    /// 停止全部任务（进程退出时卸载所有按需挂载点）。
    pub async fn stop_all(&self) {
        let ids: Vec<i64> = self.running.iter().map(|e| *e.key()).collect();
        for id in ids {
            if let Err(error) = self.stop_task(id).await {
                tracing::error!(task_id=id, %error, "task shutdown failed");
            }
        }
    }

    pub async fn pause_task(&self, db: &sqlx::SqlitePool, task_id: i64) -> Result<()> {
        if crate::service::tasks::get_task_by_id(db, task_id)
            .await?
            .is_none()
        {
            return Err(anyhow!("task not found"));
        }
        self.stop_task(task_id).await?;
        crate::service::tasks::update_task_status(db, task_id, crate::tasks::actor::status::PAUSED)
            .await?;
        tracing::info!(task_id, "task paused; executor stopped");
        Ok(())
    }

    pub async fn resume_task(&self, db: &sqlx::SqlitePool, task_id: i64) -> Result<()> {
        let task = crate::service::tasks::get_task_by_id(db, task_id)
            .await?
            .ok_or_else(|| anyhow!("task not found"))?;
        let account = crate::service::nas_account::get_nas_account_by_id(db, task.nas_acc_id)
            .await?
            .ok_or_else(|| anyhow!("account not found"))?;
        self.stop_task(task_id).await?;
        crate::service::tasks::update_task_status(
            db,
            task_id,
            crate::tasks::actor::status::SYNCING,
        )
        .await?;
        if let Err(error) = self.start_task(db, task_id, &account).await {
            let _ = crate::service::tasks::update_task_status(
                db,
                task_id,
                crate::tasks::actor::status::FAILED,
            )
            .await;
            return Err(error);
        }
        tracing::info!(task_id, "task resumed; executor recreated");
        Ok(())
    }

    pub fn is_running(&self, task_id: i64) -> bool {
        self.running
            .get(&task_id)
            .map(|r| r.is_active())
            .unwrap_or(false)
    }

    pub fn task_count(&self) -> usize {
        self.running.iter().filter(|r| r.is_active()).count()
    }

    pub fn running_count(&self) -> usize {
        self.running
            .iter()
            .filter(|r| match r.value() {
                RunningKind::Sync(t) => !t.abort.is_finished() && !t.actor.is_paused(),
                RunningKind::OnDemand { mount, .. } => mount.is_active(),
            })
            .count()
    }

    pub async fn disconnect_account(&self, acc_id: i64) {
        if let Some((_, client)) = self.account_clients.remove(&acc_id) {
            client.disconnect();
        }
        let ids: Vec<i64> = self
            .running
            .iter()
            .filter(|e| e.value().account_id() == acc_id)
            .map(|e| *e.key())
            .collect();
        for id in ids {
            if let Err(error) = self.stop_task(id).await {
                tracing::error!(task_id=id, %error, "task shutdown failed");
            }
        }
    }
}

impl Drop for TaskManager {
    fn drop(&mut self) {
        for task in self.running.iter() {
            if let RunningKind::Sync(t) = task.value() {
                t.abort.abort();
            }
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
        let client = manager.account_client(&db, &account).await.unwrap();
        let reply = client
            .rpc(serde_json::json!({"req":"file.lsDir"}))
            .await
            .unwrap();
        assert!(
            reply.get("files").and_then(|v| v.as_array()).is_some(),
            "directory response missing files"
        );
        let cached = manager.account_client(&db, &account).await.unwrap();
        assert!(std::sync::Arc::ptr_eq(&client, &cached));
        manager.delete_client(account.id as u64);
        let restored = manager.account_client(&db, &account).await.unwrap();
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
        let nas = manager.account_client(&db, &acc).await.unwrap();
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
        manager.pause_task(&db, id).await.unwrap();
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
        manager.resume_task(&db, id).await.unwrap();
        wait_contents(&local.join("state.txt"), b"second-version").await;
        nas.disconnect();
        let replacement = manager.account_client(&db, &acc).await.unwrap();
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
        manager.stop_task(id).await.unwrap();
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

#[cfg(test)]
mod live_refresh_tests {
    #[tokio::test]
    #[ignore = "requires saved NAS long token; persists the verified renewed access token"]
    async fn expired_access_token_refreshes_and_persists() {
        let db = crate::service::database::open_db(std::path::Path::new(
            &std::env::var("FN_SYNC_TEST_DB").unwrap(),
        ))
        .await
        .unwrap();
        let address = std::env::var("FN_SYNC_TEST_ADDRESS").unwrap();
        let mut account = crate::service::nas_account::get_all_nas_accounts(&db)
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.address == address)
            .unwrap();
        account.token = "qa-expired-access-token".into();
        let manager = super::TaskManager::new();
        let client = manager
            .account_client(&db, &account)
            .await
            .expect("refresh expired session");
        assert_ne!(client.token.read().as_str(), "qa-expired-access-token");
        let saved = crate::service::nas_account::get_nas_account_by_id(&db, account.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.token, client.token.read().as_str());
        assert_eq!(*client.uid.read(), account.uid);
        let reply = client
            .rpc(serde_json::json!({"req":"file.lsDir"}))
            .await
            .unwrap();
        assert!(reply["files"].is_array());
    }
}

#[cfg(test)]
mod live_pause_inflight_tests {
    #[tokio::test]
    #[ignore = "requires isolated NAS root and saved OAuth session"]
    async fn pause_cancels_active_upload_and_download_and_resume_completes() {
        use std::time::{Duration, Instant};
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::traffic::set_limit("global_upload_speed_limit", "-1");
                crate::traffic::set_limit("global_download_speed_limit", "-1");
            }
        }
        let _reset = Reset;
        let source = crate::service::database::open_db(std::path::Path::new(
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
        let root = std::env::var("FN_SYNC_TEST_REMOTE_ROOT").unwrap();
        assert!(root.contains("fn-sync-qa-"));
        for mode in ["uploadonly", "downloadonly"] {
            let temp = tempfile::tempdir().unwrap();
            let db = crate::service::database::open_db(&temp.path().join("test.sqlite"))
                .await
                .unwrap();
            crate::service::database::migrate(&db).await.unwrap();
            let create = serde_json::from_value(serde_json::to_value(&original).unwrap()).unwrap();
            let account_id = crate::service::nas_account::add_nas_account(&db, &create)
                .await
                .unwrap();
            let account = crate::service::nas_account::get_nas_account_by_id(&db, account_id)
                .await
                .unwrap()
                .unwrap();
            let manager = super::TaskManager::new();
            let nas = manager.account_client(&db, &account).await.unwrap();
            let remote = format!("{root}/inflight-{mode}-{}", uuid::Uuid::new_v4());
            nas.mkdir(&remote).await.unwrap();
            let local = temp.path().join("local");
            std::fs::create_dir(&local).unwrap();
            let bytes = vec![b'P'; 128 * 1024];
            if mode == "uploadonly" {
                std::fs::write(local.join("big.bin"), &bytes).unwrap();
            } else {
                nas.upload(&format!("{remote}/big.bin"), bytes.clone(), None)
                    .await
                    .unwrap();
            }
            let key = if mode == "uploadonly" {
                "global_upload_speed_limit"
            } else {
                "global_download_speed_limit"
            };
            crate::traffic::set_limit(key, "64");
            let task=serde_json::from_value(serde_json::json!({"name":"pause in flight","address":address,"conn_type":"ip","uuid":"1","identifier":"qa","mode":mode,"nas_path":remote,"pc_path":local,"nas_acc_id":account_id,"open_cloud":false})).unwrap();
            let id = crate::service::tasks::add_task(&db, &task).await.unwrap();
            manager.start_task(&db, id, &account).await.unwrap();
            tokio::time::sleep(Duration::from_millis(1300)).await;
            let started = Instant::now();
            manager.pause_task(&db, id).await.unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "pause waited for entire transfer"
            );
            assert!(!manager.is_running(id));
            assert_eq!(
                crate::service::tasks::get_task_by_id(&db, id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                crate::tasks::actor::status::PAUSED
            );
            tokio::time::sleep(Duration::from_millis(1500)).await;
            if mode == "downloadonly" {
                assert!(
                    !local.join("big.bin").exists(),
                    "download continued after pause"
                );
            } else {
                assert!(
                    !nas.ls(&remote)
                        .await
                        .unwrap()
                        .iter()
                        .any(|e| e.name == "big.bin"),
                    "upload completed after pause"
                );
            }
            crate::traffic::set_limit(key, "-1");
            manager.resume_task(&db, id).await.unwrap();
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let complete = if mode == "downloadonly" {
                        std::fs::read(local.join("big.bin")).ok() == Some(bytes.clone())
                    } else {
                        nas.download(&format!("{remote}/big.bin")).await.ok() == Some(bytes.clone())
                    };
                    if complete {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("resume did not finish transfer");
            manager.stop_task(id).await.unwrap();
            nas.remove(vec![remote]).await.unwrap();
            eprintln!("PASS {mode}: active transfer canceled, paused status retained, resume content verified");
        }
    }
}
