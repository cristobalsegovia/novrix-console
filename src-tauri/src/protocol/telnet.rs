//! Telnet protocol implementation (Requirement 5).
//!
//! Telnet is the simplest of the Supported_Protocols: a plain TCP byte stream
//! with an optional in-band option-negotiation sublanguage (IAC commands). This
//! module implements the uniform [`Protocol`]/[`SessionConn`] interface from
//! [`super::interface`] on top of a [`tokio::net::TcpStream`], so no extra crate
//! is needed.
//!
//! Scope of this task (9.1):
//! - [`TelnetProtocol`] — a [`Protocol`] factory for [`ProtocolKind::Telnet`].
//! - [`TelnetSession`] — a live [`SessionConn`] over a connected `TcpStream`.
//! - [`resolve_timeout`] — the pure, testable timeout-resolution rule the
//!   design calls out as Property 16.
//!
//! `establish` validates the port, connects with a bounded timeout, and maps
//! each failure mode onto the matching [`ProtocolError`]:
//! - port outside `1..=65535` -> [`ProtocolError::InvalidParameter`] (req 5.5),
//!   rejected *before* any socket is opened;
//! - the attempt exceeds the resolved timeout -> [`ProtocolError::Timeout`]
//!   (req 5.3);
//! - the host refuses the connection -> [`ProtocolError::ConnectionRefused`]
//!   (req 5.4);
//! - the host name cannot be resolved -> [`ProtocolError::HostUnresolved`]
//!   (req 5.4).
//!
//! On every failure path the partially-created socket is dropped before
//! returning `Err`, so no half-open connection is left behind (Requirement
//! 8.3). On success the session starts in [`SessionState::Connected`] and
//! [`recv`](SessionConn::recv) yields the remote host's output (req 5.1, 5.2).
//!
//! ### Telnet (IAC) negotiation — scope
//!
//! This task does the *minimal* thing required to not corrupt the stream: when
//! the server sends option-negotiation commands (`IAC DO`/`DONT`/`WILL`/
//! `WONT`), we refuse every option — answering `DO`/`DONT` with `WONT` and
//! `WILL`/`WONT` with `DONT` — and we strip those command bytes out of the
//! data handed to the caller. A literal `0xFF` data byte arrives as the escape
//! pair `IAC IAC` and is unescaped back to a single `0xFF`. Full option
//! negotiation (NAWS window size, terminal type, binary mode, etc.) is
//! intentionally out of scope; [`resize`](SessionConn::resize) is therefore a
//! documented no-op (see its doc comment).

use async_trait::async_trait;
use std::io;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::interface::{CredentialHandle, Protocol, ResolvedProfile, SessionConn};
use crate::domain::{ProtocolError, ProtocolKind, SessionState};

/// The default connection timeout, in seconds, used when the profile's
/// configured value is outside the allowed `1..=120` range (req 5.3).
const DEFAULT_TIMEOUT_SECS: u16 = 30;

/// The inclusive lower/upper bounds of a valid connection timeout, in seconds
/// (req 5.3).
const MIN_TIMEOUT_SECS: u16 = 1;
const MAX_TIMEOUT_SECS: u16 = 120;

// --- Telnet protocol bytes (RFC 854) --------------------------------------

/// Interpret As Command — the telnet escape/command-introducer byte.
const IAC: u8 = 255;
/// "Server, please stop doing option X" / we answer options we won't do.
const DONT: u8 = 254;
/// "Server, please start doing option X."
const DO: u8 = 253;
/// "Server will do option X."
const WILL: u8 = 251;
/// "Server won't do option X."
const WONT: u8 = 252;

/// Resolve the effective connection timeout for a Telnet attempt (Property 16).
///
/// Pure and total: the resolved timeout is the configured
/// `connect_timeout_secs` when it lies in `1..=120`, and
/// [`DEFAULT_TIMEOUT_SECS`] (30) otherwise. The returned value is therefore
/// always within `1..=120` seconds, which is exactly the invariant Property 16
/// asserts. Keeping this as a free function (rather than inlining it in
/// [`TelnetProtocol::establish`]) is what makes the invariant unit-testable
/// without opening a socket.
pub fn resolve_timeout(connect_timeout_secs: u16) -> u16 {
    if (MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&connect_timeout_secs) {
        connect_timeout_secs
    } else {
        DEFAULT_TIMEOUT_SECS
    }
}

/// The Telnet [`Protocol`] factory (`ProtocolKind::Telnet`).
///
/// Stateless: it only knows how to validate a target and open a session. All
/// per-connection state lives in the [`TelnetSession`] it returns.
#[derive(Debug, Default, Clone, Copy)]
pub struct TelnetProtocol;

impl TelnetProtocol {
    /// A new Telnet protocol factory.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Protocol for TelnetProtocol {
    fn kind(&self) -> ProtocolKind {
        ProtocolKind::Telnet
    }

    async fn establish(
        &self,
        cfg: &ResolvedProfile,
        _creds: CredentialHandle,
    ) -> Result<Box<dyn SessionConn>, ProtocolError> {
        let profile = &cfg.profile;

        // (req 5.5) Reject an out-of-range port before touching the network.
        // `port` is a `u16`, so the only invalid value representable here is 0;
        // the check is written against the full documented range so it reads as
        // the spec does and stays correct if the field widens later.
        let port = profile.port;
        if !(1..=65535).contains(&(port as u32)) {
            return Err(ProtocolError::InvalidParameter {
                parameter: "port".to_string(),
                message: format!("Port must be between 1 and 65535, got {port}."),
            });
        }

        let host = profile.host.trim();
        if host.is_empty() {
            return Err(ProtocolError::InvalidParameter {
                parameter: "host".to_string(),
                message: "Host is required for a Telnet connection.".to_string(),
            });
        }

        let timeout_secs = resolve_timeout(profile.connect_timeout_secs);
        let timeout = Duration::from_secs(timeout_secs as u64);

        // (req 5.1) Connect to the configured host:port, bounded by the resolved
        // timeout. `tokio::time::timeout` wrapping `TcpStream::connect` gives us
        // all three outcomes we must distinguish:
        //   - Err(elapsed)        -> the attempt exceeded the timeout (req 5.3)
        //   - Ok(Err(io_error))   -> refused / unresolved / other I/O (req 5.4)
        //   - Ok(Ok(stream))      -> connected (req 5.2)
        // On any error path the (possibly half-open) socket is dropped here, so
        // nothing is leaked (Requirement 8.3).
        let connect = TcpStream::connect((host, port));
        let stream = match tokio::time::timeout(timeout, connect).await {
            Err(_elapsed) => return Err(ProtocolError::Timeout),
            Ok(Err(io_err)) => return Err(map_connect_error(io_err, host)),
            Ok(Ok(stream)) => stream,
        };

        // (req 5.2) Session is live and connected; its reads surface host output.
        Ok(Box::new(TelnetSession::new(stream)))
    }
}

/// Map a connect-time [`io::Error`] onto the matching [`ProtocolError`] (req 5.4).
///
/// `ConnectionRefused` is reported verbatim. DNS resolution failures surface as
/// [`io::ErrorKind::NotFound`] / `Uncategorized` from the resolver; those are
/// reported as [`ProtocolError::HostUnresolved`]. Everything else is a generic
/// transport failure that still terminates the attempt with the Session left
/// disconnected.
fn map_connect_error(err: io::Error, host: &str) -> ProtocolError {
    match err.kind() {
        io::ErrorKind::ConnectionRefused => ProtocolError::ConnectionRefused,
        io::ErrorKind::NotFound | io::ErrorKind::AddrNotAvailable => {
            ProtocolError::HostUnresolved(host.to_string())
        }
        // The std resolver reports "failed to lookup address information" as an
        // uncategorized error on most platforms; treat a lookup-shaped message
        // as an unresolved host so the user gets the actionable error (req 5.4).
        _ if is_resolution_failure(&err) => ProtocolError::HostUnresolved(host.to_string()),
        _ => ProtocolError::Io(err.to_string()),
    }
}

/// Heuristic: does this I/O error look like a name-resolution failure?
///
/// `std`'s getaddrinfo errors are not given a dedicated [`io::ErrorKind`] on
/// most platforms, so we fall back to matching the message. This only runs
/// after the typed-kind checks in [`map_connect_error`], so it never shadows a
/// precise classification.
fn is_resolution_failure(err: &io::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("lookup address")
        || msg.contains("name or service not known")
        || msg.contains("nodename nor servname")
        || msg.contains("failed to lookup")
}

/// A live Telnet session over a connected [`TcpStream`] (req 5.2).
///
/// Owns the socket; dropping or [`close`](SessionConn::close)-ing it releases
/// the connection. Tracks its own [`SessionState`] so callers can observe the
/// transition to `Disconnected` on clean EOF or close.
pub struct TelnetSession {
    /// The connected transport, or `None` once closed.
    stream: Option<TcpStream>,
    /// Current connection state (`Connected` until EOF or `close`).
    state: SessionState,
    /// Carries a pending `IAC` seen at the end of a read chunk into the next
    /// read, so command sequences that straddle a buffer boundary are still
    /// parsed correctly.
    pending_iac: Vec<u8>,
}

impl TelnetSession {
    /// Wrap a freshly connected stream as a `Connected` session.
    fn new(stream: TcpStream) -> Self {
        Self {
            stream: Some(stream),
            state: SessionState::Connected,
            pending_iac: Vec::new(),
        }
    }

    /// Process one raw read buffer: strip/answer IAC negotiation and unescape
    /// `IAC IAC` into a literal `0xFF`, returning the application data and any
    /// bytes that must be written back to the server (option refusals).
    ///
    /// `self.pending_iac` holds a partial command carried over from a previous
    /// read; it is consumed/extended here so commands split across reads parse
    /// correctly.
    fn process_incoming(&mut self, raw: &[u8]) -> (Vec<u8>, Vec<u8>) {
        // Prepend any partial command bytes left over from the last read.
        let mut buf = std::mem::take(&mut self.pending_iac);
        buf.extend_from_slice(raw);

        let mut data = Vec::with_capacity(buf.len());
        let mut reply = Vec::new();
        let mut i = 0;
        while i < buf.len() {
            if buf[i] != IAC {
                data.push(buf[i]);
                i += 1;
                continue;
            }
            // buf[i] == IAC. Need at least one more byte to know the command.
            if i + 1 >= buf.len() {
                self.pending_iac.push(IAC);
                break;
            }
            let cmd = buf[i + 1];
            match cmd {
                // Escaped literal 0xFF data byte.
                IAC => {
                    data.push(IAC);
                    i += 2;
                }
                // Option negotiation: DO/DONT/WILL/WONT are 3-byte commands
                // (IAC, cmd, option). Refuse every option to keep a clean
                // line-mode stream.
                DO | DONT | WILL | WONT => {
                    if i + 2 >= buf.len() {
                        // Command's option byte hasn't arrived yet; carry over.
                        self.pending_iac.extend_from_slice(&buf[i..]);
                        break;
                    }
                    let option = buf[i + 2];
                    match cmd {
                        DO | DONT => {
                            reply.extend_from_slice(&[IAC, WONT, option]);
                        }
                        WILL | WONT => {
                            reply.extend_from_slice(&[IAC, DONT, option]);
                        }
                        _ => unreachable!(),
                    }
                    i += 3;
                }
                // Any other 2-byte command (NOP, DM, GA, ...): drop it silently.
                _ => {
                    i += 2;
                }
            }
        }
        (data, reply)
    }
}

#[async_trait]
impl SessionConn for TelnetSession {
    async fn send(&mut self, data: &[u8]) -> Result<(), ProtocolError> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| ProtocolError::Io("session is closed".to_string()))?;

        // Escape any literal 0xFF byte as `IAC IAC` so user data is never
        // mistaken for a command (RFC 854). Most keystrokes contain no 0xFF, so
        // the common path writes the slice unchanged.
        if data.contains(&IAC) {
            let mut escaped = Vec::with_capacity(data.len() + 1);
            for &b in data {
                escaped.push(b);
                if b == IAC {
                    escaped.push(IAC);
                }
            }
            stream.write_all(&escaped).await
        } else {
            stream.write_all(data).await
        }
        .map_err(|e| ProtocolError::Io(e.to_string()))?;
        stream
            .flush()
            .await
            .map_err(|e| ProtocolError::Io(e.to_string()))
    }

    async fn recv(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        loop {
            let stream = match self.stream.as_mut() {
                Some(s) => s,
                None => return Ok(None),
            };

            let mut buf = [0u8; 8192];
            let n = stream
                .read(&mut buf)
                .await
                .map_err(|e| ProtocolError::Io(e.to_string()))?;

            // Clean EOF: the remote closed the connection.
            if n == 0 {
                self.state = SessionState::Disconnected;
                self.stream = None;
                return Ok(None);
            }

            let (data, reply) = self.process_incoming(&buf[..n]);

            // Answer any option negotiation so the server stops waiting on us.
            if !reply.is_empty() {
                if let Some(stream) = self.stream.as_mut() {
                    stream
                        .write_all(&reply)
                        .await
                        .map_err(|e| ProtocolError::Io(e.to_string()))?;
                    stream
                        .flush()
                        .await
                        .map_err(|e| ProtocolError::Io(e.to_string()))?;
                }
            }

            // The chunk may have been pure negotiation; loop until there is
            // application output (or EOF) to hand back.
            if !data.is_empty() {
                return Ok(Some(data));
            }
        }
    }

    /// No-op for Telnet (documented).
    ///
    /// Conveying a terminal size to a Telnet server requires the NAWS option
    /// (RFC 1073), which is part of full option negotiation and out of scope for
    /// this task. A session with no NAWS simply uses the server's default
    /// geometry, so reporting a resize is safely ignored and returns `Ok(())`,
    /// matching the uniform-interface contract for protocols that are not driven
    /// by a character grid.
    async fn resize(&mut self, _cols: u16, _rows: u16) -> Result<(), ProtocolError> {
        Ok(())
    }

    async fn close(&mut self) -> Result<(), ProtocolError> {
        // Idempotent: closing an already-closed session succeeds.
        if let Some(mut stream) = self.stream.take() {
            // Best-effort graceful shutdown; a already-dropped peer is not an
            // error for close().
            let _ = stream.shutdown().await;
        }
        self.state = SessionState::Disconnected;
        Ok(())
    }

    fn state(&self) -> SessionState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // --- Timeout resolution (Property 16 smoke test) ----------------------

    #[test]
    fn resolve_timeout_keeps_values_in_range() {
        assert_eq!(resolve_timeout(1), 1);
        assert_eq!(resolve_timeout(30), 30);
        assert_eq!(resolve_timeout(120), 120);
        assert_eq!(resolve_timeout(45), 45);
    }

    #[test]
    fn resolve_timeout_defaults_out_of_range_values_to_30() {
        // Below the minimum (including 0) and above the maximum both default.
        assert_eq!(resolve_timeout(0), DEFAULT_TIMEOUT_SECS);
        assert_eq!(resolve_timeout(121), DEFAULT_TIMEOUT_SECS);
        assert_eq!(resolve_timeout(u16::MAX), DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn resolve_timeout_is_always_within_bounds() {
        // The invariant Property 16 asserts: the result is always 1..=120.
        for v in [0u16, 1, 30, 60, 120, 121, 500, u16::MAX] {
            let r = resolve_timeout(v);
            assert!(
                (MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&r),
                "resolve_timeout({v}) = {r} is outside 1..=120"
            );
        }
    }

    // --- Property 16: Telnet timeout resolution ---------------------------

    // A strategy that samples the full `u16` input space while deliberately
    // over-weighting the values that exercise the rule's branches and edges:
    // 0 (below min), the inclusive bounds 1 and 120, the just-outside value
    // 121, and large values up to `u16::MAX`. `prop_oneof!` keeps the uniform
    // `any::<u16>()` arm so the whole domain is still reachable, while the
    // explicit arms make the boundaries hit often instead of relying on luck.
    fn timeout_input() -> impl Strategy<Value = u16> {
        prop_oneof![
            Just(0u16),
            Just(1u16),
            Just(MAX_TIMEOUT_SECS),     // 120 — upper edge, still valid
            Just(MAX_TIMEOUT_SECS + 1), // 121 — just above the range
            1u16..=MAX_TIMEOUT_SECS,    // in-range values
            any::<u16>(),               // the full domain, incl. large values
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1000))]

        // Feature: remote-connection-manager, Property 16: For any Telnet profile, the resolved connection timeout equals the configured value or 30 and lies in 1..=120 seconds; when it fires, the Session is set to disconnected with a timeout error.
        #[test]
        fn prop16_telnet_timeout_resolution(connect_timeout_secs in timeout_input()) {
            let resolved = resolve_timeout(connect_timeout_secs);

            if (MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&connect_timeout_secs) {
                // In-range configured values are passed through unchanged.
                prop_assert_eq!(
                    resolved, connect_timeout_secs,
                    "in-range value {} must resolve to itself", connect_timeout_secs
                );
            } else {
                // Everything outside 1..=120 (including 0 and large values)
                // falls back to the default of 30 seconds.
                prop_assert_eq!(
                    resolved, DEFAULT_TIMEOUT_SECS,
                    "out-of-range value {} must resolve to the default", connect_timeout_secs
                );
            }

            // The core invariant: the resolved timeout is always within bounds,
            // regardless of the configured input.
            prop_assert!(
                (MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&resolved),
                "resolved timeout {} is outside 1..=120", resolved
            );
        }
    }

    // --- IAC negotiation handling -----------------------------------------

    fn session() -> TelnetSession {
        TelnetSession {
            stream: None,
            state: SessionState::Connected,
            pending_iac: Vec::new(),
        }
    }

    #[test]
    fn plain_data_passes_through_untouched() {
        let mut s = session();
        let (data, reply) = s.process_incoming(b"hello world");
        assert_eq!(data, b"hello world");
        assert!(reply.is_empty());
    }

    #[test]
    fn server_do_is_refused_with_wont_and_stripped_from_data() {
        let mut s = session();
        // IAC DO ECHO(1) surrounding real data.
        let raw = [b'h', b'i', IAC, DO, 1, b'!'];
        let (data, reply) = s.process_incoming(&raw);
        assert_eq!(data, b"hi!");
        assert_eq!(reply, vec![IAC, WONT, 1]);
    }

    #[test]
    fn server_will_is_refused_with_dont() {
        let mut s = session();
        let raw = [IAC, WILL, 3];
        let (data, reply) = s.process_incoming(&raw);
        assert!(data.is_empty());
        assert_eq!(reply, vec![IAC, DONT, 3]);
    }

    #[test]
    fn escaped_iac_becomes_single_literal_byte() {
        let mut s = session();
        let raw = [b'a', IAC, IAC, b'b'];
        let (data, reply) = s.process_incoming(&raw);
        assert_eq!(data, vec![b'a', IAC, b'b']);
        assert!(reply.is_empty());
    }

    #[test]
    fn command_split_across_reads_is_reassembled() {
        let mut s = session();
        // First read ends mid-command (IAC with no following bytes).
        let (data1, reply1) = s.process_incoming(&[b'x', IAC]);
        assert_eq!(data1, b"x");
        assert!(reply1.is_empty());
        // Second read supplies DO + option; the full command is now parsed.
        let (data2, reply2) = s.process_incoming(&[DO, 24]);
        assert!(data2.is_empty());
        assert_eq!(reply2, vec![IAC, WONT, 24]);
    }

    // --- Port validation (req 5.5) ----------------------------------------

    #[tokio::test]
    async fn establish_rejects_port_zero_without_connecting() {
        use crate::domain::{AuthConfig, ConnectionProfile, ProfileId, ProtocolParams};

        let profile = ConnectionProfile {
            id: ProfileId::new(),
            name: "t".to_string(),
            protocol: ProtocolKind::Telnet,
            host: "example.com".to_string(),
            port: 0,
            username: None,
            domain: None,
            folder_id: None,
            auth: AuthConfig::None,
            protocol_params: ProtocolParams::Telnet {},
            logging_enabled: false,
            log_path: None,
            connect_timeout_secs: 30,
        };
        let resolved = ResolvedProfile::new(profile);
        let result = TelnetProtocol::new()
            .establish(&resolved, CredentialHandle::none())
            .await;
        match result {
            Err(ProtocolError::InvalidParameter { parameter, .. }) => {
                assert_eq!(parameter, "port");
            }
            Err(other) => panic!("expected InvalidParameter, got {other:?}"),
            Ok(_) => panic!("port 0 must be rejected, but a session was established"),
        }
    }

    // --- Live Telnet integration (spec task 9.3) --------------------------
    //
    // Connects to a *real* Telnet server and verifies the connect + round-trip.
    // It is `#[ignore]`d so it never runs in the default suite; CI runs it with
    // `cargo test -- --ignored telnet` against the echo server the
    // `integration-telnet` job starts (socat TCP-LISTEN ... EXEC:/bin/cat).
    //
    // Server coordinates come from the environment so the test targets whatever
    // the CI job (or a developer) stood up:
    //   NOVRIX_IT_TELNET_HOST, NOVRIX_IT_TELNET_PORT
    // When those are unset (the common local case) the test SKIPS with a note
    // rather than failing, so `cargo test -- --ignored` is safe anywhere.
    #[tokio::test]
    #[ignore = "requires a live Telnet server; run in CI (integration-telnet) or set NOVRIX_IT_TELNET_*"]
    async fn telnet_live_connect_and_echo_round_trips() {
        use crate::domain::{AuthConfig, ConnectionProfile, ProfileId, ProtocolParams};

        let host = match std::env::var("NOVRIX_IT_TELNET_HOST") {
            Ok(h) if !h.trim().is_empty() => h,
            _ => {
                eprintln!(
                    "SKIP telnet_live_connect_and_echo_round_trips: \
                     NOVRIX_IT_TELNET_HOST not set"
                );
                return;
            }
        };
        let port: u16 = std::env::var("NOVRIX_IT_TELNET_PORT")
            .ok()
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or(23);

        let profile = ConnectionProfile {
            id: ProfileId::new(),
            name: "it-telnet".to_string(),
            protocol: ProtocolKind::Telnet,
            host,
            port,
            username: None,
            domain: None,
            folder_id: None,
            auth: AuthConfig::None,
            protocol_params: ProtocolParams::Telnet {},
            logging_enabled: false,
            log_path: None,
            connect_timeout_secs: 10,
        };
        let resolved = ResolvedProfile::new(profile);

        // (req 5.1/5.2) A real connect must succeed and yield a Connected session.
        let mut session = TelnetProtocol::new()
            .establish(&resolved, CredentialHandle::none())
            .await
            .expect("live Telnet connect should succeed");
        assert_eq!(session.state(), SessionState::Connected);

        // Round-trip: the echo server returns whatever we send. Send a unique
        // marker and read until we see it (bounded so a misbehaving server can
        // never hang CI).
        let marker = format!("novrix-it-{}\n", std::process::id());
        session
            .send(marker.as_bytes())
            .await
            .expect("send over live Telnet should succeed");

        let mut seen = Vec::new();
        let deadline = std::time::Duration::from_secs(10);
        let echoed = tokio::time::timeout(deadline, async {
            loop {
                match session.recv().await {
                    Ok(Some(chunk)) => {
                        seen.extend_from_slice(&chunk);
                        if seen.windows(marker.len()).any(|w| w == marker.as_bytes()) {
                            return true;
                        }
                    }
                    // Clean EOF before the echo: the server closed early.
                    Ok(None) => return false,
                    Err(e) => panic!("recv over live Telnet failed: {e:?}"),
                }
            }
        })
        .await
        .expect("timed out waiting for the Telnet echo");
        assert!(echoed, "the echo server did not return our marker");

        // Releasing the session closes the socket (req 5 / Requirement 8.3).
        session.close().await.expect("close should succeed");
    }
}
