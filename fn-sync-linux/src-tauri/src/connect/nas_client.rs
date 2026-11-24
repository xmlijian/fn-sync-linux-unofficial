//! NAS 客户端 —— 基于实测还原的主 RPC 通道。
//!
//! 通道：`wss://<host>:5667/websocket?type=main`
//! 消息：认证后每帧为 `base64(HMAC-SHA256(key, json)) + json`
//!
//! 登录：RSA-PKCS1v15 + AES-256-CBC 混合加密（见 `crypto`），
//! 响应中的 `secret` 再用同一 AES 密钥解密得到 16 字节 HMAC 密钥。
//!
//! 文件传输：
//!   - 下载：RPC `file.download{files:[path]}` → `download[0].uri` → GET
//!   - 上传：RPC `file.checkUpload{size,path,overwrite}` → `uploadName`
//!           → POST `/upload` (Trim-* 头 + multipart `trim-upload-file`)

use super::crypto;
use anyhow::{anyhow, Result};
use base64::Engine;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const WS_PATH: &str = "/websocket?type=main";
static CONN_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct NasConnConfig {
    pub address: String, // host:port
    pub user: String,
    pub password: String,
    pub device_name: String,
    pub verify_tls: bool,
}

impl NasConnConfig {
    pub fn host(&self) -> &str {
        self.address.trim_end_matches('/')
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteEntry {
    #[serde(skip)]
    pub revision: String,
    pub name: String,
    /// 服务端返回 1/0 或 true/false
    #[serde(default, deserialize_with = "de_dir")]
    pub dir: bool,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtim: i64,
    #[serde(default)]
    pub btim: i64,
    #[serde(default)]
    pub uid: i64,
}

fn de_dir<'de, D>(d: D) -> std::result::Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let v = Option::<Value>::deserialize(d)?;
    Ok(match v {
        Some(Value::Bool(b)) => b,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        _ => false,
    })
}

impl RemoteEntry {
    pub fn is_dir(&self) -> bool {
        self.dir
    }
}

struct PendingReply {
    pending: Arc<DashMap<String, oneshot::Sender<Value>>>,
    id: String,
}
impl Drop for PendingReply {
    fn drop(&mut self) {
        self.pending.remove(&self.id);
    }
}

pub struct NasClient {
    pub conn_id: u64,
    config: NasConnConfig,
    key: RwLock<Vec<u8>>,
    pub uid: RwLock<i64>,
    pub token: RwLock<String>,
    pub machine_id: RwLock<String>,
    tx: mpsc::Sender<(Option<String>, String, oneshot::Sender<Value>)>,
    pending: Arc<DashMap<String, oneshot::Sender<Value>>>,
    req_seq: AtomicU64,
    connected: Arc<AtomicBool>,
    writer_abort: tokio::task::AbortHandle,
    reader_abort: tokio::task::AbortHandle,
    http: reqwest::Client,
}

impl Drop for NasClient {
    fn drop(&mut self) {
        self.disconnect();
    }
}

impl NasClient {
    pub fn config(&self) -> &NasConnConfig {
        &self.config
    }

    /// 建立 WS + 完成登录。
    pub async fn connect(config: NasConnConfig) -> Result<Arc<Self>> {
        let client = Self::open(config).await?;
        client.login().await?;
        Ok(client)
    }

    /// 连通性探测（不登录）：GET 主机名与 NAS 版本。
    pub async fn probe(address: &str) -> Result<(String, String)> {
        let config = NasConnConfig {
            address: address.to_string(),
            user: String::new(),
            password: String::new(),
            device_name: "PcSync".into(),
            verify_tls: false,
        };
        let client = Self::open(config).await?;
        let r = client
            .rpc_plain(json!({ "req": "appcgi.sysinfo.getHostName" }))
            .await?;
        let host = r
            .get("data")
            .and_then(|d| d.get("hostName"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let ver = r
            .get("data")
            .and_then(|d| d.get("trimVersion"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Ok((host, ver))
    }

    /// 仅建立 WS（不登录）。
    pub async fn open(config: NasConnConfig) -> Result<Arc<Self>> {
        let url = format!("wss://{}{}", config.host(), WS_PATH);
        let mut request = url
            .clone()
            .into_client_request()
            .map_err(|e| anyhow!("ws request: {e}"))?;
        request
            .headers_mut()
            .insert("cookie", "mode=relay".parse().unwrap());

        let tls = Some(tls_config(&config)?);
        let (stream, _) = connect_async_tls_with_config(request, None, false, tls).await?;

        let (mut sink, mut src) = stream.split();
        let (tx, mut rx) = mpsc::channel::<(Option<String>, String, oneshot::Sender<Value>)>(256);
        let pending: Arc<DashMap<String, oneshot::Sender<Value>>> = Arc::new(DashMap::new());

        let connected = Arc::new(AtomicBool::new(true));
        // 写任务
        let pending_w = pending.clone();
        let connected_w = connected.clone();
        let writer = tokio::spawn(async move {
            while let Some((reqid, text, reply)) = rx.recv().await {
                match reqid {
                    Some(id) => {
                        if reply.is_closed() {
                            continue;
                        }
                        pending_w.insert(id, reply);
                    }
                    None => drop(reply),
                }
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            connected_w.store(false, Ordering::Relaxed);
            pending_w.clear();
        });

        // 读任务：按 reqid 派发
        let pending_r = pending.clone();
        let connected_r = connected.clone();
        let reader = tokio::spawn(async move {
            while let Some(Ok(msg)) = src.next().await {
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if v.get("res").and_then(|x| x.as_str()) == Some("pong") {
                    continue;
                }
                if let Some(id) = v.get("reqid").and_then(|x| x.as_str()) {
                    let id = id.to_string();
                    if let Some((_, s)) = pending_r.remove(&id) {
                        let _ = s.send(v);
                        continue;
                    }
                }
                // 实测 NAS 对 user.login（encrypted）的成功回包不回显 reqid。
                // 兜底：只有一个在途请求时交付给它（与官方参考客户端“读到非 pong
                // 帧即视为响应”的语义一致）；串行 RPC 场景不会歧义。
                if pending_r.len() == 1 {
                    let entry = pending_r.iter().next().map(|e| (e.key().clone()));
                    if let Some(id) = entry {
                        if let Some((_, s)) = pending_r.remove(&id) {
                            let _ = s.send(v);
                            continue;
                        }
                    }
                }
                // 服务端推送不能作为任何一个 RPC 的响应。
            }
            connected_r.store(false, Ordering::Relaxed);
            pending_r.clear();
        });

        let client = Arc::new(Self {
            conn_id: CONN_COUNTER.fetch_add(1, Ordering::Relaxed),
            http: reqwest::Client::builder()
                .danger_accept_invalid_certs(!config.verify_tls)
                .connect_timeout(Duration::from_secs(15))
                .read_timeout(Duration::from_secs(30))
                .build()?,
            config,
            key: RwLock::new(Vec::new()),
            uid: RwLock::new(0),
            token: RwLock::new(String::new()),
            machine_id: RwLock::new(String::new()),
            tx,
            pending,
            req_seq: AtomicU64::new(1),
            connected,
            writer_abort: writer.abort_handle(),
            reader_abort: reader.abort_handle(),
        });

        Ok(client)
    }

    pub fn disconnect(&self) {
        self.connected.store(false, Ordering::Relaxed);
        self.pending.clear();
        self.writer_abort.abort();
        self.reader_abort.abort();
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed) && !self.tx.is_closed()
    }

    fn next_reqid(&self) -> String {
        let n = self.req_seq.fetch_add(1, Ordering::Relaxed);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        format!("{ts:012x}{n:08x}")
    }

    async fn send_wait(&self, reqid: &str, text: String, timeout: Duration) -> Result<Value> {
        if !self.is_connected() {
            return Err(anyhow!("NAS connection closed"));
        }
        let (otx, orx) = oneshot::channel();
        self.tx
            .send((Some(reqid.to_string()), text, otx))
            .await
            .map_err(|_| anyhow!("ws closed"))?;
        let _pending_reply = PendingReply {
            pending: self.pending.clone(),
            id: reqid.to_string(),
        };
        match tokio::time::timeout(timeout, orx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(anyhow!("ws reply dropped")),
            Err(_) => Err(anyhow!("rpc timeout")),
        }
    }

    /// 未认证请求（明文 JSON）。
    pub async fn rpc_plain(&self, mut req: Value) -> Result<Value> {
        let rid = self.next_reqid();
        if let Some(o) = req.as_object_mut() {
            o.insert("reqid".into(), Value::String(rid.clone()));
        }
        let text = serde_json::to_string(&req)?;
        self.send_wait(&rid, text, Duration::from_secs(15)).await
    }

    /// 已认证请求（签名 + JSON）。
    pub async fn rpc(&self, mut req: Value) -> Result<Value> {
        let rid = self.next_reqid();
        if let Some(o) = req.as_object_mut() {
            o.insert("reqid".into(), Value::String(rid.clone()));
        }
        let js = serde_json::to_string(&req)?;
        let key = self.key.read().clone();
        let sig = crypto::hmac_b64(&key, &js);
        let reply = self
            .send_wait(&rid, format!("{sig}{js}"), Duration::from_secs(30))
            .await?;
        // 会话失效的三类 errno（实测）：
        //   135168 = token 过期；4224 = 会话被服务端作废（如他处重新登录踢下线）；
        //   65534 = 连接未鉴权。
        // 断开连接 → 任务 actor 的 client() 会重连并走 restore_session（tokenLogin 刷新），
        // 避免出现“WS 还在但会话已死”的每秒失败循环。
        let errno = reply.get("errno").and_then(|v| v.as_i64()).unwrap_or(0);
        if matches!(errno, 135168 | 4224 | 65534)
            && !matches!(
                req.get("req").and_then(|v| v.as_str()),
                Some("user.authToken" | "user.tokenLogin")
            )
        {
            tracing::warn!(errno, req = %req.get("req").and_then(|v| v.as_str()).unwrap_or(""), "NAS session rejected; reconnecting to restore");
            self.disconnect();
            return Err(anyhow!("NAS session expired (errno {errno})"));
        }
        Ok(reply)
    }

    /// 从 OAuth 保存的会话恢复连接，不需要保存用户密码。
    pub async fn restore_session(
        &self,
        acc: &crate::service::nas_account::NasAccount,
    ) -> Result<()> {
        use base64::Engine;
        let key = base64::engine::general_purpose::STANDARD
            .decode(&acc.secret)
            .map_err(|_| anyhow!("invalid saved session secret"))?;
        if key.is_empty() || acc.token.is_empty() {
            return Err(anyhow!("saved session missing credentials"));
        }
        *self.key.write() = key;
        *self.uid.write() = acc.uid;
        *self.token.write() = acc.token.clone();
        *self.machine_id.write() = acc.nas_id.clone();
        let si = self.rpc_plain(json!({"req":"util.getSI"})).await?;
        let reply = self
            .rpc(json!({"req":"user.authToken", "token":acc.token,
            "si":si.get("si").and_then(|v|v.as_str()).unwrap_or_default()}))
            .await?;
        if reply.get("result").and_then(|v| v.as_str()) != Some("succ") {
            if acc.long_token.is_empty() {
                return Err(anyhow!("saved NAS session expired; login required"));
            }
            let did = crate::platform::app_data_dir()
                .ok()
                .and_then(|dir| std::fs::read_to_string(dir.join("device_id")).ok())
                .unwrap_or_else(|| "fn-sync-linux".into());
            let refreshed = self
                .rpc(json!({"req":"user.tokenLogin","token":acc.long_token,
                "si":si.get("si").and_then(|v|v.as_str()).unwrap_or_default(),
                "deviceType":"PC","deviceName":self.config.device_name,"did":did.trim()}))
                .await?;
            if refreshed.get("result").and_then(|v| v.as_str()) != Some("succ")
                || refreshed.get("uid").and_then(|v| v.as_i64()) != Some(acc.uid)
            {
                return Err(anyhow!(
                    "saved NAS refresh session rejected; login required"
                ));
            }
            let token = refreshed
                .get("token")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("NAS refresh returned no access token"))?;
            let verified=self.rpc(json!({"req":"user.authToken","token":token,"si":si.get("si").and_then(|v|v.as_str()).unwrap_or_default()})).await?;
            if verified.get("result").and_then(|v| v.as_str()) != Some("succ") {
                return Err(anyhow!("refreshed NAS session rejected"));
            }
            *self.token.write() = token.to_string();
        }
        Ok(())
    }

    pub async fn persist_access_token(
        &self,
        db: &sqlx::SqlitePool,
        acc: &crate::service::nas_account::NasAccount,
    ) -> Result<()> {
        let token = self.token.read().clone();
        if token != acc.token {
            sqlx::query("UPDATE nas_accounts SET token=?1,updated_at=?2 WHERE id=?3")
                .bind(token)
                .bind(crate::service::database::now_ms())
                .bind(acc.id)
                .execute(db)
                .await?;
        }
        Ok(())
    }

    pub async fn login(&self) -> Result<()> {
        let pub_resp = self
            .rpc_plain(json!({ "req": "util.crypto.getRSAPub" }))
            .await?;
        let pubkey = pub_resp
            .get("pub")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("no rsa pub: {pub_resp}"))?
            .to_string();

        let si = self
            .rpc_plain(json!({ "req": "util.getSI" }))
            .await?
            .get("si")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let key_str = crypto::new_key_str();
        let iv = crypto::new_iv();
        let payload = json!({
            "req": "user.login",
            "user": self.config.user,
            "password": self.config.password,
            "deviceName": self.config.device_name,
            "deviceType": "PC",
            "stay": true,
            "did": "fn-sync-linux",
            "si": si,
        });
        let pt = serde_json::to_vec(&payload)?;
        let aes_ct = crypto::aes_cbc_encrypt(&key_str, &iv, &pt);
        let rsa_ct = crypto::rsa_encrypt_pkcs1v15(&pubkey, &key_str)?;
        let b = crypto::b64();

        let resp = self
            .rpc_plain(json!({
                "req": "encrypted",
                "iv": b.encode(iv),
                "rsa": b.encode(rsa_ct),
                "aes": b.encode(aes_ct),
            }))
            .await?;

        if resp.get("result").and_then(|v| v.as_str()) != Some("succ") {
            return Err(anyhow!("login failed: {resp}"));
        }

        let secret_b64 = resp
            .get("secret")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("login: no secret"))?;
        let secret_ct = b
            .decode(secret_b64)
            .map_err(|e| anyhow!("secret b64: {e}"))?;
        let key = crypto::aes_cbc_decrypt(&key_str, &iv, &secret_ct)?;
        *self.key.write() = key;

        *self.uid.write() = resp.get("uid").and_then(|v| v.as_i64()).unwrap_or(0);
        if let Some(t) = resp.get("token").and_then(|v| v.as_str()) {
            *self.token.write() = t.to_string();
        }
        if let Some(m) = resp.get("machineId").and_then(|v| v.as_str()) {
            *self.machine_id.write() = m.to_string();
        }
        Ok(())
    }

    // ---------------- 文件 API ----------------
    pub async fn ls_dir(&self) -> Result<Vec<RemoteEntry>> {
        let r = self.rpc(json!({ "req": "file.lsDir" })).await?;
        if r.get("result").and_then(|v| v.as_str()) == Some("fail") {
            return Err(anyhow!(
                "directory query failed (errno: {})",
                r.get("errno").unwrap_or(&Value::Null)
            ));
        }
        parse_entries(&r)
    }

    pub async fn ls(&self, path: &str) -> Result<Vec<RemoteEntry>> {
        let r = self.rpc(json!({ "req": "file.ls", "path": path })).await?;
        if r.get("result").and_then(|v| v.as_str()) == Some("fail") {
            return Err(anyhow!(
                "directory query failed (errno: {})",
                r.get("errno").unwrap_or(&Value::Null)
            ));
        }
        parse_entries(&r)
    }

    pub async fn mkdir(&self, path: &str) -> Result<()> {
        let r = self
            .rpc(json!({ "req": "file.mkdir", "path": path }))
            .await?;
        if r.get("result").and_then(|v| v.as_str()) == Some("fail") {
            let errno = r.get("errno").and_then(|v| v.as_i64()).unwrap_or(0);
            let _ = errno;
            if let Some((parent, name)) = path.trim_end_matches('/').rsplit_once('/') {
                if self
                    .ls(parent)
                    .await?
                    .iter()
                    .any(|entry| entry.name == name && entry.is_dir())
                {
                    return Ok(());
                }
            }
            return Err(anyhow!("mkdir failed: {r}"));
        }
        Ok(())
    }

    pub async fn remove(&self, paths: Vec<String>) -> Result<()> {
        let operation = self.rpc(json!({"req":"file.rm", "files":paths, "moveToTrashbin":false}));
        tokio::pin!(operation);
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut replied = false;
            loop {
                tokio::select! {
                    result = &mut operation, if !replied => {
                        replied = true;
                        let r = result?;
                        if r.get("result").and_then(|v|v.as_str()) == Some("fail") {
                            return Err(anyhow!("remove failed (errno: {})",r.get("errno").unwrap_or(&Value::Null)));
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        let mut gone = true;
                        for path in &paths {
                            let (parent, name) = path.trim_end_matches('/').rsplit_once('/').ok_or_else(||anyhow!("invalid removal path"))?;
                            if self.ls(parent).await?.iter().any(|e|e.name == name) { gone = false; break; }
                        }
                        if gone { return Ok(()); }
                    }
                }
            }
        }).await.map_err(|_|anyhow!("remove completion timeout"))?
    }

    pub async fn rename(&self, path: &str, new_name: &str) -> Result<()> {
        let r = self
            .rpc(json!({ "req": "file.rename", "path": path, "newName": new_name }))
            .await?;
        if r.get("result").and_then(|v| v.as_str()) == Some("fail") {
            return Err(anyhow!("rename failed: {r}"));
        }
        Ok(())
    }

    /// 递归列出远端目录（用于轮询比对）。
    pub async fn remote_tree(&self, root: &str) -> Result<Vec<(String, RemoteEntry)>> {
        self.remote_tree_filtered(root, None).await
    }

    pub async fn remote_tree_filtered(
        &self,
        root: &str,
        filter: Option<&crate::tasks::file_filter::FileFilter>,
    ) -> Result<Vec<(String, RemoteEntry)>> {
        let mut out = Vec::new();
        let mut stack = vec![(root.trim_matches('/').to_string(), String::new())];
        let mut guard = 0;
        while let Some((dir, rel)) = stack.pop() {
            guard += 1;
            if guard > 20000 {
                return Err(anyhow!("remote directory limit exceeded"));
            }
            let entries = self.ls(&dir).await?;
            for e in entries {
                let child_rel = if rel.is_empty() {
                    e.name.clone()
                } else {
                    format!("{rel}/{}", e.name)
                };
                if filter
                    .map(|f| {
                        !f.should_sync(&child_rel, if e.is_dir() { None } else { Some(e.size) })
                    })
                    .unwrap_or(false)
                {
                    continue;
                }
                if e.is_dir() {
                    stack.push((format!("{dir}/{}", e.name), child_rel.clone()));
                }
                out.push((child_rel, e));
            }
        }
        Ok(out)
    }

    // ---------------- 下载 ----------------
    pub async fn download(&self, remote: &str) -> Result<Vec<u8>> {
        let r = self
            .rpc(json!({ "req": "file.download", "files": [remote] }))
            .await?;
        let uri = r
            .get("download")
            .and_then(|d| d.get(0))
            .and_then(|d| d.get("uri"))
            .and_then(|u| u.as_str())
            .ok_or_else(|| anyhow!("download: {r}"))?;
        let url = format!("https://{}{}", self.config.host(), uri);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("download http {}", resp.status()));
        }
        let mut stream = resp.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            crate::traffic::throttle(chunk.len(), false).await;
            bytes.extend_from_slice(&chunk);
        }
        crate::traffic::TrafficCounter::new().add(0, bytes.len() as i64);
        Ok(bytes)
    }

    /// HTTP Range is validated before reading the body. Never fall back to a full ISO download.
    pub async fn read_range(&self, remote: &str, offset: u64, size: usize) -> Result<Vec<u8>> {
        if size == 0 || size > 1024 * 1024 {
            return Err(anyhow!("invalid range size"));
        }
        let end = offset
            .checked_add(size as u64 - 1)
            .ok_or_else(|| anyhow!("range overflow"))?;
        let r = self
            .rpc(json!({"req":"file.download", "files":[remote]}))
            .await?;
        let uri = r
            .get("download")
            .and_then(|d| d.get(0))
            .and_then(|d| d.get("uri"))
            .and_then(|u| u.as_str())
            .ok_or_else(|| anyhow!("download uri missing"))?;
        let response = self
            .http
            .get(format!("https://{}{}", self.config.host(), uri))
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .header(reqwest::header::RANGE, format!("bytes={offset}-{end}"))
            .send()
            .await?;
        read_range_body(response, offset, size).await
    }

    /// 流式下载到指定文件，边下边写，不把整个文件读进内存。
    ///
    /// `on_progress` 在每收到一个数据块后调用（参数为累计字节数），
    /// 用于实时进度显示。返回写入的总字节数。
    ///
    /// 这样几百 MB 甚至数 GB 的文件也不会一次性占用等量内存。
    pub async fn download_to<F>(
        &self,
        remote: &str,
        dest: &std::path::Path,
        on_progress: F,
    ) -> Result<u64>
    where
        F: Fn(u64) + Send + Sync,
    {
        let r = self
            .rpc(json!({ "req": "file.download", "files": [remote] }))
            .await?;
        let uri = r
            .get("download")
            .and_then(|d| d.get(0))
            .and_then(|d| d.get("uri"))
            .and_then(|u| u.as_str())
            .ok_or_else(|| anyhow!("download: {r}"))?;
        let url = format!("https://{}{}", self.config.host(), uri);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("download http {}", resp.status()));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = tokio::fs::File::create(dest).await?;
        let mut stream = resp.bytes_stream();
        let mut written: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            crate::traffic::throttle(chunk.len(), false).await;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
            written += chunk.len() as u64;
            on_progress(written);
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        drop(file);
        crate::traffic::TrafficCounter::new().add(0, written as i64);
        Ok(written)
    }

    // ---------------- 上传 ----------------
    pub async fn upload(&self, remote: &str, data: Vec<u8>, mtime: Option<i64>) -> Result<()> {
        let transfer_size = data.len() as i64;
        let overwrite = true;
        let ck = self
            .rpc(json!({
                "req": "file.checkUpload",
                "path": remote,
                "size": data.len(),
                "overwrite": overwrite,
            }))
            .await?;
        if ck.get("result").and_then(|v| v.as_str()) != Some("succ") {
            return Err(anyhow!("checkUpload failed: {ck}"));
        }
        let upload_name = ck
            .get("uploadName")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| remote.rsplit('/').next().unwrap_or(remote).to_string());
        let dir = remote.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let target = format!("{dir}/{upload_name}");

        let enc = encode_uri_path(&target);
        let key = self.key.read().clone();
        let sig = crypto::hmac_b64(&key, &enc);
        let mtime_s = mtime.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64
        });
        let token = self.token.read().clone();

        let stream = futures_util::stream::unfold((data, 0usize), |(data, offset)| async move {
            if offset >= data.len() {
                return None;
            }
            let end = (offset + 64 * 1024).min(data.len());
            let chunk = data[offset..end].to_vec();
            crate::traffic::throttle(chunk.len(), true).await;
            Some((Ok::<_, std::io::Error>(chunk), (data, end)))
        });
        let part = reqwest::multipart::Part::stream_with_length(
            reqwest::Body::wrap_stream(stream),
            transfer_size as u64,
        )
        .file_name(upload_name.clone())
        .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new().part("trim-upload-file", part);

        let mut req = self
            .http
            .post(format!("https://{}/upload", self.config.host()))
            .header("Trim-Path", enc)
            .header("Trim-From", "0")
            .header("Trim-Overwrite", if overwrite { "true" } else { "false" })
            .header("Trim-Mtim", mtime_s.to_string())
            .header("Trim-Token", token.clone())
            .header("Trim-Sign", sig)
            .multipart(form);
        if !token.is_empty() {
            req = req.header("Authorization", format!("trim {token}"));
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("upload http {status}: {text}"));
        }
        crate::traffic::TrafficCounter::new().add(transfer_size, 0);
        Ok(())
    }
}

fn parse_entries(v: &Value) -> Result<Vec<RemoteEntry>> {
    let Some(files) = v.get("files") else {
        return Ok(Vec::new());
    };
    let mut entries: Vec<RemoteEntry> = serde_json::from_value(files.clone())
        .map_err(|e| anyhow!("invalid directory entries: {e}"))?;
    let revision = v.get("uver").map(|v| v.to_string()).unwrap_or_default();
    for e in &mut entries {
        e.revision = revision.clone();
        if e.name.is_empty()
            || e.name == "."
            || e.name == ".."
            || e.name.contains('/')
            || e.name.contains('\\')
        {
            return Err(anyhow!("invalid remote entry name"));
        }
    }
    Ok(entries)
}

/// 与 JS `encodeURI` 一致：保留 `/` 与 `-_.!~*'()`。
fn encode_uri_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len() * 3);
    for b in path.bytes() {
        let c = b as char;
        let keep = c.is_ascii_alphanumeric()
            || matches!(
                c,
                '/' | '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')'
            );
        if keep {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn tls_config(_config: &NasConnConfig) -> Result<tokio_tungstenite::Connector> {
    // fnOS NAS 使用自签证书，客户端固定跳过校验（与官方客户端行为一致）
    Ok(tokio_tungstenite::Connector::Rustls(Arc::new(
        skip_verify_config()?,
    )))
}

pub fn skip_verify_config() -> Result<rustls::ClientConfig> {
    #[derive(Debug)]
    struct Skip;
    impl rustls::client::danger::ServerCertVerifier for Skip {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error>
        {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _m: &[u8],
            _c: &rustls::pki_types::CertificateDer<'_>,
            _d: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _m: &[u8],
            _c: &rustls::pki_types::CertificateDer<'_>,
            _d: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![
                rustls::SignatureScheme::RSA_PKCS1_SHA256,
                rustls::SignatureScheme::RSA_PKCS1_SHA384,
                rustls::SignatureScheme::RSA_PKCS1_SHA512,
                rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
                rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
                rustls::SignatureScheme::RSA_PSS_SHA256,
                rustls::SignatureScheme::RSA_PSS_SHA384,
                rustls::SignatureScheme::RSA_PSS_SHA512,
                rustls::SignatureScheme::ED25519,
            ]
        }
    }
    let provider = rustls::crypto::ring::default_provider();
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Skip))
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(cfg)
}

async fn read_range_body(response: reqwest::Response, offset: u64, size: usize) -> Result<Vec<u8>> {
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(anyhow!(
            "NAS 未支持分段读取（HTTP {}），请使用下载到本地",
            response.status()
        ));
    }
    let header = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| anyhow!("NAS Range response missing Content-Range"))?;
    let range = header
        .strip_prefix("bytes ")
        .and_then(|s| s.split_once('/'))
        .and_then(|(r, total)| r.split_once('-').map(|(a, b)| (a, b, total)))
        .ok_or_else(|| anyhow!("invalid Content-Range"))?;
    let start: u64 = range.0.parse()?;
    let end: u64 = range.1.parse()?;
    let total: u64 = range.2.parse()?;
    if start != offset || end < start || end >= total || end - start + 1 > size as u64 {
        return Err(anyhow!(
            "NAS Range response does not match requested interval"
        ));
    }
    let expected = (end - start + 1) as usize;
    let mut body = Vec::with_capacity(expected);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if chunk.len() > expected.saturating_sub(body.len()) {
            return Err(anyhow!("NAS Range response exceeded declared length"));
        }
        crate::traffic::throttle(chunk.len(), false).await;
        body.extend_from_slice(&chunk);
        if body.len() == expected {
            break;
        }
    }
    if body.len() != expected {
        return Err(anyhow!("NAS Range response truncated"));
    }
    crate::traffic::TrafficCounter::new().add(0, body.len() as i64);
    Ok(body)
}

#[cfg(test)]
mod live_transfer_tests {
    #[tokio::test]
    #[ignore = "requires saved NAS session and isolated FN_SYNC_TEST_REMOTE_ROOT"]
    async fn nas_upload_download_rename_delete() {
        let db = crate::service::database::open_db(std::path::Path::new(
            &std::env::var("FN_SYNC_TEST_DB").unwrap(),
        ))
        .await
        .unwrap();
        let address = std::env::var("FN_SYNC_TEST_ADDRESS").unwrap();
        let acc = crate::service::nas_account::get_all_nas_accounts(&db)
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.address == address)
            .unwrap();
        let manager = crate::task_manager::TaskManager::new();
        let nas = manager.account_client(&db, &acc).await.unwrap();
        let root = std::env::var("FN_SYNC_TEST_REMOTE_ROOT").unwrap();
        assert!(root.contains("fn-sync-qa-"));
        let path = format!("{root}/中文上传.txt");
        struct ResetLimits;
        impl Drop for ResetLimits {
            fn drop(&mut self) {
                crate::traffic::set_limit("global_upload_speed_limit", "-1");
                crate::traffic::set_limit("global_download_speed_limit", "-1");
            }
        }
        let _reset = ResetLimits;
        let bytes = vec![b'Q'; 4096];
        crate::traffic::set_limit("global_upload_speed_limit", "64");
        let started = std::time::Instant::now();
        nas.upload(&path, bytes.clone(), None)
            .await
            .expect("upload");
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(50),
            "upload bandwidth setting ignored"
        );
        crate::traffic::set_limit("global_upload_speed_limit", "-1");
        let listing = nas.ls(&root).await.expect("listing");
        assert!(
            listing.iter().any(|e| e.name == "中文上传.txt"),
            "uploaded file missing from parsed listing"
        );
        crate::traffic::set_limit("global_download_speed_limit", "64");
        let started = std::time::Instant::now();
        assert_eq!(nas.download(&path).await.expect("download"), bytes);
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(50),
            "download bandwidth setting ignored"
        );
        crate::traffic::set_limit("global_download_speed_limit", "-1");
        nas.rename(&path, "renamed.txt").await.expect("rename");
        assert_eq!(
            nas.download(&format!("{root}/renamed.txt")).await.unwrap(),
            bytes
        );
        nas.remove(vec![format!("{root}/renamed.txt")])
            .await
            .expect("delete");
        assert!(!nas
            .ls(&root)
            .await
            .unwrap()
            .iter()
            .any(|e| e.name == "renamed.txt"));
    }
}

#[cfg(test)]
mod range_response_tests {
    use super::*;
    async fn response(raw: &'static str) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await;
            socket.write_all(raw.as_bytes()).await.unwrap();
        });
        reqwest::get(format!("http://{address}/file"))
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn bounded_range_requires_206_and_matching_offset() {
        let valid = response("HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 32768-32771/4294967296\r\n\r\nABCD").await;
        assert_eq!(read_range_body(valid, 32768, 4).await.unwrap(), b"ABCD");
        let ignored = response("HTTP/1.1 200 OK\r\nContent-Length: 4294967296\r\n\r\n").await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), read_range_body(ignored, 0, 512))
                .await
                .unwrap()
                .is_err()
        );
        let wrong = response("HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/4294967296\r\n\r\nABCD").await;
        assert!(read_range_body(wrong, 32768, 4).await.is_err());
        let oversized = response("HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/4294967296\r\n\r\nABCD").await;
        assert!(read_range_body(oversized, 0, 2).await.is_err());
        let truncated = response("HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/4294967296\r\n\r\nAB").await;
        assert!(read_range_body(truncated, 0, 4).await.is_err());
    }
}
