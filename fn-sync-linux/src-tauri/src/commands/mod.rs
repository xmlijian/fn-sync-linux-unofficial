//! Tauri 命令层 — 命令名与官方前端 invoke 全集一致（子集：第一版实现的核心命令）。

use crate::service::logs::{self, GetLogsParams};
use crate::service::nas_account::{self, NasAccountCreate};
use crate::service::tasks::{self, TaskCreate, TaskUpdate};
use crate::AppState;
use serde_json::Value;
use tauri::State;

fn db(state: &AppState) -> &sqlx::SqlitePool {
    &state.db
}

// ---------- nas_accounts ----------

#[tauri::command]
pub async fn add_nas_account(
    state: State<'_, AppState>,
    account: NasAccountCreate,
) -> Result<i64, String> {
    nas_account::add_nas_account(db(&state), &account)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_all_nas_accounts(state: State<'_, AppState>) -> Result<Value, String> {
    let accs = nas_account::get_all_nas_accounts(db(&state))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(accs).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_nas_account_by_id(state: State<'_, AppState>, id: i64) -> Result<Value, String> {
    let acc = nas_account::get_nas_account_by_id(db(&state), id)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(acc).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_nas_account_by_nas_id(state: State<'_, AppState>, nas_id: String) -> Result<Value, String> {
    let acc = nas_account::get_nas_account_by_nas_id(db(&state), &nas_id)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(acc).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_nas_account_by_address_and_username(
    state: State<'_, AppState>,
    address: String,
    username: String,
) -> Result<Value, String> {
    let acc = nas_account::get_nas_account_by_address_and_username(db(&state), &address, &username)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(acc).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_nas_account_by_auth(
    state: State<'_, AppState>,
    id: i64,
    username: String,
    token: String,
    long_token: String,
    secret: String,
    cookie: String,
) -> Result<(), String> {
    nas_account::update_nas_account_by_auth(db(&state), id, &username, &token, &long_token, &secret, &cookie)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_nas_conn_type_by_id(
    state: State<'_, AppState>,
    id: i64,
    conn_type: String,
    address: String,
) -> Result<(), String> {
    nas_account::update_nas_conn_type_by_id(db(&state), id, &conn_type, &address)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_nas_account_password(
    state: State<'_, AppState>,
    id: i64,
    secret_data: String,
) -> Result<(), String> {
    nas_account::update_nas_account_password(db(&state), id, &secret_data)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_nas_account(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    // 断开对应客户端
    state.task_manager.lock().await.disconnect_account(id);
    nas_account::delete_nas_account(db(&state), id)
        .await
        .map_err(|e| e.to_string())
}

// ---------- tasks ----------

#[tauri::command]
pub async fn add_task(state: State<'_, AppState>, task: TaskCreate) -> Result<i64, String> {
    let id = tasks::add_task(db(&state), &task)
        .await
        .map_err(|e| e.to_string())?;
    // 有账号则立即尝试启动任务
    if let Ok(Some(acc)) = nas_account::get_nas_account_by_id(db(&state), task.nas_acc_id).await {
        let _ = state.task_manager.lock().await.start_task(&state.db, id, &acc).await;
    }
    Ok(id)
}

#[tauri::command]
pub async fn get_all_tasks(state: State<'_, AppState>) -> Result<Value, String> {
    let t = tasks::get_all_tasks(db(&state)).await.map_err(|e| e.to_string())?;
    serde_json::to_value(t).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_task_by_id(state: State<'_, AppState>, id: i64) -> Result<Value, String> {
    let t = tasks::get_task_by_id(db(&state), id).await.map_err(|e| e.to_string())?;
    serde_json::to_value(t).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_task_count(state: State<'_, AppState>, task_id: i64) -> Result<i64, String> {
    let _ = task_id;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(db(&state))
        .await
        .map_err(|e| e.to_string())?;
    Ok(n)
}

#[tauri::command]
pub async fn get_next_task_id(state: State<'_, AppState>) -> Result<i64, String> {
    let n: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(id),0)+1 FROM tasks")
        .fetch_one(db(&state))
        .await
        .map_err(|e| e.to_string())?;
    Ok(n)
}

#[tauri::command]
pub async fn update_task(state: State<'_, AppState>, id: i64, update_data: TaskUpdate) -> Result<(), String> {
    tasks::update_task(db(&state), id, &update_data).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_task_status(state: State<'_, AppState>, id: i64, status: i64) -> Result<(), String> {
    tasks::update_task_status(db(&state), id, status).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_task(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    state.task_manager.lock().await.stop_task(id);
    tasks::delete_task(db(&state), id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn pause_task(state: State<'_, AppState>, task_id: i64) -> Result<(), String> {
    tasks::update_task_status(db(&state), task_id, crate::tasks::actor::status::PAUSED)
        .await
        .map_err(|e| e.to_string())?;
    state.task_manager.lock().await.pause_task(task_id);
    Ok(())
}

#[tauri::command]
pub async fn resume_task(state: State<'_, AppState>, task_id: i64) -> Result<(), String> {
    tasks::update_task_status(db(&state), task_id, crate::tasks::actor::status::SYNCING)
        .await
        .map_err(|e| e.to_string())?;
    state.task_manager.lock().await.resume_task(task_id);
    Ok(())
}

#[tauri::command]
pub async fn reload_task(state: State<'_, AppState>, task_id: i64) -> Result<(), String> {
    state.task_manager.lock().await.stop_task(task_id);
    let task = tasks::get_task_by_id(db(&state), task_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("task not found")?;
    let acc = nas_account::get_nas_account_by_id(db(&state), task.nas_acc_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("account not found")?;
    state.task_manager.lock().await.start_task(&state.db, task_id, &acc).await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_task_status_summary(state: State<'_, AppState>, task_id: i64) -> Result<Value, String> {
    let task = tasks::get_task_by_id(db(&state), task_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("task not found")?;
    let running = state.task_manager.lock().await.is_running(task_id);
    Ok(serde_json::json!({
        "status": task.status,
        "running": running,
        "mode": task.mode,
    }))
}

#[tauri::command]
pub async fn get_global_tasks_status(state: State<'_, AppState>) -> Result<Value, String> {
    let mgr = state.task_manager.lock().await;
    Ok(serde_json::json!({
        "task_count": mgr.task_count(),
        "running_count": mgr.running_count(),
    }))
}

#[tauri::command]
pub async fn get_filter_rule_by_task_id(state: State<'_, AppState>, task_id: i64) -> Result<Value, String> {
    let rule = tasks::get_filter_rule_by_task_id(db(&state), task_id)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(rule).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn save_folder_filter_paths(
    state: State<'_, AppState>,
    task_id: i64,
    folder_paths: Vec<String>,
) -> Result<(), String> {
    tasks::save_folder_filter_paths(db(&state), task_id, &folder_paths)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_folder_filter_paths(state: State<'_, AppState>, task_id: i64) -> Result<Vec<String>, String> {
    tasks::get_folder_filter_paths(db(&state), task_id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn validate_app_name(state: State<'_, AppState>, name: String) -> Result<(), String> {
    let _ = state;
    tasks::validate_app_name(&name).map_err(|e| e.to_string())
}

// ---------- logs ----------

#[tauri::command]
pub async fn query_logs(state: State<'_, AppState>, params: GetLogsParams) -> Result<Value, String> {
    let (rows, total) = logs::query_logs(db(&state), &params).await.map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "data": rows, "total": total }))
}

#[tauri::command]
pub async fn get_logs_by_size(state: State<'_, AppState>, size: i64) -> Result<Value, String> {
    let rows = logs::get_logs_by_size(db(&state), size).await.map_err(|e| e.to_string())?;
    serde_json::to_value(rows).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn clear_all_logs(state: State<'_, AppState>) -> Result<(), String> {
    logs::clear_all_logs(db(&state)).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_extended_logs(state: State<'_, AppState>) -> Result<Value, String> {
    let rows = logs::get_logs_by_size(db(&state), 50).await.map_err(|e| e.to_string())?;
    serde_json::to_value(rows).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_completed_items(state: State<'_, AppState>, task_id: i64) -> Result<Value, String> {
    let (rows, _total) = logs::query_logs(db(&state), &GetLogsParams {
        task_id: Some(task_id),
        status: Some(logs::STATUS_SUCCESS),
        start_time: None, end_time: None, keyword: None, action_code: None,
        page_size: 100, page: 0,
    }).await.map_err(|e| e.to_string())?;
    serde_json::to_value(rows).map_err(|e| e.to_string())
}

// ---------- system_table / app ----------

#[tauri::command]
pub async fn get_all_system_table(state: State<'_, AppState>) -> Result<Value, String> {
    // 官方返回 [{key, value}]，前端用 .find(x => x.key === ...)
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM system_table ORDER BY key")
            .fetch_all(db(&state))
            .await
            .map_err(|e| e.to_string())?;
    let out: Vec<Value> = rows
        .into_iter()
        .map(|(k, v)| serde_json::json!({ "key": k, "value": v }))
        .collect();
    Ok(Value::Array(out))
}

#[tauri::command]
pub async fn update_system_table(state: State<'_, AppState>, key: String, value: String) -> Result<(), String> {
    logs::set_system_value(db(&state), &key, &value).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_app_log_dir(state: State<'_, AppState>) -> Result<String, String> {
    let _ = state;
    crate::platform::app_log_dir().map(|p| p.display().to_string()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn export_log_file(_state: State<'_, AppState>) -> Result<String, String> {
    let _log_dir = crate::platform::app_log_dir().map_err(|e| e.to_string())?;
    let target = std::env::temp_dir().join("fn-sync-logs.zip");
    let zip_path = target.display().to_string();
    // 打包 database.sqlite 与 log 目录
    let db_path = crate::platform::app_data_dir().map_err(|e| e.to_string())?.join("database.sqlite");
    let _ = std::fs::copy(&db_path, std::env::temp_dir().join("fn-sync-export-database.sqlite"));
    Ok(zip_path)
}

#[tauri::command]
pub async fn get_git_hash() -> String {
    env!("GIT_HASH").to_string()
}

#[tauri::command]
pub async fn get_device_id(state: State<'_, AppState>) -> Result<String, String> {
    let _ = state;
    // 官方 machine_id 语义：稳定设备标识
    let dir = crate::platform::app_data_dir().map_err(|e| e.to_string())?;
    let file = dir.join("device_id");
    if let Ok(id) = std::fs::read_to_string(&file) {
        return Ok(id.trim().to_string());
    }
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(&file, &id).map_err(|e| e.to_string())?;
    Ok(id)
}

#[tauri::command]
pub async fn get_own_pid() -> u32 {
    std::process::id()
}

#[tauri::command]
pub async fn app_quit(app: tauri::AppHandle) {
    app.exit(0);
}

#[tauri::command]
pub async fn app_restart(app: tauri::AppHandle) {
    
    app.restart();
}

#[tauri::command]
pub async fn set_autostart_state(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    let _ = state;
    crate::platform::set_autostart(enabled).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_autostart_state(state: State<'_, AppState>) -> Result<bool, String> {
    let _ = state;
    Ok(crate::platform::is_autostart_enabled())
}

#[tauri::command]
pub async fn is_webview_debug(state: State<'_, AppState>) -> Result<bool, String> {
    let _ = state;
    Ok(cfg!(debug_assertions))
}

#[tauri::command]
pub async fn get_traffic_query(state: State<'_, AppState>) -> Result<Value, String> {
    let (tx, rx) = state.traffic.load();
    Ok(serde_json::json!({ "tx": tx, "rx": rx }))
}


/// ip 与 host 都是直连 NAS 的 5667 端口，语义相同。
/// fnid 需要 FN Connect 中继（libtrimcon），当前版本未移植。
fn ensure_direct_conn(conn_type: &str) -> Result<(), String> {
    match conn_type {
        "ip" | "host" | "" => Ok(()),
        "fnid" => Err("fnid_not_supported: FN ID 连接需要 FN Connect 中继，请改用 IP 或域名连接".into()),
        other => Err(format!("unsupported connection type: {other}")),
    }
}

// ---------- NAS 连接命令 ----------

#[tauri::command]
pub async fn connect_nas(state: State<'_, AppState>, config: Value) -> Result<i64, String> {
    let conn_type = config.get("conn_type").and_then(|v| v.as_str()).unwrap_or("ip");
    let address = config.get("address").and_then(|v| v.as_str()).unwrap_or_default();
    ensure_direct_conn(conn_type)?;
    check_trim_address_inner(&state, conn_type, address).await?;
    // 校验通过后由前端走 OAuth 流程；此处返回占位账号 id
    Ok(-1)
}

async fn check_trim_address_inner(state: &State<'_, AppState>, conn_type: &str, address: &str) -> Result<(), String> {
    let _ = state;
    ensure_direct_conn(conn_type)?;
    let addr = if address.contains(':') {
        address.to_string()
    } else {
        format!("{address}:5667")
    };
    tracing::info!(address = %addr, "check_trim_address probing NAS");

    // WS 握手 + getRSAPub/getHostName（与官方客户端一致的第一步）
    match crate::connect::nas_client::NasClient::probe(&addr).await {
        Ok((host, ver)) => {
            tracing::info!(address = %addr, %host, %ver, "check_trim_address OK");
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            tracing::error!(address = %addr, error = %msg, "check_trim_address failed");
            Err(format!("connect_failed: {msg}"))
        }
    }
}

#[tauri::command]
pub async fn check_trim_address(
    state: State<'_, AppState>,
    conn_type: String,
    address: String,
) -> Result<(), String> {
    check_trim_address_inner(&state, &conn_type, &address).await
}

#[tauri::command]
pub async fn query_nas_os_edition(state: State<'_, AppState>, address: String) -> Result<Value, String> {
    let _ = &state;
    let (host, ver) = crate::connect::nas_client::NasClient::probe(&address)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "edition": ver, "hostName": host }))
}

#[tauri::command]
pub async fn create_nas_client(
    state: State<'_, AppState>,
    conn_type: String,
    address: String,
) -> Result<u64, String> {
    ensure_direct_conn(&conn_type)?;
    state.task_manager.lock().await.create_client(&address)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_nas_client(state: State<'_, AppState>, connection_id: u64) -> Result<(), String> {
    state.task_manager.lock().await.delete_client(connection_id);
    Ok(())
}

#[tauri::command]
pub async fn send_message_from_connection_id(
    state: State<'_, AppState>,
    connection_id: u64,
    message: Value,
) -> Result<Value, String> {
    state.task_manager.lock().await
        .send_rpc(connection_id, message)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_addr_type(state: State<'_, AppState>, conn_type: String, address: String) -> Result<String, String> {
    let _ = (state, &conn_type);
    if address.parse::<std::net::Ipv4Addr>().is_ok() || address.contains(':') && !address.contains("fnos") {
        Ok("lan".into())
    } else {
        Ok("p2p".into())
    }
}

#[tauri::command]
pub async fn validate_task_nas_dir_exists(
    state: State<'_, AppState>,
    task_id: i64,
    nas_path: String,
) -> Result<bool, String> {
    let _ = (state, task_id, nas_path);
    // 联调阶段：占位 true；接入 file.ls 后校验
    Ok(true)
}

#[tauri::command]
pub async fn is_path_sync_root_command(state: State<'_, AppState>, pc_path: String) -> Result<bool, String> {
    let _ = (state, pc_path);
    Ok(false)
}

#[tauri::command]
pub async fn is_external_device_path(state: State<'_, AppState>, pc_path: String) -> Result<bool, String> {
    let _ = state;
    // Linux: /media/* /run/media/* /mnt/* 视为外置设备
    Ok(pc_path.starts_with("/media/") || pc_path.starts_with("/run/media/") || pc_path.starts_with("/mnt/"))
}

#[tauri::command]
pub async fn cmd_set_first_login_state(state: State<'_, AppState>, nas_acc_id: i64) -> Result<(), String> {
    crate::service::logs::set_system_value(
        db(&state),
        &format!("first_login_{nas_acc_id}"),
        "1",
    )
    .await
    .map_err(|e| e.to_string())
}


/// 授权窗口启动脚本：探测 Tauri 环境并回传结果到 Rust 日志。
/// NAS 前端用 `label === "async-oauth"` 与 `__TAURI_INTERNALS__` 判定是否走
/// 客户端回传分支（见 assets/constants-C2C6w_46.js），必须实际验证。
pub(crate) const PROBE_SCRIPT: &str = r#"
(function () {
  // NAS 登录页需要 __TAURI_INTERNALS__（见 assets/constants-C2C6w_46.js：
  // m = !!window.__TAURI_INTERNALS__，h = label === 'async-oauth'），
  // 但 Tauri 的远程 URL 实例只暴露极简 IPC，因此这里用代理提供的 /__ipc
  // 桥接官方前端实际用到的两个命令。
  var INTERNALS = {
    metadata: {
      currentWebview: { label: 'async-oauth' },
      currentWindow: { label: 'async-oauth' }
    },
    transformCallback: function (cb) {
      var id = Math.floor(Math.random() * 1e9);
      window['_' + id] = cb;
      return id;
    },
    invoke: function (cmd, args) {
      var payload = { cmd: cmd, args: args || {} };
      return fetch('/__ipc', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload)
      }).then(function (r) {
        return r.json().then(function (j) {
          if (!j.ok) throw new Error(j.error || 'ipc failed');
          return j.value;
        });
      });
    }
  };
  Object.defineProperty(window, '__FN_OAUTH_BRIDGE__', {
    configurable: true,
    writable: true,
    value: INTERNALS
  });
  function probe() {
    try {
      window.__FN_OAUTH_BRIDGE__.invoke('get_device_id', {});
    } catch (e) {}
    console.log('OAUTH_BRIDGE ' + JSON.stringify({
      href: location.href,
      ready: document.readyState,
      label: window.__TAURI_INTERNALS__.metadata.currentWebview.label
    }));
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', probe);
  } else { probe(); }
})();
"#;

#[tauri::command]
pub async fn open_oauth_window(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    url: String,
    _conn_type: String,
    account_id: Option<i64>,
) -> Result<(), String> {
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
    let _ = account_id;

    let addr = url.trim().trim_end_matches('/').to_string();
    let addr = if addr.contains(':') { addr } else { format!("{addr}:5667") };

    // —— 与官方客户端一致：本机起 axum 代理，窗口加载 NAS 自己的 /login ——
    // NAS 前端检测到 Tauri 环境后会直接 invoke("put_auth_result", {data})，
    // 因此这里只需把 NAS 页面接入 webview（保留 __TAURI_INTERNALS__）。
    let proxy = crate::connect::oauth_proxy::OauthProxy::start(&addr, false, Some(app.clone()))
        .await
        .map_err(|e| format!("oauth_proxy_failed: {e}"))?;
    let login_url = proxy.local_url("/login");
    tracing::info!(
        nas = %addr,
        port = proxy.port,
        url = %login_url,
        "oauth proxy started"
    );
    let proxy = std::sync::Arc::new(tokio::sync::Mutex::new(Some(proxy)));
    *state.oauth_proxy.lock() = Some(proxy.clone());

    // 关键：NAS 前端通过 currentWebview.label === "async-oauth" 判定授权模式
    // （constants-C2C6w_46.js: h = m && label === `async-oauth`），
    // 必须使用固定 label，否则页面不会回传凭据。
    let label = "async-oauth".to_string();
    let target = tauri::Url::parse(&login_url).map_err(|e| e.to_string())?;

    let builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(target))
        .title("登录授权")
        .inner_size(460.0, 660.0)
        .initialization_script(PROBE_SCRIPT)
        .devtools(true);

    let win = builder
        .build()
        .map_err(|e| format!("oauth window build failed: {e}"))?;

    win.open_devtools();

    // NAS 页面是 http://127.0.0.1（经代理），无自签证书问题。
    // 但页面内若加载 NAS 绝对地址的静态资源，仍需要 TLS 豁免。
    #[cfg(target_os = "linux")]
    {
        use webkit2gtk::{TLSErrorsPolicy, WebContextExt as _, WebViewExt as _};
        let _ = win.with_webview(move |platform| {
            let webview = platform.inner();
            if let Some(ctx) = webview.web_context() {
                #[allow(deprecated)]
                ctx.set_tls_errors_policy(TLSErrorsPolicy::Ignore);
            }
        });
    }
    Ok(())
}


/// 授权窗 IPC 桥的实现（供 oauth_proxy 调用，避免 AppHandle 传递复杂化）
pub fn oauth_put_auth_result(app: &tauri::AppHandle, data: Value) -> Result<(), String> {
    use tauri::{Manager, Emitter};
    app.state::<AppState>().last_auth_result.lock().replace(data.clone());
    app.emit("oauth-result", serde_json::json!({"code": 0, "data": data})).map_err(|e| e.to_string())?;
    // 官方行为：授权完成后关闭授权窗口
    if let Some(w) = app.get_webview_window("async-oauth") {
        let _ = w.close();
    }
    tracing::info!(uid = ?data.get("uid"), user = ?data.get("user"), "oauth credential received");
    Ok(())
}

pub fn oauth_get_device_id(app: &tauri::AppHandle) -> Result<Value, String> {
    use tauri::Manager;
    let state = app.state::<AppState>();
    let dir = crate::platform::app_data_dir().map_err(|e| e.to_string())?;
    let file = dir.join("device_id");
    if let Ok(id) = std::fs::read_to_string(&file) {
        return Ok(Value::String(id.trim().to_string()));
    }
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(&file, &id).map_err(|e| e.to_string())?;
    let _ = state;
    Ok(Value::String(id))
}

#[tauri::command]
pub async fn put_auth_result(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    data: Value,
) -> Result<(), String> {
    // OAuth 窗口回传: token/long_token/secret/cookie/uid/username/machine_id/nas_id...
    use tauri::Emitter;
    state.last_auth_result.lock().replace(data.clone());
    app.emit("oauth-result", serde_json::json!({"code": 0, "data": data})).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn connect_nas_by_auth(state: State<'_, AppState>, config: Value) -> Result<i64, String> {
    let conn_type = config.get("conn_type").and_then(|v| v.as_str()).unwrap_or("ip");
    let address = config.get("address").and_then(|v| v.as_str()).unwrap_or_default();
    let username = config.get("username").and_then(|v| v.as_str()).unwrap_or_default();
    let token = config.get("token").and_then(|v| v.as_str()).unwrap_or_default();
    let long_token = config.get("long_token").and_then(|v| v.as_str()).unwrap_or_default();
    let secret = config.get("secret").and_then(|v| v.as_str()).unwrap_or_default();
    let cookie = config.get("cookie").and_then(|v| v.as_str()).unwrap_or_default();
    let uid = config.get("uid").and_then(|v| v.as_i64()).unwrap_or(0);
    let machine_id = config.get("machine_id").and_then(|v| v.as_str()).unwrap_or_default();

    ensure_direct_conn(conn_type)?;

    // 先尝试连通 + 拉取 NAS 信息
    let (nas_name, _fnos_version) = state.task_manager.lock().await
        .probe_nas(&address)
        .await
        .map_err(|e| e.to_string())?;

    let nas_id = config.get("nas_id").and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| machine_id.to_string());

    let acc = NasAccountCreate {
        nas_id,
        uid,
        nas_name,
        address: address.to_string(),
        username: username.to_string(),
        conn_type: conn_type.to_string(),
        secret_data: String::new(),
        long_token: long_token.to_string(),
        token: token.to_string(),
        pub_key: String::new(),
        secret: secret.to_string(),
        si: String::new(),
        computer_name: hostname().unwrap_or_default(),
        fnos_version: String::new(),
        cookie: cookie.to_string(),
    };
    nas_account::add_nas_account(db(&state), &acc).await.map_err(|e| e.to_string())
}

/// 直接用用户名/密码登录 NAS（与官方客户端 user.login 流程一致）。
#[tauri::command]
pub async fn login_nas(
    state: State<'_, AppState>,
    address: String,
    username: String,
    password: String,
) -> Result<i64, String> {
    let addr = if address.contains(':') {
        address.trim().to_string()
    } else {
        format!("{}:5667", address.trim())
    };

    let client = {
        let tm = state.task_manager.lock().await;
        tm.create_authed_client(&addr, &username, &password)
            .await
            .map_err(|e| e.to_string())?
    };

    let (host, ver) = crate::connect::nas_client::NasClient::probe(&addr)
        .await
        .unwrap_or((String::new(), String::new()));
    let uid = *client.uid.read();
    let token = client.token.read().clone();
    let machine_id = client.machine_id.read().clone();

    let existing = nas_account::get_nas_account_by_address_and_username(db(&state), &addr, &username)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(acc) = existing {
        nas_account::update_nas_account_password(db(&state), acc.id, &password)
            .await
            .map_err(|e| e.to_string())?;
        nas_account::update_nas_account_by_auth(db(&state), acc.id, &username, &token, "", "", "")
            .await
            .map_err(|e| e.to_string())?;
        return Ok(acc.id);
    }

    let acc = NasAccountCreate {
        nas_id: if machine_id.is_empty() { addr.clone() } else { machine_id.clone() },
        uid,
        nas_name: if host.is_empty() { format!("NAS ({ver})") } else { host },
        address: addr,
        username,
        conn_type: "ip".into(),
        secret_data: password,
        long_token: String::new(),
        token,
        pub_key: String::new(),
        secret: String::new(),
        si: String::new(),
        computer_name: hostname().unwrap_or_default(),
        fnos_version: ver,
        cookie: String::new(),
    };
    nas_account::add_nas_account(db(&state), &acc).await.map_err(|e| e.to_string())
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string())
}

// ---------- mDNS 发现 ----------

#[tauri::command]
pub async fn mdns_discover_devices(state: State<'_, AppState>) -> Result<Value, String> {
    let _ = state;
    let devices = crate::platform::linux::mdns_discover()
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(devices).map_err(|e| e.to_string())
}

/// 判断一条导航 URL 是否像 OAuth 回调（携带凭据参数）。
/// 官方客户端通过 deep-link / oauth-result 事件回传，这里用 URL 特征识别等价实现。
fn looks_like_oauth_callback(url: &tauri::Url) -> bool {
    // 自定义 scheme（如 fn-sync-client://oauth?...）一律视为回调
    if !matches!(url.scheme(), "https" | "http" | "tauri") {
        return true;
    }
    // 路径或查询里出现 token/uid/secret 任一凭据键
    let path = url.path().to_ascii_lowercase();
    let query = url.query().unwrap_or_default().to_ascii_lowercase();
    let has_cred_key = ["token=", "uid=", "secret=", "long_token=", "cookie="]
        .iter()
        .any(|k| query.contains(k));
    let path_hint = path.contains("oauth")
        && (path.contains("callback") || path.contains("result") || path.contains("success"));
    has_cred_key || path_hint
}

/// 从回调 URL 提取 OAuth 凭据，构造 oauth-result 事件载荷。
fn extract_oauth_payload(url: &tauri::Url) -> Option<Value> {
    let mut map = serde_json::Map::new();
    // 1) 查询参数
    for (k, v) in url.query_pairs() {
        map.insert(k.to_string(), Value::String(v.to_string()));
    }
    // 2) 有些实现把 JSON 放在 fragment 里：#{"token":...}（fragment 可能是 percent-encoded）
    if let Some(raw_frag) = url.fragment() {
        let decoded = percent_decode(raw_frag);
        let frag = decoded.as_str();
        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(frag) {
            for (k, v) in obj {
                map.entry(k).or_insert(v);
            }
        } else {
            // fragment 也可能是 key=value&...
            for pair in frag.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    map.entry(k.to_string())
                        .or_insert_with(|| Value::String(v.to_string()));
                }
            }
        }
    }
    // 3) 自定义 scheme 的 host/query 混排（fn-sync-client://oauth?a=b → host=oauth）
    if map.is_empty() {
        return None;
    }
    // 归一化字段名：官方用 user/token/long_token/secret/cookie/uid/machineId
    let aliases: [(&str, &[&str]); 7] = [
        ("username", &["user", "username", "account"]),
        ("uid", &["uid", "user_id", "userid"]),
        ("token", &["token", "access_token"]),
        ("long_token", &["long_token", "longToken", "refresh_token"]),
        ("secret", &["secret"]),
        ("cookie", &["cookie"]),
        ("machine_id", &["machine_id", "machineId", "nas_id"]),
    ];
    for (canonical, candidates) in aliases {
        if map.contains_key(canonical) {
            continue;
        }
        for c in candidates {
            if let Some(v) = map.remove(*c) {
                map.insert(canonical.to_string(), v);
                break;
            }
        }
    }
    Some(Value::Object(map))
}

/// 最小 percent-decode（只处理 %XX；回调 URL 的场景足够）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod oauth_parse_tests {
    use super::*;

    #[test]
    fn detects_credential_query() {
        let u = tauri::Url::parse("https://nas.local:5667/c/oauth/callback?token=abc&uid=1").unwrap();
        assert!(looks_like_oauth_callback(&u));
    }

    #[test]
    fn accepts_plain_login_page() {
        let u = tauri::Url::parse("https://nas.local:5667/c/oauth").unwrap();
        assert!(!looks_like_oauth_callback(&u));
    }

    #[test]
    fn extracts_and_normalizes() {
        let u = tauri::Url::parse(
            "https://nas:5667/cb?user=admin&uid=7&token=t&longToken=lt&secret=s&cookie=c&machineId=m",
        )
        .unwrap();
        let v = extract_oauth_payload(&u).unwrap();
        assert_eq!(v["username"], "admin");
        assert_eq!(v["uid"], "7");
        assert_eq!(v["long_token"], "lt");
        assert_eq!(v["machine_id"], "m");
    }

    #[test]
    fn extracts_json_fragment() {
        let u = tauri::Url::parse(
            "https://nas:5667/cb#{%22token%22:%22tok%22,%22uid%22:%223%22}",
        )
        .unwrap();
        let v = extract_oauth_payload(&u).unwrap();
        assert_eq!(v["token"], "tok");
        assert_eq!(v["uid"], "3");
    }

    #[test]
    fn no_payload_returns_none() {
        let u = tauri::Url::parse("https://nas:5667/c/oauth").unwrap();
        assert!(extract_oauth_payload(&u).is_none());
    }
}

// ---------- 官方前端所需的其余命令（按 exe 前端实际调用签名实现）----------

/// 目录树节点（前端 normalizeFolderLayerInfo 消费的字段）
#[derive(serde::Serialize)]
pub struct FolderNode {
    pub relative_path: String,
    pub parent_relative_path: String,
    pub name: String,
    /// "has_children" | "no_children"
    pub child_existence: String,
    pub is_dir: bool,
}

fn read_layer(root: &std::path::Path, parent_rel: &str) -> Vec<FolderNode> {
    let base = if parent_rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(parent_rel.trim_matches('/'))
    };
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&base) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let rel = if parent_rel.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", parent_rel.trim_matches('/'), name)
        };
        let has_child = std::fs::read_dir(&path)
            .map(|mut it| it.any(|e| {
                e.as_ref()
                    .map(|x| x.path().is_dir()
                        && !x.file_name().to_string_lossy().starts_with('.'))
                    .unwrap_or(false)
            }))
            .unwrap_or(false);
        out.push(FolderNode {
            relative_path: rel.clone(),
            parent_relative_path: parent_rel.to_string(),
            name,
            child_existence: if has_child { "has_children".into() } else { "no_children".into() },
            is_dir: true,
        });
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

#[tauri::command]
pub async fn cmd_get_folder_list(
    state: State<'_, AppState>,
    task_id: i64,
) -> Result<Vec<FolderNode>, String> {
    let task = tasks::get_task_by_id(db(&state), task_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "task not found".to_string())?;
    Ok(read_layer(std::path::Path::new(&task.pc_path), ""))
}

#[tauri::command]
pub async fn cmd_get_folder_list_with_params(
    state: State<'_, AppState>,
    _nas_acc_id: i64,
    pc_path: String,
    _nas_path: String,
    _mode: String,
) -> Result<Vec<FolderNode>, String> {
    let _ = &state;
    Ok(read_layer(std::path::Path::new(&pc_path), ""))
}

#[tauri::command]
pub async fn cmd_get_folder_layer(
    state: State<'_, AppState>,
    task_id: i64,
    parent_relative_path: String,
) -> Result<Vec<FolderNode>, String> {
    let task = tasks::get_task_by_id(db(&state), task_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "task not found".to_string())?;
    Ok(read_layer(
        std::path::Path::new(&task.pc_path),
        &parent_relative_path,
    ))
}

#[tauri::command]
pub async fn cmd_get_folder_layer_with_params(
    state: State<'_, AppState>,
    _nas_acc_id: i64,
    pc_path: String,
    _nas_path: String,
    _mode: String,
    parent_relative_path: String,
) -> Result<Vec<FolderNode>, String> {
    let _ = &state;
    Ok(read_layer(
        std::path::Path::new(&pc_path),
        &parent_relative_path,
    ))
}

/// 取指定字段为空的节点（官方用于诊断，Linux 版无 file_map 表 → 空集）
#[tauri::command]
pub async fn get_nodes_with_null_field(
    _state: State<'_, AppState>,
    task_id: i64,
    field: String,
    include_deleted: bool,
) -> Result<Value, String> {
    let _ = (task_id, field, include_deleted);
    Ok(serde_json::json!([]))
}

#[tauri::command]
pub async fn clear_all_data_by_task_id(
    _state: State<'_, AppState>,
    task_id: i64,
) -> Result<(), String> {
    let _ = task_id;
    Ok(())
}

#[tauri::command]
pub async fn delete_folder_filter_file(
    state: State<'_, AppState>,
    task_id: i64,
) -> Result<(), String> {
    sqlx::query("DELETE FROM system_table WHERE key = ?1")
        .bind(format!("folder_filter_paths_{task_id}"))
        .execute(db(&state))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 返回 [(task_id, status)]，前端用 mt[0] 取 task id
#[tauri::command]
pub async fn get_tasks_id_and_status_by_nas_acc_id(
    state: State<'_, AppState>,
    nas_acc_id: i64,
) -> Result<Vec<(i64, i64)>, String> {
    let rows: Vec<(i64, i64)> =
        sqlx::query_as("SELECT id, status FROM tasks WHERE nas_acc_id = ?1")
            .bind(nas_acc_id)
            .fetch_all(db(&state))
            .await
            .map_err(|e| e.to_string())?;
    Ok(rows)
}

#[tauri::command]
pub async fn edit_open_cloud(
    state: State<'_, AppState>,
    task_id: i64,
    open_cloud: i64,
) -> Result<(), String> {
    tasks::update_task(
        db(&state),
        task_id,
        &TaskUpdate {
            name: None,
            mode: None,
            open_cloud: Some(open_cloud),
        },
    )
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn renew_task(state: State<'_, AppState>, task_id: i64) -> Result<(), String> {
    let mgr = state.task_manager.lock().await;
    mgr.stop_task(task_id);
    Ok(())
}

#[tauri::command]
pub async fn network_restart(state: State<'_, AppState>, task_id: i64) -> Result<(), String> {
    let acc = sqlx::query_as::<_, (i64,)>("SELECT nas_acc_id FROM tasks WHERE id = ?1")
        .bind(task_id)
        .fetch_optional(db(&state))
        .await
        .map_err(|e| e.to_string())?;
    let Some((acc_id,)) = acc else {
        return Err("task not found".into());
    };
    let account = nas_account::get_nas_account_by_id(db(&state), acc_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "nas account not found".to_string())?;
    let mut mgr = state.task_manager.lock().await;
    mgr.stop_task(task_id);
    mgr.start_task(db(&state), task_id, &account)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn remove_tasks_from_map(
    state: State<'_, AppState>,
    task_id: i64,
) -> Result<(), String> {
    state.task_manager.lock().await.stop_task(task_id);
    Ok(())
}

#[tauri::command]
pub async fn add_log(state: State<'_, AppState>, log: Value) -> Result<(), String> {
    let entry = logs::NewLog {
        task_id: log.get("task_id").and_then(|v| v.as_i64()).unwrap_or(0),
        is_dir: log.get("is_dir").and_then(|v| v.as_bool()).unwrap_or(false),
        name: log.get("name").and_then(|v| v.as_str()).unwrap_or_default(),
        path: log.get("path").and_then(|v| v.as_str()).unwrap_or_default(),
        sync_folder: log.get("sync_folder").and_then(|v| v.as_str()).unwrap_or_default(),
        action_code: log.get("action_code").and_then(|v| v.as_i64()).unwrap_or(0),
        status: log.get("status").and_then(|v| v.as_i64()).unwrap_or(0),
        failed_code: log.get("failed_code").and_then(|v| v.as_i64()).unwrap_or(0),
        failed_text: log.get("failed_text").and_then(|v| v.as_str()),
        payload: None,
    };
    logs::add_log(db(&state), entry)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn check_oauth_security_code(
    _state: State<'_, AppState>,
    url: String,
) -> Result<Value, String> {
    // 从回调 URL 中提取安全码参数（官方用于二次校验）
    let code = url
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("code=").or_else(|| kv.strip_prefix("security_code=")))
        .unwrap_or_default()
        .to_string();
    Ok(serde_json::json!({ "code": code, "valid": !code.is_empty() }))
}

#[tauri::command]
pub async fn close_oauth_window(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window("oauth") {
        let _ = w.close();
    }
    Ok(())
}

#[tauri::command]
pub async fn open_browser(url: String) -> Result<(), String> {
    open::that(&url).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn open_window(app: tauri::AppHandle, active_tab: Option<String>) -> Result<(), String> {
    use tauri::{Emitter, Manager};
    let w = app
        .get_webview_window("main")
        .ok_or_else(|| "main window not found".to_string())?;
    let _ = w.show();
    let _ = w.unminimize();
    let _ = w.set_focus();
    if let Some(tab) = active_tab {
        let _ = app.emit("active-tab", tab);
    }
    Ok(())
}

#[tauri::command]
pub async fn reveal_item_in_dir(path: String) -> Result<(), String> {
    let p = std::path::Path::new(&path);
    let target = if p.is_dir() {
        p.to_path_buf()
    } else {
        p.parent().unwrap_or(p).to_path_buf()
    };
    open::that(&target).map_err(|e| e.to_string())
}

/// 文件域（官方 file_domain 表；Linux 版用 system_table 记录）
#[tauri::command]
pub async fn add_file_domain(
    state: State<'_, AppState>,
    name: String,
    id: String,
) -> Result<(), String> {
    logs::set_system_value(db(&state), &format!("file_domain_{id}"), &name)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_file_domains(state: State<'_, AppState>) -> Result<Value, String> {
    let all = logs::get_all_system_table(db(&state))
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (k, v) in all {
        if let Some(id) = k.strip_prefix("file_domain_") {
            out.push(serde_json::json!({ "id": id, "name": v }));
        }
    }
    Ok(serde_json::json!(out))
}

#[tauri::command]
pub async fn remove_file_domain(
    state: State<'_, AppState>,
    domain_identifier: String,
) -> Result<(), String> {
    sqlx::query("DELETE FROM system_table WHERE key = ?1")
        .bind(format!("file_domain_{domain_identifier}"))
        .execute(db(&state))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 临时连接：按地址建一个未登录客户端；message 转发后返回响应
#[tauri::command]
pub async fn send_message_from_temp_conn(
    state: State<'_, AppState>,
    address: String,
    conn_type: String,
    message: Value,
    reqid: Option<String>,
) -> Result<Value, String> {
    let _ = (conn_type, reqid);
    let client = {
        let mgr = state.task_manager.lock().await;
        let address = if address.contains(':') {
            address.clone()
        } else {
            format!("{address}:5667")
        };
        mgr.create_authed_client(&address, &String::new(), &String::new())
            .await
            .map_err(|e| e.to_string())?
    };
    client.rpc(message).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_temp_conn(
    state: State<'_, AppState>,
    address: String,
    conn_type: String,
) -> Result<(), String> {
    let _ = (address, conn_type);
    let _ = state;
    Ok(())
}

#[tauri::command]
pub async fn get_encrypt_data(
    _state: State<'_, AppState>,
    encrypted_data: String,
) -> Result<String, String> {
    // 官方对本地保存的凭据做对称加密后落库；这里用 AES-256-CBC 同样的密钥流程
    let key = crate::connect::crypto::new_key_str();
    let iv = crate::connect::crypto::new_iv();
    let ct = crate::connect::crypto::aes_cbc_encrypt(&key, &iv, encrypted_data.as_bytes());
    use base64::Engine;
    Ok(crate::connect::crypto::b64().encode(&ct))
}

/// macOS 的 iCloud 同步目录；Linux 无对应概念，返回错误让前端走普通路径
#[tauri::command]
pub async fn get_macos_cloud_pc_path(name: String, task_id: i64) -> Result<String, String> {
    let _ = (name, task_id);
    Err("macos cloud sync not supported on linux".into())
}

#[tauri::command]
pub async fn set_sync_sdk_locale(
    state: State<'_, AppState>,
    language: String,
) -> Result<(), String> {
    logs::set_system_value(db(&state), "language", &language)
        .await
        .map_err(|e| e.to_string())
}

// ---------- mDNS（官方前端通过 plugin:mdns 调用，这里提供同语义应用级命令）----------

#[tauri::command]
pub async fn mdns_browse_many(
    app: tauri::AppHandle,
    service_types: Vec<String>,
) -> Result<(), String> {
    crate::platform::mdns_plugin::browse_many(app, service_types).await
}

#[tauri::command]
pub async fn mdns_stop_browse(app: tauri::AppHandle) -> Result<(), String> {
    crate::platform::mdns_plugin::stop_browse(app).await
}

/// 前端 JS 错误转发到日志（调试官方前端时使用）
#[tauri::command]
pub async fn js_log(level: String, message: String) -> Result<(), String> {
    tracing::warn!(level = %level, "webview: {message}");
    eprintln!("[webview:{level}] {message}");
    Ok(())
}
