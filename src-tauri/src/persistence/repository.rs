//! Profile and folder repository helpers (task 2.2).
//!
//! This module layers CRUD read/write helpers on top of the [`Database`] handle
//! from [`super`] (task 2.1). The Connection_Manager (task 3.x) calls these to
//! persist and load [`Folder`]s and [`ConnectionProfile`]s; the *validation*
//! that gates those calls (host present, port `1..=65535`, supported protocol,
//! folder name/hierarchy rules) belongs to the Connection_Manager and is
//! deliberately **not** here. This layer is faithful persistence only.
//!
//! # Write integrity
//!
//! Every write runs inside a SQLite transaction. If any statement in a write
//! fails, the transaction is rolled back (on drop) and the previously stored
//! state is left unchanged — satisfying Requirement 2.6 (a failed save retains
//! the prior stored state) at the storage layer.
//!
//! # Row ↔ domain mapping
//!
//! The schema (see [`super`]) stores credential *references* only, never
//! secrets. [`AuthConfig`] is flattened into three columns:
//!
//! - `auth_kind` — `"none" | "password" | "sshKey"`,
//! - `credential_key` — the Secure_Storage key name (password auth only),
//! - `key_id` — a [`KeyId`] UUID (ssh-key auth only).
//!
//! [`ProtocolParams`] is stored as a JSON blob in `protocol_params`, matching
//! the tagged enum and keeping the protocol set extensible. All reads reverse
//! these mappings back into domain types.

use rusqlite::{OptionalExtension, Row};

use crate::domain::{
    AuthConfig, CommandSnippet, ConnectionProfile, CredentialKey, DomainError, Folder, FolderId,
    KeyId, ProfileId, ProtocolKind, SnippetId,
};

use super::Database;

/// Error code namespace used by every repository failure.
const ERR_WRITE: &str = "persistence.write";
const ERR_READ: &str = "persistence.read";
const ERR_SERIALIZE: &str = "persistence.serialize";
const ERR_DESERIALIZE: &str = "persistence.deserialize";

impl Database {
    // --- Folder CRUD -------------------------------------------------------

    /// Insert a new folder, failing if a folder with the same id already
    /// exists. On any failure the database is left unchanged.
    pub fn insert_folder(&self, folder: &Folder) -> Result<(), DomainError> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT INTO folders (id, name, parent_id, depth) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    folder.id.to_string(),
                    folder.name,
                    folder.parent_id.map(|p| p.to_string()),
                    folder.depth as i64,
                ],
            )?;
            Ok(())
        })
    }

    /// Read a single folder by id, or `None` if it does not exist.
    pub fn get_folder(&self, id: FolderId) -> Result<Option<Folder>, DomainError> {
        self.connection()
            .query_row(
                "SELECT id, name, parent_id, depth FROM folders WHERE id = ?1",
                [id.to_string()],
                map_folder_row,
            )
            .optional()
            .map_err(|e| read_error("Failed to read folder", e))?
            .transpose()
    }

    /// Read every folder, ordered by depth then name for a stable listing.
    pub fn list_folders(&self) -> Result<Vec<Folder>, DomainError> {
        let conn = self.connection();
        let mut stmt = conn
            .prepare("SELECT id, name, parent_id, depth FROM folders ORDER BY depth, name")
            .map_err(|e| read_error("Failed to prepare folder listing", e))?;
        let rows = stmt
            .query_map([], map_folder_row)
            .map_err(|e| read_error("Failed to query folders", e))?;
        collect_rows(rows, "Failed to read folder row")
    }

    /// Update an existing folder's mutable fields (name, parent, depth). Fails
    /// if no folder with the given id exists. On failure the prior state is
    /// retained.
    pub fn update_folder(&self, folder: &Folder) -> Result<(), DomainError> {
        self.with_write_tx(|tx| {
            let affected = tx.execute(
                "UPDATE folders SET name = ?2, parent_id = ?3, depth = ?4 WHERE id = ?1",
                rusqlite::params![
                    folder.id.to_string(),
                    folder.name,
                    folder.parent_id.map(|p| p.to_string()),
                    folder.depth as i64,
                ],
            )?;
            if affected == 0 {
                return Err(RepoError::NotFound {
                    entity: "folder",
                    id: folder.id.to_string(),
                });
            }
            Ok(())
        })
    }

    /// Delete a folder by id. The schema's `ON DELETE CASCADE` removes nested
    /// folders and contained profiles as part of the same transaction, so the
    /// subtree is removed atomically. Returns whether a folder was removed.
    pub fn delete_folder(&self, id: FolderId) -> Result<bool, DomainError> {
        self.with_write_tx(|tx| {
            let affected = tx.execute("DELETE FROM folders WHERE id = ?1", [id.to_string()])?;
            Ok(affected > 0)
        })
    }

    /// Apply a batch of folder updates inside a single transaction.
    ///
    /// Every folder in `folders` is updated with its new `name`, `parent_id`,
    /// and `depth`; if any update targets a non-existent folder the whole batch
    /// is rolled back and `persistence.notFound` is returned, leaving the prior
    /// stored state untouched. This is what a folder move (task 3.10) needs:
    /// reparenting a folder and re-depthing its entire subtree must land as one
    /// all-or-nothing write so a failure never leaves the hierarchy with a
    /// moved parent but stale descendant depths.
    pub fn update_folders(&self, folders: &[Folder]) -> Result<(), DomainError> {
        self.with_write_tx(|tx| {
            for folder in folders {
                let affected = tx.execute(
                    "UPDATE folders SET name = ?2, parent_id = ?3, depth = ?4 WHERE id = ?1",
                    rusqlite::params![
                        folder.id.to_string(),
                        folder.name,
                        folder.parent_id.map(|p| p.to_string()),
                        folder.depth as i64,
                    ],
                )?;
                if affected == 0 {
                    return Err(RepoError::NotFound {
                        entity: "folder",
                        id: folder.id.to_string(),
                    });
                }
            }
            Ok(())
        })
    }

    /// Insert a batch of imported folders and profiles in a single transaction
    /// (task 18.1).
    ///
    /// This is the persistence half of configuration import: the folders and
    /// profiles have already been given freshly generated ids and had their
    /// internal references remapped by the Connection_Manager's pure import
    /// logic, so this method only has to write them. Everything lands in one
    /// transaction, so a failure part-way through rolls the whole batch back and
    /// leaves the previously stored configuration untouched (Requirement 16.2) —
    /// an import never half-applies.
    ///
    /// Folders are written parent-before-child by sorting on `depth` ascending,
    /// so a folder's `parent_id` foreign key always references a row already
    /// inserted within the same transaction. Profiles are written afterward so
    /// their `folder_id` foreign keys resolve too. The caller guarantees the ids
    /// are fresh, so these are always `INSERT`s of new rows — no existing folder
    /// or profile is read, modified, or deleted.
    pub fn import_items(
        &self,
        folders: &[Folder],
        profiles: &[ConnectionProfile],
    ) -> Result<(), DomainError> {
        // Serialize profile rows up front so a serialization failure is reported
        // without having opened a transaction.
        let profile_rows: Vec<ProfileRow> = profiles
            .iter()
            .map(ProfileRow::from_domain)
            .collect::<Result<_, _>>()?;

        // Order folders so parents precede children (depth ascending), keeping
        // the self-referencing foreign key satisfiable within the transaction.
        let mut ordered_folders: Vec<&Folder> = folders.iter().collect();
        ordered_folders.sort_by_key(|f| f.depth);

        self.with_write_tx(|tx| {
            for folder in ordered_folders {
                tx.execute(
                    "INSERT INTO folders (id, name, parent_id, depth) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![
                        folder.id.to_string(),
                        folder.name,
                        folder.parent_id.map(|p| p.to_string()),
                        folder.depth as i64,
                    ],
                )?;
            }
            for row in &profile_rows {
                tx.execute(
                    "INSERT INTO profiles \
                     (id, name, protocol, host, port, folder_id, auth_kind, credential_key, key_id, \
                      protocol_params, logging_enabled, log_path, connect_timeout_secs, username, domain) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                    rusqlite::params![
                        row.id,
                        row.name,
                        row.protocol,
                        row.host,
                        row.port,
                        row.folder_id,
                        row.auth_kind,
                        row.credential_key,
                        row.key_id,
                        row.protocol_params,
                        row.logging_enabled,
                        row.log_path,
                        row.connect_timeout_secs,
                        row.username,
                        row.domain,
                    ],
                )?;
            }
            Ok(())
        })
    }

    // --- Profile CRUD ------------------------------------------------------

    /// Insert a new profile, failing if a profile with the same id already
    /// exists or references a missing folder. On any failure the database is
    /// left unchanged.
    pub fn insert_profile(&self, profile: &ConnectionProfile) -> Result<(), DomainError> {
        let row = ProfileRow::from_domain(profile)?;
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT INTO profiles \
                 (id, name, protocol, host, port, folder_id, auth_kind, credential_key, key_id, \
                  protocol_params, logging_enabled, log_path, connect_timeout_secs, username, domain) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                rusqlite::params![
                    row.id,
                    row.name,
                    row.protocol,
                    row.host,
                    row.port,
                    row.folder_id,
                    row.auth_kind,
                    row.credential_key,
                    row.key_id,
                    row.protocol_params,
                    row.logging_enabled,
                    row.log_path,
                    row.connect_timeout_secs,
                    row.username,
                    row.domain,
                ],
            )?;
            Ok(())
        })
    }

    /// Read a single profile by id, or `None` if it does not exist.
    pub fn get_profile(&self, id: ProfileId) -> Result<Option<ConnectionProfile>, DomainError> {
        self.connection()
            .query_row(PROFILE_SELECT, [id.to_string()], map_profile_row)
            .optional()
            .map_err(|e| read_error("Failed to read profile", e))?
            .transpose()
    }

    /// Read every profile, ordered by name for a stable listing.
    pub fn list_profiles(&self) -> Result<Vec<ConnectionProfile>, DomainError> {
        let conn = self.connection();
        let mut stmt = conn
            .prepare(PROFILE_SELECT_ALL)
            .map_err(|e| read_error("Failed to prepare profile listing", e))?;
        let rows = stmt
            .query_map([], map_profile_row)
            .map_err(|e| read_error("Failed to query profiles", e))?;
        collect_rows(rows, "Failed to read profile row")
    }

    /// Read every profile associated with `folder_id`.
    pub fn list_profiles_in_folder(
        &self,
        folder_id: FolderId,
    ) -> Result<Vec<ConnectionProfile>, DomainError> {
        let conn = self.connection();
        let mut stmt = conn
            .prepare(PROFILE_SELECT_BY_FOLDER)
            .map_err(|e| read_error("Failed to prepare profile listing", e))?;
        let rows = stmt
            .query_map([folder_id.to_string()], map_profile_row)
            .map_err(|e| read_error("Failed to query profiles by folder", e))?;
        collect_rows(rows, "Failed to read profile row")
    }

    /// Update an existing profile's fields. Fails if no profile with the given
    /// id exists. On failure the prior stored state is retained unchanged.
    pub fn update_profile(&self, profile: &ConnectionProfile) -> Result<(), DomainError> {
        let row = ProfileRow::from_domain(profile)?;
        self.with_write_tx(|tx| {
            let affected = tx.execute(
                "UPDATE profiles SET \
                 name = ?2, protocol = ?3, host = ?4, port = ?5, folder_id = ?6, \
                 auth_kind = ?7, credential_key = ?8, key_id = ?9, protocol_params = ?10, \
                 logging_enabled = ?11, log_path = ?12, connect_timeout_secs = ?13, \
                 username = ?14, domain = ?15 \
                 WHERE id = ?1",
                rusqlite::params![
                    row.id,
                    row.name,
                    row.protocol,
                    row.host,
                    row.port,
                    row.folder_id,
                    row.auth_kind,
                    row.credential_key,
                    row.key_id,
                    row.protocol_params,
                    row.logging_enabled,
                    row.log_path,
                    row.connect_timeout_secs,
                    row.username,
                    row.domain,
                ],
            )?;
            if affected == 0 {
                return Err(RepoError::NotFound {
                    entity: "profile",
                    id: row.id,
                });
            }
            Ok(())
        })
    }

    /// Delete a profile by id. Returns whether a profile was removed.
    pub fn delete_profile(&self, id: ProfileId) -> Result<bool, DomainError> {
        self.with_write_tx(|tx| {
            let affected = tx.execute("DELETE FROM profiles WHERE id = ?1", [id.to_string()])?;
            Ok(affected > 0)
        })
    }

    // --- Transaction helper ------------------------------------------------

    /// Run `f` inside a transaction, committing on `Ok` and rolling back on any
    /// error. Rolling back leaves the previously stored state untouched, which
    /// is how write failures preserve prior state (Requirement 2.6).
    fn with_write_tx<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, RepoError>,
    ) -> Result<T, DomainError> {
        // `connection()` hands out `&Connection`; a transaction needs `&mut`,
        // so we go through the raw handle. We only ever hold one borrow at a
        // time, so an `unsafe` cast is avoided by using the connection's own
        // `unchecked_transaction`, which begins a transaction on a shared ref
        // and still rolls back on drop if not committed.
        let conn = self.connection();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| write_error("Failed to begin transaction", e))?;
        let value = f(&tx).map_err(RepoError::into_domain)?;
        tx.commit()
            .map_err(|e| write_error("Failed to commit transaction", e))?;
        Ok(value)
    }

    // --- Command_Snippet CRUD (Requirement 19) ----------------------------

    /// Insert a new snippet. On any failure the database is left unchanged.
    pub fn insert_snippet(&self, snippet: &CommandSnippet) -> Result<(), DomainError> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT INTO snippets (id, name, command, protocol) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    snippet.id.to_string(),
                    snippet.name,
                    snippet.command,
                    snippet.protocol.map(|p| protocol_to_str(p).to_string()),
                ],
            )?;
            Ok(())
        })
    }

    /// Read every snippet, ordered by name for a stable listing.
    pub fn list_snippets(&self) -> Result<Vec<CommandSnippet>, DomainError> {
        let conn = self.connection();
        let mut stmt = conn
            .prepare("SELECT id, name, command, protocol FROM snippets ORDER BY name")
            .map_err(|e| read_error("Failed to prepare snippet listing", e))?;
        let rows = stmt
            .query_map([], map_snippet_row)
            .map_err(|e| read_error("Failed to query snippets", e))?;
        collect_rows(rows, "Failed to read snippet row")
    }

    /// Update an existing snippet's mutable fields. Fails if no snippet with the
    /// given id exists; on failure the prior state is retained.
    pub fn update_snippet(&self, snippet: &CommandSnippet) -> Result<(), DomainError> {
        self.with_write_tx(|tx| {
            let affected = tx.execute(
                "UPDATE snippets SET name = ?2, command = ?3, protocol = ?4 WHERE id = ?1",
                rusqlite::params![
                    snippet.id.to_string(),
                    snippet.name,
                    snippet.command,
                    snippet.protocol.map(|p| protocol_to_str(p).to_string()),
                ],
            )?;
            if affected == 0 {
                return Err(RepoError::NotFound {
                    entity: "snippet",
                    id: snippet.id.to_string(),
                });
            }
            Ok(())
        })
    }

    /// Delete a snippet by id. Returns whether a snippet was removed.
    pub fn delete_snippet(&self, id: SnippetId) -> Result<bool, DomainError> {
        self.with_write_tx(|tx| {
            let affected = tx.execute("DELETE FROM snippets WHERE id = ?1", [id.to_string()])?;
            Ok(affected > 0)
        })
    }
}

// --- Row model + mapping ---------------------------------------------------

const PROFILE_COLUMNS: &str = "id, name, protocol, host, port, folder_id, auth_kind, \
     credential_key, key_id, protocol_params, logging_enabled, log_path, connect_timeout_secs, \
     username, domain";

const PROFILE_SELECT: &str =
    "SELECT id, name, protocol, host, port, folder_id, auth_kind, credential_key, key_id, \
     protocol_params, logging_enabled, log_path, connect_timeout_secs, username, domain \
     FROM profiles WHERE id = ?1";

const PROFILE_SELECT_ALL: &str =
    "SELECT id, name, protocol, host, port, folder_id, auth_kind, credential_key, key_id, \
     protocol_params, logging_enabled, log_path, connect_timeout_secs, username, domain \
     FROM profiles ORDER BY name";

const PROFILE_SELECT_BY_FOLDER: &str =
    "SELECT id, name, protocol, host, port, folder_id, auth_kind, credential_key, key_id, \
     protocol_params, logging_enabled, log_path, connect_timeout_secs, username, domain \
     FROM profiles WHERE folder_id = ?1 ORDER BY name";

// Reference the constant so a future column change triggers a single edit site.
#[allow(dead_code)]
const _COLUMNS_DOC: &str = PROFILE_COLUMNS;

/// The flattened SQLite representation of a [`ConnectionProfile`]. Holds owned,
/// already-serialized column values ready to bind into a statement.
struct ProfileRow {
    id: String,
    name: String,
    protocol: String,
    host: String,
    port: i64,
    folder_id: Option<String>,
    auth_kind: String,
    credential_key: Option<String>,
    key_id: Option<String>,
    protocol_params: String,
    logging_enabled: i64,
    log_path: Option<String>,
    connect_timeout_secs: i64,
    username: Option<String>,
    domain: Option<String>,
}

impl ProfileRow {
    /// Flatten a domain profile into its column values, serializing the auth
    /// reference and protocol params. Never serializes a secret value.
    fn from_domain(profile: &ConnectionProfile) -> Result<Self, DomainError> {
        let (auth_kind, credential_key, key_id) = auth_to_columns(&profile.auth);
        let protocol_params = serde_json::to_string(&profile.protocol_params)
            .map_err(|e| serialize_error("protocol_params", e))?;
        Ok(Self {
            id: profile.id.to_string(),
            name: profile.name.clone(),
            protocol: protocol_to_str(profile.protocol).to_string(),
            host: profile.host.clone(),
            port: profile.port as i64,
            folder_id: profile.folder_id.map(|f| f.to_string()),
            auth_kind,
            credential_key,
            key_id,
            protocol_params,
            logging_enabled: profile.logging_enabled as i64,
            log_path: profile
                .log_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            connect_timeout_secs: profile.connect_timeout_secs as i64,
            username: profile.username.clone(),
            domain: profile.domain.clone(),
        })
    }
}

/// Map a `folders` row into a [`Folder`].
fn map_folder_row(row: &Row<'_>) -> rusqlite::Result<Result<Folder, DomainError>> {
    let id_str: String = row.get("id")?;
    let name: String = row.get("name")?;
    let parent_str: Option<String> = row.get("parent_id")?;
    let depth: i64 = row.get("depth")?;
    Ok((|| {
        Ok(Folder {
            id: FolderId::from(parse_uuid(&id_str, "folder.id")?),
            name,
            parent_id: match parent_str {
                Some(s) => Some(FolderId::from(parse_uuid(&s, "folder.parent_id")?)),
                None => None,
            },
            depth: depth as u8,
        })
    })())
}

/// Map a `profiles` row into a [`ConnectionProfile`], reversing the auth and
/// protocol-params serialization.
fn map_profile_row(row: &Row<'_>) -> rusqlite::Result<Result<ConnectionProfile, DomainError>> {
    let id_str: String = row.get("id")?;
    let name: String = row.get("name")?;
    let protocol_str: String = row.get("protocol")?;
    let host: String = row.get("host")?;
    let port: i64 = row.get("port")?;
    let folder_str: Option<String> = row.get("folder_id")?;
    let auth_kind: String = row.get("auth_kind")?;
    let credential_key: Option<String> = row.get("credential_key")?;
    let key_id: Option<String> = row.get("key_id")?;
    let protocol_params_json: String = row.get("protocol_params")?;
    let logging_enabled: i64 = row.get("logging_enabled")?;
    let log_path: Option<String> = row.get("log_path")?;
    let connect_timeout_secs: i64 = row.get("connect_timeout_secs")?;
    let username: Option<String> = row.get("username")?;
    let domain: Option<String> = row.get("domain")?;

    Ok((|| {
        Ok(ConnectionProfile {
            id: ProfileId::from(parse_uuid(&id_str, "profile.id")?),
            name,
            protocol: protocol_from_str(&protocol_str)?,
            host,
            port: port as u16,
            username,
            domain,
            folder_id: match folder_str {
                Some(s) => Some(FolderId::from(parse_uuid(&s, "profile.folder_id")?)),
                None => None,
            },
            auth: auth_from_columns(&auth_kind, credential_key, key_id)?,
            protocol_params: serde_json::from_str(&protocol_params_json)
                .map_err(|e| deserialize_error("protocol_params", e))?,
            logging_enabled: logging_enabled != 0,
            log_path: log_path.map(std::path::PathBuf::from),
            connect_timeout_secs: connect_timeout_secs as u16,
        })
    })())
}

// --- AuthConfig <-> columns ------------------------------------------------

/// Flatten an [`AuthConfig`] into `(auth_kind, credential_key, key_id)`.
fn auth_to_columns(auth: &AuthConfig) -> (String, Option<String>, Option<String>) {
    match auth {
        AuthConfig::None => ("none".to_string(), None, None),
        AuthConfig::Password { credential_key } => (
            "password".to_string(),
            Some(credential_key.as_str().to_string()),
            None,
        ),
        AuthConfig::SshKey { key_id } => ("sshKey".to_string(), None, Some(key_id.to_string())),
    }
}

/// Reconstruct an [`AuthConfig`] from its three columns, rejecting rows whose
/// `auth_kind` is unknown or whose required reference column is missing.
fn auth_from_columns(
    auth_kind: &str,
    credential_key: Option<String>,
    key_id: Option<String>,
) -> Result<AuthConfig, DomainError> {
    match auth_kind {
        "none" => Ok(AuthConfig::None),
        "password" => {
            let key = credential_key.ok_or_else(|| {
                deserialize_simple(
                    "auth.credential_key",
                    "password auth is missing credential_key",
                )
            })?;
            Ok(AuthConfig::Password {
                credential_key: CredentialKey::new(key),
            })
        }
        "sshKey" => {
            let raw = key_id.ok_or_else(|| {
                deserialize_simple("auth.key_id", "ssh-key auth is missing key_id")
            })?;
            Ok(AuthConfig::SshKey {
                key_id: KeyId::from(parse_uuid(&raw, "auth.key_id")?),
            })
        }
        other => Err(deserialize_simple(
            "auth.auth_kind",
            format!("unknown auth_kind `{other}`"),
        )),
    }
}

// --- ProtocolKind <-> text -------------------------------------------------

/// Serialize a [`ProtocolKind`] to its stored text form (matches the
/// camelCase `serde` tag used elsewhere).
fn protocol_to_str(kind: ProtocolKind) -> &'static str {
    match kind {
        ProtocolKind::Ssh => "ssh",
        ProtocolKind::Telnet => "telnet",
        ProtocolKind::Serial => "serial",
        ProtocolKind::Rdp => "rdp",
    }
}

/// Parse a stored protocol text back into a [`ProtocolKind`].
fn protocol_from_str(s: &str) -> Result<ProtocolKind, DomainError> {
    match s {
        "ssh" => Ok(ProtocolKind::Ssh),
        "telnet" => Ok(ProtocolKind::Telnet),
        "serial" => Ok(ProtocolKind::Serial),
        "rdp" => Ok(ProtocolKind::Rdp),
        other => Err(deserialize_simple(
            "profile.protocol",
            format!("unknown protocol `{other}`"),
        )),
    }
}

/// Map a `snippets` row (id, name, command, protocol) into a [`CommandSnippet`].
fn map_snippet_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<CommandSnippet, DomainError>> {
    let id_str: String = row.get(0)?;
    let name: String = row.get(1)?;
    let command: String = row.get(2)?;
    let protocol_str: Option<String> = row.get(3)?;
    Ok((|| {
        let protocol = match protocol_str.as_deref() {
            None => None,
            Some(s) => Some(protocol_from_str(s)?),
        };
        Ok(CommandSnippet {
            id: SnippetId::from(parse_uuid(&id_str, "snippet.id")?),
            name,
            command,
            protocol,
        })
    })())
}

// --- Shared helpers --------------------------------------------------------

/// Parse a stored UUID string, mapping a malformed value to a domain error.
fn parse_uuid(s: &str, field: &str) -> Result<uuid::Uuid, DomainError> {
    uuid::Uuid::parse_str(s).map_err(|e| {
        DomainError::new(ERR_DESERIALIZE, format!("Invalid UUID for {field}"))
            .with_cause(e.to_string())
    })
}

/// Collect an iterator of per-row `Result<Result<T, DomainError>, rusqlite::Error>`
/// into a `Vec<T>`, surfacing the first error of either layer.
fn collect_rows<T>(
    rows: impl Iterator<Item = rusqlite::Result<Result<T, DomainError>>>,
    msg: &str,
) -> Result<Vec<T>, DomainError> {
    let mut out = Vec::new();
    for row in rows {
        let item = row.map_err(|e| read_error(msg, e))??;
        out.push(item);
    }
    Ok(out)
}

fn read_error(message: &str, cause: impl std::fmt::Display) -> DomainError {
    DomainError::new(ERR_READ, message).with_cause(cause.to_string())
}

fn write_error(message: &str, cause: impl std::fmt::Display) -> DomainError {
    DomainError::new(ERR_WRITE, message).with_cause(cause.to_string())
}

fn serialize_error(field: &str, cause: impl std::fmt::Display) -> DomainError {
    DomainError::new(ERR_SERIALIZE, format!("Failed to serialize {field}"))
        .with_cause(cause.to_string())
}

fn deserialize_error(field: &str, cause: impl std::fmt::Display) -> DomainError {
    DomainError::new(ERR_DESERIALIZE, format!("Failed to deserialize {field}"))
        .with_cause(cause.to_string())
}

fn deserialize_simple(code_suffix: &str, message: impl Into<String>) -> DomainError {
    DomainError::new(format!("{ERR_DESERIALIZE}.{code_suffix}"), message.into())
}

/// Internal error type for the transaction closures. Carries either a mapped
/// `rusqlite` failure or a repository-level condition (e.g. not found) so the
/// closure can bail and trigger a rollback, after which it is converted to a
/// [`DomainError`].
enum RepoError {
    Sqlite(rusqlite::Error),
    NotFound { entity: &'static str, id: String },
}

impl From<rusqlite::Error> for RepoError {
    fn from(e: rusqlite::Error) -> Self {
        RepoError::Sqlite(e)
    }
}

impl RepoError {
    fn into_domain(self) -> DomainError {
        match self {
            RepoError::Sqlite(e) => write_error("Write operation failed", e),
            RepoError::NotFound { entity, id } => DomainError::new(
                "persistence.notFound",
                format!("No {entity} found for id {id}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        BaudRate, DataBits, HostKeyPolicy, Parity, ProtocolParams, Resolution, StopBits,
    };

    fn sample_folder() -> Folder {
        Folder {
            id: FolderId::new(),
            name: "servers".to_string(),
            parent_id: None,
            depth: 0,
        }
    }

    fn sample_profile(folder_id: Option<FolderId>) -> ConnectionProfile {
        ConnectionProfile {
            id: ProfileId::new(),
            name: "prod-web".to_string(),
            protocol: ProtocolKind::Ssh,
            host: "example.com".to_string(),
            port: 22,
            username: Some("deploy".to_string()),
            domain: None,
            folder_id,
            auth: AuthConfig::Password {
                credential_key: CredentialKey::new("profile/1/password"),
            },
            protocol_params: ProtocolParams::Ssh {
                known_host_policy: HostKeyPolicy::PromptOnUnknown,
            },
            logging_enabled: false,
            log_path: None,
            connect_timeout_secs: 30,
        }
    }

    /// Build a profile with a specific protocol + params + auth so a test can
    /// assert that a given variant combination round-trips faithfully.
    fn profile_with(
        folder_id: Option<FolderId>,
        protocol: ProtocolKind,
        auth: AuthConfig,
        protocol_params: ProtocolParams,
    ) -> ConnectionProfile {
        ConnectionProfile {
            id: ProfileId::new(),
            name: "fixture".to_string(),
            protocol,
            host: "host.example".to_string(),
            port: 2222,
            username: Some("svc\\account".to_string()),
            domain: Some("CORP".to_string()),
            folder_id,
            auth,
            protocol_params,
            logging_enabled: true,
            log_path: Some(std::path::PathBuf::from("/tmp/novrix/session.log")),
            connect_timeout_secs: 45,
        }
    }

    #[test]
    fn folder_insert_read_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        let folder = sample_folder();
        db.insert_folder(&folder).unwrap();
        let read = db.get_folder(folder.id).unwrap().unwrap();
        assert_eq!(read, folder);
    }

    #[test]
    fn profile_insert_read_roundtrip_preserves_auth_and_params() {
        let db = Database::open_in_memory().unwrap();
        let folder = sample_folder();
        db.insert_folder(&folder).unwrap();
        let profile = sample_profile(Some(folder.id));
        db.insert_profile(&profile).unwrap();
        let read = db.get_profile(profile.id).unwrap().unwrap();
        assert_eq!(read, profile);
    }

    #[test]
    fn profile_login_identity_round_trips_through_persistence() {
        // A saved profile carrying a login identity (username + domain) reads
        // back with both fields intact, and a later update that changes them is
        // likewise persisted faithfully (task 26.1).
        let db = Database::open_in_memory().unwrap();

        let mut profile = sample_profile(None);
        profile.username = Some("corp-admin".to_string());
        profile.domain = Some("CORP".to_string());
        db.insert_profile(&profile).unwrap();

        let read = db.get_profile(profile.id).unwrap().unwrap();
        assert_eq!(read.username, Some("corp-admin".to_string()));
        assert_eq!(read.domain, Some("CORP".to_string()));
        assert_eq!(read, profile);

        // A `None` identity also round-trips (stored as NULL, read back None).
        let edited = ConnectionProfile {
            username: None,
            domain: None,
            ..profile
        };
        db.update_profile(&edited).unwrap();
        let read = db.get_profile(edited.id).unwrap().unwrap();
        assert_eq!(read.username, None);
        assert_eq!(read.domain, None);
    }

    #[test]
    fn failed_insert_leaves_prior_state_intact() {
        let db = Database::open_in_memory().unwrap();
        let folder = sample_folder();
        db.insert_folder(&folder).unwrap();
        let profile = sample_profile(Some(folder.id));
        db.insert_profile(&profile).unwrap();

        // Re-inserting the same id violates the primary key and must fail,
        // leaving the single stored profile unchanged.
        let err = db.insert_profile(&profile).unwrap_err();
        assert_eq!(err.code, "persistence.write");
        assert_eq!(db.list_profiles().unwrap().len(), 1);
    }

    #[test]
    fn update_missing_profile_reports_not_found() {
        let db = Database::open_in_memory().unwrap();
        let err = db.update_profile(&sample_profile(None)).unwrap_err();
        assert_eq!(err.code, "persistence.notFound");
    }

    #[test]
    fn delete_folder_cascades_to_profiles() {
        let db = Database::open_in_memory().unwrap();
        let folder = sample_folder();
        db.insert_folder(&folder).unwrap();
        db.insert_profile(&sample_profile(Some(folder.id))).unwrap();

        assert!(db.delete_folder(folder.id).unwrap());
        assert!(db.get_folder(folder.id).unwrap().is_none());
        assert_eq!(db.list_profiles().unwrap().len(), 0);
    }

    // --- Full CRUD round-trips --------------------------------------------

    #[test]
    fn folder_full_crud_roundtrip_ends_in_none() {
        let db = Database::open_in_memory().unwrap();
        let mut folder = sample_folder();

        // insert -> read
        db.insert_folder(&folder).unwrap();
        assert_eq!(db.get_folder(folder.id).unwrap().unwrap(), folder);

        // update -> read reflects every mutable field (name, parent_id, depth)
        let parent = Folder {
            id: FolderId::new(),
            name: "root".to_string(),
            parent_id: None,
            depth: 0,
        };
        db.insert_folder(&parent).unwrap();
        folder.name = "renamed".to_string();
        folder.parent_id = Some(parent.id);
        folder.depth = 1;
        db.update_folder(&folder).unwrap();
        assert_eq!(db.get_folder(folder.id).unwrap().unwrap(), folder);

        // delete -> read returns None (the parent is untouched)
        assert!(db.delete_folder(folder.id).unwrap());
        assert!(db.get_folder(folder.id).unwrap().is_none());
        assert!(db.get_folder(parent.id).unwrap().is_some());
    }

    #[test]
    fn profile_full_crud_roundtrip_ends_in_none() {
        let db = Database::open_in_memory().unwrap();
        let folder = sample_folder();
        let other_folder = Folder {
            id: FolderId::new(),
            name: "staging".to_string(),
            parent_id: None,
            depth: 0,
        };
        db.insert_folder(&folder).unwrap();
        db.insert_folder(&other_folder).unwrap();

        let mut profile = sample_profile(Some(folder.id));

        // insert -> read
        db.insert_profile(&profile).unwrap();
        assert_eq!(db.get_profile(profile.id).unwrap().unwrap(), profile);

        // update -> read reflects the mutated fields, including a folder move
        // and an auth-kind change (password -> ssh key).
        profile.name = "edited".to_string();
        profile.host = "renamed.example".to_string();
        profile.port = 2200;
        profile.folder_id = Some(other_folder.id);
        profile.auth = AuthConfig::SshKey {
            key_id: KeyId::new(),
        };
        profile.logging_enabled = true;
        profile.connect_timeout_secs = 90;
        db.update_profile(&profile).unwrap();
        assert_eq!(db.get_profile(profile.id).unwrap().unwrap(), profile);

        // delete -> read returns None
        assert!(db.delete_profile(profile.id).unwrap());
        assert!(db.get_profile(profile.id).unwrap().is_none());
    }

    // --- list_profiles_in_folder ------------------------------------------

    #[test]
    fn list_profiles_in_folder_returns_only_members_sorted_by_name() {
        let db = Database::open_in_memory().unwrap();
        let folder_a = sample_folder();
        let folder_b = Folder {
            id: FolderId::new(),
            name: "clients".to_string(),
            parent_id: None,
            depth: 0,
        };
        db.insert_folder(&folder_a).unwrap();
        db.insert_folder(&folder_b).unwrap();

        // Two profiles in folder A (inserted out of name order) and one in B.
        let mut a_zulu = sample_profile(Some(folder_a.id));
        a_zulu.name = "zulu".to_string();
        let mut a_alpha = sample_profile(Some(folder_a.id));
        a_alpha.name = "alpha".to_string();
        let mut b_only = sample_profile(Some(folder_b.id));
        b_only.name = "b-only".to_string();
        // A root profile that belongs to no folder must never appear.
        let root = sample_profile(None);

        db.insert_profile(&a_zulu).unwrap();
        db.insert_profile(&a_alpha).unwrap();
        db.insert_profile(&b_only).unwrap();
        db.insert_profile(&root).unwrap();

        let in_a = db.list_profiles_in_folder(folder_a.id).unwrap();
        let names: Vec<&str> = in_a.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alpha", "zulu"],
            "sorted by name, folder A only"
        );

        let in_b = db.list_profiles_in_folder(folder_b.id).unwrap();
        assert_eq!(in_b.len(), 1);
        assert_eq!(in_b[0].name, "b-only");
    }

    #[test]
    fn list_profiles_in_folder_is_empty_for_folder_without_profiles() {
        let db = Database::open_in_memory().unwrap();
        let folder = sample_folder();
        db.insert_folder(&folder).unwrap();
        assert!(db.list_profiles_in_folder(folder.id).unwrap().is_empty());
    }

    // --- Write-failure state preservation (Requirement 2.6) ---------------

    #[test]
    fn failed_folder_insert_duplicate_id_preserves_prior_state() {
        let db = Database::open_in_memory().unwrap();
        let mut folder = sample_folder();
        db.insert_folder(&folder).unwrap();

        // A second folder sharing the id but with a different name violates the
        // primary key. The write must fail and the stored row must be the
        // original, untouched by the rejected name.
        folder.name = "would-overwrite".to_string();
        let err = db.insert_folder(&folder).unwrap_err();
        assert_eq!(err.code, "persistence.write");

        let stored = db.get_folder(folder.id).unwrap().unwrap();
        assert_eq!(stored.name, "servers", "prior name must be unchanged");
        assert_eq!(db.list_folders().unwrap().len(), 1);
    }

    #[test]
    fn update_missing_folder_reports_not_found_and_adds_nothing() {
        let db = Database::open_in_memory().unwrap();
        let before = db.list_folders().unwrap().len();

        let err = db.update_folder(&sample_folder()).unwrap_err();
        assert_eq!(err.code, "persistence.notFound");

        // A failed update must never create a row.
        assert_eq!(db.list_folders().unwrap().len(), before);
    }

    #[test]
    fn failed_profile_update_notfound_leaves_existing_profiles_unchanged() {
        let db = Database::open_in_memory().unwrap();
        let folder = sample_folder();
        db.insert_folder(&folder).unwrap();
        let existing = sample_profile(Some(folder.id));
        db.insert_profile(&existing).unwrap();

        // Updating a profile whose id was never stored must report notFound and
        // leave the one existing profile exactly as it was.
        let err = db
            .update_profile(&sample_profile(Some(folder.id)))
            .unwrap_err();
        assert_eq!(err.code, "persistence.notFound");

        let all = db.list_profiles().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], existing);
    }

    #[test]
    fn delete_missing_profile_returns_false_without_touching_others() {
        let db = Database::open_in_memory().unwrap();
        let folder = sample_folder();
        db.insert_folder(&folder).unwrap();
        let keep = sample_profile(Some(folder.id));
        db.insert_profile(&keep).unwrap();

        // Deleting an id that is not present is a no-op that reports `false`.
        assert!(!db.delete_profile(ProfileId::new()).unwrap());
        assert_eq!(db.list_profiles().unwrap(), vec![keep]);
    }

    #[test]
    fn delete_missing_folder_returns_false() {
        let db = Database::open_in_memory().unwrap();
        assert!(!db.delete_folder(FolderId::new()).unwrap());
    }

    // --- Faithful round-trips of every AuthConfig variant -----------------

    #[test]
    fn auth_none_round_trips() {
        let db = Database::open_in_memory().unwrap();
        let profile = profile_with(
            None,
            ProtocolKind::Telnet,
            AuthConfig::None,
            ProtocolParams::Telnet {},
        );
        db.insert_profile(&profile).unwrap();
        assert_eq!(db.get_profile(profile.id).unwrap().unwrap(), profile);
    }

    #[test]
    fn auth_password_round_trips() {
        let db = Database::open_in_memory().unwrap();
        let profile = profile_with(
            None,
            ProtocolKind::Ssh,
            AuthConfig::Password {
                credential_key: CredentialKey::new("profile/pw/secret-ref"),
            },
            ProtocolParams::Ssh {
                known_host_policy: HostKeyPolicy::Strict,
            },
        );
        db.insert_profile(&profile).unwrap();
        assert_eq!(db.get_profile(profile.id).unwrap().unwrap(), profile);
    }

    #[test]
    fn auth_ssh_key_round_trips() {
        let db = Database::open_in_memory().unwrap();
        let profile = profile_with(
            None,
            ProtocolKind::Ssh,
            AuthConfig::SshKey {
                key_id: KeyId::new(),
            },
            ProtocolParams::Ssh {
                known_host_policy: HostKeyPolicy::AcceptAny,
            },
        );
        db.insert_profile(&profile).unwrap();
        assert_eq!(db.get_profile(profile.id).unwrap().unwrap(), profile);
    }

    // --- Faithful round-trips of every ProtocolParams variant -------------

    #[test]
    fn protocol_params_ssh_round_trips() {
        let db = Database::open_in_memory().unwrap();
        let profile = profile_with(
            None,
            ProtocolKind::Ssh,
            AuthConfig::None,
            ProtocolParams::Ssh {
                known_host_policy: HostKeyPolicy::PromptOnUnknown,
            },
        );
        db.insert_profile(&profile).unwrap();
        assert_eq!(db.get_profile(profile.id).unwrap().unwrap(), profile);
    }

    #[test]
    fn protocol_params_telnet_round_trips() {
        let db = Database::open_in_memory().unwrap();
        let profile = profile_with(
            None,
            ProtocolKind::Telnet,
            AuthConfig::None,
            ProtocolParams::Telnet {},
        );
        db.insert_profile(&profile).unwrap();
        assert_eq!(db.get_profile(profile.id).unwrap().unwrap(), profile);
    }

    #[test]
    fn protocol_params_serial_round_trips() {
        let db = Database::open_in_memory().unwrap();
        let profile = profile_with(
            None,
            ProtocolKind::Serial,
            AuthConfig::None,
            ProtocolParams::Serial {
                port_name: "/dev/ttyUSB0".to_string(),
                baud: BaudRate::B115200,
                data_bits: DataBits::Eight,
                parity: Parity::Even,
                stop_bits: StopBits::Two,
            },
        );
        db.insert_profile(&profile).unwrap();
        assert_eq!(db.get_profile(profile.id).unwrap().unwrap(), profile);
    }

    #[test]
    fn protocol_params_rdp_round_trips_with_and_without_resolution() {
        let db = Database::open_in_memory().unwrap();

        let with_res = profile_with(
            None,
            ProtocolKind::Rdp,
            AuthConfig::Password {
                credential_key: CredentialKey::new("profile/rdp/secret-ref"),
            },
            ProtocolParams::Rdp {
                cert_policy: HostKeyPolicy::Strict,
                resolution: Some(Resolution {
                    width: 1920,
                    height: 1080,
                }),
            },
        );
        db.insert_profile(&with_res).unwrap();
        assert_eq!(db.get_profile(with_res.id).unwrap().unwrap(), with_res);

        let without_res = profile_with(
            None,
            ProtocolKind::Rdp,
            AuthConfig::None,
            ProtocolParams::Rdp {
                resolution: None,
                cert_policy: HostKeyPolicy::PromptOnUnknown,
            },
        );
        db.insert_profile(&without_res).unwrap();
        assert_eq!(
            db.get_profile(without_res.id).unwrap().unwrap(),
            without_res
        );
    }
}
