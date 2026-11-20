//! HTTP 传输层：`/sync/comm` 与 `/sync/{action}`，带 x-sign 签名头。
//!
//! 请求头（抓包实测，官方 0.2.3 客户端）：
//! ```text
//! x-watch-root: <base64 路径>     x-file: <base64 路径>
//! x-connid: <u64 随机>            x-trace-id: <递增序号>
//! x-ops-hash: 0                   x-comm-action: <动作名>
//! x-sign: nonce=..&timestamp=..&sign=..
//! ```
//! WebSocket 连接管理器：conn_id → 写半边。

use super::authx;
use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

static HTTP: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .danger_accept_invalid_certs(true) // NAS 自签证书；与官方 is_insecure_tls 行为一致
        .build()
        .expect("http client")
});

/// 每个连接的 trace-id 计数器（官方每请求递增）。
static TRACE_SEQ: AtomicU64 = AtomicU64::new(0);

fn gen_conn_id() -> u64 {
    use rand::Rng;
    rand::rng().random_range(10u64.pow(18)..2 * 10u64.pow(18))
}

/// 是否为「同步服务未安装/未启动」的反向代理错误。
pub fn is_service_down(status: u16) -> bool {
    status == 502 || status == 503
}

/// GET/POST `/sync/comm?x-comm-action=...`，带完整签名头。
///
/// `watch_root` / `file_path` 传入明文路径（内部按 base64 编码）。
pub async fn sync_comm(
    address: &str,
    action: &str,
    watch_root: &str,
    file_path: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value> {
    let path = "/sync/comm";
    let (x_sign, _nonce, _ts) = authx::gen_x_sign(path, action);
    let url = format!("https://{}{}", address.trim_end_matches('/'), path);
    let trace = TRACE_SEQ.fetch_add(1, Ordering::Relaxed);

    let mut req = HTTP
        .get(&url)
        .header("x-watch-root", authx::b64_path(watch_root))
        .header("x-file", authx::b64_path(file_path))
        .header("x-connid", gen_conn_id().to_string())
        .header("x-trace-id", trace.to_string())
        .header("x-ops-hash", "0")
        .header("x-comm-action", action)
        .header("x-sign", x_sign)
        .header("content-type", "application/json");
    if let Some(t) = token {
        if !t.is_empty() {
            req = req.header("x-user-token", t);
        }
    }

    tracing::debug!(action, address, %trace, "sync/comm request");
    let resp = match body {
        Some(b) => req.json(&b).send().await,
        None => req.send().await,
    };
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(action, address, error = %e, "sync/comm transport error");
            return Err(anyhow!("sync/comm {action} transport error: {e}"));
        }
    };
    let status = resp.status();
    if is_service_down(status.as_u16()) {
        tracing::error!(action, %status, "sync server unavailable (app not installed or stopped)");
        return Err(anyhow!("syncserver not install or has stoped"));
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        tracing::error!(action, %status, body = %text, "sync/comm non-2xx");
        return Err(anyhow!("sync/comm {action} failed: {status}: {text}"));
    }
    // 部分 action 返回纯文本（如 query_nas_os_edition → "Mainland"）
    let text = resp.text().await.unwrap_or_default();
    tracing::debug!(action, body = %text, "sync/comm body");
    Ok(serde_json::from_str(&text)
        .unwrap_or_else(|_| serde_json::Value::String(text.trim().to_string())))
}

/// 查询 NAS 版本/地区（`query_nas_os_edition`，真机返回如 `Mainland`）。
pub async fn query_nas_os_edition(address: &str) -> Result<String> {
    let v = sync_comm(address, "query_nas_os_edition", "/", "/", None, None).await?;
    Ok(match v {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    })
}

/// 通用 `/sync/{action}` 传输（上传/下载框架）。
#[allow(clippy::too_many_arguments)]
pub async fn sync_action(
    address: &str,
    action: &str,
    uid: i64,
    token: &str,
    watch_root: &str,
    file_path: &str,
    headers: Vec<(&str, String)>,
    body: Vec<u8>,
) -> Result<(u16, reqwest::Response)> {
    let path = format!("/sync/{action}");
    let (x_sign, _n, _t) = authx::gen_x_sign(&path, action);
    let url = format!("https://{}{}", address.trim_end_matches('/'), path);
    let trace = TRACE_SEQ.fetch_add(1, Ordering::Relaxed);

    let mut req = HTTP
        .post(&url)
        .header("x-watch-root", authx::b64_path(watch_root))
        .header("x-file", authx::b64_path(file_path))
        .header("x-connid", gen_conn_id().to_string())
        .header("x-trace-id", trace.to_string())
        .header("x-ops-hash", "0")
        .header("x-uid", uid.to_string())
        .header("x-sign", x_sign);
    if !token.is_empty() {
        req = req.header("x-user-token", token);
    }
    for (k, v) in headers {
        req = req.header(k, v);
    }
    tracing::debug!(action, len = body.len(), "sync action send");
    let resp = req.body(body).send().await.map_err(|e| {
        tracing::error!(action, error = %e, "sync action transport error");
        e
    })?;
    let code = resp.status().as_u16();
    tracing::info!(action, code, "sync action response");
    Ok((code, resp))
}

pub fn http_client() -> &'static Client {
    &HTTP
}

// ---------- WebSocket 连接管理 ----------

type WsSink = Arc<
    tokio::sync::Mutex<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tokio_tungstenite::tungstenite::Message,
        >,
    >,
>;

static WS_SINKS: Lazy<RwLock<HashMap<u64, WsSink>>> = Lazy::new(|| RwLock::new(HashMap::new()));

pub fn register_ws_sink(conn_id: u64, sink: WsSink) {
    WS_SINKS.write().insert(conn_id, sink);
}

pub fn unregister_ws_sink(conn_id: u64) {
    WS_SINKS.write().remove(&conn_id);
}

pub async fn send_ws_text(conn_id: u64, text: String) -> Result<()> {
    use futures_util::SinkExt;
    let sink = WS_SINKS
        .read()
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| anyhow!("no ws sink for conn {conn_id}"))?;
    let mut s = sink.lock().await;
    s.send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
        .await
        .map_err(|e| anyhow!("ws send failed: {e}"))
}
