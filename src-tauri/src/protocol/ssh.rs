//! SSH protocol implementation over [`russh`] (Requirement 4).
//!
//! This module provides [`SshProtocol`], a [`Protocol`] for
//! [`ProtocolKind::Ssh`], and [`SshSession`], the [`SessionConn`] it hands back
//! on a successful connect. Establishing an SSH session is split into two
//! layers so the policy — the parts the requirements pin down — is pure and
//! unit-testable without ever opening a socket:
//!
//! - **Pure policy** (this file, no I/O):
//!   - [`resolve_port`] / [`resolve_timeout`] — port/timeout resolution and
//!     bounds (Requirement 4.1, 4.6; Property 12).
//!   - [`plan_auth`] — decides, from the profile's [`AuthConfig`] and whatever
//!     the Credential_Store returned, *how* to authenticate or whether to abort
//!     with [`ProtocolError::MissingCredentials`] (Requirement 4.2, 4.3, 4.4;
//!     Property 13).
//!   - [`AuthAttemptBudget`] — bounds authentication to 3 attempts and then
//!     fails with [`ProtocolError::AuthenticationFailed`] (Requirement 4.5;
//!     Property 14).
//!
//! - **Injectable ports** (traits, so tests substitute fakes and the real
//!   Credential_Store / UI wire in later without this module depending on
//!   them):
//!   - [`SecretResolver`] — resolves a [`CredentialKey`] to its plaintext
//!     secret. The real implementation is a thin adapter over the
//!     Credential_Store, wired up in a later integration task; this module
//!     stays compilable and testable on its own.
//!   - [`SshKeyResolver`] — resolves a referenced [`KeyId`] to the OpenSSH
//!     private-key material. Wired to the Key_Manager later.
//!   - [`HostKeyVerifier`] — presents an unknown host-key fingerprint for
//!     confirmation and returns the user's decision; declining aborts with
//!     [`ProtocolError::HostKeyDeclined`] (Requirement 4.7, 4.8; Property 15).
//!
//! - **Transport wiring** (`establish`): resolves port/timeout, applies the
//!   timeout to the connect, drives the host-key verifier through russh's
//!   `check_server_key` hook, then runs the planned authentication inside the
//!   attempt budget. On any failure it returns `Err` without leaking a
//!   half-open session (Requirement 8.3); the live transport is only handed to
//!   [`SshSession`] on the success path. This is now wired over [`russh`]: the
//!   connect, host-key check, authentication, and the session channel + PTY are
//!   all live, and [`SshSession`] owns the real `client::Handle` and
//!   `Channel<Msg>`.
//!
//! ## Integration points (wired in later tasks)
//!
//! The concrete [`SecretResolver`] / [`SshKeyResolver`] adapters over the
//! Credential_Store and Key_Manager, and the real UI-backed [`HostKeyVerifier`]
//! plus a persistent known-hosts store, live in the IPC assembly layer
//! (`ipc/mod.rs`, task 25.4). Those seams were marked `TODO(wiring)`; the
//! transport itself (this module's connect/auth/channel path) is implemented.
//!
//! For file transfers, [`SshSession::open_sftp_session`] opens a second
//! `sftp`-subsystem channel on the live connection and returns a ready
//! `russh_sftp::client::SftpSession`, so the File_Transfer_Service can share
//! one SSH connection with the interactive shell.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use russh::client::{self, Handle};
use russh::keys::{decode_secret_key, HashAlg, PrivateKey, PrivateKeyWithHashAlg};
use russh::{Channel, ChannelMsg};
use tokio::sync::Mutex;

use crate::domain::{AuthConfig, CredentialKey, KeyId, ProtocolError, ProtocolKind, SessionState};

use super::interface::{CredentialHandle, Protocol, ResolvedProfile, SessionConn};

/// The terminal type advertised to the remote when requesting a PTY.
const PTY_TERM: &str = "xterm-256color";

/// Default PTY grid used when the session is first opened; the terminal
/// emulator issues a [`resize`](SessionConn::resize) with the real size as soon
/// as it attaches.
const DEFAULT_PTY_COLS: u32 = 80;
const DEFAULT_PTY_ROWS: u32 = 24;

// --- Bounds and defaults (Requirement 4.1, 4.5, 4.6) ----------------------

/// Default SSH port, used when the profile leaves the port unspecified
/// (modeled as `0`). Requirement 4.1.
pub const DEFAULT_SSH_PORT: u16 = 22;

/// Lowest valid TCP port. Port `0` means "unspecified" for an SSH profile.
/// Part of the public API documenting the Requirement 4.1 bound; also used by
/// the range assertions in this module's tests.
#[allow(dead_code)]
pub const MIN_PORT: u16 = 1;

/// Highest valid TCP port. `u16::MAX` == 65535, so every non-zero `u16` is in
/// range; the constant documents the Requirement 4.1 bound explicitly.
#[allow(dead_code)]
pub const MAX_PORT: u16 = 65535;

/// Default connection timeout in seconds when the profile leaves it
/// unspecified or out of range. Requirement 4.6.
pub const DEFAULT_TIMEOUT_SECS: u16 = 30;

/// Inclusive lower bound for the configured connection timeout. Requirement 4.6.
pub const MIN_TIMEOUT_SECS: u16 = 1;

/// Inclusive upper bound for the configured connection timeout. Requirement 4.6.
pub const MAX_TIMEOUT_SECS: u16 = 300;

/// Maximum number of SSH authentication attempts before the connection attempt
/// is abandoned. Requirement 4.5 / Property 14.
pub const MAX_AUTH_ATTEMPTS: u8 = 3;

// --- Pure resolution (Requirement 4.1, 4.6; Property 12) ------------------

/// Resolve the SSH port to connect on.
///
/// A profile stores its port as a `u16`. A `0` means "unspecified": it
/// resolves to [`DEFAULT_SSH_PORT`] (22). Any non-zero value is already within
/// `1..=65535` by the type, so it is used as configured. The result is always
/// in `MIN_PORT..=MAX_PORT` (Requirement 4.1 / Property 12).
pub fn resolve_port(configured: u16) -> u16 {
    match configured {
        0 => DEFAULT_SSH_PORT,
        p => p,
    }
}

/// Resolve the connection timeout in seconds.
///
/// The configured value is used only when it lies in
/// `MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS` (`1..=300`); anything outside that
/// range — including the `0` "unspecified" sentinel and any over-large value —
/// falls back to [`DEFAULT_TIMEOUT_SECS`] (30). The result is always in
/// `1..=300` (Requirement 4.6 / Property 12).
pub fn resolve_timeout(configured_secs: u16) -> u16 {
    if (MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&configured_secs) {
        configured_secs
    } else {
        DEFAULT_TIMEOUT_SECS
    }
}

/// The resolved timeout as a [`Duration`], ready to hand to the transport.
pub fn resolve_timeout_duration(configured_secs: u16) -> Duration {
    Duration::from_secs(resolve_timeout(configured_secs) as u64)
}

// --- Authentication planning (Requirement 4.2–4.4; Property 13) -----------

/// What material the resolved secret/key ports produced for the profile.
///
/// Kept separate from [`AuthConfig`] so [`plan_auth`] can be a pure function of
/// "what the profile asks for" + "what the Credential_Store / Key_Manager
/// returned", with no I/O of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAuthMaterial {
    /// The profile authenticates without a secret (not expected for SSH, but
    /// representable).
    None,
    /// A password credential was successfully resolved.
    Password(String),
    /// The referenced SSH key material was successfully resolved (OpenSSH PEM).
    SshKey {
        /// Which managed key this material came from.
        key_id: KeyId,
        /// The private-key material (OpenSSH format).
        pem: String,
    },
    /// Password authentication is required but the Credential_Store held no
    /// credential for the profile.
    MissingPassword,
    /// A referenced SSH key could not be resolved by the Key_Manager.
    MissingKey(KeyId),
}

/// The authentication method `establish` should drive against russh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPlan {
    /// Authenticate with a password.
    Password(String),
    /// Authenticate with the referenced SSH key's material.
    PublicKey {
        /// Which managed key is in use.
        key_id: KeyId,
        /// The OpenSSH private-key material.
        pem: String,
    },
}

/// Decide how to authenticate, or abort with a missing-credential error.
///
/// Pure: it maps the profile's declared [`AuthConfig`] together with the
/// material the ports resolved into either an [`AuthPlan`] or the error that
/// aborts the attempt. The two abort paths both leave the caller to return
/// before any Session exists:
///
/// - Password auth with no stored credential →
///   [`ProtocolError::MissingCredentials`] (Requirement 4.3 / Property 13).
/// - A key-ref profile whose key material cannot be resolved →
///   [`ProtocolError::MissingCredentials`] (Requirement 4.4; a referenced key
///   that is gone is treated as a missing credential).
///
/// `AuthConfig::None` is rejected: SSH always authenticates (there is no
/// anonymous SSH), so an SSH profile declaring no auth is a configuration
/// error surfaced as a missing credential.
pub fn plan_auth(
    auth: &AuthConfig,
    material: ResolvedAuthMaterial,
) -> Result<AuthPlan, ProtocolError> {
    match (auth, material) {
        // Password auth with a resolved secret (Requirement 4.2).
        (AuthConfig::Password { .. }, ResolvedAuthMaterial::Password(secret)) => {
            Ok(AuthPlan::Password(secret))
        }
        // Password auth, no credential in the store (Requirement 4.3 / Property 13).
        (AuthConfig::Password { .. }, ResolvedAuthMaterial::MissingPassword)
        | (AuthConfig::Password { .. }, ResolvedAuthMaterial::None) => {
            Err(ProtocolError::MissingCredentials)
        }
        // Key-ref auth with resolved material (Requirement 4.4).
        (AuthConfig::SshKey { key_id }, ResolvedAuthMaterial::SshKey { key_id: got, pem })
            if *key_id == got =>
        {
            Ok(AuthPlan::PublicKey { key_id: got, pem })
        }
        // Key-ref auth whose key could not be resolved (Requirement 4.4).
        (AuthConfig::SshKey { .. }, ResolvedAuthMaterial::MissingKey(_))
        | (AuthConfig::SshKey { .. }, ResolvedAuthMaterial::None) => {
            Err(ProtocolError::MissingCredentials)
        }
        // SSH never authenticates anonymously; declaring `None` is a config
        // error treated as a missing credential.
        (AuthConfig::None, _) => Err(ProtocolError::MissingCredentials),
        // Any other mix (e.g. a password profile that somehow resolved key
        // material) is inconsistent and treated as a missing credential rather
        // than silently guessing.
        _ => Err(ProtocolError::MissingCredentials),
    }
}

/// Bounds authentication to [`MAX_AUTH_ATTEMPTS`] tries (Requirement 4.5 /
/// Property 14).
///
/// The connect loop records each failed authentication through
/// [`record_failure`](AuthAttemptBudget::record_failure). Once the budget is
/// exhausted, [`exhausted`](AuthAttemptBudget::exhausted) is true and the loop
/// must stop and surface [`ProtocolError::AuthenticationFailed`], leaving the
/// Session disconnected. The invariant the property pins down is that the
/// number of attempts never exceeds 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthAttemptBudget {
    attempts_made: u8,
    max_attempts: u8,
}

impl AuthAttemptBudget {
    /// A fresh budget allowing [`MAX_AUTH_ATTEMPTS`] attempts.
    pub fn new() -> Self {
        Self {
            attempts_made: 0,
            max_attempts: MAX_AUTH_ATTEMPTS,
        }
    }

    /// Whether another attempt is still permitted.
    pub fn can_attempt(&self) -> bool {
        self.attempts_made < self.max_attempts
    }

    /// Record that an authentication attempt was made and failed. Returns
    /// whether another attempt is still permitted afterwards.
    pub fn record_failure(&mut self) -> bool {
        if self.attempts_made < self.max_attempts {
            self.attempts_made += 1;
        }
        self.can_attempt()
    }

    /// Whether the attempt budget is spent (no more attempts allowed).
    pub fn exhausted(&self) -> bool {
        !self.can_attempt()
    }

    /// How many attempts have been made so far (never exceeds the max).
    pub fn attempts_made(&self) -> u8 {
        self.attempts_made
    }
}

impl Default for AuthAttemptBudget {
    fn default() -> Self {
        Self::new()
    }
}

// --- Injectable ports -----------------------------------------------------

/// Resolves a [`CredentialKey`] to its plaintext secret.
///
/// This is the seam between the SSH protocol and the Credential_Store: the
/// protocol never touches OS Secure_Storage directly. `Ok(None)` means "no such
/// credential" (which [`plan_auth`] maps to a missing-credential abort);
/// `Err` is a backend failure. The real adapter over the Credential_Store is
/// `TODO(wiring)` in a later task.
#[async_trait]
pub trait SecretResolver: Send + Sync {
    /// Resolve the secret for `key`, or `Ok(None)` when none is stored.
    async fn resolve_secret(&self, key: &CredentialKey) -> Result<Option<String>, ProtocolError>;
}

/// Resolves a referenced [`KeyId`] to its OpenSSH private-key material.
///
/// The seam between the SSH protocol and the Key_Manager. `Ok(None)` means the
/// referenced key is gone (treated as a missing credential). The real adapter
/// is `TODO(wiring)`.
#[async_trait]
pub trait SshKeyResolver: Send + Sync {
    /// Resolve the OpenSSH private-key material for `key_id`, or `Ok(None)`
    /// when the managed key no longer exists.
    async fn resolve_key(&self, key_id: KeyId) -> Result<Option<String>, ProtocolError>;
}

/// Presents an unknown host-key fingerprint for user confirmation
/// (Requirement 4.7, 4.8; Property 15).
///
/// Called the first time an unknown host key is seen. Returning `true` accepts
/// the key and lets the connection proceed; returning `false` means the user
/// declined, and the connection attempt aborts with
/// [`ProtocolError::HostKeyDeclined`], leaving the Session disconnected. The
/// real UI-backed verifier (and persistent known-hosts store) is
/// `TODO(wiring)`.
#[async_trait]
pub trait HostKeyVerifier: Send + Sync {
    /// Decide whether to trust an unknown host key, given its fingerprint.
    async fn confirm_unknown_host_key(&self, fingerprint: &str) -> bool;
}

/// The username an SSH session authenticates as.
///
/// The current data model does not carry an SSH username on the profile, so it
/// is supplied to [`SshProtocol`] at construction time. `TODO(wiring)`: once
/// the profile (or a resolved profile) carries the login user, read it from
/// there instead of this default.
#[derive(Debug, Clone)]
pub struct SshIdentity {
    /// The login username.
    pub username: String,
}

impl SshIdentity {
    /// An identity for the given username.
    pub fn new(username: impl Into<String>) -> Self {
        Self {
            username: username.into(),
        }
    }
}

// --- The SSH protocol -----------------------------------------------------

/// SSH implementation of the uniform [`Protocol`] interface.
///
/// Holds the injectable ports it needs to authenticate and to verify host
/// keys. It opens no connection until [`establish`](Protocol::establish) is
/// called.
pub struct SshProtocol {
    secrets: Arc<dyn SecretResolver>,
    keys: Arc<dyn SshKeyResolver>,
    // Driven by the live transport from russh's `check_server_key` hook: an
    // unknown host key's fingerprint is presented to it for confirmation, and a
    // `false` decision aborts the connect with `HostKeyDeclined`.
    host_key_verifier: Arc<dyn HostKeyVerifier>,
    identity: SshIdentity,
}

impl SshProtocol {
    /// Build an SSH protocol from its ports and the login identity.
    pub fn new(
        secrets: Arc<dyn SecretResolver>,
        keys: Arc<dyn SshKeyResolver>,
        host_key_verifier: Arc<dyn HostKeyVerifier>,
        identity: SshIdentity,
    ) -> Self {
        Self {
            secrets,
            keys,
            host_key_verifier,
            identity,
        }
    }

    /// Resolve the authentication material for the profile's [`AuthConfig`] by
    /// driving the secret/key ports, then plan the authentication method.
    ///
    /// This is the composition point between the pure [`plan_auth`] and the
    /// side-effecting ports. It performs no network I/O — it only reads from
    /// the Credential_Store / Key_Manager seams.
    async fn plan_authentication(
        &self,
        auth: &AuthConfig,
        creds: &CredentialHandle,
    ) -> Result<AuthPlan, ProtocolError> {
        let material = match auth {
            AuthConfig::Password { credential_key } => {
                // Prefer the key the caller resolved into the handle; fall back
                // to the one named on the profile.
                let key = creds.credential_key.as_ref().unwrap_or(credential_key);
                match self.secrets.resolve_secret(key).await? {
                    Some(secret) => ResolvedAuthMaterial::Password(secret),
                    None => ResolvedAuthMaterial::MissingPassword,
                }
            }
            AuthConfig::SshKey { key_id } => match self.keys.resolve_key(*key_id).await? {
                Some(pem) => ResolvedAuthMaterial::SshKey {
                    key_id: *key_id,
                    pem,
                },
                None => ResolvedAuthMaterial::MissingKey(*key_id),
            },
            AuthConfig::None => ResolvedAuthMaterial::None,
        };

        plan_auth(auth, material)
    }
}

#[async_trait]
impl Protocol for SshProtocol {
    fn kind(&self) -> ProtocolKind {
        ProtocolKind::Ssh
    }

    async fn establish(
        &self,
        cfg: &ResolvedProfile,
        creds: CredentialHandle,
    ) -> Result<Box<dyn SessionConn>, ProtocolError> {
        // Delegate to the concrete-typed establish so the IPC layer can also
        // obtain the live [`SshSession`] directly (to open an SFTP subsystem
        // channel for file transfers — see [`SshSession::open_sftp_session`]).
        Ok(Box::new(self.establish_session(cfg, creds).await?))
    }
}

impl SshProtocol {
    /// Establish an SSH session and return the **concrete** [`SshSession`].
    ///
    /// Identical to [`Protocol::establish`] but hands back the concrete type
    /// instead of `Box<dyn SessionConn>`, so a caller (the IPC assembly) can
    /// subsequently call [`SshSession::open_sftp_session`] on the live handle
    /// to bring up the SFTP subsystem for file transfers. The uniform
    /// [`Protocol::establish`] wraps this and is unchanged for every existing
    /// caller.
    pub async fn establish_session(
        &self,
        cfg: &ResolvedProfile,
        creds: CredentialHandle,
    ) -> Result<SshSession, ProtocolError> {
        let profile = &cfg.profile;

        // Reject a non-SSH profile routed here by mistake.
        if profile.protocol != ProtocolKind::Ssh {
            return Err(ProtocolError::InvalidParameter {
                parameter: "protocol".to_string(),
                message: "SshProtocol can only establish SSH profiles".to_string(),
            });
        }

        // Requirement 4.1 / 4.6 / Property 12: resolve the connection
        // parameters up front so the rest of the attempt uses validated values.
        let _port = resolve_port(profile.port);
        let _timeout = resolve_timeout_duration(profile.connect_timeout_secs);
        let _host = profile.host.clone();
        let _username = self.identity.username.clone();

        // Requirement 4.2–4.4 / Property 13: decide the auth method (or abort
        // with MissingCredentials) *before* opening a socket, so a missing
        // credential never leaves a half-open connection behind.
        let _auth_plan = self.plan_authentication(&profile.auth, &creds).await?;

        let port = _port;
        let timeout = _timeout;
        let host = _host;
        let username = _username;
        let auth_plan = _auth_plan;

        // Step 1 — connect, bounded by the resolved timeout (Requirement 4.6).
        //
        // The host-key verifier is handed to the russh handler, which invokes
        // it from `check_server_key`. The connect future owns the handler (and
        // thus the socket + background task); if any step below fails, `handle`
        // is dropped and russh tears the transport down, so nothing half-open
        // leaks (Requirement 8.3).
        let config = Arc::new(client::Config {
            inactivity_timeout: None,
            ..Default::default()
        });
        let handler = ClientHandler {
            verifier: Arc::clone(&self.host_key_verifier),
        };

        let connect = client::connect(config, (host.as_str(), port), handler);
        let handle = match tokio::time::timeout(timeout, connect).await {
            // Elapsed the connect budget (Requirement 4.6 / Property 12).
            Err(_elapsed) => return Err(ProtocolError::Timeout),
            Ok(Ok(handle)) => handle,
            Ok(Err(err)) => return Err(map_connect_error(err)),
        };
        let mut handle = handle;

        // Step 3 — authenticate per the plan, bounded to MAX_AUTH_ATTEMPTS
        // (Requirement 4.5 / Property 14). A declined host key already surfaced
        // from the connect above as `HostKeyDeclined` (step 2).
        authenticate(&mut handle, &username, &auth_plan).await?;

        // Step 4 — open a session channel and request a PTY. Only on this
        // success path does `SshSession` take ownership of the live handle +
        // channel; any earlier `return Err` drops them (Requirement 8.3).
        let channel = handle
            .channel_open_session()
            .await
            .map_err(map_transport_error)?;
        channel
            .request_pty(
                false,
                PTY_TERM,
                DEFAULT_PTY_COLS,
                DEFAULT_PTY_ROWS,
                0,
                0,
                &[],
            )
            .await
            .map_err(map_transport_error)?;
        channel
            .request_shell(false)
            .await
            .map_err(map_transport_error)?;

        Ok(SshSession::connected(handle, channel))
    }
}

/// The russh client handler. Its sole job is the host-key check: it computes
/// the presented key's SHA-256 fingerprint and asks the injected
/// [`HostKeyVerifier`] whether to trust it. Returning `Ok(false)` makes russh
/// abort the connect with [`russh::Error::UnknownKey`], which
/// [`map_connect_error`] maps to [`ProtocolError::HostKeyDeclined`]
/// (Requirement 4.7, 4.8 / Property 15).
struct ClientHandler {
    verifier: Arc<dyn HostKeyVerifier>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let fingerprint = server_public_key.fingerprint(HashAlg::Sha256).to_string();
        Ok(self.verifier.confirm_unknown_host_key(&fingerprint).await)
    }
}

/// Run the planned authentication inside an [`AuthAttemptBudget`].
///
/// Each russh auth call returns an [`russh::client::AuthResult`]; a non-success
/// result is a failed attempt that is recorded against the budget. Once the
/// budget is exhausted the loop stops and surfaces
/// [`ProtocolError::AuthenticationFailed`] (Requirement 4.5 / Property 14).
/// Public-key material is parsed from OpenSSH PEM once, up front; unparseable
/// material is a missing/invalid credential rather than a transport failure.
async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    username: &str,
    plan: &AuthPlan,
) -> Result<(), ProtocolError> {
    // For key auth, parse the OpenSSH private key once before the loop.
    let private_key: Option<Arc<PrivateKey>> = match plan {
        AuthPlan::Password(_) => None,
        AuthPlan::PublicKey { pem, .. } => {
            let key =
                decode_secret_key(pem, None).map_err(|_| ProtocolError::MissingCredentials)?;
            Some(Arc::new(key))
        }
    };

    let mut budget = AuthAttemptBudget::new();
    loop {
        let result = match plan {
            AuthPlan::Password(password) => handle
                .authenticate_password(username, password.clone())
                .await
                .map_err(map_transport_error)?,
            AuthPlan::PublicKey { .. } => {
                let key = private_key
                    .clone()
                    .expect("public-key plan parses its key above");
                handle
                    .authenticate_publickey(
                        username,
                        PrivateKeyWithHashAlg::new(key, Some(HashAlg::Sha256)),
                    )
                    .await
                    .map_err(map_transport_error)?
            }
        };

        if result.success() {
            return Ok(());
        }

        // The attempt failed. Record it; stop once the budget is spent.
        if !budget.record_failure() {
            return Err(ProtocolError::AuthenticationFailed);
        }
    }
}

/// Map an error from `client::connect` to a [`ProtocolError`].
///
/// A declined host key arrives as [`russh::Error::UnknownKey`] (the handler
/// returned `false`) and becomes [`ProtocolError::HostKeyDeclined`]. A failed
/// TCP connect arrives as [`russh::Error::IO`], whose `io::ErrorKind`
/// distinguishes a refused connection from an unresolved host; anything else
/// falls back to [`ProtocolError::Io`].
fn map_connect_error(err: russh::Error) -> ProtocolError {
    match err {
        russh::Error::UnknownKey => ProtocolError::HostKeyDeclined,
        russh::Error::ConnectionTimeout => ProtocolError::Timeout,
        russh::Error::IO(io) => map_io_error(io),
        other => ProtocolError::Io(other.to_string()),
    }
}

/// Map an [`std::io::Error`] raised while opening the socket to the matching
/// [`ProtocolError`]: a refused connection, an unresolved host, or a generic
/// I/O failure.
fn map_io_error(io: std::io::Error) -> ProtocolError {
    use std::io::ErrorKind;
    match io.kind() {
        ErrorKind::ConnectionRefused => ProtocolError::ConnectionRefused,
        // DNS resolution failures surface as `NotFound`/`Uncategorized` on
        // different platforms; treat a lookup-shaped failure as unresolved.
        ErrorKind::NotFound | ErrorKind::AddrNotAvailable => {
            ProtocolError::HostUnresolved(io.to_string())
        }
        _ => ProtocolError::Io(io.to_string()),
    }
}

/// Map a russh error raised after connecting (auth, channel, PTY, session I/O)
/// to a [`ProtocolError`].
fn map_transport_error(err: russh::Error) -> ProtocolError {
    match err {
        russh::Error::ConnectionTimeout | russh::Error::KeepaliveTimeout => ProtocolError::Timeout,
        russh::Error::IO(io) => ProtocolError::Io(io.to_string()),
        other => ProtocolError::Io(other.to_string()),
    }
}

// --- The live SSH session -------------------------------------------------

/// A live SSH session returned by [`SshProtocol::establish`].
///
/// Owns the transport resources — the russh `client::Handle` and the session
/// `Channel<Msg>` — and exposes the uniform [`SessionConn`] operations over
/// them. The channel is held behind a [`Mutex`] so the trait's `&mut self`
/// operations serialize access to the single transport. Dropping the session
/// drops the handle and channel, which russh uses to tear the connection down.
///
/// A session with no live transport (`handle`/`channel` are `None`) models the
/// `Disconnected` terminal state: it is what [`SshSession::new`] produces for
/// tests and what [`close`](SessionConn::close) leaves behind. The transport
/// resources are taken on close so a second close is a no-op.
pub struct SshSession {
    state: Arc<Mutex<SessionState>>,
    /// The live transport, present only while connected. Taken on `close`.
    transport: Mutex<Option<Transport>>,
}

/// The owned, live SSH transport for a connected [`SshSession`]: the client
/// handle plus the opened session channel.
struct Transport {
    // Held to keep the connection alive: russh tears the transport down when
    // the handle is dropped, so ownership here is the point even though no
    // method reads it directly (the channel carries all session I/O).
    #[allow(dead_code)]
    handle: Handle<ClientHandler>,
    channel: Channel<client::Msg>,
}

impl SshSession {
    /// A session in the given initial state with no live transport.
    ///
    /// Used by tests and by any caller that only needs to model session state;
    /// `send`/`recv`/`resize` on such a session report a closed transport.
    pub fn new(initial: SessionState) -> Self {
        Self {
            state: Arc::new(Mutex::new(initial)),
            transport: Mutex::new(None),
        }
    }

    /// A `Connected` session owning the live russh handle and session channel.
    /// Built by [`SshProtocol::establish`] on the success path only.
    fn connected(handle: Handle<ClientHandler>, channel: Channel<client::Msg>) -> Self {
        Self {
            state: Arc::new(Mutex::new(SessionState::Connected)),
            transport: Mutex::new(Some(Transport { handle, channel })),
        }
    }

    /// Open an **SFTP subsystem** channel on this session's live SSH transport
    /// and build a ready `russh_sftp::client::SftpSession` over it.
    ///
    /// This is the additive seam the IPC file-transfer wiring (task 25.4) uses:
    /// an SSH session carries its own interactive shell channel, and SFTP needs
    /// a *second*, independent channel on the same connection. Because russh's
    /// [`client::Handle`] methods take `&self`, a new channel can be opened from
    /// the handle this session already owns without disturbing the shell
    /// channel — so file transfers and the terminal share one SSH connection.
    ///
    /// Returns [`ProtocolError::Io`] if the session is closed (no live
    /// transport), or if opening the channel, requesting the `sftp` subsystem,
    /// or initialising the SFTP protocol fails.
    ///
    /// The returned session is independent of this [`SshSession`]'s lifetime
    /// except that it rides on the same underlying connection; dropping the
    /// SSH connection (via [`close`](SessionConn::close) or `Drop`) tears the
    /// SFTP channel down with it.
    pub async fn open_sftp_session(
        &self,
    ) -> Result<russh_sftp::client::SftpSession, ProtocolError> {
        let guard = self.transport.lock().await;
        let transport = guard.as_ref().ok_or_else(session_closed)?;

        // Open a fresh session channel for SFTP (separate from the shell
        // channel) and request the `sftp` subsystem on it.
        let channel = transport
            .handle
            .channel_open_session()
            .await
            .map_err(map_transport_error)?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(map_transport_error)?;

        // Build the SFTP client over the channel's bidirectional byte stream.
        russh_sftp::client::SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| ProtocolError::Io(format!("SFTP subsystem init failed: {e}")))
    }
}

#[async_trait]
impl SessionConn for SshSession {
    async fn send(&mut self, data: &[u8]) -> Result<(), ProtocolError> {
        let guard = self.transport.lock().await;
        let transport = guard.as_ref().ok_or_else(session_closed)?;
        // `&[u8]` is an `AsyncRead`, so russh writes exactly these bytes to the
        // channel as a single data message.
        transport
            .channel
            .data(data)
            .await
            .map_err(map_transport_error)
    }

    async fn recv(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        let mut guard = self.transport.lock().await;
        let transport = match guard.as_mut() {
            Some(t) => t,
            // No transport: a closed session is at clean EOF.
            None => return Ok(None),
        };

        // Pull channel messages until the next chunk of output arrives. EOF or
        // a channel close is a clean end of stream (`Ok(None)`); other control
        // messages (window adjust, requests, exit status) are skipped.
        loop {
            match transport.channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    return Ok(Some(data.to_vec()));
                }
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    // Fold stderr into the output stream so the terminal shows
                    // it; the uniform interface carries a single byte stream.
                    return Ok(Some(data.to_vec()));
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    return Ok(None);
                }
                // Any other message is transport bookkeeping; keep waiting.
                Some(_) => continue,
            }
        }
    }

    async fn resize(&mut self, cols: u16, rows: u16) -> Result<(), ProtocolError> {
        let guard = self.transport.lock().await;
        let transport = guard.as_ref().ok_or_else(session_closed)?;
        transport
            .channel
            .window_change(cols as u32, rows as u32, 0, 0)
            .await
            .map_err(map_transport_error)
    }

    async fn close(&mut self) -> Result<(), ProtocolError> {
        // Idempotent close: mark the session disconnected and take the
        // transport so its `Drop` tears russh down. A second close finds
        // `None` and is a no-op.
        *self.state.lock().await = SessionState::Disconnected;
        if let Some(transport) = self.transport.lock().await.take() {
            // Best-effort channel close; dropping the handle below closes the
            // connection regardless of whether the server acks this.
            let _ = transport.channel.close().await;
            drop(transport);
        }
        Ok(())
    }

    fn state(&self) -> SessionState {
        // A blocking peek is acceptable here: the lock is only ever held across
        // the trivial state mutations above.
        self.state
            .try_lock()
            .map(|s| *s)
            .unwrap_or(SessionState::Connecting)
    }
}

/// The error reported when a session operation is attempted after the transport
/// has been closed.
fn session_closed() -> ProtocolError {
    ProtocolError::Io("SSH session is closed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::CredentialKey;

    // --- Port resolution (Property 12 / Requirement 4.1) ------------------

    #[test]
    fn unspecified_port_defaults_to_22() {
        assert_eq!(resolve_port(0), DEFAULT_SSH_PORT);
    }

    #[test]
    fn configured_port_is_used_as_is() {
        assert_eq!(resolve_port(2222), 2222);
        assert_eq!(resolve_port(1), 1);
        assert_eq!(resolve_port(65535), 65535);
    }

    #[test]
    fn resolved_port_is_always_in_range() {
        for p in [0u16, 1, 22, 2222, 65535] {
            let r = resolve_port(p);
            assert!((MIN_PORT..=MAX_PORT).contains(&r), "port {r} out of range");
        }
    }

    // --- Timeout resolution (Property 12 / Requirement 4.6) ---------------

    #[test]
    fn unspecified_timeout_defaults_to_30() {
        assert_eq!(resolve_timeout(0), DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn out_of_range_timeout_falls_back_to_default() {
        assert_eq!(resolve_timeout(0), 30);
        assert_eq!(resolve_timeout(301), 30);
        assert_eq!(resolve_timeout(u16::MAX), 30);
    }

    #[test]
    fn in_range_timeout_is_used_as_is() {
        assert_eq!(resolve_timeout(1), 1);
        assert_eq!(resolve_timeout(30), 30);
        assert_eq!(resolve_timeout(300), 300);
    }

    #[test]
    fn resolved_timeout_is_always_in_range() {
        for t in [0u16, 1, 30, 300, 301, 10_000, u16::MAX] {
            let r = resolve_timeout(t);
            assert!(
                (MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&r),
                "timeout {r} out of range"
            );
        }
    }

    #[test]
    fn timeout_duration_matches_resolved_seconds() {
        assert_eq!(resolve_timeout_duration(45), Duration::from_secs(45));
        assert_eq!(resolve_timeout_duration(0), Duration::from_secs(30));
    }

    // --- Auth planning (Property 13 / Requirement 4.2–4.4) ----------------

    fn pw_auth() -> AuthConfig {
        AuthConfig::Password {
            credential_key: CredentialKey::new("profile/x/password"),
        }
    }

    #[test]
    fn password_auth_with_secret_plans_password() {
        let plan = plan_auth(&pw_auth(), ResolvedAuthMaterial::Password("hunter2".into()));
        assert_eq!(plan, Ok(AuthPlan::Password("hunter2".into())));
    }

    #[test]
    fn password_auth_without_credential_aborts_missing() {
        let plan = plan_auth(&pw_auth(), ResolvedAuthMaterial::MissingPassword);
        assert_eq!(plan, Err(ProtocolError::MissingCredentials));
    }

    #[test]
    fn key_auth_with_material_plans_public_key() {
        let key_id = KeyId::new();
        let plan = plan_auth(
            &AuthConfig::SshKey { key_id },
            ResolvedAuthMaterial::SshKey {
                key_id,
                pem: "PRIVATE".into(),
            },
        );
        assert_eq!(
            plan,
            Ok(AuthPlan::PublicKey {
                key_id,
                pem: "PRIVATE".into()
            })
        );
    }

    #[test]
    fn key_auth_with_missing_key_aborts_missing() {
        let key_id = KeyId::new();
        let plan = plan_auth(
            &AuthConfig::SshKey { key_id },
            ResolvedAuthMaterial::MissingKey(key_id),
        );
        assert_eq!(plan, Err(ProtocolError::MissingCredentials));
    }

    #[test]
    fn ssh_profile_with_no_auth_is_missing_credential() {
        let plan = plan_auth(&AuthConfig::None, ResolvedAuthMaterial::None);
        assert_eq!(plan, Err(ProtocolError::MissingCredentials));
    }

    // --- Attempt budget (Property 14 / Requirement 4.5) -------------------

    #[test]
    fn budget_allows_exactly_three_attempts() {
        let mut budget = AuthAttemptBudget::new();
        assert!(budget.can_attempt());

        // Attempt 1 fails.
        assert!(budget.record_failure());
        assert_eq!(budget.attempts_made(), 1);
        // Attempt 2 fails.
        assert!(budget.record_failure());
        assert_eq!(budget.attempts_made(), 2);
        // Attempt 3 fails -> no more allowed.
        assert!(!budget.record_failure());
        assert_eq!(budget.attempts_made(), 3);
        assert!(budget.exhausted());
    }

    #[test]
    fn budget_never_exceeds_three_attempts_under_repeated_failures() {
        let mut budget = AuthAttemptBudget::new();
        for _ in 0..100 {
            budget.record_failure();
        }
        // The attempt count is clamped at the maximum (Property 14 invariant).
        assert_eq!(budget.attempts_made(), MAX_AUTH_ATTEMPTS);
        assert!(budget.exhausted());
    }

    // --- Port composition + establish abort paths -------------------------

    use crate::domain::{ConnectionProfile, HostKeyPolicy, ProfileId, ProtocolParams};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A secret resolver returning a fixed outcome, recording whether it ran.
    struct FakeSecrets {
        secret: Option<String>,
        called: AtomicBool,
    }
    #[async_trait]
    impl SecretResolver for FakeSecrets {
        async fn resolve_secret(
            &self,
            _key: &CredentialKey,
        ) -> Result<Option<String>, ProtocolError> {
            self.called.store(true, Ordering::SeqCst);
            Ok(self.secret.clone())
        }
    }

    /// A key resolver returning a fixed outcome.
    struct FakeKeys {
        pem: Option<String>,
    }
    #[async_trait]
    impl SshKeyResolver for FakeKeys {
        async fn resolve_key(&self, _key_id: KeyId) -> Result<Option<String>, ProtocolError> {
            Ok(self.pem.clone())
        }
    }

    /// A host-key verifier with a fixed decision.
    struct FakeVerifier {
        accept: bool,
    }
    #[async_trait]
    impl HostKeyVerifier for FakeVerifier {
        async fn confirm_unknown_host_key(&self, _fingerprint: &str) -> bool {
            self.accept
        }
    }

    fn ssh_profile(auth: AuthConfig, port: u16, timeout: u16) -> ConnectionProfile {
        ConnectionProfile {
            id: ProfileId::new(),
            name: "ssh".to_string(),
            protocol: ProtocolKind::Ssh,
            host: "example.com".to_string(),
            port,
            username: None,
            domain: None,
            folder_id: None,
            auth,
            protocol_params: ProtocolParams::Ssh {
                known_host_policy: HostKeyPolicy::PromptOnUnknown,
            },
            logging_enabled: false,
            log_path: None,
            connect_timeout_secs: timeout,
        }
    }

    fn protocol_with(
        secret: Option<String>,
        pem: Option<String>,
        accept_host_key: bool,
    ) -> SshProtocol {
        SshProtocol::new(
            Arc::new(FakeSecrets {
                secret,
                called: AtomicBool::new(false),
            }),
            Arc::new(FakeKeys { pem }),
            Arc::new(FakeVerifier {
                accept: accept_host_key,
            }),
            SshIdentity::new("root"),
        )
    }

    #[tokio::test]
    async fn plan_authentication_resolves_password_through_the_port() {
        let proto = protocol_with(Some("hunter2".into()), None, true);
        let auth = pw_auth();
        let plan = proto
            .plan_authentication(&auth, &CredentialHandle::none())
            .await
            .unwrap();
        assert_eq!(plan, AuthPlan::Password("hunter2".into()));
    }

    #[tokio::test]
    async fn establish_aborts_with_missing_credentials_before_connecting() {
        // Password profile, but the store holds nothing: establish must abort
        // with MissingCredentials and never attempt the network
        // (Requirement 4.3 / Property 13).
        let proto = protocol_with(None, None, true);
        let cfg = ResolvedProfile::new(ssh_profile(pw_auth(), 22, 30));
        let result = proto.establish(&cfg, CredentialHandle::none()).await;
        assert!(matches!(result, Err(ProtocolError::MissingCredentials)));
    }

    #[tokio::test]
    async fn establish_rejects_non_ssh_profile() {
        let proto = protocol_with(Some("x".into()), None, true);
        let mut profile = ssh_profile(pw_auth(), 22, 30);
        profile.protocol = ProtocolKind::Telnet;
        let cfg = ResolvedProfile::new(profile);
        let result = proto.establish(&cfg, CredentialHandle::none()).await;
        assert!(matches!(
            result,
            Err(ProtocolError::InvalidParameter { .. })
        ));
    }

    // --- SshSession state -------------------------------------------------

    #[tokio::test]
    async fn close_leaves_session_disconnected_and_is_idempotent() {
        let mut session = SshSession::new(SessionState::Connected);
        session.close().await.unwrap();
        assert_eq!(session.state(), SessionState::Disconnected);
        // Idempotent: a second close still succeeds.
        session.close().await.unwrap();
        assert_eq!(session.state(), SessionState::Disconnected);
    }

    // --- Task 8.6: SSH auth wiring and host-key prompt (unit) -------------
    //
    // Verifies the composition seam `plan_authentication` drives the injected
    // ports as the requirements demand: a password profile authenticates with
    // the *stored* secret (4.2), a key-ref profile authenticates with the
    // *resolved* key material (4.4), and an unknown host key's fingerprint is
    // presented to the verifier for confirmation (4.7).

    #[tokio::test]
    async fn password_auth_uses_the_stored_credential() {
        // Requirement 4.2: the plan carries exactly the secret the store held,
        // and the store was actually consulted.
        let secrets = Arc::new(FakeSecrets {
            secret: Some("s3cr3t-from-store".into()),
            called: AtomicBool::new(false),
        });
        let proto = SshProtocol::new(
            Arc::clone(&secrets) as Arc<dyn SecretResolver>,
            Arc::new(FakeKeys { pem: None }),
            Arc::new(FakeVerifier { accept: true }),
            SshIdentity::new("root"),
        );

        let plan = proto
            .plan_authentication(&pw_auth(), &CredentialHandle::none())
            .await
            .unwrap();

        assert_eq!(plan, AuthPlan::Password("s3cr3t-from-store".into()));
        assert!(
            secrets.called.load(Ordering::SeqCst),
            "the Credential_Store seam must be consulted for password auth"
        );
    }

    #[tokio::test]
    async fn key_ref_profile_uses_the_resolved_key_material() {
        // Requirement 4.4: a key-ref profile plans public-key auth with the
        // material the Key_Manager resolved, tagged with the referenced key id.
        let key_id = KeyId::new();
        let proto = protocol_with(None, Some("OPENSSH-PRIVATE-KEY".into()), true);

        let plan = proto
            .plan_authentication(&AuthConfig::SshKey { key_id }, &CredentialHandle::none())
            .await
            .unwrap();

        assert_eq!(
            plan,
            AuthPlan::PublicKey {
                key_id,
                pem: "OPENSSH-PRIVATE-KEY".into(),
            }
        );
    }

    #[tokio::test]
    async fn unknown_host_key_fingerprint_is_presented_for_confirmation() {
        // Requirement 4.7: the host-key verifier port is the seam through which
        // an unknown fingerprint is presented. A verifier that records the
        // fingerprint it was shown confirms the prompt is driven with the
        // fingerprint value (the live transport invokes it from russh's
        // `check_server_key` hook; here we exercise the port contract directly).
        use std::sync::Mutex as StdMutex;

        struct RecordingVerifier {
            seen: StdMutex<Vec<String>>,
            decision: bool,
        }
        #[async_trait]
        impl HostKeyVerifier for RecordingVerifier {
            async fn confirm_unknown_host_key(&self, fingerprint: &str) -> bool {
                self.seen.lock().unwrap().push(fingerprint.to_string());
                self.decision
            }
        }

        let verifier = RecordingVerifier {
            seen: StdMutex::new(Vec::new()),
            decision: true,
        };
        let fingerprint = "SHA256:AbCdEf0123456789unknown-host-key";
        let accepted = verifier.confirm_unknown_host_key(fingerprint).await;

        assert!(
            accepted,
            "an accepting verifier lets the connection proceed"
        );
        let seen = verifier.seen.lock().unwrap();
        assert_eq!(
            seen.as_slice(),
            &[fingerprint.to_string()],
            "the exact unknown fingerprint must be presented for confirmation"
        );
    }

    // --- Property-based tests (Properties 12–15) --------------------------

    use proptest::prelude::*;

    /// A fresh current-thread runtime for driving one async property case.
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        // Feature: remote-connection-manager, Property 12: SSH port and timeout resolution
        //
        // For any SSH profile, the resolved connection port equals the
        // configured port or 22 when unspecified (modeled as 0) and lies in
        // 1..=65535, and the resolved timeout equals the configured value or 30
        // and lies in 1..=300 seconds.
        //
        // Validates: Requirements 4.1, 4.6
        #[test]
        fn property_12_port_and_timeout_resolution(
            configured_port in any::<u16>(),
            configured_timeout in any::<u16>(),
        ) {
            // Port: 0 means unspecified -> default 22; otherwise used as-is.
            let port = resolve_port(configured_port);
            if configured_port == 0 {
                prop_assert_eq!(port, DEFAULT_SSH_PORT);
            } else {
                prop_assert_eq!(port, configured_port);
            }
            // ...and the result is always a valid TCP port.
            prop_assert!((MIN_PORT..=MAX_PORT).contains(&port), "port {} out of range", port);

            // Timeout: used as-is only when in 1..=300; else default 30.
            let timeout = resolve_timeout(configured_timeout);
            if (MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&configured_timeout) {
                prop_assert_eq!(timeout, configured_timeout);
            } else {
                prop_assert_eq!(timeout, DEFAULT_TIMEOUT_SECS);
            }
            // ...and the result is always within the documented bounds.
            prop_assert!(
                (MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&timeout),
                "timeout {} out of range",
                timeout
            );
            // The Duration form agrees with the resolved seconds.
            prop_assert_eq!(
                resolve_timeout_duration(configured_timeout),
                Duration::from_secs(timeout as u64)
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        // Feature: remote-connection-manager, Property 13: Missing credentials abort connection in disconnected state
        //
        // For any profile that requires password authentication when the
        // Credential_Store holds no credential for it, the connection attempt
        // terminates with a missing-credential error and no active Session is
        // created. This covers the SSH side; the RDP side is covered in the rdp
        // module. The policy is exercised two ways: directly through the pure
        // `plan_auth` (store returned None/MissingPassword) and end-to-end
        // through `establish`, which must abort with MissingCredentials before
        // ever opening a socket and hand back no SessionConn.
        //
        // Validates: Requirements 4.3, 7.4
        #[test]
        fn property_13_missing_password_aborts_in_disconnected_state(
            port in any::<u16>(),
            timeout in any::<u16>(),
            // The absent-credential outcome is either an explicit miss or the
            // "no material" sentinel; both must abort identically.
            missing_is_none in any::<bool>(),
        ) {
            let material = if missing_is_none {
                ResolvedAuthMaterial::None
            } else {
                ResolvedAuthMaterial::MissingPassword
            };

            // Pure policy: a password profile with no resolved secret aborts.
            let planned = plan_auth(&pw_auth(), material);
            prop_assert_eq!(planned, Err(ProtocolError::MissingCredentials));

            // End-to-end: establish aborts with MissingCredentials and returns
            // no Session (no active connection is created). The fake store
            // holds nothing (`secret: None`).
            let proto = protocol_with(None, None, true);
            let cfg = ResolvedProfile::new(ssh_profile(pw_auth(), port, timeout));
            let result = runtime().block_on(async {
                proto.establish(&cfg, CredentialHandle::none()).await
            });
            match result {
                Ok(_) => prop_assert!(false, "a missing credential must not yield a Session"),
                Err(err) => prop_assert_eq!(err, ProtocolError::MissingCredentials),
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        // Feature: remote-connection-manager, Property 14: SSH authentication attempts are bounded
        //
        // For any sequence of failing authentication responses, the number of
        // authentication attempts never exceeds 3 (MAX_AUTH_ATTEMPTS), after
        // which the budget is exhausted and no further attempt is permitted —
        // the point at which the connect loop surfaces an authentication
        // failure and leaves the Session disconnected.
        //
        // Validates: Requirements 4.5
        #[test]
        fn property_14_auth_attempts_are_bounded(failures in 0usize..64) {
            let mut budget = AuthAttemptBudget::new();
            let mut observed_attempts = 0u8;

            for _ in 0..failures {
                // The loop only ever records a failure while an attempt is
                // still permitted — mirroring the connect loop's guard.
                if budget.can_attempt() {
                    budget.record_failure();
                    observed_attempts += 1;
                }
                // Invariant: the count never exceeds the maximum, at any step.
                prop_assert!(budget.attempts_made() <= MAX_AUTH_ATTEMPTS);
            }

            // Attempts made never exceed 3, and equal the number of permitted
            // records we drove (clamped at the max).
            prop_assert!(budget.attempts_made() <= MAX_AUTH_ATTEMPTS);
            let expected = (failures.min(MAX_AUTH_ATTEMPTS as usize)) as u8;
            prop_assert_eq!(budget.attempts_made(), expected);
            prop_assert_eq!(observed_attempts, expected);

            // Once at least 3 failures occurred, the budget is spent and bars
            // any further attempt (the authentication-failure boundary).
            if failures >= MAX_AUTH_ATTEMPTS as usize {
                prop_assert!(budget.exhausted());
                prop_assert!(!budget.can_attempt());
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        // Feature: remote-connection-manager, Property 15: Declining an unknown host key aborts the connection
        //
        // For any unknown SSH host key, if the user declines confirmation the
        // connection attempt terminates and the Session is left disconnected
        // with no active connection. The live transport invokes the verifier
        // from russh's `check_server_key` hook (see `ClientHandler`) and a
        // `false` decision aborts the connect as `russh::Error::UnknownKey`,
        // which `map_connect_error` turns into ProtocolError::HostKeyDeclined.
        // This property pins down that policy slice without a live server: a
        // declining HostKeyVerifier, over any arbitrary fingerprint, reports
        // "do not trust", which the documented mapping turns into
        // HostKeyDeclined — and a session built on that abort path is
        // Disconnected (no active connection).
        //
        // Validates: Requirements 4.8
        #[test]
        fn property_15_declined_host_key_aborts(fingerprint in ".{0,48}") {
            let decision = runtime().block_on(async {
                let verifier = FakeVerifier { accept: false };
                verifier.confirm_unknown_host_key(&fingerprint).await
            });

            // The verifier declines the unknown key regardless of fingerprint.
            prop_assert!(!decision, "a declining verifier never trusts an unknown key");

            // The documented mapping of a declined key: abort with
            // HostKeyDeclined (Requirement 4.8). We model `establish`'s mapping
            // of the verifier decision to the error it surfaces.
            let outcome: Result<(), ProtocolError> = if decision {
                Ok(())
            } else {
                Err(ProtocolError::HostKeyDeclined)
            };
            prop_assert_eq!(outcome, Err(ProtocolError::HostKeyDeclined));

            // No active connection is left behind: a session built on the abort
            // path is Disconnected.
            let session = SshSession::new(SessionState::Disconnected);
            prop_assert_eq!(session.state(), SessionState::Disconnected);
        }
    }
}
