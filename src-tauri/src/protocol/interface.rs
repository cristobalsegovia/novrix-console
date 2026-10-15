//! The uniform `Protocol` / `SessionConn` traits (Requirement 8.1).
//!
//! These two traits are the whole of the "uniform interface": they define the
//! operations to establish, maintain, and close a Session, and they apply
//! unchanged to SSH, Telnet, Serial, and RDP. A new protocol is added by
//! implementing them and registering it — no change to this file is required
//! (Requirement 8.2).

use async_trait::async_trait;

use crate::domain::{ConnectionProfile, CredentialKey, ProtocolError, ProtocolKind, SessionState};

/// The names of the operations the uniform interface requires every protocol
/// to provide, in the order the registry reports them.
///
/// A protocol that cannot provide one of these is incomplete and is rejected
/// by [`ProtocolRegistry::register`](super::ProtocolRegistry::register), which
/// names the missing operations (Requirement 8.4). Keeping the list in one
/// place lets the registry and the completeness check agree on the exact
/// spelling the error message uses.
pub const UNIFORM_OPERATIONS: &[&str] =
    &["establish", "send", "recv", "resize", "close", "state"];

/// A connection profile whose credential references have been resolved to the
/// concrete handles the protocol needs to connect.
///
/// The design refers to this as `ResolvedProfile`. The resolution step (reading
/// secrets out of the Credential_Store, validating bounds) belongs to the
/// Connection_Manager and the per-protocol tasks (8–11); here it is a thin,
/// faithful carrier so the uniform [`Protocol::establish`] signature matches the
/// design. Later tasks may enrich it without changing the trait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    /// The validated profile the session is being opened from.
    pub profile: ConnectionProfile,
}

impl ResolvedProfile {
    /// Wrap an already-validated [`ConnectionProfile`].
    pub fn new(profile: ConnectionProfile) -> Self {
        Self { profile }
    }

    /// The protocol this profile connects with.
    pub fn kind(&self) -> ProtocolKind {
        self.profile.protocol
    }
}

/// An opaque handle to the secret material a protocol needs to authenticate.
///
/// The design refers to this as `CredentialHandle`. It intentionally carries a
/// *reference* ([`CredentialKey`]) rather than plaintext, matching the rule
/// that secrets are resolved only through the Credential_Store and never held
/// in the data model. `None` means the profile authenticates without a secret
/// (e.g. plain Telnet / open serial). The concrete secret-resolution wiring is
/// a later task.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CredentialHandle {
    /// The Secure_Storage key naming the secret, or `None` when none is needed.
    pub credential_key: Option<CredentialKey>,
}

impl CredentialHandle {
    /// A handle that references a stored secret by key.
    pub fn with_key(credential_key: CredentialKey) -> Self {
        Self {
            credential_key: Some(credential_key),
        }
    }

    /// A handle for protocols that need no secret.
    pub fn none() -> Self {
        Self {
            credential_key: None,
        }
    }
}

/// A live, established session to a remote target.
///
/// Every protocol exposes the same five operations through this trait; the
/// Session_Manager, Terminal_Emulator, Script_Engine, and logging all drive a
/// session purely through it, with no knowledge of the underlying protocol
/// (Requirement 8.1).
///
/// Implementations own the transport resources (sockets, handles, tasks) and
/// release them on [`close`](SessionConn::close) or on drop.
#[async_trait]
pub trait SessionConn: Send {
    /// Send raw bytes (keystrokes / script commands) to the remote.
    async fn send(&mut self, data: &[u8]) -> Result<(), ProtocolError>;

    /// Receive the next chunk of output. `Ok(None)` signals a clean EOF.
    async fn recv(&mut self) -> Result<Option<Vec<u8>>, ProtocolError>;

    /// Notify the remote of a new terminal size.
    ///
    /// A no-op for RDP, which is driven by resolution rather than a character
    /// grid; such protocols return `Ok(())` without doing anything.
    async fn resize(&mut self, cols: u16, rows: u16) -> Result<(), ProtocolError>;

    /// Close the session and release all resources. Idempotent: calling it on
    /// an already-closed session succeeds without error.
    async fn close(&mut self) -> Result<(), ProtocolError>;

    /// The current connection state: `Connecting`, `Connected`, or
    /// `Disconnected`.
    fn state(&self) -> SessionState;
}

/// The uniform interface a Supported_Protocol implements to be registrable.
///
/// A `Protocol` is a factory: it names its [`kind`](Protocol::kind) and knows
/// how to [`establish`](Protocol::establish) a [`SessionConn`]. One registered
/// `Protocol` instance corresponds to one selectable [`ProtocolKind`]
/// (Requirement 8.2).
#[async_trait]
pub trait Protocol: Send + Sync {
    /// The protocol kind this implementation handles. The registry uses this to
    /// decide which kind becomes selectable once registration succeeds.
    fn kind(&self) -> ProtocolKind;

    /// The uniform-interface operations this protocol actually implements.
    ///
    /// A complete protocol implements all of [`UNIFORM_OPERATIONS`], which is
    /// the default. The registry compares this against the required set and
    /// rejects any protocol that omits one, naming what is missing
    /// (Requirement 8.4). A protocol under construction can override this to
    /// advertise only the operations it has wired up so far; such a protocol is
    /// refused and excluded from the selectable options until it is complete.
    fn supported_operations(&self) -> &[&'static str] {
        UNIFORM_OPERATIONS
    }

    /// Establish a live session from a resolved profile and its credentials.
    ///
    /// On success, returns a ready [`SessionConn`]. On failure, the
    /// implementation **must** have released every resource it allocated during
    /// the attempt (sockets, OS handles, spawned tasks) before returning `Err`,
    /// so no half-open connection is left behind (Requirement 8.3). The
    /// idiomatic way to guarantee this is RAII: hold each resource in a guard
    /// whose `Drop` tears it down, and only hand ownership to the returned
    /// `SessionConn` on the success path.
    async fn establish(
        &self,
        cfg: &ResolvedProfile,
        creds: CredentialHandle,
    ) -> Result<Box<dyn SessionConn>, ProtocolError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AuthConfig, ProfileId, ProtocolParams};

    fn sample_profile(kind: ProtocolKind) -> ConnectionProfile {
        ConnectionProfile {
            id: ProfileId::new(),
            name: "sample".to_string(),
            protocol: kind,
            host: "example.com".to_string(),
            port: 23,
            username: None,
            domain: None,
            folder_id: None,
            auth: AuthConfig::None,
            protocol_params: ProtocolParams::Telnet {},
            logging_enabled: false,
            log_path: None,
            connect_timeout_secs: 30,
        }
    }

    #[test]
    fn uniform_operations_cover_the_whole_interface() {
        // The two trait definitions above expose exactly these operations;
        // the list the registry reports must stay in lock-step with them.
        assert_eq!(
            UNIFORM_OPERATIONS,
            &["establish", "send", "recv", "resize", "close", "state"]
        );
    }

    #[test]
    fn resolved_profile_reports_its_kind() {
        let resolved = ResolvedProfile::new(sample_profile(ProtocolKind::Telnet));
        assert_eq!(resolved.kind(), ProtocolKind::Telnet);
    }

    #[test]
    fn credential_handle_distinguishes_secret_from_none() {
        assert!(CredentialHandle::none().credential_key.is_none());
        let handle = CredentialHandle::with_key(CredentialKey::new("profile/1/password"));
        assert_eq!(
            handle.credential_key.as_ref().map(|k| k.as_str()),
            Some("profile/1/password")
        );
    }

    /// Property-based test for the resource-safety contract of a failed
    /// [`Protocol::establish`] (task 7.3).
    ///
    /// `establish` is async, so each proptest case drives a single-threaded
    /// Tokio runtime, mirroring the Credential_Store property tests. The mock
    /// protocol models each allocated Session resource as a guard whose `Drop`
    /// increments a shared freed-counter (RAII), so "all resources released"
    /// becomes the checkable equality `allocated == freed` once `establish`
    /// returns `Err`.
    mod properties {
        use super::*;
        use crate::domain::{AuthConfig, ProfileId, ProtocolError, ProtocolParams};
        use proptest::prelude::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::runtime::Runtime;

        /// A fresh current-thread runtime for driving one property case.
        fn runtime() -> Runtime {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("build tokio runtime")
        }

        /// An RAII guard standing in for one allocated Session resource (socket,
        /// OS handle, spawned task). Allocating it bumps `allocated`; dropping
        /// it bumps `freed`. A correct `establish` holds every guard until it
        /// either hands them to the returned `SessionConn` (success) or lets
        /// them drop (failure), so on the `Err` path `allocated == freed`.
        struct ResourceGuard {
            freed: Arc<AtomicUsize>,
        }

        impl ResourceGuard {
            fn allocate(allocated: &Arc<AtomicUsize>, freed: Arc<AtomicUsize>) -> Self {
                allocated.fetch_add(1, Ordering::SeqCst);
                Self { freed }
            }
        }

        impl Drop for ResourceGuard {
            fn drop(&mut self) {
                self.freed.fetch_add(1, Ordering::SeqCst);
            }
        }

        /// A mock protocol whose `establish` allocates `n` RAII-guarded
        /// resources and then fails, returning a specified error. Because the
        /// guards are locals, unwinding the error path drops every one of them.
        struct FailingEstablishProtocol {
            kind: ProtocolKind,
            resource_count: usize,
            error: ProtocolError,
            allocated: Arc<AtomicUsize>,
            freed: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl Protocol for FailingEstablishProtocol {
            fn kind(&self) -> ProtocolKind {
                self.kind
            }

            async fn establish(
                &self,
                _cfg: &ResolvedProfile,
                _creds: CredentialHandle,
            ) -> Result<Box<dyn SessionConn>, ProtocolError> {
                // Allocate resources under RAII guards, exactly as a real
                // protocol would while opening a connection.
                let mut guards = Vec::with_capacity(self.resource_count);
                for _ in 0..self.resource_count {
                    guards.push(ResourceGuard::allocate(
                        &self.allocated,
                        Arc::clone(&self.freed),
                    ));
                }

                // The attempt fails. `guards` is dropped here on the error path,
                // tearing down every resource before `Err` is returned — no
                // half-open connection is handed back.
                Err(self.error.clone())
            }
        }

        fn sample_profile(kind: ProtocolKind) -> ConnectionProfile {
            ConnectionProfile {
                id: ProfileId::new(),
                name: "sample".to_string(),
                protocol: kind,
                host: "example.com".to_string(),
                port: 22,
                username: None,
                domain: None,
                folder_id: None,
                auth: AuthConfig::None,
                protocol_params: ProtocolParams::Telnet {},
                logging_enabled: false,
                log_path: None,
                connect_timeout_secs: 30,
            }
        }

        /// A strategy over the error causes a failed establish can report, so
        /// the property holds regardless of *why* the attempt failed.
        fn failure_cause() -> impl Strategy<Value = ProtocolError> {
            prop_oneof![
                Just(ProtocolError::Timeout),
                Just(ProtocolError::ConnectionRefused),
                Just(ProtocolError::AuthenticationFailed),
                Just(ProtocolError::MissingCredentials),
                Just(ProtocolError::HostKeyDeclined),
                ".{0,24}".prop_map(ProtocolError::HostUnresolved),
                ".{0,24}".prop_map(ProtocolError::Io),
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            // Feature: remote-connection-manager, Property 20: Failed protocol establishment releases all resources
            //
            // *For any* protocol whose establishment fails, all allocated
            // Session resources are released (no leak) and an error indication
            // identifying the cause is returned to the Connection_Manager.
            //
            // Validates: Requirements 8.3
            #[test]
            fn failed_establish_releases_all_resources(
                resource_count in 0usize..16,
                error in failure_cause(),
            ) {
                let allocated = Arc::new(AtomicUsize::new(0));
                let freed = Arc::new(AtomicUsize::new(0));

                let protocol = FailingEstablishProtocol {
                    kind: ProtocolKind::Ssh,
                    resource_count,
                    error: error.clone(),
                    allocated: Arc::clone(&allocated),
                    freed: Arc::clone(&freed),
                };

                let cfg = ResolvedProfile::new(sample_profile(ProtocolKind::Ssh));

                let result = runtime().block_on(async {
                    protocol.establish(&cfg, CredentialHandle::none()).await
                });

                // An error indication identifying the cause is returned...
                // (`Box<dyn SessionConn>` is not `Debug`, so match rather than
                // `expect_err`.)
                match result {
                    Ok(_) => prop_assert!(false, "a failed establish must return Err"),
                    Err(returned) => prop_assert_eq!(returned, error.clone()),
                }

                // ...the resources it allocated were in fact allocated...
                prop_assert_eq!(allocated.load(Ordering::SeqCst), resource_count);

                // ...and every allocated resource was released: no leak.
                prop_assert_eq!(freed.load(Ordering::SeqCst), resource_count);
                prop_assert_eq!(
                    allocated.load(Ordering::SeqCst),
                    freed.load(Ordering::SeqCst)
                );
            }
        }
    }
}
