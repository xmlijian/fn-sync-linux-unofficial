//! x-sign 签名 — 逆向自 fn-sync 0.2.3，**已与真实 NAS 端到端验证通过**。
//!
//! 算法（抓包 + Ghidra 反编译 + 真机验证三重确认）：
//!
//! ```text
//! digest = hex(sha256(action))                       // 小写 hex，64 字符
//! msg    = "{SIGN_SLOT1}_{path}_{nonce}_{timestamp}_{digest}"
//! sign   = HMAC-SHA256(key = SIGN_HMAC_KEY, msg)     // 小写 hex
//! x-sign = "nonce={nonce}&timestamp={timestamp}&sign={sign}"
//! ```
//!
//! 其中 `path` 是请求路径（如 `/sync/comm`），`action` 是 `x-comm-action` 的值，
//! `timestamp` 为毫秒，`nonce` ∈ [10000, 100000000)。
//!
//! 真机验证：对 fnOS NAS 发 `x-comm-action: query_nas_os_edition`，
//! 返回 `Mainland`（NAS 版本信息），而非 `invalid sign`。
//!
//! 注意：服务端**有 nonce 重放保护**（重复 nonce 返回 `duplicat nonce`），
//! 每次都必须用新 nonce。
//!
//! 常量来源（`_OBFBYTES_SDATA` 混淆块逐字节解码，26 字节整）：
//!   SIGN_SLOT1  = "Fb93ZNftshbftT9143nbFtf8Md"
//!   SIGN_HMAC_KEY = "zF01TfcmzRhmcs3qlzk4zh36nC"

use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::{Digest, Sha256};

/// HMAC 密钥（gen_sign 中 `hmac::get_der_key` 的 26 字节输入）。
pub const SIGN_HMAC_KEY: &str = "zF01TfcmzRhmcs3qlzk4zh36nC";

/// 签名消息首段（26 字节常量）。
pub const SIGN_SLOT1: &str = "Fb93ZNftshbftT9143nbFtf8Md";

/// nonce 取值范围（gen_sign 实测 [10000, 100000000)）。
pub const NONCE_RANGE: (u32, u32) = (10_000, 100_000_000);

pub fn gen_nonce() -> u32 {
    rand::rng().random_range(NONCE_RANGE.0..NONCE_RANGE.1)
}

pub fn timestamp_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 小写 hex 的 md5（上传 `x-checksum` 等场景使用）。
pub fn md5_hex(data: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(data);
    let mut s = String::with_capacity(32);
    for b in h.finalize() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// 小写 hex 的 sha256。
pub fn hex_sha256(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    let mut s = String::with_capacity(64);
    for b in h.finalize() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// 小写 hex 的 HMAC-SHA256。
pub fn hmac_sha256_hex(key: &str, message: &str) -> String {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    let mut s = String::with_capacity(64);
    for b in mac.finalize().into_bytes() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// 组装 HMAC 输入：`slot1_path_nonce_timestamp_digest`
pub fn build_sign_message(path: &str, action: &str, nonce: u32, timestamp: i64) -> String {
    let digest = hex_sha256(action.as_bytes());
    format!("{SIGN_SLOT1}_{path}_{nonce}_{timestamp}_{digest}")
}

/// 计算签名值（不含 `nonce=` 等字段名）。
pub fn compute_sign(path: &str, action: &str, nonce: u32, timestamp: i64) -> String {
    let msg = build_sign_message(path, action, nonce, timestamp);
    hmac_sha256_hex(SIGN_HMAC_KEY, &msg)
}

/// 生成完整的 `x-sign` 头值，同时返回所用的 (nonce, timestamp)。
pub fn gen_x_sign(path: &str, action: &str) -> (String, u32, i64) {
    let nonce = gen_nonce();
    let ts = timestamp_ms();
    let sign = compute_sign(path, action, nonce, ts);
    (
        format!("nonce={nonce}&timestamp={ts}&sign={sign}"),
        nonce,
        ts,
    )
}

/// 路径的 base64（`x-watch-root` / `x-file` 实测为 base64 编码）。
pub fn b64_path(path: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(path.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实抓包样本 1（官方 Windows 客户端 0.2.3 → 假 NAS）。
    #[test]
    fn matches_captured_signature_sample_1() {
        let sign = compute_sign("/sync/comm", "query_nas_os_edition", 8120301, 1795158318931);
        assert_eq!(
            sign,
            "debb01bfa9ed15cb5ad43ab27a1b08f444eea1a24582100af5b41a344cb2b077"
        );
    }

    /// 真实抓包样本 2（另一次连接）。
    #[test]
    fn matches_captured_signature_sample_2() {
        let sign = compute_sign("/sync/comm", "query_nas_os_edition", 5295902, 1795159161807);
        assert_eq!(
            sign,
            "2ad7e1e593238cd0f926ab1695badecebd4f643a0680f941201729b1fc6555a5"
        );
    }

    #[test]
    fn constants_are_26_bytes() {
        assert_eq!(SIGN_SLOT1.len(), 26);
        assert_eq!(SIGN_HMAC_KEY.len(), 26);
    }

    #[test]
    fn message_shape() {
        let m = build_sign_message("/sync/comm", "query_nas_os_edition", 8120301, 1795158318931);
        assert_eq!(
            m,
            "Fb93ZNftshbftT9143nbFtf8Md_/sync/comm_8120301_1795158318931_\
             8cf3f101a77264fad2dd99a3b17349e3b0b6f5a3488e50fa7429b447f03f416d"
        );
    }

    #[test]
    fn digest_of_positive_action() {
        assert_eq!(
            hex_sha256(b"query_nas_os_edition"),
            "8cf3f101a77264fad2dd99a3b17349e3b0b6f5a3488e50fa7429b447f03f416d"
        );
    }

    #[test]
    fn nonce_in_observed_range() {
        for _ in 0..200 {
            let n = gen_nonce();
            assert!(
                n >= NONCE_RANGE.0 && n < NONCE_RANGE.1,
                "nonce {n} out of range"
            );
        }
    }

    #[test]
    fn b64_path_matches_capture() {
        // 抓包实测 x-watch-root: Lw== 即 base64("/")
        assert_eq!(b64_path("/"), "Lw==");
    }

    #[test]
    fn hmac_matches_rfc4231() {
        assert_eq!(
            hmac_sha256_hex("Jefe", "what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn x_sign_format() {
        let (v, nonce, ts) = gen_x_sign("/sync/comm", "query_nas_os_edition");
        let parts: Vec<&str> = v.split('&').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], format!("nonce={nonce}"));
        assert_eq!(parts[1], format!("timestamp={ts}"));
        assert!(parts[2].starts_with("sign="));
        assert_eq!(parts[2].len(), "sign=".len() + 64);
        // 自洽性：用返回的 nonce/ts 重算应一致
        assert_eq!(
            &parts[2]["sign=".len()..],
            compute_sign("/sync/comm", "query_nas_os_edition", nonce, ts)
        );
    }

    #[test]
    fn md5_hex_known_value() {
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn hex_sha256_known_value() {
        assert_eq!(
            hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
