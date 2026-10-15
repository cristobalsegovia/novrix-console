//! Structured error model for the Novrix Console core.
//!
//! Every core operation returns `Result<T, DomainError>`. `DomainError` carries
//! a machine-readable `code`, a user-facing `message`, and an optional `cause`,
//! so the frontend can render the specific validation field or failure reason
//! the requirements demand. Errors derive `serde` so they cross the Tauri IPC
//! boundary as structured payloads.
//!
//! Alongside the umbrella `DomainError`, this module defines the typed error
//! variants named in the design's "Error Handling" section
//! (`ValidationError`, `CredentialError`, `ProtocolError`, `RegistrationError`,
//! `KeyError`, `TransferError`, `LogError`, `ScriptError`, `ImportError`). Each
//! typed error converts into a `DomainError` via `From`, so component APIs can
//! return a precise error type while the IPC boundary always sees the uniform
//! shape.
//!
//! This task only defines the error *shape*; the business logic that produces
//! these errors (validation, persistence, protocol handling, ...) lands in
//! later tasks.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The uniform error payload returned by every core operation.
///
/// - `code` is a stable, machine-readable identifier (e.g. `validation.port`)
///   that the UI can switch on without parsing prose.
/// - `message` is a user-facing description safe to display in a dialog.
/// - `cause` is an optional lower-level detail (e.g. an OS error string) kept
///   separate from the user-facing message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error("{code}: {message}")]
#[serde(rename_all = "camelCase")]
pub struct DomainError {
    /// Machine-readable error code (stable identifier for the UI).
    pub code: String,
    /// User-facing message describing the failure.
    pub message: String,
    /// Optional lower-level cause (not shown verbatim to the user).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

impl DomainError {
    /// Construct a `DomainError` from a code and user-facing message.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            cause: None,
        }
    }

    /// Attach an optional lower-level cause.
    pub fn with_cause(mut self, cause: impl Into<String>) -> Self {
        self.cause = Some(cause.into());
        self
    }
}

/// Machine-readable discriminator shared by every typed error variant.
///
/// Serialized with each typed error so the frontend can route errors by
/// category in addition to the specific `code` carried by `DomainError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    Validation,
    Credential,
    Protocol,
    Registration,
    Key,
    Transfer,
    Log,
    Script,
    Import,
}

/// Invalid profile, folder name, port, serial parameter, resolution, key
/// type/size, or import schema. Returned before any state change; the offending
/// field is named in `message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error("validation error on `{field}`: {message}")]
#[serde(rename_all = "camelCase")]
pub struct ValidationError {
    /// The offending field or rule (e.g. `host`, `port`, `name`, `resolution`).
    pub field: String,
    /// User-facing explanation of why the value was rejected.
    pub message: String,
}

/// A failure in the OS Secure_Storage-backed Credential_Store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "camelCase")]
pub enum CredentialError {
    /// The requested credential does not exist in Secure_Storage.
    #[error("no credential found for the requested key")]
    NotFound,
    /// Secure_Storage failed to store the credential; the caller's in-memory
    /// value is preserved and any previously stored value is left unchanged.
    #[error("failed to store credential: {0}")]
    StoreFailed(String),
    /// Secure_Storage failed to retrieve an existing credential.
    #[error("failed to retrieve credential: {0}")]
    RetrieveFailed(String),
    /// Secure_Storage failed to delete a credential.
    #[error("failed to delete credential: {0}")]
    DeleteFailed(String),
}

/// A failure establishing, maintaining, or closing a protocol session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "camelCase")]
pub enum ProtocolError {
    /// The connection attempt exceeded the configured timeout.
    #[error("connection timed out")]
    Timeout,
    /// The remote host refused the connection.
    #[error("connection refused")]
    ConnectionRefused,
    /// The configured host could not be resolved.
    #[error("host could not be resolved: {0}")]
    HostUnresolved(String),
    /// Authentication failed (e.g. SSH after the attempt limit, RDP auth).
    #[error("authentication failed")]
    AuthenticationFailed,
    /// A required credential was missing from the Credential_Store.
    #[error("required credentials are missing")]
    MissingCredentials,
    /// An unknown host key was presented and the user declined to trust it.
    #[error("unknown host key was declined")]
    HostKeyDeclined,
    /// A configured parameter was invalid (e.g. port, serial param, resolution).
    #[error("invalid connection parameter `{parameter}`: {message}")]
    InvalidParameter {
        /// The offending parameter name.
        parameter: String,
        /// User-facing explanation.
        message: String,
    },
    /// A transport-level or I/O failure during the session.
    #[error("protocol I/O failure: {0}")]
    Io(String),
}

/// A failure registering a protocol with the `ProtocolRegistry`.
///
/// A manual `Display`/`Error` impl is used (rather than `#[derive(Error)]`)
/// because the message needs to join the `missing_operations` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationError {
    /// The protocol that failed registration (human-readable identifier).
    pub protocol: String,
    /// The uniform-interface operations the protocol failed to implement.
    pub missing_operations: Vec<String>,
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "protocol `{}` is missing required operations: {}",
            self.protocol,
            self.missing_operations.join(", ")
        )
    }
}

impl std::error::Error for RegistrationError {}

/// A failure generating, importing, or deleting a managed SSH key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "camelCase")]
pub enum KeyError {
    /// The requested key type is not supported.
    #[error("unsupported key type: {0}")]
    UnsupportedType(String),
    /// The requested key size is outside the valid range for the key type.
    #[error("invalid key size for the selected type: {0}")]
    InvalidSize(String),
    /// The imported bytes are not a valid SSH private key.
    #[error("invalid SSH private key")]
    InvalidKey,
    /// The imported key exceeds the 1 MB size limit.
    #[error("key file exceeds the 1 MB size limit")]
    OversizedKey,
    /// The referenced managed key does not exist.
    #[error("managed key not found")]
    NotFound,
    /// Deletion of a managed key was requested without confirmation; the key
    /// and any associated passphrase are left unchanged until the caller
    /// confirms (requirements 12.6, 12.8).
    #[error("deleting a managed SSH key requires confirmation")]
    ConfirmationRequired,
    /// Storing or retrieving the key or its passphrase failed.
    #[error("key storage failure: {0}")]
    StorageFailed(String),
}

/// A failure during an SCP/SFTP file transfer or remote navigation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "camelCase")]
pub enum TransferError {
    /// The transfer failed before all bytes were sent; the incomplete
    /// destination file is removed and the outcome reported as failed.
    #[error("transfer failed: {0}")]
    Failed(String),
    /// The user cancelled the transfer; the incomplete destination is removed.
    #[error("transfer canceled")]
    Canceled,
    /// A remote directory could not be accessed; the prior listing is kept.
    #[error("remote directory is inaccessible: {0}")]
    DirectoryInaccessible(String),
    /// A transport-level or I/O failure during transfer.
    #[error("transfer I/O failure: {0}")]
    Io(String),
}

/// A failure recording session I/O to a log file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "camelCase")]
pub enum LogError {
    /// The configured log destination does not exist or is not writable at
    /// initiation; the Session is not opened with logging enabled.
    #[error("invalid log destination: {0}")]
    InvalidDestination(String),
    /// A write to the log file failed; entries written before the failure are
    /// retained.
    #[error("log write failed: {0}")]
    WriteFailed(String),
}

/// A failure running an automation script against a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "camelCase")]
pub enum ScriptError {
    /// The expected prompt text was not received within the configured timeout;
    /// execution stops with remaining commands unsent.
    #[error("timed out waiting for prompt at command {command_index}")]
    PromptTimeout {
        /// The index (0-based) of the command that was waiting for the prompt.
        command_index: usize,
    },
    /// The Session became disconnected during execution; execution stops with
    /// remaining commands unsent.
    #[error("session disconnected during script execution at command {command_index}")]
    Disconnected {
        /// The index (0-based) of the command at which execution stopped.
        command_index: usize,
    },
}

/// A failure importing a configuration export file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "camelCase")]
pub enum ImportError {
    /// The file is malformed or does not conform to the expected export
    /// structure; the existing configuration is left unchanged.
    #[error("import file is malformed: {0}")]
    Malformed(String),
    /// The export file's schema version is not supported by this build.
    #[error("unsupported export schema version: {0}")]
    UnsupportedVersion(u32),
    /// The file could not be read from disk.
    #[error("failed to read import file: {0}")]
    Io(String),
}

// --- Conversions into the uniform `DomainError` ---------------------------

impl From<ValidationError> for DomainError {
    fn from(e: ValidationError) -> Self {
        DomainError::new(format!("validation.{}", e.field), e.message)
    }
}

impl From<CredentialError> for DomainError {
    fn from(e: CredentialError) -> Self {
        let code = match &e {
            CredentialError::NotFound => "credential.notFound",
            CredentialError::StoreFailed(_) => "credential.storeFailed",
            CredentialError::RetrieveFailed(_) => "credential.retrieveFailed",
            CredentialError::DeleteFailed(_) => "credential.deleteFailed",
        };
        DomainError::new(code, e.to_string())
    }
}

impl From<ProtocolError> for DomainError {
    fn from(e: ProtocolError) -> Self {
        let code = match &e {
            ProtocolError::Timeout => "protocol.timeout",
            ProtocolError::ConnectionRefused => "protocol.connectionRefused",
            ProtocolError::HostUnresolved(_) => "protocol.hostUnresolved",
            ProtocolError::AuthenticationFailed => "protocol.authenticationFailed",
            ProtocolError::MissingCredentials => "protocol.missingCredentials",
            ProtocolError::HostKeyDeclined => "protocol.hostKeyDeclined",
            ProtocolError::InvalidParameter { .. } => "protocol.invalidParameter",
            ProtocolError::Io(_) => "protocol.io",
        };
        DomainError::new(code, e.to_string())
    }
}

impl From<RegistrationError> for DomainError {
    fn from(e: RegistrationError) -> Self {
        DomainError::new("registration.incomplete", e.to_string())
    }
}

impl From<KeyError> for DomainError {
    fn from(e: KeyError) -> Self {
        let code = match &e {
            KeyError::UnsupportedType(_) => "key.unsupportedType",
            KeyError::InvalidSize(_) => "key.invalidSize",
            KeyError::InvalidKey => "key.invalidKey",
            KeyError::OversizedKey => "key.oversizedKey",
            KeyError::NotFound => "key.notFound",
            KeyError::ConfirmationRequired => "key.confirmationRequired",
            KeyError::StorageFailed(_) => "key.storageFailed",
        };
        DomainError::new(code, e.to_string())
    }
}

impl From<TransferError> for DomainError {
    fn from(e: TransferError) -> Self {
        let code = match &e {
            TransferError::Failed(_) => "transfer.failed",
            TransferError::Canceled => "transfer.canceled",
            TransferError::DirectoryInaccessible(_) => "transfer.directoryInaccessible",
            TransferError::Io(_) => "transfer.io",
        };
        DomainError::new(code, e.to_string())
    }
}

impl From<LogError> for DomainError {
    fn from(e: LogError) -> Self {
        let code = match &e {
            LogError::InvalidDestination(_) => "log.invalidDestination",
            LogError::WriteFailed(_) => "log.writeFailed",
        };
        DomainError::new(code, e.to_string())
    }
}

impl From<ScriptError> for DomainError {
    fn from(e: ScriptError) -> Self {
        let code = match &e {
            ScriptError::PromptTimeout { .. } => "script.promptTimeout",
            ScriptError::Disconnected { .. } => "script.disconnected",
        };
        DomainError::new(code, e.to_string())
    }
}

impl From<ImportError> for DomainError {
    fn from(e: ImportError) -> Self {
        let code = match &e {
            ImportError::Malformed(_) => "import.malformed",
            ImportError::UnsupportedVersion(_) => "import.unsupportedVersion",
            ImportError::Io(_) => "import.io",
        };
        DomainError::new(code, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_error_serializes_as_camel_case_payload() {
        let err = DomainError::new("validation.port", "Port must be 1..=65535");
        let json = serde_json::to_string(&err).unwrap();
        // `cause` is omitted when None.
        assert_eq!(
            json,
            r#"{"code":"validation.port","message":"Port must be 1..=65535"}"#
        );

        let roundtrip: DomainError = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, err);
    }

    #[test]
    fn domain_error_includes_cause_when_present() {
        let err = DomainError::new("protocol.io", "I/O failure").with_cause("broken pipe");
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains(r#""cause":"broken pipe""#));
    }

    #[test]
    fn validation_error_maps_to_namespaced_code() {
        let err = ValidationError {
            field: "host".to_string(),
            message: "Host is required".to_string(),
        };
        let domain: DomainError = err.into();
        assert_eq!(domain.code, "validation.host");
        assert_eq!(domain.message, "Host is required");
    }

    #[test]
    fn typed_errors_map_to_expected_codes() {
        assert_eq!(
            DomainError::from(CredentialError::NotFound).code,
            "credential.notFound"
        );
        assert_eq!(
            DomainError::from(ProtocolError::Timeout).code,
            "protocol.timeout"
        );
        assert_eq!(
            DomainError::from(KeyError::OversizedKey).code,
            "key.oversizedKey"
        );
        assert_eq!(
            DomainError::from(TransferError::Canceled).code,
            "transfer.canceled"
        );
        assert_eq!(
            DomainError::from(LogError::WriteFailed("disk full".into())).code,
            "log.writeFailed"
        );
        assert_eq!(
            DomainError::from(ScriptError::PromptTimeout { command_index: 3 }).code,
            "script.promptTimeout"
        );
        assert_eq!(
            DomainError::from(ImportError::UnsupportedVersion(99)).code,
            "import.unsupportedVersion"
        );
    }

    #[test]
    fn registration_error_lists_missing_operations() {
        let err = RegistrationError {
            protocol: "ssh".to_string(),
            missing_operations: vec!["recv".to_string(), "resize".to_string()],
        };
        let domain: DomainError = err.into();
        assert_eq!(domain.code, "registration.incomplete");
        assert!(domain.message.contains("recv"));
        assert!(domain.message.contains("resize"));
    }
}
