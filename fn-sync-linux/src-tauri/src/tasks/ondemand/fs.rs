//! 按需同步 FUSE 文件系统。
//!
//! 语义对齐官方按需同步（Windows CfAPI / macOS FileProvider）：
//! - 目录实时反映 NAS 内容（file.ls，TTL 缓存 + 内核失效通知）；
//! - 文件初始为“占位”（只有元数据，不占磁盘），首次 open/read 时下载（等价 DemandWriter）；
//! - 本地修改落缓存并在 flush/worker 中上传（等价官方上传通道）；
//! - 远端已修改 → Conflicted Copy 保留双方；
//! - 缓存 LRU 淘汰（等价官方“释放空间”）。
//!
//! 并发模型：fuser 派发循环单线程；可能耗时的操作（下载、写、fsync）
//! 在独立线程 / tokio 任务中完成后异步回复，避免整棵挂载树卡死。

use super::cache::{CacheMgr, CacheState};
use super::remote::RemoteOps;
use super::OnDemandConfig;
use crate::service::logs;
use crate::tasks::file_filter::FileFilter;
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig, MountOption, Notifier, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    ReplyXattr, Request, TimeOrNow,
};
use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// 属性 TTL 设为 0：每次 stat 都回问守护进程，保证状态列（已下载/下载中）
// 反映实时状态。代价可忽略（纯内存查询）。
const ATTR_TTL: Duration = Duration::from_secs(0);
// 目录列表 TTL：lookup 未命中时才会走远端 ls。TTL 太短会让每次 lookup 都
// 变成一次网络 RPC，是“整个挂载树卡死”的主要来源。远端变化由 worker 轮询
// + 内核失效通知补齐，因此这里放宽到 5s。
const DIR_TTL: Duration = Duration::from_secs(5);
/// 每轮差异轮询最多处理的目录数（并发）。缓存目录很多时避免把 NAS 打满。
const MAX_DIRS_PER_POLL: usize = 8;
const ROOT_POLL_TICKS: u32 = 2; // worker 每 tick 2s，差异轮询每 4s
const EVICT_TICKS: u32 = 15; // 每 30s 检查一次缓存淘汰
const DIR_SIZE: u64 = 4096;
const RANGE_THRESHOLD: u64 = 16 * 1024 * 1024;
const RANGE_BLOCK_BYTES: usize = 256 * 1024;
const MAX_RANGE_BLOCKS: usize = 16;
type RangeKey = (String, u64, (u64, i64));

pub const ROOT_INO: u64 = 1;

/// 供 `getfattr` 查询的扩展属性名。
///
/// Linux 没有 Windows/macOS 那种系统级云朵图标，xattr 是让状态可被脚本、
/// 文件管理器插件和终端查询的通用方式。
pub const XATTR_PREFIX: &str = "user.fnsync.";
pub const XATTR_STATE: &str = "user.fnsync.state";
pub const XATTR_REMOTE_SIZE: &str = "user.fnsync.remote-size";
pub const XATTR_LOCAL_SIZE: &str = "user.fnsync.local-size";
/// 当前传输进度（0-100）；无传输时不存在。
pub const XATTR_PROGRESS: &str = "user.fnsync.progress";
/// 是否"始终保留在此电脑"（pin）：1/0。对应 PinState=Pinned。
pub const XATTR_PINNED: &str = "user.fnsync.pinned";

#[derive(Debug, Clone)]
pub struct Inode {
    /// 相对 nas_root 的路径；根为 ""
    pub rel: String,
    pub kind: FileType,
    /// 可见大小（脏 → 本地大小；否则远端大小）
    pub size: u64,
    /// 可见 mtime（秒）
    pub mtime: i64,
    /// 最近一次观察到的远端戳 (size, mtim)
    pub stamp: (u64, i64),
    pub dirty: bool,
    pub pinned: bool,
    /// 自定义权限位（0 = 按类型取默认值）。只保留 mode 的低 12 位，
    /// 这样 `chmod` / 回收站要求的 sticky 位（1777）才能生效。
    pub mode: u32,
    /// 由本地 create 产生、尚未上传成功（期间由 overlay 保证可见）
    pub created: bool,
}

#[derive(Debug, Default)]
struct Overlay {
    /// 本地新建（未上传成功）：名字 → ino
    created: BTreeMap<String, u64>,
    /// 本地删除（远端删除进行中/失败重试）
    deleted: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct RemoteListEntry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    pub mtim: i64,
    pub btim: i64,
}

struct DirList {
    entries: Vec<RemoteListEntry>,
    fetched: Instant,
}

/// 固定大小的阻塞任务池。
///
/// FUSE 的派发循环是**单线程**的：只要某个回调在派发线程上等待网络，
/// 整棵挂载树就再也收不到任何请求——这就是“挂载卡死”的表象。
/// 因此所有会碰远端（lookup / readdir / create / mkdir / unlink / rename /
/// fsync / read 未命中）的请求一律提交到这里执行。
///
/// 用固定线程数而不是每个请求一个线程：`find` 之类的递归遍历会瞬间产生
/// 成千上万个 lookup，每请求一线程会直接把进程压垮。
pub struct OpPool {
    tx: std::sync::mpsc::SyncSender<Box<dyn FnOnce() + Send + 'static>>,
    /// 在途操作：id → (标签, 开始时刻)。用于诊断“哪个回调卡住了”。
    inflight: Arc<DashMap<u64, (&'static str, Instant)>>,
    seq: Arc<AtomicU64>,
    /// 已提交但尚未取到线程执行的数量（队列积压）。
    queued: Arc<AtomicU64>,
}

impl OpPool {
    fn new(threads: usize) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Box<dyn FnOnce() + Send + 'static>>(128);
        let rx = Arc::new(Mutex::new(rx));
        let inflight: Arc<DashMap<u64, (&'static str, Instant)>> = Arc::new(DashMap::new());
        let queued = Arc::new(AtomicU64::new(0));
        for i in 0..threads {
            let rx = rx.clone();
            let queued = queued.clone();
            let _ = std::thread::Builder::new()
                .name(format!("fnsync-od-op-{i}"))
                .spawn(move || loop {
                    // 只在 recv 期间持锁，执行任务时不持锁，保证并发
                    let job = {
                        let rx = rx.lock();
                        rx.recv()
                    };
                    match job {
                        Ok(job) => {
                            queued.fetch_sub(1, Ordering::Relaxed);
                            job()
                        }
                        Err(_) => break, // 发送端（Shared）已释放
                    }
                });
        }
        Self {
            tx,
            inflight,
            seq: Arc::new(AtomicU64::new(1)),
            queued,
        }
    }

    /// 提交任务；池已关闭时返回 false。
    pub fn submit(&self, label: &'static str, job: Box<dyn FnOnce() + Send + 'static>) -> bool {
        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        self.inflight.insert(id, (label, Instant::now()));
        self.queued.fetch_add(1, Ordering::Relaxed);
        let inflight = self.inflight.clone();
        let wrapped: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
            job();
            inflight.remove(&id);
        });
        match self.tx.try_send(wrapped) {
            Ok(()) => true,
            Err(_) => {
                self.inflight.remove(&id);
                self.queued.fetch_sub(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// 诊断：列出执行时间超过 `threshold` 的操作。
    pub fn slow(&self, threshold: Duration) -> Vec<(String, Duration)> {
        self.inflight
            .iter()
            .filter_map(|e| {
                let elapsed = e.value().1.elapsed();
                (elapsed >= threshold).then(|| (e.value().0.to_string(), elapsed))
            })
            .collect()
    }

    pub fn queued(&self) -> u64 {
        self.queued.load(Ordering::Relaxed)
    }
}

/// 需要发给内核的失效通知。
#[derive(Debug, Clone)]
enum NotifyMsg {
    /// 目录项失效（父 inode + 名字）：文件被创建/删除/改名时用
    Entry { parent: u64, name: String },
    /// inode 属性/内容失效：文件状态变化（如下载完成）时用
    Inode { ino: u64 },
}

/// 正在进行的传输（对应官方 CfAPI 的 transfer key / macOS 的 NSProgress）。
///
/// 目的：让用户能看见“正在下载/上传”。官方向文件管理器推送进度条，
/// Linux 的 FUSE 没有等价通道，因此改为通过 xattr/状态列暴露，
/// 文件管理器列和 `fnsync-ctl status` 都能实时看到。
#[derive(Debug)]
pub struct Transfer {
    /// "download" | "upload"
    pub kind: &'static str,
    /// 总字节数（未知时 0）
    pub total: u64,
    /// 已传输字节数，下载/上传过程中实时更新（闭包里需要共享，故用 Arc）
    pub done: Arc<AtomicU64>,
    pub started: Instant,
}

impl Transfer {
    fn new(kind: &'static str, total: u64) -> Self {
        Self {
            kind,
            total,
            done: Arc::new(AtomicU64::new(0)),
            started: Instant::now(),
        }
    }

    /// 进度百分比（0-100）。总量未知时返回 0。
    pub fn percent(&self) -> u64 {
        if self.total == 0 {
            return 0;
        }
        (self.done.load(Ordering::Relaxed).saturating_mul(100) / self.total).min(100)
    }
}

pub struct Shared {
    pub cfg: OnDemandConfig,
    pub nas: Arc<dyn RemoteOps>,
    pub rt: tokio::runtime::Handle,
    pub cache: CacheMgr,
    pub db: sqlx::SqlitePool,
    pub filter: FileFilter,
    /// ino → inode
    pub inodes: DashMap<u64, RwLock<Inode>>,
    /// rel → ino
    pub ino_by_rel: DashMap<String, u64>,
    /// 目录列表缓存：parent ino → 列表
    dir_cache: DashMap<u64, RwLock<DirList>>,
    /// 每个目录一把异步锁：并发 lookup/readdir 未命中时只让一个真正请求远端 ls
    dir_locks: DashMap<u64, Arc<tokio::sync::Mutex<()>>>,
    /// 打开的文件：ino → 文件句柄（pread/pwrite）
    open_files: DashMap<u64, Arc<Mutex<std::fs::File>>>,
    /// 打开计数：多个句柄共享同一缓存文件，最后一个 release 才真正关闭。
    /// 提前关闭会让仍在读写的进程拿到已失效的打开文件。
    open_count: DashMap<u64, usize>,
    /// 脏元数据待落盘。写路径只置位，由 worker 合并落盘；若在 write 回调里
    /// 每次都 persist_meta（序列化全部条目 + 写盘 + rename），大文件写入会
    /// 直接在派发线程上卡死。
    meta_dirty: AtomicBool,
    /// 本地变更 overlay：parent ino → 变更集
    overlays: DashMap<u64, RwLock<Overlay>>,
    /// 删除待执行（rel → 已重试次数）
    pub pending_removes: DashMap<String, u32>,
    /// rel → 正在进行的传输（用于状态展示与进度）
    pub transfers: DashMap<String, Transfer>,
    /// 「始终保留在此电脑」的路径集合（对应官方 PinState=Pinned）。
    /// 持久化在缓存目录的 pins.json，CLI/菜单修改后 worker 会自动重载。
    pub pins: DashMap<String, ()>,
    next_ino: AtomicU64,
    /// 内核失效通知的发送端。真正的 `Notifier::inval_*` 是同步写 /dev/fuse
    /// 的阻塞调用，绝不能在任何 runtime 线程上执行（否则会锁死整个 runtime，
    /// 表现就是挂载树卡死）。因此交给一个专用线程串行发送。
    notify_tx: std::sync::mpsc::SyncSender<NotifyMsg>,
    /// 通知线程的接收端（仅用于启动线程时移交）
    notify_rx: Arc<Mutex<std::sync::mpsc::Receiver<NotifyMsg>>>,
    /// 阻塞型 FUSE 回调的执行池（见 OpPool 文档）
    pub ops: Arc<OpPool>,
    pub transfer_ops: Arc<OpPool>,
    pub errors: DashMap<String, String>,
    pub queued_downloads: DashMap<String, ()>,
    pub control_slots: tokio::sync::Semaphore,
    read_slots: Arc<tokio::sync::Semaphore>,
    range_slots: tokio::sync::Semaphore,
    range_cache: Mutex<HashMap<RangeKey, Arc<Vec<u8>>>>,
    download_slots: tokio::sync::Semaphore,
    pin_lock: Mutex<()>,
    open_gate: Mutex<()>,
    pub stopping: Arc<AtomicBool>,
    uid: u32,
    gid: u32,
}

impl Shared {
    pub fn new(
        cfg: OnDemandConfig,
        nas: Arc<dyn RemoteOps>,
        rt: tokio::runtime::Handle,
        db: sqlx::SqlitePool,
        filter: FileFilter,
    ) -> Result<Self> {
        let cache = CacheMgr::new(cfg.cache_root.clone(), cfg.cache_limit_bytes)?;
        cache.load_meta();
        cache.clean_tmp();
        // 失效通知通道：专用线程消费，避免阻塞 runtime
        let (notify_tx, notify_rx) = std::sync::mpsc::sync_channel::<NotifyMsg>(256);
        let notify_rx = Arc::new(Mutex::new(notify_rx));
        let shared = Self {
            notify_rx,
            inodes: DashMap::new(),
            ino_by_rel: DashMap::new(),
            dir_cache: DashMap::new(),
            dir_locks: DashMap::new(),
            open_files: DashMap::new(),
            open_count: DashMap::new(),
            meta_dirty: AtomicBool::new(false),
            overlays: DashMap::new(),
            pending_removes: DashMap::new(),
            transfers: DashMap::new(),
            pins: DashMap::new(),
            next_ino: AtomicU64::new(ROOT_INO + 1),
            notify_tx,
            // 并发下载/遍历会同时占用多个池线程；线程太少会让后续请求排队等待。
            ops: Arc::new(OpPool::new(8)),
            transfer_ops: Arc::new(OpPool::new(8)),
            errors: DashMap::new(),
            queued_downloads: DashMap::new(),
            control_slots: tokio::sync::Semaphore::new(4),
            read_slots: Arc::new(tokio::sync::Semaphore::new(128)),
            range_slots: tokio::sync::Semaphore::new(4),
            range_cache: Mutex::new(HashMap::new()),
            download_slots: tokio::sync::Semaphore::new(4),
            pin_lock: Mutex::new(()),
            open_gate: Mutex::new(()),
            stopping: Arc::new(AtomicBool::new(false)),
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            cfg,
            nas,
            rt,
            cache,
            db,
            filter,
        };
        // 登记 root inode（rel = ""）
        shared.inodes.insert(
            ROOT_INO,
            RwLock::new(Inode {
                rel: String::new(),
                kind: FileType::Directory,
                size: DIR_SIZE,
                mtime: 0,
                stamp: (0, 0),
                dirty: false,
                pinned: false,
                mode: 0,
                created: false,
            }),
        );
        // 载入「始终保留」集合（上次运行留下的）
        shared.load_pins();
        Ok(shared)
    }

    pub fn remote_path(&self, rel: &str) -> String {
        if rel.is_empty() {
            self.cfg.nas_root.clone()
        } else {
            format!("{}/{}", self.cfg.nas_root, rel)
        }
    }

    /// 回收站目录是本机专用的保留名：**绝不同步到 NAS**。
    ///
    /// freedesktop 回收站规范要求 `$topdir/.Trash` 或 `$topdir/.Trash-$uid`，
    /// 二者必须在同一文件系统上。FUSE 挂载点自成一体，因此把这些名字当作
    /// “本地保留名”处理：只在挂载内存在，读写都走本地，不会出现在 NAS 上。
    /// 这样文件管理器按 Delete 键（移动到回收站）才能成功，
    /// 并把“删除”按官方语义传播到 NAS。
    fn is_local_only(rel: &str) -> bool {
        match rel.split('/').next() {
            Some(first) => first == ".Trash" || first.starts_with(".Trash-"),
            None => false,
        }
    }

    /// 本地目录的磁盘位置（缓存根下同名路径）
    fn local_dir_path(&self, rel: &str) -> std::path::PathBuf {
        if rel.is_empty() {
            self.cache.root().to_path_buf()
        } else {
            self.cache.root().join(rel)
        }
    }

    /// 列出本地目录（用于回收站这类不同步的目录）
    fn local_list(&self, rel: &str, filter: &FileFilter) -> Vec<RemoteListEntry> {
        let mut out = Vec::new();
        let dir = self.local_dir_path(rel);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return out;
        };
        for e in rd.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            let name = e.file_name().to_string_lossy().to_string();
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let is_dir = meta.is_dir();
            let size = if is_dir { 0 } else { meta.len() };
            if !filter.should_sync(&child_rel, if is_dir { None } else { Some(size) }) {
                continue;
            }
            out.push(RemoteListEntry {
                name,
                dir: is_dir,
                size,
                mtim: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                btim: 0,
            });
        }
        out
    }

    fn parent_rel(rel: &str) -> &str {
        match rel.rsplit_once('/') {
            Some((p, _)) => p,
            None => "",
        }
    }

    fn file_name(rel: &str) -> &str {
        rel.rsplit('/').next().unwrap_or(rel)
    }

    fn alloc_ino(&self) -> u64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    /// 取或分配 ino，并确保 inode 记录存在。
    pub fn get_or_alloc(&self, rel: &str, kind: FileType) -> u64 {
        if rel.is_empty() {
            return ROOT_INO;
        }
        if let Some(ino) = self.ino_by_rel.get(rel) {
            return *ino;
        }
        let ino = self.alloc_ino();
        self.ino_by_rel.insert(rel.to_string(), ino);
        self.inodes.insert(
            ino,
            RwLock::new(Inode {
                rel: rel.to_string(),
                kind,
                size: 0,
                mtime: 0,
                stamp: (0, 0),
                dirty: false,
                pinned: false,
                mode: 0,
                created: false,
            }),
        );
        ino
    }

    fn drop_inode(&self, rel: &str) {
        if let Some((_, ino)) = self.ino_by_rel.remove(rel) {
            self.inodes.remove(&ino);
            self.dir_cache.remove(&ino);
        }
    }

    pub fn set_notifier(&self, n: Notifier) {
        // 专用通知线程：串行消费 NotifyMsg，逐个调用 inval_*。
        // 放在独立线程上是因为这些调用会同步写 /dev/fuse；若跑在 tokio
        // runtime 线程上，一旦内核侧等待就会锁死整个 runtime。
        let rx = self.notify_rx.clone();
        let _ = std::thread::Builder::new()
            .name("fnsync-od-notify".into())
            .spawn(move || loop {
                let msg = {
                    let rx = rx.lock();
                    rx.recv()
                };
                match msg {
                    Ok(m) => match m {
                        NotifyMsg::Entry { parent, name } => {
                            let _ = n.inval_entry(parent, OsStr::new(&name));
                        }
                        NotifyMsg::Inode { ino } => {
                            let _ = n.inval_inode(ino, 0, -1);
                        }
                    },
                    Err(_) => break,
                }
            });
    }

    /// 请求内核让某个目录项失效（异步、非阻塞）。
    fn notify_entry(&self, parent: u64, name: &str) {
        let _ = self.notify_tx.try_send(NotifyMsg::Entry {
            parent,
            name: name.to_string(),
        });
    }

    /// 请求内核刷新 inode 属性（状态列、大小等），并触发一次属性变更事件，
    /// 让文件管理器的状态列自动刷新。
    fn notify_inode(&self, ino: u64) {
        let _ = self.notify_tx.try_send(NotifyMsg::Inode { ino });
        // UI consumes daemon status directly; never re-enter the FUSE mount to touch files.
    }

    /// 远端列出目录（带 TTL 缓存与离线回退）。
    pub async fn list_dir(&self, parent: u64) -> Result<Vec<RemoteListEntry>> {
        let rel = {
            let ino = self
                .inodes
                .get(&parent)
                .ok_or_else(|| anyhow!("no inode {parent}"))?;
            let node = ino.read();
            if node.kind != FileType::Directory {
                anyhow::bail!("not a directory");
            }
            node.rel.clone()
        };
        // 本地保留名目录（回收站）：只列本地磁盘
        if Self::is_local_only(&rel) {
            return Ok(self.local_list(&rel, &self.filter));
        }
        if let Some(list) = self.dir_cache.get(&parent) {
            if list.read().fetched.elapsed() < DIR_TTL {
                let cached = list.read().entries.clone();
                return Ok(cached);
            }
        }
        // 同一目录的并发未命中只让一个请求真正打远端：否则 `find`/编辑器扫描
        // 会同时发出成百上千个重复 file.ls，把连接和线程池一起拖死。
        let lock = self
            .dir_locks
            .entry(parent)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        // 有界等待：持锁者若卡在网络上，其它请求不能无限排队（否则整棵树卡死）
        // 有界等待持锁者（它自己有 10s 硬超时）。40s 会让并发浏览卡住半分钟，
        // 用户体感就是文件管理器卡死；15s 留出余量又不至于无法忍受。
        let _guard = match tokio::time::timeout(Duration::from_secs(15), lock.lock()).await {
            Ok(g) => g,
            Err(_) => anyhow::bail!("directory listing lock timed out"),
        };
        if let Some(list) = self.dir_cache.get(&parent) {
            if list.read().fetched.elapsed() < DIR_TTL {
                return Ok(list.read().entries.clone());
            }
        }
        match self.fetch_remote_list(&rel).await {
            Ok(entries) => {
                self.dir_cache.insert(
                    parent,
                    RwLock::new(DirList {
                        entries: entries.clone(),
                        fetched: Instant::now(),
                    }),
                );
                Ok(entries)
            }
            Err(e) => {
                // 离线回退：过期的旧列表仍可用
                if let Some(list) = self.dir_cache.get(&parent) {
                    tracing::warn!(parent, error = %e, "ls failed; serving stale listing");
                    return Ok(list.read().entries.clone());
                }
                Err(e)
            }
        }
    }

    async fn fetch_remote_list(&self, rel: &str) -> Result<Vec<RemoteListEntry>> {
        let path = self.remote_path(rel);
        let raw = self.nas.ls(&path).await?;
        let mut out = Vec::with_capacity(raw.len());
        for entry in raw {
            let child_rel = if rel.is_empty() {
                entry.name.clone()
            } else {
                format!("{}/{}", rel, entry.name)
            };
            // 本地保留名（回收站）不参与远端同步
            if Self::is_local_only(&child_rel) {
                continue;
            }
            if !self.filter.should_sync(
                &child_rel,
                if entry.is_dir() {
                    None
                } else {
                    Some(entry.size)
                },
            ) {
                continue;
            }
            out.push(RemoteListEntry {
                name: entry.name.clone(),
                dir: entry.is_dir(),
                size: entry.size,
                mtim: entry.mtim,
                btim: entry.btim,
            });
        }
        Ok(out)
    }

    /// 解析 (parent, name) → ino。overlay 优先，然后目录列表缓存（TTL 内不再远端取）。
    pub async fn resolve_child(&self, parent: u64, name: &OsStr) -> Result<Option<u64>> {
        let name = name.to_string_lossy().to_string();
        if name.contains('/') || name.is_empty() || name == "." || name == ".." {
            return Ok(None);
        }
        if let Some(ov) = self.overlays.get(&parent) {
            let ov = ov.read();
            if ov.deleted.contains(&name) {
                return Ok(None);
            }
            if let Some(ino) = ov.created.get(&name) {
                return Ok(Some(*ino));
            }
        }
        let parent_rel = {
            let ino = self
                .inodes
                .get(&parent)
                .ok_or_else(|| anyhow!("no inode"))?;
            let node = ino.read();
            node.rel.clone()
        };
        let rel = if parent_rel.is_empty() {
            name.clone()
        } else {
            format!("{parent_rel}/{name}")
        };

        // 回收站等本地保留名：只查本地磁盘，绝不请求远端，
        // 也绝不走“远端戳变化就丢弃缓存”的逻辑（那是给同步文件用的）。
        if Self::is_local_only(&rel) {
            let path = self.local_dir_path(&rel);
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => return Ok(None),
            };
            let kind = if meta.is_dir() {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            let ino = self.get_or_alloc(&rel, kind);
            {
                let inode = self.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
                let mut node = inode.write();
                node.size = if kind == FileType::Directory {
                    DIR_SIZE
                } else {
                    meta.len()
                };
                node.mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                node.dirty = false;
            }
            return Ok(Some(ino));
        }

        let entries = self.list_dir(parent).await?;
        let entry = entries.iter().find(|e| e.name == name).cloned();
        let Some(entry) = entry else {
            return Ok(None);
        };
        let kind = if entry.dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        let ino = self.get_or_alloc(&rel, kind);
        let remote_stamp = (entry.size, entry.mtim);
        // 远端戳变化 → 旧缓存已过期。必须显式丢弃，否则“大小相同但内容变了”
        // 的情况会一直读到旧内容（`cached_content_ok` 只比较大小）。
        {
            let stale_stamp = self
                .inodes
                .get(&ino)
                .map(|n| n.read().stamp)
                .filter(|old| *old != (0, 0) && *old != remote_stamp);
            if let Some(old) = stale_stamp {
                if !self.is_dirty(&rel) {
                    let path = self.cache.file_path(&rel);
                    if path.exists() {
                        tracing::info!(rel = %rel, ?old, ?remote_stamp, "remote changed; dropping stale cache");
                        let _ = std::fs::remove_file(&path);
                    }
                    self.cache.remove(&rel);
                }
            }
        }
        {
            let inode = self.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
            let mut node = inode.write();
            // 目录列表缓存（TTL 内）可能落后于 worker 已推送的远端变化：
            // inode 上的 stamp 是最近一次“已采纳”的远端戳，只有当远端戳比它更新
            // 时才覆盖视图。否则 poll_root_diff 刚更新的新尺寸会被一次普通 lookup
            // 用旧列表抹掉（表现为“远端改了，本地一直显示旧值”）。
            if !node.dirty && remote_stamp > node.stamp {
                node.size = entry.size;
                node.mtime = entry.mtim;
                node.stamp = remote_stamp;
            }
        }
        // pin 状态（重启后从 pins.json 载入的集合）
        if self.pins.contains_key(&rel) {
            if let Some(inode) = self.inodes.get(&ino) {
                inode.write().pinned = true;
            }
        }
        // 重启恢复：缓存层记录的脏状态绑定到 inode
        if let Some(state) = self.cache.state(&rel) {
            if state.dirty {
                let inode = self.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
                let mut node = inode.write();
                node.dirty = true;
                node.size = state.size;
                node.mtime = state.mtime;
            }
        } else if kind == FileType::RegularFile {
            // 重启后 `load_meta` 只恢复脏文件，干净缓存的状态会丢失。
            // 此时磁盘上明明有完整内容，却被当成“占位”：du 显示 0 块，
            // 而且每次读取都会重新下载。这里用「远端大小 == 磁盘文件大小」
            // 认领已有缓存，并用刚取到的远端戳登记状态。
            let disk_size = std::fs::metadata(self.cache.file_path(&rel))
                .map(|m| m.len())
                .unwrap_or(u64::MAX);
            if disk_size == entry.size {
                let stamp = (entry.size, entry.mtim);
                self.cache.set_state(
                    &rel,
                    CacheState {
                        size: entry.size,
                        dirty: false,
                        uploading: false,
                        stamp,
                        mtime: entry.mtim,
                        last_access_ms: self.cache.now_ms(),
                    },
                );
            }
        }
        Ok(Some(ino))
    }

    fn attr_for(&self, ino: u64, node: &Inode) -> FileAttr {
        let kind = node.kind;
        let s = if kind == FileType::Directory {
            DIR_SIZE
        } else {
            node.size
        };
        let default_perm = if kind == FileType::Directory {
            0o755
        } else {
            0o644
        };
        // 允许 setattr(chmod) 覆盖；粘滞位（回收站 .Trash 需要 1777）必须保留。
        let perm = if node.mode != 0 {
            (node.mode & 0o7777) as u16
        } else {
            default_perm
        };
        let nlink = if kind == FileType::Directory { 2 } else { 1 };
        // 占位文件必须报告 0 个数据块，否则内核/文件管理器会认为它已经占满磁盘。
        //
        // 这是 Linux 上唯一通用的“是否已下载”信号（对应 Windows CfAPI 的
        // FILE_ATTRIBUTE_OFFLINE、macOS FileProvider 的 dataless 标记）：
        //   ls -l   / --apparent-size  → 远端大小（逻辑大小）
        //   du                          → 真实占用（未下载 = 0）
        //   stat -c %b                  → 0 表示仅有占位
        // 若按 size 计算块数，du 会显示满额，用户完全无法分辨文件是否已下载。
        let blocks = if kind == FileType::Directory {
            (DIR_SIZE + 511) / 512
        } else if node.dirty || self.cached_content_ok(&node.rel, node.size) {
            (s + 511) / 512
        } else {
            0
        };
        let t = UNIX_EPOCH + Duration::from_secs(node.mtime.max(0) as u64);
        FileAttr {
            ino,
            size: s,
            blocks,
            atime: t,
            mtime: t,
            ctime: t,
            crtime: t,
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn open_handle(&self, ino: u64) -> Result<Arc<Mutex<std::fs::File>>> {
        if let Some(f) = self.open_files.get(&ino) {
            return Ok(f.clone());
        }
        let rel = {
            let inode = self.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
            let node = inode.read();
            node.rel.clone()
        };
        let path = self.cache.file_path(&rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let handle = Arc::new(Mutex::new(file));
        self.open_files.insert(ino, handle.clone());
        Ok(handle)
    }

    fn mark_meta_dirty(&self) {
        self.meta_dirty.store(true, Ordering::Relaxed);
    }

    /// worker 每个 tick 合并一次落盘，避免写路径上反复序列化全部条目。
    fn flush_meta(&self) {
        if self.meta_dirty.swap(false, Ordering::Relaxed) {
            self.cache.persist_meta();
        }
    }

    fn inc_open(&self, ino: u64) {
        let _gate = self.open_gate.lock();
        *self.open_count.entry(ino).or_insert(0) += 1;
    }

    /// 递减打开计数；归零时才真正关闭缓存文件句柄。
    fn dec_open(&self, ino: u64) {
        let _gate = self.open_gate.lock();
        let remaining = {
            let mut count = self.open_count.entry(ino).or_insert(0);
            *count = count.saturating_sub(1);
            *count
        };
        if remaining == 0 {
            self.open_count.remove(&ino);
            self.open_files.remove(&ino);
        }
    }

    fn is_in_use(&self, rel: &str) -> bool {
        self.ino_by_rel
            .get(rel)
            .map(|ino| {
                self.open_count.get(&*ino).map(|n| *n > 0).unwrap_or(false)
                    || self.open_files.contains_key(&*ino)
            })
            .unwrap_or(false)
    }

    fn is_dirty(&self, rel: &str) -> bool {
        self.cache.state(rel).map(|s| s.dirty).unwrap_or(false)
    }

    /// pins.json 路径（放在缓存目录，与任务一一对应）
    fn pins_path(&self) -> std::path::PathBuf {
        self.cache.root().join("pins.json")
    }

    /// 从磁盘重载 pin 集合（CLI/菜单写入后由 worker 周期调用）。
    fn load_pins(&self) {
        let Ok(text) = std::fs::read_to_string(self.pins_path()) else {
            return;
        };
        let Ok(list) = serde_json::from_str::<Vec<String>>(&text) else {
            return;
        };
        let wanted: HashSet<String> = list.into_iter().collect();
        // 同步新增
        for rel in &wanted {
            self.pins.insert(rel.clone(), ());
        }
        // 同步移除
        let stale: Vec<String> = self
            .pins
            .iter()
            .map(|e| e.key().clone())
            .filter(|k| !wanted.contains(k))
            .collect();
        for k in stale {
            self.pins.remove(&k);
        }
        // 回写到 inode，供 xattr 查询
        for e in self.ino_by_rel.iter() {
            if let Some(inode) = self.inodes.get(&*e.value()) {
                let mut node = inode.write();
                node.pinned = self.pins.contains_key(&*e.key());
            }
        }
    }

    /// 设置/取消「始终保留在此电脑」，并持久化。
    pub fn set_pin(&self, rel: &str, pinned: bool) -> Result<()> {
        let _pin_guard = self.pin_lock.lock();
        if pinned {
            self.pins.insert(rel.to_string(), ());
        } else {
            self.pins.remove(rel);
        }
        if let Some(ino) = self.ino_by_rel.get(rel).map(|v| *v) {
            if let Some(inode) = self.inodes.get(&ino) {
                inode.write().pinned = pinned;
            }
        }
        let list: Vec<String> = self.pins.iter().map(|e| e.key().clone()).collect();
        let tmp = self.cache.root().join("pins.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&list)?)?;
        std::fs::rename(&tmp, self.pins_path())?;
        if let Some(ino) = self.ino_by_rel.get(rel).map(|v| *v) {
            self.notify_inode(ino);
        }
        Ok(())
    }

    /// 释放本地内容（对应官方「释放空间」/ `free_space` / `evictCache`）。
    ///
    /// 语义：删除缓存副本，文件回到占位状态；远端内容不受影响。
    /// 有未上传修改或已设为「始终保留」时拒绝，避免丢数据或违背用户意图。
    pub fn release(&self, rel: &str) -> Result<()> {
        let lock = self.cache.lock_for(rel);
        let _guard = lock
            .try_lock()
            .map_err(|_| anyhow!("文件正在传输或修改，稍后再试"))?;
        let _pin_guard = self.pin_lock.lock();
        let _open_guard = self.open_gate.lock();
        if self.transfers.contains_key(rel) {
            anyhow::bail!("文件正在传输，稍后再试");
        }
        if self.is_dirty(rel) {
            anyhow::bail!("文件有未上传的本地修改，不能释放");
        }
        if self.pins.contains_key(rel) {
            anyhow::bail!("已设为“始终保留在此电脑”，请先取消保留");
        }
        if self.is_in_use(rel) {
            anyhow::bail!("文件正在被使用，稍后再试");
        }
        let path = self.cache.file_path(rel);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        // 清空元数据缓存里的内容信息，状态随之回到 placeholder
        if let Some(mut st) = self.cache.state(rel) {
            st.size = 0;
            st.dirty = false;
            st.uploading = false;
            self.cache.set_state(rel, st);
        }
        self.cache.remove(rel);
        self.range_cache
            .lock()
            .retain(|(path, _, _), _| path != rel);
        self.errors.remove(rel);
        if let Some(ino) = self.ino_by_rel.get(rel).map(|v| *v) {
            self.notify_inode(ino);
        }
        tracing::info!(rel, "on-demand content released (freed disk space)");
        Ok(())
    }

    fn is_pinned(&self, rel: &str) -> bool {
        self.pins.contains_key(rel)
    }

    /// 文件当前的按需同步状态：`dirty`（本地有未上传修改）、
    /// `cached`（已下载）、`placeholder`（仅有占位，内容不占本地磁盘）。
    pub fn file_state(&self, node: &Inode) -> &'static str {
        if node.kind == FileType::Directory {
            return "directory";
        }
        // 传输中优先展示：这是用户最需要看到的即时反馈
        if let Some(t) = self.transfers.get(&node.rel) {
            return t.kind; // "download" | "upload"
        }
        if self.queued_downloads.contains_key(&node.rel) {
            return "queued";
        }
        if node.dirty {
            return "dirty";
        }
        if self.cached_content_ok(&node.rel, node.size) {
            "cached"
        } else {
            "placeholder"
        }
    }

    /// 查某个 inode 的扩展属性值；未知属性返回 None。
    fn xattr_value(&self, ino: u64, name: &str) -> Option<String> {
        let inode = self.inodes.get(&ino)?;
        let node = inode.read();
        match name {
            XATTR_STATE => Some(self.file_state(&node).to_string()),
            XATTR_PINNED => Some(if node.pinned { "1" } else { "0" }.to_string()),
            XATTR_PROGRESS => Some(
                self.transfers
                    .get(&node.rel)
                    .map(|t| t.percent())
                    .unwrap_or(0)
                    .to_string(),
            ),
            XATTR_REMOTE_SIZE => Some(node.size.to_string()),
            XATTR_LOCAL_SIZE => {
                let local = self
                    .cache
                    .state(&node.rel)
                    .map(|s| s.size)
                    .filter(|_| self.cached_content_ok(&node.rel, node.size))
                    .unwrap_or(0);
                Some(local.to_string())
            }
            _ => None,
        }
    }

    /// 缓存内容是否**确实**在磁盘上且完整（大小等于远端大小）。
    ///
    /// 绝不能只看内存里的 `cache.state`：淘汰或外部清理会让内存状态与磁盘不一致。
    /// 此时若仍当作“已缓存”，`open_handle` 会用 `create(true)` 凭空造出空文件，
    /// 读取返回 0 字节且不报错 —— 用户看到“文件变空了”，属于静默数据损坏。
    fn cached_content_ok(&self, rel: &str, expect_size: u64) -> bool {
        // 只看磁盘上的真实内容，**不依赖内存状态**：
        // 重启后 load_meta 只恢复脏文件，干净缓存的内存状态会丢失；
        // 若因此判断为“未缓存”，du 会误报 0 块且每次读取都重新下载。
        match std::fs::metadata(self.cache.file_path(rel)) {
            Ok(meta) => meta.len() == expect_size,
            Err(_) => false,
        }
    }

    /// Serialize resize with hydration/upload; preserve old bytes for nonzero truncation.
    pub async fn resize_content(&self, ino: u64, size: u64) -> Result<()> {
        if size > 0 {
            self.hydrate(ino).await?;
        }
        let rel = self
            .inodes
            .get(&ino)
            .ok_or_else(|| anyhow!("no inode"))?
            .read()
            .rel
            .clone();
        let lock = self.cache.lock_for(&rel);
        let _guard = tokio::time::timeout(Duration::from_secs(30), lock.lock())
            .await
            .map_err(|_| anyhow!("调整文件大小等待超时"))?;
        if self.stopping.load(Ordering::Relaxed) {
            anyhow::bail!("挂载正在停止");
        }
        let file = self.open_handle(ino)?;
        file.lock().set_len(size)?;
        let now = now_secs();
        let stamp = self
            .inodes
            .get(&ino)
            .ok_or_else(|| anyhow!("no inode"))?
            .read()
            .stamp;
        self.cache.ensure_state(&rel, stamp, now);
        self.cache.update(&rel, |s| {
            s.size = size;
            s.dirty = true;
            s.mtime = now;
        });
        if let Some(inode) = self.inodes.get(&ino) {
            let mut node = inode.write();
            node.dirty = true;
            node.size = size;
            node.mtime = now;
        }
        self.mark_meta_dirty();
        self.notify_inode(ino);
        Ok(())
    }

    /// Read clean large placeholders in bounded ranges. File manager MIME probes must
    /// never materialize multi-GB files after the user explicitly freed local content.
    /// Ranges are only a bounded memory cache and do not change the downloaded state.
    pub async fn read_placeholder(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        let rel = self
            .inodes
            .get(&ino)
            .ok_or_else(|| anyhow!("no inode"))?
            .read()
            .rel
            .clone();
        let lock = self.cache.lock_for(&rel);
        // Serialize with resize/write/upload, but never wait a full ISO hydration.
        let _guard = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(1), lock.lock()) =>
                result.map_err(|_| anyhow!(std::io::Error::from_raw_os_error(libc::EAGAIN)))?,
            _ = self.wait_stopping() => anyhow::bail!("挂载正在停止"),
        };
        let node = self
            .inodes
            .get(&ino)
            .ok_or_else(|| anyhow!("no inode"))?
            .read()
            .clone();
        if node.dirty {
            anyhow::bail!("文件正在修改，请重试");
        }
        let length = (size as u64).min(node.size.saturating_sub(offset)) as usize;
        if length == 0 {
            return Ok(Vec::new());
        }
        let mut result = Vec::with_capacity(length);
        let mut cursor = offset;
        while result.len() < length {
            let block = cursor / RANGE_BLOCK_BYTES as u64 * RANGE_BLOCK_BYTES as u64;
            let key = (node.rel.clone(), block, node.stamp);
            let cached = self.range_cache.lock().get(&key).cloned();
            let data = if let Some(data) = cached {
                data
            } else {
                let _permit = tokio::select! {
                    p = self.range_slots.acquire() => p.map_err(|_|anyhow!("分段读取队列关闭"))?,
                    _ = self.wait_stopping() => anyhow::bail!("挂载正在停止"),
                };
                let count = RANGE_BLOCK_BYTES.min(node.size.saturating_sub(block) as usize);
                let remote = self.remote_path(&node.rel);
                let bytes = tokio::select! {
                    r = self.nas.read_range(&remote, block, count) => r?,
                    _ = self.wait_stopping() => anyhow::bail!("挂载正在停止"),
                };
                if bytes.len() != count {
                    anyhow::bail!("远端文件大小已变化，请刷新后重试");
                }
                let bytes = Arc::new(bytes);
                let mut cache = self.range_cache.lock();
                if cache.len() >= MAX_RANGE_BLOCKS {
                    cache.clear();
                }
                cache.insert(key, bytes.clone());
                bytes
            };
            let start = (cursor - block) as usize;
            let count = (length - result.len()).min(data.len().saturating_sub(start));
            if count == 0 {
                anyhow::bail!("远端分段内容不完整");
            }
            result.extend_from_slice(&data[start..start + count]);
            cursor += count as u64;
        }
        Ok(result)
    }

    /// 确保内容已缓存（等价官方 DemandWriter）：下载远端内容到缓存。
    pub async fn hydrate(&self, ino: u64) -> Result<()> {
        let rel = {
            let inode = self.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
            let node = inode.read();
            node.rel.clone()
        };
        let lock = self.cache.lock_for(&rel);
        // Same-file readers await one hydration asynchronously; stop wakes all waiters.
        let _guard = tokio::select! {
            guard = lock.lock() => guard,
            _ = self.wait_stopping() => anyhow::bail!("挂载正在停止"),
        };
        if self.stopping.load(Ordering::Relaxed) {
            anyhow::bail!("挂载正在停止");
        }
        self.queued_downloads.remove(&rel);
        self.errors.remove(&rel);
        // 双检：可能已被并发下载或已变脏
        {
            let inode = self.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
            let node = inode.read();
            if node.dirty {
                return Ok(());
            }
            // 只要磁盘上已有完整内容就无需下载。
            // 不依赖 cache.state：重启后干净缓存的内存状态会丢失，
            // 若因此重新下载，等于每次重启都白下所有文件。
            // 过期缓存在 resolve_child 里已按远端戳清理，这里不会读到旧内容。
            if self.cached_content_ok(&rel, node.size) {
                self.cache.touch(&rel);
                return Ok(());
            }
        }
        self.queued_downloads.insert(rel.clone(), ());
        let _download_slot = tokio::select! {
            permit = self.download_slots.acquire() => permit.map_err(|_| anyhow!("下载队列关闭"))?,
            _ = self.wait_stopping() => anyhow::bail!("挂载正在停止"),
        };
        self.queued_downloads.remove(&rel);
        let stamp = {
            let inode = self.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
            let node = inode.read();
            node.stamp
        };
        let remote = self.remote_path(&rel);
        // 标记“下载中”，让状态列/CLI 立即可见（官方是推 shell 进度条）
        self.transfers
            .insert(rel.clone(), Transfer::new("download", stamp.0));
        // 立刻通知一次：状态列要马上从“未下载”变成“下载中”
        self.notify_inode(ino);
        // 流式下载到暂存文件：边下边写，不把整个文件读进内存，
        // 同时把已写入字节数实时反馈到 transfers，供进度显示查询。
        let tmp = self.cache.tmp_path();
        if let Some(parent) = tmp.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // 进度计数器由 Transfer 共享给下载闭包，实现实时进度
        let done_counter = self
            .transfers
            .get(&rel)
            .map(|t| t.done.clone())
            .unwrap_or_else(|| Arc::new(AtomicU64::new(0)));
        let progress: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(move |done| {
            done_counter.store(done, Ordering::Relaxed);
        });
        let downloaded = tokio::select! {
            result = self.nas.download_to(&remote, &tmp, progress) => result,
            _ = self.wait_stopping() => Err(anyhow!("挂载正在停止")),
        };
        self.transfers.remove(&rel);
        let written = downloaded.map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            self.errors.insert(rel.clone(), e.to_string());
            self.notify_inode(ino);
            tracing::warn!(rel = %rel, error = %e, "on-demand download failed");
            e
        })?;
        let target = self.cache.file_path(&rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&tmp, &target)?;
        {
            let inode = self.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
            let mut node = inode.write();
            if !node.dirty {
                node.size = stamp.0;
            }
        }
        self.cache.set_state(
            &rel,
            CacheState {
                size: written,
                dirty: false,
                uploading: false,
                stamp,
                mtime: stamp.1,
                last_access_ms: self.cache.now_ms(),
            },
        );
        self.add_log(logs::ACTION_DOWNLOAD, &rel, false).await;
        // 通知内核：文件状态已变（占位 → 已下载），让文件管理器立即刷新
        self.notify_inode(ino);
        let parent_rel = Self::parent_rel(&rel);
        if let Some(parent_ino) = self.ino_by_rel.get(parent_rel).map(|v| *v) {
            self.notify_entry(parent_ino, Self::file_name(&rel));
        }
        Ok(())
    }

    /// 上传单个脏文件（worker 与 fsync 共用）。远端已变化 → Conflicted Copy 保双方。
    async fn upload_ino(&self, ino: u64) -> Result<()> {
        let rel = {
            let inode = self.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
            let node = inode.read();
            node.rel.clone()
        };
        // 回收站等本地保留名绝不上传（含 .trashinfo 元数据文件）
        if Self::is_local_only(&rel) {
            return Ok(());
        }
        let lock = self.cache.lock_for(&rel);
        // 同一文件的上传可能被 flush / worker / fsync 并发触发；
        // 若前一次上传卡在网络上，后续请求不能无限排队（会让 close/fsync 挂住）。
        let _guard = match tokio::time::timeout(Duration::from_secs(30), lock.lock()).await {
            Ok(g) => g,
            Err(_) => {
                tracing::warn!(rel = %rel, "upload lock wait timed out");
                return Err(anyhow!("上传锁等待超时，文件仍有未上传修改"));
            }
        };
        let Some(state) = self.cache.state(&rel) else {
            return Ok(());
        };
        if !state.dirty || state.uploading {
            return Ok(());
        }
        self.cache.update(&rel, |s| s.uploading = true);
        let total = std::fs::metadata(self.cache.file_path(&rel))
            .map(|m| m.len())
            .unwrap_or(0);
        self.transfers
            .insert(rel.clone(), Transfer::new("upload", total));

        // 冲突检测基准 = 本地内容对应的远端戳（下载/上次上传时记录），
        // 而非 inode 上随时被轮询刷新的观察戳。
        let upload = tokio::select! {
            result = self.do_upload(&rel, &state.stamp) => result,
            _ = self.wait_stopping() => Err(anyhow!("挂载正在停止")),
        };
        match upload {
            Ok(new_stamp) => {
                self.errors.remove(&rel);
                self.transfers.remove(&rel);
                let size = self.cache.state(&rel).map(|s| s.size).unwrap_or(0);
                self.cache.update(&rel, |s| {
                    s.dirty = false;
                    s.uploading = false;
                    s.stamp = new_stamp;
                });
                // 上传完成即落盘（脏状态已消除，必须立刻持久化）
                self.cache.persist_meta();
                {
                    let inode = self.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
                    let mut node = inode.write();
                    node.dirty = false;
                    node.created = false;
                    node.stamp = new_stamp;
                    node.size = size;
                }
                // 本地新建 → overlay 移除
                let parent_rel = Self::parent_rel(&rel);
                let parent_ino = self
                    .ino_by_rel
                    .get(parent_rel)
                    .map(|v| *v)
                    .unwrap_or(ROOT_INO);
                if let Some(ov) = self.overlays.get(&parent_ino) {
                    ov.write().created.remove(Self::file_name(&rel));
                }
                self.add_log(logs::ACTION_UPLOAD, &rel, false).await;
                self.notify_inode(ino);
                let parent_rel = Self::parent_rel(&rel);
                if let Some(parent_ino) = self.ino_by_rel.get(parent_rel).map(|v| *v) {
                    self.notify_entry(parent_ino, Self::file_name(&rel));
                }
                Ok(())
            }
            Err(e) => {
                self.transfers.remove(&rel);
                self.cache.update(&rel, |s| s.uploading = false);
                self.errors.insert(rel.clone(), e.to_string());
                self.notify_inode(ino);
                Err(e)
            }
        }
    }

    async fn do_upload(&self, rel: &str, stamp: &(u64, i64)) -> Result<(u64, i64)> {
        let path = self.cache.file_path(rel);
        let bytes = std::fs::read(&path)?;
        let mtime = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // 远端冲突检测：远端存在且戳与本地记录不一致 → 把远端版本存为 Conflicted Copy
        let remote = self.remote_path(rel);
        let parent_rel = Self::parent_rel(rel);
        let name = Self::file_name(rel);
        let parent_path = self.remote_path(parent_rel);
        let entries = self.nas.ls(&parent_path).await?;
        if let Some(cur) = entries.iter().find(|e| e.name == name && !e.is_dir()) {
            let cur_stamp = (cur.size, cur.mtim);
            if cur_stamp != *stamp {
                let conflict_name = format!(
                    "{} - Conflicted Copy {}",
                    name,
                    chrono::Local::now().format("%Y-%m-%d %H-%M-%S")
                );
                let conflict_remote = format!("{parent_path}/{conflict_name}");
                let cur_bytes = self.nas.download(&remote).await?;
                {
                    tracing::info!(
                        rel = %rel,
                        "remote changed during edit; keeping conflicted copy {conflict_name}"
                    );
                    self.nas
                        .upload(&conflict_remote, cur_bytes, Some(cur.mtim))
                        .await?;
                }
            }
        }
        self.nas.upload(&remote, bytes.clone(), Some(mtime)).await?;
        // 用服务端真实回读戳覆盖，避免时间戳取整导致误判冲突
        let entries = self.nas.ls(&parent_path).await?;
        let new_stamp = entries
            .iter()
            .find(|e| e.name == name && !e.is_dir())
            .map(|e| (e.size, e.mtim))
            .unwrap_or((bytes.len() as u64, mtime));
        Ok(new_stamp)
    }

    async fn add_log(&self, action_code: i64, rel: &str, is_dir: bool) {
        let _ = logs::add_log(
            &self.db,
            logs::NewLog {
                task_id: self.cfg.task_id,
                is_dir,
                name: Self::file_name(rel),
                path: rel,
                sync_folder: &self.cfg.task_name,
                action_code,
                status: logs::STATUS_SUCCESS,
                failed_code: 0,
                failed_text: None,
                payload: None,
            },
        )
        .await;
    }

    /// 删除 NAS 上残留的本地保留名目录（例如旧版本误上传的 .Trash-1000）。
    async fn purge_remote_reserved(&self) -> Result<()> {
        let path = self.remote_path("");
        let entries = self.nas.ls(&path).await?;
        for e in entries {
            if Self::is_local_only(&e.name) {
                let remote = format!("{}/{}", self.cfg.nas_root, e.name);
                tracing::warn!(name = %e.name, "removing reserved name leaked to NAS");
                let _ = self.nas.remove(&[remote]).await;
            }
        }
        Ok(())
    }

    fn invalidate_parent_listing(&self, parent: u64) {
        self.dir_cache.remove(&parent);
    }

    // ---------- worker ----------

    /// 后台 worker：补传脏文件、执行删除、root 远端差异推送、缓存淘汰。
    pub async fn worker(self: Arc<Self>) {
        let mut tick: u32 = 0;
        loop {
            if self.stopping.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            if self.stopping.load(Ordering::Relaxed) {
                break;
            }
            tick += 1;
            // 只做一次：清理历史上被误上传到 NAS 的回收站目录
            if tick == 1 {
                if let Err(e) = self.purge_remote_reserved().await {
                    tracing::debug!(error = %e, "purge remote reserved skipped");
                }
            }
            self.flush_meta();
            self.flush_dirty_uploads();
            self.run_pending_removes().await;
            if tick % ROOT_POLL_TICKS == 0 {
                self.poll_all_diffs().await;
            }
            if tick % EVICT_TICKS == 0 {
                self.evict();
            }
        }
    }

    fn dirty_inos(&self) -> Vec<(u64, String)> {
        let mut out: HashMap<u64, String> = self
            .inodes
            .iter()
            .filter(|e| e.value().read().dirty)
            .map(|e| {
                let node = e.value().read();
                (*e.key(), node.rel.clone())
            })
            .collect();
        // 重启恢复的脏文件（尚未 lookup）也纳入补传
        for entry in self.cache.dirty_entries() {
            if let Some(ino) = self.ino_by_rel.get(&entry).map(|v| *v) {
                out.insert(ino, entry);
            }
        }
        out.into_iter().collect()
    }

    fn flush_dirty_uploads(self: &Arc<Self>) {
        for (ino, rel) in self.dirty_inos() {
            let Some(state) = self.cache.state(&rel) else {
                continue;
            };
            if state.dirty && !state.uploading {
                let shared = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = shared.upload_ino(ino).await {
                        tracing::debug!(rel = %rel, error = %e, "upload retry failed");
                    }
                });
            }
        }
    }

    async fn run_pending_removes(&self) {
        let items: Vec<(String, u32)> = self
            .pending_removes
            .iter()
            .map(|e| (e.key().clone(), *e.value()))
            .collect();
        for (rel, tries) in items {
            if tries > 5 {
                tracing::warn!(rel = %rel, "remote remove retry limit reached");
                self.pending_removes.remove(&rel);
                continue;
            }
            let remote = self.remote_path(&rel);
            let paths = vec![remote.clone()];
            match self.nas.remove(&paths).await {
                Ok(()) => {
                    self.pending_removes.remove(&rel);
                    let parent_rel = Self::parent_rel(&rel);
                    let parent_ino = self
                        .ino_by_rel
                        .get(parent_rel)
                        .map(|v| *v)
                        .unwrap_or(ROOT_INO);
                    let name = Self::file_name(&rel);
                    if let Some(ov) = self.overlays.get(&parent_ino) {
                        let mut ov = ov.write();
                        ov.deleted.remove(name);
                        ov.created.remove(name);
                    }
                    self.drop_inode(&rel);
                    self.add_log(logs::ACTION_DELETE, &rel, false).await;
                }
                Err(e) => {
                    self.pending_removes.insert(rel.clone(), tries + 1);
                    tracing::debug!(rel = %rel, error = %e, "remote remove retry");
                }
            }
        }
    }

    /// 轮询所有已缓存目录的远端差异（不只根目录），更新视图并发失效通知。
    async fn poll_all_diffs(&self) {
        // 收集当前已缓存的目录 inode（避免持守卫遍历时再插入）
        let dirs: Vec<(u64, String)> = self
            .dir_cache
            .iter()
            .filter_map(|e| {
                let ino = *e.key();
                let rel = self
                    .inodes
                    .get(&ino)
                    .map(|n| n.read().rel.clone())
                    .unwrap_or_default();
                Some((ino, rel))
            })
            .collect();
        // 并发轮询（每轮上限 MAX_DIRS_PER_POLL 个）：缓存目录可能很多，
        // 串行发 ls 会把一轮拉长到几十秒，期间 worker 无法处理删除队列与上传。
        let batch: Vec<(u64, String)> = dirs.into_iter().take(MAX_DIRS_PER_POLL).collect();
        let polls: Vec<_> = batch
            .into_iter()
            .map(|(ino, rel)| async move {
                // 单个目录最多等 8 秒；一个慢/失效目录不能拖住整轮。
                match tokio::time::timeout(Duration::from_secs(8), self.poll_dir_diff(ino, &rel))
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::debug!(dir = %rel, error = %e, "dir diff poll skipped"),
                    Err(_) => tracing::debug!(dir = %rel, "dir diff poll timed out"),
                }
            })
            .collect();
        futures_util::future::join_all(polls).await;
    }

    /// 单个目录的远端差异 → 更新本地视图 + 内核失效通知。
    async fn poll_dir_diff(&self, ino: u64, rel: &str) -> Result<()> {
        if Self::is_local_only(rel) {
            return Ok(());
        }
        let fresh = self.fetch_remote_list(rel).await?;
        // 必须在独立作用域里取出旧列表并收集成 BTreeMap 再释放守卫：
        // `DashMap::get` 返回读守卫，若留到函数末尾再 insert 同一分片，
        // 就是「持读锁再去拿写锁」→ 自死锁，会冻结整棵挂载树。
        let previous: BTreeMap<String, RemoteListEntry>;
        {
            let Some(list) = self.dir_cache.get(&ino) else {
                return Ok(());
            };
            previous = list
                .read()
                .entries
                .iter()
                .map(|e| (e.name.clone(), e.clone()))
                .collect();
        }
        for entry in &fresh {
            let changed = match previous.get(&entry.name) {
                Some(old) => {
                    old.dir != entry.dir || old.size != entry.size || old.mtim != entry.mtim
                }
                None => true,
            };
            if !changed {
                continue;
            }
            let child_rel = if rel.is_empty() {
                entry.name.clone()
            } else {
                format!("{rel}/{}", entry.name)
            };
            if let Some(child) = self.ino_by_rel.get(&child_rel).map(|v| *v) {
                if let Some(inode) = self.inodes.get(&child) {
                    let mut node = inode.write();
                    if !node.dirty {
                        node.size = entry.size;
                        node.mtime = entry.mtim;
                    }
                    node.stamp = (entry.size, entry.mtim);
                }
                // 通知走专用线程，不会阻塞 runtime
                self.notify_inode(child);
            }
            self.notify_entry(ino, &entry.name);
        }
        let fresh_names: HashSet<&str> = fresh.iter().map(|e| e.name.as_str()).collect();
        for name in previous.keys() {
            if !fresh_names.contains(name.as_str()) {
                tracing::debug!(dir = %rel, name, "remote entry removed");
                self.notify_entry(ino, name);
            }
        }
        self.dir_cache.insert(
            ino,
            RwLock::new(DirList {
                entries: fresh,
                fetched: Instant::now(),
            }),
        );
        Ok(())
    }

    async fn wait_stopping(&self) {
        while !self.stopping.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn evict(&self) {
        for rel in self
            .cache
            .plan_eviction(&|rel| self.is_pinned(rel), &|rel| self.is_in_use(rel))
        {
            // Selection is advisory; recheck under the same lock as downloads/uploads.
            if let Err(error) = self.release(&rel) {
                tracing::debug!(%rel, %error, "cache eviction deferred");
            }
        }
    }

    #[allow(dead_code)]
    pub fn mounted_files_count(&self) -> usize {
        self.cache.count()
    }
}

pub struct OnDemandFs {
    pub shared: Arc<Shared>,
}

impl OnDemandFs {
    fn inode_attr(&self, ino: u64) -> Result<FileAttr> {
        let inode = self
            .shared
            .inodes
            .get(&ino)
            .ok_or_else(|| anyhow!("no inode {ino}"))?;
        let node = inode.read().clone();
        Ok(self.shared.attr_for(ino, &node))
    }
}

fn secs_of(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_secs() -> i64 {
    secs_of(SystemTime::now())
}

async fn apply_write(shared: &Shared, ino: u64, off: u64, data: &[u8]) -> Result<()> {
    let rel = {
        let inode = shared.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
        let node = inode.read();
        node.rel.clone()
    };
    let lock = shared.cache.lock_for(&rel);
    let _guard = tokio::time::timeout(Duration::from_secs(30), lock.lock())
        .await
        .map_err(|_| anyhow!("写入等待超时"))?;
    if shared.stopping.load(Ordering::Relaxed) {
        anyhow::bail!("挂载正在停止");
    }
    let file = shared.open_handle(ino)?;
    {
        let f = file.lock();
        f.write_at(data, off)?;
    }
    let existing = shared
        .cache
        .state(&rel)
        .map(|s| s.size)
        .unwrap_or_else(|| shared.inodes.get(&ino).map(|n| n.read().size).unwrap_or(0));
    let new_size = std::cmp::max(off + data.len() as u64, existing);
    let now = now_secs();
    // 写未缓存的占位文件时先补状态记录（内容不对应任何远端版本 → stamp (0,0)）
    shared.cache.ensure_state(&rel, (0, 0), now);
    shared.cache.update(&rel, |s| {
        s.size = new_size;
        s.dirty = true;
        s.mtime = now;
    });
    if let Some(inode) = shared.inodes.get(&ino) {
        let mut node = inode.write();
        node.dirty = true;
        node.size = new_size;
        node.mtime = now;
    }
    shared.mark_meta_dirty();
    Ok(())
}

impl Filesystem for OnDemandFs {
    fn init(&mut self, _req: &Request<'_>, _config: &mut KernelConfig) -> Result<(), libc::c_int> {
        Ok(())
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let shared = self.shared.clone();
        let name_owned = name.to_os_string();
        // lookup 未命中会走远端 ls —— 必须在池里跑，不能阻塞派发线程
        let _ = shared.ops.clone().submit(
            "lookup",
            Box::new(move || {
                match shared
                    .rt
                    .block_on(shared.resolve_child(parent, &name_owned))
                {
                    Ok(Some(ino)) => match Self::attr_of(&shared, ino) {
                        Ok(attr) => reply.entry(&ATTR_TTL, &attr, 0),
                        Err(_) => reply.error(libc::ENOENT),
                    },
                    Ok(None) => reply.error(libc::ENOENT),
                    Err(_) => reply.error(libc::EIO),
                }
            }),
        );
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let attr = self.inode_attr(ino);
        match attr {
            Ok(attr) => reply.attr(&ATTR_TTL, &attr),
            Err(_) => reply.error(libc::ENOENT),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let shared = self.shared.clone();
        // 尺寸变更可能触发 hydrate；交给池线程执行，避免卡派发循环
        let _ = shared.transfer_ops.clone().submit(
            "setattr",
            Box::new(move || {
                let result: Result<()> = shared.rt.block_on(async {
                    let kind = {
                        let inode = shared.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
                        let node = inode.read();
                        node.kind
                    };
                    // chmod：目录也必须支持（回收站要求 .Trash 为 1777 粘滞目录）
                    if let Some(m) = mode {
                        if let Some(inode) = shared.inodes.get(&ino) {
                            inode.write().mode = m & 0o7777;
                        }
                    }
                    if kind != FileType::RegularFile {
                        if let Some(TimeOrNow::SpecificTime(t)) = mtime {
                            if let Some(inode) = shared.inodes.get(&ino) {
                                inode.write().mtime = secs_of(t);
                            }
                        }
                        return Ok(());
                    }
                    if let Some(new_size) = size {
                        shared.resize_content(ino, new_size).await?;
                    } else if let Some(t) = mtime {
                        let secs = match t {
                            TimeOrNow::SpecificTime(t) => secs_of(t),
                            TimeOrNow::Now => now_secs(),
                        };
                        if let Some(inode) = shared.inodes.get(&ino) {
                            let mut node = inode.write();
                            let rel = node.rel.clone();
                            if node.dirty {
                                node.mtime = secs;
                                shared.cache.update(&rel, |s| s.mtime = secs);
                            } else {
                                node.mtime = secs;
                            }
                        }
                    }
                    Ok(())
                });
                match result {
                    Ok(()) => match Self::attr_of(&shared, ino) {
                        Ok(attr) => reply.attr(&ATTR_TTL, &attr),
                        Err(_) => reply.error(libc::ENOENT),
                    },
                    Err(_) => reply.error(libc::EIO),
                }
            }),
        );
    }

    fn mknod(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        self.do_create(parent, name, reply);
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let shared = self.shared.clone();
        let name = name.to_os_string();
        let _ = shared.ops.clone().submit(
            "mkdir",
            Box::new(
                move || match shared.rt.block_on(shared.resolve_new_file(parent, &name)) {
                    Ok(ino) => {
                        shared.inc_open(ino);
                        if shared.open_handle(ino).is_err() {
                            shared.dec_open(ino);
                            reply.error(libc::EIO);
                            return;
                        }
                        match Self::attr_of(&shared, ino) {
                            Ok(attr) => reply.created(&ATTR_TTL, &attr, 0, ino, 0),
                            Err(_) => reply.error(libc::EIO),
                        }
                    }
                    Err(_) => reply.error(libc::EIO),
                },
            ),
        );
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let shared = self.shared.clone();
        let name = name.to_os_string();
        let _ = shared.ops.clone().submit(
            "unlink",
            Box::new(
                move || match shared.rt.block_on(shared.do_mkdir(parent, &name)) {
                    Ok(ino) => match Self::attr_of(&shared, ino) {
                        Ok(attr) => reply.entry(&ATTR_TTL, &attr, 0),
                        Err(_) => reply.error(libc::EIO),
                    },
                    Err(_) => reply.error(libc::EIO),
                },
            ),
        );
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let shared = self.shared.clone();
        let name = name.to_os_string();
        let _ = shared.ops.clone().submit(
            "rmdir",
            Box::new(
                move || match shared.rt.block_on(shared.do_remove(parent, &name)) {
                    Ok(()) => reply.ok(),
                    Err(_) => reply.error(libc::EIO),
                },
            ),
        );
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let shared = self.shared.clone();
        let name = name.to_os_string();
        let _ = shared.ops.clone().submit(
            "rename",
            Box::new(
                move || match shared.rt.block_on(shared.do_remove(parent, &name)) {
                    Ok(()) => reply.ok(),
                    Err(_) => reply.error(libc::EIO),
                },
            ),
        );
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let shared = self.shared.clone();
        let name = name.to_os_string();
        let newname = newname.to_os_string();
        let _ = shared.ops.clone().submit(
            "open",
            Box::new(move || {
                match shared
                    .rt
                    .block_on(shared.do_rename(parent, &name, newparent, &newname))
                {
                    Ok(()) => reply.ok(),
                    Err(e) if e.to_string().contains("cross-device") => reply.error(libc::EXDEV),
                    Err(_) => reply.error(libc::EIO),
                }
            }),
        );
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let write = flags & libc::O_WRONLY != 0 || flags & libc::O_RDWR != 0;
        let truncate = flags & libc::O_TRUNC != 0;
        let shared = self.shared.clone();
        if !write && !truncate {
            shared.inc_open(ino);
            let direct = shared
                .inodes
                .get(&ino)
                .map(|inode| {
                    let node = inode.read();
                    node.size >= RANGE_THRESHOLD
                        && !node.dirty
                        && !shared.cached_content_ok(&node.rel, node.size)
                })
                .unwrap_or(false);
            reply.opened(
                ino,
                if direct {
                    fuser::consts::FOPEN_DIRECT_IO
                } else {
                    0
                },
            );
            return;
        }
        shared.inc_open(ino);
        // 写打开未缓存文件：先 hydrate 再回复（池线程，绝不占用派发线程）
        let cleanup = shared.clone();
        let accepted = shared.transfer_ops.clone().submit(
            "open-write",
            Box::new(move || {
                let result: Result<()> = shared.rt.block_on(async {
                    let (rel, dirty, size) = {
                        let inode = shared.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
                        let node = inode.read();
                        (node.rel.clone(), node.dirty, node.size)
                    };
                    if truncate {
                        shared.resize_content(ino, 0).await?;
                        return Ok(());
                    }
                    if !dirty && size > 0 && !shared.cache.downloaded(&rel) {
                        shared.hydrate(ino).await?;
                    }
                    let _ = shared.open_handle(ino)?;
                    Ok(())
                });
                match result {
                    Ok(()) => reply.opened(ino, 0),
                    Err(_) => {
                        shared.dec_open(ino);
                        reply.error(libc::EIO)
                    }
                }
            }),
        );
        if !accepted {
            cleanup.dec_open(ino);
        }
    }

    fn read(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let shared = self.shared.clone();
        let rel = match shared.inodes.get(&ino) {
            Some(inode) => inode.read().rel.clone(),
            None => return reply.error(libc::ENOENT),
        };
        // 快路径：已缓存且未脏且戳一致 → 直接读
        // 快路径要求“状态匹配 **且** 内容确实在磁盘上”。缺了后者，一旦缓存文件
        // 被清掉而状态还在，open_handle 的 create(true) 会造出空文件并返回 0 字节。
        let fast = match shared.cache.state(&rel) {
            Some(state) => {
                !state.dirty
                    && shared
                        .inodes
                        .get(&ino)
                        .map(|n| {
                            let node = n.read();
                            node.stamp == state.stamp && node.size == state.size
                        })
                        .unwrap_or(false)
                    && shared.cached_content_ok(
                        &rel,
                        shared.inodes.get(&ino).map(|n| n.read().size).unwrap_or(0),
                    )
            }
            None => false,
        };
        if fast {
            let Ok(file) = shared.open_handle(ino) else {
                return reply.error(libc::EIO);
            };
            shared.cache.touch(&rel);
            let f = file.lock();
            let mut buf = vec![0u8; size as usize];
            match f.read_at(&mut buf, offset.max(0) as u64) {
                Ok(n) => reply.data(&buf[..n]),
                Err(_) => reply.error(libc::EIO),
            }
            return;
        }
        let ranged = shared
            .inodes
            .get(&ino)
            .map(|inode| {
                let node = inode.read();
                node.size >= RANGE_THRESHOLD
                    && !node.dirty
                    && !shared.cached_content_ok(&node.rel, node.size)
            })
            .unwrap_or(false);
        let pid = req.pid();
        // Waiting for the same file never occupies an OS worker thread.
        let Ok(slot) = shared.read_slots.clone().try_acquire_owned() else {
            reply.error(libc::EAGAIN);
            return;
        };
        let rt = shared.rt.clone();
        rt.spawn(async move {
            let _slot = slot;
            let result = async {
                if ranged {
                    if offset < 65536 {
                        tracing::info!(
                            pid,
                            ino,
                            offset,
                            size,
                            "large placeholder range read (no full download)"
                        );
                    }
                    return shared
                        .read_placeholder(ino, offset.max(0) as u64, size)
                        .await;
                }
                tracing::info!(
                    pid,
                    ino,
                    offset,
                    size,
                    "placeholder hydration triggered by file read"
                );
                shared.hydrate(ino).await?;
                let file = shared.open_handle(ino)?;
                let mut buf = vec![0u8; size as usize];
                let n = file.lock().read_at(&mut buf, offset.max(0) as u64)?;
                Ok::<Vec<u8>, anyhow::Error>(buf[..n].to_vec())
            }
            .await;
            match result {
                Ok(bytes) => reply.data(&bytes),
                Err(error) => {
                    tracing::warn!(pid, ino, offset, size, %error, "file read failed");
                    let code = error
                        .downcast_ref::<std::io::Error>()
                        .and_then(|e| e.raw_os_error())
                        .unwrap_or(libc::EIO);
                    reply.error(code);
                }
            }
        });
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let shared = self.shared.clone();
        let data = data.to_vec();
        let off = offset.max(0) as u64;
        // 未缓存且远端有内容且从非零偏移写 → 必须先下载（否则会破坏未写区域）
        let need_hydrate = match shared.inodes.get(&ino) {
            Some(inode) => {
                let node = inode.read();
                !node.dirty && node.size > 0 && off > 0 && !shared.cache.downloaded(&node.rel)
            }
            None => return reply.error(libc::ENOENT),
        };
        let data_len = data.len() as u32;
        if need_hydrate {
            let _ = shared.transfer_ops.clone().submit(
                "write-hydrate",
                Box::new(move || {
                    let result = shared.rt.block_on(async {
                        shared.hydrate(ino).await?;
                        apply_write(&shared, ino, off, &data).await
                    });
                    match result {
                        Ok(()) => reply.written(data_len),
                        Err(_) => reply.error(libc::EIO),
                    }
                }),
            );
        } else {
            let _ = shared.transfer_ops.clone().submit(
                "write",
                Box::new(
                    move || match shared.rt.block_on(apply_write(&shared, ino, off, &data)) {
                        Ok(()) => reply.written(data_len),
                        Err(_) => reply.error(libc::EIO),
                    },
                ),
            );
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        let shared = self.shared.clone();
        // 上传异步执行；失败由 worker 重试
        let rt = shared.rt.clone();
        rt.spawn(async move {
            if let Err(e) = shared.upload_ino(ino).await {
                tracing::debug!(ino, error = %e, "flush upload deferred");
            }
        });
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        flush: bool,
        reply: ReplyEmpty,
    ) {
        let shared = self.shared.clone();
        // 打开计数：多个进程/句柄可能共享同一 inode，最后一个 release 才关文件
        shared.dec_open(ino);
        if flush {
            let rt = shared.rt.clone();
            rt.spawn(async move {
                let _ = shared.upload_ino(ino).await;
            });
        }
        reply.ok();
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let shared = self.shared.clone();
        // fsync 会等待一次完整上传（含多次网络 RPC），必须离开派发线程
        let _ = shared.transfer_ops.clone().submit(
            "fsync",
            Box::new(move || match shared.rt.block_on(shared.upload_ino(ino)) {
                Ok(()) => reply.ok(),
                Err(_) => reply.error(libc::EIO),
            }),
        );
    }

    fn opendir(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        reply: ReplyDirectory,
    ) {
        let shared = self.shared.clone();
        // readdir 未命中 TTL 就要远端 ls —— 池线程执行
        let _ = shared.ops.clone().submit(
            "setattr",
            Box::new(move || Self::readdir_impl(&shared, ino, offset, reply)),
        );
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        // 用缓存目录所在文件系统的剩余空间，近似“本地剩余空间”
        let path = self.shared.cache.root().to_string_lossy().to_string();
        let Ok(c_path) = std::ffi::CString::new(path) else {
            return reply.error(libc::EIO);
        };
        unsafe {
            let mut st: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c_path.as_ptr(), &mut st) != 0 {
                return reply.error(libc::EIO);
            }
            reply.statfs(
                st.f_blocks,
                st.f_bfree,
                st.f_bavail,
                0,
                0,
                st.f_bsize as u32,
                255,
                st.f_frsize as u32,
            );
        }
    }

    /// 暴露按需同步状态，便于 `getfattr` / 文件管理器插件 / 脚本查询。
    fn getxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        let Some(value) = self.shared.xattr_value(ino, &name.to_string_lossy()) else {
            return reply.error(libc::ENODATA);
        };
        if size == 0 {
            // 先问长度，再按长度取内容
            return reply.size(value.len() as u32);
        }
        if (size as usize) < value.len() {
            return reply.error(libc::ERANGE);
        }
        reply.data(value.as_bytes());
    }

    fn listxattr(&mut self, _req: &Request<'_>, ino: u64, size: u32, reply: ReplyXattr) {
        let is_file = self
            .shared
            .inodes
            .get(&ino)
            .map(|n| n.read().kind != FileType::Directory)
            .unwrap_or(false);
        if !is_file {
            return reply.size(0);
        }
        // NUL 分隔的属性名列表
        let mut names = Vec::new();
        for n in [
            XATTR_STATE,
            XATTR_REMOTE_SIZE,
            XATTR_LOCAL_SIZE,
            XATTR_PINNED,
            XATTR_PROGRESS,
        ] {
            names.extend_from_slice(n.as_bytes());
            names.push(0);
        }
        if size == 0 {
            return reply.size(names.len() as u32);
        }
        if (size as usize) < names.len() {
            return reply.error(libc::ERANGE);
        }
        reply.data(&names);
    }
}

impl OnDemandFs {
    /// readdir 的实际实现（在池线程上执行，可能触发远端 ls）。
    fn readdir_impl(shared: &Arc<Shared>, ino: u64, offset: i64, mut reply: ReplyDirectory) {
        let entries = match shared.rt.block_on(shared.list_dir(ino)) {
            Ok(e) => e,
            Err(_) => return reply.error(libc::EIO),
        };
        // 应用 overlay：本地新建出现、本地删除隐藏
        let mut skip: HashSet<String> = HashSet::new();
        let mut extra: Vec<(u64, FileType, String)> = Vec::new();
        if let Some(ov) = shared.overlays.get(&ino) {
            let ov = ov.read();
            skip.extend(ov.deleted.iter().cloned());
            for (name, created_ino) in ov.created.iter() {
                let kind = match shared.inodes.get(created_ino) {
                    Some(n) => n.read().kind,
                    None => FileType::RegularFile,
                };
                extra.push((*created_ino, kind, name.clone()));
            }
        }
        let parent_rel = shared
            .inodes
            .get(&ino)
            .map(|n| n.read().rel.clone())
            .unwrap_or_default();
        let parent_ino = if parent_rel.is_empty() {
            ROOT_INO
        } else {
            shared
                .ino_by_rel
                .get(Shared::parent_rel(&parent_rel))
                .map(|v| *v)
                .unwrap_or(ROOT_INO)
        };
        let mut listing: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".into()),
            (parent_ino, FileType::Directory, "..".into()),
        ];
        for e in entries {
            if skip.contains(&e.name) {
                continue;
            }
            let rel = if parent_rel.is_empty() {
                e.name.clone()
            } else {
                format!("{parent_rel}/{}", e.name)
            };
            let kind = if e.dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            let child = shared.get_or_alloc(&rel, kind);
            listing.push((child, kind, e.name));
        }
        // 根目录额外列出本机保留名（回收站）。它们不参与远端同步，
        // 且通常以 "." 开头，会被忽略点文件的过滤规则挡掉，因此单独处理。
        if parent_rel.is_empty() {
            if let Ok(rd) = std::fs::read_dir(shared.cache.root()) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if !Shared::is_local_only(&name) {
                        continue;
                    }
                    let kind = if e.path().is_dir() {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    };
                    let child = shared.get_or_alloc(&name, kind);
                    listing.push((child, kind, name));
                }
            }
        }
        listing.extend(extra);
        listing.sort_by(|a, b| a.2.cmp(&b.2));

        for (i, (child_ino, kind, name)) in listing.iter().enumerate().skip(offset.max(0) as usize)
        {
            if reply.add(*child_ino, (i + 1) as i64, *kind, OsStr::new(name)) {
                break;
            }
        }
        reply.ok();
    }

    fn attr_of(shared: &Arc<Shared>, ino: u64) -> Result<FileAttr> {
        let inode = shared
            .inodes
            .get(&ino)
            .ok_or_else(|| anyhow!("no inode {ino}"))?;
        let node = inode.read().clone();
        Ok(shared.attr_for(ino, &node))
    }

    fn do_create(&mut self, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let shared = self.shared.clone();
        let name = name.to_os_string();
        let _ = shared.ops.clone().submit(
            "do_create",
            Box::new(
                move || match shared.rt.block_on(shared.resolve_new_file(parent, &name)) {
                    Ok(ino) => {
                        let _ = shared.open_handle(ino);
                        match Self::attr_of(&shared, ino) {
                            Ok(attr) => reply.entry(&ATTR_TTL, &attr, 0),
                            Err(_) => reply.error(libc::EIO),
                        }
                    }
                    Err(_) => reply.error(libc::EIO),
                },
            ),
        );
    }
}

// ---------- Shared 的变更操作（供 Filesystem 调用） ----------

impl Shared {
    /// 新建本地文件（create/mknod）：立即产生本地缓存文件并登记 overlay。
    async fn resolve_new_file(&self, parent: u64, name: &OsStr) -> Result<u64> {
        let name_s = name.to_string_lossy().to_string();
        if name_s.contains('/') || name_s.is_empty() {
            anyhow::bail!("invalid name");
        }
        let entries = self.list_dir(parent).await?;
        let parent_rel = {
            let inode = self
                .inodes
                .get(&parent)
                .ok_or_else(|| anyhow!("no inode"))?;
            let node = inode.read();
            node.rel.clone()
        };
        let rel = if parent_rel.is_empty() {
            name_s.clone()
        } else {
            format!("{parent_rel}/{name_s}")
        };
        // 本地保留名：只落本地，绝不进入上传流程
        if Self::is_local_only(&rel) {
            let path = self.local_dir_path(&rel);
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            let ino = self.get_or_alloc(&rel, FileType::RegularFile);
            if let Some(inode) = self.inodes.get(&ino) {
                let mut node = inode.write();
                node.dirty = false;
                node.created = false;
                node.size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                node.mtime = now_secs();
            }
            self.invalidate_parent_listing(parent);
            return Ok(ino);
        }
        let exists_remotely = entries.iter().any(|e| e.name == name_s);
        let ino = self.get_or_alloc(&rel, FileType::RegularFile);
        {
            let inode = self.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
            let mut node = inode.write();
            node.created = true;
            if !node.dirty {
                node.dirty = true;
                node.size = 0;
                node.mtime = now_secs();
                node.stamp = (0, 0);
            }
        }
        if !exists_remotely {
            // 覆盖本地旧缓存（若有）
            let path = self.cache.file_path(&rel);
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            self.cache.remove(&rel);
            self.cache.set_state(
                &rel,
                CacheState {
                    size: 0,
                    dirty: true,
                    uploading: false,
                    stamp: (0, 0),
                    mtime: now_secs(),
                    last_access_ms: self.cache.now_ms(),
                },
            );
        } else {
            // create 语义（O_CREAT 无 O_EXCL）：远端已存在 → 本地内容不对应
            // 任何远端版本（stamp (0,0)），上传时触发保双方
            self.cache.remove(&rel);
            self.cache.ensure_state(&rel, (0, 0), now_secs());
        }
        self.overlays
            .entry(parent)
            .or_default()
            .write()
            .deleted
            .remove(&name_s);
        if !exists_remotely {
            self.overlays
                .entry(parent)
                .or_default()
                .write()
                .created
                .insert(name_s, ino);
        }
        self.invalidate_parent_listing(parent);
        // 注意：不得在请求处理路径中发内核失效通知（会与待处理请求互相等待导致死锁），
        // 目录可见性由 TTL 与 worker 轮询保证。
        self.mark_meta_dirty();
        Ok(ino)
    }

    async fn do_mkdir(&self, parent: u64, name: &OsStr) -> Result<u64> {
        let name_s = name.to_string_lossy().to_string();
        if name_s.contains('/') || name_s.is_empty() {
            anyhow::bail!("invalid name");
        }
        let parent_rel = {
            let inode = self
                .inodes
                .get(&parent)
                .ok_or_else(|| anyhow!("no inode"))?;
            let node = inode.read();
            node.rel.clone()
        };
        let rel = if parent_rel.is_empty() {
            name_s.clone()
        } else {
            format!("{parent_rel}/{name_s}")
        };
        if Self::is_local_only(&rel) {
            // 回收站目录：只在本机创建，绝不同步到 NAS
            std::fs::create_dir_all(self.local_dir_path(&rel))?;
            let ino = self.get_or_alloc(&rel, FileType::Directory);
            if let Some(inode) = self.inodes.get(&ino) {
                let mut node = inode.write();
                node.size = DIR_SIZE;
                node.mtime = now_secs();
            }
            self.invalidate_parent_listing(parent);
            return Ok(ino);
        }
        let remote = self.remote_path(&rel);
        self.nas.mkdir(&remote).await?;
        let ino = self.get_or_alloc(&rel, FileType::Directory);
        {
            let inode = self.inodes.get(&ino).ok_or_else(|| anyhow!("no inode"))?;
            let mut node = inode.write();
            node.size = DIR_SIZE;
            node.mtime = now_secs();
            node.stamp = (0, 0);
        }
        self.overlays
            .entry(parent)
            .or_default()
            .write()
            .created
            .insert(name_s.clone(), ino);
        self.invalidate_parent_listing(parent);
        self.add_log(logs::ACTION_META_CREATE, &rel, true).await;
        Ok(ino)
    }

    async fn do_remove(&self, parent: u64, name: &OsStr) -> Result<()> {
        let name_s = name.to_string_lossy().to_string();
        let parent_rel = {
            let inode = self
                .inodes
                .get(&parent)
                .ok_or_else(|| anyhow!("no inode"))?;
            let node = inode.read();
            node.rel.clone()
        };
        let rel = if parent_rel.is_empty() {
            name_s.clone()
        } else {
            format!("{parent_rel}/{name_s}")
        };
        // 回收站内的条目：只删本地，不触碰 NAS
        if Self::is_local_only(&rel) {
            let path = self.local_dir_path(&rel);
            let meta = std::fs::metadata(&path);
            match meta {
                Ok(m) if m.is_dir() => {
                    let _ = std::fs::remove_dir_all(&path);
                }
                Ok(_) => {
                    let _ = std::fs::remove_file(&path);
                }
                Err(_) => {}
            }
            self.cache.remove(&rel);
            self.invalidate_parent_listing(parent);
            return Ok(());
        }
        // 纯本地新建 → 直接丢弃
        let was_created = self
            .overlays
            .get(&parent)
            .map(|ov| ov.read().created.contains_key(&name_s))
            .unwrap_or(false);
        if was_created {
            if let Some(ov) = self.overlays.get(&parent) {
                ov.write().created.remove(&name_s);
            }
            let _ = std::fs::remove_file(self.cache.file_path(&rel));
            self.cache.remove(&rel);
            self.mark_meta_dirty();
            self.invalidate_parent_listing(parent);
            return Ok(());
        }
        // 本地删除 + 远端删除（worker 重试）
        self.pending_removes.insert(rel.clone(), 0);
        self.overlays
            .entry(parent)
            .or_default()
            .write()
            .deleted
            .insert(name_s.clone());
        let _ = std::fs::remove_file(self.cache.file_path(&rel));
        self.cache.remove(&rel);
        self.mark_meta_dirty();
        self.invalidate_parent_listing(parent);
        Ok(())
    }

    /// 处理“移入回收站”（= 删除，按官方语义同步删除 NAS）与“从回收站还原”。
    async fn rename_with_trash(
        &self,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
    ) -> Result<()> {
        let name_s = name.to_string_lossy().to_string();
        let new_s = newname.to_string_lossy().to_string();
        let parent_rel = {
            let inode = self
                .inodes
                .get(&parent)
                .ok_or_else(|| anyhow!("no inode"))?;
            let node = inode.read();
            node.rel.clone()
        };
        let newparent_rel = {
            let inode = self
                .inodes
                .get(&newparent)
                .ok_or_else(|| anyhow!("no inode"))?;
            let node = inode.read();
            node.rel.clone()
        };
        let rel = if parent_rel.is_empty() {
            name_s.clone()
        } else {
            format!("{parent_rel}/{name_s}")
        };
        let new_rel = if newparent_rel.is_empty() {
            new_s.clone()
        } else {
            format!("{newparent_rel}/{new_s}")
        };

        let into_trash = Self::is_local_only(&new_rel);
        let out_of_trash = Self::is_local_only(&rel) && !into_trash;
        if !into_trash && !out_of_trash {
            anyhow::bail!("cross-device rename not supported");
        }

        if into_trash {
            // 1) 删除远端（官方语义：本地删除即通知远端删除）
            if !Self::is_local_only(&rel) {
                self.pending_removes.insert(rel.clone(), 0);
            }
            // 2) 把本地内容搬到回收站目录（供用户在回收站里看到）
            let src = self.cache.file_path(&rel);
            let dst = self.cache.file_path(&new_rel);
            if let Some(p) = dst.parent() {
                std::fs::create_dir_all(p)?;
            }
            if src.exists() {
                let _ = std::fs::rename(&src, &dst);
            }
            self.cache.remove(&rel);
            self.drop_inode(&rel);
            self.invalidate_parent_listing(parent);
            self.invalidate_parent_listing(newparent);
            return Ok(());
        }

        // 从回收站还原：当作新的本地文件，交给 worker 上传
        let src = self.cache.file_path(&rel);
        let dst = self.cache.file_path(&new_rel);
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        if src.exists() {
            std::fs::rename(&src, &dst)?;
        }
        self.cache.remove(&rel);
        self.drop_inode(&rel);
        let size = std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
        let ino = self.get_or_alloc(&new_rel, FileType::RegularFile);
        if let Some(inode) = self.inodes.get(&ino) {
            let mut node = inode.write();
            node.dirty = true;
            node.created = true;
            node.size = size;
            node.mtime = now_secs();
            node.stamp = (0, 0);
        }
        self.overlays
            .entry(newparent)
            .or_default()
            .write()
            .created
            .insert(new_s.clone(), ino);
        self.cache.set_state(
            &new_rel,
            CacheState {
                size,
                dirty: true,
                uploading: false,
                stamp: (0, 0),
                mtime: now_secs(),
                last_access_ms: self.cache.now_ms(),
            },
        );
        self.mark_meta_dirty();
        self.invalidate_parent_listing(parent);
        self.invalidate_parent_listing(newparent);
        Ok(())
    }

    async fn do_rename(
        &self,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
    ) -> Result<()> {
        // 跨目录改名：只对“移入/移出回收站”放行，其余仍拒绝
        // （挂载内跨目录移动需要远端 move 语义，暂不支持）。
        if parent != newparent {
            return self
                .rename_with_trash(parent, name, newparent, newname)
                .await;
        }
        let name_s = name.to_string_lossy().to_string();
        let new_s = newname.to_string_lossy().to_string();
        if new_s.contains('/') || new_s.is_empty() {
            anyhow::bail!("invalid rename target");
        }
        let parent_rel = {
            let inode = self
                .inodes
                .get(&parent)
                .ok_or_else(|| anyhow!("no inode"))?;
            let node = inode.read();
            node.rel.clone()
        };
        let rel = if parent_rel.is_empty() {
            name_s.clone()
        } else {
            format!("{parent_rel}/{name_s}")
        };
        let new_rel = if parent_rel.is_empty() {
            new_s.clone()
        } else {
            format!("{parent_rel}/{new_s}")
        };
        // 本地保留名（回收站目录内）的改名：纯本地操作，绝不触碰 NAS。
        // 例如 gio 写入 .trashinfo 时会先写临时文件再改名为正式文件名。
        if Self::is_local_only(&rel) || Self::is_local_only(&new_rel) {
            let src = self.local_dir_path(&rel);
            let dst = self.local_dir_path(&new_rel);
            if let Some(p) = dst.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::rename(&src, &dst)?;
            let ino = self.get_or_alloc(&rel, FileType::RegularFile);
            self.ino_by_rel.remove(&rel);
            self.ino_by_rel.insert(new_rel.clone(), ino);
            if let Some(inode) = self.inodes.get(&ino) {
                let mut node = inode.write();
                node.rel = new_rel.clone();
                node.size = std::fs::metadata(&dst)
                    .map(|m| m.len())
                    .unwrap_or(node.size);
            }
            self.invalidate_parent_listing(parent);
            return Ok(());
        }
        let ino = self
            .ino_by_rel
            .get(&rel)
            .map(|v| *v)
            .ok_or_else(|| anyhow!("rename source not found"))?;
        // overlay 优先：纯本地改名
        let purely_local = self
            .overlays
            .get(&parent)
            .map(|ov| ov.read().created.contains_key(&name_s))
            .unwrap_or(false);
        if !purely_local {
            let remote = self.remote_path(&rel);
            self.nas.rename(&remote, &new_s).await?;
        }
        // 更新映射
        self.ino_by_rel.remove(&rel);
        self.ino_by_rel.insert(new_rel.clone(), ino);
        if let Some(inode) = self.inodes.get(&ino) {
            inode.write().rel = new_rel.clone();
        }
        // 缓存文件改名 + 状态迁移
        if let Some(state) = self.cache.state(&rel) {
            self.cache.remove(&rel);
            self.cache.set_state(&new_rel, state);
            let old_path = self.cache.file_path(&rel);
            let new_path = self.cache.file_path(&new_rel);
            if old_path.exists() {
                if let Some(p) = new_path.parent() {
                    std::fs::create_dir_all(p)?;
                }
                let _ = std::fs::rename(&old_path, &new_path);
            }
        }
        // overlay 更名
        if let Some(ov) = self.overlays.get(&parent) {
            let mut ov = ov.write();
            if let Some(v) = ov.created.remove(&name_s) {
                ov.created.insert(new_s.clone(), v);
            }
            if ov.deleted.contains(&name_s) {
                ov.deleted.remove(&name_s);
                ov.deleted.insert(new_s.clone());
            }
        }
        if !purely_local {
            // 覆盖目标：远端旧目标不存在了
            if let Some(ov) = self.overlays.get(&parent) {
                ov.write().deleted.remove(&new_s);
            }
        }
        self.invalidate_parent_listing(parent);
        self.add_log(logs::ACTION_RENAME, &new_rel, false).await;
        Ok(())
    }
}

#[allow(dead_code)]
pub(crate) fn mount_options() -> Vec<MountOption> {
    vec![
        MountOption::FSName("fn-sync".into()),
        MountOption::Subtype("fnsync-ondemand".into()),
        MountOption::DefaultPermissions,
        MountOption::RW,
    ]
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;
    #[test]
    fn transfer_saturation_does_not_block_metadata_and_queue_is_bounded() {
        let transfers = OpPool::new(1);
        let metadata = OpPool::new(1);
        let (release, wait) = std::sync::mpsc::channel();
        let (started, begin) = std::sync::mpsc::channel();
        assert!(transfers.submit(
            "slow",
            Box::new(move || {
                started.send(()).unwrap();
                wait.recv().unwrap();
            })
        ));
        begin.recv_timeout(Duration::from_secs(1)).unwrap();
        for _ in 0..128 {
            assert!(transfers.submit("queued", Box::new(|| {})));
        }
        assert!(!transfers.submit("overflow", Box::new(|| {})));
        let (done, result) = std::sync::mpsc::channel();
        assert!(metadata.submit(
            "lookup",
            Box::new(move || {
                done.send(()).unwrap();
            })
        ));
        result.recv_timeout(Duration::from_secs(1)).unwrap();
        release.send(()).unwrap();
    }
}
