//! Local persistence layer for the Novrix Console core (task 2.1).
//!
//! Profiles, folders, and application settings live in a local SQLite database
//! accessed through [`rusqlite`] (bundled SQLite). This module owns three
//! concerns and nothing more:
//!
//! - the **schema** for the `folders`, `profiles`, and `settings` tables,
//! - the **connection bootstrap** (open or create-on-first-run), and
//! - a simple **schema-version migration** driven by SQLite's `user_version`
//!   pragma.
//!
//! The repository CRUD helpers the Connection_Manager calls (insert/read/
//! update/delete of profiles and folders) are deliberately *not* here — they
//! are task 2.2 and build on top of the [`Database`] handle this module
//! produces.
//!
//! # Secrets never land here
//!
//! Per the design's persistence note, secrets never enter SQLite. Credentials
//! live only in OS Secure_Storage and are referenced indirectly:
//!
//! - a profile's [`AuthConfig::Password`] stores only a [`CredentialKey`]
//!   string (a *name*, never a password), and
//! - a profile's [`AuthConfig::SshKey`] stores only a [`KeyId`] (a UUID).
//!
//! The schema therefore has no password/passphrase/private-key columns; it
//! holds credential *references* only.
//!
//! [`AuthConfig::Password`]: crate::domain::AuthConfig::Password
//! [`AuthConfig::SshKey`]: crate::domain::AuthConfig::SshKey
//! [`CredentialKey`]: crate::domain::CredentialKey
//! [`KeyId`]: crate::domain::KeyId

use std::path::Path;

use rusqlite::Connection;

use crate::domain::DomainError;

/// Profile and folder CRUD repository helpers (task 2.2), layered on the
/// [`Database`] handle this module produces. Implemented as inherent methods on
/// [`Database`]; this declaration brings the `impl` block into the build.
mod repository;

/// Persistent known-hosts / known-certs store and the pure trust decision
/// (task 26.3). Maps `(host, port, kind)` to the accepted SSH host-key / RDP
/// certificate fingerprint; added by schema version 3.
pub mod known_hosts;

pub use known_hosts::{
    certificate_fingerprint, decide_trust, KnownHostsStore, TrustDecision, TrustKind,
};

/// The schema version this build understands.
///
/// Stored in SQLite's `PRAGMA user_version`. [`Database::open`] migrates any
/// older database forward to this version on open. Bumping this constant and
/// adding a matching arm to [`migrate`] is how future schema changes ship.
pub const SCHEMA_VERSION: u32 = 3;

/// A handle to the local SQLite database holding profiles, folders, and
/// settings.
///
/// Construct one with [`Database::open`] (a real on-disk file, created on first
/// run) or [`Database::open_in_memory`] (an ephemeral database, used by tests).
/// The CRUD helpers that read and write profiles and folders are layered on top
/// of this handle in task 2.2.
pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open the database at `path`, creating it on first run, and migrate it to
    /// [`SCHEMA_VERSION`].
    ///
    /// If the file does not exist it is created; if the parent directory does
    /// not exist it is created first. Foreign-key enforcement is enabled and
    /// the schema is brought up to date before the handle is returned, so the
    /// caller always receives a database at the current version.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DomainError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    persistence_error("persistence.open", "Failed to create data directory", e)
                })?;
            }
        }
        let conn = Connection::open(path)
            .map_err(|e| persistence_error("persistence.open", "Failed to open database", e))?;
        Self::bootstrap(conn)
    }

    /// Open a fresh in-memory database migrated to [`SCHEMA_VERSION`].
    ///
    /// The database vanishes when the handle is dropped. Primarily useful for
    /// unit tests and the bootstrap/migration tests below.
    pub fn open_in_memory() -> Result<Self, DomainError> {
        let conn = Connection::open_in_memory().map_err(|e| {
            persistence_error("persistence.open", "Failed to open in-memory database", e)
        })?;
        Self::bootstrap(conn)
    }

    /// Enable pragmas and run migrations on a freshly opened connection.
    fn bootstrap(conn: Connection) -> Result<Self, DomainError> {
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(|e| {
                persistence_error("persistence.pragma", "Failed to enable foreign keys", e)
            })?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    /// The current schema version recorded in the database.
    pub fn schema_version(&self) -> Result<u32, DomainError> {
        read_user_version(&self.conn)
    }

    /// Borrow the underlying connection.
    ///
    /// Exposed so the task 2.2 repository helpers can run statements against
    /// this already-bootstrapped, already-migrated database.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Migrate the database from its stored version up to [`SCHEMA_VERSION`].
    ///
    /// Each step runs inside a transaction and bumps `user_version`, so an
    /// interrupted upgrade never leaves the database half-migrated. A database
    /// already at the current version is left untouched; a database newer than
    /// this build understands is rejected rather than silently downgraded.
    fn migrate(&mut self) -> Result<(), DomainError> {
        let mut version = read_user_version(&self.conn)?;

        if version > SCHEMA_VERSION {
            return Err(DomainError::new(
                "persistence.schemaTooNew",
                format!(
                    "Database schema version {version} is newer than this build supports ({SCHEMA_VERSION})"
                ),
            ));
        }

        while version < SCHEMA_VERSION {
            let next = version + 1;
            let tx = self.conn.transaction().map_err(|e| {
                persistence_error("persistence.migrate", "Failed to begin migration", e)
            })?;
            migrate(&tx, next)?;
            tx.pragma_update(None, "user_version", next).map_err(|e| {
                persistence_error("persistence.migrate", "Failed to record schema version", e)
            })?;
            tx.commit().map_err(|e| {
                persistence_error("persistence.migrate", "Failed to commit migration", e)
            })?;
            version = next;
        }

        Ok(())
    }
}

/// Read SQLite's `user_version` pragma as the stored schema version.
fn read_user_version(conn: &Connection) -> Result<u32, DomainError> {
    conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map(|v| v as u32)
        .map_err(|e| persistence_error("persistence.pragma", "Failed to read schema version", e))
}

/// Apply the single migration step that upgrades the schema *to* `target`.
///
/// Version 1 creates the initial `folders`, `profiles`, and `settings` tables.
/// Future versions add their own arm here (and bump [`SCHEMA_VERSION`]).
fn migrate(conn: &Connection, target: u32) -> Result<(), DomainError> {
    match target {
        1 => migrate_to_v1(conn),
        2 => migrate_to_v2(conn),
        3 => migrate_to_v3(conn),
        other => Err(DomainError::new(
            "persistence.migrate",
            format!("No migration defined for schema version {other}"),
        )),
    }
}

/// The initial schema (version 1).
///
/// Design decisions worth calling out:
///
/// - **Stable UUID ids.** `folders.id` and `profiles.id` are TEXT columns
///   holding the UUID string of a [`FolderId`]/[`ProfileId`]. The ids are
///   generated by the domain layer and stay stable across edits.
/// - **No secrets.** Password auth is stored as `auth_kind = 'password'` plus a
///   `credential_key` *reference*; key auth is stored as `auth_kind = 'sshKey'`
///   plus a `key_id` UUID. There is no column anywhere for a password,
///   passphrase, or private key.
/// - **Hierarchy integrity.** `folders.parent_id` is a self-referencing foreign
///   key (`ON DELETE CASCADE`) so deleting a folder removes its subtree, and
///   `profiles.folder_id` references `folders.id` (`ON DELETE CASCADE`).
/// - **Protocol params** are kept as a JSON blob in `protocol_params` rather
///   than exploded into protocol-specific columns, matching the tagged
///   `ProtocolParams` enum and keeping the protocol set extensible.
///
/// [`FolderId`]: crate::domain::FolderId
/// [`ProfileId`]: crate::domain::ProfileId
fn migrate_to_v1(conn: &Connection) -> Result<(), DomainError> {
    conn.execute_batch(
        r#"
        CREATE TABLE folders (
            id         TEXT PRIMARY KEY NOT NULL,
            name       TEXT NOT NULL,
            parent_id  TEXT REFERENCES folders(id) ON DELETE CASCADE,
            depth      INTEGER NOT NULL
        );

        CREATE INDEX idx_folders_parent ON folders(parent_id);

        CREATE TABLE profiles (
            id                  TEXT PRIMARY KEY NOT NULL,
            name                TEXT NOT NULL,
            protocol            TEXT NOT NULL,
            host                TEXT NOT NULL,
            port                INTEGER NOT NULL,
            folder_id           TEXT REFERENCES folders(id) ON DELETE CASCADE,
            -- Credential REFERENCES only. Never a secret value.
            --   auth_kind: 'none' | 'password' | 'sshKey'
            --   credential_key: Secure_Storage key name (password auth)
            --   key_id: ManagedKey UUID (ssh-key auth)
            auth_kind           TEXT NOT NULL,
            credential_key      TEXT,
            key_id              TEXT,
            protocol_params     TEXT NOT NULL,
            logging_enabled     INTEGER NOT NULL DEFAULT 0,
            log_path            TEXT,
            connect_timeout_secs INTEGER NOT NULL
        );

        CREATE INDEX idx_profiles_folder ON profiles(folder_id);

        CREATE TABLE settings (
            key   TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        );
        "#,
    )
    .map_err(|e| persistence_error("persistence.migrate", "Failed to create schema v1", e))?;
    Ok(())
}

/// Schema version 2: add optional login-identity columns to `profiles`.
///
/// Adds two nullable `TEXT` columns — `username` (SSH login user / RDP account
/// name) and `domain` (RDP logon domain) — carrying a profile's optional login
/// identity. Both default to `NULL`, so every pre-existing row keeps the prior
/// behaviour (the connect layer falls back to its documented per-protocol
/// default) and a database opened by an older build upgrades in place without
/// data loss. The columns hold a *username/domain* only — never a secret.
fn migrate_to_v2(conn: &Connection) -> Result<(), DomainError> {
    conn.execute_batch(
        r#"
        ALTER TABLE profiles ADD COLUMN username TEXT;
        ALTER TABLE profiles ADD COLUMN domain   TEXT;
        "#,
    )
    .map_err(|e| persistence_error("persistence.migrate", "Failed to apply schema v2", e))?;
    Ok(())
}

/// Schema version 3: add the `known_hosts` trust store (task 26.3).
///
/// Records, per `(host, port, kind)`, the single fingerprint the user has
/// accepted — the real replacement for the trust-on-first-use stubs. One table
/// serves both SSH host keys and RDP certificates, kept apart by the `kind`
/// column (`'ssh-host-key'` / `'rdp-certificate'`), so the same `host:port`
/// trusted for SSH and for RDP keeps independent entries. The composite primary
/// key `(host, port, kind)` makes the store an upsert target: accepting a new
/// key for a host replaces the prior one (which only happens after the old entry
/// is explicitly forgotten, since a *changed* key is otherwise a hard reject).
///
/// Like every other column in this schema, a `fingerprint` is a **digest**, not
/// a secret — never a private key or password — so it is safe at rest here.
fn migrate_to_v3(conn: &Connection) -> Result<(), DomainError> {
    conn.execute_batch(
        r#"
        CREATE TABLE known_hosts (
            host        TEXT NOT NULL,
            port        INTEGER NOT NULL,
            -- 'ssh-host-key' | 'rdp-certificate'
            kind        TEXT NOT NULL,
            -- SHA256:<base64> digest of the accepted host key / certificate.
            fingerprint TEXT NOT NULL,
            PRIMARY KEY (host, port, kind)
        );
        "#,
    )
    .map_err(|e| persistence_error("persistence.migrate", "Failed to apply schema v3", e))?;
    Ok(())
}

/// Build a [`DomainError`] from a `rusqlite` error, attaching the underlying
/// message as the `cause` (kept separate from the user-facing message).
fn persistence_error(code: &str, message: &str, cause: impl std::fmt::Display) -> DomainError {
    DomainError::new(code, message).with_cause(cause.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_migrates_to_current_version() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn bootstrap_creates_the_three_tables() {
        let db = Database::open_in_memory().unwrap();
        let names: Vec<String> = db
            .connection()
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(names.contains(&"folders".to_string()));
        assert!(names.contains(&"profiles".to_string()));
        assert!(names.contains(&"settings".to_string()));
    }

    #[test]
    fn bootstrap_creates_the_known_hosts_table() {
        let db = Database::open_in_memory().unwrap();
        let names: Vec<String> = db
            .connection()
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            names.contains(&"known_hosts".to_string()),
            "schema v3 must create the known_hosts trust store"
        );
    }

    #[test]
    fn profiles_table_has_no_secret_columns_only_references() {
        let db = Database::open_in_memory().unwrap();
        let columns: Vec<String> = db
            .connection()
            .prepare("SELECT name FROM pragma_table_info('profiles')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();

        // Reference columns are present...
        assert!(columns.contains(&"credential_key".to_string()));
        assert!(columns.contains(&"key_id".to_string()));
        assert!(columns.contains(&"auth_kind".to_string()));

        // ...but no column ever holds a secret value.
        for forbidden in ["password", "passphrase", "secret", "private_key"] {
            assert!(
                !columns.iter().any(|c| c.contains(forbidden)),
                "profiles table must not contain a `{forbidden}` column"
            );
        }
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let db = Database::open_in_memory().unwrap();
        let fk: i64 = db
            .connection()
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(fk, 1, "foreign key enforcement must be on");
    }

    /// A unique temp directory for a test, cleaned up on drop.
    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let unique = format!(
                "novrix-persist-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn migration_is_idempotent_across_reopen() {
        let dir = TempDir::new("reopen");
        let path = dir.path.join("novrix.db");

        // First run creates the file and migrates it.
        {
            let db = Database::open(&path).unwrap();
            assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        }
        // Second run opens an already-migrated database without error and
        // leaves the version unchanged.
        {
            let db = Database::open(&path).unwrap();
            assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        }
    }

    #[test]
    fn open_creates_missing_parent_directory_on_first_run() {
        let dir = TempDir::new("nested");
        let path = dir.path.join("nested").join("sub").join("novrix.db");
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        assert!(path.exists());
    }

    #[test]
    fn database_newer_than_build_is_rejected() {
        // Simulate a database written by a future build: user_version ahead of
        // what this build understands.
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", (SCHEMA_VERSION + 1) as i64)
            .unwrap();

        let mut db = Database { conn };
        let err = db.migrate().unwrap_err();
        assert_eq!(err.code, "persistence.schemaTooNew");
    }

    #[test]
    fn stored_uuid_ids_round_trip_as_stable_text() {
        use crate::domain::{FolderId, ProfileId};

        let db = Database::open_in_memory().unwrap();
        let folder_id = FolderId::new();
        let profile_id = ProfileId::new();

        db.connection()
            .execute(
                "INSERT INTO folders (id, name, parent_id, depth) VALUES (?1, ?2, NULL, 0)",
                rusqlite::params![folder_id.to_string(), "servers"],
            )
            .unwrap();
        db.connection()
            .execute(
                "INSERT INTO profiles \
                 (id, name, protocol, host, port, folder_id, auth_kind, credential_key, key_id, \
                  protocol_params, logging_enabled, log_path, connect_timeout_secs) \
                 VALUES (?1, ?2, 'ssh', 'example.com', 22, ?3, 'none', NULL, NULL, '{}', 0, NULL, 30)",
                rusqlite::params![profile_id.to_string(), "prod", folder_id.to_string()],
            )
            .unwrap();

        let stored: String = db
            .connection()
            .query_row(
                "SELECT id FROM profiles WHERE id = ?1",
                [profile_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, profile_id.to_string());
    }
}
