//! Shared domain data model for the Novrix Console core.
//!
//! These are the enums, structs, and id newtypes from the design's "Data
//! Models" section. They are intentionally pure *type definitions* — the
//! business logic that validates, persists, and acts on them (bounds checks,
//! hierarchy integrity, protocol establishment, ...) lands in later tasks.
//!
//! Every type derives `serde` so profiles, folders, sessions, keys, and the
//! export file cross the Tauri IPC boundary as structured payloads. Documented
//! bounds (port `1..=65535`, folder depth `0..=9`, resolution
//! `640x480..=7680x4320`, etc.) are recorded here as doc comments; their
//! *enforcement* is a later task.
//!
//! Secrets never live in these types: credentials are referenced indirectly via
//! [`CredentialKey`] and [`KeyId`], and resolved only through the
//! Credential_Store against OS Secure_Storage.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Declares a UUID-backed newtype id with `serde` + conversions.
///
/// Each id is a transparent wrapper around a [`Uuid`] so the type system keeps
/// profile, folder, session, and key ids from being mixed up, while still
/// serializing as a plain UUID string across IPC.
macro_rules! uuid_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Generate a fresh random (v4) id.
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Borrow the underlying [`Uuid`].
            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Uuid> for $name {
            fn from(id: Uuid) -> Self {
                Self(id)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

uuid_newtype! {
    /// Stable identifier for a [`ConnectionProfile`].
    ProfileId
}
uuid_newtype! {
    /// Stable identifier for a [`Folder`].
    FolderId
}
uuid_newtype! {
    /// Stable identifier for a [`Session`].
    SessionId
}
uuid_newtype! {
    /// Stable identifier for a [`ManagedKey`].
    KeyId
}

/// Opaque reference to a secret held in OS Secure_Storage.
///
/// A `CredentialKey` is never a secret itself — it only names where the
/// Credential_Store can find the password/passphrase. This keeps plaintext out
/// of profiles, the SQLite database, and export files.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CredentialKey(pub String);

impl CredentialKey {
    /// Wrap a storage key string.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// Borrow the underlying key string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CredentialKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The connection protocols the System can establish. Extensible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProtocolKind {
    Ssh,
    Telnet,
    Serial,
    Rdp,
}

/// SSH host-key verification policy for an SSH profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HostKeyPolicy {
    /// Prompt the user to confirm the first time an unknown host key is seen.
    PromptOnUnknown,
    /// Accept any host key without prompting (insecure; opt-in).
    AcceptAny,
    /// Reject any host key that is not already known.
    Strict,
}

/// How a Session authenticates to its remote target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AuthConfig {
    /// No authentication (e.g. plain Telnet / open serial).
    None,
    /// Password authentication; the secret lives in Secure_Storage.
    Password { credential_key: CredentialKey },
    /// SSH key authentication referencing a [`ManagedKey`].
    SshKey { key_id: KeyId },
}

/// Supported serial baud rates (bits per second).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BaudRate {
    B300,
    B1200,
    B2400,
    B4800,
    B9600,
    B19200,
    B38400,
    B57600,
    B115200,
}

/// Supported serial data-bit widths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DataBits {
    Five,
    Six,
    Seven,
    Eight,
}

/// Supported serial parity settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Parity {
    None,
    Even,
    Odd,
}

/// Supported serial stop-bit settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopBits {
    One,
    Two,
}

/// A requested RDP display resolution.
///
/// Bounded `640..=7680` (width) x `480..=4320` (height); the bound is enforced
/// in the RDP protocol task, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

/// Protocol-specific parameters carried by a [`ConnectionProfile`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "camelCase")]
pub enum ProtocolParams {
    Ssh {
        known_host_policy: HostKeyPolicy,
    },
    Telnet {},
    Serial {
        port_name: String,
        baud: BaudRate,
        data_bits: DataBits,
        parity: Parity,
        stop_bits: StopBits,
    },
    Rdp {
        /// Optional requested resolution, bounded `640x480..=7680x4320`.
        resolution: Option<Resolution>,
    },
}

/// A saved configuration describing how to connect to a single remote target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionProfile {
    /// Stable UUID identifier.
    pub id: ProfileId,
    pub name: String,
    pub protocol: ProtocolKind,
    /// Required for network protocols.
    pub host: String,
    /// Validated `1..=65535`.
    pub port: u16,
    /// Optional login username (SSH login user; RDP account name). When `None`
    /// the connect layer falls back to its documented per-protocol default.
    /// Validated `1..=256` characters when present.
    pub username: Option<String>,
    /// Optional login domain, used by RDP (NLA/CredSSP) only. `None` means no
    /// domain (a local account). Validated `1..=256` characters when present.
    pub domain: Option<String>,
    /// Owning folder, or `None` when the profile sits at the root.
    pub folder_id: Option<FolderId>,
    pub auth: AuthConfig,
    pub protocol_params: ProtocolParams,
    pub logging_enabled: bool,
    pub log_path: Option<PathBuf>,
    /// Bounded per protocol; the bound is enforced at connect time.
    pub connect_timeout_secs: u16,
}

/// A named container grouping profiles into a hierarchy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: FolderId,
    /// `1..=255` chars, unique within its parent.
    pub name: String,
    /// Owning parent folder, or `None` for a root folder.
    pub parent_id: Option<FolderId>,
    /// `0..=9` (root = 0); nesting is capped at 10 levels.
    pub depth: u8,
}

/// Connection state of a [`Session`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionState {
    Connecting,
    Connected,
    Disconnected,
}

/// An active, open connection to a remote target.
///
/// The scrollback buffer type lives with the Terminal_Emulator (a later task);
/// here the session records only its identity, source profile, kind, and state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: SessionId,
    /// Source profile, or `None` for an ad hoc session.
    pub profile_id: Option<ProfileId>,
    pub kind: ProtocolKind,
    pub state: SessionState,
}

/// Supported SSH key types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeyType {
    Rsa,
    Ecdsa,
    Ed25519,
}

/// A managed SSH key pair. The private key material and any passphrase live in
/// Secure_Storage; only metadata and the public key are held here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedKey {
    pub id: KeyId,
    pub key_type: KeyType,
    /// The public component, safe to display and copy.
    pub public_key: String,
    /// Whether the private key is protected by a passphrase in Secure_Storage.
    pub has_passphrase: bool,
}

/// A versioned configuration export.
///
/// Contains only folders and profiles with credential *references* — never
/// plaintext secrets. On import, items receive fresh identifiers so they are
/// always added as new entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportFile {
    pub schema_version: u32,
    pub folders: Vec<Folder>,
    /// Auth references only, never secrets.
    pub profiles: Vec<ConnectionProfile>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_newtypes_are_distinct_and_roundtrip_as_uuid_strings() {
        let id = ProfileId::new();
        let json = serde_json::to_string(&id).unwrap();
        // Transparent: serializes as a bare quoted UUID string.
        assert_eq!(json, format!("\"{}\"", id.as_uuid()));
        let back: ProfileId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn auth_config_tagged_serialization() {
        let auth = AuthConfig::Password {
            credential_key: CredentialKey::new("profile/42/password"),
        };
        let json = serde_json::to_value(&auth).unwrap();
        assert_eq!(json["type"], "password");
        assert_eq!(json["credential_key"], "profile/42/password");

        let roundtrip: AuthConfig = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, auth);
    }

    #[test]
    fn protocol_params_tagged_serialization() {
        let params = ProtocolParams::Serial {
            port_name: "/dev/ttyUSB0".to_string(),
            baud: BaudRate::B115200,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["protocol"], "serial");
        assert_eq!(json["baud"], "b115200");

        let roundtrip: ProtocolParams = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, params);
    }

    #[test]
    fn connection_profile_roundtrips_across_ipc_shape() {
        let profile = ConnectionProfile {
            id: ProfileId::new(),
            name: "prod-web".to_string(),
            protocol: ProtocolKind::Ssh,
            host: "example.com".to_string(),
            port: 22,
            username: Some("deploy".to_string()),
            domain: None,
            folder_id: Some(FolderId::new()),
            auth: AuthConfig::SshKey {
                key_id: KeyId::new(),
            },
            protocol_params: ProtocolParams::Ssh {
                known_host_policy: HostKeyPolicy::PromptOnUnknown,
            },
            logging_enabled: false,
            log_path: None,
            connect_timeout_secs: 30,
        };
        let json = serde_json::to_string(&profile).unwrap();
        let roundtrip: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, profile);
    }

    #[test]
    fn export_file_roundtrips() {
        let export = ExportFile {
            schema_version: 1,
            folders: vec![Folder {
                id: FolderId::new(),
                name: "servers".to_string(),
                parent_id: None,
                depth: 0,
            }],
            profiles: vec![],
        };
        let json = serde_json::to_string(&export).unwrap();
        let roundtrip: ExportFile = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, export);
    }
}
