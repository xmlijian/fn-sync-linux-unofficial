//! 平台相关：Linux 自启动（~/.config/autostart）、网络监控、应用目录。

use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn app_data_dir() -> Result<PathBuf> {
    let base = dirs::data_local_dir().context("no local data dir")?;
    let dir = base.join("fn-sync-client");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn app_log_dir() -> Result<PathBuf> {
    let dir = app_data_dir()?.join("logs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 开机自启：写 ~/.config/autostart/fn-sync-client.desktop（官方 settings.autoStart 语义）。
pub fn set_autostart(enabled: bool) -> Result<()> {
    let config_dir = dirs::config_dir().context("no config dir")?;
    let desktop = config_dir.join("autostart/fn-sync-client.desktop");
    set_autostart_file(
        &desktop,
        enabled,
        &std::env::current_exe().context("current exe")?,
    )
}

fn set_autostart_file(
    desktop: &std::path::Path,
    enabled: bool,
    exe: &std::path::Path,
) -> Result<()> {
    if enabled {
        std::fs::create_dir_all(desktop.parent().context("invalid autostart path")?)?;
        let content=format!("[Desktop Entry]\nType=Application\nName=飞牛同步\nExec={} --autostart\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",exe.display());
        std::fs::write(desktop, content)?;
    } else if let Err(error) = std::fs::remove_file(desktop) {
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error.into());
        }
    }
    Ok(())
}

pub fn is_autostart_enabled() -> bool {
    dirs::config_dir()
        .map(|c| c.join("autostart/fn-sync-client.desktop").exists())
        .unwrap_or(false)
}

/// 网络状态监控（官方 network_restart 语义）：监听 netlink 路由变化 → 触发重连。
/// 首版实现为周期探测默认路由，变化时回调。
pub async fn spawn_network_monitor(mut on_change: impl FnMut() + Send + 'static) {
    tokio::spawn(async move {
        let mut last_route = current_default_route();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let route = current_default_route();
            if route != last_route {
                last_route = route;
                on_change();
            }
        }
    });
}

/// 读取默认路由接口（/proc/net/route）。
fn current_default_route() -> Option<String> {
    let content = std::fs::read_to_string("/proc/net/route").ok()?;
    for line in content.lines().skip(1) {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() >= 8 && cols[1] == "00000000" && cols[7] == "00000003" {
            return Some(cols[0].to_string());
        }
    }
    None
}

/// mDNS 发现 NAS 设备（_trim._tcp.local.，TXT https=<port>，默认 5667）。
pub async fn mdns_discover() -> Result<Vec<serde_json::Value>> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};
    let mdns = ServiceDaemon::new().map_err(|e| anyhow::anyhow!("mdns daemon: {e}"))?;
    let receiver = mdns
        .browse("_trim._tcp.local.")
        .map_err(|e| anyhow::anyhow!("mdns browse: {e}"))?;
    let mut devices = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let timeout = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(timeout, receiver.recv_async()).await {
            Ok(Ok(ServiceEvent::ServiceResolved(info))) => {
                let port = info
                    .get_property_val_str("https")
                    .and_then(|v| v.parse::<u16>().ok())
                    .unwrap_or(5667);
                let host = info.get_hostname().trim_end_matches('.').to_string();
                let ips: Vec<String> = info.get_addresses().iter().map(|a| a.to_string()).collect();
                devices.push(serde_json::json!({
                    "name": info.get_fullname(),
                    "host": host,
                    "port": port,
                    "ips": ips,
                }));
                if devices.len() >= 20 {
                    break;
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => {}
            Err(_) => {} // timeout
        }
    }
    let _ = mdns.stop_browse("_trim._tcp.local.");
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_toggle() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = dir.path().join("autostart/fn-sync-client.desktop");
        set_autostart_file(
            &desktop,
            true,
            std::path::Path::new("/usr/bin/fn-sync-client"),
        )
        .unwrap();
        assert!(std::fs::read_to_string(&desktop)
            .unwrap()
            .contains("Exec=/usr/bin/fn-sync-client --autostart"));
        set_autostart_file(
            &desktop,
            false,
            std::path::Path::new("/usr/bin/fn-sync-client"),
        )
        .unwrap();
        assert!(!desktop.exists());
    }

    #[test]
    fn default_route_readable() {
        // Linux 环境下 /proc/net/route 存在（可能无默认路由）
        let _ = current_default_route();
    }
}
