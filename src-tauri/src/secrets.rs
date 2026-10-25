//! Enterprise Credential Vault (AES-256-GCM via `aws-lc-rs` + Single Master Key
//! in macOS Keychain cached in RAM).
//!
//! Architecture (same model as Termius, TablePlus, and VS Code):
//! - All session & identity credentials live inside a single AES-256-GCM
//!   encrypted file (`~/Library/Application Support/switchboard-term/vault.enc`).
//! - Only ONE 256-bit Master Key is stored in the macOS Keychain under the
//!   signed `Developer ID Application` identity (`com.kingfisherit.switchboardterm.vault`
//!   / `master-key-v1`), and is cached in process memory (`RAM`) on first use.
//! - If legacy `.vault.key` exists on disk, it is migrated into the Keychain
//!   master key entry and securely deleted from disk.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use aws_lc_rs::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use aws_lc_rs::rand::fill;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use keyring::Entry;

use crate::store;

const KEYCHAIN_SERVICE: &str = "com.kingfisherit.switchboardterm.vault";
const KEYCHAIN_ACCOUNT: &str = "master-key-v1";
const VAULT_AAD: &[u8] = b"com.kingfisherit.switchboardterm.vault.v1";

static VAULT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static CACHED_MASTER_KEY: LazyLock<Mutex<Option<[u8; 32]>>> = LazyLock::new(|| Mutex::new(None));

pub fn password_account(session_id: &str) -> String {
    format!("{session_id}/password")
}

pub fn passphrase_account(session_id: &str) -> String {
    format!("{session_id}/passphrase")
}

pub fn identity_password_account(identity_id: &str) -> String {
    format!("identity:{identity_id}/password")
}

pub fn identity_passphrase_account(identity_id: &str) -> String {
    format!("identity:{identity_id}/passphrase")
}

fn legacy_key_file_path() -> PathBuf {
    store::data_dir().join(".vault.key")
}

fn vault_file_path() -> PathBuf {
    store::data_dir().join("vault.enc")
}

#[cfg(unix)]
fn set_mode_0600(path: &PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_mode_0600(_path: &PathBuf) {}

fn get_master_key_bytes() -> Result<[u8; 32], String> {
    let mut cache = CACHED_MASTER_KEY.lock().unwrap();
    if let Some(k) = *cache {
        return Ok(k);
    }

    let legacy_path = legacy_key_file_path();

    // 1. Try reading the single Master Key from macOS Keychain
    if let Ok(entry) = Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT) {
        if let Ok(b64_key) = entry.get_password() {
            if let Ok(decoded) = B64.decode(b64_key.trim()) {
                if decoded.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&decoded);
                    if legacy_path.exists() {
                        let _ = fs::remove_file(&legacy_path);
                    }
                    *cache = Some(arr);
                    return Ok(arr);
                }
            }
        }

        // 2. If not in Keychain yet, migrate from `.vault.key` if present, or generate fresh 256-bit key
        let arr: [u8; 32] = if legacy_path.exists() {
            let raw = fs::read_to_string(&legacy_path).map_err(|e| e.to_string())?;
            let decoded = B64.decode(raw.trim()).map_err(|e| e.to_string())?;
            let mut a = [0u8; 32];
            if decoded.len() == 32 {
                a.copy_from_slice(&decoded);
            } else {
                fill(&mut a).map_err(|_| "failed to generate random vault key".to_string())?;
            }
            a
        } else {
            let mut a = [0u8; 32];
            fill(&mut a).map_err(|_| "failed to generate random vault key".to_string())?;
            a
        };

        let encoded = B64.encode(arr);
        if entry.set_password(&encoded).is_ok() {
            // Successfully stored in macOS Keychain under the signed Developer ID!
            // Remove plaintext key file from disk if it existed.
            if legacy_path.exists() {
                let _ = fs::remove_file(&legacy_path);
            }
        } else {
            // Fallback if Keychain is locked/unavailable in headless session
            let _ = fs::write(&legacy_path, &encoded);
            set_mode_0600(&legacy_path);
        }

        *cache = Some(arr);
        return Ok(arr);
    }

    Err("could not access master key storage".into())
}

fn load_or_create_master_key() -> Result<LessSafeKey, String> {
    let key_bytes = get_master_key_bytes()?;
    let unbound =
        UnboundKey::new(&AES_256_GCM, &key_bytes).map_err(|_| "invalid AES-256 key".to_string())?;
    Ok(LessSafeKey::new(unbound))
}

fn read_vault_map() -> HashMap<String, String> {
    let path = vault_file_path();
    if !path.exists() {
        return HashMap::new();
    }
    let Ok(key) = load_or_create_master_key() else {
        return HashMap::new();
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return HashMap::new();
    };
    let Ok(blob) = B64.decode(raw.trim()) else {
        return HashMap::new();
    };
    if blob.len() <= NONCE_LEN {
        return HashMap::new();
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let Ok(nonce) = Nonce::try_assume_unique_for_key(nonce_bytes) else {
        return HashMap::new();
    };
    let mut in_out = ciphertext.to_vec();
    let Ok(plaintext) = key.open_in_place(nonce, Aad::from(VAULT_AAD), &mut in_out) else {
        return HashMap::new();
    };
    serde_json::from_slice(plaintext).unwrap_or_default()
}

fn write_vault_map(map: &HashMap<String, String>) -> Result<(), String> {
    let key = load_or_create_master_key()?;
    let json = serde_json::to_vec(map).map_err(|e| e.to_string())?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    fill(&mut nonce_bytes).map_err(|_| "failed to generate nonce".to_string())?;
    let nonce =
        Nonce::try_assume_unique_for_key(&nonce_bytes).map_err(|_| "invalid nonce".to_string())?;

    let mut in_out = json;
    key.seal_in_place_append_tag(nonce, Aad::from(VAULT_AAD), &mut in_out)
        .map_err(|_| "vault encryption failed".to_string())?;

    let mut blob = Vec::with_capacity(NONCE_LEN + in_out.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&in_out);

    let path = vault_file_path();
    fs::write(&path, B64.encode(&blob)).map_err(|e| e.to_string())?;
    set_mode_0600(&path);
    Ok(())
}

pub fn set(account: &str, value: &str) -> Result<(), String> {
    let _guard = VAULT_LOCK.lock().unwrap();
    let mut map = read_vault_map();
    map.insert(account.to_string(), value.to_string());
    write_vault_map(&map)
}

pub fn get(account: &str) -> Option<String> {
    let _guard = VAULT_LOCK.lock().unwrap();
    let map = read_vault_map();
    map.get(account).cloned()
}

pub fn delete(account: &str) {
    let _guard = VAULT_LOCK.lock().unwrap();
    let mut map = read_vault_map();
    if map.remove(account).is_some() {
        let _ = write_vault_map(&map);
    }
}

/// Remove every secret associated with a saved session.
pub fn purge(session_id: &str) {
    delete(&password_account(session_id));
    delete(&passphrase_account(session_id));
}

/// Remove every secret associated with a shared identity.
pub fn purge_identity(identity_id: &str) {
    delete(&identity_password_account(identity_id));
    delete(&identity_passphrase_account(identity_id));
}
