//! NAS 账号服务 — nas_accounts 表 CRUD（命令名与官方 Tauri 命令一致）。

use super::database::now_ms;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct NasAccount {
    pub id: i64,
    pub nas_id: String,
    pub uid: i64,
    pub nas_name: String,
    pub address: String,
    pub username: String,
    pub conn_type: String,
    pub secret_data: String,
    pub long_token: String,
    pub token: String,
    pub failed_code: i64,
    pub pub_key: String,
    pub secret: String,
    pub si: String,
    pub computer_name: String,
    pub fnos_version: String,
    pub cookie: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct NasAccountCreate {
    pub nas_id: String,
    pub uid: i64,
    pub nas_name: String,
    pub address: String,
    pub username: String,
    pub conn_type: String,
    #[serde(default)]
    pub secret_data: String,
    #[serde(default)]
    pub long_token: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub pub_key: String,
    #[serde(default)]
    pub secret: String,
    #[serde(default)]
    pub si: String,
    #[serde(default)]
    pub computer_name: String,
    #[serde(default)]
    pub fnos_version: String,
    #[serde(default)]
    pub cookie: String,
}

pub const MAX_NAS_ACCOUNTS: i64 = 20;

pub async fn add_nas_account(pool: &SqlitePool, acc: &NasAccountCreate) -> Result<i64> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nas_accounts")
        .fetch_one(pool)
        .await?;
    if count >= MAX_NAS_ACCOUNTS {
        anyhow::bail!("add_nas_account_failed");
    }
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM nas_accounts WHERE nas_id = ?1")
        .bind(&acc.nas_id)
        .fetch_optional(pool)
        .await?;
    if exists.is_some() {
        anyhow::bail!("account.checkSameNasIdFailed");
    }
    let now = now_ms();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO nas_accounts (nas_id, uid, nas_name, address, username, conn_type, \
         secret_data, long_token, token, pub_key, secret, si, computer_name, fnos_version, cookie, \
         created_at, updated_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17) RETURNING id",
    )
    .bind(&acc.nas_id)
    .bind(acc.uid)
    .bind(&acc.nas_name)
    .bind(&acc.address)
    .bind(&acc.username)
    .bind(&acc.conn_type)
    .bind(&acc.secret_data)
    .bind(&acc.long_token)
    .bind(&acc.token)
    .bind(&acc.pub_key)
    .bind(&acc.secret)
    .bind(&acc.si)
    .bind(&acc.computer_name)
    .bind(&acc.fnos_version)
    .bind(&acc.cookie)
    .bind(now)
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn get_all_nas_accounts(pool: &SqlitePool) -> Result<Vec<NasAccount>> {
    let rows = sqlx::query_as::<_, NasAccount>(
        "SELECT id, nas_id, uid, nas_name, address, username, conn_type, secret_data, long_token, \
         token, failed_code, pub_key, secret, si, computer_name, fnos_version, cookie, \
         created_at, updated_at FROM nas_accounts ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_nas_account_by_id(pool: &SqlitePool, id: i64) -> Result<Option<NasAccount>> {
    let row = sqlx::query_as::<_, NasAccount>(
        "SELECT id, nas_id, uid, nas_name, address, username, conn_type, secret_data, long_token, \
         token, failed_code, pub_key, secret, si, computer_name, fnos_version, cookie, \
         created_at, updated_at FROM nas_accounts WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn get_nas_account_by_nas_id(
    pool: &SqlitePool,
    nas_id: &str,
) -> Result<Option<NasAccount>> {
    let row = sqlx::query_as::<_, NasAccount>(
        "SELECT id, nas_id, uid, nas_name, address, username, conn_type, secret_data, long_token, \
         token, failed_code, pub_key, secret, si, computer_name, fnos_version, cookie, \
         created_at, updated_at FROM nas_accounts WHERE nas_id = ?1",
    )
    .bind(nas_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn get_nas_account_by_address_and_username(
    pool: &SqlitePool,
    address: &str,
    username: &str,
) -> Result<Option<NasAccount>> {
    let row = sqlx::query_as::<_, NasAccount>(
        "SELECT id, nas_id, uid, nas_name, address, username, conn_type, secret_data, long_token, \
         token, failed_code, pub_key, secret, si, computer_name, fnos_version, cookie, \
         created_at, updated_at FROM nas_accounts WHERE address = ?1 AND username = ?2",
    )
    .bind(address)
    .bind(username)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// 更新 OAuth 凭据（update_nas_account_by_auth）。
#[allow(clippy::too_many_arguments)]
pub async fn update_nas_account_by_auth(
    pool: &SqlitePool,
    id: i64,
    username: &str,
    token: &str,
    long_token: &str,
    secret: &str,
    cookie: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE nas_accounts SET username=?1, token=?2, long_token=?3, secret=?4, cookie=?5, updated_at=?6 WHERE id=?7",
    )
    .bind(username)
    .bind(token)
    .bind(long_token)
    .bind(secret)
    .bind(cookie)
    .bind(now_ms())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_nas_conn_type_by_id(
    pool: &SqlitePool,
    id: i64,
    conn_type: &str,
    address: &str,
) -> Result<()> {
    sqlx::query("UPDATE nas_accounts SET conn_type=?1, address=?2, updated_at=?3 WHERE id=?4")
        .bind(conn_type)
        .bind(address)
        .bind(now_ms())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_nas_account_password(
    pool: &SqlitePool,
    id: i64,
    secret_data: &str,
) -> Result<()> {
    sqlx::query("UPDATE nas_accounts SET secret_data=?1, updated_at=?2 WHERE id=?3")
        .bind(secret_data)
        .bind(now_ms())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_nas_account(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query(
        "DELETE FROM task_filter_rules WHERE task_id IN (SELECT id FROM tasks WHERE nas_acc_id=?1)",
    )
    .bind(id)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM tasks WHERE nas_acc_id=?1")
        .bind(id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM nas_accounts WHERE id=?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::database::{migrate, open_db};

    async fn test_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = open_db(&dir.path().join("db.sqlite")).await.unwrap();
        migrate(&pool).await.unwrap();
        (dir, pool)
    }

    fn sample(nas_id: &str) -> NasAccountCreate {
        NasAccountCreate {
            nas_id: nas_id.into(),
            uid: 1,
            nas_name: "MyNAS".into(),
            address: "192.168.1.10:5667".into(),
            username: "admin".into(),
            conn_type: "ip".into(),
            secret_data: String::new(),
            long_token: "lt".into(),
            token: "tk".into(),
            pub_key: "pk".into(),
            secret: "sec".into(),
            si: "si".into(),
            computer_name: "PC".into(),
            fnos_version: "0.20.11".into(),
            cookie: "ck".into(),
        }
    }

    #[tokio::test]
    async fn add_and_get() {
        let (_d, pool) = test_pool().await;
        let id = add_nas_account(&pool, &sample("NAS1")).await.unwrap();
        let acc = get_nas_account_by_id(&pool, id).await.unwrap().unwrap();
        assert_eq!(acc.nas_name, "MyNAS");
        assert_eq!(acc.conn_type, "ip");
        let by_nas = get_nas_account_by_nas_id(&pool, "NAS1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_nas.id, id);
    }

    #[tokio::test]
    async fn duplicate_nas_id_rejected() {
        let (_d, pool) = test_pool().await;
        add_nas_account(&pool, &sample("NAS1")).await.unwrap();
        assert!(add_nas_account(&pool, &sample("NAS1")).await.is_err());
    }

    #[tokio::test]
    async fn delete_cascades_tasks() {
        let (_d, pool) = test_pool().await;
        let id = add_nas_account(&pool, &sample("NAS1")).await.unwrap();
        let now = now_ms();
        sqlx::query("INSERT INTO tasks (nas_acc_id, name, address, conn_type, uuid, identifier, mode, nas_path, pc_path, status, created_at, updated_at) VALUES (?1,'t','a','ip','u','i','bisynchronous','/n','/p',1,?2,?2)")
            .bind(id).bind(now).execute(&pool).await.unwrap();
        delete_nas_account(&pool, id).await.unwrap();
        let tasks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tasks, 0);
    }
}
