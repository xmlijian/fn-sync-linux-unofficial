//! NAS 混合加密：RSA-PKCS1v15 + AES-256-CBC + HMAC-SHA256。
//!
//! 与官方客户端/网页完全一致：
//!   key_str = 32 个随机字符 [0-9a-zA-Z]  → AES-256 密钥（ASCII 字节）
//!   iv      = 16 字节随机
//!   aes_ct  = AES-256-CBC(PKCS7)
//!   rsa_ct  = RSA-PKCS1v15(pub, key_str.as_bytes())

use aes::Aes256;
use anyhow::{anyhow, Result};
use base64::Engine;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use rand::Rng;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use sha2::Sha256;

type Enc = cbc::Encryptor<Aes256>;
type Dec = cbc::Decryptor<Aes256>;

const ALNUM: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

pub fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// 生成 32 个随机字符（AES 密钥材料）。
pub fn new_key_str() -> Vec<u8> {
    let mut rng = rand::rng();
    (0..32)
        .map(|_| ALNUM[rng.random_range(0..ALNUM.len())])
        .collect()
}

pub fn new_iv() -> [u8; 16] {
    let mut rng = rand::rng();
    let mut iv = [0u8; 16];
    rng.fill(&mut iv);
    iv
}

pub fn aes_cbc_encrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Vec<u8> {
    Enc::new_from_slices(key, iv)
        .expect("aes key/iv length")
        .encrypt_padded_vec_mut::<Pkcs7>(data)
}

pub fn aes_cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    Dec::new_from_slices(key, iv)
        .map_err(|e| anyhow!("aes init: {e}"))?
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|e| anyhow!("aes decrypt: {e}"))
}

pub fn rsa_encrypt_pkcs1v15(pub_pem: &str, data: &[u8]) -> Result<Vec<u8>> {
    let key = RsaPublicKey::from_public_key_pem(pub_pem)
        .map_err(|e| anyhow!("parse rsa public key: {e}"))?;
    key.encrypt(&mut rsa::rand_core::OsRng, Pkcs1v15Encrypt, data)
        .map_err(|e| anyhow!("rsa encrypt: {e}"))
}

/// HMAC-SHA256 签名，base64 输出（NAS 请求签名）。
pub fn hmac_b64(key: &[u8], msg: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    mac.update(msg.as_bytes());
    b64().encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_roundtrip() {
        let key = new_key_str();
        let iv = new_iv();
        let pt = b"{\"req\":\"user.login\"}";
        let ct = aes_cbc_encrypt(&key, &iv, pt);
        assert_ne!(&ct[..], &pt[..]);
        assert_eq!(ct.len() % 16, 0);
        assert_eq!(aes_cbc_decrypt(&key, &iv, &ct).unwrap(), pt);
    }

    #[test]
    fn key_str_charset() {
        let k = new_key_str();
        assert_eq!(k.len(), 32);
        assert!(k.iter().all(|c| ALNUM.contains(c)));
    }

    #[test]
    fn hmac_known() {
        let got = hmac_b64(b"key", "The quick brown fox jumps over the lazy dog");
        assert_eq!(got, "97yD9DBThCSxMpjmqm+xQ+9NWaFJRhdZl0edvC0aPNg=");
    }
}
