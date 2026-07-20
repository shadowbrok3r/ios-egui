//! Password-encrypted Settings backups (AES-256-GCM + Argon2id).

use crate::types::Settings;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::RngCore;

const MAGIC: &[u8; 8] = b"CMFYBK01";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// Argon2id params tuned for interactive mobile unlock.
fn kdf() -> Argon2<'static> {
    let params = Params::new(19_456, 2, 1, Some(KEY_LEN)).expect("argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

fn derive_key(passphrase: &str, salt: &[u8]) -> Result<[u8; KEY_LEN], String> {
    let mut key = [0u8; KEY_LEN];
    kdf()
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| format!("kdf: {e}"))?;
    Ok(key)
}

/// Encrypt `settings` JSON under `passphrase`. File bytes: magic || salt || nonce || ciphertext.
pub fn encrypt(settings: &Settings, passphrase: &str) -> Result<Vec<u8>, String> {
    if passphrase.len() < 8 {
        return Err("passphrase must be at least 8 characters".into());
    }
    let json = serde_json::to_vec_pretty(settings).map_err(|e| format!("json: {e}"))?;
    let mut salt = [0u8; SALT_LEN];
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let key = derive_key(passphrase, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("cipher: {e}"))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, json.as_ref())
        .map_err(|_| "encrypt failed".to_string())?;
    let mut out = Vec::with_capacity(MAGIC.len() + SALT_LEN + NONCE_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt a `CMFYBK01` blob back into Settings.
pub fn decrypt(bytes: &[u8], passphrase: &str) -> Result<Settings, String> {
    if passphrase.is_empty() {
        return Err("passphrase required".into());
    }
    let min = MAGIC.len() + SALT_LEN + NONCE_LEN + 16;
    if bytes.len() < min {
        return Err("backup file too short".into());
    }
    if !bytes.starts_with(MAGIC) {
        return Err("not a ComfyUI encrypted backup (bad magic)".into());
    }
    let salt = &bytes[8..8 + SALT_LEN];
    let nonce_bytes = &bytes[8 + SALT_LEN..8 + SALT_LEN + NONCE_LEN];
    let ct = &bytes[8 + SALT_LEN + NONCE_LEN..];
    let key = derive_key(passphrase, salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("cipher: {e}"))?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let json = cipher
        .decrypt(nonce, ct)
        .map_err(|_| "decrypt failed — wrong passphrase or corrupt file".to_string())?;
    serde_json::from_slice(&json).map_err(|e| format!("settings json: {e}"))
}

/// Default backup filename with a UTC-ish local timestamp.
pub fn default_filename() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("comfyui-backup-{secs}.comfybk")
}

/// List `*.comfybk` files under `dirs` (basename + full path), newest name last.
pub fn list_backups(dirs: &[impl AsRef<std::path::Path>]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if name.ends_with(".comfybk") && path.is_file() {
                out.push((name.to_string(), path.display().to_string()));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Settings {
        use crate::types::Params;
        let params = Params::default();
        serde_json::from_value(serde_json::json!({
            "server_url": "https://comfy.example/api",
            "api_key": "secret-key",
            "username": "shadow",
            "session": "sess-token",
            "params": params,
            "characters": [{"name": "Eve", "identity": "1girl"}],
        }))
        .unwrap()
    }

    #[test]
    fn round_trip_preserves_credentials_and_characters() {
        let s = sample();
        let blob = encrypt(&s, "test-pass-ok").unwrap();
        assert!(blob.starts_with(MAGIC));
        let back = decrypt(&blob, "test-pass-ok").unwrap();
        assert_eq!(back.server_url, s.server_url);
        assert_eq!(back.api_key, s.api_key);
        assert_eq!(back.username, s.username);
        assert_eq!(back.session, s.session);
        assert_eq!(back.characters.len(), 1);
        assert_eq!(back.characters[0].name, "Eve");
    }

    #[test]
    fn wrong_passphrase_fails() {
        let blob = encrypt(&sample(), "test-pass-ok").unwrap();
        assert!(decrypt(&blob, "wrong-password").is_err());
    }

    #[test]
    fn short_passphrase_rejected() {
        assert!(encrypt(&sample(), "short").is_err());
    }
}
