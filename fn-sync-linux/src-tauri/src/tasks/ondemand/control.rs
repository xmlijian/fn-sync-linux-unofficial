//! Local control never traverses the FUSE mount or modifies cache from another process.
use super::{
    cache::CacheMgr,
    fs::{Shared, ROOT_INO},
};
use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    action: String,
    #[serde(default)]
    path: String,
}

pub fn start(shared: Arc<Shared>) -> Result<()> {
    let path = shared.cache.root().join("control.sock");
    // Cache directory belongs to this user; stale socket from the prior mount is safe to replace.
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let slots = Arc::new(tokio::sync::Semaphore::new(16));
    shared.rt.clone().spawn(async move {
        while !shared.stopping.load(Ordering::Relaxed) {
            let accepted = tokio::select! {
                result = listener.accept() => result,
                _ = tokio::time::sleep(Duration::from_millis(200)) => continue,
            };
            if let Ok((stream, _)) = accepted {
                if let Ok(slot) = slots.clone().try_acquire_owned() {
                    let s = shared.clone();
                    tokio::spawn(async move {
                        let _slot = slot;
                        let _ = serve(stream, s).await;
                    });
                }
            }
        }
        let _ = std::fs::remove_file(path);
    });
    Ok(())
}

async fn serve(stream: UnixStream, shared: Arc<Shared>) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let read = BufReader::new(read).take(8193);
    let mut read = BufReader::new(read);
    tokio::time::timeout(Duration::from_secs(2), read.read_line(&mut line)).await??;
    let response = if line.len() > 8192 {
        json!({"ok":false,"error":"request too large"})
    } else {
        match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                match tokio::time::timeout(Duration::from_secs(60), handle(&shared, request))
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("控制操作超时")))
                {
                    Ok(value) => json!({"ok":true,"result":value}),
                    Err(error) => json!({"ok":false,"error":error.to_string()}),
                }
            }
            Err(error) => json!({"ok":false,"error":error.to_string()}),
        }
    };
    tokio::time::timeout(
        Duration::from_secs(2),
        write.write_all(format!("{response}\n").as_bytes()),
    )
    .await??;
    Ok(())
}

async fn resolve(shared: &Shared, rel: &str) -> Result<u64> {
    let mut ino = ROOT_INO;
    for component in rel.split('/').filter(|c| !c.is_empty()) {
        ino = shared
            .resolve_child(ino, std::ffi::OsStr::new(component))
            .await?
            .ok_or_else(|| anyhow!("文件不存在"))?;
    }
    Ok(ino)
}

pub fn snapshot(shared: &Shared) -> Value {
    let files: Vec<Value> = shared
        .inodes
        .iter()
        .filter_map(|entry| {
            let node = entry.try_read()?;
            if node.rel.is_empty() {
                return None;
            }
            let transfer = shared.transfers.get(&node.rel);
            let (done, total, progress, speed, eta) = match transfer.as_ref() {
                Some(t) => {
                    let (speed, eta) = t.speed_and_eta();
                    (
                        t.done.load(Ordering::Relaxed),
                        t.total,
                        t.percent(),
                        speed,
                        eta,
                    )
                }
                None => (0, node.size, 0, 0, None),
            };
            Some(
                json!({"path":node.rel,"state":shared.file_state(&node),"size":node.size,
            "pinned":shared.pins.contains_key(&node.rel),
            "done":done,"total":total,"progress":progress,
            "speed":speed,"eta":eta,
            "error":shared.errors.get(&node.rel).map(|e|e.value().clone())}),
            )
        })
        .collect();
    json!({"mountpoint":shared.cfg.mountpoint,"task_id":shared.cfg.task_id,
        "stopping":shared.stopping.load(Ordering::Relaxed),"files":files})
}

async fn handle(shared: &Arc<Shared>, request: Request) -> Result<Value> {
    if request.action == "status" {
        return Ok(snapshot(shared));
    }
    if shared.stopping.load(Ordering::Relaxed) {
        anyhow::bail!("挂载正在停止");
    }
    let _command_slot = shared
        .control_slots
        .try_acquire()
        .map_err(|_| anyhow!("控制操作繁忙，请稍后重试"))?;
    if request.action == "list" {
        let rel = CacheMgr::sanitize_rel(&request.path)?;
        let root = tokio::time::timeout(Duration::from_secs(20), resolve(shared, &rel)).await??;
        let mut stack = vec![root];
        let mut files = Vec::new();
        while let Some(ino) = stack.pop() {
            if files.len() + stack.len() > 10000 {
                anyhow::bail!("目录过大，请选择子目录");
            }
            let node = shared
                .inodes
                .get(&ino)
                .ok_or_else(|| anyhow!("文件不存在"))?
                .read()
                .clone();
            if node.kind != fuser::FileType::Directory {
                files.push(node.rel);
                continue;
            }
            for entry in shared.list_dir(ino).await? {
                if let Some(child) = shared
                    .resolve_child(ino, std::ffi::OsStr::new(&entry.name))
                    .await?
                {
                    stack.push(child);
                }
            }
        }
        return Ok(json!({"files":files}));
    }
    if request.path.starts_with('/') || request.path.is_empty() {
        anyhow::bail!("需要有效的相对文件路径");
    }
    let rel = CacheMgr::sanitize_rel(&request.path)?;
    if !["download", "free", "pin", "unpin"].contains(&request.action.as_str()) {
        anyhow::bail!("未知操作");
    }
    let ino = tokio::time::timeout(Duration::from_secs(20), resolve(shared, &rel)).await??;
    if shared
        .inodes
        .get(&ino)
        .map(|n| n.read().kind == fuser::FileType::Directory)
        .unwrap_or(true)
    {
        anyhow::bail!("请选择文件；目录批量操作需逐文件执行");
    }
    match request.action.as_str() {
        "free" => {
            if shared.releasing.contains_key(&rel) {
                return Ok(json!({"accepted":true,"path":rel}));
            }
            // 释放本身只是 unlink，通常瞬时；但「被占用/有修改/已保留」的失败
            // 必须能通过状态暴露给 CLI（通知横幅可能被系统关闭）。
            shared.releasing.insert(rel.clone(), ());
            shared.errors.remove(&rel);
            let s = shared.clone();
            let name = rel.clone();
            if !shared.transfer_ops.submit(
                "control-free",
                Box::new(move || {
                    if let Err(error) = s.release(&name) {
                        s.errors.insert(name.clone(), error.to_string());
                    }
                    s.releasing.remove(&name);
                    if let Some(ino) = s.ino_by_rel.get(&name).map(|v| *v) {
                        s.notify_inode(ino);
                    }
                }),
            ) {
                shared.releasing.remove(&rel);
                anyhow::bail!("传输队列已满，请稍后再试");
            }
            return Ok(json!({"accepted":true,"path":rel}));
        }
        "unpin" => {
            shared.errors.remove(&rel);
            shared.set_pin(&rel, false)?;
        }
        "download" | "pin" => {
            if request.action == "pin" {
                shared.errors.remove(&rel);
                shared.set_pin(&rel, true)?;
            }
            if shared.queued_downloads.contains_key(&rel) || shared.transfers.contains_key(&rel) {
                return Ok(json!({"accepted":true,"path":rel}));
            }
            shared.errors.remove(&rel);
            shared.queued_downloads.insert(rel.clone(), ());
            let s = shared.clone();
            let name = rel.clone();
            if !shared.transfer_ops.submit(
                "control-download",
                Box::new(move || {
                    if let Err(error) = s.rt.block_on(s.hydrate(ino)) {
                        s.errors.insert(name.clone(), error.to_string());
                    }
                    s.queued_downloads.remove(&name);
                }),
            ) {
                shared.queued_downloads.remove(&rel);
                anyhow::bail!("传输队列已满，请稍后再试");
            }
            return Ok(json!({"accepted":true,"path":rel}));
        }
        _ => unreachable!(),
    }
    Ok(json!({"path":rel,"completed":true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn control_request_rejects_unknown_fields() {
        assert!(
            serde_json::from_str::<Request>(r#"{"action":"free","path":"a","cache":"/tmp"}"#)
                .is_err()
        );
    }
    #[test]
    fn control_relative_paths_cannot_escape() {
        assert!(CacheMgr::sanitize_rel("a/../../secret").is_err());
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::{
        service::database,
        tasks::{
            file_filter::FileFilter,
            ondemand::{remote::fake::FakeNas, OnDemandConfig},
        },
    };
    async fn fixture() -> (tempfile::TempDir, Arc<Shared>) {
        let tmp = tempfile::tempdir().unwrap();
        let nas = Arc::new(FakeNas::new());
        nas.seed_dir("root");
        nas.seed("root/a.txt", b"hello");
        let db = database::open_db(&tmp.path().join("db.sqlite"))
            .await
            .unwrap();
        database::migrate(&db).await.unwrap();
        let cfg = OnDemandConfig {
            task_id: 1,
            task_name: "test".into(),
            mountpoint: tmp.path().join("mount"),
            nas_root: "root".into(),
            cache_root: tmp.path().join("cache"),
            cache_limit_bytes: 1024,
        };
        let shared = Arc::new(
            Shared::new(
                cfg,
                nas,
                tokio::runtime::Handle::current(),
                db,
                FileFilter::from_db(None),
            )
            .unwrap(),
        );
        (tmp, shared)
    }
    async fn rpc(shared: &Shared, action: &str, path: &str) -> Value {
        let mut stream = UnixStream::connect(shared.cache.root().join("control.sock"))
            .await
            .unwrap();
        stream
            .write_all(format!("{}\n", json!({"action":action,"path":path})).as_bytes())
            .await
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn socket_download_progress_release_and_pin_guards() {
        let (_tmp, shared) = fixture().await;
        start(shared.clone()).unwrap();
        assert_eq!(
            std::fs::metadata(shared.cache.root().join("control.sock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            rpc(&shared, "list", "").await["result"]["files"],
            json!(["a.txt"])
        );
        assert_eq!(rpc(&shared, "download", "a.txt").await["ok"], true);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let state = rpc(&shared, "status", "").await;
                if state["result"]["files"][0]["state"] == "cached" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(rpc(&shared, "pin", "a.txt").await["ok"], true);
        // 释放改为异步执行：pinned 的拒绝要通过状态里的 error 暴露。
        // pin 会对已缓存文件再做一次 hydrate 检查、短暂持有缓存锁，因此这里
        // 允许重试：只有确认是“始终保留”的拒绝才算数。
        let mut pinned_error = String::new();
        for _ in 0..50 {
            assert_eq!(rpc(&shared, "free", "a.txt").await["ok"], true);
            pinned_error = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let state = rpc(&shared, "status", "").await;
                    if let Some(err) = state["result"]["files"][0]["error"].as_str() {
                        break err.to_string();
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            if pinned_error.contains("始终保留") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(pinned_error.contains("始终保留"), "{pinned_error}");
        assert!(shared.cache.file_path("a.txt").exists());
        assert_eq!(rpc(&shared, "unpin", "a.txt").await["ok"], true);
        let mut released = false;
        for _ in 0..50 {
            assert_eq!(rpc(&shared, "free", "a.txt").await["ok"], true);
            let outcome = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let state = rpc(&shared, "status", "").await;
                    let file = &state["result"]["files"][0];
                    if let Some(err) = file["error"].as_str() {
                        break Err(err.to_string());
                    }
                    if file["state"] == "placeholder" {
                        break Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            match outcome {
                Ok(()) => {
                    released = true;
                    break;
                }
                Err(err) if err.contains("正在传输") => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(err) => panic!("{err}"),
            }
        }
        assert!(released, "file never reached placeholder");
        assert!(!shared.cache.file_path("a.txt").exists());
        assert_eq!(
            rpc(&shared, "status", "").await["result"]["files"][0]["state"],
            "placeholder"
        );
        assert_eq!(rpc(&shared, "free", "../secret").await["ok"], false);
        shared.stopping.store(true, Ordering::Relaxed);
        assert_eq!(rpc(&shared, "download", "a.txt").await["ok"], false);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_reports_speed_and_eta() {
        let (_tmp, shared) = fixture().await;
        let _ = resolve(&shared, "a.txt").await.unwrap();
        shared.transfers.insert(
            "a.txt".into(),
            crate::tasks::ondemand::fs::Transfer {
                kind: "download",
                total: 1000,
                done: Arc::new(std::sync::atomic::AtomicU64::new(500)),
                started: std::time::Instant::now() - Duration::from_secs(2),
            },
        );
        let snap = snapshot(&shared);
        let file = snap["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == "a.txt")
            .unwrap();
        assert_eq!(file["state"], "download");
        let speed = file["speed"].as_u64().unwrap();
        assert!((240..=260).contains(&speed), "speed={speed}");
        assert_eq!(file["eta"].as_u64(), Some(2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn release_refuses_locked_and_dirty_content() {
        let (_tmp, shared) = fixture().await;
        let ino = resolve(&shared, "a.txt").await.unwrap();
        shared.hydrate(ino).await.unwrap();
        let lock = shared.cache.lock_for("a.txt");
        let guard = lock.lock().await;
        assert!(shared.release("a.txt").is_err());
        assert!(shared.cache.file_path("a.txt").exists());
        drop(guard);
        shared.cache.update("a.txt", |state| state.dirty = true);
        assert!(shared.release("a.txt").is_err());
        assert!(shared.cache.file_path("a.txt").exists());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resizing_placeholder_preserves_existing_prefix() {
        let (_tmp, shared) = fixture().await;
        let ino = resolve(&shared, "a.txt").await.unwrap();
        assert!(!shared.cache.file_path("a.txt").exists());
        shared.resize_content(ino, 8).await.unwrap();
        assert_eq!(
            std::fs::read(shared.cache.file_path("a.txt")).unwrap(),
            b"hello\0\0\0"
        );
        assert!(shared.cache.state("a.txt").unwrap().dirty);
        assert!(shared.release("a.txt").is_err());
        shared.resize_content(ino, 0).await.unwrap();
        assert_eq!(
            std::fs::metadata(shared.cache.file_path("a.txt"))
                .unwrap()
                .len(),
            0
        );
    }
}

#[cfg(test)]
mod large_placeholder_tests {
    use super::*;
    use crate::{
        connect::nas_client::RemoteEntry,
        service::database,
        tasks::{
            file_filter::FileFilter,
            ondemand::{remote::RemoteOps, OnDemandConfig},
        },
    };
    use futures_util::future::BoxFuture;
    use std::sync::atomic::AtomicU64;
    struct RangeOnlyNas {
        bytes: AtomicU64,
        full: AtomicU64,
    }
    impl RemoteOps for RangeOnlyNas {
        fn ls<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Vec<RemoteEntry>>> {
            Box::pin(async {
                Ok(vec![RemoteEntry {
                    name: "large.iso".into(),
                    size: 4 * 1024 * 1024 * 1024,
                    mtim: 1,
                    ..Default::default()
                }])
            })
        }
        fn mkdir<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { anyhow::bail!("unexpected mutation") })
        }
        fn remove<'a>(&'a self, _: &'a [String]) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { anyhow::bail!("unexpected mutation") })
        }
        fn rename<'a>(&'a self, _: &'a str, _: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { anyhow::bail!("unexpected mutation") })
        }
        fn download<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async {
                self.full.fetch_add(1, Ordering::Relaxed);
                anyhow::bail!("full download forbidden")
            })
        }
        fn download_to<'a>(
            &'a self,
            _: &'a str,
            _: &'a std::path::Path,
            _: Arc<dyn Fn(u64) + Send + Sync>,
        ) -> BoxFuture<'a, Result<u64>> {
            Box::pin(async {
                self.full.fetch_add(1, Ordering::Relaxed);
                anyhow::bail!("full download forbidden")
            })
        }
        fn upload<'a>(
            &'a self,
            _: &'a str,
            _: Vec<u8>,
            _: Option<i64>,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { anyhow::bail!("unexpected mutation") })
        }
        fn read_range<'a>(
            &'a self,
            _: &'a str,
            offset: u64,
            size: usize,
        ) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async move {
                self.bytes.fetch_add(size as u64, Ordering::Relaxed);
                Ok((offset..offset + size as u64)
                    .map(|n| (n % 251) as u8)
                    .collect())
            })
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn four_gib_mime_probes_never_hydrate_or_recreate_released_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let nas = Arc::new(RangeOnlyNas {
            bytes: AtomicU64::new(0),
            full: AtomicU64::new(0),
        });
        let db = database::open_db(&tmp.path().join("db.sqlite"))
            .await
            .unwrap();
        database::migrate(&db).await.unwrap();
        let shared = Shared::new(
            OnDemandConfig {
                task_id: 1,
                task_name: "range-test".into(),
                mountpoint: tmp.path().join("mount"),
                nas_root: "root".into(),
                cache_root: tmp.path().join("cache"),
                cache_limit_bytes: 1024 * 1024,
            },
            nas.clone(),
            tokio::runtime::Handle::current(),
            db,
            FileFilter::from_db(None),
        )
        .unwrap();
        let ino = shared
            .resolve_child(ROOT_INO, std::ffi::OsStr::new("large.iso"))
            .await
            .unwrap()
            .unwrap();
        // Simulate a cached ISO that was released, then Nautilus/GIO sniffing several offsets.
        let cached = std::fs::File::create(shared.cache.file_path("large.iso")).unwrap();
        cached.set_len(4 * 1024 * 1024 * 1024).unwrap();
        drop(cached);
        shared.release("large.iso").unwrap();
        for (offset, size) in [(0, 512), (32768, 2048), (65536, 131072)] {
            let data = shared.read_placeholder(ino, offset, size).await.unwrap();
            assert_eq!(data.len(), size as usize);
            assert_eq!(data[0], (offset % 251) as u8);
        }
        assert_eq!(nas.full.load(Ordering::Relaxed), 0);
        assert_eq!(nas.bytes.load(Ordering::Relaxed), 256 * 1024);
        assert!(!shared.cache.file_path("large.iso").exists());
        assert_eq!(
            shared.file_state(&shared.inodes.get(&ino).unwrap().read()),
            "placeholder"
        );
        let last = shared
            .read_placeholder(ino, 4 * 1024 * 1024 * 1024 - 512, 4096)
            .await
            .unwrap();
        assert_eq!(last.len(), 512);
        assert_eq!(nas.full.load(Ordering::Relaxed), 0);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires /dev/fuse; fake 4GiB NAS only"]
    async fn fuse_four_gib_mime_reads_remain_placeholder() {
        use crate::tasks::ondemand::OnDemandMount;
        use std::io::{Read, Seek, SeekFrom};
        let tmp = tempfile::tempdir().unwrap();
        let nas = Arc::new(RangeOnlyNas {
            bytes: AtomicU64::new(0),
            full: AtomicU64::new(0),
        });
        let db = database::open_db(&tmp.path().join("db.sqlite"))
            .await
            .unwrap();
        database::migrate(&db).await.unwrap();
        let cfg = OnDemandConfig {
            task_id: 1,
            task_name: "range-fuse".into(),
            mountpoint: tmp.path().join("mount"),
            nas_root: "root".into(),
            cache_root: tmp.path().join("cache"),
            cache_limit_bytes: 1024 * 1024,
        };
        let mount =
            OnDemandMount::start(cfg.clone(), nas.clone(), db, FileFilter::from_db(None)).unwrap();
        let mut file = std::fs::File::open(cfg.mountpoint.join("large.iso")).unwrap();
        for offset in [0, 32768, 65536, 4 * 1024 * 1024 * 1024 - 512] {
            file.seek(SeekFrom::Start(offset)).unwrap();
            let mut bytes = [0; 512];
            file.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes[0], (offset % 251) as u8);
        }
        drop(file);
        assert_eq!(nas.full.load(Ordering::Relaxed), 0);
        assert!(nas.bytes.load(Ordering::Relaxed) <= 512 * 1024);
        assert!(!mount.shared.cache.file_path("large.iso").exists());
        mount.stop().unwrap();
    }
}
