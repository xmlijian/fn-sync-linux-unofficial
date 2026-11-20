//! 日志服务 — logs 表 + log.sqlite 保存策略（log_save_day/log_save_size）。

use super::database::now_ms;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

// ActionCode（官方枚举）
pub const ACTION_UPLOAD: i64 = 1;
pub const ACTION_DOWNLOAD: i64 = 2;
pub const ACTION_DELETE: i64 = 3;
pub const ACTION_RENAME: i64 = 4;
pub const ACTION_EXCEPTION: i64 = 5;
pub const ACTION_META_CREATE: i64 = 6;
pub const ACTION_MOVE: i64 = 7;

// status
pub const STATUS_FAILED: i64 = 0;
pub const STATUS_SUCCESS: i64 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct LogEntry {
    pub id: i64,
    #[serde(alias = "taskId")]
    pub task_id: i64,
    pub is_dir: i64,
    pub name: String,
    pub path: String,
    pub sync_folder: String,
    #[serde(alias = "actionCode")]
    pub action_code: i64,
    pub status: i64,
    pub failed_code: i64,
    pub failed_text: Option<String>,
    pub payload: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct GetLogsParams {
    #[serde(default)]
    #[serde(alias = "startTime")]
    pub start_time: Option<i64>,
    #[serde(default)]
    #[serde(alias = "endTime")]
    pub end_time: Option<i64>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    #[serde(alias = "actionCode")]
    pub action_code: Option<i64>,
    #[serde(default)]
    pub status: Option<i64>,
    #[serde(default)]
    #[serde(alias = "taskId")]
    pub task_id: Option<i64>,
    #[serde(default = "default_page_size")]
    #[serde(alias = "pageSize")]
    pub page_size: i64,
    #[serde(default)]
    pub page: i64,
}

fn default_page_size() -> i64 {
    100
}

/// 一条日志的构造载荷（官方 add_log 参数）。
pub struct NewLog<'a> {
    pub task_id: i64,
    pub is_dir: bool,
    pub name: &'a str,
    pub path: &'a str,
    pub sync_folder: &'a str,
    pub action_code: i64,
    pub status: i64,
    pub failed_code: i64,
    pub failed_text: Option<&'a str>,
    pub payload: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
pub async fn add_log(pool: &SqlitePool, log: NewLog<'_>) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO logs (task_id, is_dir, name, path, sync_folder, action_code, status, \
         failed_code, failed_text, payload, created_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) RETURNING id",
    )
    .bind(log.task_id)
    .bind(if log.is_dir { 1 } else { 0 })
    .bind(log.name)
    .bind(log.path)
    .bind(log.sync_folder)
    .bind(log.action_code)
    .bind(log.status)
    .bind(log.failed_code)
    .bind(log.failed_text)
    .bind(log.payload)
    .bind(now_ms())
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn query_logs(pool: &SqlitePool, p: &GetLogsParams) -> Result<(Vec<LogEntry>, i64)> {
    let mut where_parts: Vec<String> = vec!["1=1".into()];
    if let Some(t) = p.start_time {
        where_parts.push(format!("created_at >= {t}"));
    }
    if let Some(t) = p.end_time {
        where_parts.push(format!("created_at <= {t}"));
    }
    if let Some(k) = &p.keyword {
        let like = k.replace('\'', "''");
        where_parts.push(format!("(name LIKE '%{like}%' OR path LIKE '%{like}%')"));
    }
    if let Some(a) = p.action_code {
        where_parts.push(format!("action_code = {a}"));
    }
    if let Some(s) = p.status {
        where_parts.push(format!("status = {s}"));
    }
    if let Some(t) = p.task_id {
        where_parts.push(format!("task_id = {t}"));
    }
    let where_clause = where_parts.join(" AND ");
    let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM logs WHERE {where_clause}"))
        .fetch_one(pool)
        .await?;
    let offset = p.page.saturating_mul(p.page_size);
    let sql = format!(
        "SELECT id, task_id, is_dir, name, path, sync_folder, action_code, status, failed_code, \
         failed_text, payload, created_at FROM logs WHERE {where_clause} \
         ORDER BY created_at DESC LIMIT {} OFFSET {}",
        p.page_size, offset
    );
    let rows = sqlx::query_as::<_, LogEntry>(&sql).fetch_all(pool).await?;
    Ok((rows, total))
}

pub async fn get_logs_by_size(pool: &SqlitePool, size: i64) -> Result<Vec<LogEntry>> {
    let rows = sqlx::query_as::<_, LogEntry>(
        "SELECT id, task_id, is_dir, name, path, sync_folder, action_code, status, failed_code, \
         failed_text, payload, created_at FROM logs ORDER BY created_at DESC LIMIT ?1",
    )
    .bind(size.clamp(1, 1000))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn clear_all_logs(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM logs").execute(pool).await?;
    Ok(())
}

/// 按保存天数/条数上限清理（check_and_clear_logs）。
pub async fn check_and_clear_logs(pool: &SqlitePool) -> Result<u64> {
    let save_day: i64 = get_system_value(pool, "log_save_day")
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(360);
    let save_size: i64 = get_system_value(pool, "log_save_size")
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let cutoff = now_ms() - save_day * 86_400_000;
    let mut removed: u64 = if save_day <= 0 {
        sqlx::query("DELETE FROM logs")
            .execute(pool)
            .await?
            .rows_affected()
    } else {
        sqlx::query("DELETE FROM logs WHERE created_at < ?1")
            .bind(cutoff)
            .execute(pool)
            .await?
            .rows_affected()
    };
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM logs")
        .fetch_one(pool)
        .await?;
    if count > save_size {
        let overflow = count - save_size;
        let r = sqlx::query(
            "DELETE FROM logs WHERE id IN (SELECT id FROM logs ORDER BY created_at ASC LIMIT ?1)",
        )
        .bind(overflow)
        .execute(pool)
        .await?;
        removed += r.rows_affected();
    }
    Ok(removed)
}

pub async fn get_system_value(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let v: Option<String> = sqlx::query_scalar("SELECT value FROM system_table WHERE key = ?1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(v)
}

pub async fn set_system_value(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO system_table (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_all_system_table(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<String, String>> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT key, value FROM system_table")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().collect())
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

    #[tokio::test]
    async fn add_and_query() {
        let (_d, p) = pool().await;
        add_log(
            &p,
            NewLog {
                task_id: 1,
                is_dir: false,
                name: "a.txt",
                path: "/sync/a.txt",
                sync_folder: "photos",
                action_code: ACTION_UPLOAD,
                status: STATUS_SUCCESS,
                failed_code: 0,
                failed_text: None,
                payload: None,
            },
        )
        .await
        .unwrap();
        add_log(
            &p,
            NewLog {
                task_id: 1,
                is_dir: false,
                name: "b.txt",
                path: "/sync/b.txt",
                sync_folder: "photos",
                action_code: ACTION_DOWNLOAD,
                status: STATUS_FAILED,
                failed_code: 1010,
                failed_text: Some("storage full"),
                payload: None,
            },
        )
        .await
        .unwrap();
        let (rows, total) = query_logs(
            &p,
            &GetLogsParams {
                start_time: None,
                end_time: None,
                keyword: None,
                action_code: None,
                status: None,
                task_id: None,
                page_size: 10,
                page: 0,
            },
        )
        .await
        .unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows.len(), 2); // created_at 同毫秒时顺序不定，只验证都返回
        let (rows, _) = query_logs(
            &p,
            &GetLogsParams {
                action_code: Some(ACTION_DOWNLOAD),
                start_time: None,
                end_time: None,
                keyword: None,
                status: None,
                task_id: None,
                page_size: 10,
                page: 0,
            },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].failed_code, 1010);
    }

    #[tokio::test]
    async fn retention_by_day() {
        let (_d, p) = pool().await;
        set_system_value(&p, "log_save_day", "0").await.unwrap();
        add_log(
            &p,
            NewLog {
                task_id: 1,
                is_dir: false,
                name: "x",
                path: "/x",
                sync_folder: "f",
                action_code: 1,
                status: 1,
                failed_code: 0,
                failed_text: None,
                payload: None,
            },
        )
        .await
        .unwrap();
        let removed = check_and_clear_logs(&p).await.unwrap();
        assert_eq!(removed, 1);
    }

    #[tokio::test]
    async fn system_table_roundtrip() {
        let (_d, p) = pool().await;
        set_system_value(&p, "language", "zh").await.unwrap();
        assert_eq!(
            get_system_value(&p, "language").await.unwrap().unwrap(),
            "zh"
        );
        let all = get_all_system_table(&p).await.unwrap();
        assert!(all.contains_key("language"));
    }
}

#[cfg(test)]
mod ui_input_tests {
    #[test]
    fn accepts_ui_camel_case_log_filters() {
        let params:super::GetLogsParams=serde_json::from_value(serde_json::json!({"taskId":2,"actionCode":3,"startTime":100,"endTime":200,"pageSize":25,"page":1})).unwrap();
        assert_eq!(params.task_id, Some(2));
        assert_eq!(params.action_code, Some(3));
        assert_eq!(params.start_time, Some(100));
        assert_eq!(params.end_time, Some(200));
        assert_eq!(params.page_size, 25);
    }
}
