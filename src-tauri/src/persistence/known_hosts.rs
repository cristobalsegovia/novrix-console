//! Persistent known-hosts / known-certs store and the pure host-key /
//! certificate **trust decision** (task 26.3).
//!
//! Replacing the trust-on-first-use stubs with real trust needs three pieces,
//! two of which live here:
//!
//! 1. A **persistent store** mapping a `(host, port, kind)` key to the single
//!    fingerprint the user has accepted for it, backed by the same SQLite
//!    [`Database`](crate::persistence::Database) the profiles live in (schema
//!    version 3 adds the `known_hosts` table — see
//!    [`migrate_to_v3`](crate::persistence::migrate_to_v3)). One store type
//!    serves **both** SSH host keys and RDP certificate fingerprints,
//!    distinguished by the [`TrustKind`] column, so a host reused across SSH
//!    and RDP keeps independent entries.
//! 2. The **pure trust decision** [`decide_trust`]: given the stored
//!    fingerprint (if any), the presented fingerprint, and the profile's
//!    [`HostKeyPolicy`], it returns a [`TrustDecision`] with no I/O. This is the
//!    unit-testable heart of the feature.
//!
//! The third piece — the UI prompt channel and the verifier wiring that
//! consults this store, applies the policy, and (on first use) prompts — lives
//! in the IPC layer (`ipc/mod.rs`, `ipc/events.rs`).
//!
//! # Fingerprint format and secret hygiene
//!
//! A fingerprint here is the `SHA256:<base64>` string russh already produces
//! for SSH host keys ([`russh::keys::ssh_key::Fingerprint`]); RDP certificate
//! fingerprints are computed in the **same** shape by [`certificate_fingerprint`]
//! so the one store and the one decision function handle both. A fingerprint is
//! a digest, not a secret — it is safe to persist and to show the user — but the
//! full key/certificate material it is derived from is never stored here.

use rusqlite::OptionalExtension;

use crate::domain::{DomainError, HostKeyPolicy};

use super::Database;

/// Which kind of trusted fingerprint an entry records.
///
/// One `known_hosts` table serves both protocols; the `kind` column keeps SSH
/// host keys and RDP certificate fingerprints in separate namespaces so the
/// same `host:port` can be trusted independently for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustKind {
    /// An SSH server host-key fingerprint.
    SshHostKey,
    /// An RDP server TLS leaf-certificate fingerprint.
    RdpCertificate,
}

impl TrustKind {
    /// The stable string stored in the `kind` column.
    pub fn as_str(self) -> &'static str {
        match self {
            TrustKind::SshHostKey => "ssh-host-key",
            TrustKind::RdpCertificate => "rdp-certificate",
        }
    }
}

/// The outcome of the pure [`decide_trust`] rule.
///
/// The verifier maps each variant to an action: [`Proceed`](Self::Proceed)
/// connects silently, [`Prompt`](Self::Prompt) asks the user (and on accept
/// remembers + proceeds, on decline rejects), and [`Reject`](Self::Reject)
/// aborts the connect with `HostKeyDeclined` without prompting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustDecision {
    /// The presented fingerprint matches the stored one — trust it silently.
    Proceed,
    /// No entry is stored yet and the policy permits prompting — ask the user.
    /// On accept the caller must [`remember`](KnownHostsStore::remember) the
    /// fingerprint and proceed; on decline it must reject.
    Prompt,
    /// Reject without prompting. Either the policy is `Strict` and nothing is
    /// stored, or a **changed** fingerprint was presented (a hard fail; see the
    /// module docs). The caller aborts the connect with `HostKeyDeclined`.
    Reject,
}

/// The pure host-key / certificate trust decision (task 26.3, Requirement 4.7/4.8).
///
/// Inputs:
/// - `stored`: the fingerprint previously accepted for this `(host, port, kind)`,
///   or `None` if the host is unknown.
/// - `presented`: the fingerprint the server presented on this connect.
/// - `policy`: the profile's [`HostKeyPolicy`].
///
/// Decision table:
///
/// | stored        | policy            | result    |
/// |---------------|-------------------|-----------|
/// | `Some(== p)`  | *any*             | `Proceed` |
/// | `Some(!= p)`  | *any*             | `Reject`  | ← **changed key is a hard fail** |
/// | `None`        | `Strict`          | `Reject`  |
/// | `None`        | `PromptOnUnknown` | `Prompt`  |
/// | `None`        | `AcceptAny`       | `Prompt`* |
///
/// \* `AcceptAny` still routes through `Prompt`; the verifier treats an
/// `AcceptAny` policy as auto-accepting the prompt (and remembering), so the
/// opt-in insecure policy records the key rather than re-accepting it blindly
/// every connect. This keeps the "changed key is detected" guarantee even for
/// `AcceptAny`: once a key is remembered, a later *different* key is rejected.
///
/// A **changed** fingerprint (`stored != presented`) is always `Reject`,
/// regardless of policy: a key that silently changes is a potential MITM and
/// must not re-prompt. The documented recovery path is an explicit user action
/// to *forget* the old entry (see [`KnownHostsStore::forget`]); only after that
/// does the host look unknown again and the normal prompt flow apply.
pub fn decide_trust(
    stored: Option<&str>,
    presented: &str,
    policy: HostKeyPolicy,
) -> TrustDecision {
    match stored {
        // Known host: trust iff the fingerprint is unchanged, otherwise a hard
        // reject (never a silent re-prompt).
        Some(known) if known == presented => TrustDecision::Proceed,
        Some(_changed) => TrustDecision::Reject,
        // Unknown host: the policy decides whether we may prompt at all.
        None => match policy {
            HostKeyPolicy::Strict => TrustDecision::Reject,
            HostKeyPolicy::PromptOnUnknown | HostKeyPolicy::AcceptAny => TrustDecision::Prompt,
        },
    }
}

/// Compute the `SHA256:<base64>` fingerprint of a DER-encoded X.509
/// certificate, matching the shape russh produces for SSH host keys.
///
/// Used for RDP: the TLS leaf certificate's DER bytes are hashed so the one
/// store and the one [`decide_trust`] rule handle RDP certificates and SSH host
/// keys uniformly. The base64 is unpadded, as in OpenSSH `SHA256:` fingerprints.
pub fn certificate_fingerprint(cert_der: &[u8]) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(cert_der);
    let b64 = base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest);
    format!("SHA256:{b64}")
}

/// A persistent store of accepted host-key / certificate fingerprints.
///
/// Backed by the `known_hosts` table (schema v3). It is deliberately tiny:
/// [`get`](Self::get) reads the accepted fingerprint for a `(host, port, kind)`
/// and [`remember`](Self::remember) upserts one. The verifier composes these
/// with the pure [`decide_trust`] rule and the prompt channel.
///
/// This owns its own [`Database`] handle (a second SQLite connection to the same
/// file as the profile store), because the profile store runs on a dedicated
/// actor thread and SQLite serializes per connection. Both connections migrate
/// the shared file to the current schema version idempotently.
pub struct KnownHostsStore {
    // `rusqlite::Connection` is `Send` but not `Sync`; the store is shared
    // (`Arc`) across the SSH/RDP verifiers and must be `Sync`, so the connection
    // lives behind a mutex. Reads/writes are single-row and brief, so the lock
    // is never meaningfully contended.
    db: std::sync::Mutex<Database>,
}

impl KnownHostsStore {
    /// Build a store over an already-opened (and migrated) [`Database`].
    pub fn new(db: Database) -> Self {
        Self {
            db: std::sync::Mutex::new(db),
        }
    }

    /// Lock the connection, mapping a poisoned lock to a storage error.
    fn with_conn<T>(
        &self,
        action: &str,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, DomainError> {
        let guard = self
            .db
            .lock()
            .map_err(|_| known_hosts_error(action, "known-hosts store lock was poisoned"))?;
        f(guard.connection()).map_err(|e| known_hosts_error(action, e))
    }

    /// The fingerprint previously accepted for `(host, port, kind)`, or `None`
    /// if the host is unknown for that kind.
    pub fn get(
        &self,
        host: &str,
        port: u16,
        kind: TrustKind,
    ) -> Result<Option<String>, DomainError> {
        self.with_conn("read a known-host entry", |conn| {
            conn.query_row(
                "SELECT fingerprint FROM known_hosts \
                 WHERE host = ?1 AND port = ?2 AND kind = ?3",
                rusqlite::params![host, port, kind.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
        })
    }

    /// Remember `fingerprint` as the accepted one for `(host, port, kind)`,
    /// replacing any previous entry for the same key.
    ///
    /// Replacing is intentional: a `remember` only ever happens after the user
    /// explicitly accepted a prompt (or `forget` cleared a changed key first),
    /// so the newest accepted fingerprint wins.
    pub fn remember(
        &self,
        host: &str,
        port: u16,
        kind: TrustKind,
        fingerprint: &str,
    ) -> Result<(), DomainError> {
        self.with_conn("record a known-host entry", |conn| {
            conn.execute(
                "INSERT INTO known_hosts (host, port, kind, fingerprint) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(host, port, kind) DO UPDATE SET fingerprint = excluded.fingerprint",
                rusqlite::params![host, port, kind.as_str(), fingerprint],
            )
            .map(|_| ())
        })
    }

    /// Forget the accepted entry for `(host, port, kind)`, if any.
    ///
    /// This is the documented recovery path for a **changed** key: once the old
    /// entry is forgotten the host looks unknown again, so the normal
    /// prompt/strict flow applies on the next connect. Idempotent — forgetting
    /// an absent entry is a no-op success. Wiring a user-facing "forget host
    /// key" command on top of this is left as a follow-up (see the IPC layer's
    /// `TODO`).
    pub fn forget(
        &self,
        host: &str,
        port: u16,
        kind: TrustKind,
    ) -> Result<(), DomainError> {
        self.with_conn("forget a known-host entry", |conn| {
            conn.execute(
                "DELETE FROM known_hosts WHERE host = ?1 AND port = ?2 AND kind = ?3",
                rusqlite::params![host, port, kind.as_str()],
            )
            .map(|_| ())
        })
    }
}

/// Build a [`DomainError`] for a known-hosts storage failure.
fn known_hosts_error(action: &str, cause: impl std::fmt::Display) -> DomainError {
    DomainError::new(
        "persistence.knownHosts",
        format!("Failed to {action}."),
    )
    .with_cause(cause.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Pure trust decision (the unit-testable heart) --------------------

    #[test]
    fn unknown_host_with_prompt_policy_prompts() {
        let d = decide_trust(None, "SHA256:abc", HostKeyPolicy::PromptOnUnknown);
        assert_eq!(d, TrustDecision::Prompt);
    }

    #[test]
    fn unknown_host_with_accept_any_prompts_then_remembers() {
        // AcceptAny still routes through Prompt (the verifier auto-accepts),
        // so even the insecure policy records a key and can detect a later change.
        let d = decide_trust(None, "SHA256:abc", HostKeyPolicy::AcceptAny);
        assert_eq!(d, TrustDecision::Prompt);
    }

    #[test]
    fn unknown_host_with_strict_policy_rejects_without_prompt() {
        let d = decide_trust(None, "SHA256:abc", HostKeyPolicy::Strict);
        assert_eq!(d, TrustDecision::Reject);
    }

    #[test]
    fn known_matching_fingerprint_proceeds_under_every_policy() {
        for policy in [
            HostKeyPolicy::PromptOnUnknown,
            HostKeyPolicy::AcceptAny,
            HostKeyPolicy::Strict,
        ] {
            let d = decide_trust(Some("SHA256:abc"), "SHA256:abc", policy);
            assert_eq!(d, TrustDecision::Proceed, "policy {policy:?}");
        }
    }

    #[test]
    fn changed_fingerprint_rejects_under_every_policy() {
        // A changed key is a hard fail regardless of policy — never a re-prompt.
        for policy in [
            HostKeyPolicy::PromptOnUnknown,
            HostKeyPolicy::AcceptAny,
            HostKeyPolicy::Strict,
        ] {
            let d = decide_trust(Some("SHA256:old"), "SHA256:new", policy);
            assert_eq!(d, TrustDecision::Reject, "policy {policy:?}");
        }
    }

    // --- Fingerprint helper -----------------------------------------------

    #[test]
    fn certificate_fingerprint_is_stable_and_prefixed() {
        let fp1 = certificate_fingerprint(b"der-bytes");
        let fp2 = certificate_fingerprint(b"der-bytes");
        assert_eq!(fp1, fp2, "same input must hash identically");
        assert!(fp1.starts_with("SHA256:"), "fingerprint is SHA256-prefixed");
        // A different certificate must yield a different fingerprint.
        assert_ne!(fp1, certificate_fingerprint(b"other-bytes"));
    }

    // --- Store round-trip -------------------------------------------------

    fn store() -> KnownHostsStore {
        KnownHostsStore::new(Database::open_in_memory().unwrap())
    }

    #[test]
    fn get_is_none_for_an_unknown_host() {
        let s = store();
        assert_eq!(
            s.get("example.com", 22, TrustKind::SshHostKey).unwrap(),
            None
        );
    }

    #[test]
    fn remember_then_get_round_trips_the_fingerprint() {
        let s = store();
        s.remember("example.com", 22, TrustKind::SshHostKey, "SHA256:abc")
            .unwrap();
        assert_eq!(
            s.get("example.com", 22, TrustKind::SshHostKey).unwrap(),
            Some("SHA256:abc".to_string())
        );
    }

    #[test]
    fn remember_replaces_a_prior_entry_for_the_same_key() {
        let s = store();
        s.remember("h", 22, TrustKind::SshHostKey, "SHA256:one")
            .unwrap();
        s.remember("h", 22, TrustKind::SshHostKey, "SHA256:two")
            .unwrap();
        assert_eq!(
            s.get("h", 22, TrustKind::SshHostKey).unwrap(),
            Some("SHA256:two".to_string())
        );
    }

    #[test]
    fn ssh_and_rdp_entries_are_namespaced_by_kind() {
        let s = store();
        s.remember("h", 3389, TrustKind::SshHostKey, "SHA256:ssh")
            .unwrap();
        s.remember("h", 3389, TrustKind::RdpCertificate, "SHA256:rdp")
            .unwrap();
        // Same host:port, different kind → independent entries.
        assert_eq!(
            s.get("h", 3389, TrustKind::SshHostKey).unwrap(),
            Some("SHA256:ssh".to_string())
        );
        assert_eq!(
            s.get("h", 3389, TrustKind::RdpCertificate).unwrap(),
            Some("SHA256:rdp".to_string())
        );
    }

    #[test]
    fn entries_are_namespaced_by_port() {
        let s = store();
        s.remember("h", 22, TrustKind::SshHostKey, "SHA256:a")
            .unwrap();
        assert_eq!(s.get("h", 2222, TrustKind::SshHostKey).unwrap(), None);
    }

    #[test]
    fn forget_clears_an_entry_and_is_idempotent() {
        let s = store();
        s.remember("h", 22, TrustKind::SshHostKey, "SHA256:a")
            .unwrap();
        s.forget("h", 22, TrustKind::SshHostKey).unwrap();
        assert_eq!(s.get("h", 22, TrustKind::SshHostKey).unwrap(), None);
        // Forgetting again is a no-op success.
        s.forget("h", 22, TrustKind::SshHostKey).unwrap();
    }

    #[test]
    fn full_flow_unknown_remember_then_changed_is_rejected() {
        // Models the verifier's composition: unknown → prompt → remember →
        // same key proceeds → a changed key is rejected.
        let s = store();
        let policy = HostKeyPolicy::PromptOnUnknown;

        let stored = s.get("h", 22, TrustKind::SshHostKey).unwrap();
        assert_eq!(decide_trust(stored.as_deref(), "SHA256:k1", policy), TrustDecision::Prompt);

        s.remember("h", 22, TrustKind::SshHostKey, "SHA256:k1").unwrap();
        let stored = s.get("h", 22, TrustKind::SshHostKey).unwrap();
        assert_eq!(decide_trust(stored.as_deref(), "SHA256:k1", policy), TrustDecision::Proceed);
        assert_eq!(decide_trust(stored.as_deref(), "SHA256:k2", policy), TrustDecision::Reject);
    }
}
