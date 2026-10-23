//! 加密配置备份：Argon2id 派生密钥 + AES-256-GCM。
//! 文件格式：`ABUF1`(5B) | salt(16B) | nonce(12B) | ciphertext(+16B tag)。
//! 明文为 JSON：`{"config": "...yaml...", "env": {"NAME": "value", ...}}`。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::Argon2;

pub const MAGIC: &[u8; 5] = b"ABUF1";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;

/// 密码 + 明文 → 加密字节串。
pub fn seal(password: &str, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::getrandom(&mut salt).map_err(|e| format!("随机数生成失败: {e}"))?;
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), &salt, &mut key)
        .map_err(|e| format!("密钥派生失败: {e}"))?;
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce).map_err(|e| format!("随机数生成失败: {e}"))?;
    let ct = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| format!("AES 初始化失败: {e}"))?
        .encrypt(Nonce::from_slice(&nonce), plaintext)
        .map_err(|_| "加密失败".to_string())?;
    let mut out = Vec::with_capacity(MAGIC.len() + SALT_LEN + NONCE_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// 密码 + 密文 → 明文。密码错误/文件损坏/篡改均返回 Err。
pub fn unseal(password: &str, blob: &[u8]) -> Result<Vec<u8>, String> {
    let head = MAGIC.len() + SALT_LEN + NONCE_LEN;
    if blob.len() < head + 16 || &blob[..MAGIC.len()] != MAGIC {
        return Err("不是有效的 AgentBuffet 加密备份文件".into());
    }
    let salt = &blob[MAGIC.len()..MAGIC.len() + SALT_LEN];
    let nonce = &blob[MAGIC.len() + SALT_LEN..head];
    let ct = &blob[head..];
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| format!("密钥派生失败: {e}"))?;
    Aes256Gcm::new_from_slice(&key)
        .map_err(|e| format!("AES 初始化失败: {e}"))?
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| "解密失败：密码错误或文件已损坏".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_wrong_password() {
        let pt = b"gateway:\n  addr: \"127.0.0.1:3427\"\n";
        let blob = seal("hunter2", pt).expect("seal");
        assert_eq!(&blob[..5], MAGIC);
        assert_eq!(unseal("hunter2", &blob).unwrap(), pt);
        assert!(unseal("wrong", &blob).is_err());
        let mut tampered = blob.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        assert!(unseal("hunter2", &tampered).is_err());
        assert!(unseal("hunter2", b"short").is_err());
    }
}
