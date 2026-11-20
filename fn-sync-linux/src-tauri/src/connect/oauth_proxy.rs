//! 本地授权代理 —— 与官方客户端行为一致（exe 中的 `async-oauth`）。
//!
//! 官方做法：
//!   1. 客户端在本机起一个 axum 代理（日志 `oauth proxy started on port: <p>`）
//!   2. 授权窗口加载 `http://127.0.0.1:<p>/login`（抓包 referer 证实）
//!   3. 该请求由代理转发到 NAS 的 `https://<nas>/login`
//!      NAS 返回自己的登录页，页面内所有相对请求（/assets/...）同样经代理转发
//!   4. NAS 登录完成后回调 `async-oauth/c/<code>`，代理截获该路径并回传凭据
//!
//! 这样授权界面 100% 是 NAS 原生的，不需要自造任何 UI。

use anyhow::{anyhow, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

#[derive(Clone)]
pub struct ProxyState {
    pub nas_base: String, // https://host:port
    pub client: reqwest::Client,
    /// 授权窗口的 IPC 桥：页面用 fetch 调 `/__ipc`，这里转给 Rust 命令层。
    pub app: Option<tauri::AppHandle>,
}

pub struct OauthProxy {
    pub port: u16,
    pub nas_base: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl OauthProxy {
    /// 在随机端口启动代理，指向给定的 NAS。
    pub async fn start(nas_addr: &str, verify_tls: bool, app: Option<tauri::AppHandle>) -> Result<Self> {
        let nas_base = if nas_addr.starts_with("http") {
            nas_addr.to_string()
        } else {
            format!("https://{nas_addr}")
        };
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(!verify_tls)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(120))
            .build()?;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        let state = ProxyState {
            nas_base: nas_base.clone(),
            client,
            app,
        };
        let router = axum::Router::new()
            .route("/__ipc", axum::routing::post(ipc_handler))
            .fallback(proxy_handler)
            .with_state(state);

        tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });

        Ok(Self {
            port,
            nas_base,
            shutdown: Some(tx),
        })
    }

    pub fn local_url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }
}

impl Drop for OauthProxy {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// 逐跳头，不能透传
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

fn rewrite_location(value: &str, nas_base: &str, local_base: &str) -> String {
    // NAS 返回的绝对 302 要改回本地代理，避免 webview 直接跳出代理
    value
        .replace(nas_base, local_base)
        .replace(
            &nas_base.replace("https://", "http://"),
            local_base,
        )
}


/// 授权窗口的 IPC 桥。
///
/// Tauri v2 不会给远程 URL（http://127.0.0.1）注入可用的 IPC，
/// 而 NAS 登录页需要调用 `put_auth_result` / `get_device_id`（见
/// assets/index-D1auxRov.js 的 rKe/nKe）。这里用普通 HTTP 回传，
/// 由本代理转给真实命令层，行为与官方一致。
async fn ipc_handler(State(st): State<ProxyState>, body: String) -> Response {
    #[derive(serde::Deserialize)]
    struct IpcCall {
        cmd: String,
        #[serde(default)]
        args: serde_json::Value,
    }
    let call: IpcCall = match serde_json::from_str(&body) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad ipc: {e}")).into_response(),
    };
    tracing::info!(cmd = %call.cmd, "oauth ipc call");

    let Some(app) = st.app.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no app handle").into_response();
    };

    // 复用 commands 层的实现
    let result: Result<serde_json::Value, String> = match call.cmd.as_str() {
        "put_auth_result" => {
            let data = call
                .args
                .get("data")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            crate::commands::oauth_put_auth_result(&app, data)
                .map(|_| serde_json::Value::Null)
                .map_err(|e| e)
        }
        "get_device_id" => crate::commands::oauth_get_device_id(&app).map_err(|e| e),
        other => Err(format!("unknown ipc cmd: {other}")),
    };

    match result {
        Ok(v) => axum::Json(serde_json::json!({ "ok": true, "value": v })).into_response(),
        Err(e) => {
            tracing::warn!(cmd = %call.cmd, error = %e, "oauth ipc failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({ "ok": false, "error": e })),
            )
                .into_response()
        }
    }
}

async fn proxy_handler(State(st): State<ProxyState>, req: Request) -> Response {
    // WebSocket 升级必须走专用的双向转发，否则 NAS 页面会提示「连接已断开」
    if req
        .headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
    {
        return proxy_websocket(st, req).await;
    }
    let method = req.method().clone();
    let uri = req.uri().clone();
    let local_base = format!(
        "http://127.0.0.1:{}",
        uri.authority().map(|a| a.as_str()).unwrap_or_default()
            .rsplit(':')
            .next()
            .unwrap_or_default()
    );

    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let target = format!("{}{}", st.nas_base, path_and_query);

    // 复制请求头（去掉 hop-by-hop 与 host）
    let mut headers = HeaderMap::new();
    for (k, v) in req.headers() {
        if is_hop_by_hop(k.as_str()) {
            continue;
        }
        headers.insert(k.clone(), v.clone());
    }
    // HTML 需要在 NAS 模块执行前注入授权桥，要求上游返回未压缩内容。
    headers.insert(HeaderName::from_static("accept-encoding"), HeaderValue::from_static("identity"));
    // 与官方一致：代理请求带 cookie: mode=relay
    {
        let existing = headers
            .get("cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let merged = if existing.is_empty() {
            "mode=relay".to_string()
        } else if existing.contains("mode=relay") {
            existing
        } else {
            format!("{existing}; mode=relay")
        };
        if let Ok(v) = HeaderValue::from_str(&merged) {
            headers.insert(HeaderName::from_static("cookie"), v);
        }
    }

    tracing::debug!(target = %target, method = %method, "oauth proxy →");
    let body_bytes = match axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("body read failed: {e}")).into_response()
        }
    };

    let method_conv = match method {
        Method::GET => reqwest::Method::GET,
        Method::POST => reqwest::Method::POST,
        Method::PUT => reqwest::Method::PUT,
        Method::DELETE => reqwest::Method::DELETE,
        Method::HEAD => reqwest::Method::HEAD,
        Method::OPTIONS => reqwest::Method::OPTIONS,
        Method::PATCH => reqwest::Method::PATCH,
    _ => return (StatusCode::METHOD_NOT_ALLOWED, "method").into_response(),
    };

    let mut rb = st.client.request(method_conv, &target).headers(headers);
    if !body_bytes.is_empty() {
        rb = rb.body(body_bytes.to_vec());
    }

    tracing::debug!(url = %target, "oauth proxy ← sending");
    let resp = match rb.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(url = %target, error = %e, "oauth proxy upstream failed");
            return (
                StatusCode::BAD_GATEWAY,
                format!("upstream {target}: {e}"),
            )
                .into_response();
        }
    };

    let status = resp.status();
    let mut out_headers = HeaderMap::new();
    for (k, v) in resp.headers() {
        if is_hop_by_hop(k.as_str()) {
            continue;
        }
        let mut val = v.clone();
        if k.as_str().eq_ignore_ascii_case("location") {
            if let Ok(s) = v.to_str() {
                if let Ok(nv) = HeaderValue::from_str(&rewrite_location(s, &st.nas_base, &local_base)) {
                    val = nv;
                }
            }
        }
        out_headers.insert(k.clone(), val);
    }

    let is_html = resp.headers().get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("text/html")).unwrap_or(false);
    let body = match resp.bytes().await {
        Ok(body) => body,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("upstream body: {e}")).into_response(),
    };
    let body = if is_html {
        match std::str::from_utf8(&body) {
            Ok(html) => inject_oauth_bridge(html).into_bytes(),
            Err(_) => return (StatusCode::BAD_GATEWAY, "invalid upstream HTML encoding").into_response(),
        }
    } else { body.to_vec() };
    let mut builder = Response::builder().status(status.as_u16());
    if let Some(hs) = builder.headers_mut() {
        *hs = out_headers;
    }
    builder
        .body(Body::from(body))
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}

/// 便捷函数：把各种失败包装成 anyhow
#[allow(dead_code)]
pub fn ensure(cond: bool, msg: &str) -> Result<()> {
    if cond {
        Ok(())
    } else {
        Err(anyhow!(msg.to_string()))
    }
}

#[allow(dead_code)]
fn _unused(_: Uri) {}

/// WebSocket 双向转发：本地 axum <-> NAS wss。
/// NAS 登录页依赖 `/websocket?type=main`，不做这一步页面会显示「连接已断开」。
async fn proxy_websocket(st: ProxyState, req: Request) -> Response {
    use axum::extract::ws::{Message as AxMsg, WebSocketUpgrade};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let (parts, _body) = req.into_parts();
    tracing::info!(uri = %parts.uri, "oauth ws upgrade incoming");
    let path_q = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    let ws_url = st
        .nas_base
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1)
        + &path_q;

    let mut creq = match ws_url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("ws build: {e}")).into_response(),
    };
    {
        let h = creq.headers_mut();
        // 保留页面带来的 cookie / sec-websocket-protocol 等，补 mode=relay
        for (k, v) in parts.headers.iter() {
            if matches!(
                k.as_str().to_ascii_lowercase().as_str(),
                "host"
                    | "connection"
                    | "upgrade"
                    | "sec-websocket-key"
                    | "sec-websocket-version"
                    | "sec-websocket-extensions"
            ) {
                continue;
            }
            h.insert(k.clone(), v.clone());
        }
        let cookie = h
            .get("cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let merged = if cookie.is_empty() {
            "mode=relay".to_string()
        } else if cookie.contains("mode=relay") {
            cookie
        } else {
            format!("{cookie}; mode=relay")
        };
        if let Ok(v) = HeaderValue::from_str(&merged) {
            h.insert(HeaderName::from_static("cookie"), v);
        }
    }

    let tls = match tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(
        match crate::connect::nas_client::skip_verify_config() {
            Ok(c) => c,
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("tls: {e}"))
                    .into_response()
            }
        },
    )) {
        c => Some(c),
    };

    tracing::info!(url = %ws_url, "oauth ws upstream connecting");
    let upstream = match tokio_tungstenite::connect_async_tls_with_config(creq, None, false, tls).await
    {
        Ok((ws, _)) => ws,
        Err(e) => {
            tracing::error!(url = %ws_url, error = %e, "ws upstream failed");
            return (StatusCode::BAD_GATEWAY, format!("ws upstream: {e}")).into_response();
        }
    };

    let (mut up_tx, mut up_rx) = upstream.split();

    use axum::extract::FromRequestParts;
    let mut parts_owned = parts;
    let ws = match <WebSocketUpgrade as FromRequestParts<()>>::from_request_parts(
        &mut parts_owned,
        &(),
    )
    .await
    {
        Ok(u) => u,
        Err(r) => return r.into_response(),
    };

    ws.on_upgrade(move |socket| async move {
        let (mut cl_tx, mut cl_rx) = socket.split();
        // 客户端 → NAS
        let fwd_up = async {
            while let Some(Ok(msg)) = cl_rx.next().await {
                let out = match msg {
                    AxMsg::Text(t) => tokio_tungstenite::tungstenite::Message::Text(t.to_string().into()),
                    AxMsg::Binary(b) => {
                        tokio_tungstenite::tungstenite::Message::Binary(b.to_vec().into())
                    }
                    AxMsg::Ping(p) => tokio_tungstenite::tungstenite::Message::Ping(p.to_vec().into()),
                    AxMsg::Pong(p) => tokio_tungstenite::tungstenite::Message::Pong(p.to_vec().into()),
                    AxMsg::Close(_) => break,
                };
                if up_tx.send(out).await.is_err() {
                    break;
                }
            }
        };
        // NAS → 客户端
        let fwd_down = async {
            while let Some(Ok(msg)) = up_rx.next().await {
                let out = match msg {
                    tokio_tungstenite::tungstenite::Message::Text(t) => AxMsg::Text(t.to_string().into()),
                    tokio_tungstenite::tungstenite::Message::Binary(b) => {
                        AxMsg::Binary(b.to_vec().into())
                    }
                    tokio_tungstenite::tungstenite::Message::Ping(p) => AxMsg::Ping(p.to_vec().into()),
                    tokio_tungstenite::tungstenite::Message::Pong(p) => AxMsg::Pong(p.to_vec().into()),
                    tokio_tungstenite::tungstenite::Message::Close(_) => break,
                    _ => continue,
                };
                if cl_tx.send(out).await.is_err() {
                    break;
                }
            }
        };
        tokio::select! {
            _ = fwd_up => {}
            _ = fwd_down => {}
        }
    })
    .into_response()
}

// 页面初始化脚本可能被 WebKit/Tauri 自己的初始化覆盖；在 HTML 中、模块之前
// 再安装桥接，保证 NAS 在首次导入 constants 时就能识别 async-oauth。
fn inject_oauth_bridge(html: &str) -> String {
    let script = format!("<script>{}</script>", crate::commands::PROBE_SCRIPT);
    if let Some(pos) = html.to_ascii_lowercase().find("<head>") {
        let pos = pos + "<head>".len();
        format!("{}{}{}", &html[..pos], script, &html[pos..])
    } else {
        format!("{script}{html}")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bridge_precedes_nas_modules() {
        let html = super::inject_oauth_bridge("<!doctype html><html><head><script type=\"module\" src=\"/app.js\"></script></head></html>");
        assert!(html.find("__TAURI_INTERNALS__").unwrap() < html.find("type=\"module\"").unwrap());
        assert!(html.starts_with("<!doctype html>"));
    }
}
