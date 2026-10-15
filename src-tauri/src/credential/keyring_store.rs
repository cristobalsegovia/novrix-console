//! The real [`SecureStore`] backed by `keyring-rs` over OS Secure_Storage.
//!
//! Each [`CredentialKey`] value becomes the *user* of a `keyring::Entry` under
//! a fixed Novrix Console *service* namespace. On macOS this maps to a generic
//! Keychain item (Requirement 1.3); on Windows to a Credential Manager entry
//! (Requirement 1.4). `keyring` is the only thing that persists a secret, and
//! it never writes plaintext to a file we control.
//!
//! `keyring` is synchronous and may touch the OS keystore (which can block), so
//! each operation runs on a blocking thread via [`tokio::task::spawn_blocking`]
//! and the result is mapped onto the structured [`CredentialError`] variants.

use async_trait::async_trait;
use keyring::{Entry, Error as KeyringError};

use super::{Secret, SecureStore};
use crate::domain::CredentialError;

/// The service name under which every Novrix Console credential is filed in the
/// OS keystore. Keeping it constant isolates our entries from other apps'.
const SERVICE: &str = "com.novrix.console";

/// A [`SecureStore`] that persists secrets in the OS Keychain / Credential
/// Manager via `keyring-rs`.
#[derive(Debug, Clone)]
pub struct KeyringSecureStore {
    service: String,
}

impl Default for KeyringSecureStore {
    fn default() -> Self {
        Self {
            service: SERVICE.to_string(),
        }
    }
}

impl KeyringSecureStore {
    /// Build a store under a custom service namespace.
    ///
    /// Production code uses [`KeyringSecureStore::default`]; a distinct service
    /// name is useful to isolate test entries from the real app's.
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    /// Resolve a `keyring::Entry` for `key` under this store's service.
    fn entry(&self, key: &str) -> Result<Entry, KeyringError> {
        Entry::new(&self.service, key)
    }
}

#[async_trait]
impl SecureStore for KeyringSecureStore {
    async fn set(&self, key: &str, secret: &Secret) -> Result<(), CredentialError> {
        let service = self.service.clone();
        let key = key.to_string();
        let value = secret.expose().to_string();

        run_blocking(move || {
            let entry = Entry::new(&service, &key).map_err(store_failed)?;
            // On failure, `keyring` does not modify the existing entry, so a
            // previously stored value is left unchanged (Requirements 1.5,
            // 11.2).
            entry.set_password(&value).map_err(store_failed)
        })
        .await
    }

    async fn get(&self, key: &str) -> Result<Secret, CredentialError> {
        let entry = self.entry(key).map_err(retrieve_failed)?;

        run_blocking(move || match entry.get_password() {
            Ok(password) => Ok(Secret::new(password)),
            // A missing entry is a distinct, expected outcome — not a failure
            // to retrieve (Requirement 11.4).
            Err(KeyringError::NoEntry) => Err(CredentialError::NotFound),
            Err(e) => Err(retrieve_failed(e)),
        })
        .await
    }

    async fn delete(&self, key: &str) -> Result<(), CredentialError> {
        let entry = self.entry(key).map_err(delete_failed)?;

        run_blocking(move || match entry.delete_credential() {
            Ok(()) => Ok(()),
            // Deleting an absent entry is a successful no-op, so profile
            // deletion need not probe for existence first.
            Err(KeyringError::NoEntry) => Ok(()),
            Err(e) => Err(delete_failed(e)),
        })
        .await
    }
}

/// Run a blocking keyring operation off the async runtime, flattening the
/// `spawn_blocking` join error into a [`CredentialError`].
async fn run_blocking<T, F>(f: F) -> Result<T, CredentialError>
where
    F: FnOnce() -> Result<T, CredentialError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join) => Err(CredentialError::StoreFailed(format!(
            "secure-storage task failed to run: {join}"
        ))),
    }
}

fn store_failed(e: KeyringError) -> CredentialError {
    CredentialError::StoreFailed(e.to_string())
}

fn retrieve_failed(e: KeyringError) -> CredentialError {
    CredentialError::RetrieveFailed(e.to_string())
}

fn delete_failed(e: KeyringError) -> CredentialError {
    CredentialError::DeleteFailed(e.to_string())
}
