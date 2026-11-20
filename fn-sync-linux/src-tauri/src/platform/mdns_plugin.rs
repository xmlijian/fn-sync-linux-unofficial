//! `plugin:mdns` —— 官方前端使用的 mDNS 发现插件接口。
//!
//! 前端调用：
//!   invoke("plugin:mdns|browse_many", { serviceTypes: ["_trim._tcp.local."] })
//!   invoke("plugin:mdns|stop_browse")
//! 并监听事件：
//!   plugin:mdns:service-resolved-event  { service: {...} }
//!   plugin:mdns:service-removed-event   { instance_name }
//!   plugin:mdns:search-started-event / search-stopped-event
//!
//! service 字段（与插件 crate 保持一致）：instance_name / service_type / hostname /
//! port / addresses / fn_txt / fullname。

use mdns_sd::{ServiceDaemon, ServiceEvent};
use parking_lot::Mutex;
use serde_json::json;
use std::sync::Arc;
use tauri::{Emitter, Manager, Runtime};

struct BrowseState {
    daemon: Option<ServiceDaemon>,
    generation: u64,
}

#[derive(Default)]
pub struct MdnsPluginState(pub Arc<Mutex<BrowseState>>);

impl Default for BrowseState {
    fn default() -> Self {
        Self {
            daemon: None,
            generation: 0,
        }
    }
}

#[tauri::command]
pub async fn browse_many<R: Runtime>(
    app: tauri::AppHandle<R>,
    service_types: Vec<String>,
) -> Result<(), String> {
    let state = app.state::<MdnsPluginState>().0.clone();
    let generation = {
        let mut st = state.lock();
        st.generation += 1;
        st.generation
    };

    // 关闭上一次的 daemon
    if let Some(old) = state.lock().daemon.take() {
        let _ = old.shutdown();
    }

    let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
    for st in &service_types {
        let receiver = daemon.browse(st).map_err(|e| e.to_string())?;
        let app = app.clone();
        let state = state.clone();
        let service_type = st.clone();
        tokio::spawn(async move {
            while let Ok(event) = receiver.recv_async().await {
                if state.lock().generation != generation {
                    break;
                }
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let port = info
                            .get_property_val_str("https")
                            .and_then(|v| v.parse::<u16>().ok())
                            .unwrap_or(info.get_port());
                        let txt = info
                            .get_properties()
                            .iter()
                            .map(|p| format!("{}={}", p.key(), p.val_str()))
                            .collect::<Vec<_>>()
                            .join(" ");
                        let payload = json!({
                            "service": {
                                "instance_name": info.get_fullname(),
                                "fullname": info.get_fullname(),
                                "service_type": service_type,
                                "hostname": info.get_hostname().trim_end_matches('.').to_string(),
                                "port": port,
                                "addresses": info
                                    .get_addresses()
                                    .iter()
                                    .map(|a| a.to_string())
                                    .collect::<Vec<_>>(),
                                "fn_txt": txt,
                            }
                        });
                        let _ = app.emit("plugin:mdns:service-resolved-event", payload);
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        let _ = app.emit(
                            "plugin:mdns:service-removed-event",
                            json!({ "instance_name": fullname, "service_type": service_type }),
                        );
                    }
                    ServiceEvent::SearchStarted(st) => {
                        let _ = app.emit(
                            "plugin:mdns:search-started-event",
                            json!({ "service_type": st }),
                        );
                    }
                    ServiceEvent::SearchStopped(st) => {
                        let _ = app.emit(
                            "plugin:mdns:search-stopped-event",
                            json!({ "service_type": st }),
                        );
                    }
                    _ => {}
                }
            }
        });
    }
    state.lock().daemon = Some(daemon);
    Ok(())
}

#[tauri::command]
pub async fn stop_browse<R: Runtime>(app: tauri::AppHandle<R>) -> Result<(), String> {
    let state = app.state::<MdnsPluginState>().0.clone();
    let mut st = state.lock();
    st.generation += 1;
    if let Some(daemon) = st.daemon.take() {
        let _ = daemon.shutdown();
    }
    Ok(())
}

pub fn init<R: Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::<R>::new("mdns")
        .invoke_handler(tauri::generate_handler![browse_many, stop_browse])
        .build()
}
