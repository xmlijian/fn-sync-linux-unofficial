//! 任务服务 — tasks / task_filter_rules 表（命令与官方一致）。

use super::database::now_ms;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

pub const MAX_TASKS: i64 = 100;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Task {
    pub id: i64,
    pub nas_acc_id: i64,
    pub name: String,
    pub address: String,
    pub conn_type: String,
    pub uuid: String,
    pub identifier: String,
    pub mode: String,
    pub nas_path: String,
    pub pc_path: String,
    pub status: i64,
    pub is_init_sync: i64,
    pub open_cloud: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct TaskCreate {
    pub name: String,
    pub address: String,
    pub conn_type: String,
    pub uuid: String,
    pub identifier: String,
    pub mode: String,
    pub nas_path: String,
    pub pc_path: String,
    #[serde(default)]
    pub status: i64,
    pub nas_acc_id: i64,
    #[serde(default)]
    pub is_init_sync: i64,
    #[serde(default)]
    pub filter_rule: Option<FilterRuleInput>,
    #[serde(default)]
    pub open_cloud: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FilterRuleInput {
    #[serde(default = "default_true")]
    pub ignore_dotfiles: i64,
    #[serde(default = "default_true")]
    pub enable_ignore_ext: i64,
    #[serde(default, deserialize_with = "deserialize_extensions")]
    pub ignore_extensions: Vec<String>,
    #[serde(default)]
    pub enable_size_lower: i64,
    #[serde(default, deserialize_with = "deserialize_optional_integer")]
    pub size_lower_bound: Option<i64>,
    #[serde(default)]
    pub enable_size_upper: i64,
    #[serde(default, deserialize_with = "deserialize_optional_integer")]
    pub size_upper_bound: Option<i64>,
}

fn deserialize_extensions<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input { List(Vec<String>), Csv(String) }
    Ok(match Input::deserialize(d)? {
        Input::List(list) => list,
        Input::Csv(csv) => csv.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect(),
    })
}

fn deserialize_optional_integer<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<i64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input { Number(i64), Text(String) }
    match Option::<Input>::deserialize(d)? {
        None => Ok(None),
        Some(Input::Number(n)) => Ok(Some(n)),
        Some(Input::Text(s)) if s.trim().is_empty() => Ok(None),
        Some(Input::Text(s)) => s.parse().map(Some).map_err(serde::de::Error::custom),
    }
}

fn default_true() -> i64 {
    1
}

impl FilterRuleInput {
    pub fn default_rule() -> Self {
        Self {
            ignore_dotfiles: 1,
            enable_ignore_ext: 1,
            ignore_extensions: vec!["lnk".into(), "pst".into(), "swp".into()],
            enable_size_lower: 0,
            size_lower_bound: None,
            enable_size_upper: 0,
            size_upper_bound: None,
        }
    }
}

pub const VALID_MODES: &[&str] = &["bisynchronous", "uploadonly", "downloadonly"];

pub fn validate_app_name(name: &str) -> Result<()> {
    if name.chars().count() > 32 {
        anyhow::bail!("task.appNameTooLong");
    }
    if name.trim().is_empty() {
        anyhow::bail!("task.checkNameFailed");
    }
    Ok(())
}

pub async fn add_task(pool: &SqlitePool, task: &TaskCreate) -> Result<i64> {
    validate_app_name(&task.name)?;
    if !VALID_MODES.contains(&task.mode.as_str()) {
        anyhow::bail!("unsupported sync mode: {}", task.mode);
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(pool)
        .await?;
    if count >= MAX_TASKS {
        anyhow::bail!("task.maxTasksReached");
    }
    let dup: Option<i64> = sqlx::query_scalar("SELECT id FROM tasks WHERE name = ?1")
        .bind(&task.name)
        .fetch_optional(pool)
        .await?;
    if dup.is_some() {
        anyhow::bail!("task.nameExistsWithName");
    }
    let now = now_ms();
    let next_id: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(id), 0) + 1 FROM tasks",
    )
    .fetch_one(pool)
    .await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO tasks (id, nas_acc_id, name, address, conn_type, uuid, identifier, mode, \
         nas_path, pc_path, status, is_init_sync, open_cloud, created_at, updated_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15) RETURNING id",
    )
    .bind(next_id)
    .bind(task.nas_acc_id)
    .bind(&task.name)
    .bind(&task.address)
    .bind(&task.conn_type)
    .bind(&task.uuid)
    .bind(&task.identifier)
    .bind(&task.mode)
    .bind(&task.nas_path)
    .bind(&task.pc_path)
    .bind(task.status)
    .bind(task.is_init_sync)
    .bind(task.open_cloud)
    .bind(now)
    .bind(now)
    .fetch_one(pool)
    .await
    .context("insert task")?;

    let rule = task.filter_rule.clone().unwrap_or_default();
    sqlx::query(
        "INSERT INTO task_filter_rules (task_id, ignore_dotfiles, enable_ignore_ext, \
         ignore_extensions, enable_size_lower, size_lower_bound, enable_size_upper, \
         size_upper_bound, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
    )
    .bind(id)
    .bind(rule.ignore_dotfiles)
    .bind(rule.enable_ignore_ext)
    .bind(rule.ignore_extensions.join(","))
    .bind(rule.enable_size_lower)
    .bind(rule.size_lower_bound)
    .bind(rule.enable_size_upper)
    .bind(rule.size_upper_bound)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn get_all_tasks(pool: &SqlitePool) -> Result<Vec<Task>> {
    let rows = sqlx::query_as::<_, Task>(
        "SELECT id, nas_acc_id, name, address, conn_type, uuid, identifier, mode, nas_path, \
         pc_path, status, is_init_sync, open_cloud, created_at, updated_at \
         FROM tasks ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_task_by_id(pool: &SqlitePool, id: i64) -> Result<Option<Task>> {
    let row = sqlx::query_as::<_, Task>(
        "SELECT id, nas_acc_id, name, address, conn_type, uuid, identifier, mode, nas_path, \
         pc_path, status, is_init_sync, open_cloud, created_at, updated_at \
         FROM tasks WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

#[derive(Debug, Deserialize)]
pub struct TaskUpdate {
    pub name: Option<String>,
    pub mode: Option<String>,
    pub open_cloud: Option<i64>,
}

pub async fn update_task(pool: &SqlitePool, id: i64, upd: &TaskUpdate) -> Result<()> {
    if let Some(name) = &upd.name {
        validate_app_name(name)?;
        let dup: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM tasks WHERE name = ?1 AND id != ?2",
        )
        .bind(name)
        .bind(id)
        .fetch_optional(pool)
        .await?;
        if dup.is_some() {
            anyhow::bail!("task.nameExistsWithName");
        }
    }
    sqlx::query(
        "UPDATE tasks SET name = COALESCE(?1, name), mode = COALESCE(?2, mode), \
         open_cloud = COALESCE(?3, open_cloud), updated_at = ?4 WHERE id = ?5",
    )
    .bind(&upd.name)
    .bind(&upd.mode)
    .bind(upd.open_cloud)
    .bind(now_ms())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_task_status(pool: &SqlitePool, id: i64, status: i64) -> Result<()> {
    sqlx::query("UPDATE tasks SET status=?1, updated_at=?2 WHERE id=?3")
        .bind(status)
        .bind(now_ms())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_task(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM task_filter_rules WHERE task_id=?1")
        .bind(id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM tasks WHERE id=?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_filter_rule_by_task_id(
    pool: &SqlitePool,
    task_id: i64,
) -> Result<Option<FilterRuleInput>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        ignore_dotfiles: i64,
        enable_ignore_ext: i64,
        ignore_extensions: Option<String>,
        enable_size_lower: i64,
        size_lower_bound: Option<i64>,
        enable_size_upper: i64,
        size_upper_bound: Option<i64>,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT ignore_dotfiles, enable_ignore_ext, ignore_extensions, enable_size_lower, \
         size_lower_bound, enable_size_upper, size_upper_bound \
         FROM task_filter_rules WHERE task_id = ?1",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| FilterRuleInput {
        ignore_dotfiles: r.ignore_dotfiles,
        enable_ignore_ext: r.enable_ignore_ext,
        ignore_extensions: r
            .ignore_extensions
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        enable_size_lower: r.enable_size_lower,
        size_lower_bound: r.size_lower_bound,
        enable_size_upper: r.enable_size_upper,
        size_upper_bound: r.size_upper_bound,
    }))
}

pub async fn save_folder_filter_paths(
    pool: &SqlitePool,
    task_id: i64,
    paths: &[String],
) -> Result<()> {
    // 官方把相对路径选择集存于 system_table（key = task 前缀）；沿用同一约定
    let key = format!("folder_filter_paths_{task_id}");
    let value = serde_json::to_string(paths)?;
    sqlx::query(
        "INSERT INTO system_table (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(&key)
    .bind(&value)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_folder_filter_paths(pool: &SqlitePool, task_id: i64) -> Result<Vec<String>> {
    let key = format!("folder_filter_paths_{task_id}");
    let v: Option<String> =
        sqlx::query_scalar("SELECT value FROM system_table WHERE key = ?1")
            .bind(&key)
            .fetch_optional(pool)
            .await?;
    Ok(v.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::database::{migrate, open_db};

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let d = tempfile::tempdir().unwrap();
        let p = open_db(&d.path().join("db.sqlite")).await.unwrap();
        migrate(&p).await.unwrap();
        (d, p)
    }

    fn task(name: &str, mode: &str) -> TaskCreate {
        TaskCreate {
            name: name.into(),
            address: "192.168.1.10:5667".into(),
            conn_type: "ip".into(),
            uuid: "1700000000000".into(),
            identifier: "tieren.SyncFile-admin-192.168.1.10:5667".into(),
            mode: mode.into(),
            nas_path: "/vol1/photos".into(),
            pc_path: "/home/user/photos".into(),
            status: 0,
            nas_acc_id: 1,
            is_init_sync: 0,
            filter_rule: Some(FilterRuleInput::default_rule()),
            open_cloud: 0,
        }
    }

    #[tokio::test]
    async fn add_task_with_default_filter() {
        let (_d, p) = pool().await;
        let id = add_task(&p, &task("photos", "bisynchronous")).await.unwrap();
        assert_eq!(id, 1);
        let rule = get_filter_rule_by_task_id(&p, id).await.unwrap().unwrap();
        assert_eq!(rule.ignore_dotfiles, 1);
        assert_eq!(rule.ignore_extensions, vec!["lnk", "pst", "swp"]);
        let t = get_task_by_id(&p, id).await.unwrap().unwrap();
        assert_eq!(t.mode, "bisynchronous");
    }

    #[tokio::test]
    async fn duplicate_name_rejected() {
        let (_d, p) = pool().await;
        add_task(&p, &task("a", "bisynchronous")).await.unwrap();
        assert!(add_task(&p, &task("a", "uploadonly")).await.is_err());
    }

    #[tokio::test]
    async fn name_too_long() {
        let (_d, p) = pool().await;
        let long = "x".repeat(33);
        assert!(add_task(&p, &task(&long, "bisynchronous")).await.is_err());
    }

    #[tokio::test]
    async fn invalid_mode_rejected() {
        let (_d, p) = pool().await;
        assert!(add_task(&p, &task("m", "bothway")).await.is_err());
    }

    #[tokio::test]
    async fn update_and_delete() {
        let (_d, p) = pool().await;
        let id = add_task(&p, &task("t1", "bisynchronous")).await.unwrap();
        update_task(
            &p,
            id,
            &TaskUpdate {
                name: None,
                mode: Some("uploadonly".into()),
                open_cloud: Some(1),
            },
        )
        .await
        .unwrap();
        let t = get_task_by_id(&p, id).await.unwrap().unwrap();
        assert_eq!(t.mode, "uploadonly");
        assert_eq!(t.open_cloud, 1);
        update_task_status(&p, id, 4).await.unwrap();
        assert_eq!(get_task_by_id(&p, id).await.unwrap().unwrap().status, 4);
        delete_task(&p, id).await.unwrap();
        assert!(get_task_by_id(&p, id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn folder_filter_paths_roundtrip() {
        let (_d, p) = pool().await;
        save_folder_filter_paths(&p, 1, &["sub".into(), "sub/deep".into()])
            .await
            .unwrap();
        let v = get_folder_filter_paths(&p, 1).await.unwrap();
        assert_eq!(v, vec!["sub", "sub/deep"]);
    }
}

#[cfg(test)]
mod ipc_input_tests {
    #[test]
    fn accepts_official_ui_filter_payload() {
        let input: super::FilterRuleInput = serde_json::from_value(serde_json::json!({
            "ignore_extensions":"lnk,pst,swp", "size_lower_bound":"0", "size_upper_bound":"1048576"
        })).unwrap();
        assert_eq!(input.ignore_extensions, vec!["lnk", "pst", "swp"]);
        assert_eq!(input.size_lower_bound, Some(0));
        assert_eq!(input.size_upper_bound, Some(1048576));
    }
    #[test]
    fn accepts_native_filter_payload_and_rejects_invalid_numbers() {
        let input: super::FilterRuleInput = serde_json::from_value(serde_json::json!({
            "ignore_extensions":["tmp"], "size_lower_bound":0, "size_upper_bound":null
        })).unwrap();
        assert_eq!(input.ignore_extensions, vec!["tmp"]);
        assert_eq!(input.size_upper_bound, None);
        assert!(serde_json::from_value::<super::FilterRuleInput>(serde_json::json!({"size_lower_bound":"invalid"})).is_err());
    }
}
