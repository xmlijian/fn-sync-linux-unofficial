//! Exercise actual Tauri argument deserialization and command responses used by the UI.
use crate::{commands, AppState};
use serde_json::{json, Value};
use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime};

fn invoke(window: &tauri::WebviewWindow<MockRuntime>, cmd: &str, args: Value) -> Value {
    tauri::test::get_ipc_response(
        window,
        tauri::webview::InvokeRequest {
            cmd: cmd.into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: "tauri://localhost".parse().unwrap(),
            body: tauri::ipc::InvokeBody::Json(args),
            headers: Default::default(),
            invoke_key: tauri::test::INVOKE_KEY.into(),
        },
    )
    .unwrap_or_else(|e| panic!("{cmd} rejected: {e:?}"))
    .deserialize()
    .unwrap()
}

#[test]
fn ui_task_creation_settings_logs_and_deletion_contracts() {
    let temp = tempfile::tempdir().unwrap();
    let db = tauri::async_runtime::block_on(async {
        let db = crate::service::database::open_db(&temp.path().join("db.sqlite"))
            .await
            .unwrap();
        crate::service::database::migrate(&db).await.unwrap();
        db
    });
    let state = AppState {
        db,
        task_manager: tokio::sync::Mutex::new(crate::task_manager::TaskManager::new()),
        last_auth_result: parking_lot::Mutex::new(None),
        oauth_proxy: parking_lot::Mutex::new(None),
        traffic: crate::traffic::TrafficCounter::new(),
    };
    let app = mock_builder()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::add_task,
            commands::get_task_by_id,
            commands::get_all_tasks,
            commands::delete_task,
            commands::pause_task,
            commands::get_filter_rule_by_task_id,
            commands::update_task,
            commands::edit_open_cloud,
            commands::query_logs,
            commands::add_log,
            commands::get_global_tasks_status,
            commands::get_task_status_summary,
            commands::is_path_sync_root_command,
            commands::is_external_device_path,
            commands::save_folder_filter_paths,
            commands::get_folder_filter_paths,
            commands::cmd_get_folder_list_with_params,
            commands::export_log_file,
            commands::validate_app_name,
            commands::update_system_table,
            commands::get_all_system_table
        ])
        .build(mock_context(noop_assets()))
        .unwrap();
    let window = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();
    let local = temp.path().join("local");
    std::fs::create_dir_all(local.join("中文目录")).unwrap();
    assert_eq!(
        invoke(&window, "is_path_sync_root_command", json!({"path":local})),
        false
    );
    assert_eq!(
        invoke(&window, "is_external_device_path", json!({"path":local})),
        false
    );
    let id=invoke(&window,"add_task",json!({"task":{
        "name":"UI task","address":"test","conn_type":"ip","uuid":"1","identifier":"qa",
        "mode":"bisynchronous","nas_path":"vol1/1000/test","pc_path":local,"nas_acc_id":0,"open_cloud":true,
        "filter_rule":{"ignore_dotfiles":1,"enable_ignore_ext":1,"ignore_extensions":"lnk,pst,swp",
            "enable_size_lower":0,"size_lower_bound":"0","enable_size_upper":0,"size_upper_bound":"0"}
    }})).as_i64().unwrap();
    let rule = invoke(&window, "get_filter_rule_by_task_id", json!({"taskId":id}));
    assert_eq!(rule["ignore_extensions"], "lnk,pst,swp");
    invoke(
        &window,
        "edit_open_cloud",
        json!({"taskId":id,"openCloud":false}),
    );
    invoke(
        &window,
        "update_task",
        json!({"id":id,"updateData":{"mode":"downloadonly","filter_rule":{"ignore_extensions":"bak","size_lower_bound":"10"}}}),
    );
    assert_eq!(
        invoke(&window, "get_task_by_id", json!({"id":id}))["mode"],
        "downloadonly"
    );
    assert_eq!(
        invoke(&window, "get_filter_rule_by_task_id", json!({"taskId":id}))["ignore_extensions"],
        "bak"
    );
    invoke(
        &window,
        "add_log",
        json!({"log":{"task_id":id,"name":"uploaded.txt","path":"uploaded.txt","sync_folder":"UI task","action_code":1,"status":1}}),
    );
    let logs = invoke(
        &window,
        "query_logs",
        json!({"params":{"taskId":id,"pageSize":25,"page":1,"actionCode":null,"startTime":"","endTime":""}}),
    );
    assert!(logs["list"].is_array());
    assert_eq!(logs["total"], 1);
    assert_eq!(logs["list"].as_array().unwrap().len(), 1);
    let page_two = invoke(
        &window,
        "query_logs",
        json!({"params":{"taskId":id,"pageSize":1,"page":2}}),
    );
    assert!(page_two["list"].as_array().unwrap().is_empty());
    let filtered = invoke(
        &window,
        "query_logs",
        json!({"params":{"taskId":id+1,"pageSize":25,"page":1}}),
    );
    assert_eq!(filtered["total"], 0);
    invoke(
        &window,
        "update_system_table",
        json!({"key":"global_upload_speed_limit","value":"64"}),
    );
    let started = std::time::Instant::now();
    tauri::async_runtime::block_on(crate::traffic::throttle(4096, true));
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(50),
        "saved speed setting did not apply immediately"
    );
    invoke(
        &window,
        "update_system_table",
        json!({"key":"global_upload_speed_limit","value":"-1"}),
    );
    let settings = invoke(&window, "get_all_system_table", json!({}));
    assert!(settings
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["key"] == "global_upload_speed_limit" && row["value"] == "-1"));
    invoke(
        &window,
        "update_system_table",
        json!({"key":"log_save_size","value":"0"}),
    );
    assert_eq!(
        invoke(
            &window,
            "query_logs",
            json!({"params":{"startTime":"","endTime":"","page":1}})
        )["total"],
        1
    );
    invoke(
        &window,
        "add_log",
        json!({"log":{"task_id":id,"name":"most-recent.txt","path":"most-recent.txt","action_code":1,"status":1}}),
    );
    invoke(
        &window,
        "update_system_table",
        json!({"key":"log_save_size","value":"1"}),
    );
    let retained = invoke(
        &window,
        "query_logs",
        json!({"params":{"startTime":"","endTime":"","page":1}}),
    );
    assert_eq!(retained["total"], 1);
    assert_eq!(retained["list"][0]["name"], "most-recent.txt");
    let exported = invoke(
        &window,
        "export_log_file",
        json!({"path":temp.path().join("同步日志")}),
    )
    .as_str()
    .unwrap()
    .to_string();
    assert!(std::fs::read(&exported).unwrap().starts_with(&[0x1f, 0x8b]));
    assert!(std::process::Command::new("tar")
        .arg("-tzf")
        .arg(&exported)
        .output()
        .unwrap()
        .status
        .success());
    assert!(exported.ends_with("同步日志.tar.gz"));
    let records = std::process::Command::new("tar")
        .arg("-xOzf")
        .arg(&exported)
        .arg("sync-logs.jsonl")
        .output()
        .unwrap();
    assert!(records.status.success());
    let record: Value = serde_json::from_slice(&records.stdout).unwrap();
    assert_eq!(record["name"], "most-recent.txt");
    std::fs::remove_file(exported).unwrap();
    let counts = invoke(&window, "get_global_tasks_status", json!({}));
    assert_eq!(counts["task_count"], 1);
    for key in [
        "paused_count",
        "connecting_count",
        "account_exception_count",
        "permission_exception_count",
        "task_exception_count",
        "sync_failed_count",
        "processing_count",
        "execute_count",
        "completed_count",
        "sync_error_count",
    ] {
        assert!(counts[key].is_number(), "missing {key}");
    }
    invoke(&window, "pause_task", json!({"taskId":id}));
    assert_eq!(
        invoke(&window, "get_task_by_id", json!({"id":id}))["status"],
        3
    );
    let summary = invoke(&window, "get_task_status_summary", json!({"taskId":id}));
    assert!(summary["processing_count"].is_number());
    invoke(
        &window,
        "save_folder_filter_paths",
        json!({"taskId":id,"folderPaths":["中文目录"]}),
    );
    assert_eq!(
        invoke(&window, "get_folder_filter_paths", json!({"taskId":id})),
        json!(["中文目录"])
    );
    let dirs = invoke(
        &window,
        "cmd_get_folder_list_with_params",
        json!({"nasAccId":0,"pcPath":local,"nasPath":"vol1/1000/test","mode":"uploadonly"}),
    );
    assert!(dirs.is_array());
    if let Ok(destination) = std::env::var("FN_SYNC_UI_FIXTURES") {
        let fixtures = json!({
            "get_all_tasks":invoke(&window,"get_all_tasks",json!({})),
            "get_filter_rule_by_task_id":invoke(&window,"get_filter_rule_by_task_id",json!({"taskId":id})),
            "query_logs":logs,"get_global_tasks_status":counts,"get_task_status_summary":summary,
            "get_all_system_table":[{"key":"language","value":"zh"}],
            "get_all_nas_accounts":[{"id":0,"nas_name":"QA NAS","address":"qa.invalid:5667","username":"qa","uid":1000,"conn_type":"ip","failed_code":0}],
            "get_extended_logs":[],"get_logs_by_size":[],"get_folder_filter_paths":["中文目录"],
            "cmd_get_folder_list_with_params":dirs,"cmd_get_folder_list":dirs,"cmd_get_folder_layer":[],
            "get_device_id":"qa-device","get_git_hash":"qa","get_traffic_query":{"tx":0,"rx":0},
            "get_autostart_state":false,"is_webview_debug":true
        });
        std::fs::write(destination, serde_json::to_vec(&fixtures).unwrap()).unwrap();
    }
    invoke(&window, "delete_task", json!({"id":id}));
    assert_eq!(invoke(&window, "get_all_tasks", json!({})), json!([]));
}

#[test]
#[ignore = "read-only check against FN_SYNC_TEST_DB with existing sync logs"]
fn saved_sync_logs_are_visible_with_actual_ui_date_payload() {
    let path = std::env::var("FN_SYNC_TEST_DB").expect("existing sync database path");
    assert!(std::path::Path::new(&path).is_file());
    let db = tauri::async_runtime::block_on(crate::service::database::open_db(
        std::path::Path::new(&path),
    ))
    .unwrap();
    let state = AppState {
        db,
        task_manager: tokio::sync::Mutex::new(crate::task_manager::TaskManager::new()),
        last_auth_result: parking_lot::Mutex::new(None),
        oauth_proxy: parking_lot::Mutex::new(None),
        traffic: crate::traffic::TrafficCounter::new(),
    };
    let app = mock_builder()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::query_logs,
            commands::export_log_file
        ])
        .build(mock_context(noop_assets()))
        .unwrap();
    let window = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();
    let result = invoke(
        &window,
        "query_logs",
        json!({"params":{"isDir":null,"keyword":"","taskId":null,"status":null,"actionCode":null,"pageSize":25,"startTime":"","endTime":"","page":1}}),
    );
    let total = result["total"].as_i64().unwrap();
    assert!(total > 0, "expected saved sync records");
    assert_eq!(
        result["list"].as_array().unwrap().len(),
        total.min(25) as usize
    );
    let start = (crate::service::database::now_ms() - 7 * 86_400_000).to_string();
    let dated = invoke(
        &window,
        "query_logs",
        json!({"params":{"startTime":start,"endTime":"","pageSize":25,"page":1}}),
    );
    assert!(
        dated["total"].as_i64().unwrap() > 0,
        "recent sync logs missing with string timestamp filter"
    );
    let temp = tempfile::tempdir().unwrap();
    let archive = invoke(
        &window,
        "export_log_file",
        json!({"path":temp.path().join("existing-sync-logs.tar.gz")}),
    );
    let archive = archive.as_str().unwrap();
    let exported = std::process::Command::new("tar")
        .arg("-xOzf")
        .arg(archive)
        .arg("sync-logs.jsonl")
        .output()
        .unwrap();
    assert!(exported.status.success());
    let count = String::from_utf8(exported.stdout).unwrap().lines().count();
    assert!(count > 0, "export contained no saved sync records");
    eprintln!("Verified real sync log export: {count} records");
    eprintln!(
        "Verified saved sync logs: {total}; first page: {}",
        result["list"].as_array().unwrap().len()
    );
}
