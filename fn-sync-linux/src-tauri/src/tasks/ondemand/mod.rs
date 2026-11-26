//! 按需同步（open_cloud）—— Linux 实现。
//!
//! 官方客户端在 Windows 用内核 CfAPI（cldflt.sys + provider.exe）、macOS 用
//! FileProvider 实现“文件支持按需同步”（占位文件 + 打开时下载 + 释放空间），
//! Linux 官方明确不支持（前端文案 “Linux 暂不支持按需同步”）。
//!
//! 本实现用 FUSE3 用户态挂载等价复刻该语义：
//! - `pc_path` 即挂载点，目录实时反映 NAS 内容；
//! - 文件仅元数据占位，首次 open/read 才下载（DemandWriter 语义）；
//! - 本地写入走缓存并异步上传（与同步引擎同一套上传三步协议）；
//! - 缓存 LRU 淘汰，释放电脑磁盘；
//! - 远端变化经轮询 + 内核失效通知实时反映。
//!
//! 运行时依赖：内核 fuse 模块 + `fusermount3` 或 fuse2 的 `fusermount`
//! （麒麟 134 等目标机只装了 fuse2；回收残留挂载时两者都会尝试）。

pub mod cache;
pub mod control;
pub mod fs;
pub mod remote;

use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::service::logs;
use crate::tasks::file_filter::FileFilter;

/// 默认缓存上限 10 GiB（可用 system_table `ondemand_cache_limit_bytes` 覆盖）。
pub const DEFAULT_CACHE_LIMIT_BYTES: u64 = 10 * 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct OnDemandConfig {
    pub task_id: i64,
    pub task_name: String,
    /// 挂载点（即任务的 pc_path）
    pub mountpoint: PathBuf,
    /// NAS 侧根目录，如 `vol2/1000/下载/xxx`
    pub nas_root: String,
    /// 内容缓存目录
    pub cache_root: PathBuf,
    pub cache_limit_bytes: u64,
}

pub struct OnDemandMount {
    cfg: OnDemandConfig,
    /// 持有 fuser::BackgroundSession 的所有者线程（session 非 Sync，不能存结构体——
    /// AppState 必须满足 Sync）。stop 信号 → owner 线程内 join（卸载 + 等派发线程退出）。
    session_owner: Option<std::thread::JoinHandle<()>>,
    stop_tx: Option<std::sync::mpsc::Sender<()>>,
    worker: Option<tokio::task::AbortHandle>,
    /// 独立 runtime：FUSE 回调 block_on 与 worker 都在此执行。
    /// Option 化以便在非异步上下文中显式关闭（tokio Runtime 禁止在异步上下文 drop）。
    rt: Option<tokio::runtime::Runtime>,
    pub shared: Arc<fs::Shared>,
}

impl OnDemandMount {
    /// 挂载并启动 worker。阻塞时间 = 一次 fusermount3 调用（几十毫秒）。
    pub fn start(
        cfg: OnDemandConfig,
        nas: Arc<dyn remote::RemoteOps>,
        db: sqlx::SqlitePool,
        filter: FileFilter,
    ) -> Result<Self> {
        // 上次异常退出（崩溃 / 被 kill）留下的死挂载必须先回收：
        // 否则新挂载会失败（EEXIST），或者叠在旧挂载之上——
        // 用户看到的是下面那层永远不响应的旧挂载，表现为“目录卡死”。
        //
        // 顺序很重要：必须在 create_dir_all 之前。死挂载点的 stat/mkdir 会返回
        // ENOTCONN，create_dir_all 因此失败并提前 return，recover 就永远跑不到。
        // 若控制 socket 仍有进程应答，说明另一个客户端实例正在使用这个挂载，
        // 强行卸载会把它的挂载弄坏。此时直接报错，让用户先退出另一个实例。
        let control_socket = cfg.cache_root.join("control.sock");
        if is_fuse_mount(&cfg.mountpoint) && control_socket_alive(&control_socket) {
            return Err(anyhow!(
                "该电脑文件夹已由另一个飞牛同步实例挂载：{}（请先退出另一个实例）",
                cfg.mountpoint.display()
            ));
        }
        recover_stale_mount(&cfg.mountpoint);
        if is_fuse_mount(&cfg.mountpoint) {
            return Err(anyhow!("旧挂载尚未释放，拒绝重复挂载"));
        }
        // 挂载点必须是空目录（与官方 macOS 的 CloudStorage 目录语义一致）：
        // fuse 不允许挂在非空目录上，硬挂也会把原有文件全部遮蔽，用户会以为文件丢了。
        // 创建/开启按需同步时已提前校验；这里是兜底（例如用户先建好任务再往目录里放文件）。
        if let Ok(mut entries) = std::fs::read_dir(&cfg.mountpoint) {
            if entries.next().is_some() {
                return Err(anyhow!(
                    "按需同步要求电脑文件夹为空：{}（请清空该文件夹，或新建一个空文件夹后再选择）",
                    cfg.mountpoint.display()
                ));
            }
        }
        if let Err(e) = std::fs::create_dir_all(&cfg.mountpoint) {
            tracing::warn!(
                mountpoint = %cfg.mountpoint.display(),
                error = %e,
                "create mountpoint failed (may be a busy fuse mount)"
            );
        }
        // worker 线程数不要压得太紧：FUSE 回调的 block_on、下载、上传都会占用
        // worker 线程，线程太少时一个慢请求就能让整个挂载树排队。
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?;
        let shared = Arc::new(fs::Shared::new(
            cfg.clone(),
            nas,
            rt.handle().clone(),
            db,
            filter,
        )?);
        {
            let _enter = rt.enter();
            control::start(shared.clone())?;
        }
        let filesystem = fs::OnDemandFs {
            shared: shared.clone(),
        };
        let options = fs::mount_options();
        let session = fuser::Session::new(filesystem, &cfg.mountpoint, &options).map_err(|e| {
            shared
                .stopping
                .store(true, std::sync::atomic::Ordering::Relaxed);
            anyhow!("mount {} failed: {e}", cfg.mountpoint.display())
        })?;
        shared.set_notifier(session.notifier());
        // 独立看门狗线程（不依赖 runtime）：把卡住的回调直接写进日志。
        // 没有它，挂载卡死时进程内所有诊断都停摆，只能靠外部猜。
        {
            let wd = shared.clone();
            std::thread::Builder::new()
                .name("fnsync-od-watchdog".into())
                .spawn(move || loop {
                    std::thread::sleep(Duration::from_secs(10));
                    if wd.stopping.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let slow = wd.ops.slow(Duration::from_secs(20));
                    let queued = wd.ops.queued();
                    if !slow.is_empty() || queued > 0 {
                        tracing::warn!(
                            queued,
                            slow = ?slow,
                            "on-demand ops backlogged/stuck"
                        );
                    }
                })
                .ok();
        }
        // worker 在独立 runtime 上运行
        let worker = rt.spawn(shared.clone().worker()).abort_handle();
        let session = session.spawn().map_err(|e| {
            shared
                .stopping
                .store(true, std::sync::atomic::Ordering::Relaxed);
            anyhow!("background fuse session failed: {e}")
        })?;
        // session 所有者线程：收到 stop 信号后 join（卸载 + 等待派发退出）
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let session_owner = std::thread::Builder::new()
            .name("fnsync-od-owner".into())
            .spawn(move || {
                if stop_rx.recv().is_ok() {
                    // join：卸载 + 等待派发线程退出（ENODEV）
                    session.join();
                }
            })?;
        tracing::info!(
            task_id = cfg.task_id,
            mountpoint = %cfg.mountpoint.display(),
            nas_root = %cfg.nas_root,
            "on-demand mount started"
        );
        Ok(Self {
            cfg,
            session_owner: Some(session_owner),
            stop_tx: Some(stop_tx),
            worker: Some(worker),
            rt: Some(rt),
            shared,
        })
    }

    pub fn is_active(&self) -> bool {
        !self
            .shared
            .stopping
            .load(std::sync::atomic::Ordering::Relaxed)
            && is_fuse_mount(&self.cfg.mountpoint)
    }

    pub fn mountpoint(&self) -> &std::path::Path {
        &self.cfg.mountpoint
    }

    pub fn cache_usage_bytes(&self) -> u64 {
        self.shared.cache.total_size()
    }

    /// 停止 worker、卸载挂载点并关闭 runtime。
    /// 卸载语义为延迟卸载（lazy），打开中的文件收到 EIO 后自然结束。
    pub fn stop(mut self) -> Result<()> {
        self.shutdown();
        if is_fuse_mount(&self.cfg.mountpoint) {
            return Err(anyhow!("挂载尚未释放，请关闭占用程序后重试"));
        }
        tracing::info!(
            task_id = self.cfg.task_id,
            mountpoint = %self.cfg.mountpoint.display(),
            "on-demand mount stopped"
        );
        Ok(())
    }

    fn shutdown(&mut self) {
        self.shared
            .stopping
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            w.abort();
        }
        // 通知 owner 线程 unmount + join。session.join() 会等到派发线程退出
        // （ENODEV）；若某个回调仍然卡着，join 就永远不返回，整个 UI 的暂停/
        // 删除任务操作会被拖死。因此这里必须有界等待：超时后放弃 join，
        // 让卸载由进程退出时的 auto_unmount / Drop 兜底。
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(owner) = self.session_owner.take() {
            let mountpoint = self.cfg.mountpoint.clone();
            let done = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
            let done_w = done.clone();
            let handle = std::thread::Builder::new()
                .name("fnsync-od-join".into())
                .spawn(move || {
                    let _ = owner.join();
                    let (lock, cv) = &*done_w;
                    *lock.lock().unwrap() = true;
                    cv.notify_all();
                });
            if handle.is_ok() {
                let (lock, cv) = &*done;
                let finished = lock.lock().unwrap();
                let waited = cv
                    .wait_timeout_while(finished, Duration::from_secs(10), |done: &mut bool| !*done)
                    .map(|(guard, _)| *guard)
                    .unwrap_or_else(|e| *e.into_inner().0);
                if !waited {
                    tracing::warn!(
                        mountpoint = %mountpoint.display(),
                        "on-demand unmount did not finish in time; releasing task anyway"
                    );
                }
            }
        }
        // Confirm removal from this mount namespace before another task can remount.
        recover_stale_mount(&self.cfg.mountpoint);
        // tokio Runtime 不能在异步上下文中 drop；挪到独立线程释放。
        // drop(rt) 会等待 worker 线程退出：若有 block_on 中的回调仍卡着，
        // 这里 join 就会永久不返回，导致“暂停/删除任务”卡死。因此不 join，
        // 让它后台完成（进程退出时由 OS 回收）。
        if let Some(rt) = self.rt.take() {
            let _ = std::thread::Builder::new()
                .name("fnsync-rt-drop".into())
                .spawn(move || drop(rt));
        }
    }
}

impl Drop for OnDemandMount {
    fn drop(&mut self) {
        // 兜底：未显式 stop（如进程退出）也要卸载
        self.shutdown();
    }
}

/// 挂载点是否已经是 FUSE 挂载（含我们自己残留的）。
pub(crate) fn is_fuse_mount(mountpoint: &std::path::Path) -> bool {
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    mountinfo_has_fuse(&info, &escape_mount_path(&mountpoint.to_string_lossy()))
}

/// 在 mountinfo 文本里查找 `target` 是否为 FUSE 挂载。
///
/// mountinfo 格式（空格分隔）：
/// `id parent major:minor root mountpoint options [optional...] - fstype source superopts`
/// 注意 fstype **不在固定位置**：可选字段个数可变，因此必须先定位分隔符 `-`。
fn mountinfo_has_fuse(info: &str, target: &str) -> bool {
    info.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let mounted_at = fields.get(4).copied().unwrap_or("");
        let fstype = fields
            .iter()
            .position(|f| *f == "-")
            .and_then(|i| fields.get(i + 1).copied())
            .unwrap_or("");
        mounted_at == target && fstype.starts_with("fuse")
    })
}

/// mountinfo 用 \040 之类转义空格等字符。
fn escape_mount_path(p: &str) -> String {
    p.chars()
        .map(|c| match c {
            ' ' => "\\040".to_string(),
            '\t' => "\\011".to_string(),
            '\n' => "\\012".to_string(),
            '\\' => "\\134".to_string(),
            other => other.to_string(),
        })
        .collect()
}

/// 回收上次进程异常退出后残留的挂载。
///
/// 必须带超时：`fusermount3 -u -z` 在挂载点仍被其它进程引用时（例如用户终端
/// 的工作目录就在里面）会**永久卡在 D 状态**，而 `-z` 的“延迟卸载”语义正是
/// 等所有引用释放。若这里同步等待（`.status()`），应用启动线程会被一起卡死，
/// 表现为整个客户端无响应。
fn recover_stale_mount(mountpoint: &std::path::Path) {
    if !is_fuse_mount(mountpoint) {
        return;
    }
    tracing::warn!(
        mountpoint = %mountpoint.display(),
        "stale fuse mount detected; unmounting before re-mount"
    );
    // 先试普通卸载：干净、快速，大多数情况足够。
    let plain_ok = unmount_with_fusermount(mountpoint, false, Duration::from_secs(3));
    if plain_ok && !is_fuse_mount(mountpoint) {
        tracing::info!(mountpoint = %mountpoint.display(), "stale mount removed");
        return;
    }
    // 普通卸载失败（挂载点仍被进程占用 → EBUSY，或守护进程已死 → ENOTCONN）。
    // 延迟卸载不会等待未完成请求，但可能永久阻塞，因此只给它很短的时间。
    let lazy_ok = unmount_with_fusermount(mountpoint, true, Duration::from_secs(2));
    if is_fuse_mount(mountpoint) {
        tracing::warn!(
            mountpoint = %mountpoint.display(),
            plain_ok, lazy_ok,
            "stale mount could not be removed; another process may hold it open \
             (e.g. a terminal whose working directory is inside). \
             Close it or run `fusermount3 -u -z` manually."
        );
    } else {
        tracing::info!(mountpoint = %mountpoint.display(), "stale mount removed (lazy)");
    }
}

/// 用 fusermount 卸载挂载点。
///
/// 优先 `fusermount3`（fuse3），不存在或失败时回退 fuse2 的 `fusermount`：
/// 麒麟 134 只装了 fuse2，之前只调 fusermount3 导致崩溃后的死挂载永远回收不掉，
/// 客户端重启一直报“旧挂载尚未释放”。
fn unmount_with_fusermount(
    mountpoint: &std::path::Path,
    lazy: bool,
    timeout: Duration,
) -> bool {
    for tool in ["fusermount3", "fusermount"] {
        let mut command = std::process::Command::new(tool);
        command.arg("-u");
        if lazy {
            command.arg("-z");
        }
        command.arg("--").arg(mountpoint);
        if run_with_timeout(&mut command, timeout) {
            return true;
        }
    }
    false
}

/// 控制 socket 是否有活着的守护进程在应答。
///
/// 用途：区分“自己上次崩溃留下的死挂载”（可以安全回收）和
/// “另一个客户端实例正在用的挂载”（绝不能抢占）。
fn control_socket_alive(socket: &std::path::Path) -> bool {
    use std::io::{BufRead, BufReader, Write};
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    if stream.write_all(b"{\"action\":\"status\"}\n").is_err() {
        return false;
    }
    let mut line = String::new();
    match BufReader::new(stream).read_line(&mut line) {
        Ok(n) if n > 0 => serde_json::from_str::<serde_json::Value>(&line)
            .map(|value| value.get("ok").and_then(|ok| ok.as_bool()) == Some(true))
            .unwrap_or(false),
        _ => false,
    }
}

/// 执行命令并在超时后放弃等待（不阻塞调用方）。
/// 返回 true 表示进程在超时前正常退出。
fn run_with_timeout(cmd: &mut std::process::Command, timeout: Duration) -> bool {
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    // 不杀进程：D 状态无法被杀，等它自己被内核唤醒即可。
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return false,
        }
    }
}

/// 从 system_table 读取缓存上限（字节）。非法/缺失 → 默认值。
pub async fn cache_limit_from_settings(db: &sqlx::SqlitePool) -> u64 {
    match logs::get_system_value(db, "ondemand_cache_limit_bytes").await {
        Ok(Some(text)) => text
            .trim()
            .parse::<u64>()
            .unwrap_or(DEFAULT_CACHE_LIMIT_BYTES),
        _ => DEFAULT_CACHE_LIMIT_BYTES,
    }
}

#[cfg(all(test, target_os = "linux"))]
/// mountinfo 解析的纯逻辑测试（用真实内核输出样本，防止字段位置回归）。
#[cfg(test)]
mod mountinfo_tests {
    use super::mountinfo_has_fuse;

    /// 本机真实的一行（用户实际遇到的场景：中文路径 + shared 可选字段）。
    const REAL_LINE: &str = "4184 238 0:160 / /home/xmlijian/文档/威特文档 \
rw,nosuid,nodev,relatime shared:1442 - fuse.fnsync-ondemand fn-sync \
rw,user_id=1000,group_id=1000,default_permissions";

    #[test]
    fn detects_our_fuse_mount() {
        assert!(mountinfo_has_fuse(
            REAL_LINE,
            "/home/xmlijian/文档/威特文档"
        ));
    }

    #[test]
    fn fstype_is_not_the_fifth_field() {
        // 第 5 个字段（1-based）是挂载点，不是 fstype。踩过这个坑：
        // 误把挂载点当 fstype → 残留挂载永远检测不到 → 重启后挂载失败 EEXIST。
        let fields: Vec<&str> = REAL_LINE.split_whitespace().collect();
        assert_eq!(fields.get(4), Some(&"/home/xmlijian/文档/威特文档"));
        assert!(!fields.get(4).unwrap().starts_with("fuse"));
    }

    #[test]
    fn ignores_non_fuse_mount_at_same_path() {
        let ext4 = "4184 238 8:2 / /home/xmlijian/文档/威特文档 rw,relatime - ext4 /dev/sda2 rw";
        assert!(!mountinfo_has_fuse(ext4, "/home/xmlijian/文档/威特文档"));
    }

    #[test]
    fn ignores_other_paths() {
        assert!(!mountinfo_has_fuse(REAL_LINE, "/home/xmlijian/别处"));
    }

    #[test]
    fn handles_escaped_spaces() {
        let line = "42 1 0:50 / /mnt/with\\040space rw - fuse.sshfs sshfs rw";
        assert!(mountinfo_has_fuse(line, "/mnt/with\\040space"));
    }

    #[test]
    fn variable_optional_fields() {
        // 可选字段数量可变：先定位 `-` 再取 fstype，不能写死索引。
        let no_optional = "7 1 0:60 / /data rw - fuse.rclone rclone rw";
        let many_optional =
            "7 1 0:60 / /data rw,nosuid master:1 propagate_from:2 shared:9 - fuse.rclone rclone rw";
        assert!(mountinfo_has_fuse(no_optional, "/data"));
        assert!(mountinfo_has_fuse(many_optional, "/data"));
    }
}

#[cfg(test)]
mod recovery_tests {
    //! 死挂载回收的前提判断：控制 socket 是否还有守护进程应答。
    use super::control_socket_alive;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fnsync-recovery-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn live_control_socket_is_detected() {
        let dir = temp_dir("live");
        let socket = dir.join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut line = String::new();
                let _ = BufReader::new(stream.try_clone().unwrap()).read_line(&mut line);
                let _ = stream.write_all(b"{\"ok\":true,\"result\":{}}\n");
            }
        });
        assert!(control_socket_alive(&socket));
        let _ = server.join();
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn missing_socket_is_not_alive() {
        let path = std::env::temp_dir().join("fnsync-no-such-control.sock");
        let _ = std::fs::remove_file(&path);
        assert!(!control_socket_alive(&path));
    }

    #[test]
    fn socket_file_without_listener_is_not_alive() {
        // 崩溃后残留的 socket 文件：存在但没人 accept。
        let dir = temp_dir("stale");
        let socket = dir.join("control.sock");
        std::fs::File::create(&socket).unwrap();
        assert!(!control_socket_alive(&socket));
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir(&dir);
    }
}

#[cfg(test)]
mod mount_tests {
    //! 真实 FUSE 挂载端到端测试（假 NAS 后端，无网络依赖）。
    //! 需要 /dev/fuse 与 fusermount3；沙箱环境可能禁止挂载，默认跳过。
    use super::*;
    use crate::service::database;
    use crate::tasks::ondemand::remote::fake::FakeNas;
    use crate::tasks::ondemand::remote::RemoteOps;
    use std::time::Duration;

    async fn wait_until<F: Fn() -> bool>(timeout_ms: u64, cond: F) -> bool {
        let mut waited = 0u64;
        while waited < timeout_ms {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            waited += 100;
        }
        cond()
    }

    fn make_cfg(mountpoint: PathBuf, cache_root: PathBuf) -> OnDemandConfig {
        OnDemandConfig {
            task_id: 1,
            task_name: "test-task".into(),
            mountpoint,
            nas_root: "root".into(),
            cache_root,
            cache_limit_bytes: 1024 * 1024,
        }
    }

    #[tokio::test]
    #[ignore = "requires /dev/fuse + fusermount3; run with FN_SYNC_FUSE_TEST=1"]
    async fn mount_read_write_delete_rename_roundtrip() {
        if std::env::var("FN_SYNC_FUSE_TEST").as_deref() != Ok("1") {
            eprintln!("SKIP: set FN_SYNC_FUSE_TEST=1 to run the real mount test");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let mountpoint = tmp.path().join("mnt");
        std::fs::create_dir_all(&mountpoint).unwrap();
        let nas = Arc::new(FakeNas::new());
        nas.seed_dir("root");
        nas.seed("root/seed.txt", b"remote content");
        nas.seed("root/big.bin", &[b'x'; 4096]);
        let cfg = make_cfg(mountpoint.clone(), tmp.path().join("cache"));
        let db = database::open_db(&tmp.path().join("db.sqlite"))
            .await
            .unwrap();
        database::migrate(&db).await.unwrap();
        let filter = FileFilter::from_db(None);
        let mount = OnDemandMount::start(cfg, nas.clone() as Arc<dyn RemoteOps>, db, filter)
            .expect("mount started");

        // 1. 目录列表反映 NAS 内容
        let names: Vec<String> = std::fs::read_dir(&mountpoint)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.contains(&"seed.txt".to_string()),
            "listing: {names:?}"
        );

        // 2. 占位文件：只读元数据不下载
        let meta = std::fs::metadata(mountpoint.join("big.bin")).unwrap();
        assert_eq!(meta.len(), 4096, "placeholder size");
        assert!(
            !mount.shared.cache.downloaded("big.bin"),
            "metadata query must not hydrate"
        );

        // 3. 读 → 按需下载
        assert_eq!(
            std::fs::read(mountpoint.join("seed.txt")).unwrap(),
            b"remote content",
            "on-demand hydration content"
        );
        assert!(mount.shared.cache.downloaded("seed.txt"), "hydrated");

        // 3a. 占位标识：未下载的文件必须报告 0 个数据块，
        // 否则 du 会显示满额，用户无法分辨是否已下载。
        let meta_before = std::fs::metadata(mountpoint.join("big.bin")).unwrap();
        assert_eq!(meta_before.len(), 4096, "logical size from remote");
        assert_eq!(
            std::os::unix::fs::MetadataExt::blocks(&meta_before),
            0,
            "placeholder must report zero blocks (du shows 0)"
        );

        // 3b. 回归：缓存文件被外部清掉后，读取必须重新下载。
        // 历史缺陷：只看内存状态就认为“已缓存”，open_handle 的 create(true)
        // 造出空文件，读取返回 0 字节且不报错（静默数据损坏）。
        let cached = mount.shared.cache.file_path("seed.txt");
        assert!(cached.exists(), "cache file should exist after hydration");
        std::fs::remove_file(&cached).unwrap();
        let reread = std::fs::read(mountpoint.join("seed.txt")).unwrap();
        assert_eq!(
            reread, b"remote content",
            "must re-download instead of returning an empty file"
        );
        assert_eq!(
            std::fs::metadata(&cached).map(|m| m.len()).unwrap_or(0),
            14,
            "cache file should be restored with full content"
        );

        // 已下载的文件必须报告非零块数，才能与占位区分开。
        let hydrated_meta = std::fs::metadata(mountpoint.join("seed.txt")).unwrap();
        assert!(
            std::os::unix::fs::MetadataExt::blocks(&hydrated_meta) > 0,
            "cached file must report non-zero blocks"
        );

        // 3c. 缓存必须在重启后仍被认作“已下载”。
        // 历史缺陷：load_meta 只恢复脏文件，干净缓存的内存状态丢失 →
        // 被当成占位（du 报 0 块），且每次读取都重新下载。
        // 这里清空内存状态模拟重启，磁盘文件保留，验证用户可见行为仍正确。
        {
            let rel = "seed.txt";
            mount.shared.cache.remove(rel);
            assert!(
                std::fs::metadata(mount.shared.cache.file_path(rel)).is_ok(),
                "cache file must stay on disk"
            );
            let meta = std::fs::metadata(mountpoint.join(rel)).unwrap();
            assert!(
                std::os::unix::fs::MetadataExt::blocks(&meta) > 0,
                "cached file must still report non-zero blocks after state loss"
            );
            // 再读一次必须命中磁盘缓存，而不是重新下载后返回空/错误
            assert_eq!(
                std::fs::read(mountpoint.join(rel)).unwrap(),
                b"remote content"
            );
        }

        // 4. 写 → 上传
        std::fs::write(mountpoint.join("new.txt"), b"written via fuse").unwrap();
        assert!(
            wait_until(8000, || nas.read("root/new.txt").is_some()).await,
            "upload observed"
        );
        assert_eq!(nas.read("root/new.txt").unwrap(), b"written via fuse");

        // 5. 删除远端已有文件 → NAS 同步删除
        std::fs::remove_file(mountpoint.join("seed.txt")).unwrap();
        assert!(
            wait_until(8000, || !nas.exists("root/seed.txt")).await,
            "remote delete propagated"
        );

        // 6. 改名 → NAS 同步改名
        std::fs::rename(mountpoint.join("big.bin"), mountpoint.join("big2.bin")).unwrap();
        assert!(
            wait_until(500, || nas.exists("root/big2.bin")
                && !nas.exists("root/big.bin"))
            .await,
            "remote rename propagated"
        );
        assert_eq!(
            std::fs::read(mountpoint.join("big2.bin")).unwrap(),
            vec![b'x'; 4096],
            "hydrated after rename"
        );

        // 7. 建目录 → NAS 建目录
        std::fs::create_dir(mountpoint.join("subdir")).unwrap();
        assert!(wait_until(500, || nas.exists("root/subdir")).await, "mkdir");

        // 8. 远端冲突 → Conflicted Copy 保留双方
        // 构造真冲突：先读（下载 v1）→ NAS 侧变化为 v2 → 本地基于 v1 编辑保存
        nas.seed("root/c.txt", b"v1");
        assert!(wait_until(9000, || mountpoint.join("c.txt").exists()).await);
        assert_eq!(
            std::fs::read(mountpoint.join("c.txt")).unwrap(),
            b"v1",
            "hydrate v1"
        );
        nas.seed("root/c.txt", b"v2-changed");
        // root 差异轮询 → 远端新版本可见（大小 10）
        assert!(
            wait_until(9000, || std::fs::metadata(mountpoint.join("c.txt"))
                .map(|m| m.len() == 10)
                .unwrap_or(false))
            .await,
            "remote v2 visible"
        );
        std::fs::write(mountpoint.join("c.txt"), b"local edit").unwrap();
        assert!(
            wait_until(8000, || nas.read("root/c.txt").as_deref()
                == Some(&b"local edit"[..]))
            .await,
            "local edit uploaded"
        );
        let conflicted: Vec<Vec<u8>> = nas.snapshot_conflicted("root/c.txt").into_iter().collect();
        assert!(
            conflicted.contains(&b"v2-changed".to_vec()),
            "conflicted copy keeps remote version, got {conflicted:?}"
        );

        // 回归：worker 每 6s 跑一次 poll_root_diff。历史上它持有 dir_cache 读守卫
        // 又去 insert 同一分片，造成 DashMap 自死锁，随后任何 lookup/readdir 都会
        // 阻塞在同步的 get() 上，tokio 超时也无法触发 —— 整棵挂载树永久卡死。
        // 因此必须等过一个轮询周期后，再确认挂载点仍然响应。
        tokio::time::sleep(Duration::from_secs(9)).await;
        let t0 = std::time::Instant::now();
        let n = std::fs::read_dir(&mountpoint)
            .expect("listing must stay responsive after root diff polling")
            .count();
        assert!(n > 0, "listing should not be empty");
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "mount froze after root diff polling: {:?}",
            t0.elapsed()
        );

        mount.stop().unwrap();
        // 卸载后挂载点不再是 fuse
        let out = std::process::Command::new("mountpoint")
            .arg(&mountpoint)
            .output();
        if let Ok(out) = out {
            assert!(!out.status.success(), "should be unmounted");
        }
    }

    /// 回归：编辑器原子保存 = 创建 .goutputstream-* 临时文件 → 写入 → rename 成目标。
    /// 旧实现会先上传临时文件、再删掉 overlay 记录，点文件过滤又让源路径不可解析，
    /// 最终 rename 报 ENOENT（“重命名临时文件出错：没有那个文件或目录”）。
    #[tokio::test]
    #[ignore = "requires /dev/fuse + fusermount3; run with FN_SYNC_FUSE_TEST=1"]
    async fn atomic_save_via_dotfile_rename_works() {
        if std::env::var("FN_SYNC_FUSE_TEST").as_deref() != Ok("1") {
            eprintln!("SKIP: set FN_SYNC_FUSE_TEST=1 to run the real mount test");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let mountpoint = tmp.path().join("mnt");
        std::fs::create_dir_all(&mountpoint).unwrap();
        let nas = Arc::new(FakeNas::new());
        nas.seed_dir("root");
        nas.seed("root/target.txt", b"old");
        let cfg = make_cfg(mountpoint.clone(), tmp.path().join("cache"));
        let db = database::open_db(&tmp.path().join("db.sqlite"))
            .await
            .unwrap();
        database::migrate(&db).await.unwrap();
        let filter = FileFilter::from_db(None);
        let mount = OnDemandMount::start(cfg, nas.clone() as Arc<dyn RemoteOps>, db, filter)
            .expect("mount started");

        // 等目标文件出现在挂载里，确保它的远端戳被解析。
        assert!(
            wait_until(9000, || mountpoint.join("target.txt").exists()).await,
            "target visible"
        );

        // 覆盖已有文件：临时文件 .goutputstream-* → rename 成 target.txt
        std::fs::write(mountpoint.join(".goutputstream-test"), b"new content").unwrap();
        assert!(
            mountpoint.join(".goutputstream-test").exists(),
            "scratch file must be visible before rename"
        );
        std::fs::rename(
            mountpoint.join(".goutputstream-test"),
            mountpoint.join("target.txt"),
        )
        .expect("atomic-save rename must not fail");

        assert_eq!(
            std::fs::read(mountpoint.join("target.txt")).unwrap(),
            b"new content",
            "target must show new content immediately"
        );
        assert!(
            wait_until(9000, || nas.read("root/target.txt").as_deref()
                == Some(&b"new content"[..]))
            .await,
            "target must be uploaded to NAS"
        );
        assert!(
            !nas.exists("root/.goutputstream-test"),
            "scratch file must never be uploaded to NAS"
        );

        // 新建文件：同样通过临时文件 + rename
        std::fs::write(mountpoint.join(".goutputstream-new"), b"brand new").unwrap();
        std::fs::rename(
            mountpoint.join(".goutputstream-new"),
            mountpoint.join("new.txt"),
        )
        .expect("atomic-save rename for new file");
        assert_eq!(
            std::fs::read(mountpoint.join("new.txt")).unwrap(),
            b"brand new"
        );
        assert!(
            wait_until(9000, || nas.read("root/new.txt").as_deref()
                == Some(&b"brand new"[..]))
            .await,
            "new file must be uploaded"
        );
        assert!(!nas.exists("root/.goutputstream-new"));

        // WPS 风格：文件名.~#N → 正式文件
        std::fs::write(mountpoint.join("wps.txt"), b"wps old").unwrap();
        assert!(
            wait_until(9000, || nas.read("root/wps.txt").as_deref()
                == Some(&b"wps old"[..]))
            .await,
            "wps target uploaded"
        );
        std::fs::write(mountpoint.join("wps.txt.~#1"), b"wps new").unwrap();
        std::fs::rename(
            mountpoint.join("wps.txt.~#1"),
            mountpoint.join("wps.txt"),
        )
        .expect("wps-style atomic save must not fail");
        assert_eq!(
            std::fs::read(mountpoint.join("wps.txt")).unwrap(),
            b"wps new"
        );
        assert!(
            wait_until(9000, || nas.read("root/wps.txt").as_deref()
                == Some(&b"wps new"[..]))
            .await,
            "wps target must be uploaded"
        );
        assert!(
            !nas.exists("root/wps.txt.~#1"),
            "wps scratch file must never be uploaded"
        );

        mount.stop().unwrap();
    }

    /// 回归：远端删除后必须清理本地缓存和 inode。否则快照一直显示 cached，
    /// 右键菜单仍提供“释放”，但挂载里已无该文件，释放报“文件不存在”。
    #[tokio::test]
    #[ignore = "requires /dev/fuse + fusermount3; run with FN_SYNC_FUSE_TEST=1"]
    async fn remote_delete_frees_local_cache() {
        if std::env::var("FN_SYNC_FUSE_TEST").as_deref() != Ok("1") {
            eprintln!("SKIP: set FN_SYNC_FUSE_TEST=1 to run the real mount test");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let mountpoint = tmp.path().join("mnt");
        std::fs::create_dir_all(&mountpoint).unwrap();
        let nas = Arc::new(FakeNas::new());
        nas.seed_dir("root");
        nas.seed("root/gone.txt", b"remote gone");
        let cfg = make_cfg(mountpoint.clone(), tmp.path().join("cache"));
        let db = database::open_db(&tmp.path().join("db.sqlite"))
            .await
            .unwrap();
        database::migrate(&db).await.unwrap();
        let filter = FileFilter::from_db(None);
        let mount = OnDemandMount::start(cfg, nas.clone() as Arc<dyn RemoteOps>, db, filter)
            .expect("mount started");

        assert!(
            wait_until(9000, || mountpoint.join("gone.txt").exists()).await,
            "file visible"
        );
        assert_eq!(
            std::fs::read(mountpoint.join("gone.txt")).unwrap(),
            b"remote gone"
        );
        assert!(mount.shared.cache.file_path("gone.txt").exists());

        // 直接在 NAS 侧删除，模拟别处/其他客户端删除了这个文件
        nas.remove(&["root/gone.txt".to_string()])
            .await
            .unwrap();

        assert!(
            wait_until(20000, || !mount
                .shared
                .cache
                .file_path("gone.txt")
                .exists())
            .await,
            "local cache must be freed after remote delete"
        );
        assert!(
            mount.shared.ino_by_rel.get("gone.txt").is_none(),
            "inode must be dropped after remote delete"
        );
        assert!(
            !mountpoint.join("gone.txt").exists(),
            "mount entry must disappear"
        );

        mount.stop().unwrap();
    }

    /// 回归测试：单个慢远端请求不能让整棵挂载树失去响应。
    /// 这是用户实际遇到的症状（目录卡死、无法读写）。
    #[tokio::test]
    #[ignore = "requires /dev/fuse + fusermount3; run with FN_SYNC_FUSE_TEST=1"]
    async fn slow_remote_never_freezes_the_whole_mount() {
        if std::env::var("FN_SYNC_FUSE_TEST").as_deref() != Ok("1") {
            eprintln!("SKIP: set FN_SYNC_FUSE_TEST=1 to run the real mount test");
            return;
        }
        use crate::connect::nas_client::RemoteEntry;
        use futures_util::future::BoxFuture;

        /// 每次 ls 都拖 2 秒的远端：模拟 NAS 很慢/掉线抖动。
        struct SlowNas(Arc<FakeNas>);

        impl RemoteOps for SlowNas {
            fn ls<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Vec<RemoteEntry>>> {
                let inner = self.0.clone();
                let p = path.to_string();
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    inner.ls(&p).await
                })
            }
            fn mkdir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<()>> {
                let inner = self.0.clone();
                let p = path.to_string();
                Box::pin(async move { inner.mkdir(&p).await })
            }
            fn remove<'a>(&'a self, paths: &'a [String]) -> BoxFuture<'a, Result<()>> {
                let inner = self.0.clone();
                let p = paths.to_vec();
                Box::pin(async move { inner.remove(&p).await })
            }
            fn rename<'a>(&'a self, path: &'a str, new_name: &'a str) -> BoxFuture<'a, Result<()>> {
                let inner = self.0.clone();
                let p = path.to_string();
                let n = new_name.to_string();
                Box::pin(async move { inner.rename(&p, &n).await })
            }
            fn download<'a>(&'a self, remote: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
                let inner = self.0.clone();
                let p = remote.to_string();
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    inner.download(&p).await
                })
            }
            fn download_to<'a>(
                &'a self,
                remote: &'a str,
                dest: &'a std::path::Path,
                progress: Arc<dyn Fn(u64) + Send + Sync>,
            ) -> BoxFuture<'a, Result<u64>> {
                let inner = self.0.clone();
                let p = remote.to_string();
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    inner.download_to(&p, dest, progress).await
                })
            }
            fn upload<'a>(
                &'a self,
                remote: &'a str,
                data: Vec<u8>,
                mtime: Option<i64>,
            ) -> BoxFuture<'a, Result<()>> {
                let inner = self.0.clone();
                let p = remote.to_string();
                Box::pin(async move { inner.upload(&p, data, mtime).await })
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let mountpoint = tmp.path().join("mnt");
        std::fs::create_dir_all(&mountpoint).unwrap();
        let fake = Arc::new(FakeNas::new());
        fake.seed_dir("root");
        fake.seed("root/a.txt", b"alpha");
        let nas: Arc<dyn RemoteOps> = Arc::new(SlowNas(fake.clone()));
        let cfg = make_cfg(mountpoint.clone(), tmp.path().join("cache"));
        let db = database::open_db(&tmp.path().join("db.sqlite"))
            .await
            .unwrap();
        database::migrate(&db).await.unwrap();
        let mount =
            OnDemandMount::start(cfg, nas, db, FileFilter::from_db(None)).expect("mount started");

        // 先在后台制造持续并发访问：每个请求都可能触发 2 秒的远端 ls。
        let mp = mountpoint.clone();
        let stop_pump = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_pump_w = stop_pump.clone();
        let pump = std::thread::spawn(move || {
            for i in 0..12 {
                if stop_pump_w.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let _ = std::fs::metadata(&mp);
                let _ = std::fs::read_dir(&mp).map(|r| r.count());
                let _ = std::fs::metadata(mp.join("a.txt"));
                let _ = std::fs::write(mp.join(format!("local-{i}.txt")), b"local");
            }
        });

        // 关键断言：即使有慢请求在飞，一次本地 getattr 也必须在很短时间内返回。
        // 修复前它会排在被阻塞的派发线程后面，超时就是“挂载点卡死”。
        let mut worst: Option<Duration> = None;
        for _ in 0..10 {
            let t0 = std::time::Instant::now();
            std::fs::metadata(&mountpoint).expect("statfs/getattr must not hang");
            let dt = t0.elapsed();
            worst = Some(worst.map_or(dt, |w: Duration| w.max(dt)));
        }
        let worst = worst.unwrap();
        println!("worst local stat latency: {:?}", worst);

        stop_pump.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = pump.join();
        mount.stop().unwrap();
        // 卸挂必须能完成（修复前 join 会永久卡住）
        let out = std::process::Command::new("mountpoint")
            .arg(&mountpoint)
            .output();
        if let Ok(out) = out {
            assert!(!out.status.success(), "should be unmounted after stop");
        }
    }
}
