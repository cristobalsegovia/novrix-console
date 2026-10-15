//! Credential_Store: a thin, auditable wrapper over OS Secure_Storage.
//!
//! This module is the single place the Novrix Console core touches secrets. It
//! stores, retrieves, and deletes passwords/passphrases keyed by a
//! [`CredentialKey`], **never persisting plaintext to disk** — the only durable
//! home for a secret is the OS Secure_Storage (macOS Keychain / Windows
//! Credential Manager), reached through `keyring-rs`.
//!
//! The design keeps three concerns separate so each is independently testable:
//!
//! - [`Secret`] — a plaintext secret wrapper that refuses to leak its contents
//!   through `Debug`/`Display` and best-effort scrubs its memory on drop.
//! - [`SecureStore`] — a small, string-keyed backend trait (`set`/`get`/`delete`)
//!   over the OS store. The real implementation is [`KeyringSecureStore`]; the
//!   property/integration tasks (5.2–5.5) can swap in an in-memory mock without
//!   touching the Keychain.
//! - [`CredentialStore`] — the component-level trait from the design
//!   (`save`/`get`/`delete_for_profile`). [`KeyringCredentialStore`] implements
//!   it on top of any [`SecureStore`].
//!
//! Failure policy (design "Secure storage failures"; Requirements 1.5, 11.2,
//! 11.4): a failing `save` returns a structured [`CredentialError`] and does
//! **not** touch any previously stored value — the caller keeps its in-memory
//! secret. A `get` for a missing or unretrievable credential returns an error
//! and no value.

use async_trait::async_trait;

use crate::domain::{CredentialError, CredentialKey, ProfileId};

mod secret;
pub use secret::Secret;

mod keyring_store;
pub use keyring_store::KeyringSecureStore;

/// The default sub-key under a profile namespace that holds the profile's
/// password credential.
const PROFILE_PASSWORD_SUBKEY: &str = "password";

/// Build the [`CredentialKey`] under which a profile's password is stored.
///
/// Profile-scoped credentials are namespaced `profile/{profile_id}/{sub_key}`.
/// Centralizing the convention here keeps the Credential_Store and the profile
/// deletion path (task 3.5) in agreement about which keys belong to a profile,
/// which is what makes [`CredentialStore::delete_for_profile`] able to remove
/// them without enumerating the OS store (OS keystores are not enumerable).
pub fn profile_credential_key(profile: ProfileId, sub_key: &str) -> CredentialKey {
    CredentialKey::new(format!("profile/{profile}/{sub_key}"))
}

/// Build the [`CredentialKey`] for a profile's password credential.
pub fn profile_password_key(profile: ProfileId) -> CredentialKey {
    profile_credential_key(profile, PROFILE_PASSWORD_SUBKEY)
}

/// Backend abstraction over an OS secure-storage keystore.
///
/// Keyed by an opaque string (the [`CredentialKey`] value). This trait is the
/// seam that lets tests substitute an in-memory store for the real Keychain /
/// Credential Manager. Implementations MUST:
///
/// - never persist the plaintext anywhere but the secure store;
/// - on a failing [`set`](SecureStore::set), leave any previously stored value
///   for that key unchanged;
/// - report a missing entry as [`CredentialError::NotFound`] from
///   [`get`](SecureStore::get), and surface other failures as the matching
///   `*Failed` variant.
#[async_trait]
pub trait SecureStore: Send + Sync {
    /// Store (or replace) the secret for `key`.
    async fn set(&self, key: &str, secret: &Secret) -> Result<(), CredentialError>;

    /// Retrieve the secret for `key`, or [`CredentialError::NotFound`] when the
    /// entry does not exist.
    async fn get(&self, key: &str) -> Result<Secret, CredentialError>;

    /// Delete the entry for `key`. Deleting a key with no entry is a no-op
    /// success (idempotent), so profile deletion need not probe first.
    async fn delete(&self, key: &str) -> Result<(), CredentialError>;
}

/// The Credential_Store component interface (design "Components and
/// Interfaces").
///
/// Operations are keyed by [`CredentialKey`] / [`ProfileId`] and return a
/// structured [`CredentialError`] on failure. Secrets cross this boundary only
/// as [`Secret`], never as bare `String`.
#[async_trait]
pub trait CredentialStore: Send + Sync {
    /// Store `secret` under `key` in Secure_Storage (Requirement 11.1).
    ///
    /// On failure the previously stored value for `key` is left unchanged and
    /// the caller's in-memory `secret` is untouched (Requirements 1.5, 11.2).
    async fn save(&self, key: CredentialKey, secret: Secret) -> Result<(), CredentialError>;

    /// Retrieve the secret stored under `key` (Requirement 11.3).
    ///
    /// Returns an error and no value when the credential is missing or cannot
    /// be retrieved (Requirement 11.4).
    async fn get(&self, key: CredentialKey) -> Result<Secret, CredentialError>;

    /// Remove every credential associated with `profile` (Requirement 11.6).
    async fn delete_for_profile(&self, id: ProfileId) -> Result<(), CredentialError>;
}

/// [`CredentialStore`] implemented over any [`SecureStore`] backend.
///
/// Holds no plaintext itself — every secret flows straight through to the
/// backend. Generic over the backend so production code uses
/// [`KeyringSecureStore`] while tests use an in-memory mock.
pub struct KeyringCredentialStore<S: SecureStore> {
    backend: S,
}

impl<S: SecureStore> KeyringCredentialStore<S> {
    /// Wrap a secure-storage backend.
    pub fn new(backend: S) -> Self {
        Self { backend }
    }

    /// Borrow the underlying backend (useful for tests/diagnostics).
    pub fn backend(&self) -> &S {
        &self.backend
    }
}

impl KeyringCredentialStore<KeyringSecureStore> {
    /// Construct a Credential_Store backed by the OS Keychain / Credential
    /// Manager under the Novrix Console service namespace.
    pub fn with_os_keystore() -> Self {
        Self::new(KeyringSecureStore::default())
    }
}

#[async_trait]
impl<S: SecureStore> CredentialStore for KeyringCredentialStore<S> {
    async fn save(&self, key: CredentialKey, secret: Secret) -> Result<(), CredentialError> {
        self.backend.set(key.as_str(), &secret).await
    }

    async fn get(&self, key: CredentialKey) -> Result<Secret, CredentialError> {
        self.backend.get(key.as_str()).await
    }

    async fn delete_for_profile(&self, id: ProfileId) -> Result<(), CredentialError> {
        // Remove the known profile-scoped credential(s). OS keystores cannot be
        // enumerated, so deletion targets the keys the Credential_Store itself
        // owns for a profile (see `profile_credential_key`). `delete` is
        // idempotent, so absent entries are not treated as failures.
        self.backend
            .delete(profile_password_key(id).as_str())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Minimal in-memory [`SecureStore`] for exercising the Credential_Store
    /// logic without the OS keystore. (The property/integration tasks 5.2–5.5
    /// build the full-featured mocks; this one keeps *this* module honest.)
    #[derive(Default)]
    struct InMemoryStore {
        entries: Mutex<HashMap<String, String>>,
        /// When set, the next `set` fails without mutating `entries`.
        fail_next_set: Mutex<bool>,
    }

    impl InMemoryStore {
        fn fail_next_set(&self) {
            *self.fail_next_set.lock().unwrap() = true;
        }
    }

    #[async_trait]
    impl SecureStore for InMemoryStore {
        async fn set(&self, key: &str, secret: &Secret) -> Result<(), CredentialError> {
            if std::mem::replace(&mut *self.fail_next_set.lock().unwrap(), false) {
                // Simulate a backend failure: do NOT touch stored state.
                return Err(CredentialError::StoreFailed("simulated failure".into()));
            }
            self.entries
                .lock()
                .unwrap()
                .insert(key.to_string(), secret.expose().to_string());
            Ok(())
        }

        async fn get(&self, key: &str) -> Result<Secret, CredentialError> {
            self.entries
                .lock()
                .unwrap()
                .get(key)
                .map(|v| Secret::new(v.clone()))
                .ok_or(CredentialError::NotFound)
        }

        async fn delete(&self, key: &str) -> Result<(), CredentialError> {
            self.entries.lock().unwrap().remove(key);
            Ok(())
        }
    }

    #[tokio::test]
    async fn save_then_get_roundtrips_the_secret() {
        let store = KeyringCredentialStore::new(InMemoryStore::default());
        let key = CredentialKey::new("profile/x/password");
        store
            .save(key.clone(), Secret::new("hunter2"))
            .await
            .unwrap();
        let got = store.get(key).await.unwrap();
        assert_eq!(got.expose(), "hunter2");
    }

    #[tokio::test]
    async fn get_missing_credential_returns_not_found_and_no_value() {
        let store = KeyringCredentialStore::new(InMemoryStore::default());
        let err = store
            .get(CredentialKey::new("absent"))
            .await
            .unwrap_err();
        assert_eq!(err, CredentialError::NotFound);
    }

    #[tokio::test]
    async fn save_failure_preserves_previously_stored_value() {
        let backend = InMemoryStore::default();
        let store = KeyringCredentialStore::new(backend);
        let key = CredentialKey::new("profile/x/password");

        // Seed a previously stored value.
        store
            .save(key.clone(), Secret::new("old"))
            .await
            .unwrap();

        // Arrange for the next save to fail.
        store.backend().fail_next_set();
        let err = store
            .save(key.clone(), Secret::new("new"))
            .await
            .unwrap_err();
        assert!(matches!(err, CredentialError::StoreFailed(_)));

        // The previously stored value must be unchanged.
        assert_eq!(store.get(key).await.unwrap().expose(), "old");
    }

    #[tokio::test]
    async fn delete_for_profile_removes_the_profile_password() {
        let store = KeyringCredentialStore::new(InMemoryStore::default());
        let profile = ProfileId::new();
        let key = profile_password_key(profile);

        store
            .save(key.clone(), Secret::new("secret"))
            .await
            .unwrap();
        store.delete_for_profile(profile).await.unwrap();

        assert_eq!(store.get(key).await.unwrap_err(), CredentialError::NotFound);
    }

    #[tokio::test]
    async fn delete_for_profile_is_idempotent_when_nothing_stored() {
        let store = KeyringCredentialStore::new(InMemoryStore::default());
        // No credential stored; deletion still succeeds.
        store.delete_for_profile(ProfileId::new()).await.unwrap();
    }

    #[test]
    fn profile_credential_key_is_namespaced_by_profile() {
        let profile = ProfileId::new();
        let key = profile_password_key(profile);
        assert_eq!(key.as_str(), format!("profile/{profile}/password"));
    }

    /// Property-based tests for the Credential_Store (tasks 5.2–5.4).
    ///
    /// The `CredentialStore` surface is async; proptest bodies are sync, so each
    /// property builds a single-threaded Tokio runtime and `block_on`s the
    /// operations. The in-memory [`InMemoryStore`] above is reused as the
    /// swappable backend so the Keychain is never touched.
    mod properties {
        use super::*;
        use proptest::prelude::*;
        use tokio::runtime::Runtime;

        /// A fresh current-thread runtime for driving one property case.
        fn runtime() -> Runtime {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("build tokio runtime")
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            // Feature: remote-connection-manager, Property 2: Credential-storage failure preserves data and reports error
            //
            // *For any* credential value, when the underlying store fails, the
            // unsaved credential remains available in memory, any previously
            // stored value is left unchanged, and an error indication is
            // returned.
            //
            // Validates: Requirements 1.5, 11.2
            #[test]
            fn storage_failure_preserves_data_and_reports_error(
                key in "[a-z]{1,16}/[a-z]{1,16}",
                old_value in ".{0,64}",
                new_value in ".{0,64}",
            ) {
                let store = KeyringCredentialStore::new(InMemoryStore::default());
                let key = CredentialKey::new(key);
                let new_secret = Secret::new(new_value.clone());

                runtime().block_on(async {
                    // Seed a previously stored value.
                    store
                        .save(key.clone(), Secret::new(old_value.clone()))
                        .await
                        .expect("seed save succeeds");

                    // Arrange for the next backend `set` to fail.
                    store.backend().fail_next_set();

                    // The failing save returns an error indication...
                    let err = store
                        .save(key.clone(), new_secret.clone())
                        .await
                        .expect_err("save must fail when the backend fails");
                    prop_assert!(matches!(err, CredentialError::StoreFailed(_)));

                    // ...the caller's in-memory (unsaved) credential is intact...
                    prop_assert_eq!(new_secret.expose(), new_value.as_str());

                    // ...and the previously stored value is left unchanged.
                    let stored = store.get(key.clone()).await.expect("previous value remains");
                    prop_assert_eq!(stored.expose(), old_value.as_str());

                    Ok(())
                })?;
            }

            // Feature: remote-connection-manager, Property 30: Missing credential retrieval returns an error, never a value
            //
            // *For any* credential key that is absent or unretrievable,
            // retrieval returns an error indication and no credential value.
            //
            // Validates: Requirement 11.4
            #[test]
            fn missing_credential_retrieval_returns_error_never_value(
                key in ".{0,64}",
            ) {
                // A pristine store never holds the key, so every key is absent.
                let store = KeyringCredentialStore::new(InMemoryStore::default());
                let key = CredentialKey::new(key);

                runtime().block_on(async {
                    let result = store.get(key).await;
                    // No credential value is ever returned...
                    prop_assert!(result.is_err());
                    // ...and the error is specifically NotFound.
                    prop_assert_eq!(result.unwrap_err(), CredentialError::NotFound);
                    Ok(())
                })?;
            }

            // Feature: remote-connection-manager, Property 32: Deleting a profile removes its credentials
            //
            // *For any* profile, after it is deleted no credential associated
            // with that profile remains in Secure_Storage.
            //
            // Validates: Requirement 11.6
            #[test]
            fn deleting_a_profile_removes_its_credentials(
                password in ".{0,64}",
            ) {
                let store = KeyringCredentialStore::new(InMemoryStore::default());
                let profile = ProfileId::new();
                let key = profile_password_key(profile);

                runtime().block_on(async {
                    // Store the profile's password credential.
                    store
                        .save(key.clone(), Secret::new(password.clone()))
                        .await
                        .expect("save profile password");

                    // Delete the profile.
                    store
                        .delete_for_profile(profile)
                        .await
                        .expect("delete_for_profile succeeds");

                    // No credential associated with that profile remains.
                    let after = store.get(key).await;
                    prop_assert_eq!(after.unwrap_err(), CredentialError::NotFound);
                    Ok(())
                })?;
            }
        }
    }
}
