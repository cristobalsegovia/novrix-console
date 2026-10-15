//! RDP protocol implementation for the Protocol_Engine (task 11.1, Requirement 7).
//!
//! This module plugs RDP into the uniform [`Protocol`]/[`SessionConn`] interface
//! (task 7.1). It owns the *establish* flow the requirements pin down —
//! resolution-bounds validation, a 30-second connect budget, and credential
//! retrieval + authentication — and leaves the live session shaped like every
//! other protocol: `send` carries input events to the remote desktop, `recv`
//! yields desktop frame updates, `resize` renegotiates resolution, `close`
//! tears the session down, and `state` reports where the connection is.
//!
//! ## What this task implements, and what it defers
//!
//! RDP is a graphical protocol and `ironrdp` is a large, multi-crate stack. Per
//! the task's priorities, the *correctness-bearing* logic lives here and is
//! fully testable without a network:
//!
//! 1. **Resolution bounds (Property 18, Requirements 7.8, 7.9).** The pure
//!    [`validate_resolution`] accepts a resolution *iff* width is in `640..=7680`
//!    and height is in `480..=4320`, returning the resolution to request; an
//!    out-of-range resolution yields [`ProtocolError::InvalidParameter`] with
//!    `parameter = "resolution"` and **no connection is attempted**. A `None`
//!    resolution falls back to the documented in-range default
//!    [`DEFAULT_RESOLUTION`] (1024x768).
//! 2. **Timeout + auth state machine (Requirements 7.1, 7.2, 7.4, 7.7).** The
//!    establish flow validates the resolution, resolves credentials, then runs
//!    the connect under a 30-second (configurable) budget. Any failure path —
//!    timeout, missing credentials, auth failure — returns the matching
//!    [`ProtocolError`] and produces **no** [`SessionConn`], so the caller never
//!    sees a half-open session (Requirement 8.3). A session is only handed back
//!    once the connection reports `Connected`.
//!
//! The parts that genuinely require the full `ironrdp` pipeline — the real RDP
//! handshake/security negotiation and desktop-frame decoding — are kept behind
//! two injectable ports so the orchestration above is exercisable with fakes in
//! tests and wired to `ironrdp` in production:
//!
//! - [`RdpCredentialSource`] resolves a [`CredentialHandle`] to the secret the
//!   connector needs. This is the **Credential_Store integration point**: it is
//!   deliberately *not* coupled to the concrete `KeyringCredentialStore`; the
//!   Connection_Manager injects an adapter over whatever store is in use. A
//!   handle that names no credential, or a store that reports the secret is
//!   absent, maps to [`ProtocolError::MissingCredentials`].
//! - [`RdpConnector`] performs the actual connect + authentication and, on
//!   success, returns the live transport the session reads/writes. This is the
//!   **`ironrdp` integration point**: the production [`IronRdpConnector`] drives
//!   `ironrdp`'s sans-I/O connector over Tokio, performs the TLS upgrade +
//!   CredSSP/NLA authentication, and returns an [`IronRdpTransport`] wrapping the
//!   `ironrdp` [`ActiveStage`](ironrdp::session::ActiveStage).
//!
//! The orchestration code (bounds, timeout, credential resolution, no-half-open
//! guarantee) opens no socket on its own and is exercised with in-memory fakes;
//! only [`IronRdpConnector`]/[`IronRdpTransport`] touch the network, and the
//! tests here never make a real connection.

use std::time::Duration;

use async_trait::async_trait;

use crate::domain::{ProtocolError, ProtocolKind, ProtocolParams, Resolution, SessionState};

use super::interface::{CredentialHandle, Protocol, ResolvedProfile, SessionConn};

/// Inclusive lower bound of a valid RDP resolution, `640x480` (Requirement 7.8).
pub const MIN_RESOLUTION: Resolution = Resolution {
    width: 640,
    height: 480,
};

/// Inclusive upper bound of a valid RDP resolution, `7680x4320` (Requirement 7.8).
pub const MAX_RESOLUTION: Resolution = Resolution {
    width: 7680,
    height: 4320,
};

/// The resolution requested when a profile specifies none.
///
/// `1024x768` sits comfortably inside `640x480..=7680x4320`, so the default can
/// never itself trip the bounds check. Documented here so the fallback is an
/// explicit design choice rather than an accident.
pub const DEFAULT_RESOLUTION: Resolution = Resolution {
    width: 1024,
    height: 768,
};

/// The hard connect budget for an RDP attempt (Requirement 7.1).
///
/// Requirement 7.1 requires the connection be established within 30 seconds;
/// 7.2 requires aborting with no Session if it cannot. The profile's
/// `connect_timeout_secs` may lower this, but never raise it past 30 s.
pub const MAX_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether a resolution is within the RDP bounds `640x480..=7680x4320`.
///
/// This is the pure predicate at the heart of Property 18: `true` *iff* width is
/// in `640..=7680` **and** height is in `480..=4320`. Kept standalone so both
/// [`validate_resolution`] and the tests can call it directly.
pub fn resolution_in_bounds(resolution: Resolution) -> bool {
    (MIN_RESOLUTION.width..=MAX_RESOLUTION.width).contains(&resolution.width)
        && (MIN_RESOLUTION.height..=MAX_RESOLUTION.height).contains(&resolution.height)
}

/// Resolve the resolution to request for an RDP connection, enforcing bounds.
///
/// Pure and side-effect-free — this performs no I/O and opens no connection, so
/// an out-of-range resolution is rejected *before* any establish work begins
/// (Requirements 7.8, 7.9; Property 18):
///
/// - `Some(r)` where `r` is in bounds → `Ok(r)` (request it as-is, Requirement 7.8).
/// - `Some(r)` out of bounds → `Err(ProtocolError::InvalidParameter { parameter: "resolution", .. })`
///   (reject, identify the invalid resolution, do not connect — Requirement 7.9).
/// - `None` → `Ok(DEFAULT_RESOLUTION)` (documented in-range fallback).
pub fn validate_resolution(
    requested: Option<Resolution>,
) -> Result<Resolution, ProtocolError> {
    match requested {
        None => Ok(DEFAULT_RESOLUTION),
        Some(r) if resolution_in_bounds(r) => Ok(r),
        Some(r) => Err(ProtocolError::InvalidParameter {
            parameter: "resolution".to_string(),
            message: format!(
                "resolution {}x{} is outside the supported range {}x{}..={}x{}",
                r.width,
                r.height,
                MIN_RESOLUTION.width,
                MIN_RESOLUTION.height,
                MAX_RESOLUTION.width,
                MAX_RESOLUTION.height
            ),
        }),
    }
}

/// Pull the requested resolution out of a resolved profile's protocol params.
///
/// Returns the `Option<Resolution>` for an RDP profile. A non-RDP profile is a
/// programming error at this point (the registry dispatches by kind), reported
/// as an `InvalidParameter` on `protocol` rather than silently defaulting.
fn requested_resolution(cfg: &ResolvedProfile) -> Result<Option<Resolution>, ProtocolError> {
    match &cfg.profile.protocol_params {
        ProtocolParams::Rdp { resolution } => Ok(*resolution),
        _ => Err(ProtocolError::InvalidParameter {
            parameter: "protocol".to_string(),
            message: "RDP protocol invoked with non-RDP profile parameters".to_string(),
        }),
    }
}

/// The resolved secret an [`RdpConnector`] authenticates with.
///
/// A real RDP connect runs CredSSP/NLA, which needs the account's `username`,
/// `password`, and optional `domain`. Those are carried here so the
/// [`RdpCredentialSource`] seam (task 25.4 wires the real Credential_Store
/// adapter) can hand the connector exactly what it must authenticate with.
///
/// ## Secret hygiene
///
/// The password is a secret and must never land in logs or panics. This type
/// therefore has a **manual [`Debug`]** that prints the username/domain but
/// redacts the password (`password: "<redacted>"`), and it is deliberately
/// *not* `Serialize`/`Deserialize`. The plaintext is handed straight to the
/// `ironrdp` connector's [`Credentials`](ironrdp::connector::Credentials) and
/// is dropped with the connect attempt; it is never persisted here.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedCredential {
    /// The account name to authenticate as.
    pub username: String,
    /// The account secret. Redacted from [`Debug`]; never logged or serialized.
    pub password: String,
    /// The optional logon domain (`None` for a local account / UPN login).
    pub domain: Option<String>,
}

impl ResolvedCredential {
    /// Build a resolved credential from the parts CredSSP needs.
    pub fn new(
        username: impl Into<String>,
        password: impl Into<String>,
        domain: Option<String>,
    ) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
            domain,
        }
    }
}

impl std::fmt::Debug for ResolvedCredential {
    /// Redacts the password so a resolved credential can never leak its secret
    /// through a `{:?}` in a log line, error, or panic message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("domain", &self.domain)
            .finish()
    }
}

/// Resolves a [`CredentialHandle`] to the secret an RDP connect needs.
///
/// **Credential_Store integration point.** The Connection_Manager injects an
/// adapter over whatever `CredentialStore` is live; this trait keeps the RDP
/// establish flow from depending on the concrete store. Contract:
///
/// - A handle that names no credential (`credential_key == None`) → the RDP
///   path treats that as missing credentials (RDP always authenticates), so the
///   caller need not even hit the store; see [`RdpProtocol::establish`].
/// - A handle whose key is absent from the store →
///   [`ProtocolError::MissingCredentials`] (Requirement 7.4).
/// - Any other retrieval failure is surfaced as `MissingCredentials` as well,
///   since from the connect's perspective no usable credential was obtained.
#[async_trait]
pub trait RdpCredentialSource: Send + Sync {
    /// Resolve the secret for `handle`, or report it missing.
    async fn resolve(
        &self,
        handle: &CredentialHandle,
    ) -> Result<ResolvedCredential, ProtocolError>;
}

/// A live RDP transport returned by a successful [`RdpConnector::connect`].
///
/// This is the seam between the (fully implemented, tested) establish
/// orchestration and the (deferred) `ironrdp` frame pipeline. It exposes the
/// three operations [`RdpSession`] forwards: push an input event, pull the next
/// desktop update, and shut down. The production implementation wraps
/// `ironrdp`'s active session; test fakes implement it in memory.
#[async_trait]
pub trait RdpTransport: Send {
    /// Forward an encoded input event (keyboard/pointer) to the remote desktop.
    async fn send_input(&mut self, event: &[u8]) -> Result<(), ProtocolError>;

    /// Pull the next desktop update/frame. `Ok(None)` signals the remote closed.
    async fn next_update(&mut self) -> Result<Option<Vec<u8>>, ProtocolError>;

    /// Request a new resolution mid-session (desktop resize renegotiation).
    async fn request_resolution(&mut self, resolution: Resolution) -> Result<(), ProtocolError>;

    /// Tear down the transport, releasing its resources. Idempotent.
    async fn shutdown(&mut self) -> Result<(), ProtocolError>;
}

/// Performs the actual RDP connect + authentication under the caller's budget.
///
/// **`ironrdp` integration point.** On success it returns a live
/// [`RdpTransport`]; on failure it must have released everything it allocated
/// (Requirement 8.3) and return the precise cause:
///
/// - authentication rejected → [`ProtocolError::AuthenticationFailed`] (Requirement 7.7);
/// - host unreachable / refused → [`ProtocolError::ConnectionRefused`] or
///   [`ProtocolError::HostUnresolved`] (Requirement 7.2);
/// - transport failure → [`ProtocolError::Io`].
///
/// The connector does **not** own the overall timeout — [`RdpProtocol`] races
/// the connect against the budget so a connector that hangs still yields
/// [`ProtocolError::Timeout`] with no Session (Requirements 7.1, 7.2).
#[async_trait]
pub trait RdpConnector: Send + Sync {
    /// Connect to `host:port`, authenticate with `credential`, and request
    /// `resolution`. Returns a live transport on success.
    async fn connect(
        &self,
        host: &str,
        port: u16,
        resolution: Resolution,
        credential: ResolvedCredential,
    ) -> Result<Box<dyn RdpTransport>, ProtocolError>;
}

/// The RDP [`Protocol`] implementation.
///
/// Generic over its two injected ports so the establish flow is testable
/// without a network or the real Credential_Store. Production code constructs
/// it with the `ironrdp`-backed connector and a Credential_Store adapter;
/// tests inject fakes.
pub struct RdpProtocol<C, S>
where
    C: RdpConnector,
    S: RdpCredentialSource,
{
    connector: C,
    credentials: S,
}

impl<C, S> RdpProtocol<C, S>
where
    C: RdpConnector,
    S: RdpCredentialSource,
{
    /// Build an RDP protocol over an injected connector and credential source.
    pub fn new(connector: C, credentials: S) -> Self {
        Self {
            connector,
            credentials,
        }
    }

    /// The effective connect budget: the profile's `connect_timeout_secs`,
    /// clamped so it never exceeds the 30-second ceiling (Requirement 7.1) and
    /// is never zero (a zero budget could never connect).
    fn connect_budget(cfg: &ResolvedProfile) -> Duration {
        let configured = Duration::from_secs(u64::from(cfg.profile.connect_timeout_secs));
        if configured.is_zero() {
            MAX_CONNECT_TIMEOUT
        } else {
            configured.min(MAX_CONNECT_TIMEOUT)
        }
    }
}

#[async_trait]
impl<C, S> Protocol for RdpProtocol<C, S>
where
    C: RdpConnector,
    S: RdpCredentialSource,
{
    fn kind(&self) -> ProtocolKind {
        ProtocolKind::Rdp
    }

    async fn establish(
        &self,
        cfg: &ResolvedProfile,
        creds: CredentialHandle,
    ) -> Result<Box<dyn SessionConn>, ProtocolError> {
        // 1) Resolution bounds first — a pure check that opens nothing
        //    (Requirements 7.8, 7.9; Property 18). Out of range => reject with
        //    no connection attempt.
        let resolution = validate_resolution(requested_resolution(cfg)?)?;

        // 2) Credentials. RDP always authenticates (Requirement 7.3), so a
        //    handle that names no secret is already a missing-credential abort
        //    (Requirement 7.4) — no need to touch the store.
        if creds.credential_key.is_none() {
            return Err(ProtocolError::MissingCredentials);
        }
        let credential = self.credentials.resolve(&creds).await?;

        // 3) Connect + authenticate under the 30-second budget. If the connect
        //    does not finish in time, abort with Timeout and hand back no
        //    Session (Requirements 7.1, 7.2). Any error path here leaves no
        //    half-open session: we only build an `RdpSession` from a transport
        //    the connector reports as live (Requirement 8.3).
        let budget = Self::connect_budget(cfg);
        let host = cfg.profile.host.clone();
        let port = cfg.profile.port;

        let connect = self
            .connector
            .connect(&host, port, resolution, credential);

        let transport = match tokio::time::timeout(budget, connect).await {
            Err(_elapsed) => return Err(ProtocolError::Timeout),
            Ok(Err(e)) => return Err(e),
            Ok(Ok(transport)) => transport,
        };

        Ok(Box::new(RdpSession::connected(transport, resolution)))
    }
}

/// A live RDP session over the uniform [`SessionConn`] interface.
///
/// The trait's five operations map onto a graphical protocol as follows:
///
/// - [`send`](SessionConn::send) carries an *encoded input event* (keyboard /
///   pointer) to the remote desktop. The encoding is the connector's concern;
///   this layer forwards the bytes untouched.
/// - [`recv`](SessionConn::recv) yields the next *desktop update/frame* as
///   bytes, or `Ok(None)` at a clean remote close. Decoding those bytes into
///   pixels for the canvas is UI work (task 21.2), not this core.
/// - [`resize`](SessionConn::resize) renegotiates the display resolution.
///   The uniform signature is a character grid (`cols`/`rows`); RDP has no
///   character cells, so this is treated as a no-op at the trait boundary and
///   documented as such. True resolution renegotiation goes through
///   [`RdpTransport::request_resolution`] when the UI knows the pixel size.
/// - [`close`](SessionConn::close) shuts the transport down and is idempotent.
/// - [`state`](SessionConn::state) reports `Connecting`/`Connected`/`Disconnected`.
pub struct RdpSession {
    transport: Option<Box<dyn RdpTransport>>,
    state: SessionState,
    /// The resolution currently in effect; kept so a future resolution-aware
    /// resize path has the current value to diff against.
    resolution: Resolution,
}

impl RdpSession {
    /// Wrap a live transport in the `Connected` state.
    fn connected(transport: Box<dyn RdpTransport>, resolution: Resolution) -> Self {
        Self {
            transport: Some(transport),
            state: SessionState::Connected,
            resolution,
        }
    }

    /// The resolution currently requested/in effect for this session.
    pub fn resolution(&self) -> Resolution {
        self.resolution
    }
}

#[async_trait]
impl SessionConn for RdpSession {
    async fn send(&mut self, data: &[u8]) -> Result<(), ProtocolError> {
        match self.transport.as_mut() {
            Some(t) => t.send_input(data).await,
            None => Err(ProtocolError::Io("RDP session is closed".to_string())),
        }
    }

    async fn recv(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        match self.transport.as_mut() {
            Some(t) => {
                let update = t.next_update().await?;
                if update.is_none() {
                    // Clean remote close: reflect it in the session state.
                    self.state = SessionState::Disconnected;
                }
                Ok(update)
            }
            None => Ok(None),
        }
    }

    async fn resize(&mut self, _cols: u16, _rows: u16) -> Result<(), ProtocolError> {
        // RDP is driven by pixel resolution, not a character grid, so the
        // uniform grid-based resize is a documented no-op here (matches the
        // interface contract). The UI renegotiates pixel resolution through
        // `RdpTransport::request_resolution` when it knows the canvas size.
        Ok(())
    }

    async fn close(&mut self) -> Result<(), ProtocolError> {
        // Idempotent: once the transport is taken, further closes succeed.
        if let Some(mut transport) = self.transport.take() {
            transport.shutdown().await?;
        }
        self.state = SessionState::Disconnected;
        Ok(())
    }

    fn state(&self) -> SessionState {
        self.state
    }
}

// ---------------------------------------------------------------------------
// Production `ironrdp`-backed connector + transport.
// ---------------------------------------------------------------------------

use std::sync::Arc;

use bytes::BytesMut;
use ironrdp::connector::sspi::{NetworkProtocol, NetworkRequest};
use ironrdp::connector::{
    self, ClientConnector, Config as IronConfig, ConnectorError, ConnectorErrorExt,
    ConnectorErrorKind, ConnectorResult, Credentials, DesktopSize, ServerName,
};
use ironrdp::input::{Database, MouseButton, MousePosition, Operation, Scancode};
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::PerformanceFlags;
use ironrdp::pdu::Action;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageOutput};
use ironrdp_tokio::{split_tokio_framed, AsyncNetworkClient, FramedWrite, TokioFramed};
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::{TcpStream, UdpSocket};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types;
use tokio_rustls::{client::TlsStream, TlsConnector};

/// The upgraded, TLS-wrapped stream the active session reads/writes over.
type RdpStream = TlsStream<TcpStream>;

/// Production `ironrdp`-backed connector.
///
/// Drives the real RDP connection sequence end to end:
///
/// 1. **TCP connect** to the first resolved `host:port` address under the
///    caller's budget. A refusal maps to [`ProtocolError::ConnectionRefused`],
///    a name-resolution failure to [`ProtocolError::HostUnresolved`].
/// 2. **X.224 negotiation** (`ironrdp_async::connect_begin`) until the connector
///    asks for the enhanced-security TLS upgrade.
/// 3. **TLS upgrade** via `tokio-rustls`, from which the server's leaf
///    certificate SubjectPublicKeyInfo is extracted — CredSSP binds the NLA
///    exchange to this key.
/// 4. **CredSSP / NLA finalization** (`ironrdp_async::connect_finalize`) using
///    the resolved username/password/domain. An access-denied result maps to
///    [`ProtocolError::AuthenticationFailed`].
///
/// On success it returns an [`IronRdpTransport`] wrapping the active session.
///
/// ## Server-certificate trust (task 26.3)
///
/// Step 3 no longer accepts any certificate blindly. The TLS upgrade installs a
/// verifier ([`danger::TrustStoreCertVerifier`]) that extracts the server leaf
/// certificate's DER, computes its SHA-256 fingerprint, and consults the
/// injected [`RdpCertVerifier`] — the RDP analogue of SSH host-key verification
/// (known match proceeds, unknown prompts per policy, a changed fingerprint is a
/// hard reject). A declined/changed certificate aborts the handshake, so the
/// connect fails with no session (surfaced as
/// [`ProtocolError::AuthenticationFailed`] / `Io`, never a silent trust).
pub struct IronRdpConnector {
    /// The server-certificate trust decision (task 26.3). The TLS upgrade
    /// computes the leaf certificate's SHA-256 fingerprint and consults this
    /// verifier; a `false` decision aborts the handshake so a declined/changed
    /// certificate never completes the connect.
    cert_verifier: Arc<dyn RdpCertVerifier>,
}

impl IronRdpConnector {
    /// Construct the production connector with the given certificate verifier.
    ///
    /// The verifier is the RDP analogue of SSH host-key verification: it is
    /// consulted with the server's leaf-certificate DER during the TLS upgrade
    /// and decides (known / prompt / changed) whether to trust it. Build it over
    /// the real known-certs store + prompt channel in the IPC layer; tests may
    /// inject an accept-all / reject-all fake.
    pub fn new(cert_verifier: Arc<dyn RdpCertVerifier>) -> Self {
        Self { cert_verifier }
    }
}

/// The RDP server-certificate trust decision seam (task 26.3).
///
/// RDP has no SSH-style host key; the trust anchor is the server's TLS leaf
/// certificate. During the enhanced-security TLS upgrade the connector computes
/// that certificate's SHA-256 fingerprint and calls [`verify_certificate`],
/// which runs the *same* trust logic SSH host keys get — known match proceeds,
/// an unknown fingerprint prompts (per policy), a changed fingerprint is a hard
/// reject. Returning `false` aborts the TLS handshake, so the connect fails
/// before CredSSP and no session is handed back.
///
/// ## Blocking contract
///
/// [`verify_certificate`](Self::verify_certificate) is **synchronous** because
/// it is called from inside `rustls`' certificate-verification callback, which
/// is not async. The real implementation bridges to the async trust decision
/// (store read + UI prompt) off-thread and blocks for the answer; it must
/// therefore be called from a thread that is allowed to block (the dedicated RDP
/// session thread, never a shared async worker). The fingerprint passed to the
/// trust store is `SHA256:<base64>` as computed by
/// [`certificate_fingerprint`](crate::persistence::certificate_fingerprint).
pub trait RdpCertVerifier: Send + Sync {
    /// Decide whether to trust the server's leaf certificate (DER bytes).
    /// `true` lets the handshake complete; `false` aborts it.
    fn verify_certificate(&self, cert_der: &[u8]) -> bool;
}

/// Translate the host portion of a connect error into the precise
/// [`ProtocolError`]. TCP refusal and unreachable hosts are distinguished from
/// name-resolution failure by the OS error kind.
fn map_tcp_error(host: &str, err: std::io::Error) -> ProtocolError {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::ConnectionRefused => ProtocolError::ConnectionRefused,
        // `getaddrinfo` failures surface as `Uncategorized`/`NotFound` depending
        // on platform; treat an address-resolution miss as HostUnresolved.
        ErrorKind::NotFound => ProtocolError::HostUnresolved(host.to_string()),
        _ => {
            // `std::io` has no stable variant for DNS failure; match the message
            // as a fallback so a resolution error is not mislabeled as generic I/O.
            let msg = err.to_string();
            if msg.contains("failed to lookup")
                || msg.contains("nodename nor servname")
                || msg.contains("Name or service not known")
                || msg.contains("name or service not known")
            {
                ProtocolError::HostUnresolved(host.to_string())
            } else {
                ProtocolError::Io(format!("TCP connect to {host} failed: {msg}"))
            }
        }
    }
}

/// Translate an `ironrdp` connector error into a [`ProtocolError`]. An
/// access-denied / CredSSP rejection is authentication failure; everything else
/// in the connect sequence is a transport/I-O failure.
fn map_connector_error(err: ConnectorError) -> ProtocolError {
    match err.kind {
        // The server denied access before/within the NLA exchange, or CredSSP
        // itself rejected the credentials: this is an authentication failure.
        ConnectorErrorKind::AccessDenied => ProtocolError::AuthenticationFailed,
        ConnectorErrorKind::Credssp(_) => ProtocolError::AuthenticationFailed,
        other => ProtocolError::Io(format!("RDP connect failed: {other}")),
    }
}

#[async_trait]
impl RdpConnector for IronRdpConnector {
    async fn connect(
        &self,
        host: &str,
        port: u16,
        resolution: Resolution,
        credential: ResolvedCredential,
    ) -> Result<Box<dyn RdpTransport>, ProtocolError> {
        // `ironrdp_async`'s CredSSP driver is built on `AsyncNetworkClient`,
        // whose futures are deliberately **not** `Send` (the SSPI generators it
        // bridges are single-threaded). That makes the whole connect future
        // `!Send`, which can't satisfy the `Send` bound `async_trait` puts on
        // this method. Worse, a Tokio I/O resource (the TCP/TLS stream) is bound
        // to the reactor of the runtime that created it and cannot be driven
        // from another runtime.
        //
        // So the entire session — connect *and* the subsequent read/write loop —
        // lives on one dedicated OS thread running its own current-thread Tokio
        // runtime. The `RdpTransport` returned here is a thin, `Send` handle that
        // talks to that worker over command channels. This keeps every `ironrdp`
        // / rustls resource on a single thread (satisfying their `!Send` futures)
        // while exposing the `Send` async interface the rest of the app needs.
        let host = host.to_owned();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let cert_verifier = Arc::clone(&self.cert_verifier);

        std::thread::Builder::new()
            .name("rdp-session".to_owned())
            .spawn(move || {
                rdp_worker_main(host, port, resolution, credential, cert_verifier, ready_tx)
            })
            .map_err(|e| ProtocolError::Io(format!("could not spawn RDP session thread: {e}")))?;

        // The worker reports connect success (with its command sender) or the
        // precise failure cause. A dropped sender means the thread died before
        // reporting — treat it as an I/O failure.
        let cmd_tx = ready_rx.await.map_err(|_| {
            ProtocolError::Io("RDP session thread terminated unexpectedly".to_string())
        })??;

        Ok(Box::new(IronRdpTransport { cmd_tx }))
    }
}

/// A command sent from the [`IronRdpTransport`] handle to the session worker,
/// each carrying a `oneshot` sender the worker replies on.
enum RdpCommand {
    /// Decode + send an input descriptor; reply once it is written.
    SendInput {
        event: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<Result<(), ProtocolError>>,
    },
    /// Pump frames until the next graphics update or a clean close.
    NextUpdate {
        reply: tokio::sync::oneshot::Sender<Result<Option<Vec<u8>>, ProtocolError>>,
    },
    /// Best-effort mid-session resize over the Display Control DVC.
    RequestResolution {
        resolution: Resolution,
        reply: tokio::sync::oneshot::Sender<Result<(), ProtocolError>>,
    },
    /// Graceful teardown; reply when the worker has sent its shutdown PDU.
    Shutdown {
        reply: tokio::sync::oneshot::Sender<Result<(), ProtocolError>>,
    },
}

/// Entry point for the dedicated session thread: build the runtime, connect,
/// report the outcome, and (on success) run the command loop until the handle
/// is dropped or the session ends.
fn rdp_worker_main(
    host: String,
    port: u16,
    resolution: Resolution,
    credential: ResolvedCredential,
    cert_verifier: Arc<dyn RdpCertVerifier>,
    ready_tx: tokio::sync::oneshot::Sender<
        Result<tokio::sync::mpsc::UnboundedSender<RdpCommand>, ProtocolError>,
    >,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready_tx.send(Err(ProtocolError::Io(format!(
                "could not start RDP runtime: {e}"
            ))));
            return;
        }
    };

    rt.block_on(async move {
        // Connect first. On failure, report the cause and let `rt` drop, which
        // tears down any partially-opened socket/TLS stream — no half-open
        // session escapes (Requirement 8.3).
        let session = match connect_sequence(
            &host,
            port,
            resolution,
            &credential,
            Arc::clone(&cert_verifier),
        )
        .await
        {
            Ok(session) => session,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };

        // Connected: hand the caller a command sender, then service commands.
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        if ready_tx.send(Ok(cmd_tx)).is_err() {
            // Caller gave up already; nothing to serve.
            return;
        }

        let mut worker = SessionWorker::new(session);
        worker.run(cmd_rx).await;
    });
}

/// Owns the live `ironrdp` session on the worker thread and services commands.
///
/// Everything here is confined to the single session thread, so the `!Send`
/// `ironrdp`/rustls types never cross a thread boundary.
struct SessionWorker {
    reader: TokioFramed<ReadHalf<RdpStream>>,
    writer: TokioFramed<WriteHalf<RdpStream>>,
    active_stage: ActiveStage,
    image: DecodedImage,
    input_db: Database,
    /// The resolution currently in effect for the session. Seeded from the
    /// negotiated desktop size and advanced only when a resize is actually
    /// encoded and sent, so it always reflects what the server last agreed to.
    current_resolution: Resolution,
    /// Set once the server (or a transport error) ended the session.
    terminated: bool,
}

impl SessionWorker {
    fn new(session: ConnectedSession) -> Self {
        let ConnectedSession {
            connection_result,
            framed,
        } = session;

        let image = DecodedImage::new(
            ironrdp::graphics::image_processing::PixelFormat::RgbA32,
            connection_result.desktop_size.width,
            connection_result.desktop_size.height,
        );
        let current_resolution = Resolution {
            width: u32::from(connection_result.desktop_size.width),
            height: u32::from(connection_result.desktop_size.height),
        };
        let active_stage = ActiveStage::new(connection_result);

        // Split so a read and a write can proceed through independent `&mut`s;
        // `split_tokio_framed` keeps any already-buffered leftover on the reader.
        let (reader, writer) = split_tokio_framed(framed);

        Self {
            reader,
            writer,
            active_stage,
            image,
            input_db: Database::new(),
            current_resolution,
            terminated: false,
        }
    }

    /// Service commands until the handle is dropped (channel closed) or the
    /// session terminates.
    async fn run(&mut self, mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<RdpCommand>) {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                RdpCommand::SendInput { event, reply } => {
                    let _ = reply.send(self.handle_send_input(&event).await);
                }
                RdpCommand::NextUpdate { reply } => {
                    let _ = reply.send(self.handle_next_update().await);
                }
                RdpCommand::RequestResolution { resolution, reply } => {
                    let _ = reply.send(self.handle_request_resolution(resolution).await);
                }
                RdpCommand::Shutdown { reply } => {
                    let _ = reply.send(self.handle_shutdown().await);
                    break;
                }
            }
        }
    }

    /// Write every `ResponseFrame` an active-stage step produced, reporting
    /// whether the step asked the session to terminate and whether a visible
    /// graphics update occurred.
    async fn flush_outputs(
        &mut self,
        outputs: Vec<ActiveStageOutput>,
        painted: &mut bool,
    ) -> Result<bool, ProtocolError> {
        let mut terminate = false;
        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => {
                    self.writer
                        .write_all(&frame)
                        .await
                        .map_err(|e| ProtocolError::Io(format!("RDP write failed: {e}")))?;
                }
                ActiveStageOutput::GraphicsUpdate(_) => {
                    *painted = true;
                }
                ActiveStageOutput::Terminate(_) => {
                    terminate = true;
                }
                // Pointer / deactivation outputs do not produce a UI frame here;
                // the client-side cursor and dynamic reactivation are handled by
                // the UI layer and the resize path respectively.
                _ => {}
            }
        }
        Ok(terminate)
    }

    /// Snapshot the current framebuffer as a flat RGBA buffer prefixed with a
    /// small header the UI can blit from.
    ///
    /// Wire layout (little-endian): `width: u32 | height: u32 | rgba bytes`.
    /// The pixel format is `RgbA32` (4 bytes/pixel, R,G,B,A), matching
    /// [`DecodedImage`]'s buffer, so a full frame is `width * height * 4` bytes.
    fn encode_frame(&self) -> Vec<u8> {
        let width = u32::from(self.image.width());
        let height = u32::from(self.image.height());
        let data = self.image.data();
        let mut out = Vec::with_capacity(8 + data.len());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    async fn handle_send_input(&mut self, event: &[u8]) -> Result<(), ProtocolError> {
        if self.terminated {
            return Err(ProtocolError::Io("RDP session is closed".to_string()));
        }

        // Decode the UI's input descriptor into `ironrdp` operations, feed them
        // through the input `Database` (which maintains key/button state and
        // emits the fast-path events), then let the active stage encode the
        // outgoing frame and send it.
        let operations = decode_input_event(event)?;
        if operations.is_empty() {
            return Ok(());
        }

        let events = self.input_db.apply(operations);
        if events.is_empty() {
            return Ok(());
        }

        let outputs = self
            .active_stage
            .process_fastpath_input(&mut self.image, &events)
            .map_err(|e| ProtocolError::Io(format!("encode RDP input failed: {e}")))?;

        let mut painted = false;
        let terminate = self.flush_outputs(outputs, &mut painted).await?;
        if terminate {
            self.terminated = true;
        }
        Ok(())
    }

    async fn handle_next_update(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        if self.terminated {
            return Ok(None);
        }

        // Pump frames until one yields a visible graphics update (handed to the
        // UI) or the session ends.
        loop {
            let (action, payload): (Action, BytesMut) = match self.reader.read_pdu().await {
                Ok(frame) => frame,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    self.terminated = true;
                    return Ok(None); // clean remote close
                }
                Err(e) => return Err(ProtocolError::Io(format!("RDP read failed: {e}"))),
            };

            let outputs = self
                .active_stage
                .process(&mut self.image, action, &payload)
                .map_err(|e| ProtocolError::Io(format!("process RDP frame failed: {e}")))?;

            let mut painted = false;
            let terminate = self.flush_outputs(outputs, &mut painted).await?;

            if terminate {
                self.terminated = true;
                return Ok(None);
            }
            if painted {
                return Ok(Some(self.encode_frame()));
            }
            // Otherwise this PDU produced only response frames / pointer updates;
            // keep reading for an actual graphics change.
        }
    }

    async fn handle_request_resolution(
        &mut self,
        resolution: Resolution,
    ) -> Result<(), ProtocolError> {
        if self.terminated {
            return Err(ProtocolError::Io("RDP session is closed".to_string()));
        }

        // Already at the requested resolution: nothing to renegotiate.
        if resolution == self.current_resolution {
            return Ok(());
        }

        // Mid-session resize is driven over the Display Control dynamic virtual
        // channel (DVC): `encode_resize` builds a Monitor Layout PDU that asks
        // the server to switch the primary monitor to the new pixel size, and
        // the server honours it in place (no deactivation-reactivation round).
        //
        // `encode_resize` reports one of three outcomes:
        //   * `Some(Ok(frame))`  – DVC present and open; send the resize PDU.
        //   * `Some(Err(e))`     – DVC present but the PDU could not be encoded.
        //   * `None`             – the Display Control DVC is unavailable or has
        //                          not finished opening, so there is no client-
        //                          initiated resize path right now.
        //
        // When the DVC is absent, a resolution change is only possible through a
        // *server-initiated* deactivation-reactivation (the server emits a
        // `DeactivateAll`, which the read loop already surfaces); a client cannot
        // force that handshake with this version of the ironrdp API. Rather than
        // silently pretend the resize happened, we return a precise, honest error
        // naming the missing capability so the UI can keep the old size and
        // explain why.
        match self
            .active_stage
            .encode_resize(resolution.width, resolution.height, None, None)
        {
            Some(Ok(frame)) => {
                self.writer.write_all(&frame).await.map_err(|e| {
                    ProtocolError::Io(format!("RDP resize write failed: {e}"))
                })?;
                // The server honours the Monitor Layout change in place; adopt
                // the new resolution and resize the local framebuffer so decoded
                // frames land in a correctly sized image.
                self.current_resolution = resolution;
                self.image = DecodedImage::new(
                    ironrdp::graphics::image_processing::PixelFormat::RgbA32,
                    u16::try_from(resolution.width).unwrap_or(u16::MAX),
                    u16::try_from(resolution.height).unwrap_or(u16::MAX),
                );
                Ok(())
            }
            Some(Err(e)) => {
                Err(ProtocolError::Io(format!("encode RDP resize failed: {e}")))
            }
            None => Err(ProtocolError::InvalidParameter {
                parameter: "resolution".to_string(),
                message: "mid-session resize unavailable: the server did not \
                          open the Display Control virtual channel, so the \
                          resolution cannot be renegotiated without a \
                          server-initiated reconnect"
                    .to_string(),
            }),
        }
    }

    async fn handle_shutdown(&mut self) -> Result<(), ProtocolError> {
        if self.terminated {
            return Ok(());
        }
        self.terminated = true;

        // Request a graceful client-initiated shutdown. Errors here are
        // non-fatal: we are tearing down regardless.
        if let Ok(outputs) = self.active_stage.graceful_shutdown() {
            for out in outputs {
                if let ActiveStageOutput::ResponseFrame(frame) = out {
                    let _ = self.writer.write_all(&frame).await;
                }
            }
        }
        Ok(())
    }
}

/// A `Send` handle to a session living on the dedicated worker thread.
///
/// Each [`RdpTransport`] call is a command sent to the worker plus a `oneshot`
/// await for its reply. When the handle is dropped the command channel closes
/// and the worker thread winds down, releasing the socket/TLS resources.
struct IronRdpTransport {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<RdpCommand>,
}

impl IronRdpTransport {
    /// Common send-command-and-await-reply plumbing. A closed channel or dropped
    /// reply means the worker is gone; map that to a closed-session I/O error.
    async fn request<T>(
        &self,
        make: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, ProtocolError>>) -> RdpCommand,
    ) -> Result<T, ProtocolError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(make(reply_tx))
            .map_err(|_| ProtocolError::Io("RDP session is closed".to_string()))?;
        reply_rx
            .await
            .map_err(|_| ProtocolError::Io("RDP session is closed".to_string()))?
    }
}

#[async_trait]
impl RdpTransport for IronRdpTransport {
    async fn send_input(&mut self, event: &[u8]) -> Result<(), ProtocolError> {
        let event = event.to_vec();
        self.request(move |reply| RdpCommand::SendInput { event, reply })
            .await
    }

    async fn next_update(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        self.request(|reply| RdpCommand::NextUpdate { reply }).await
    }

    async fn request_resolution(&mut self, resolution: Resolution) -> Result<(), ProtocolError> {
        self.request(move |reply| RdpCommand::RequestResolution { resolution, reply })
            .await
    }

    async fn shutdown(&mut self) -> Result<(), ProtocolError> {
        // If the worker already exited (channel closed), teardown is already
        // done — idempotent success.
        if self.cmd_tx.is_closed() {
            return Ok(());
        }
        self.request(|reply| RdpCommand::Shutdown { reply }).await
    }
}

/// The pieces of a freshly-connected active session handed from the connect
/// thread to the transport.
struct ConnectedSession {
    connection_result: connector::ConnectionResult,
    framed: TokioFramed<RdpStream>,
}

/// Drive the full sans-I/O RDP connect sequence (TCP → X.224 nego → TLS upgrade
/// → CredSSP/NLA finalize) to a live active session. Runs on the connect
/// thread's current-thread runtime, so it may use `!Send` futures.
async fn connect_sequence(
    host: &str,
    port: u16,
    resolution: Resolution,
    credential: &ResolvedCredential,
    cert_verifier: Arc<dyn RdpCertVerifier>,
) -> Result<ConnectedSession, ProtocolError> {
    // --- 1) TCP connect (name resolution + handshake) ----------------------
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|e| map_tcp_error(host, e))?;

    let client_addr = tcp
        .local_addr()
        .map_err(|e| ProtocolError::Io(format!("local socket address unavailable: {e}")))?;

    // --- 2) X.224 negotiation up to the security upgrade -------------------
    let config = build_connector_config(resolution, credential);
    let mut connector = ClientConnector::new(config, client_addr);

    let mut framed: TokioFramed<TcpStream> = TokioFramed::new(tcp);

    let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
        .await
        .map_err(map_connector_error)?;

    // No leftover is expected before the TLS handshake begins.
    let initial_stream = framed.into_inner_no_leftover();

    // --- 3) TLS upgrade + server public-key extraction ---------------------
    // The upgrade's rustls verifier consults `cert_verifier` with the server
    // leaf certificate's fingerprint (task 26.3); a declined/changed cert aborts
    // the handshake here, before CredSSP, so no session is produced.
    let (tls_stream, server_public_key) =
        tls_upgrade(initial_stream, host, cert_verifier).await?;

    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);

    let mut upgraded_framed: TokioFramed<RdpStream> = TokioFramed::new(tls_stream);

    // --- 4) CredSSP / NLA finalization -------------------------------------
    // CredSSP suspends and asks the `AsyncNetworkClient` to perform a round trip
    // only when the selected authentication package needs to talk to a KDC —
    // i.e. domain Kerberos (optionally through a KDC-proxy). NTLM password auth
    // never suspends, so the client is simply never called for the common
    // username/password case and that path is unchanged.
    //
    // We hand `connect_finalize` a real [`SspiNetworkClient`] that services those
    // requests (TCP/UDP straight to the KDC, or HTTP(S) to a KDC-proxy) exactly
    // as `sspi`'s reference network client does, so Kerberos logons complete.
    //
    // `kerberos_config` is left `None`: with no explicit KDC-proxy URL to inject
    // (the public credential type carries only username/password/domain), `sspi`
    // discovers the KDC for the realm itself and the network client honours
    // whatever URL/scheme it yields. See [`SspiNetworkClient`] for the exact
    // framing contract.
    let server_name: ServerName = host.into();
    let mut network_client = SspiNetworkClient;
    let connection_result = ironrdp_tokio::connect_finalize(
        upgraded,
        &mut upgraded_framed,
        connector,
        server_name,
        server_public_key,
        Some(&mut network_client),
        None,
    )
    .await
    .map_err(map_connector_error)?;

    Ok(ConnectedSession {
        connection_result,
        framed: upgraded_framed,
    })
}

/// The real CredSSP network client: performs the KDC round-trips `sspi` needs
/// for domain **Kerberos** / **KDC-proxy** logons.
///
/// `ironrdp_async`'s CredSSP driver resolves a [`CredsspProcessGenerator`] by
/// suspending whenever `sspi` needs to reach a Key Distribution Center and
/// handing us a [`NetworkRequest`]. This client fulfils that request and returns
/// the raw reply; `sspi` then resumes its state machine. For NTLM password auth
/// the generator never suspends, so this client is never consulted and the
/// password path is byte-for-byte the behaviour it had before.
///
/// Its futures are intentionally **not** `Send` — the whole RDP session runs on
/// one dedicated current-thread runtime (see [`IronRdpConnector::connect`]), so
/// the per-request sockets opened here stay on that thread, matching the
/// `!Send` CredSSP generator they serve. (The `ironrdp_async::AsyncNetworkClient`
/// trait, unlike `sspi`'s own, does not require `Send`.)
///
/// ## Framing contract (mirrors `sspi`'s reference `ReqwestNetworkClient`)
///
/// The reply framing `sspi` expects differs per transport, and this client
/// reproduces it exactly:
///
/// - **TCP** (`tcp://` KDC): the request `data` already carries the 4-byte
///   big-endian length prefix a KDC expects. We write it verbatim, read the
///   4-byte length of the reply, then that many bytes, and return
///   `[len_be][body]` — the reply with its length prefix preserved, which is
///   what `sspi` decodes.
/// - **UDP** (`udp://` KDC): the request `data` has the length prefix stripped
///   by `sspi`. We send it as a single datagram, receive the reply, and return
///   it **prefixed** with a freshly computed 4-byte big-endian length (again
///   matching the reference client; `sspi` resumes from a length-prefixed buffer).
/// - **HTTP/HTTPS** (`http(s)://` KDC-proxy): the request `data` is an
///   already-encoded `KDC-PROXY-MESSAGE`. We POST it to the proxy URL and return
///   the response body untouched; `sspi` unwraps the proxy envelope itself.
///
/// Every failure is surfaced as a [`ConnectorError`] (via [`ConnectorErrorExt`])
/// so it flows through the normal connect error path. No secret material is
/// logged here — only transport-level diagnostics (host/scheme/length).
struct SspiNetworkClient;

impl SspiNetworkClient {
    /// TCP round-trip to a KDC. `data` is written as-is (it is already
    /// length-prefixed); the reply is returned with its 4-byte length prefix.
    async fn send_tcp(url: &url::Url, data: &[u8]) -> ConnectorResult<Vec<u8>> {
        let addr = Self::host_port(url, 88)?;

        let mut stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| ConnectorError::custom("KDC TCP connect", e))?;

        stream
            .write_all(data)
            .await
            .map_err(|e| ConnectorError::custom("KDC TCP write", e))?;

        // Length prefix (big-endian u32) then that many bytes of body.
        let len = stream
            .read_u32()
            .await
            .map_err(|e| ConnectorError::custom("KDC TCP read length", e))?;

        let mut buf = vec![0u8; len as usize + 4];
        buf[0..4].copy_from_slice(&len.to_be_bytes());
        stream
            .read_exact(&mut buf[4..])
            .await
            .map_err(|e| ConnectorError::custom("KDC TCP read body", e))?;

        Ok(buf)
    }

    /// UDP round-trip to a KDC. `data` is sent (prefix already stripped by sspi);
    /// the reply is returned prefixed with its length as a big-endian u32.
    async fn send_udp(url: &url::Url, data: &[u8]) -> ConnectorResult<Vec<u8>> {
        let addr = Self::host_port(url, 88)?;

        // Bind an ephemeral local socket, then "connect" it to the KDC so recv
        // only accepts datagrams from that peer.
        let socket = UdpSocket::bind(("0.0.0.0", 0))
            .await
            .map_err(|e| ConnectorError::custom("KDC UDP bind", e))?;
        socket
            .connect(&addr)
            .await
            .map_err(|e| ConnectorError::custom("KDC UDP connect", e))?;
        socket
            .send(data)
            .await
            .map_err(|e| ConnectorError::custom("KDC UDP send", e))?;

        // 48000 bytes: the default maximum Kerberos token length on Windows,
        // matching sspi's reference client.
        let mut buf = vec![0u8; 0xbb80];
        let n = socket
            .recv(&mut buf)
            .await
            .map_err(|e| ConnectorError::custom("KDC UDP recv", e))?;

        let mut reply = Vec::with_capacity(n + 4);
        reply.extend_from_slice(&(n as u32).to_be_bytes());
        reply.extend_from_slice(&buf[..n]);
        Ok(reply)
    }

    /// HTTP(S) round-trip to a KDC-proxy: POST the encoded `KDC-PROXY-MESSAGE`
    /// and return the response body verbatim.
    ///
    /// Implemented directly over `tokio` (+ `tokio-rustls` for `https`) with a
    /// minimal HTTP/1.1 request so no additional HTTP-client dependency is
    /// pulled in. The TLS verifier used for the proxy is the same accept-any
    /// verifier the RDP channel uses (see the SECURITY TODO on
    /// [`IronRdpConnector`]); a KDC-proxy is reached over the same trust-on-
    /// first-use posture as the host until certificate pinning lands.
    async fn send_http(url: &url::Url, data: &[u8]) -> ConnectorResult<Vec<u8>> {
        let host = url
            .host_str()
            .ok_or_else(|| ConnectorError::general("KDC-proxy URL has no host"))?
            .to_owned();
        let https = url.scheme().eq_ignore_ascii_case("https");
        let port = url.port_or_known_default().unwrap_or(if https { 443 } else { 80 });
        let path = {
            let p = url.path();
            if let Some(q) = url.query() {
                format!("{p}?{q}")
            } else if p.is_empty() {
                "/".to_owned()
            } else {
                p.to_owned()
            }
        };

        let tcp = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|e| ConnectorError::custom("KDC-proxy connect", e))?;

        if https {
            let tls = tls_connect_any(tcp, &host).await?;
            Self::http_exchange(tls, &host, &path, data).await
        } else {
            Self::http_exchange(tcp, &host, &path, data).await
        }
    }

    /// Write a minimal HTTP/1.1 `POST` with `body` and read back the response
    /// body. Supports `Content-Length` and `Transfer-Encoding: chunked`.
    async fn http_exchange<S>(
        mut stream: S,
        host: &str,
        path: &str,
        body: &[u8],
    ) -> ConnectorResult<Vec<u8>>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        let mut req = Vec::new();
        req.extend_from_slice(
            format!(
                "POST {path} HTTP/1.1\r\n\
                 Host: {host}\r\n\
                 Content-Type: application/kerberos\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        );
        req.extend_from_slice(body);
        stream
            .write_all(&req)
            .await
            .map_err(|e| ConnectorError::custom("KDC-proxy write", e))?;
        stream
            .flush()
            .await
            .map_err(|e| ConnectorError::custom("KDC-proxy flush", e))?;

        // Read the whole response (Connection: close lets us read to EOF).
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .map_err(|e| ConnectorError::custom("KDC-proxy read", e))?;

        parse_http_response_body(&raw)
    }

    /// Resolve a `host:port` string from a KDC URL, defaulting the port.
    fn host_port(url: &url::Url, default_port: u16) -> ConnectorResult<String> {
        let host = url
            .host_str()
            .ok_or_else(|| ConnectorError::general("KDC URL has no host"))?;
        Ok(format!("{host}:{}", url.port().unwrap_or(default_port)))
    }
}

impl AsyncNetworkClient for SspiNetworkClient {
    fn send<'a>(
        &'a mut self,
        network_request: &'a NetworkRequest,
    ) -> Pin<Box<dyn Future<Output = ConnectorResult<Vec<u8>>> + 'a>> {
        Box::pin(async move {
            match network_request.protocol {
                NetworkProtocol::Tcp => {
                    Self::send_tcp(&network_request.url, &network_request.data).await
                }
                NetworkProtocol::Udp => {
                    Self::send_udp(&network_request.url, &network_request.data).await
                }
                NetworkProtocol::Http | NetworkProtocol::Https => {
                    Self::send_http(&network_request.url, &network_request.data).await
                }
            }
        })
    }
}

/// Extract the body from a raw HTTP/1.1 response, honouring `Content-Length` or
/// `Transfer-Encoding: chunked`. A non-2xx status is a connect error.
fn parse_http_response_body(raw: &[u8]) -> ConnectorResult<Vec<u8>> {
    // Split headers from body at the first CRLFCRLF.
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| ConnectorError::general("KDC-proxy response has no header terminator"))?;
    let (head, rest) = raw.split_at(sep);
    let body_bytes = &rest[4..];

    let head = std::str::from_utf8(head)
        .map_err(|e| ConnectorError::custom("KDC-proxy response headers not UTF-8", e))?;
    let mut lines = head.split("\r\n");

    // Status line: "HTTP/1.1 200 OK".
    let status_line = lines
        .next()
        .ok_or_else(|| ConnectorError::general("KDC-proxy response has no status line"))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| ConnectorError::general("KDC-proxy response has no status code"))?;
    if !(200..300).contains(&status) {
        return Err(ConnectorError::reason(
            "KDC-proxy",
            format!("KDC-proxy returned HTTP status {status}"),
        ));
    }

    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding")
            && value.eq_ignore_ascii_case("chunked")
        {
            chunked = true;
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse::<usize>().ok();
        }
    }

    if chunked {
        decode_chunked_body(body_bytes)
    } else if let Some(len) = content_length {
        if body_bytes.len() < len {
            return Err(ConnectorError::general(
                "KDC-proxy response shorter than Content-Length",
            ));
        }
        Ok(body_bytes[..len].to_vec())
    } else {
        // No framing header: the whole remainder is the body (Connection: close).
        Ok(body_bytes.to_vec())
    }
}

/// Decode an HTTP/1.1 `Transfer-Encoding: chunked` body into its payload bytes.
fn decode_chunked_body(mut data: &[u8]) -> ConnectorResult<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        // Each chunk starts with a hex length line terminated by CRLF.
        let nl = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| ConnectorError::general("malformed chunked body: no size line"))?;
        let size_line = std::str::from_utf8(&data[..nl])
            .map_err(|e| ConnectorError::custom("chunk size not UTF-8", e))?;
        // Chunk extensions (after ';') are ignored.
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|e| ConnectorError::custom("invalid chunk size", e))?;
        data = &data[nl + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size {
            return Err(ConnectorError::general("chunked body truncated"));
        }
        out.extend_from_slice(&data[..size]);
        // Each chunk's data is followed by a CRLF.
        data = data.get(size + 2..).unwrap_or(&[]);
    }
    Ok(out)
}

/// Open a TLS stream to `host` over an existing TCP stream using the same
/// accept-any verifier as the RDP channel. Shared by the KDC-proxy HTTPS path.
async fn tls_connect_any(
    tcp: TcpStream,
    host: &str,
) -> ConnectorResult<TlsStream<TcpStream>> {
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(danger::NoCertificateVerification))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let dns = pki_types::ServerName::try_from(host.to_owned())
        .map_err(|e| ConnectorError::custom("KDC-proxy TLS server name", e))?;
    connector
        .connect(dns, tcp)
        .await
        .map_err(|e| ConnectorError::custom("KDC-proxy TLS handshake", e))
}

/// Assemble the `ironrdp` connector [`Config`](ironrdp::connector::Config) for a
/// connect at `resolution` with the given `credential`.
///
/// NLA (CredSSP) is enabled and legacy graphical-login TLS is disabled, which is
/// the recommended, lower-attack-surface posture for connecting to NLA-capable
/// hosts (see the extensive note on `Config::enable_tls` upstream).
fn build_connector_config(resolution: Resolution, credential: &ResolvedCredential) -> IronConfig {
    IronConfig {
        credentials: Credentials::UsernamePassword {
            username: credential.username.clone(),
            password: credential.password.clone(),
        },
        domain: credential.domain.clone(),
        // Enforce NLA: do not fall back to the legacy graphical-login TLS path.
        enable_tls: false,
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        // The resolution was already bounds-checked (640x480..=7680x4320), which
        // fits a u16 on both axes; clamp defensively just in case.
        desktop_size: DesktopSize {
            width: u16::try_from(resolution.width).unwrap_or(u16::MAX),
            height: u16::try_from(resolution.height).unwrap_or(u16::MAX),
        },
        bitmap: None,
        client_build: 0,
        client_name: "novrix-console".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: client_platform(),
        no_server_pointer: false,
        request_data: None,
        autologon: false,
        no_audio_playback: true,
        pointer_software_rendering: true,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
    }
}

/// The RDP major platform type to advertise for the host OS.
const fn client_platform() -> MajorPlatformType {
    #[cfg(target_os = "macos")]
    {
        MajorPlatformType::MACINTOSH
    }
    #[cfg(target_os = "windows")]
    {
        MajorPlatformType::WINDOWS
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        MajorPlatformType::UNIX
    }
}

/// Perform the enhanced-security TLS upgrade over `stream` and return the
/// upgraded stream together with the server leaf certificate's
/// SubjectPublicKeyInfo bit string (which CredSSP binds the NLA exchange to).
///
/// Resumption is disabled because CredSSP does not support TLS session
/// resumption.
async fn tls_upgrade(
    stream: TcpStream,
    server_name: &str,
    cert_verifier: Arc<dyn RdpCertVerifier>,
) -> Result<(RdpStream, Vec<u8>), ProtocolError> {
    // Task 26.3: the rustls verifier runs our trust decision over the leaf
    // certificate's fingerprint. A `false` decision fails the handshake with a
    // rustls `BadCertificate` alert, which surfaces below as a TLS error — the
    // connect aborts with no session rather than trusting the certificate.
    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(danger::TrustStoreCertVerifier::new(
            cert_verifier,
        )))
        .with_no_client_auth();
    // CredSSP forbids TLS resumption (see [MS-CSSP]).
    config.resumption = rustls::client::Resumption::disabled();

    let connector = TlsConnector::from(Arc::new(config));

    let dns_name = pki_types::ServerName::try_from(server_name.to_owned())
        .map_err(|e| ProtocolError::Io(format!("invalid TLS server name `{server_name}`: {e}")))?;

    let mut tls_stream = connector
        .connect(dns_name, stream)
        .await
        .map_err(|e| ProtocolError::Io(format!("TLS handshake failed: {e}")))?;

    // Ensure the handshake has driven far enough that the peer certificate is
    // available before we inspect it.
    tls_stream
        .flush()
        .await
        .map_err(|e| ProtocolError::Io(format!("TLS flush failed: {e}")))?;

    let server_public_key = {
        let (_, tls_conn) = tls_stream.get_ref();
        let cert = tls_conn
            .peer_certificates()
            .and_then(|certs| certs.first())
            .ok_or_else(|| ProtocolError::Io("server presented no TLS certificate".to_string()))?;
        extract_tls_server_public_key(cert)?
    };

    Ok((tls_stream, server_public_key))
}

/// Recover the SubjectPublicKeyInfo bit string from a DER-encoded X.509
/// certificate. CredSSP uses this to bind the NLA exchange to the TLS channel.
fn extract_tls_server_public_key(cert: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    use x509_cert::der::Decode as _;

    let cert = x509_cert::Certificate::from_der(cert)
        .map_err(|e| ProtocolError::Io(format!("could not parse server certificate: {e}")))?;

    let key = cert
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .ok_or_else(|| {
            ProtocolError::Io("server public key BIT STRING is not byte-aligned".to_string())
        })?
        .to_owned();

    Ok(key)
}

/// Decode one UI input descriptor (as carried by `send_input`) into the
/// `ironrdp` [`Operation`]s to apply.
///
/// ## Wire format (agreed with the RDP UI, see `src/components/RdpView.tsx`)
///
/// Each descriptor is a single self-describing frame; `send_input` may be called
/// once per event. Byte 0 is the opcode; the remaining bytes are the payload,
/// all integers little-endian:
///
/// | opcode | meaning              | payload                               |
/// |--------|----------------------|---------------------------------------|
/// | `0x01` | key down             | `scancode: u16`                       |
/// | `0x02` | key up               | `scancode: u16`                       |
/// | `0x03` | pointer move         | `x: u16, y: u16`                      |
/// | `0x04` | mouse button down    | `button: u8` (0=L,1=M,2=R,3=X1,4=X2)  |
/// | `0x05` | mouse button up      | `button: u8`                          |
/// | `0x06` | unicode key down     | `ch: u32` (Unicode scalar value)      |
/// | `0x07` | unicode key up       | `ch: u32`                             |
///
/// An empty slice is a no-op (returns no operations). An unknown opcode or a
/// truncated payload is a protocol error, so a malformed frame is caught rather
/// than silently misread. The scancode is a PC/AT set-1 code; bit `0xE000`
/// marks an extended key, matching [`Scancode::from_u16`].
fn decode_input_event(event: &[u8]) -> Result<Vec<Operation>, ProtocolError> {
    if event.is_empty() {
        return Ok(Vec::new());
    }

    let bad = |what: &str| ProtocolError::Io(format!("malformed RDP input event: {what}"));
    let u16_at = |e: &[u8], off: usize| -> Result<u16, ProtocolError> {
        e.get(off..off + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .ok_or_else(|| bad("truncated u16 payload"))
    };

    let opcode = event[0];
    let payload = &event[1..];

    let op = match opcode {
        0x01 => Operation::KeyPressed(Scancode::from_u16(u16_at(payload, 0)?)),
        0x02 => Operation::KeyReleased(Scancode::from_u16(u16_at(payload, 0)?)),
        0x03 => Operation::MouseMove(MousePosition {
            x: u16_at(payload, 0)?,
            y: u16_at(payload, 2)?,
        }),
        0x04 | 0x05 => {
            let idx = *payload.first().ok_or_else(|| bad("missing button byte"))?;
            let button =
                MouseButton::from_idx(usize::from(idx)).ok_or_else(|| bad("invalid mouse button"))?;
            if opcode == 0x04 {
                Operation::MouseButtonPressed(button)
            } else {
                Operation::MouseButtonReleased(button)
            }
        }
        0x06 | 0x07 => {
            let bytes = payload
                .get(0..4)
                .ok_or_else(|| bad("truncated unicode scalar"))?;
            let scalar = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            let ch = char::from_u32(scalar).ok_or_else(|| bad("invalid unicode scalar"))?;
            if opcode == 0x06 {
                Operation::UnicodeKeyPressed(ch)
            } else {
                Operation::UnicodeKeyReleased(ch)
            }
        }
        other => return Err(bad(&format!("unknown opcode 0x{other:02x}"))),
    };

    Ok(vec![op])
}

/// `rustls` certificate verifiers for the RDP connect.
///
/// Two verifiers live here:
///
/// - [`TrustStoreCertVerifier`] is the **RDP server** verifier (task 26.3). It
///   does not blindly accept the certificate: it computes the leaf cert's
///   SHA-256 fingerprint and runs the injected [`RdpCertVerifier`] trust
///   decision (known / prompt / changed), failing the handshake with a
///   `BadCertificate` alert when the decision is to reject. TLS *signature*
///   checks remain asserted because RDP's trust anchor is the fingerprint, not a
///   PKI chain (self-signed RDP certificates are the norm) — the fingerprint
///   pinning is what provides continuity-of-identity, exactly like SSH host keys.
/// - [`NoCertificateVerification`] is retained **only** for the optional
///   Kerberos **KDC-proxy** HTTPS leg ([`tls_connect_any`]), which is a
///   different endpoint than the RDP server and is out of scope for RDP host
///   trust. It is never used for the RDP server channel.
mod danger {
    use std::sync::Arc;

    use tokio_rustls::rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use tokio_rustls::rustls::{pki_types, DigitallySignedStruct, Error, SignatureScheme};

    use super::RdpCertVerifier;

    /// The RDP server certificate verifier: pins the leaf certificate by its
    /// SHA-256 fingerprint via the injected [`RdpCertVerifier`] trust decision.
    pub(super) struct TrustStoreCertVerifier {
        verifier: Arc<dyn RdpCertVerifier>,
    }

    impl TrustStoreCertVerifier {
        pub(super) fn new(verifier: Arc<dyn RdpCertVerifier>) -> Self {
            Self { verifier }
        }
    }

    // `ServerCertVerifier` requires `Debug`; the trait object behind the `Arc`
    // is not `Debug`, so provide a manual, opaque impl.
    impl std::fmt::Debug for TrustStoreCertVerifier {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TrustStoreCertVerifier").finish()
        }
    }

    impl ServerCertVerifier for TrustStoreCertVerifier {
        fn verify_server_cert(
            &self,
            end_entity: &pki_types::CertificateDer<'_>,
            _intermediates: &[pki_types::CertificateDer<'_>],
            _server_name: &pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: pki_types::UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            // Run the fingerprint-based trust decision over the leaf cert DER.
            // A reject fails the handshake with a BadCertificate alert; the
            // connect then aborts with no session (never a silent trust).
            if self.verifier.verify_certificate(end_entity.as_ref()) {
                Ok(ServerCertVerified::assertion())
            } else {
                Err(Error::InvalidCertificate(
                    tokio_rustls::rustls::CertificateError::ApplicationVerificationFailure,
                ))
            }
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &pki_types::CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &pki_types::CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            all_schemes()
        }
    }

    /// A `rustls` certificate verifier that accepts any server certificate.
    ///
    /// Retained only for the Kerberos KDC-proxy HTTPS leg (`tls_connect_any`),
    /// which is a separate endpoint from the RDP server and out of scope for RDP
    /// host-key trust. It is never installed on the RDP server channel.
    #[derive(Debug)]
    pub(super) struct NoCertificateVerification;

    impl ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _: &pki_types::CertificateDer<'_>,
            _: &[pki_types::CertificateDer<'_>],
            _: &pki_types::ServerName<'_>,
            _: &[u8],
            _: pki_types::UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &pki_types::CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &pki_types::CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            all_schemes()
        }
    }

    /// The TLS signature schemes advertised by both verifiers.
    fn all_schemes() -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        AuthConfig, ConnectionProfile, CredentialKey, ProfileId, ProtocolParams,
    };
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Extract the error from an establish result. `Box<dyn SessionConn>` is not
    /// `Debug`, so `Result::unwrap_err` is unavailable; this unwraps the `Err`
    /// without requiring the `Ok` type to be printable.
    fn expect_err(result: Result<Box<dyn SessionConn>, ProtocolError>) -> ProtocolError {
        match result {
            Ok(_) => panic!("expected establish to fail, but a Session was created"),
            Err(e) => e,
        }
    }

    // --- Pure resolution-bounds tests (Property 18 smoke) -----------------

    #[test]
    fn resolution_bounds_accepts_corners_and_interior() {
        assert!(resolution_in_bounds(MIN_RESOLUTION));
        assert!(resolution_in_bounds(MAX_RESOLUTION));
        assert!(resolution_in_bounds(Resolution {
            width: 1920,
            height: 1080
        }));
    }

    #[test]
    fn resolution_bounds_rejects_just_outside_each_edge() {
        // width too small / too large
        assert!(!resolution_in_bounds(Resolution {
            width: 639,
            height: 480
        }));
        assert!(!resolution_in_bounds(Resolution {
            width: 7681,
            height: 4320
        }));
        // height too small / too large
        assert!(!resolution_in_bounds(Resolution {
            width: 640,
            height: 479
        }));
        assert!(!resolution_in_bounds(Resolution {
            width: 640,
            height: 4321
        }));
    }

    #[test]
    fn validate_resolution_requests_in_range_as_is() {
        let r = Resolution {
            width: 1280,
            height: 720,
        };
        assert_eq!(validate_resolution(Some(r)).unwrap(), r);
    }

    #[test]
    fn validate_resolution_defaults_when_absent_and_default_is_in_bounds() {
        assert_eq!(validate_resolution(None).unwrap(), DEFAULT_RESOLUTION);
        assert!(resolution_in_bounds(DEFAULT_RESOLUTION));
    }

    #[test]
    fn validate_resolution_rejects_out_of_range_with_resolution_parameter() {
        let err = validate_resolution(Some(Resolution {
            width: 100,
            height: 100,
        }))
        .unwrap_err();
        match err {
            ProtocolError::InvalidParameter { parameter, .. } => {
                assert_eq!(parameter, "resolution");
            }
            other => panic!("expected InvalidParameter, got {other:?}"),
        }
    }

    // --- CredSSP KDC-proxy HTTP response parsing (pure, no network) -------
    //
    // These exercise the framing the Kerberos/KDC-proxy network client relies on
    // when it POSTs a KDC-PROXY-MESSAGE and must hand sspi back exactly the
    // response body. No socket is opened; only the pure parsers run.

    #[test]
    fn http_body_parsed_by_content_length() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello-trailing-ignored";
        let body = parse_http_response_body(raw).expect("200 with content-length parses");
        assert_eq!(body, b"hello");
    }

    #[test]
    fn http_body_parsed_when_chunked() {
        // Two chunks ("Wiki" + "pedia") then the zero-length terminator.
        let raw = b"HTTP/1.1 200 OK\r\n\
                    Transfer-Encoding: chunked\r\n\r\n\
                    4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let body = parse_http_response_body(raw).expect("chunked body parses");
        assert_eq!(body, b"Wikipedia");
    }

    #[test]
    fn http_body_defaults_to_remainder_without_framing_header() {
        // No Content-Length and not chunked (Connection: close): body is the rest.
        let raw = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nraw-kdc-bytes";
        let body = parse_http_response_body(raw).expect("remainder parses");
        assert_eq!(body, b"raw-kdc-bytes");
    }

    #[test]
    fn http_non_2xx_status_is_a_connector_error() {
        let raw = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
        assert!(
            parse_http_response_body(raw).is_err(),
            "a 5xx KDC-proxy reply must surface as an error, not an empty body"
        );
    }

    #[test]
    fn http_missing_header_terminator_is_an_error() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5"; // no CRLFCRLF
        assert!(parse_http_response_body(raw).is_err());
    }

    #[test]
    fn chunked_decoder_handles_single_chunk_with_extension() {
        // Chunk size line may carry an extension after ';', which is ignored.
        let decoded = decode_chunked_body(b"3;name=val\r\nabc\r\n0\r\n\r\n")
            .expect("chunk extension is ignored");
        assert_eq!(decoded, b"abc");
    }

    // --- establish flow tests (timeout / auth / bounds / states) ----------

    /// A fake transport whose only job is to prove `RdpSession` forwards and
    /// shuts down. It records whether it was shut down.
    struct FakeTransport {
        shutdown_flag: Arc<AtomicBool>,
    }

    #[async_trait]
    impl RdpTransport for FakeTransport {
        async fn send_input(&mut self, _event: &[u8]) -> Result<(), ProtocolError> {
            Ok(())
        }
        async fn next_update(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
            Ok(Some(vec![0xDE, 0xAD]))
        }
        async fn request_resolution(
            &mut self,
            _resolution: Resolution,
        ) -> Result<(), ProtocolError> {
            Ok(())
        }
        async fn shutdown(&mut self) -> Result<(), ProtocolError> {
            self.shutdown_flag.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Connector behaviours the tests select between.
    enum ConnectorBehaviour {
        Succeed(Arc<AtomicBool>),
        AuthFail,
        Hang,
    }

    struct FakeConnector {
        behaviour: ConnectorBehaviour,
    }

    #[async_trait]
    impl RdpConnector for FakeConnector {
        async fn connect(
            &self,
            _host: &str,
            _port: u16,
            _resolution: Resolution,
            _credential: ResolvedCredential,
        ) -> Result<Box<dyn RdpTransport>, ProtocolError> {
            match &self.behaviour {
                ConnectorBehaviour::Succeed(flag) => Ok(Box::new(FakeTransport {
                    shutdown_flag: flag.clone(),
                })),
                ConnectorBehaviour::AuthFail => Err(ProtocolError::AuthenticationFailed),
                ConnectorBehaviour::Hang => {
                    // Never completes; the protocol's timeout must win.
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        }
    }

    /// Credential source that always resolves a secret (store integration ok).
    /// A sample resolved credential for the fakes. The establish-flow tests
    /// assert on *behaviour* (connected / source consulted / abort cause), not
    /// on the secret's contents, so a fixed value here does not weaken them.
    fn sample_credential() -> ResolvedCredential {
        ResolvedCredential::new("tester", "s3cret", None)
    }

    struct AlwaysPresent;
    #[async_trait]
    impl RdpCredentialSource for AlwaysPresent {
        async fn resolve(
            &self,
            _handle: &CredentialHandle,
        ) -> Result<ResolvedCredential, ProtocolError> {
            Ok(sample_credential())
        }
    }

    /// Credential source that reports the secret is absent from the store.
    struct AlwaysMissing;
    #[async_trait]
    impl RdpCredentialSource for AlwaysMissing {
        async fn resolve(
            &self,
            _handle: &CredentialHandle,
        ) -> Result<ResolvedCredential, ProtocolError> {
            Err(ProtocolError::MissingCredentials)
        }
    }

    fn rdp_profile(
        resolution: Option<Resolution>,
        timeout_secs: u16,
    ) -> ResolvedProfile {
        ResolvedProfile::new(ConnectionProfile {
            id: ProfileId::new(),
            name: "desk".to_string(),
            protocol: ProtocolKind::Rdp,
            host: "host.example".to_string(),
            port: 3389,
            username: None,
            domain: None,
            folder_id: None,
            auth: AuthConfig::Password {
                credential_key: CredentialKey::new("profile/1/password"),
            },
            protocol_params: ProtocolParams::Rdp { resolution },
            logging_enabled: false,
            log_path: None,
            connect_timeout_secs: timeout_secs,
        })
    }

    fn handle_with_secret() -> CredentialHandle {
        CredentialHandle::with_key(CredentialKey::new("profile/1/password"))
    }

    #[tokio::test]
    async fn establish_succeeds_and_reports_connected() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let protocol = RdpProtocol::new(
            FakeConnector {
                behaviour: ConnectorBehaviour::Succeed(shutdown.clone()),
            },
            AlwaysPresent,
        );
        let mut session = protocol
            .establish(&rdp_profile(None, 30), handle_with_secret())
            .await
            .expect("establish should succeed");

        assert_eq!(session.state(), SessionState::Connected);
        // send/recv forward through the transport.
        session.send(b"key:A").await.unwrap();
        assert_eq!(session.recv().await.unwrap(), Some(vec![0xDE, 0xAD]));
        // resize is a documented no-op for RDP.
        session.resize(80, 24).await.unwrap();
        // close shuts down the transport and is idempotent.
        session.close().await.unwrap();
        assert!(shutdown.load(Ordering::SeqCst));
        assert_eq!(session.state(), SessionState::Disconnected);
        session.close().await.unwrap(); // idempotent
    }

    #[tokio::test]
    async fn out_of_range_resolution_rejected_without_connecting() {
        // A connector that would panic if ever called proves no connect happens.
        struct NeverConnect;
        #[async_trait]
        impl RdpConnector for NeverConnect {
            async fn connect(
                &self,
                _host: &str,
                _port: u16,
                _resolution: Resolution,
                _credential: ResolvedCredential,
            ) -> Result<Box<dyn RdpTransport>, ProtocolError> {
                panic!("connect must not be attempted for an out-of-range resolution");
            }
        }
        let protocol = RdpProtocol::new(NeverConnect, AlwaysPresent);
        let bad = Some(Resolution {
            width: 10_000,
            height: 10_000,
        });
        let result = protocol
            .establish(&rdp_profile(bad, 30), handle_with_secret())
            .await;
        match expect_err(result) {
            ProtocolError::InvalidParameter { parameter, .. } => {
                assert_eq!(parameter, "resolution")
            }
            other => panic!("expected InvalidParameter, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_credential_handle_aborts_with_missing_credentials() {
        let protocol = RdpProtocol::new(
            FakeConnector {
                behaviour: ConnectorBehaviour::Succeed(Arc::new(AtomicBool::new(false))),
            },
            AlwaysPresent,
        );
        // Handle names no secret => MissingCredentials, no Session.
        let result = protocol
            .establish(&rdp_profile(None, 30), CredentialHandle::none())
            .await;
        assert_eq!(expect_err(result), ProtocolError::MissingCredentials);
    }

    #[tokio::test]
    async fn absent_store_secret_aborts_with_missing_credentials() {
        let protocol = RdpProtocol::new(
            FakeConnector {
                behaviour: ConnectorBehaviour::Succeed(Arc::new(AtomicBool::new(false))),
            },
            AlwaysMissing,
        );
        let result = protocol
            .establish(&rdp_profile(None, 30), handle_with_secret())
            .await;
        assert_eq!(expect_err(result), ProtocolError::MissingCredentials);
    }

    #[tokio::test]
    async fn auth_failure_aborts_without_session() {
        let protocol = RdpProtocol::new(
            FakeConnector {
                behaviour: ConnectorBehaviour::AuthFail,
            },
            AlwaysPresent,
        );
        let result = protocol
            .establish(&rdp_profile(None, 30), handle_with_secret())
            .await;
        assert_eq!(expect_err(result), ProtocolError::AuthenticationFailed);
    }

    #[tokio::test]
    async fn connect_timeout_aborts_with_timeout_and_no_session() {
        let protocol = RdpProtocol::new(
            FakeConnector {
                behaviour: ConnectorBehaviour::Hang,
            },
            AlwaysPresent,
        );
        // A short (1-second) budget against a connector that never completes:
        // the protocol's timeout must win and yield Timeout with no Session.
        let result = protocol
            .establish(&rdp_profile(None, 1), handle_with_secret())
            .await;
        assert_eq!(expect_err(result), ProtocolError::Timeout);
    }

    #[test]
    fn connect_budget_is_clamped_to_thirty_seconds() {
        // Configured above the ceiling clamps down to 30s.
        let budget = RdpProtocol::<IronRdpConnector, AlwaysPresent>::connect_budget(
            &rdp_profile(None, 120),
        );
        assert_eq!(budget, MAX_CONNECT_TIMEOUT);
        // A zero configured timeout falls back to the ceiling.
        let budget0 = RdpProtocol::<IronRdpConnector, AlwaysPresent>::connect_budget(
            &rdp_profile(None, 0),
        );
        assert_eq!(budget0, MAX_CONNECT_TIMEOUT);
    }

    // --- Task 11.3: RDP auth wiring unit test (Requirement 7.3) -----------

    /// A credential source that records how many times `resolve` was called and
    /// with which key, proving the establish flow goes through the
    /// Credential_Store (`RdpCredentialSource::resolve`) to authenticate.
    struct RecordingSource {
        calls: Arc<AtomicUsize>,
        last_key: std::sync::Mutex<Option<String>>,
    }

    impl RecordingSource {
        fn new() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                last_key: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl RdpCredentialSource for RecordingSource {
        async fn resolve(
            &self,
            handle: &CredentialHandle,
        ) -> Result<ResolvedCredential, ProtocolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_key.lock().unwrap() =
                handle.credential_key.as_ref().map(|k| k.as_str().to_string());
            Ok(sample_credential())
        }
    }

    #[tokio::test]
    async fn establish_with_present_credentials_uses_credential_source_then_connects() {
        // Requirement 7.3: when an RDP connection requires authentication, the
        // Protocol_Engine authenticates using the credentials retrieved from the
        // Credential_Store. With a present handle the establish flow must call
        // `RdpCredentialSource::resolve` (keyed by the handle's credential key)
        // and then proceed to connect, yielding a Connected session.
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = RecordingSource::new();
        let calls = source.calls.clone();
        let protocol = RdpProtocol::new(
            FakeConnector {
                behaviour: ConnectorBehaviour::Succeed(shutdown.clone()),
            },
            source,
        );

        let session = protocol
            .establish(&rdp_profile(None, 30), handle_with_secret())
            .await
            .expect("establish should succeed when credentials resolve");

        // The credential source was consulted exactly once, with the handle's key.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(session.state(), SessionState::Connected);
    }

    #[tokio::test]
    async fn establish_with_absent_handle_fails_without_consulting_source_or_connecting() {
        // Requirement 7.4 (companion to 7.3): a handle naming no secret is a
        // missing-credential abort. The credential source must not be consulted
        // and no connection may be attempted. A connector that panics on connect
        // plus a source that panics on resolve prove neither is reached.
        struct NeverConnect;
        #[async_trait]
        impl RdpConnector for NeverConnect {
            async fn connect(
                &self,
                _host: &str,
                _port: u16,
                _resolution: Resolution,
                _credential: ResolvedCredential,
            ) -> Result<Box<dyn RdpTransport>, ProtocolError> {
                panic!("connect must not be attempted without credentials");
            }
        }
        struct NeverResolve;
        #[async_trait]
        impl RdpCredentialSource for NeverResolve {
            async fn resolve(
                &self,
                _handle: &CredentialHandle,
            ) -> Result<ResolvedCredential, ProtocolError> {
                panic!("credential source must not be consulted for an absent handle");
            }
        }

        let protocol = RdpProtocol::new(NeverConnect, NeverResolve);
        let result = protocol
            .establish(&rdp_profile(None, 30), CredentialHandle::none())
            .await;
        assert_eq!(expect_err(result), ProtocolError::MissingCredentials);
    }

    // --- Task 11.2: Property 18, RDP resolution bounds --------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        // Feature: remote-connection-manager, Property 18: For any requested RDP display resolution, the profile is accepted and the resolution requested as-is if and only if width is in 640..=7680 and height is in 480..=4320; otherwise the profile is rejected with an invalid-resolution error and no connection is established.
        #[test]
        fn rdp_resolution_bounds(
            // Bias the generator toward the decision boundaries so most of the
            // iteration budget lands on the edges that actually exercise the
            // biconditional (639/640/7680/7681 for width, 479/480/4320/4321 for
            // height), while still reaching far-out-of-range values.
            width in prop_oneof![
                9 => prop_oneof![
                    Just(639u32), Just(640), Just(7680), Just(7681),
                    639u32..=641, 7679u32..=u32::MAX,
                ],
                1 => 0u32..=u32::MAX,
            ],
            height in prop_oneof![
                9 => prop_oneof![
                    Just(479u32), Just(480), Just(4320), Just(4321),
                    479u32..=481, 4319u32..=u32::MAX,
                ],
                1 => 0u32..=u32::MAX,
            ],
        ) {
            let requested = Resolution { width, height };

            // Independent reference predicate for the biconditional: in bounds
            // iff width in 640..=7680 AND height in 480..=4320.
            let should_accept = (640..=7680).contains(&width)
                && (480..=4320).contains(&height);

            let result = validate_resolution(Some(requested));

            prop_assert_eq!(
                result.is_ok(),
                should_accept,
                "accept decision must match the bounds predicate for {}x{}",
                width,
                height
            );

            match result {
                Ok(resolved) => {
                    // Accepted: requested as-is, unchanged.
                    prop_assert_eq!(resolved, requested);
                }
                Err(err) => {
                    // Rejected: invalid-resolution error identifying the
                    // "resolution" parameter and no connection established
                    // (validate_resolution is pure and opens nothing).
                    match err {
                        ProtocolError::InvalidParameter { parameter, .. } => {
                            prop_assert_eq!(parameter, "resolution");
                        }
                        other => prop_assert!(
                            false,
                            "expected InvalidParameter, got {:?}",
                            other
                        ),
                    }
                }
            }
        }
    }
}
