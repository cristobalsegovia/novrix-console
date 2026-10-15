//! Shared domain layer for the Novrix Console core.
//!
//! This module groups the pure, cross-cutting type definitions that every core
//! component (Connection_Manager, Credential_Store, Protocol_Engine, ...) and
//! the Tauri IPC bridge build on:
//!
//! - [`error`] — the structured [`DomainError`] and the typed error variants.
//! - [`model`] — the data-model enums, structs, and id newtypes.
//!
//! All types derive `serde` so they cross the IPC boundary as structured
//! payloads. Business logic lives in later tasks; this layer is types only.

pub mod error;
pub mod model;

pub use error::{
    CredentialError, DomainError, ErrorKind, ImportError, KeyError, LogError, ProtocolError,
    RegistrationError, ScriptError, TransferError, ValidationError,
};
pub use model::{
    AuthConfig, BaudRate, ConnectionProfile, CredentialKey, DataBits, ExportFile, Folder, FolderId,
    HostKeyPolicy, KeyId, KeyType, ManagedKey, Parity, ProfileId, ProtocolKind, ProtocolParams,
    Resolution, Session, SessionId, SessionState, StopBits,
};
