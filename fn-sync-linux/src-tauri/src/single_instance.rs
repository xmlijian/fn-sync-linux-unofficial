//! 单实例保护。
//!
//! 两个实例同时运行会让按需同步互相破坏：第二个抢挂载点失败
//! （`os error 17` 挂载点已存在），并且可能清理/重建共享缓存，
//! 把第一个实例的本地文件状态与待上传队列一起清掉。
//!
//! 用文件锁（`flock` + `LOCK_EX|LOCK_NB`）实现：
//! - 锁文件放在应用数据目录，不随挂载点变化
//! - 进程退出/崩溃时内核自动释放锁，不会留下需要手工清理的陈旧状态
//! - 锁文件里写入 PID，便于提示用户“已有实例在运行”

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

/// 尝试获取单实例锁。
///
/// 返回 `Some(已有实例的 PID)` 表示已有实例在运行，调用方应直接退出；
/// 返回 `None` 表示本次进程持有了锁（锁在进程生命周期内一直有效）。
pub fn acquire() -> Option<String> {
    let dir = crate::platform::app_data_dir().ok()?;
    let path = dir.join("app.lock");
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .ok()?;

    // 非阻塞独占锁：失败即说明别的实例还活着。
    if !try_lock_exclusive(&file) {
        let mut pid = String::new();
        let _ = file.read_to_string(&mut pid);
        let pid = pid.trim().to_string();
        return Some(if pid.is_empty() { "未知".into() } else { pid });
    }

    // 拿到锁：写入本进程 PID 供后续诊断。
    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();

    // 锁靠文件描述符存活；进程不退出就一直持有（drop 时会释放，
    // 因此这里用 forget 让它跟随进程生命周期）。
    std::mem::forget(file);
    None
}

#[cfg(unix)]
fn try_lock_exclusive(file: &std::fs::File) -> bool {
    use std::os::unix::io::AsRawFd;
    // SAFETY: fd 来自仍然存活的 File。
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

#[cfg(not(unix))]
fn try_lock_exclusive(_file: &std::fs::File) -> bool {
    true
}
