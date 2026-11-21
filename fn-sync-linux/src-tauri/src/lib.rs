//! 库入口 — Tauri 应用（lib 模式，Tauri v2 标准）。命令与状态在此注册。

pub mod commands;
pub mod connect;
pub mod platform;
pub mod service;
pub mod task_manager;
pub mod tasks;
pub mod traffic;

use parking_lot::Mutex as StdMutex;
use serde_json::Value;
use std::sync::Arc;
use task_manager::TaskManager;
use tauri::Manager;

pub const GIT_HASH: &str = env!("GIT_HASH");

/// 持有 tracing 的 WorkerGuard，保证进程退出前日志落盘。
static LOG_GUARD: std::sync::OnceLock<tracing_appender::non_blocking::WorkerGuard> =
    std::sync::OnceLock::new();

pub struct AppState {
    pub db: sqlx::SqlitePool,
    pub task_manager: tokio::sync::Mutex<TaskManager>,
    pub last_auth_result: StdMutex<Option<Value>>,
    /// 官方登录代理（`async-oauth`）：窗口存活期间持有
    pub oauth_proxy:
        StdMutex<Option<Arc<tokio::sync::Mutex<Option<crate::connect::oauth_proxy::OauthProxy>>>>>,
    pub traffic: crate::traffic::TrafficCounter,
}

impl AppState {
    pub async fn new() -> Result<Self, anyhow::Error> {
        let dir = platform::app_data_dir()?;
        let db = service::database::open_db(&dir.join("database.sqlite")).await?;
        service::database::migrate(&db).await?;
        #[cfg(target_os = "linux")]
        sqlx::query("UPDATE tasks SET open_cloud=0 WHERE open_cloud<>0")
            .execute(&db)
            .await?;
        for key in ["global_upload_speed_limit", "global_download_speed_limit"] {
            if let Some(value) = service::logs::get_system_value(&db, key).await? {
                crate::traffic::set_limit(key, &value);
            }
        }
        // 日志库独立文件（log.sqlite），与官方一致共享 schema
        let log_db = service::database::open_db(&dir.join("log.sqlite")).await?;
        service::database::migrate(&log_db).await?;
        Ok(Self {
            db,
            task_manager: tokio::sync::Mutex::new(TaskManager::new()),
            last_auth_result: StdMutex::new(None),
            oauth_proxy: StdMutex::new(None),
            traffic: crate::traffic::TrafficCounter::new(),
        })
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(crate::platform::mdns_plugin::init())
        .manage(crate::platform::mdns_plugin::MdnsPluginState::default())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            // 官方客户端可接受 NAS 自签证书（WebView2: dangerousInsecureTransportProtocol；
            // macOS: 同等策略）。webkit2gtk 上等价做法是设置全局 TLS 错误策略为 Ignore。
            // 必须在任何 webview 创建之前执行。
            #[cfg(target_os = "linux")]
            setup_linux_tls_policy();

            // 自检模式：验证 webview 能加载自签证书的 HTTPS 页面。
            // 用法: fn-sync-client --selftest-url https://127.0.0.1:5667/c/oauth
            if let Some(url) = selftest_url() {
                return run_selftest(app, &url);
            }

            // mDNS 在界面的 invoke 包装函数中转发；这里仅收集页面异常。
            if let Some(win) = {
                use tauri::Manager;
                app.get_webview_window("main")
            } {
                let _ = win.eval(
                    r#"(function(){
  const I = window.__TAURI_INTERNALS__;
  if (!I || I.__mdnsShim) return;
  I.__mdnsShim = true;
  const orig = I.invoke.bind(I);
  const log = (lvl, msg) => { try { orig('js_log', { level: lvl, message: String(msg).slice(0, 600) }); } catch(_){} };
  window.addEventListener('error', e => log('js-error', (e.message||'') + ' @ ' + (e.filename||'') + ':' + (e.lineno||'')));
  window.addEventListener('unhandledrejection', e => log('js-reject', (e.reason && (e.reason.stack || e.reason)) || 'unknown'));
  log('boot', 'shim installed, readyState=' + document.readyState);
})();"#,
                );
            }

            let handle = app.handle().clone();
            tauri::async_runtime::block_on(async move {
                let state = AppState::new().await.expect("app state init");
                handle.manage(state);
                tauri::async_runtime::spawn(async move {
                    use tauri::Manager;
                    let state = handle.state::<AppState>();
                    if let Ok(tasks) = service::tasks::get_all_tasks(&state.db).await {
                        for task in tasks {
                            if task.status == tasks::actor::status::PAUSED { continue; }
                            if let Ok(Some(acc)) = service::nas_account::get_nas_account_by_id(&state.db, task.nas_acc_id).await {
                                if let Err(e) = state.task_manager.lock().await.start_task(&state.db, task.id, &acc).await {
                                    tracing::warn!(task_id=task.id,error=%e,"saved task restore failed");
                                    let _ = service::tasks::update_task_status(&state.db,task.id,tasks::actor::status::FAILED).await;
                                }
                            }
                        }
                    }
                });
            });
            setup_tray(app.handle())?;
            // 文件日志（应用数据目录/logs/fn-sync.log）
            // 注意：WorkerGuard 必须存活到进程结束，否则缓冲日志永不落盘。
            if let Ok(log_dir) = platform::app_log_dir() {
                let file_appender = tracing_appender::rolling::daily(&log_dir, "fn-sync.log");
                let (writer, guard) = tracing_appender::non_blocking(file_appender);
                // 先尝试初始化（tauri 可能已设置 subscriber）；无论成败都保住 guard
                let installed = tracing_subscriber::fmt()
                    .with_writer(writer)
                    .with_env_filter(
                        tracing_subscriber::EnvFilter::try_from_default_env()
                            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                    )
                    .try_init()
                    .is_ok();
                if !installed {
                    eprintln!("warning: tracing subscriber already set; file logging disabled");
                }
                if LOG_GUARD.set(guard).is_err() {
                    eprintln!("warning: log guard already stored");
                }
            }
            // 网络监控：默认路由变化 → 提示任务重连（官方 network_restart 语义）
            tauri::async_runtime::spawn(async {
                platform::spawn_network_monitor(|| {
                    tracing::info!("network route changed, tasks will reconnect");
                })
                .await;
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // accounts
            commands::add_nas_account,
            commands::get_all_nas_accounts,
            commands::get_nas_account_by_id,
            commands::get_nas_account_by_nas_id,
            commands::get_nas_account_by_address_and_username,
            commands::update_nas_account_by_auth,
            commands::update_nas_conn_type_by_id,
            commands::update_nas_account_password,
            commands::delete_nas_account,
            // tasks
            commands::add_task,
            commands::get_all_tasks,
            commands::get_task_by_id,
            commands::get_task_count,
            commands::get_next_task_id,
            commands::update_task,
            commands::update_task_status,
            commands::delete_task,
            commands::pause_task,
            commands::resume_task,
            commands::reload_task,
            commands::get_task_status_summary,
            commands::get_global_tasks_status,
            commands::get_filter_rule_by_task_id,
            commands::save_folder_filter_paths,
            commands::get_folder_filter_paths,
            commands::validate_app_name,
            // logs
            commands::query_logs,
            commands::get_logs_by_size,
            commands::clear_all_logs,
            commands::get_extended_logs,
            commands::get_completed_items,
            // system
            commands::get_all_system_table,
            commands::update_system_table,
            commands::get_app_log_dir,
            commands::export_log_file,
            commands::get_git_hash,
            commands::get_device_id,
            commands::get_own_pid,
            commands::app_quit,
            commands::app_restart,
            commands::set_autostart_state,
            commands::get_autostart_state,
            commands::is_webview_debug,
            commands::get_traffic_query,
            // nas connect
            commands::connect_nas,
            commands::check_trim_address,
            commands::query_nas_os_edition,
            commands::create_nas_client,
            commands::delete_nas_client,
            commands::send_message_from_connection_id,
            commands::get_addr_type,
            commands::validate_task_nas_dir_exists,
            commands::is_path_sync_root_command,
            commands::is_external_device_path,
            commands::cmd_set_first_login_state,
            commands::open_oauth_window,
            commands::put_auth_result,
            commands::login_nas,
            // 官方前端所需的其余命令
            commands::add_file_domain,
            commands::add_log,
            commands::check_oauth_security_code,
            commands::clear_all_data_by_task_id,
            commands::close_oauth_window,
            commands::cmd_get_folder_layer,
            commands::cmd_get_folder_layer_with_params,
            commands::cmd_get_folder_list,
            commands::cmd_get_folder_list_with_params,
            commands::delete_folder_filter_file,
            commands::delete_temp_conn,
            commands::edit_open_cloud,
            commands::get_encrypt_data,
            commands::get_macos_cloud_pc_path,
            commands::get_nodes_with_null_field,
            commands::get_tasks_id_and_status_by_nas_acc_id,
            commands::list_file_domains,
            commands::network_restart,
            commands::open_browser,
            commands::open_window,
            commands::remove_file_domain,
            commands::remove_tasks_from_map,
            commands::renew_task,
            commands::reveal_item_in_dir,
            commands::send_message_from_temp_conn,
            commands::set_sync_sdk_locale,
            commands::mdns_browse_many,
            commands::mdns_stop_browse,
            commands::js_log,
            commands::connect_nas_by_auth,
            // mdns
            commands::mdns_discover_devices,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn setup_tray(app: &tauri::AppHandle) -> Result<(), tauri::Error> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::TrayIconBuilder;

    let show = MenuItem::with_id(app, "show", "打开飞牛同步", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    let _tray = TrayIconBuilder::with_id("main-tray")
        .menu(&menu)
        .tooltip("飞牛同步")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// 将 webkit2gtk 全局 WebContext 的 TLS 错误策略设为 Ignore。
/// 等价于官方客户端的 is_insecure_tls / dangerousInsecureTransportProtocol：
/// fnOS NAS 默认使用自签证书，校验失败会导致 OAuth 页与文件传输不可用。
#[cfg(target_os = "linux")]
#[allow(deprecated)] // set_tls_errors_policy 自 2.32 标记 deprecated，但在 4.1 上仍有效且是全局策略的唯一入口
fn setup_linux_tls_policy() {
    use webkit2gtk::{TLSErrorsPolicy, WebContextExt};
    if let Some(ctx) = webkit2gtk::WebContext::default() {
        ctx.set_tls_errors_policy(TLSErrorsPolicy::Ignore);
        eprintln!(
            "[tls] webkit2gtk TLS errors policy set to Ignore (self-signed NAS certificates)"
        );
    } else {
        eprintln!("[tls] warning: default WebContext unavailable, TLS policy unchanged");
    }
}

/// 解析 --selftest-url 参数。
fn selftest_url() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--selftest-url" {
            return args.next();
        }
        if let Some(v) = a.strip_prefix("--selftest-url=") {
            return Some(v.to_string());
        }
    }
    None
}

/// 自检：创建隐藏 webview 加载 URL，成功则退出 0，失败/超时退出 1。
/// 用于在无 GUI 交互能力的环境下验证 TLS 策略是否真的让自签证书页面可加载。
fn run_selftest(app: &tauri::App, url: &str) -> Result<(), Box<dyn std::error::Error>> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    let target = WebviewUrl::External(tauri::Url::parse(url)?);
    let win = WebviewWindowBuilder::new(app, "selftest", target)
        .title("selftest")
        .visible(false)
        .inner_size(400.0, 300.0)
        .on_page_load(|_webview, payload| {
            use tauri::webview::PageLoadEvent;
            if payload.event() == PageLoadEvent::Finished {
                println!("SELFTEST_OK {}", payload.url());
                std::process::exit(0);
            }
        })
        .build()?;

    // 与 OAuth 窗口一致：在该窗口自身 WebContext 上放开 TLS（NAS 自签证书）
    #[cfg(target_os = "linux")]
    {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        use webkit2gtk::{TLSErrorsPolicy, WebContextExt as _, WebViewExt as _};
        let retries = Arc::new(AtomicU32::new(0));
        let _ = win.with_webview(move |platform| {
            let webview = platform.inner();
            if let Some(ctx) = webview.web_context() {
                #[allow(deprecated)]
                ctx.set_tls_errors_policy(TLSErrorsPolicy::Ignore);
            }
            webview.connect_load_failed_with_tls_errors(move |webview, uri, cert, errs| {
                let n = retries.fetch_add(1, Ordering::SeqCst);
                let host = tauri::Url::parse(uri)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .unwrap_or_default();
                eprintln!(
                    "SELFTEST_TLS_RETRY attempt={} uri={} host={} errs={:?}",
                    n + 1,
                    uri,
                    host,
                    errs
                );
                if n >= 3 {
                    eprintln!("SELFTEST_FAIL tls after 3 retries: {uri}");
                    std::process::exit(1);
                }
                if let Some(ctx) = webview.web_context() {
                    #[allow(deprecated)]
                    ctx.set_tls_errors_policy(TLSErrorsPolicy::Ignore);
                    ctx.allow_tls_certificate_for_host(cert, &host);
                }
                webview.reload();
                true
            });
        });
    }

    // 15 秒超时保护
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(15));
        eprintln!("SELFTEST_FAIL timeout loading url");
        std::process::exit(1);
    });
    Ok(())
}

/// 诊断：对真实 NAS 执行 登录 → 上传 → 下载 → 校验。
/// 成功打印 SELFTEST_OK 并返回 0。
pub async fn selftest_nas(addr: &str, user: &str, pass: &str) -> i32 {
    use connect::nas_client::{NasClient, NasConnConfig};

    let address = if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:5667")
    };
    let config = NasConnConfig {
        address: address.clone(),
        user: user.to_string(),
        password: pass.to_string(),
        device_name: "PcSync".into(),
        verify_tls: false,
    };

    println!("[1/5] 连接并登录 {address} ...");
    let client = match NasClient::connect(config).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SELFTEST_FAIL login: {e}");
            return 1;
        }
    };
    let uid = *client.uid.read();
    println!("      登录成功 uid={uid}");

    println!("[2/5] 读取卷列表 ...");
    match client.ls_dir().await {
        Ok(v) => println!(
            "      卷: {:?}",
            v.iter().map(|e| e.name.clone()).collect::<Vec<_>>()
        ),
        Err(e) => {
            eprintln!("SELFTEST_FAIL lsDir: {e}");
            return 1;
        }
    }

    // 找一个可写目录：优先 vol2/1000/下载/test
    let remote_dir = "vol2/1000/下载/test";
    let name = format!("rust-selftest-{}.txt", chrono::Utc::now().timestamp());
    let remote = format!("{remote_dir}/{name}");
    let payload = format!("hello from rust client @ {}", chrono::Utc::now()).into_bytes();

    println!("[3/5] 上传 {} ({} 字节)", remote, payload.len());
    if let Err(e) = client.upload(&remote, payload.clone(), None).await {
        eprintln!("SELFTEST_FAIL upload: {e}");
        return 1;
    }

    println!("[4/5] 下载回读 ...");
    let back = match client.download(&remote).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("SELFTEST_FAIL download: {e}");
            return 1;
        }
    };

    println!("[5/5] 校验内容 {} 字节", back.len());
    if back != payload {
        eprintln!(
            "SELFTEST_FAIL content mismatch: got {} want {}",
            String::from_utf8_lossy(&back),
            String::from_utf8_lossy(&payload)
        );
        let _ = client.remove(vec![remote]).await;
        return 1;
    }

    let _ = client.remove(vec![remote.clone()]).await;
    println!("SELFTEST_OK {remote} roundtrip verified");
    0
}

/// 诊断：验证真实同步引擎 —— 本地目录与 NAS 目录双向同步。
/// 成功打印 SELFTEST_SYNC_OK。
pub async fn selftest_sync(addr: &str, user: &str, pass: &str) -> i32 {
    use connect::nas_client::{NasClient, NasConnConfig};
    use std::path::PathBuf;
    use tasks::file_filter::FileFilter;
    use tasks::watch::LocalWatcher;
    use tokio::sync::mpsc;

    let address = if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:5667")
    };
    let config = NasConnConfig {
        address: address.clone(),
        user: user.to_string(),
        password: pass.to_string(),
        device_name: "PcSync".into(),
        verify_tls: false,
    };

    let ts = chrono::Utc::now().timestamp();
    let nas_dir = format!("vol2/1000/下载/fnsync-e2e-{ts}");
    let local_dir = PathBuf::from(format!("/tmp/fnsync-e2e-{ts}"));

    println!("[1/6] 登录 ...");
    let nas = match NasClient::connect(config).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SELFTEST_SYNC_FAIL login: {e}");
            return 1;
        }
    };
    println!("      uid={}", *nas.uid.read());

    println!("[2/6] 建 NAS 目录 {nas_dir}");
    if let Err(e) = nas.mkdir(&nas_dir).await {
        eprintln!("SELFTEST_SYNC_FAIL mkdir: {e}");
        return 1;
    }
    std::fs::create_dir_all(&local_dir).ok();

    // 构造一个最小 Task（直接驱动 actor）
    let task = crate::service::tasks::Task {
        id: 0,
        nas_acc_id: 0,
        name: format!("e2e-{ts}"),
        address: address.clone(),
        conn_type: "ip".into(),
        uuid: ts.to_string(),
        identifier: format!("e2e-{ts}"),
        mode: "bisynchronous".into(),
        nas_path: nas_dir.clone(),
        pc_path: local_dir.to_string_lossy().to_string(),
        status: 2,
        is_init_sync: 0,
        open_cloud: 0,
        created_at: 0,
        updated_at: 0,
    };

    let db_path = std::path::PathBuf::from(format!("/tmp/fnsync-e2e-{ts}.db"));
    let pool = match crate::service::database::open_db(&db_path).await {
        Ok(p) => match crate::service::database::migrate(&p).await {
            Ok(()) => p,
            Err(e) => {
                eprintln!("SELFTEST_SYNC_FAIL migrate: {e}");
                return 1;
            }
        },
        Err(e) => {
            eprintln!("SELFTEST_SYNC_FAIL db: {e}");
            return 1;
        }
    };
    let filter = FileFilter::from_db(None);
    let (local_tx, local_rx) = mpsc::channel(256);
    let watcher = match LocalWatcher::start(local_dir.clone(), filter.clone(), local_tx) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("SELFTEST_SYNC_FAIL watcher: {e}");
            return 1;
        }
    };
    let actor = match crate::tasks::actor::TaskActor::new(task, nas.clone(), pool, filter) {
        Ok(a) => std::sync::Arc::new(a),
        Err(e) => {
            eprintln!("SELFTEST_SYNC_FAIL actor: {e}");
            return 1;
        }
    };
    let handle = tokio::spawn(actor.run(local_rx, watcher));

    println!("[3/6] 本地写文件 → 等待上传");
    std::fs::write(
        local_dir.join("from-linux.txt"),
        b"uploaded by linux client\n",
    )
    .ok();
    tokio::time::sleep(std::time::Duration::from_secs(12)).await;

    let listed = nas.ls(&nas_dir).await.unwrap_or_default();
    let names: Vec<String> = listed.iter().map(|e| e.name.clone()).collect();
    println!("      NAS 目录内容: {names:?}");
    if !names.iter().any(|n| n == "from-linux.txt") {
        eprintln!("SELFTEST_SYNC_FAIL upload not observed");
        handle.abort();
        let _ = nas.remove(vec![nas_dir.clone()]).await;
        return 1;
    }

    println!("[4/6] NAS 写文件 → 等待下载");
    let payload = b"downloaded by linux client\n".to_vec();
    if let Err(e) = nas
        .upload(&format!("{nas_dir}/from-nas.txt"), payload.clone(), None)
        .await
    {
        eprintln!("SELFTEST_SYNC_FAIL seed upload: {e}");
        handle.abort();
        return 1;
    }
    let target = local_dir.join("from-nas.txt");
    let mut ok = false;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if let Ok(b) = std::fs::read(&target) {
            if b == payload {
                ok = true;
                break;
            }
        }
    }

    println!("[5/6] 校验双向同步");
    handle.abort();
    let _ = nas.remove(vec![nas_dir.clone()]).await;
    let _ = std::fs::remove_dir_all(&local_dir);

    if ok {
        println!("SELFTEST_SYNC_OK 双向同步验证通过");
        0
    } else {
        eprintln!(
            "SELFTEST_SYNC_FAIL download not observed at {}",
            target.display()
        );
        1
    }
}

#[cfg(test)]
mod ipc_tests;
