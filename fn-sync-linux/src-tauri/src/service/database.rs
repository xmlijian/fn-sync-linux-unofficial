//! SQLite 服务层 — schema 与官方 0.2.3 完全一致（PROTOCOL.md §7）。

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;

pub const DATABASE_MIGRATIONS: &[&str] = &[
    // v1: 初始 schema（与官方 CREATE TABLE 完全一致）
    r#"
    CREATE TABLE IF NOT EXISTS system_table (
        key VARCHAR PRIMARY KEY ON CONFLICT IGNORE,
        value VARCHAR NOT NULL
    );
    INSERT INTO system_table (key, value) VALUES
        ('is_complete_first_run', '0'),
        ('auto_update', '0'),
        ('log_save_day', '360'),
        ('log_save_size', '1000000');
    INSERT OR IGNORE INTO system_table (key, value) VALUES ('language', '');

    CREATE TABLE IF NOT EXISTS nas_accounts (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        nas_id TEXT NOT NULL,
        uid INTEGER NOT NULL,
        nas_name TEXT NOT NULL,
        address TEXT NOT NULL,
        username TEXT NOT NULL,
        conn_type TEXT NOT NULL,
        secret_data TEXT NOT NULL DEFAULT '',
        long_token TEXT NOT NULL DEFAULT '',
        token TEXT NOT NULL DEFAULT '',
        failed_code INTEGER NOT NULL DEFAULT 0,
        pub_key TEXT NOT NULL,
        secret TEXT NOT NULL,
        si TEXT NOT NULL,
        computer_name TEXT DEFAULT '',
        fnos_version TEXT DEFAULT '',
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS tasks (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        nas_acc_id INTEGER NOT NULL,
        name TEXT NOT NULL,
        address TEXT NOT NULL,
        conn_type TEXT NOT NULL,
        uuid TEXT NOT NULL,
        identifier TEXT NOT NULL,
        mode TEXT NOT NULL,
        nas_path TEXT NOT NULL,
        pc_path TEXT NOT NULL,
        status INTEGER NOT NULL,
        is_init_sync INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS task_filter_rules (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        task_id INTEGER NOT NULL,
        ignore_dotfiles INTEGER NOT NULL,
        enable_ignore_ext INTEGER NOT NULL,
        ignore_extensions TEXT,
        enable_size_lower INTEGER NOT NULL,
        size_lower_bound INTEGER,
        enable_size_upper INTEGER NOT NULL,
        size_upper_bound INTEGER,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_task_filter_rules_task_id ON task_filter_rules(task_id);

    CREATE TABLE IF NOT EXISTS logs (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        task_id INTEGER NOT NULL,
        is_dir INTEGER NOT NULL,
        name TEXT NOT NULL,
        path TEXT NOT NULL,
        sync_folder TEXT NOT NULL,
        action_code INTEGER NOT NULL,
        status INTEGER NOT NULL,
        failed_code INTEGER NOT NULL DEFAULT 0,
        failed_text TEXT,
        payload TEXT,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_logs_created_at ON logs(created_at DESC);
    CREATE INDEX IF NOT EXISTS idx_logs_action_code ON logs(action_code);
    CREATE INDEX IF NOT EXISTS idx_logs_name ON logs(name);
    CREATE INDEX IF NOT EXISTS idx_logs_status ON logs(status);
    "#,
    // v2: open_cloud 列（官方迁移 "Add open_cloud field to tasks table"）
    "ALTER TABLE tasks ADD COLUMN open_cloud INTEGER NOT NULL DEFAULT 0;",
    // v3: cookie 列（官方迁移 "add nas account cookie"）
    "ALTER TABLE nas_accounts ADD COLUMN cookie TEXT NOT NULL DEFAULT '';",
];

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub async fn open_db(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .context("open database")?;
    Ok(pool)
}

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    // 记录已应用的迁移版本于 system_table
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL)",
    )
    .execute(pool)
    .await?;
    for (idx, sql) in DATABASE_MIGRATIONS.iter().enumerate() {
        let version = (idx + 1) as i64;
        let applied: Option<i64> =
            sqlx::query_scalar("SELECT version FROM schema_migrations WHERE version = ?1")
                .bind(version)
                .fetch_optional(pool)
                .await?;
        if applied.is_none() {
            // ALTER TABLE 迁移若列已存在会失败 — 官方迁移同样幂等处理；容错
            if let Err(e) = sqlx::query(sql).execute(pool).await {
                let msg = e.to_string();
                if !msg.contains("duplicate column name") {
                    return Err(e).context(format!("migration v{version}"));
                }
            }
            sqlx::query("INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)")
                .bind(version)
                .bind(now_ms())
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migration_creates_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("database.sqlite");
        let pool = open_db(&db).await.unwrap();
        migrate(&pool).await.unwrap();

        let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM system_table")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(keys.contains(&"is_complete_first_run".to_string()));
        assert!(keys.contains(&"language".to_string()));

        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        for t in [
            "nas_accounts",
            "tasks",
            "task_filter_rules",
            "logs",
            "system_table",
        ] {
            assert!(tables.contains(&t.to_string()), "missing {t}");
        }
    }

    #[tokio::test]
    async fn migration_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let pool = open_db(&dir.path().join("db.sqlite")).await.unwrap();
        migrate(&pool).await.unwrap();
        migrate(&pool).await.unwrap(); // 第二次不报错
    }
}
