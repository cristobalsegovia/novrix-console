//! Credential storage. Passwords and key passphrases never touch SQLite or
//! any config file — they live in the OS keychain (macOS Keychain via the
//! Security framework, Secret Service on Linux, Credential Manager on
//! Windows) under one service name, keyed by saved-session id.

use keyring::Entry;

const SERVICE: &str = "com.kingfisherit.switchboardterm";

pub fn password_account(session_id: &str) -> String {
    format!("{session_id}/password")
}

pub fn passphrase_account(session_id: &str) -> String {
    format!("{session_id}/passphrase")
}

pub fn set(account: &str, value: &str) -> Result<(), String> {
    Entry::new(SERVICE, account)
        .map_err(|e| e.to_string())?
        .set_password(value)
        .map_err(|e| e.to_string())
}

pub fn get(account: &str) -> Option<String> {
    Entry::new(SERVICE, account).ok()?.get_password().ok()
}

pub fn delete(account: &str) {
    if let Ok(entry) = Entry::new(SERVICE, account) {
        let _ = entry.delete_credential();
    }
}

/// Remove every secret associated with a saved session.
pub fn purge(session_id: &str) {
    delete(&password_account(session_id));
    delete(&passphrase_account(session_id));
}
