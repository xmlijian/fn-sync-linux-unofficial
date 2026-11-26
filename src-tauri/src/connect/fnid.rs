//! FN ID（FN Connect）地址解析。

use anyhow::{anyhow, Result};
use serde_json::Value;

const SIGN_PREFIX: &str = "NDzZTVxnRKP8Z0jXg1VAMonaG8akvh";
const SIGN_API_KEY: &str = "zIGtkc3dqZnJpd29qZXJqa2w7c";
const RESOLVE_PATH: &str = "/api/v1/fn/con";
const CONNECT_DOMAINS: [&str; 2] = ["fnos.net", "5ddd.com"];

fn md5_hex(data: &str) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(data.as_bytes());
    let mut s = String::with_capacity(32);
    for b in h.finalize() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

pub fn authx_header(body: &str) -> String {
    let nonce = format!(
        "{:06}",
        rand::Rng::random_range(&mut rand::rng(), 100_000u32..1_000_000u32)
    );
    let ts = chrono::Utc::now().timestamp_millis();
    let msg = [
        SIGN_PREFIX,
        RESOLVE_PATH,
        &nonce,
        &ts.to_string(),
        &md5_hex(body),
        SIGN_API_KEY,
    ]
    .join("_");
    format!("nonce={nonce}&timestamp={ts}&sign={}", md5_hex(&msg))
}

pub fn extract_fnid(input: &str) -> Option<String> {
    let trimmed = input
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let host_and_port = trimmed.split('/').next().unwrap_or(trimmed);
    let host = host_and_port
        .rsplit_once(':')
        .map(|(h, p)| {
            if p.parse::<u16>().is_ok() {
                h
            } else {
                host_and_port
            }
        })
        .unwrap_or(host_and_port);
    let parts: Vec<&str> = host.split('.').collect();
    let suffix = match parts.len() {
        2 => host.to_string(),
        3 => format!("{}.{}", parts[1], parts[2]),
        _ => return None,
    };
    if !CONNECT_DOMAINS.iter().any(|d| suffix == *d) {
        return None;
    }
    if trimmed.contains('/') {
        trimmed
            .split('/')
            .nth(1)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    } else {
        parts
            .first()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    }
}

pub async fn resolve(fnid: &str) -> Result<String> {
    let fnid = fnid.trim();
    if fnid.is_empty() {
        return Err(anyhow!("fnid_host_missing"));
    }
    let body = serde_json::json!({ "fnId": fnid }).to_string();
    let authx = authx_header(&body);

    let mut last_error = String::new();
    for domain in CONNECT_DOMAINS {
        let url = format!("https://{domain}{RESOLVE_PATH}");
        let resp = crate::connect::transport::http_client()
            .post(&url)
            .header("content-type", "application/json")
            .header("authx", &authx)
            .body(body.clone())
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                last_error = e.to_string();
                continue;
            }
        };
        let text = resp.text().await.unwrap_or_default();
        match parse_relay(&text) {
            Ok(addr) => return Ok(addr),
            Err(e) => last_error = e.to_string(),
        }
    }
    Err(anyhow!("fnid_not_found: {last_error}"))
}

fn parse_relay(text: &str) -> Result<String> {
    let v: Value = serde_json::from_str(text).map_err(|e| anyhow!("/con body_json: {e}"))?;
    if v.get("code").and_then(|c| c.as_i64()) != Some(0) {
        return Err(anyhow!("fnid_host_missing: unexpected response"));
    }
    v.get("data")
        .and_then(|d| d.get("fn"))
        .and_then(|f| f.as_array())
        .and_then(|a| a.first())
        .and_then(|a| a.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("fnid_host_missing"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_fnid_from_relay_domain() {
        assert_eq!(extract_fnid("demo-abc.fnos.net"), Some("demo-abc".into()));
        assert_eq!(
            extract_fnid("demo-abc.fnos.net:443"),
            Some("demo-abc".into())
        );
        assert_eq!(
            extract_fnid("https://demo-abc.fnos.net:443/"),
            Some("demo-abc".into())
        );
        assert_eq!(extract_fnid("fnos.net/demo-abc"), Some("demo-abc".into()));
        assert_eq!(extract_fnid("demo-abc.5ddd.com"), Some("demo-abc".into()));
    }

    #[test]
    fn ignores_regular_addresses() {
        assert_eq!(extract_fnid("10.0.0.5:5667"), None);
        assert_eq!(extract_fnid("nas.example.com:5667"), None);
        assert_eq!(extract_fnid("demo-abc"), None);
    }

    #[test]
    fn authx_matches_expected_shape() {
        let header = authx_header("{\"fnId\":\"demo-abc\"}");
        let parts: Vec<&str> = header.split('&').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].starts_with("nonce="));
        assert_eq!(parts[0].len(), "nonce=".len() + 6);
        assert!(parts[1].starts_with("timestamp="));
        assert_eq!(parts[2].len(), "sign=".len() + 32);
    }

    #[test]
    fn parses_relay_from_response() {
        let body =
            r#"{"code":0,"msg":"","data":{"fn":["demo-abc.fnos.net:443"],"ipv4":["10.0.0.5"]}}"#;
        assert_eq!(parse_relay(body).unwrap(), "demo-abc.fnos.net:443");
        let missing = r#"{"code":3000037,"msg":"Not Found Error","data":null}"#;
        assert!(parse_relay(missing).is_err());
    }

    #[tokio::test]
    #[ignore = "requires FN_SYNC_TEST_FNID for a live FN Connect id"]
    async fn live_resolve_returns_relay_address() {
        let id = std::env::var("FN_SYNC_TEST_FNID").expect("fnid");
        let relay = resolve(&id).await.expect("resolve relay");
        assert!(
            relay.contains('.') && relay.contains(':'),
            "unexpected relay"
        );
    }
}
