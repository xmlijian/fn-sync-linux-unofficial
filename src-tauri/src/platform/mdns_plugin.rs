//! `plugin:mdns` —— 官方前端使用的 mDNS 发现插件接口（Tauri v1 适配版）。
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
//!
//! v1 差异：v1 插件无 `invoke_handler(generate_handler![..])` 宏机制，改为手动
//! 解析 Invoke 参数；browse/stop 的业务逻辑与 v2 版完全一致。

use mdns_sd::{ServiceDaemon, ServiceEvent};
use parking_lot::Mutex;
use serde_json::json;
use std::sync::Arc;
use tauri::{AppHandle, Manager, Runtime};

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

pub async fn browse_many<R: Runtime>(
    app: AppHandle<R>,
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
                        // v1: Manager::emit_all 等价 v2 的 app.emit 广播
                        let _ = app.emit_all("plugin:mdns:service-resolved-event", payload);
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        let _ = app.emit_all(
                            "plugin:mdns:service-removed-event",
                            json!({ "instance_name": fullname, "service_type": service_type }),
                        );
                    }
                    ServiceEvent::SearchStarted(st) => {
                        let _ = app.emit_all(
                            "plugin:mdns:search-started-event",
                            json!({ "service_type": st }),
                        );
                    }
                    ServiceEvent::SearchStopped(st) => {
                        let _ = app.emit_all(
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

pub async fn stop_browse<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    let state = app.state::<MdnsPluginState>().0.clone();
    let mut st = state.lock();
    st.generation += 1;
    if let Some(daemon) = st.daemon.take() {
        let _ = daemon.shutdown();
    }
    Ok(())
}

/// v1 插件：手动匹配 `plugin:mdns|browse_many` / `plugin:mdns|stop_browse`。
/// 未识别的命令返回 false，交回主 invoke handler（保持 v1 插件路由语义）。
pub fn init<R: Runtime>() -> tauri::plugin::TauriPlugin<R> {
    // v1.8：插件 invoke_handler 收 owned Invoke（无返回值），
    // 路由按 plugin:mdns 前缀分发到本插件；AppHandle 从 invoke.message.window() 取。
    tauri::plugin::Builder::<R>::new("mdns")
        .invoke_handler(|invoke| {
            let cmd = invoke.message.command().to_string();
            let args: serde_json::Value = invoke.message.payload().clone();
            let app = invoke.message.window().app_handle().clone();
            let resolver = invoke.resolver;
            match cmd.as_str() {
                "plugin:mdns|browse_many" => {
                    // 前端传 { serviceTypes: [...] }（camelCase）；兼容 snake_case 写法
                    let service_types = args
                        .get("serviceTypes")
                        .or_else(|| args.get("service_types"))
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_owned))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    tauri::async_runtime::spawn(async move {
                        match browse_many(app, service_types).await {
                            Ok(_) => resolver.resolve(json!(null)),
                            Err(e) => resolver.reject(e),
                        }
                    });
                }
                "plugin:mdns|stop_browse" => {
                    tauri::async_runtime::spawn(async move {
                        match stop_browse(app).await {
                            Ok(_) => resolver.resolve(json!(null)),
                            Err(e) => resolver.reject(e),
                        }
                    });
                }
                other => {
                    resolver.reject(format!("unknown mdns command: {other}"));
                }
            }
        })
        .build()
}
