//! NAS 文件操作抽象 —— 按需同步后端。
//!
//! 抽象成 trait 以便用本地假实现（`FakeNas`）做真实 FUSE 挂载的端到端测试，
//! 生产实现直接包装 [`NasClient`]（file.ls / file.mkdir / file.rm / file.rename /
//! file.download → GET / file.checkUpload → POST /upload）。

use super::super::super::connect::nas_client::{NasClient, RemoteEntry};
use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use parking_lot::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub trait RemoteOps: Send + Sync + 'static {
    fn ls<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Vec<RemoteEntry>>>;
    fn mkdir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<()>>;
    fn remove<'a>(&'a self, paths: &'a [String]) -> BoxFuture<'a, Result<()>>;
    fn rename<'a>(&'a self, path: &'a str, new_name: &'a str) -> BoxFuture<'a, Result<()>>;
    /// Bounded random read without materializing the whole file.
    fn read_range<'a>(
        &'a self,
        _remote: &'a str,
        _offset: u64,
        _size: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async { anyhow::bail!("远端不支持分段读取，请使用下载到本地") })
    }
    fn download<'a>(&'a self, remote: &'a str) -> BoxFuture<'a, Result<Vec<u8>>>;
    /// 流式下载到文件（不整文件进内存），`progress` 每块回调累计字节数。
    fn download_to<'a>(
        &'a self,
        remote: &'a str,
        dest: &'a std::path::Path,
        progress: Arc<dyn Fn(u64) + Send + Sync>,
    ) -> BoxFuture<'a, Result<u64>>;
    fn upload<'a>(
        &'a self,
        remote: &'a str,
        data: Vec<u8>,
        mtime: Option<i64>,
    ) -> BoxFuture<'a, Result<()>>;
}

/// 带存活探测的远端会话。
///
/// [`NasClient`] 在收到会话失效（errno 4224/135168/65534）时会主动 `disconnect()`；
/// 普通同步由 `TaskActor` 负责重连，但按需同步的挂载没有 actor，因此必须由这一层
/// 自己保证“连接死了能重建”，否则一次会话失效就会让整棵挂载树永久不可用。
pub trait RemoteSession: RemoteOps {
    /// 连接当前是否仍可用。
    fn is_live(&self) -> bool;
}

impl RemoteSession for NasClient {
    fn is_live(&self) -> bool {
        self.is_connected()
    }
}

/// 重建一个已认证会话的工厂。返回 `Err` 表示这次重建失败。
pub type SessionConnector =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Arc<dyn RemoteSession>>> + Send + Sync>;

/// 重连失败后的冷却窗口：NAS 不可达时避免每个回调都发起一次建连
/// （每次建连最长要等一个网络超时）。
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

/// 单次建连/认证的最长耗时。必须是有限的：网络握手卡住时若无限等待，
/// 重连闸门会被永久持有，所有 FUSE 请求都得不到应答。
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(15);

struct ReconnectState {
    last_failure: Option<(Instant, String)>,
}

/// 自动重连的远端包装。
///
/// 并发语义很关键：fnOS 同一账号只保留**最新**会话，多个调用方同时重建会互相把
/// 对方踢下线，形成无限互踢。因此重建必须单飞（同一时刻只允许一个在建连）。
pub struct ReconnectingRemote {
    connector: SessionConnector,
    current: RwLock<Option<Arc<dyn RemoteSession>>>,
    /// 单飞闸门 + 失败冷却；持有期间才会真正发起建连。
    gate: tokio::sync::Mutex<ReconnectState>,
    reconnects: AtomicU64,
    /// 建连/认证的超时上限（测试可注入更短的值）。
    reconnect_timeout: Duration,
}

impl ReconnectingRemote {
    pub fn new(initial: Option<Arc<dyn RemoteSession>>, connector: SessionConnector) -> Arc<Self> {
        Arc::new(Self {
            connector,
            current: RwLock::new(initial),
            gate: tokio::sync::Mutex::new(ReconnectState { last_failure: None }),
            reconnects: AtomicU64::new(0),
            reconnect_timeout: RECONNECT_TIMEOUT,
        })
    }

    /// 仅供测试：缩短建连超时，便于断言“卡住的建连不会挂死调用方”。
    #[cfg(test)]
    fn with_reconnect_timeout(self: Arc<Self>, t: Duration) -> Arc<Self> {
        let mut inner = Arc::try_unwrap(self).ok().expect("sole owner");
        inner.reconnect_timeout = t;
        Arc::new(inner)
    }

    /// 已建立的会话总数（含首次连接）。
    pub fn reconnect_count(&self) -> u64 {
        self.reconnects.load(Ordering::Relaxed)
    }

    fn live_current(&self) -> Option<Arc<dyn RemoteSession>> {
        match self.current.read().as_ref() {
            Some(s) if s.is_live() => Some(s.clone()),
            _ => None,
        }
    }

    async fn session(&self) -> Result<Arc<dyn RemoteSession>> {
        if let Some(s) = self.live_current() {
            return Ok(s);
        }
        self.reconnect().await
    }

    async fn reconnect(&self) -> Result<Arc<dyn RemoteSession>> {
        // 闸门等待本身也要有上限：绝不能因为别人在重连就无限排队。
        let mut gate = match tokio::time::timeout(self.reconnect_timeout, self.gate.lock()).await {
            Ok(g) => g,
            Err(_) => anyhow::bail!("timed out waiting for NAS reconnect gate"),
        };
        // 双检：排队等闸门的其它调用方可能已经把会话建好了。
        if let Some(s) = self.live_current() {
            return Ok(s);
        }
        if let Some((at, msg)) = &gate.last_failure {
            if at.elapsed() < RECONNECT_BACKOFF {
                anyhow::bail!("NAS reconnect backing off after recent failure: {msg}");
            }
        }
        // 建连/认证必须限时。否则网络握手一旦卡住，闸门会被永久持有，
        // 所有 FUSE 请求都拿不到应答 —— 用户看到的就是“整棵树卡死”。
        let built = match tokio::time::timeout(self.reconnect_timeout, (self.connector)()).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!(
                "NAS reconnect timed out after {:?}",
                self.reconnect_timeout
            )),
        };
        match built {
            Ok(s) => {
                gate.last_failure = None;
                *self.current.write() = Some(s.clone());
                let n = self.reconnects.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::info!(reconnects = n, "on-demand NAS session established");
                Ok(s)
            }
            Err(e) => {
                gate.last_failure = Some((Instant::now(), e.to_string()));
                Err(e).context("reconnect NAS session for on-demand mount")
            }
        }
    }

    /// 执行一次操作；若操作后会话已断（服务端作废），重建并重试一次，用户无感。
    async fn first_session(&self) -> Result<Arc<dyn RemoteSession>> {
        self.session().await
    }

    /// 会话失效后重建（清空当前引用，交给单飞闸门处理）。
    async fn after_death(&self) -> Result<Arc<dyn RemoteSession>> {
        *self.current.write() = None;
        self.reconnect().await
    }
}

macro_rules! od_op {
    ($self:expr, $label:literal, $limit:expr, |$s:ident| $call:expr) => {{
        // 硬上限：会话获取 + 重连 + 实际调用，任何一步都不能无限期等待。
        // FUSE 回调只要不回包，内核就会一直等下去，整棵挂载树表现为“卡死”。
        let outer = $limit + $self.reconnect_timeout + Duration::from_secs(5);
        let run = async {
            let session = $self.first_session().await?;
            let outcome = {
                let $s = &session;
                within($limit, $label, $call).await
            };
            match outcome {
                Ok(v) => Ok(v),
                Err(e) => {
                    if session.is_live() {
                        return Err(e);
                    }
                    tracing::warn!(op = $label, error = %e, "session died mid-operation; reconnecting");
                    let fresh = $self.after_death().await?;
                    let $s = &fresh;
                    within($limit, $label, $call).await
                }
            }
        };
        match tokio::time::timeout(outer, run).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!(
                "{} exceeded hard deadline {:?}",
                $label,
                outer
            )),
        }
    }};
}

impl RemoteOps for ReconnectingRemote {
    fn ls<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Vec<RemoteEntry>>> {
        Box::pin(async move { od_op!(self, "ls", OP_TIMEOUT, |s| RemoteOps::ls(&**s, path)) })
    }

    fn mkdir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { od_op!(self, "mkdir", OP_TIMEOUT, |s| RemoteOps::mkdir(&**s, path)) })
    }

    fn remove<'a>(&'a self, paths: &'a [String]) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            od_op!(self, "remove", OP_TIMEOUT, |s| RemoteOps::remove(
                &**s, paths
            ))
        })
    }

    fn rename<'a>(&'a self, path: &'a str, new_name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            od_op!(self, "rename", OP_TIMEOUT, |s| RemoteOps::rename(
                &**s, path, new_name
            ))
        })
    }

    fn read_range<'a>(
        &'a self,
        remote: &'a str,
        offset: u64,
        size: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            od_op!(self, "read_range", OP_TIMEOUT, |s| RemoteOps::read_range(
                &**s, remote, offset, size
            ))
        })
    }
    fn download<'a>(&'a self, remote: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            od_op!(self, "download", DOWNLOAD_TIMEOUT, |s| RemoteOps::download(
                &**s, remote
            ))
        })
    }

    fn download_to<'a>(
        &'a self,
        remote: &'a str,
        dest: &'a std::path::Path,
        progress: Arc<dyn Fn(u64) + Send + Sync>,
    ) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            od_op!(self, "download_to", DOWNLOAD_TIMEOUT, |s| {
                RemoteOps::download_to(&**s, remote, dest, progress.clone())
            })
        })
    }

    fn upload<'a>(
        &'a self,
        remote: &'a str,
        data: Vec<u8>,
        mtime: Option<i64>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            od_op!(self, "upload", UPLOAD_TIMEOUT, |s| {
                RemoteOps::upload(&**s, remote, data.clone(), mtime)
            })
        })
    }
}

impl RemoteOps for NasClient {
    fn ls<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Vec<RemoteEntry>>> {
        Box::pin(async move { within(OP_TIMEOUT, "ls", NasClient::ls(self, path)).await })
    }

    fn mkdir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { within(OP_TIMEOUT, "mkdir", NasClient::mkdir(self, path)).await })
    }

    fn remove<'a>(&'a self, paths: &'a [String]) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            within(
                OP_TIMEOUT,
                "remove",
                NasClient::remove(self, paths.to_vec()),
            )
            .await
        })
    }

    fn rename<'a>(&'a self, path: &'a str, new_name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            within(
                OP_TIMEOUT,
                "rename",
                NasClient::rename(self, path, new_name),
            )
            .await
        })
    }

    fn read_range<'a>(
        &'a self,
        remote: &'a str,
        offset: u64,
        size: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            within(
                OP_TIMEOUT,
                "read_range",
                NasClient::read_range(self, remote, offset, size),
            )
            .await
        })
    }
    fn download<'a>(&'a self, remote: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            within(
                DOWNLOAD_TIMEOUT,
                "download",
                NasClient::download(self, remote),
            )
            .await
        })
    }

    fn download_to<'a>(
        &'a self,
        remote: &'a str,
        dest: &'a std::path::Path,
        progress: Arc<dyn Fn(u64) + Send + Sync>,
    ) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            within(DOWNLOAD_TIMEOUT, "download_to", async {
                NasClient::download_to(self, remote, dest, move |n| progress(n)).await
            })
            .await
        })
    }

    fn upload<'a>(
        &'a self,
        remote: &'a str,
        data: Vec<u8>,
        mtime: Option<i64>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            within(
                UPLOAD_TIMEOUT,
                "upload",
                NasClient::upload(self, remote, data, mtime),
            )
            .await
        })
    }
}

/// FUSE 回调上的远端调用必须有上限：NAS 掉线时若不设期限，请求会永远悬着，
/// 用户看到的就是“挂载点卡死、进程也退不掉”。
// 元数据操作（ls/stat/rename/remove）的硬超时。
// 远端正常应在 1 秒内返回；给到 30 秒会让文件管理器在任何一次网络抖动里
// 卡住半分钟，用户体感就是“卡死”。10 秒足以覆盖慢速网络与冷启动。
const OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
// HTTP body has a 30s idle timeout; allow large files on slow links to keep progressing.
const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
const UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

async fn within<T>(
    limit: std::time::Duration,
    op: &str,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!("{op} timed out after {:?}", limit)),
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! 内存假 NAS：仅供本地 FUSE 端到端测试。

    use super::*;
    use parking_lot::RwLock;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[derive(Debug, Clone, Default)]
    struct Node {
        dir: bool,
        data: Vec<u8>,
        mtim: i64,
    }

    /// 路径 → 节点。路径不含首尾 '/'，如 `dir/a.txt`。
    #[derive(Clone)]
    pub struct FakeNas {
        pub nodes: Arc<RwLock<BTreeMap<String, Node>>>,
    }

    impl Default for FakeNas {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FakeNas {
        pub fn new() -> Self {
            Self {
                nodes: Arc::new(RwLock::new(BTreeMap::new())),
            }
        }

        /// 创建一个远端文件（模拟 NAS 侧已有的内容）。
        pub fn seed(&self, path: &str, data: &[u8]) {
            self.nodes.write().insert(
                path.trim_matches('/').to_string(),
                Node {
                    dir: false,
                    data: data.to_vec(),
                    mtim: 1700000000,
                },
            );
        }

        pub fn seed_dir(&self, path: &str) {
            self.nodes.write().insert(
                path.trim_matches('/').to_string(),
                Node {
                    dir: true,
                    ..Default::default()
                },
            );
        }

        pub fn exists(&self, path: &str) -> bool {
            self.nodes.read().contains_key(path.trim_matches('/'))
        }

        pub fn all_keys(&self) -> Vec<String> {
            self.nodes.read().keys().cloned().collect()
        }

        pub fn read(&self, path: &str) -> Option<Vec<u8>> {
            self.nodes
                .read()
                .get(path.trim_matches('/'))
                .map(|n| n.data.clone())
        }

        /// 取某路径的历史冲突副本内容（文件名含 " - Conflicted Copy"）。
        pub fn snapshot_conflicted(&self, path: &str) -> Vec<Vec<u8>> {
            let (parent, name) = match path.trim_matches('/').rsplit_once('/') {
                Some((p, n)) => (format!("{p}/"), n.to_string()),
                None => (String::new(), path.trim_matches('/').to_string()),
            };
            self.nodes
                .read()
                .iter()
                .filter(|(k, _)| {
                    k.starts_with(&parent)
                        && k[parent.len()..]
                            .strip_prefix(&name)
                            .map(|rest| rest.starts_with(" - Conflicted Copy"))
                            .unwrap_or(false)
                })
                .map(|(_, v)| v.data.clone())
                .collect()
        }

        fn entries_of(&self, dir: &str) -> Vec<RemoteEntry> {
            let nodes = self.nodes.read();
            let prefix = if dir.is_empty() {
                String::new()
            } else {
                format!("{dir}/")
            };
            let mut out = Vec::new();
            for (path, node) in nodes.iter() {
                let Some(rest) = path.strip_prefix(&prefix) else {
                    continue;
                };
                if rest.is_empty() || rest.contains('/') {
                    continue;
                }
                out.push(RemoteEntry {
                    revision: String::new(),
                    name: rest.to_string(),
                    dir: node.dir,
                    size: node.data.len() as u64,
                    mtim: node.mtim,
                    btim: node.mtim,
                    uid: 1000,
                });
            }
            out
        }
    }

    impl RemoteOps for FakeNas {
        fn ls<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Vec<RemoteEntry>>> {
            let dir = path.trim_matches('/').to_string();
            Box::pin(async move {
                if !dir.is_empty() {
                    let nodes = self.nodes.read();
                    match nodes.get(&dir) {
                        Some(n) if !n.dir => return Err(anyhow::anyhow!("not a directory: {dir}")),
                        None => return Err(anyhow::anyhow!("no such directory: {dir}")),
                        _ => {}
                    }
                }
                Ok(self.entries_of(&dir))
            })
        }

        fn mkdir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<()>> {
            let path = path.trim_matches('/').to_string();
            Box::pin(async move {
                self.nodes.write().insert(
                    path,
                    Node {
                        dir: true,
                        ..Default::default()
                    },
                );
                Ok(())
            })
        }

        fn remove<'a>(&'a self, paths: &'a [String]) -> BoxFuture<'a, Result<()>> {
            let paths: Vec<String> = paths
                .iter()
                .map(|p| p.trim_matches('/').to_string())
                .collect();
            Box::pin(async move {
                let mut nodes = self.nodes.write();
                let to_remove: Vec<String> = nodes
                    .keys()
                    .filter(|k| {
                        paths
                            .iter()
                            .any(|p| k.as_str() == p || k.starts_with(&format!("{p}/")))
                    })
                    .cloned()
                    .collect();
                for k in to_remove {
                    nodes.remove(&k);
                }
                Ok(())
            })
        }

        fn rename<'a>(&'a self, path: &'a str, new_name: &'a str) -> BoxFuture<'a, Result<()>> {
            let path = path.trim_matches('/').to_string();
            let new_name = new_name.to_string();
            Box::pin(async move {
                let (parent, _old_name) = path
                    .rsplit_once('/')
                    .map(|(p, n)| (p.to_string(), n.to_string()))
                    .unwrap_or((String::new(), path.clone()));
                let new_path = if parent.is_empty() {
                    new_name.clone()
                } else {
                    format!("{parent}/{new_name}")
                };
                let mut nodes = self.nodes.write();
                let node = nodes
                    .remove(&path)
                    .ok_or_else(|| anyhow::anyhow!("rename source missing: {path}"))?;
                nodes.insert(new_path, node);
                Ok(())
            })
        }

        fn read_range<'a>(
            &'a self,
            remote: &'a str,
            offset: u64,
            size: usize,
        ) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async move {
                let nodes = self.nodes.read();
                let node = nodes
                    .get(remote.trim_matches('/'))
                    .ok_or_else(|| anyhow::anyhow!("not found"))?;
                let start = (offset as usize).min(node.data.len());
                let end = start.saturating_add(size).min(node.data.len());
                Ok(node.data[start..end].to_vec())
            })
        }
        fn download<'a>(&'a self, remote: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
            let remote = remote.trim_matches('/').to_string();
            Box::pin(async move {
                let nodes = self.nodes.read();
                let node = nodes
                    .get(&remote)
                    .ok_or_else(|| anyhow::anyhow!("no such file: {remote}"))?;
                if node.dir {
                    return Err(anyhow::anyhow!("is a directory: {remote}"));
                }
                Ok(node.data.clone())
            })
        }

        fn download_to<'a>(
            &'a self,
            remote: &'a str,
            dest: &'a std::path::Path,
            progress: Arc<dyn Fn(u64) + Send + Sync>,
        ) -> BoxFuture<'a, Result<u64>> {
            Box::pin(async move {
                let data = RemoteOps::download(self, remote).await?;
                if let Some(p) = dest.parent() {
                    std::fs::create_dir_all(p)?;
                }
                // 模拟分块写入，让测试能观察到进度递增
                let mut written = 0u64;
                let mut f = std::fs::File::create(dest)?;
                use std::io::Write;
                for chunk in data.chunks(64 * 1024) {
                    f.write_all(chunk)?;
                    written += chunk.len() as u64;
                    progress(written);
                }
                Ok(written)
            })
        }

        fn upload<'a>(
            &'a self,
            remote: &'a str,
            data: Vec<u8>,
            mtime: Option<i64>,
        ) -> BoxFuture<'a, Result<()>> {
            let remote = remote.trim_matches('/').to_string();
            Box::pin(async move {
                self.nodes.write().insert(
                    remote,
                    Node {
                        dir: false,
                        data,
                        mtim: mtime.unwrap_or(1700000000),
                    },
                );
                Ok(())
            })
        }
    }
}

#[cfg(test)]
mod reconnect_tests {
    //! 重连语义的确定性测试：全部使用内存假会话，不触碰网络。
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    /// 可控存活状态的假会话。`poison` 表示“下一次操作会被服务端判定会话失效”。
    struct FakeSession {
        live: AtomicBool,
        poison: AtomicBool,
    }

    impl FakeSession {
        fn new(live: bool) -> Arc<Self> {
            Arc::new(Self {
                live: AtomicBool::new(live),
                poison: AtomicBool::new(false),
            })
        }
        fn poisoned(self: &Arc<Self>) -> Arc<Self> {
            self.poison.store(true, Ordering::SeqCst);
            self.clone()
        }
        fn die(&self) {
            self.live.store(false, Ordering::SeqCst);
        }
    }

    fn entry() -> RemoteEntry {
        RemoteEntry {
            revision: String::new(),
            name: "a.txt".into(),
            dir: false,
            size: 1,
            mtim: 0,
            btim: 0,
            uid: 1000,
        }
    }

    impl RemoteOps for FakeSession {
        fn ls<'a>(&'a self, _path: &'a str) -> BoxFuture<'a, Result<Vec<RemoteEntry>>> {
            Box::pin(async move {
                if self.poison.swap(false, Ordering::SeqCst) {
                    // 模拟 NasClient 收到 4224 后自断并返回错误
                    self.die();
                    anyhow::bail!("NAS session expired (errno 4224)");
                }
                if !self.live.load(Ordering::SeqCst) {
                    anyhow::bail!("NAS connection closed");
                }
                Ok(vec![entry()])
            })
        }
        fn mkdir<'a>(&'a self, _path: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn remove<'a>(&'a self, _paths: &'a [String]) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn rename<'a>(&'a self, _p: &'a str, _n: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn download<'a>(&'a self, _remote: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async { Ok(vec![1u8]) })
        }
        fn download_to<'a>(
            &'a self,
            _remote: &'a str,
            dest: &'a std::path::Path,
            progress: Arc<dyn Fn(u64) + Send + Sync>,
        ) -> BoxFuture<'a, Result<u64>> {
            Box::pin(async move {
                std::fs::write(dest, [1u8])?;
                progress(1);
                Ok(1)
            })
        }
        fn upload<'a>(
            &'a self,
            _remote: &'a str,
            _data: Vec<u8>,
            _mtime: Option<i64>,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl RemoteSession for FakeSession {
        fn is_live(&self) -> bool {
            self.live.load(Ordering::SeqCst)
        }
    }

    #[tokio::test]
    async fn retries_once_after_session_is_invalidated() {
        let calls = Arc::new(AtomicUsize::new(0));
        let initial = FakeSession::new(true);
        let poisoned = initial.poisoned();

        let c = calls.clone();
        let connector: SessionConnector = Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            let fresh: Arc<dyn RemoteSession> = FakeSession::new(true);
            Box::pin(async move { Ok(fresh) })
        });

        let remote = ReconnectingRemote::new(Some(poisoned as Arc<dyn RemoteSession>), connector);
        // 首次调用会命中“会话失效”，应自动重建并重试成功。
        let entries = RemoteOps::ls(&*remote, "root").await.expect("ls recovers");
        assert_eq!(entries.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "应只重建一次");
        assert_eq!(remote.reconnect_count(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_callers_share_a_single_reconnect() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let connector: SessionConnector = Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                // 建连有耗时，期间其它调用方必须排队而不是各自建连
                tokio::time::sleep(Duration::from_millis(120)).await;
                let s: Arc<dyn RemoteSession> = FakeSession::new(true);
                Ok(s)
            })
        });
        // 初始会话已死：所有并发调用都必须走重连路径
        let dead: Arc<dyn RemoteSession> = FakeSession::new(false);
        let remote = Arc::new(ReconnectingRemote::new(Some(dead), connector));

        let mut handlers = Vec::new();
        for _ in 0..8 {
            let r = remote.clone();
            handlers.push(tokio::spawn(async move {
                RemoteOps::ls(&**r, "root").await.map(|v| v.len())
            }));
        }
        for h in handlers {
            assert_eq!(h.await.unwrap().unwrap(), 1);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "并发重连必须单飞，否则会互踢会话"
        );
    }

    /// 关键回归：建连永远不返回（网络握手卡死）时，调用方必须限时失败，
    /// 绝不能永久挂住 —— 那会让整个 FUSE 挂载树卡死。
    #[tokio::test]
    async fn hung_connect_fails_fast_instead_of_hanging_forever() {
        let connector: SessionConnector = Arc::new(move || {
            Box::pin(async move {
                // 永不返回，模拟卡住的 TLS/WebSocket 握手
                std::future::pending::<()>().await;
                unreachable!()
            })
        });
        let dead: Arc<dyn RemoteSession> = FakeSession::new(false);
        let remote = ReconnectingRemote::new(Some(dead), connector)
            .with_reconnect_timeout(Duration::from_millis(200));

        let t0 = std::time::Instant::now();
        let err = RemoteOps::ls(&*remote, "root").await.unwrap_err();
        let elapsed = t0.elapsed();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("timed out"),
            "unexpected error chain: {chain}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "调用方被挂住了 {elapsed:?}，必须限时失败"
        );
    }

    #[tokio::test]
    async fn failed_reconnect_backs_off_instead_of_hammering() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let connector: SessionConnector = Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(anyhow::anyhow!("NAS unreachable")) })
        });
        let dead: Arc<dyn RemoteSession> = FakeSession::new(false);
        let remote = ReconnectingRemote::new(Some(dead), connector);

        assert!(RemoteOps::ls(&*remote, "root").await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // 冷却期内不再发起建连（避免 8 个池线程各等一次 30s 超时）
        let err = RemoteOps::ls(&*remote, "root").await.unwrap_err();
        assert!(
            err.to_string().contains("backing off"),
            "unexpected error: {err}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "冷却期内不应重试建连");
    }
}
