//! NAS 侧 WebSocket JSON-v1 协议类型（PROTOCOL.md §4）。
//! protobuf 与 json-v1 双格式；服务端旧版仅支持 json-v1，客户端检测后自适应。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncDirection {
    #[serde(rename = "UpOnly")]
    UpOnly,
    #[serde(rename = "DownOnly")]
    DownOnly,
    #[serde(rename = "Both")]
    Both,
}

/// CtlAddWatch 的过滤规则（json-v1 字段名与 protobuf 字段名一致）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterRule {
    #[serde(rename = "only_dir", skip_serializing_if = "Option::is_none")]
    pub only_dir: Option<bool>,
    #[serde(rename = "ignore_dotfiles")]
    pub ignore_dotfiles: bool,
    #[serde(rename = "ignore_extensions", default)]
    pub ignore_extensions: Vec<String>,
    #[serde(rename = "size_lower_bound", skip_serializing_if = "Option::is_none")]
    pub size_lower_bound: Option<u64>,
    #[serde(rename = "size_upper_bound", skip_serializing_if = "Option::is_none")]
    pub size_upper_bound: Option<u64>,
}

/// 服务端文件事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum FileEvent {
    Create {
        path: String,
        #[serde(rename = "is_dir")]
        is_dir: bool,
        fmtime: i64,
        #[serde(rename = "file_size", skip_serializing_if = "Option::is_none")]
        file_size: Option<u64>,
    },
    Modify {
        path: String,
        fmtime: i64,
        #[serde(rename = "file_size", skip_serializing_if = "Option::is_none")]
        file_size: Option<u64>,
        #[serde(
            rename = "allow_change_action",
            skip_serializing_if = "Option::is_none"
        )]
        allow_change_action: Option<bool>,
    },
    Remove {
        path: String,
    },
    Rename {
        #[serde(rename = "src_path")]
        src_path: String,
        #[serde(rename = "dst_path")]
        dst_path: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    #[serde(rename = "schema_version", skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    #[serde(
        rename = "modify_missing_as_create_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub modify_missing_as_create_default: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandShakeResp {
    #[serde(rename = "ServerPubKey", skip_serializing_if = "Option::is_none")]
    pub server_pub_key: Option<String>,
    #[serde(rename = "nasid", skip_serializing_if = "Option::is_none")]
    pub nasid: Option<String>,
    #[serde(rename = "has_exists", skip_serializing_if = "Option::is_none")]
    pub has_exists: Option<bool>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// NAS 文件 RPC 请求（PROTOCOL.md §5）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NasRpcRequest {
    pub req: String,
    pub reqid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NasFileEntry {
    pub name: String,
    #[serde(default)]
    pub dir: bool,
    #[serde(default)]
    pub v: i64,
    #[serde(default)]
    pub size: u64,
    #[serde(default, rename = "fmtime")]
    pub fmtime: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NasRpcResponse {
    #[serde(default)]
    pub files: Vec<NasFileEntry>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// reqid 生成：hex(unix_ts,8) + hex(counter,4) + "0000000000000000"
pub struct ReqIdGen {
    counter: std::sync::atomic::AtomicU64,
}

impl Default for ReqIdGen {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqIdGen {
    pub const BACK_ID: &'static str = "0000000000000000";

    pub const fn new() -> Self {
        Self {
            counter: std::sync::atomic::AtomicU64::new(1),
        }
    }

    pub fn next(&self) -> String {
        let c = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ts = chrono::Utc::now().timestamp();
        format!(
            "{:08x}{:04x}{}{:04x}",
            ts,
            c & 0xffff,
            Self::BACK_ID,
            c & 0xffff
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reqid_format() {
        let g = ReqIdGen::new();
        let id = g.next();
        assert_eq!(id.len(), 8 + 4 + 16 + 4);
        assert!(id.ends_with(&format!("{:04x}", 1)));
    }

    #[test]
    fn filter_rule_json() {
        let r = FilterRule {
            only_dir: None,
            ignore_dotfiles: true,
            ignore_extensions: vec!["lnk".into(), "pst".into(), "swp".into()],
            size_lower_bound: None,
            size_upper_bound: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"ignore_dotfiles\":true"));
        assert!(s.contains("\"ignore_extensions\":[\"lnk\",\"pst\",\"swp\"]"));
    }
}
