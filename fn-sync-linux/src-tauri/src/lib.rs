//! 库入口 — Tauri 应用（lib 模式，Tauri v2 标准）。命令与状态在此注册。

pub mod commands;
pub mod connect;
pub mod platform;
pub mod service;
mod single_instance;
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
        for key in ["global_upload_speed_limit", "global_download_speed_limit"] {
            if let Some(value) = service::logs::get_system_value(&db, key).await? {
                crate::traffic::set_limit(key, &value);
            }
        }
        service::logs::set_system_value(&db, "auto_update", "0").await?;
        service::logs::check_and_clear_logs(&db).await?;
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
    if let Some(existing) = crate::single_instance::acquire() {
        // 已有实例在运行：不要启动第二个。
        //
        // 两个实例会让按需同步互相踩踏——第二个抢挂载点失败（os error 17），
        // 更糟的是它可能清理/重建共享缓存，导致第一个实例的文件状态全乱。
        eprintln!("已有实例在运行（PID {existing}），本次启动退出。");
        return;
    }
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
        .on_window_event(|window, event| {
            // 官方前端语义：主窗口点“关闭”仅隐藏到托盘，应用继续常驻；
            // 真正退出走托盘菜单“退出”或 app_quit 命令（均为 app.exit(0)）。
            // 仅拦截 main 窗口；OAuth/selftest 等其它窗口保持原生关闭行为。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
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
                    let mut interval=tokio::time::interval(std::time::Duration::from_secs(60));
                    loop {
                        interval.tick().await;
                        if let Err(error)=service::logs::check_and_clear_logs(&state.db).await {
                            tracing::warn!(error=%error,"log retention maintenance failed");
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
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|app_handle, event| {
            // 进程退出前停掉所有任务：按需同步挂载点必须卸载，
            // 否则留下死挂载点（后续访问报 Transport endpoint is not connected）。
            if let tauri::RunEvent::Exit = event {
                use tauri::Manager;
                let state = app_handle.state::<AppState>();
                let _ = tauri::async_runtime::block_on(async {
                    state.task_manager.lock().await.stop_all().await;
                });
            }
        });
}

fn setup_tray(app: &tauri::AppHandle) -> Result<(), tauri::Error> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::TrayIconBuilder;

    let show = MenuItem::with_id(app, "show", "打开飞牛同步", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    let _tray = TrayIconBuilder::with_id("main-tray")
        .menu(&menu)
        .icon(tauri::include_image!("icons/tray.png"))
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

/// 诊断：验证按需同步（FUSE 挂载 + 真实 NAS）。
/// 流程：登录 → 建目录/种子文件 → 挂载 → 读挂载点（按需下载）→ 写挂载点（异步上传）
///       → 删除挂载点文件（删除传播）→ 卸载清理。
/// 成功打印 SELFTEST_ONDEMAND_OK。
pub async fn selftest_ondemand(addr: &str, user: &str, pass: &str) -> i32 {
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

    println!("[1/6] 登录 ...");
    let nas = match NasClient::connect(config).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SELFTEST_ONDEMAND_FAIL login: {e}");
            return 1;
        }
    };
    println!("      uid={}", *nas.uid.read());
    run_ondemand_scenario(nas).await
}

/// 诊断：用本地已保存的 NAS 会话（OAuth 凭据 / longToken）验证按需同步，
/// 不需要用户密码。用于安装后验证。
pub async fn selftest_ondemand_saved() -> i32 {
    use service::nas_account::get_all_nas_accounts;
    use task_manager::TaskManager;

    let db = match service::database::open_db(
        &platform::app_data_dir().unwrap().join("database.sqlite"),
    )
    .await
    {
        Ok(db) => db,
        Err(e) => {
            eprintln!("SELFTEST_ONDEMAND_FAIL open db: {e}");
            return 1;
        }
    };
    let accounts = match get_all_nas_accounts(&db).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("SELFTEST_ONDEMAND_FAIL list accounts: {e}");
            return 1;
        }
    };
    let Some(acc) = accounts.into_iter().next() else {
        eprintln!("SELFTEST_ONDEMAND_FAIL no saved NAS account; sign in once first");
        return 1;
    };
    println!("[1/6] 用已保存会话连接 {} ({})", acc.nas_name, acc.address);
    let nas = match TaskManager::new().account_client(&db, &acc).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SELFTEST_ONDEMAND_FAIL restore session: {e}");
            return 1;
        }
    };
    println!("      uid={}", *nas.uid.read());
    run_ondemand_scenario(nas).await
}

/// 诊断：验证按需挂载在 **会话被服务端作废后能自动重连**（真实 NAS）。
///
/// 复现真实故障：挂载后另开一个同账号连接，把挂载自己的会话踢掉
/// （fnOS 只保留最新会话），再读取一个尚未缓存的文件。
/// 修复前这里会永久卡死；修复后应重建会话并成功读取。
pub async fn selftest_ondemand_reconnect() -> i32 {
    use service::nas_account::get_all_nas_accounts;
    use std::sync::Arc;
    use tasks::file_filter::FileFilter;
    use tasks::ondemand::remote::{
        ReconnectingRemote, RemoteOps, RemoteSession, SessionConnector,
    };
    use tasks::ondemand::{OnDemandConfig, OnDemandMount};

    let db = match service::database::open_db(
        &platform::app_data_dir().unwrap().join("database.sqlite"),
    )
    .await
    {
        Ok(db) => db,
        Err(e) => {
            eprintln!("SELFTEST_RECONNECT_FAIL open db: {e}");
            return 1;
        }
    };
    let Some(acc) = get_all_nas_accounts(&db)
        .await
        .ok()
        .and_then(|a| a.into_iter().next())
    else {
        eprintln!("SELFTEST_RECONNECT_FAIL no saved account");
        return 1;
    };

    println!("[1/6] 建立初始会话 {} ({})", acc.nas_name, acc.address);
    let first = match task_manager::open_account_client(&db, &acc).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SELFTEST_RECONNECT_FAIL connect: {e}");
            return 1;
        }
    };

    let db_c = db.clone();
    let acc_c = acc.clone();
    let connector: SessionConnector = Arc::new(move || {
        let db = db_c.clone();
        let acc = acc_c.clone();
        Box::pin(async move {
            let latest = crate::service::nas_account::get_nas_account_by_id(&db, acc.id)
                .await?
                .unwrap_or(acc);
            let client = crate::task_manager::open_account_client(&db, &latest).await?;
            let session: Arc<dyn RemoteSession> = client;
            Ok(session)
        })
    });
    let remote = ReconnectingRemote::new(Some(first as Arc<dyn RemoteSession>), connector);

    let ts = chrono::Utc::now().timestamp();
    let nas_dir = format!("vol2/1000/下载/fnsync-rc-{ts}");
    let mount_dir = std::path::PathBuf::from(format!("/tmp/fnsync-rc-{ts}/mnt"));
    let cache_root = std::path::PathBuf::from(format!("/tmp/fnsync-rc-{ts}/cache"));

    println!("[2/6] 建远端目录与两个种子文件（第二个稍后才读）");
    let a_bytes = b"before-invalidation".to_vec();
    let b_bytes = b"after-invalidation".to_vec();
    for (name, body) in [("a.txt", &a_bytes), ("b.txt", &b_bytes)] {
        if let Err(e) = remote.upload(&format!("{nas_dir}/{name}"), body.to_vec(), None).await {
            eprintln!("SELFTEST_RECONNECT_FAIL seed {name}: {e}");
            let _ = remote.remove(&[nas_dir.clone()]).await;
            return 1;
        }
    }

    println!("[3/6] 挂载 {}", mount_dir.display());
    std::fs::create_dir_all(&mount_dir).ok();
    let cfg = OnDemandConfig {
        task_id: 0,
        task_name: format!("selftest-rc-{ts}"),
        mountpoint: mount_dir.clone(),
        nas_root: nas_dir.clone(),
        cache_root,
        cache_limit_bytes: tasks::ondemand::DEFAULT_CACHE_LIMIT_BYTES,
    };
    let ops: Arc<dyn RemoteOps> = remote.clone();
    let db_for_mount = db.clone();
    let mount = match tokio::task::spawn_blocking(move || {
        OnDemandMount::start(
            cfg,
            ops,
            db_for_mount,
            FileFilter::from_db(None),
        )
    })
    .await
    {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => {
            eprintln!("SELFTEST_RECONNECT_FAIL mount: {e}");
            let _ = remote.remove(&[nas_dir.clone()]).await;
            return 1;
        }
        Err(e) => {
            eprintln!("SELFTEST_RECONNECT_FAIL mount task: {e}");
            return 1;
        }
    };

    let mut ok = true;
    println!("[4/6] 正常读取 a.txt（验证挂载可用）");
    match std::fs::read(mount_dir.join("a.txt")) {
        Ok(b) if b == a_bytes => println!("      a.txt 内容一致 ✓"),
        other => {
            eprintln!("SELFTEST_RECONNECT_FAIL read a: {other:?}");
            ok = false;
        }
    }

    println!("[5/6] 顶掉挂载会话（换新 token），再读 b.txt（必须自动重连）");
    // 真实故障是这样发生的：他处（另一台设备/另一进程）重新认证后拿到新 token，
    // 旧 token 立即失效，挂载持有的会话在下一次 RPC 收到 errno 4224 并自断。
    // 这里用一个无效的旧 token 触发 `restore_session` 走 tokenLogin 换新 token，
    // 从而让挂载手里的旧 token 失效。
    let mut stale = acc.clone();
    stale.token = "stale-token-for-reconnect-test".into();
    match task_manager::open_account_client(&db, &stale).await {
        Ok(_) => println!("      已顶掉旧 token（挂载会话应被作废）"),
        Err(e) => println!("      顶替连接失败（仍继续验证）: {e}"),
    }
    let recovered = std::fs::read(mount_dir.join("b.txt"));
    match recovered {
        Ok(b) if b == b_bytes => println!("      b.txt 读取成功 ✓"),
        other => {
            eprintln!("SELFTEST_RECONNECT_FAIL read b after invalidation: {other:?}");
            ok = false;
        }
    }
    let reconnects = remote.reconnect_count();
    println!("      重连次数 = {reconnects}");
    if reconnects == 0 {
        eprintln!("SELFTEST_RECONNECT_FAIL no reconnect happened");
        ok = false;
    }

    println!("[6/6] 卸载 + 清理");
    tokio::task::spawn_blocking(move || mount.stop()).await.ok();
    let _ = remote.remove(&[nas_dir.clone()]).await;
    let _ = std::fs::remove_dir_all(mount_dir.parent().unwrap());

    if ok {
        println!("SELFTEST_RECONNECT_OK 会话失效后可自动恢复");
        0
    } else {
        1
    }
}

/// 按需同步场景：建远端目录 → 挂载 → 按需下载 → 上传 → 删除传播 → 卸载。
/// `nas` 必须是已认证的连接。
async fn run_ondemand_scenario(nas: std::sync::Arc<connect::nas_client::NasClient>) -> i32 {
    use std::sync::Arc;
    use tasks::file_filter::FileFilter;
    use tasks::ondemand::{OnDemandConfig, OnDemandMount};

    let ts = chrono::Utc::now().timestamp();
    let nas_dir = format!("vol2/1000/下载/fnsync-od-{ts}");
    let mount_dir = std::path::PathBuf::from(format!("/tmp/fnsync-od-{ts}/mnt"));
    let cache_root = std::path::PathBuf::from(format!("/tmp/fnsync-od-{ts}/cache"));

    println!("[2/6] 建 NAS 目录 {nas_dir} + 种子文件");
    if let Err(e) = nas.mkdir(&nas_dir).await {
        eprintln!("SELFTEST_ONDEMAND_FAIL mkdir: {e}");
        return 1;
    }
    let seed = format!("remote-seed-{ts}").into_bytes();
    if let Err(e) = nas
        .upload(&format!("{nas_dir}/seed.txt"), seed.clone(), None)
        .await
    {
        eprintln!("SELFTEST_ONDEMAND_FAIL seed upload: {e}");
        let _ = nas.remove(vec![nas_dir.clone()]).await;
        return 1;
    }

    println!("[3/6] FUSE 挂载 {}", mount_dir.display());
    std::fs::create_dir_all(&mount_dir).ok();
    let cfg = OnDemandConfig {
        task_id: 0,
        task_name: format!("selftest-od-{ts}"),
        mountpoint: mount_dir.clone(),
        nas_root: nas_dir.clone(),
        cache_root,
        cache_limit_bytes: tasks::ondemand::DEFAULT_CACHE_LIMIT_BYTES,
    };
    let nas_for_mount: Arc<dyn tasks::ondemand::remote::RemoteOps> = nas.clone();
    let mount = match tokio::task::spawn_blocking(move || {
        OnDemandMount::start(
            cfg,
            nas_for_mount,
            sqlx::SqlitePool::connect_lazy("sqlite::memory:")
                .expect("memory db"),
            FileFilter::from_db(None),
        )
    })
    .await
    {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => {
            eprintln!("SELFTEST_ONDEMAND_FAIL mount: {e}");
            let _ = nas.remove(vec![nas_dir.clone()]).await;
            return 1;
        }
        Err(e) => {
            eprintln!("SELFTEST_ONDEMAND_FAIL mount task: {e}");
            let _ = nas.remove(vec![nas_dir.clone()]).await;
            return 1;
        }
    };

    let mut ok = true;
    println!("[4/6] 读挂载点（按需下载）...");
    match std::fs::read(mount_dir.join("seed.txt")) {
        Ok(bytes) if bytes == seed => println!("      内容一致 ✓"),
        Ok(bytes) => {
            eprintln!("SELFTEST_ONDEMAND_FAIL content mismatch: {bytes:?}");
            ok = false;
        }
        Err(e) => {
            eprintln!("SELFTEST_ONDEMAND_FAIL read: {e}");
            ok = false;
        }
    }

    println!("[5/6] 写挂载点 → 等待上传 → 删除传播 ...");
    let written = b"written via on-demand mount";
    if let Err(e) = std::fs::write(mount_dir.join("from-linux.txt"), written) {
        eprintln!("SELFTEST_ONDEMAND_FAIL write: {e}");
        ok = false;
    }
    let remote_check = format!("{nas_dir}/from-linux.txt");
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if nas.download(&remote_check).await.ok().as_deref() == Some(written.as_slice()) {
            break;
        }
    }
    if nas.download(&remote_check).await.ok().as_deref() != Some(written.as_slice()) {
        eprintln!("SELFTEST_ONDEMAND_FAIL upload not observed");
        ok = false;
    }
    let _ = std::fs::remove_file(mount_dir.join("from-linux.txt"));
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if !nas.ls(&nas_dir).await.unwrap_or_default().iter().any(|e| e.name == "from-linux.txt") {
            break;
        }
    }
    if nas.ls(&nas_dir).await.unwrap_or_default().iter().any(|e| e.name == "from-linux.txt") {
        eprintln!("SELFTEST_ONDEMAND_FAIL delete not propagated");
        ok = false;
    }

    println!("[6/6] 卸载 + 清理 ...");
    tokio::task::spawn_blocking(move || mount.stop()).await.ok();
    let _ = nas.remove(vec![nas_dir.clone()]).await;
    let _ = std::fs::remove_dir_all(mount_dir.parent().unwrap());

    if ok {
        println!("SELFTEST_ONDEMAND_OK 按需同步验证通过");
        0
    } else {
        1
    }
}

#[cfg(test)]
mod ipc_tests;
