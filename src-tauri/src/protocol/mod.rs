//! Protocol_Engine — the uniform protocol interface and registry.
//!
//! This module defines the single interface every Supported_Protocol (SSH,
//! Telnet, Serial, RDP) must implement, plus the [`ProtocolRegistry`] that
//! validates registrations and reports which protocols are selectable when a
//! user creates a Connection_Profile (design: "Protocol_Engine — Uniform
//! Protocol Interface", Requirement 8).
//!
//! Scope of this task (7.1): the *interface definition* and the registry only.
//! The concrete SSH/Telnet/Serial/RDP implementations land in tasks 8–11, so
//! nothing here opens a socket, a serial port, or an RDP channel. The traits
//! and registry are shaped so those later implementations, and the property
//! tests in tasks 7.2–7.5, drop in without changing this contract.
//!
//! The two uniform-interface traits are:
//!
//! - [`Protocol`] — a protocol factory: it reports its [`ProtocolKind`] and
//!   knows how to [`establish`](Protocol::establish) a live session. A failed
//!   `establish` must release every resource it allocated (RAII), leaving no
//!   half-open connection behind (Requirement 8.3).
//! - [`SessionConn`] — a live, established session: `send`, `recv`, `resize`,
//!   `close`, and `state`. The same five operations apply unchanged to every
//!   protocol (Requirement 8.1).
//!
//! Both traits are `#[async_trait]` so they stay object-safe behind
//! `Box<dyn Protocol>` / `Box<dyn SessionConn>`, exactly as the design's
//! `register(p: Box<dyn Protocol>)` and
//! `establish(...) -> Box<dyn SessionConn>` signatures require.

mod interface;
mod rdp;
mod registry;
mod serial;
mod ssh;
mod telnet;

pub use interface::{CredentialHandle, Protocol, ResolvedProfile, SessionConn, UNIFORM_OPERATIONS};
pub use rdp::{
    resolution_in_bounds, validate_resolution, IronRdpConnector, RdpCertVerifier, RdpConnector,
    RdpCredentialSource, RdpProtocol, RdpSession, RdpTransport, ResolvedCredential,
    DEFAULT_RESOLUTION, MAX_CONNECT_TIMEOUT, MAX_RESOLUTION, MIN_RESOLUTION,
};
pub use registry::ProtocolRegistry;
pub use serial::{available_port_names, validate_serial_params, SerialProtocol, SerialSession};
// SSH protocol over russh (task 8.1). Pure port/timeout resolution, auth
// planning (incl. missing-credential abort), the 3-attempt budget, and the
// injectable host-key/credential ports; transport wiring is marked TODO.
pub use ssh::{
    resolve_port as resolve_ssh_port, AuthAttemptBudget, AuthPlan, HostKeyVerifier,
    ResolvedAuthMaterial, SecretResolver, SshIdentity, SshKeyResolver, SshProtocol, SshSession,
};
pub use telnet::{resolve_timeout as telnet_resolve_timeout, TelnetProtocol, TelnetSession};
