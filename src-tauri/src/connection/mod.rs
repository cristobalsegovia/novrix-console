//! Connection_Manager — profile validation and persistence (task 3.1).
//!
//! This module owns the Connection_Manager's profile half: it validates a
//! [`ProfileDraft`] against the rules the requirements demand and, on success,
//! persists the resulting [`ConnectionProfile`] through the task 2.2 repository
//! layer. Folder management, profile deletion, move operations, and
//! import/export are separate later tasks and are deliberately **not** here.
//!
//! # What this task implements
//!
//! - [`validate_draft`] — a *pure* validation function (no persistence) that
//!   enforces the three profile rules:
//!   - **host present** (Requirement 2.2): the host must be non-empty;
//!   - **port in `1..=65535`** (Requirement 2.3): because `port` is a [`u16`]
//!     the only out-of-range value representable is `0`, which is rejected;
//!   - **protocol is a Supported_Protocol** (Requirement 2.4): the draft's
//!     protocol must be among the set the System supports.
//!   Each rejection returns a [`ValidationError`] whose `field` names the
//!   offending input (`host`, `port`, or `protocol`), so the UI can point at
//!   the right control.
//!
//! - [`ConnectionManager::create_profile`] — validates a draft, assigns a fresh
//!   [`ProfileId`], persists it, and returns the stored [`ConnectionProfile`]
//!   (Requirement 2.1).
//!
//! - [`ConnectionManager::update_profile`] — applies the *identical* validation
//!   rules and persists the updated values against an existing id
//!   (Requirement 2.5).
//!
//! # Validation is separated from persistence
//!
//! [`validate_draft`] is a free function that takes only the draft and the set
//! of supported protocols. It touches no database, so it is cheap to exercise
//! across many inputs — exactly what the design's Properties 3, 4, and 5
//! (subtasks 3.2–3.4) need. The `ConnectionManager` methods call it first and
//! only reach the repository once a draft is known valid.
//!
//! # Persistence-failure semantics (Requirement 2.6)
//!
//! On create, the repository insert is transactional: a failed write rolls back
//! and nothing is stored. On update, the repository likewise rolls back on
//! failure, so an existing profile's prior stored state is retained unchanged.
//! In both cases the error returned by the repository is surfaced to the caller
//! so the failure can be named. See [`save_failure`] for how a raw repository
//! error is wrapped into a save-failure `DomainError`.

use crate::credential::{profile_password_key, CredentialStore, Secret};
use crate::domain::{
    AuthConfig, CommandSnippet, ConnectionProfile, DomainError, Folder, FolderId, ImportError,
    ProfileId, ProtocolKind, ProtocolParams, SnippetDraft, SnippetId, ValidationError,
};
use crate::persistence::Database;

/// Versioned export and additive import — the pure, persistence-free core
/// (parsing, id remapping, count reporting). The `export_config` /
/// `import_config` methods below wrap it with repository I/O (task 18.1).
mod importexport;

pub use importexport::{
    build_export, import_counts, parse_export, remap_import, serialize_export, ExportSummary,
    ImportSummary, EXPORT_SCHEMA_VERSION,
};

/// Inclusive bounds on a folder name's length, in characters (Requirements
/// 3.1, 3.2, 3.9, 3.10). A name shorter than `1` (empty) or longer than `255`
/// is rejected.
pub const FOLDER_NAME_MIN_CHARS: usize = 1;
pub const FOLDER_NAME_MAX_CHARS: usize = 255;

/// Inclusive bounds on a login identity value (username / domain) in
/// characters, applied only when the optional field is present. An empty
/// (whitespace-only) value or one longer than [`LOGIN_FIELD_MAX_CHARS`] is
/// rejected; `None` is always accepted (the connect layer supplies a default).
pub const LOGIN_FIELD_MIN_CHARS: usize = 1;
pub const LOGIN_FIELD_MAX_CHARS: usize = 256;

/// The set of protocols the System supports as Supported_Protocols.
///
/// Requirement 2.4 rejects a profile whose protocol is not a Supported_Protocol.
/// Today every [`ProtocolKind`] variant is supported, so [`validate_draft`]
/// checks membership against this list rather than hard-coding "always valid":
/// should a future draft arrive carrying a protocol the running build does not
/// support (e.g. a protocol disabled at registration time), the check already
/// rejects it and the list is the single edit site.
pub const SUPPORTED_PROTOCOLS: &[ProtocolKind] = &[
    ProtocolKind::Ssh,
    ProtocolKind::Telnet,
    ProtocolKind::Serial,
    ProtocolKind::Rdp,
];

/// The inbound data for creating or editing a [`ConnectionProfile`].
///
/// A draft is everything a profile is *except* its stable identity: there is no
/// [`ProfileId`] here. On create the manager assigns a fresh id; on update the
/// id is supplied separately so the same draft shape drives both paths and the
/// *identical* validation rules apply (Requirement 2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileDraft {
    /// Human-readable profile name.
    pub name: String,
    /// The connection protocol; must be a Supported_Protocol (Requirement 2.4).
    pub protocol: ProtocolKind,
    /// Remote host; must be non-empty (Requirement 2.2).
    pub host: String,
    /// TCP port; must be in `1..=65535` (Requirement 2.3).
    pub port: u16,
    /// Optional login username (SSH login user; RDP account name). When present
    /// it must be `1..=256` characters; when `None` the connect layer falls
    /// back to its documented per-protocol default.
    pub username: Option<String>,
    /// Optional login domain (RDP only). When present it must be `1..=256`
    /// characters; `None` means no domain.
    pub domain: Option<String>,
    /// Owning folder, or `None` for the root.
    pub folder_id: Option<crate::domain::FolderId>,
    /// How the session authenticates (credential references only, no secrets).
    pub auth: crate::domain::AuthConfig,
    /// Protocol-specific parameters.
    pub protocol_params: ProtocolParams,
    /// Whether session logging is enabled for this profile.
    pub logging_enabled: bool,
    /// Optional log destination path.
    pub log_path: Option<std::path::PathBuf>,
    /// Connect timeout in seconds (bound enforced per protocol at connect time).
    pub connect_timeout_secs: u16,
}

impl ProfileDraft {
    /// Materialize a validated draft into a [`ConnectionProfile`] carrying `id`.
    ///
    /// This performs no validation itself; callers must have run
    /// [`validate_draft`] first.
    fn into_profile(self, id: ProfileId) -> ConnectionProfile {
        // Normalize a password profile's credential key to the profile's own
        // canonical key (`profile/{id}/password`). The UI no longer asks the
        // user for a raw credential-key reference; the key is derived from the
        // profile id so it always matches where `save_credential` stores the
        // secret and where the connect path reads it back. Other auth kinds are
        // carried through unchanged.
        let auth = match self.auth {
            AuthConfig::Password { .. } => AuthConfig::Password {
                credential_key: profile_password_key(id),
            },
            other => other,
        };
        ConnectionProfile {
            id,
            name: self.name,
            protocol: self.protocol,
            host: self.host,
            port: self.port,
            username: self.username,
            domain: self.domain,
            folder_id: self.folder_id,
            auth,
            protocol_params: self.protocol_params,
            logging_enabled: self.logging_enabled,
            log_path: self.log_path,
            connect_timeout_secs: self.connect_timeout_secs,
        }
    }
}

/// Validate a [`ProfileDraft`] against the profile rules, returning the first
/// offending field as a [`ValidationError`].
///
/// This is a pure function: it reads only `draft` and `supported` and never
/// touches persistence, which keeps it trivially exercisable across many inputs
/// (design Properties 3–5). The checks run in a stable order so a draft with
/// several problems reports the first in field order:
///
/// 1. **host** (Requirement 2.2) — rejected when empty or whitespace-only.
/// 2. **port** (Requirement 2.3) — rejected when outside `1..=65535`. Since a
///    [`u16`] cannot exceed `65535`, the only invalid value is `0`.
/// 3. **protocol** (Requirement 2.4) — rejected when not in `supported`.
///
/// On success returns `Ok(())`; the caller may then persist the draft.
pub fn validate_draft(
    draft: &ProfileDraft,
    supported: &[ProtocolKind],
) -> Result<(), ValidationError> {
    if draft.host.trim().is_empty() {
        return Err(ValidationError {
            field: "host".to_string(),
            message: "Host is required.".to_string(),
        });
    }

    if !is_valid_port(draft.port) {
        return Err(ValidationError {
            field: "port".to_string(),
            message: format!("Port must be between 1 and 65535 (got {}).", draft.port),
        });
    }

    if !supported.contains(&draft.protocol) {
        return Err(ValidationError {
            field: "protocol".to_string(),
            message: "The selected protocol is not a supported protocol.".to_string(),
        });
    }

    // Optional login identity: validated only when present. These are not
    // required (Telnet/Serial never use them, and SSH/RDP fall back to a
    // documented default), but a supplied value must be non-empty and bounded.
    validate_login_field("username", draft.username.as_deref())?;
    validate_login_field("domain", draft.domain.as_deref())?;

    Ok(())
}

/// Validate an optional login identity `value` for `field`, enforcing the
/// `1..=256`-character bound only when a value is present.
///
/// `None` is always accepted — the field is optional and the connect layer
/// supplies a per-protocol default. A present value is rejected when it is
/// empty/whitespace-only (below [`LOGIN_FIELD_MIN_CHARS`]) or longer than
/// [`LOGIN_FIELD_MAX_CHARS`], with a [`ValidationError`] naming `field`.
fn validate_login_field(field: &str, value: Option<&str>) -> Result<(), ValidationError> {
    let Some(value) = value else {
        return Ok(());
    };

    if value.trim().is_empty() {
        return Err(ValidationError {
            field: field.to_string(),
            message: format!("{field} must not be empty when provided."),
        });
    }

    let len = value.chars().count();
    if len > LOGIN_FIELD_MAX_CHARS {
        return Err(ValidationError {
            field: field.to_string(),
            message: format!(
                "{field} must be at most {LOGIN_FIELD_MAX_CHARS} characters (got {len})."
            ),
        });
    }

    Ok(())
}

/// Whether `port` falls in the valid `1..=65535` range (Requirement 2.3).
///
/// `u16` already caps the upper bound at `65535`, so only `0` is out of range.
fn is_valid_port(port: u16) -> bool {
    (1..=65535).contains(&port)
}

/// Validate a folder `name` against its `siblings`' names, returning the first
/// offending rule as a [`ValidationError`] on `field = "name"`.
///
/// This is the pure core of folder create/rename: it reads only the candidate
/// name and the names it must be unique against, touches no persistence, and so
/// is trivially exercisable across many inputs — exactly what design Property 7
/// (subtask 3.9) needs. The `ConnectionManager` folder methods assemble the
/// sibling set from the repository and call this before any write.
///
/// The rules, applied in order (Requirements 3.1, 3.2, 3.9, 3.10):
///
/// 1. **non-empty** — a name whose character count is below
///    [`FOLDER_NAME_MIN_CHARS`] (i.e. empty) is rejected.
/// 2. **length** — a name whose character count exceeds
///    [`FOLDER_NAME_MAX_CHARS`] is rejected.
/// 3. **uniqueness among siblings** — a name equal to any entry in `siblings`
///    is rejected. The caller is responsible for building `siblings` from the
///    folders sharing the candidate's parent and for excluding the folder being
///    renamed, so a rename that keeps a folder's own name is not treated as a
///    self-collision.
///
/// Length is measured in Unicode scalar values (`chars().count()`), not bytes,
/// so a 255-character name of multi-byte characters is accepted. Uniqueness is
/// an exact, case-sensitive string comparison, matching how the names are
/// stored and displayed.
///
/// On success returns `Ok(())`; the caller may then persist the name.
/// Maximum length (in characters) of a snippet name or command.
pub const SNIPPET_NAME_MAX_CHARS: usize = 255;
pub const SNIPPET_COMMAND_MAX_CHARS: usize = 10_000;

/// Validate a [`SnippetDraft`] (Requirement 19.2): both `name` and `command`
/// must be non-empty (after trimming) and within length bounds. Returns the
/// first offending field as a [`ValidationError`]; a pure function so it is
/// trivially testable without persistence.
pub fn validate_snippet_draft(draft: &SnippetDraft) -> Result<(), ValidationError> {
    let name = draft.name.trim();
    if name.is_empty() {
        return Err(ValidationError {
            field: "name".to_string(),
            message: "A snippet name is required.".to_string(),
        });
    }
    if name.chars().count() > SNIPPET_NAME_MAX_CHARS {
        return Err(ValidationError {
            field: "name".to_string(),
            message: format!("A snippet name must be at most {SNIPPET_NAME_MAX_CHARS} characters."),
        });
    }
    if draft.command.trim().is_empty() {
        return Err(ValidationError {
            field: "command".to_string(),
            message: "Snippet command text is required.".to_string(),
        });
    }
    if draft.command.chars().count() > SNIPPET_COMMAND_MAX_CHARS {
        return Err(ValidationError {
            field: "command".to_string(),
            message: format!(
                "Snippet command text must be at most {SNIPPET_COMMAND_MAX_CHARS} characters."
            ),
        });
    }
    Ok(())
}

pub fn validate_folder_name(name: &str, siblings: &[String]) -> Result<(), ValidationError> {
    let len = name.chars().count();

    if len < FOLDER_NAME_MIN_CHARS {
        return Err(ValidationError {
            field: "name".to_string(),
            message: "Folder name is required.".to_string(),
        });
    }

    if len > FOLDER_NAME_MAX_CHARS {
        return Err(ValidationError {
            field: "name".to_string(),
            message: format!(
                "Folder name must be at most {FOLDER_NAME_MAX_CHARS} characters (got {len})."
            ),
        });
    }

    if siblings.iter().any(|existing| existing == name) {
        return Err(ValidationError {
            field: "name".to_string(),
            message: format!("A folder named \"{name}\" already exists in this location."),
        });
    }

    Ok(())
}

/// The deepest folder depth the hierarchy permits.
///
/// The design caps nesting at **10 levels** with `depth` recorded `0..=9`
/// (a root folder is depth `0`). We adopt that convention literally here: a
/// folder's own depth, and every one of its descendants' depths, must stay
/// `<= MAX_FOLDER_DEPTH`. A move that would push the moved folder — or any
/// folder beneath it — past depth `9` (i.e. to an 11th level) is rejected
/// (Requirements 3.4, 3.5).
pub const MAX_FOLDER_DEPTH: u8 = 9;

/// What a confirmed folder deletion removed (design `delete_folder` signature).
///
/// Returned by [`ConnectionManager::delete_folder`] on a confirmed deletion so
/// the caller/UI can report precisely what disappeared and so design Property
/// 10 can assert that *exactly* the target subtree — and nothing outside it —
/// was removed. It carries the ids of every folder and every profile that the
/// cascade removed (the target folder plus all nested folders and all contained
/// profiles), each listed once.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeletionSummary {
    /// Ids of the deleted folders: the target folder and all its descendants.
    pub folders: Vec<FolderId>,
    /// Ids of the deleted profiles: every profile contained anywhere in the
    /// deleted subtree.
    pub profiles: Vec<ProfileId>,
}

/// The outcome of validating a folder move against a snapshot of the hierarchy
/// — either the recomputed depths every affected folder must take, or the
/// specific rule the move violates.
///
/// Returned by [`validate_folder_move`] so the pure integrity check can be
/// exercised independently of persistence (design Property 9, subtask 3.12):
/// the plan it produces is exactly the set of `(FolderId, new_depth)` the
/// persistence layer must apply, and the error names the violated rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderMovePlan {
    /// The moved folder's new parent (`None` = root).
    pub new_parent: Option<FolderId>,
    /// The new depth for the moved folder and every folder in its subtree,
    /// paired by id. The moved folder itself is included.
    pub new_depths: Vec<(FolderId, u8)>,
}

/// Validate moving folder `id` under `dest` against the folder set `folders`,
/// returning either the depth re-plan for the moved subtree or the specific
/// violated rule.
///
/// This is the *pure* core of [`ConnectionManager::move_folder`]: it reads only
/// the candidate move and a snapshot of the hierarchy (`folders`, keyed by id),
/// touches no persistence, and so is trivially exercisable across many inputs
/// — exactly what design Property 9 (subtask 3.12) needs. The manager loads the
/// folder set from the repository, calls this, and on success applies the
/// returned depths in a single transaction.
///
/// Rules, applied in order:
///
/// 1. **existence** — both the moved folder and (when `dest` is `Some`) the
///    destination folder must exist in `folders`; a missing folder is a
///    `persistence.notFound` so the caller can distinguish it from an integrity
///    violation.
/// 2. **acyclicity** (Requirement 3.5) — `dest` must not be the moved folder
///    itself nor any folder in its subtree. Moving a folder onto itself or into
///    one of its own descendants would create a cycle, so it is rejected with a
///    `validation.destination` error naming the descendant/cycle violation.
/// 3. **depth cap** (Requirements 3.4, 3.5) — with the moved folder reparented
///    under `dest`, the moved folder's new depth is `0` at the root or one
///    deeper than `dest`, and each descendant shifts by the same delta. If any
///    resulting depth exceeds [`MAX_FOLDER_DEPTH`] the move is rejected with a
///    `validation.destination` error naming the max-depth violation.
///
/// On success returns a [`FolderMovePlan`] carrying the new parent and the new
/// depth for the moved folder and every descendant.
pub fn validate_folder_move(
    folders: &std::collections::HashMap<FolderId, Folder>,
    id: FolderId,
    dest: Option<FolderId>,
) -> Result<FolderMovePlan, DomainError> {
    // Rule 1: the moved folder must exist.
    let moved = folders
        .get(&id)
        .ok_or_else(|| DomainError::new("persistence.notFound", "No such folder."))?;

    // Rule 1: a named destination must exist, and its depth anchors the move.
    let dest_depth = match dest {
        None => None,
        Some(dest_id) => {
            let dest_folder = folders.get(&dest_id).ok_or_else(|| {
                DomainError::new(
                    "persistence.notFound",
                    "The destination folder does not exist.",
                )
            })?;
            Some(dest_folder.depth)
        }
    };

    // Collect the moved folder's subtree (itself + all descendants).
    let subtree = subtree_ids(folders, id);

    // Rule 2 (acyclicity): the destination may not be the folder itself or any
    // of its descendants — that would make the hierarchy cyclic.
    if let Some(dest_id) = dest {
        if subtree.contains(&dest_id) {
            return Err(ValidationError {
                field: "destination".to_string(),
                message: if dest_id == id {
                    "A folder cannot be moved into itself.".to_string()
                } else {
                    "A folder cannot be moved into one of its own descendants (cycle).".to_string()
                },
            }
            .into());
        }
    }

    // Rule 3 (depth cap): the moved folder's new depth is 0 at the root or one
    // below the destination; every descendant shifts by the same signed delta.
    let new_root_depth = match dest_depth {
        None => 0u8,
        Some(d) => d.saturating_add(1),
    };

    // Signed delta applied to every folder in the subtree (old moved depth ->
    // new moved depth). Using i32 keeps the arithmetic unambiguous for both
    // deepening and shallowing moves.
    let delta = new_root_depth as i32 - moved.depth as i32;

    let mut new_depths = Vec::with_capacity(subtree.len());
    for folder_id in &subtree {
        let old_depth = folders
            .get(folder_id)
            .expect("subtree ids come from folders")
            .depth as i32;
        let new_depth = old_depth + delta;
        // A negative depth is impossible here (the moved folder's new depth is
        // >= 0 and descendants are deeper), but guard defensively.
        if new_depth < 0 || new_depth > MAX_FOLDER_DEPTH as i32 {
            return Err(ValidationError {
                field: "destination".to_string(),
                message: format!(
                    "Moving the folder would exceed the maximum nesting depth of {} levels (maxDepth).",
                    MAX_FOLDER_DEPTH as usize + 1
                ),
            }
            .into());
        }
        new_depths.push((*folder_id, new_depth as u8));
    }

    Ok(FolderMovePlan {
        new_parent: dest,
        new_depths,
    })
}

/// Collect the ids of `root` and every folder transitively parented beneath it
/// from the folder set `folders`.
///
/// A pure graph walk over `parent_id` edges used by [`validate_folder_move`] to
/// identify the subtree a move carries (for the acyclicity check) and to
/// re-depth. The walk is iterative and bounded by the number of folders, so it
/// terminates even on a pathological (already-cyclic) input.
pub fn subtree_ids(
    folders: &std::collections::HashMap<FolderId, Folder>,
    root: FolderId,
) -> std::collections::HashSet<FolderId> {
    let mut subtree = std::collections::HashSet::new();
    subtree.insert(root);
    // Repeatedly sweep the folder set, adding any folder whose parent is already
    // in the subtree, until a sweep adds nothing. Bounded by folder count.
    loop {
        let mut added = false;
        for folder in folders.values() {
            if let Some(parent) = folder.parent_id {
                if subtree.contains(&parent) && subtree.insert(folder.id) {
                    added = true;
                }
            }
        }
        if !added {
            break;
        }
    }
    subtree
}

/// The Connection_Manager's profile operations over a [`Database`].
///
/// Holds the persistence handle and an injected [`CredentialStore`], and
/// exposes create/update/delete of profiles. It applies [`validate_draft`]
/// before every write and surfaces repository failures as save-failure errors
/// (Requirement 2.6).
///
/// # Why the Credential_Store is injected
///
/// Profile deletion must also remove the profile's secrets from Secure_Storage
/// (Requirements 2.8 + 11.6). Rather than reach for the OS keystore directly,
/// the manager takes a [`CredentialStore`] as a type parameter. Production code
/// passes [`KeyringCredentialStore::with_os_keystore`](crate::credential::KeyringCredentialStore::with_os_keystore);
/// tests can pass an in-memory mock so deletion can be exercised without ever
/// touching the real Keychain / Credential Manager — which is what the
/// confirmed-deletion property (subtask 3.6) and the credential-removal
/// property (subtask 5.4) need.
pub struct ConnectionManager<C: CredentialStore> {
    db: Database,
    credentials: C,
}

impl<C: CredentialStore> ConnectionManager<C> {
    /// Build a manager over an open [`Database`] and a [`CredentialStore`].
    ///
    /// The credential store is used by [`delete_profile`](Self::delete_profile)
    /// to remove a deleted profile's secrets (Requirements 2.8, 11.6). It plays
    /// no part in create/update, so those paths are unaffected by the choice of
    /// backend.
    pub fn new(db: Database, credentials: C) -> Self {
        Self { db, credentials }
    }

    /// Borrow the underlying database (useful for later tasks and tests).
    pub fn database(&self) -> &Database {
        &self.db
    }

    /// Borrow the injected Credential_Store (useful for tests/diagnostics).
    pub fn credentials(&self) -> &C {
        &self.credentials
    }

    /// Validate and create a new profile (Requirement 2.1).
    ///
    /// Validation runs first; an invalid draft is rejected with a
    /// [`ValidationError`] naming the offending field and nothing is persisted.
    /// On success a fresh [`ProfileId`] is assigned, the profile is inserted
    /// transactionally, and the stored [`ConnectionProfile`] is returned. If
    /// the insert fails the transaction rolls back (nothing is stored) and a
    /// save-failure error is returned (Requirement 2.6).
    pub fn create_profile(&self, draft: ProfileDraft) -> Result<ConnectionProfile, DomainError> {
        validate_draft(&draft, SUPPORTED_PROTOCOLS)?;
        let profile = draft.into_profile(ProfileId::new());
        self.db
            .insert_profile(&profile)
            .map_err(|e| save_failure("create", e))?;
        Ok(profile)
    }

    /// Validate and update an existing profile (Requirement 2.5).
    ///
    /// Applies the *identical* validation rules as creation. On success the
    /// profile keeps its `id` and its new values are persisted; the stored
    /// profile is returned. If no profile with `id` exists the repository
    /// reports `persistence.notFound`. If the update fails for any other
    /// reason the repository rolls back, so the prior stored state is retained
    /// unchanged, and a save-failure error is returned (Requirement 2.6).
    pub fn update_profile(
        &self,
        id: ProfileId,
        draft: ProfileDraft,
    ) -> Result<ConnectionProfile, DomainError> {
        validate_draft(&draft, SUPPORTED_PROTOCOLS)?;
        let profile = draft.into_profile(id);
        self.db
            .update_profile(&profile)
            .map_err(|e| save_failure("update", e))?;
        Ok(profile)
    }

    /// Validate and create a new folder (Requirements 3.1, 3.2).
    ///
    /// The `name` must be `1..=255` characters and unique among the folders
    /// that share `parent` (siblings); otherwise the create is rejected with a
    /// [`ValidationError`] on `name` naming the specific failure (empty, too
    /// long, or duplicate) and nothing is persisted. On success a fresh
    /// [`FolderId`] is assigned, the folder's `depth` is derived from `parent`
    /// (`0` at the root, otherwise one deeper than the parent), the folder is
    /// inserted transactionally, and the stored [`Folder`] is returned so the
    /// UI can display it in the hierarchy (Requirement 3.1).
    ///
    /// Depth is computed here only as a faithful property of the new folder;
    /// the depth-cap and acyclicity rules that govern *moving* folders are
    /// Requirements 3.4/3.5 and belong to the move operation (task 3.10), not
    /// to this name-focused create.
    ///
    /// If the parent id is supplied but no such folder exists, the repository's
    /// `persistence.notFound` is surfaced so the caller can distinguish a
    /// dangling parent from a name problem.
    pub fn create_folder(
        &self,
        name: String,
        parent: Option<FolderId>,
    ) -> Result<Folder, DomainError> {
        let siblings = self.sibling_names(parent, None)?;
        validate_folder_name(&name, &siblings)?;

        let depth = self.depth_for_parent(parent)?;
        let folder = Folder {
            id: FolderId::new(),
            name,
            parent_id: parent,
            depth,
        };
        self.db
            .insert_folder(&folder)
            .map_err(|e| save_failure("create folder", e))?;
        Ok(folder)
    }

    // --- Command_Snippet management (Requirement 19) -----------------------

    /// Validate and create a Command_Snippet (Requirement 19.1, 19.2). On a
    /// validation failure nothing is stored; on success the saved snippet is
    /// returned.
    pub fn create_snippet(&self, draft: SnippetDraft) -> Result<CommandSnippet, DomainError> {
        validate_snippet_draft(&draft)?;
        let snippet = CommandSnippet {
            id: SnippetId::new(),
            name: draft.name.trim().to_string(),
            command: draft.command,
            protocol: draft.protocol,
        };
        self.db
            .insert_snippet(&snippet)
            .map_err(|e| save_failure("create snippet", e))?;
        Ok(snippet)
    }

    /// Validate and update an existing Command_Snippet, applying the same rules
    /// as creation (Requirement 19.3). On failure the prior state is retained.
    pub fn update_snippet(
        &self,
        id: SnippetId,
        draft: SnippetDraft,
    ) -> Result<CommandSnippet, DomainError> {
        validate_snippet_draft(&draft)?;
        let snippet = CommandSnippet {
            id,
            name: draft.name.trim().to_string(),
            command: draft.command,
            protocol: draft.protocol,
        };
        self.db
            .update_snippet(&snippet)
            .map_err(|e| save_failure("update snippet", e))?;
        Ok(snippet)
    }

    /// Delete a Command_Snippet after confirmation (Requirement 19.3). Without
    /// confirmation nothing is removed.
    pub fn delete_snippet(&self, id: SnippetId, confirmed: bool) -> Result<(), DomainError> {
        if !confirmed {
            return Err(DomainError::new(
                CONFIRMATION_REQUIRED_CODE,
                "Deleting a snippet requires confirmation.",
            ));
        }
        self.db
            .delete_snippet(id)
            .map_err(|e| save_failure("delete snippet", e))?;
        Ok(())
    }

    /// List every stored Command_Snippet.
    pub fn list_snippets(&self) -> Result<Vec<CommandSnippet>, DomainError> {
        self.db.list_snippets()
    }

    /// Validate and rename an existing folder (Requirements 3.9, 3.10).
    ///
    /// Applies the *identical* name rules as [`create_folder`](Self::create_folder):
    /// the new `name` must be `1..=255` characters and unique among the folder's
    /// siblings (the folders sharing its parent) — excluding the folder itself,
    /// so renaming a folder to the name it already has is permitted. On
    /// rejection a [`ValidationError`] on `name` names the specific failure and
    /// nothing is persisted. On success only the name changes (parent and depth
    /// are preserved) and the updated [`Folder`] is returned.
    ///
    /// If no folder with `id` exists the repository reports
    /// `persistence.notFound`.
    pub fn rename_folder(&self, id: FolderId, name: String) -> Result<Folder, DomainError> {
        // Load the existing folder so the rename preserves its parent and depth
        // and so uniqueness is checked against the correct sibling set.
        let existing = self
            .db
            .get_folder(id)?
            .ok_or_else(|| DomainError::new("persistence.notFound", "No such folder."))?;

        let siblings = self.sibling_names(existing.parent_id, Some(id))?;
        validate_folder_name(&name, &siblings)?;

        let updated = Folder { name, ..existing };
        self.db
            .update_folder(&updated)
            .map_err(|e| save_failure("rename folder", e))?;
        Ok(updated)
    }

    /// Move a profile into `dest` (or to the root when `dest` is `None`),
    /// dropping any prior folder association (Requirement 3.3).
    ///
    /// The move is modeled exactly as the design prescribes: a profile's
    /// membership is the single `folder_id` field, so "associate with the
    /// destination and remove the association with any previous folder" is one
    /// assignment — `folder_id = dest`. A profile therefore always sits in at
    /// most one folder; there is no separate step to detach the old parent
    /// (design Property 8, subtask 3.11).
    ///
    /// When `dest` is `Some`, the destination folder must exist; otherwise the
    /// repository's `persistence.notFound` is surfaced and nothing is changed.
    /// If no profile with `id` exists, `persistence.notFound` is likewise
    /// returned. On a write failure the prior stored state is retained
    /// (Requirement 2.6, via the repository's transactional update).
    pub fn move_profile(&self, id: ProfileId, dest: Option<FolderId>) -> Result<(), DomainError> {
        // Validate the destination exists before touching the profile, so a
        // dangling destination is reported without any write.
        if let Some(dest_id) = dest {
            if self.db.get_folder(dest_id)?.is_none() {
                return Err(DomainError::new(
                    "persistence.notFound",
                    "The destination folder does not exist.",
                ));
            }
        }

        let existing = self
            .db
            .get_profile(id)?
            .ok_or_else(|| DomainError::new("persistence.notFound", "No such profile."))?;

        // Setting folder_id is the whole move: the new association replaces the
        // old one, so exactly one folder association results (Requirement 3.3).
        let moved = ConnectionProfile {
            folder_id: dest,
            ..existing
        };
        self.db
            .update_profile(&moved)
            .map_err(|e| save_failure("move profile", e))?;
        Ok(())
    }

    /// Move folder `id` under `dest` (or to the root when `dest` is `None`),
    /// keeping the hierarchy acyclic and within the depth cap (Requirements
    /// 3.4, 3.5).
    ///
    /// The integrity decision is delegated to the pure
    /// [`validate_folder_move`] over a snapshot of every folder, so the
    /// cycle/descendant and depth-cap rules are testable without persistence
    /// (design Property 9). This method's job is the I/O around it: load the
    /// folder set, run the check, and — on success — persist the moved folder's
    /// new `parent_id` together with the recomputed `depth` of the moved folder
    /// *and its entire subtree* in a single transaction, so a failure leaves
    /// the prior hierarchy intact (no moved parent with stale descendant
    /// depths).
    ///
    /// Rejections surface as a `validation.destination` error naming the
    /// specific violation — a cycle/descendant move or a max-depth breach —
    /// while a missing moved or destination folder surfaces
    /// `persistence.notFound`.
    pub fn move_folder(&self, id: FolderId, dest: Option<FolderId>) -> Result<(), DomainError> {
        // Snapshot the hierarchy keyed by id for the pure integrity check.
        let folders: std::collections::HashMap<FolderId, Folder> = self
            .db
            .list_folders()?
            .into_iter()
            .map(|f| (f.id, f))
            .collect();

        let plan = validate_folder_move(&folders, id, dest)?;

        // Build the batch of updated folders: the moved folder gets its new
        // parent and depth; every descendant keeps its parent but takes its
        // recomputed depth. Applied atomically so the move is all-or-nothing.
        let updates: Vec<Folder> = plan
            .new_depths
            .iter()
            .map(|(folder_id, new_depth)| {
                let current = folders
                    .get(folder_id)
                    .expect("plan ids come from the folder snapshot");
                let parent_id = if *folder_id == id {
                    plan.new_parent
                } else {
                    current.parent_id
                };
                Folder {
                    id: *folder_id,
                    name: current.name.clone(),
                    parent_id,
                    depth: *new_depth,
                }
            })
            .collect();

        self.db
            .update_folders(&updates)
            .map_err(|e| save_failure("move folder", e))?;
        Ok(())
    }

    /// Collect the names of the folders that share `parent`, excluding the
    /// folder identified by `exclude` (used by rename so a folder is not
    /// treated as colliding with itself).
    ///
    /// Uniqueness is scoped to a single parent (Requirements 3.2, 3.10): two
    /// folders in different parents may share a name. [`Database::list_folders`]
    /// returns every folder, so this filters to those whose `parent_id` matches
    /// `parent`.
    fn sibling_names(
        &self,
        parent: Option<FolderId>,
        exclude: Option<FolderId>,
    ) -> Result<Vec<String>, DomainError> {
        Ok(self
            .db
            .list_folders()?
            .into_iter()
            .filter(|f| f.parent_id == parent && Some(f.id) != exclude)
            .map(|f| f.name)
            .collect())
    }

    /// Derive the depth a new folder would occupy under `parent`.
    ///
    /// A root folder (`parent = None`) sits at depth `0`; otherwise the folder
    /// is one level deeper than its parent. The parent must exist; a missing
    /// parent surfaces the repository's `persistence.notFound`. Depth is
    /// recorded as a faithful property of the folder here; enforcing the
    /// 10-level cap is the move operation's concern (task 3.10).
    fn depth_for_parent(&self, parent: Option<FolderId>) -> Result<u8, DomainError> {
        match parent {
            None => Ok(0),
            Some(parent_id) => {
                let parent = self.db.get_folder(parent_id)?.ok_or_else(|| {
                    DomainError::new("persistence.notFound", "The parent folder does not exist.")
                })?;
                Ok(parent.depth.saturating_add(1))
            }
        }
    }

    /// Delete a profile, requiring explicit confirmation first (Requirements
    /// 2.7, 2.8), and remove its stored credentials (wiring Requirement 11.6).
    ///
    /// # Confirmation semantics (Requirement 2.7)
    ///
    /// Confirmation is modeled as the boolean `confirmed` flag the design
    /// prescribes. When `confirmed` is `false` the method performs **no**
    /// destructive work at all — it touches neither the repository nor the
    /// Credential_Store — and returns a [`DomainError`] with the stable code
    /// [`CONFIRMATION_REQUIRED_CODE`]. That error is how the gate ("deletion was
    /// requested but not yet confirmed") is signalled to the caller/UI, which
    /// then shows the confirmation prompt and, on the user's approval, calls
    /// again with `confirmed = true`. Choosing a distinct, machine-readable
    /// error code (rather than, say, silently returning `Ok`) keeps "pending
    /// confirmation" cleanly distinguishable from "deleted" at the type/code
    /// level while matching the design's `Result<(), DomainError>` signature.
    ///
    /// # On confirmation (Requirements 2.8, 11.6)
    ///
    /// With `confirmed = true` the profile is removed from the repository
    /// first, then its credentials are removed from Secure_Storage via
    /// [`CredentialStore::delete_for_profile`]. The order matters only in that
    /// a repository failure aborts before any credential work; because
    /// `delete_for_profile` is idempotent (deleting an absent secret is a
    /// successful no-op), re-running a confirmed deletion is safe.
    ///
    /// Deleting a profile id that does not exist is a benign no-op: the
    /// repository reports nothing was removed and the (also idempotent)
    /// credential cleanup still runs, so the end state — "no such profile, no
    /// such credential" — holds either way.
    pub async fn delete_profile(&self, id: ProfileId, confirmed: bool) -> Result<(), DomainError> {
        // Requirement 2.7: without confirmation, remove nothing and signal that
        // confirmation is required. No repository or credential call happens.
        if !confirmed {
            return Err(DomainError::new(
                CONFIRMATION_REQUIRED_CODE,
                "Deleting a connection profile requires confirmation.",
            ));
        }

        // Requirement 2.8: remove the profile from the repository.
        self.db.delete_profile(id).map_err(delete_failure)?;

        // Requirement 11.6 (wired here): remove the profile's credentials from
        // Secure_Storage. Idempotent, so an absent secret is not an error.
        self.credentials.delete_for_profile(id).await?;

        Ok(())
    }

    /// Store, replace, or clear a profile's password credential in
    /// Secure_Storage (Requirement 17).
    ///
    /// The password is keyed by the profile's own password key (the UI never
    /// handles the raw credential-key reference). Semantics:
    ///
    /// - `Some(pw)` where `pw` is non-empty → save/replace the stored secret.
    /// - `Some("")` (an explicitly cleared field) → remove the stored secret.
    /// - `None` (the editor left the field untouched) → no-op; the existing
    ///   stored secret is preserved unchanged.
    ///
    /// On a storage failure the prior stored secret is left unchanged (the
    /// backend's `save`/`delete` are the only writers and they fail atomically),
    /// and the error is returned so the editor can keep the entered value and
    /// surface it (Requirement 17.6). The secret is never written to the
    /// connection database (Requirement 17.7): it only ever flows to the
    /// Credential_Store here.
    pub async fn save_credential(
        &self,
        id: ProfileId,
        password: Option<String>,
    ) -> Result<(), DomainError> {
        let key = profile_password_key(id);
        match password {
            Some(pw) if !pw.is_empty() => {
                self.credentials.save(key, Secret::new(pw)).await?;
            }
            Some(_) => {
                // Explicitly cleared: remove any stored credential for the
                // profile. Idempotent, so clearing an absent secret is fine.
                self.credentials.delete_for_profile(id).await?;
            }
            None => {
                // Untouched field: preserve whatever is stored.
            }
        }
        Ok(())
    }

    /// Whether a profile currently has a stored password credential
    /// (Requirement 17.3). Returns only presence — never the secret itself — so
    /// the editor can show a "credential stored" indicator without exposing it.
    pub async fn has_credential(&self, id: ProfileId) -> Result<bool, DomainError> {
        match self.credentials.get(profile_password_key(id)).await {
            Ok(_) => Ok(true),
            Err(crate::domain::CredentialError::NotFound) => Ok(false),
            Err(other) => Err(other.into()),
        }
    }

    /// Delete a folder and its entire subtree, requiring confirmation first
    /// (Requirements 3.6, 3.7, 3.8), and remove every contained profile's
    /// stored credentials (consistent with Requirement 11.6).
    ///
    /// # Confirmation semantics (Requirement 3.6)
    ///
    /// Confirmation is modeled with the same boolean `confirmed` flag as
    /// [`delete_profile`](Self::delete_profile). The requirement demands
    /// confirmation "when a folder contains profiles", so this method first
    /// surveys the folder's subtree (itself plus every descendant folder) and
    /// collects the profiles contained anywhere within it. If that subtree
    /// contains **any** profile and `confirmed` is `false`, the method performs
    /// no destructive work at all — it touches neither the repository nor the
    /// Credential_Store — and returns a [`DomainError`] with the stable code
    /// [`CONFIRMATION_REQUIRED_CODE`], exactly the gate the profile path uses.
    ///
    /// An **empty** subtree (no profiles anywhere beneath the folder) carries
    /// nothing a user could lose, so — matching the requirement's wording,
    /// which gates confirmation specifically on *containing profiles* — it is
    /// deleted directly without a confirmation round-trip, regardless of
    /// `confirmed`. The essential guarantee the requirement names still holds:
    /// when profiles are present, nothing is removed without confirmation.
    ///
    /// # On confirmation (Requirement 3.7)
    ///
    /// With confirmation satisfied the folder and its whole subtree are removed
    /// via [`Database::delete_folder`], whose `ON DELETE CASCADE` deletes the
    /// nested folders and contained profiles atomically — so everything outside
    /// the subtree is left untouched. Then, for each profile that lived in the
    /// subtree, its credentials are cleared from Secure_Storage via
    /// [`CredentialStore::delete_for_profile`] (idempotent, so an absent secret
    /// is a successful no-op). The ids of every removed folder and profile are
    /// returned in a [`DeletionSummary`].
    ///
    /// # On cancellation (Requirement 3.8)
    ///
    /// Cancellation is the `confirmed = false` case over a non-empty subtree:
    /// the method returns the confirmation-required error and changes nothing,
    /// so the folder and all its contents are retained unchanged.
    ///
    /// Deleting a folder id that does not exist is a benign no-op: nothing is in
    /// its (empty) subtree, the repository reports nothing removed, and an empty
    /// [`DeletionSummary`] is returned.
    pub async fn delete_folder(
        &self,
        id: FolderId,
        confirmed: bool,
    ) -> Result<DeletionSummary, DomainError> {
        // Snapshot the hierarchy and compute the subtree the deletion carries:
        // the target folder plus every folder transitively beneath it. This
        // reuses the same pure walk the move operation uses (`subtree_ids`), so
        // "what gets deleted" is identified the same way everywhere.
        let folders: std::collections::HashMap<FolderId, Folder> = self
            .db
            .list_folders()?
            .into_iter()
            .map(|f| (f.id, f))
            .collect();

        // If the target folder does not exist its subtree is just {id} with no
        // real folders behind it; the deletion degrades to a benign no-op below.
        let subtree = subtree_ids(&folders, id);

        // Collect the ids of every profile contained anywhere in the subtree.
        // These drive both the confirmation gate (Requirement 3.6) and the
        // post-cascade credential cleanup (Requirement 11.6).
        let mut profile_ids: Vec<ProfileId> = Vec::new();
        for folder_id in &subtree {
            for profile in self.db.list_profiles_in_folder(*folder_id)? {
                profile_ids.push(profile.id);
            }
        }

        // Requirement 3.6 / 3.8: when the subtree contains profiles, deletion
        // requires confirmation. Without it, remove nothing and signal the
        // gate — identical to the profile-delete contract.
        if !profile_ids.is_empty() && !confirmed {
            return Err(DomainError::new(
                CONFIRMATION_REQUIRED_CODE,
                "Deleting a folder that contains profiles requires confirmation.",
            ));
        }

        // The real folders being removed are those that actually exist in the
        // snapshot (the target plus its descendants). A non-existent target
        // yields an empty set, matching the no-op outcome.
        let folder_ids: Vec<FolderId> = subtree
            .iter()
            .copied()
            .filter(|fid| folders.contains_key(fid))
            .collect();

        // Requirement 3.7: delete the folder and its whole subtree atomically.
        // The schema's ON DELETE CASCADE removes nested folders and contained
        // profiles in one transaction, so nothing outside the subtree changes.
        self.db.delete_folder(id).map_err(delete_folder_failure)?;

        // Requirement 11.6: the cascade removed the contained profiles, so
        // their credentials must be cleaned from Secure_Storage too. Idempotent
        // per profile, so a profile without a stored secret is not an error.
        for profile_id in &profile_ids {
            self.credentials.delete_for_profile(*profile_id).await?;
        }

        Ok(DeletionSummary {
            folders: folder_ids,
            profiles: profile_ids,
        })
    }

    /// Export the entire configuration — every folder and profile — to a single
    /// versioned JSON file at `path`, excluding plaintext credentials
    /// (Requirements 16.1, 11.5).
    ///
    /// The folders and profiles are read from the repository and handed to the
    /// pure [`build_export`], which stamps the file with
    /// [`EXPORT_SCHEMA_VERSION`] and copies the items verbatim. Because the
    /// domain model only ever holds credential *references* (a
    /// [`CredentialKey`](crate::domain::CredentialKey) name or a
    /// [`KeyId`](crate::domain::KeyId)), the export is structurally incapable of
    /// containing a secret — so this method **never** consults the
    /// Credential_Store (design Property 31). The serialized bytes are written
    /// to `path`.
    ///
    /// On success returns an [`ExportSummary`] with the exact number of folders
    /// and profiles written. A read, serialization, or file-write failure
    /// surfaces a structured [`DomainError`]; nothing partial is reported as
    /// success.
    pub fn export_config(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<ExportSummary, DomainError> {
        let folders = self.db.list_folders()?;
        let profiles = self.db.list_profiles()?;

        let summary = ExportSummary {
            folders_exported: folders.len(),
            profiles_exported: profiles.len(),
        };

        let export = build_export(folders, profiles);
        let bytes = serialize_export(&export).map_err(DomainError::from)?;

        std::fs::write(path.as_ref(), bytes).map_err(|e| {
            DomainError::new(
                "connection.exportFailed",
                "Failed to write the configuration export file.",
            )
            .with_cause(e.to_string())
        })?;

        Ok(summary)
    }

    /// Import a versioned configuration file at `path`, adding its folders and
    /// profiles to the existing configuration as brand-new entries (Requirements
    /// 16.2, 16.3, 16.4, 16.5).
    ///
    /// The flow mirrors the three guarantees the requirements name, with the
    /// decision logic kept in pure functions so it is testable without I/O:
    ///
    /// 1. **Read + validate (Requirement 16.3).** The file is read and handed to
    ///    [`parse_export`], which rejects invalid JSON / wrong shape
    ///    ([`ImportError::Malformed`]) and an unsupported `schema_version`
    ///    ([`ImportError::UnsupportedVersion`]). Rejection happens *before* any
    ///    repository write, so a bad file leaves the existing configuration
    ///    completely unchanged. A read failure surfaces [`ImportError::Io`].
    ///
    /// 2. **Remap ids (Requirement 16.4).** [`remap_import`] gives every incoming
    ///    folder and profile a fresh identifier and rewrites the file's internal
    ///    `parent_id` / `folder_id` references to the new ids. An incoming id
    ///    that collides with an existing one is therefore added as a *new* entry
    ///    — never overwriting what is already stored — and the imported subtree
    ///    stays self-consistent.
    ///
    /// 3. **Insert atomically (Requirement 16.2).** The remapped folders and
    ///    profiles are inserted through [`Database::import_items`] in a single
    ///    transaction so a mid-insert failure rolls back entirely, leaving the
    ///    prior configuration intact; folders are inserted parent-before-child
    ///    so their foreign keys resolve. No pre-existing item is modified or
    ///    deleted.
    ///
    /// On success returns an [`ImportSummary`] with the accurate number of
    /// folders and profiles added (Requirement 16.5) — equal to the file's item
    /// counts, since import is strictly additive.
    pub fn import_config(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<ImportSummary, ImportError> {
        // Step 1: read + validate. Any failure here is reported before touching
        // persistence, so the existing configuration is left unchanged.
        let bytes = std::fs::read(path.as_ref()).map_err(|e| ImportError::Io(e.to_string()))?;
        let export = parse_export(&bytes)?;

        // Step 2: remap every id and translate internal references.
        let (folders, profiles) = remap_import(export);
        let summary = import_counts(&folders, &profiles);

        // Step 3: insert the remapped items atomically. A persistence failure is
        // surfaced as a malformed/import error carrying the cause; the
        // transaction rollback guarantees the prior configuration is intact.
        self.db
            .import_items(&folders, &profiles)
            .map_err(|e| ImportError::Malformed(format!("{}: {}", e.code, e.message)))?;

        Ok(summary)
    }
}

/// Stable error code returned by [`ConnectionManager::delete_profile`] when a
/// deletion is requested without confirmation (Requirement 2.7).
///
/// Exposed so the IPC layer, the UI, and the tests can recognise the
/// "confirmation required" outcome by code rather than by matching message
/// text.
pub const CONFIRMATION_REQUIRED_CODE: &str = "connection.confirmationRequired";

/// Wrap a repository error as a save-failure `DomainError` naming the failed
/// operation (Requirement 2.6).
///
/// A `persistence.notFound` from [`Database::update_profile`] is passed through
/// unchanged so the caller can distinguish "no such profile" from a genuine
/// write failure. Any other persistence error keeps its original message as the
/// `cause` while the surfaced code and message name the save failure.
fn save_failure(operation: &str, err: DomainError) -> DomainError {
    if err.code == "persistence.notFound" {
        return err;
    }
    DomainError::new(
        "connection.saveFailed",
        format!("Failed to save the connection profile during {operation}."),
    )
    .with_cause(format!("{}: {}", err.code, err.message))
}

/// Wrap a repository error from the profile-delete path as a structured
/// `DomainError` naming the delete failure (parallels [`save_failure`]).
///
/// The repository's `delete_profile` only returns genuine write failures (a
/// missing row is reported as "nothing removed", not an error), so there is no
/// not-found code to pass through here — any error is surfaced as a delete
/// failure with the original cause preserved for diagnostics.
fn delete_failure(err: DomainError) -> DomainError {
    DomainError::new(
        "connection.deleteFailed",
        "Failed to delete the connection profile.",
    )
    .with_cause(format!("{}: {}", err.code, err.message))
}

/// Wrap a repository error from the folder-delete path as a structured
/// `DomainError` naming the delete failure (parallels [`delete_failure`]).
///
/// [`Database::delete_folder`] reports a missing folder as "nothing removed"
/// rather than an error, so there is no not-found code to pass through — any
/// error here is a genuine write failure and is surfaced as such with the
/// original cause preserved for diagnostics.
fn delete_folder_failure(err: DomainError) -> DomainError {
    DomainError::new("connection.deleteFailed", "Failed to delete the folder.")
        .with_cause(format!("{}: {}", err.code, err.message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::{profile_password_key, Secret};
    use crate::domain::{AuthConfig, CredentialError, CredentialKey, HostKeyPolicy};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Minimal in-memory [`CredentialStore`] so profile-deletion wiring can be
    /// exercised without the OS keystore. Records deletions so a test can
    /// assert the Credential_Store was asked to clean up a deleted profile.
    #[derive(Default)]
    struct MockCredentialStore {
        entries: Mutex<HashMap<String, String>>,
    }

    impl MockCredentialStore {
        fn contains(&self, key: &CredentialKey) -> bool {
            self.entries.lock().unwrap().contains_key(key.as_str())
        }
    }

    #[async_trait::async_trait]
    impl CredentialStore for MockCredentialStore {
        async fn save(&self, key: CredentialKey, secret: Secret) -> Result<(), CredentialError> {
            self.entries
                .lock()
                .unwrap()
                .insert(key.as_str().to_string(), secret.expose().to_string());
            Ok(())
        }

        async fn get(&self, key: CredentialKey) -> Result<Secret, CredentialError> {
            self.entries
                .lock()
                .unwrap()
                .get(key.as_str())
                .map(|v| Secret::new(v.clone()))
                .ok_or(CredentialError::NotFound)
        }

        async fn delete_for_profile(&self, id: ProfileId) -> Result<(), CredentialError> {
            // Mirror the production key convention so deletion targets the same
            // key the Credential_Store itself owns for a profile.
            self.entries
                .lock()
                .unwrap()
                .remove(profile_password_key(id).as_str());
            Ok(())
        }
    }

    /// Build a manager over a fresh in-memory DB and mock Credential_Store.
    fn manager() -> ConnectionManager<MockCredentialStore> {
        let db = Database::open_in_memory().unwrap();
        ConnectionManager::new(db, MockCredentialStore::default())
    }

    /// A valid SSH draft used as the baseline for mutation in tests.
    fn valid_draft() -> ProfileDraft {
        ProfileDraft {
            name: "prod-web".to_string(),
            protocol: ProtocolKind::Ssh,
            host: "example.com".to_string(),
            port: 22,
            username: None,
            domain: None,
            folder_id: None,
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

    #[test]
    fn valid_draft_passes_validation() {
        assert!(validate_draft(&valid_draft(), SUPPORTED_PROTOCOLS).is_ok());
    }

    #[test]
    fn empty_host_is_rejected_naming_host() {
        let mut draft = valid_draft();
        draft.host = String::new();
        let err = validate_draft(&draft, SUPPORTED_PROTOCOLS).unwrap_err();
        assert_eq!(err.field, "host");
    }

    #[test]
    fn whitespace_host_is_rejected_naming_host() {
        let mut draft = valid_draft();
        draft.host = "   ".to_string();
        let err = validate_draft(&draft, SUPPORTED_PROTOCOLS).unwrap_err();
        assert_eq!(err.field, "host");
    }

    #[test]
    fn zero_port_is_rejected_naming_port() {
        let mut draft = valid_draft();
        draft.port = 0;
        let err = validate_draft(&draft, SUPPORTED_PROTOCOLS).unwrap_err();
        assert_eq!(err.field, "port");
    }

    #[test]
    fn boundary_ports_are_accepted() {
        let mut draft = valid_draft();
        draft.port = 1;
        assert!(validate_draft(&draft, SUPPORTED_PROTOCOLS).is_ok());
        draft.port = 65535;
        assert!(validate_draft(&draft, SUPPORTED_PROTOCOLS).is_ok());
    }

    #[test]
    fn unsupported_protocol_is_rejected_naming_protocol() {
        let draft = valid_draft();
        // Supply a supported set that excludes the draft's protocol to exercise
        // the Supported_Protocol rejection path (Requirement 2.4).
        let restricted = [ProtocolKind::Telnet];
        let err = validate_draft(&draft, &restricted).unwrap_err();
        assert_eq!(err.field, "protocol");
    }

    #[test]
    fn absent_login_identity_is_accepted() {
        // Username/domain are optional: a draft with neither is valid (the
        // connect layer supplies the documented per-protocol default).
        let mut draft = valid_draft();
        draft.username = None;
        draft.domain = None;
        assert!(validate_draft(&draft, SUPPORTED_PROTOCOLS).is_ok());
    }

    #[test]
    fn present_login_identity_within_bounds_is_accepted() {
        let mut draft = valid_draft();
        draft.username = Some("corp-admin".to_string());
        draft.domain = Some("CORP".to_string());
        assert!(validate_draft(&draft, SUPPORTED_PROTOCOLS).is_ok());
    }

    #[test]
    fn empty_username_is_rejected_naming_username() {
        let mut draft = valid_draft();
        draft.username = Some(String::new());
        let err = validate_draft(&draft, SUPPORTED_PROTOCOLS).unwrap_err();
        assert_eq!(err.field, "username");

        // Whitespace-only is likewise treated as empty.
        draft.username = Some("   ".to_string());
        let err = validate_draft(&draft, SUPPORTED_PROTOCOLS).unwrap_err();
        assert_eq!(err.field, "username");
    }

    #[test]
    fn empty_domain_is_rejected_naming_domain() {
        let mut draft = valid_draft();
        draft.domain = Some(String::new());
        let err = validate_draft(&draft, SUPPORTED_PROTOCOLS).unwrap_err();
        assert_eq!(err.field, "domain");
    }

    #[test]
    fn overlong_username_is_rejected_naming_username() {
        let mut draft = valid_draft();
        draft.username = Some("u".repeat(LOGIN_FIELD_MAX_CHARS + 1));
        let err = validate_draft(&draft, SUPPORTED_PROTOCOLS).unwrap_err();
        assert_eq!(err.field, "username");

        // The boundary length is accepted.
        draft.username = Some("u".repeat(LOGIN_FIELD_MAX_CHARS));
        assert!(validate_draft(&draft, SUPPORTED_PROTOCOLS).is_ok());
    }

    #[test]
    fn create_profile_persists_and_returns_stored_profile() {
        let manager = manager();
        let draft = valid_draft();

        let stored = manager.create_profile(draft.clone()).unwrap();

        // Returned profile mirrors the submitted fields (Requirement 2.1).
        assert_eq!(stored.name, draft.name);
        assert_eq!(stored.protocol, draft.protocol);
        assert_eq!(stored.host, draft.host);
        assert_eq!(stored.port, draft.port);

        // ...and it is actually persisted.
        let read = manager.database().get_profile(stored.id).unwrap().unwrap();
        assert_eq!(read, stored);
    }

    #[test]
    fn create_profile_rejects_invalid_draft_without_persisting() {
        let manager = manager();
        let mut draft = valid_draft();
        draft.host = String::new();

        let err = manager.create_profile(draft).unwrap_err();
        assert_eq!(err.code, "validation.host");
        // Nothing was stored.
        assert_eq!(manager.database().list_profiles().unwrap().len(), 0);
    }

    #[test]
    fn update_profile_applies_same_rules_and_persists() {
        let manager = manager();
        let created = manager.create_profile(valid_draft()).unwrap();

        let mut draft = valid_draft();
        draft.host = "updated.example.com".to_string();
        draft.port = 2222;

        let updated = manager.update_profile(created.id, draft).unwrap();
        assert_eq!(updated.id, created.id);
        assert_eq!(updated.host, "updated.example.com");
        assert_eq!(updated.port, 2222);

        let read = manager.database().get_profile(created.id).unwrap().unwrap();
        assert_eq!(read, updated);
    }

    #[test]
    fn update_profile_rejects_invalid_draft_retaining_prior_state() {
        let manager = manager();
        let created = manager.create_profile(valid_draft()).unwrap();

        let mut draft = valid_draft();
        draft.port = 0; // invalid

        let err = manager.update_profile(created.id, draft).unwrap_err();
        assert_eq!(err.code, "validation.port");

        // Prior stored state is unchanged (Requirement 2.6 at the validation gate).
        let read = manager.database().get_profile(created.id).unwrap().unwrap();
        assert_eq!(read, created);
    }

    #[test]
    fn update_missing_profile_reports_not_found() {
        let manager = manager();
        let err = manager
            .update_profile(ProfileId::new(), valid_draft())
            .unwrap_err();
        assert_eq!(err.code, "persistence.notFound");
    }

    // --- Profile deletion (task 3.5) ---------------------------------------

    #[tokio::test]
    async fn delete_without_confirmation_removes_nothing_and_signals_confirmation() {
        let manager = manager();
        let created = manager.create_profile(valid_draft()).unwrap();
        // Seed a credential so we can prove it is left untouched.
        let key = profile_password_key(created.id);
        manager
            .credentials()
            .save(key.clone(), Secret::new("secret"))
            .await
            .unwrap();

        let err = manager.delete_profile(created.id, false).await.unwrap_err();

        // Requirement 2.7: the gate is signalled by a stable code...
        assert_eq!(err.code, CONFIRMATION_REQUIRED_CODE);
        // ...and nothing was removed from either store.
        assert!(manager
            .database()
            .get_profile(created.id)
            .unwrap()
            .is_some());
        assert!(manager.credentials().contains(&key));
    }

    #[tokio::test]
    async fn confirmed_delete_removes_profile_and_its_credentials() {
        let manager = manager();
        let created = manager.create_profile(valid_draft()).unwrap();
        let key = profile_password_key(created.id);
        manager
            .credentials()
            .save(key.clone(), Secret::new("secret"))
            .await
            .unwrap();

        manager.delete_profile(created.id, true).await.unwrap();

        // Requirement 2.8: the profile is gone.
        assert!(manager
            .database()
            .get_profile(created.id)
            .unwrap()
            .is_none());
        // Requirement 11.6 (wired here): its credential is gone too.
        assert!(!manager.credentials().contains(&key));
    }

    #[tokio::test]
    async fn confirmed_delete_of_missing_profile_is_a_benign_no_op() {
        let manager = manager();
        // No such profile; confirmed deletion still succeeds (idempotent).
        manager
            .delete_profile(ProfileId::new(), true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn save_credential_stores_replaces_clears_and_reports_presence() {
        // Requirement 17: the save_credential lifecycle over the Credential_Store.
        let manager = manager();
        let created = manager.create_profile(valid_draft()).unwrap();
        let key = profile_password_key(created.id);

        // Initially no credential, and has_credential reflects that.
        assert!(!manager.has_credential(created.id).await.unwrap());

        // Some(non-empty): stores the secret.
        manager
            .save_credential(created.id, Some("hunter2".to_string()))
            .await
            .unwrap();
        assert!(manager.has_credential(created.id).await.unwrap());
        assert_eq!(
            manager
                .credentials()
                .get(key.clone())
                .await
                .unwrap()
                .expose(),
            "hunter2"
        );

        // None: untouched — the stored secret is preserved unchanged.
        manager.save_credential(created.id, None).await.unwrap();
        assert_eq!(
            manager
                .credentials()
                .get(key.clone())
                .await
                .unwrap()
                .expose(),
            "hunter2"
        );

        // Some(non-empty) again: replaces the secret.
        manager
            .save_credential(created.id, Some("s3cret".to_string()))
            .await
            .unwrap();
        assert_eq!(
            manager
                .credentials()
                .get(key.clone())
                .await
                .unwrap()
                .expose(),
            "s3cret"
        );

        // Some(""): cleared — the stored credential is removed.
        manager
            .save_credential(created.id, Some(String::new()))
            .await
            .unwrap();
        assert!(!manager.has_credential(created.id).await.unwrap());
        assert!(!manager.credentials().contains(&key));
    }

    #[tokio::test]
    async fn password_profile_normalizes_its_credential_key() {
        // Requirement 17: a saved password profile points at its own canonical
        // key, regardless of what the draft carried, so save_credential and the
        // connect path agree on where the secret lives.
        let manager = manager();
        let mut draft = valid_draft();
        draft.auth = AuthConfig::Password {
            credential_key: CredentialKey::new("whatever/the/ui/sent"),
        };
        let created = manager.create_profile(draft).unwrap();
        match &created.auth {
            AuthConfig::Password { credential_key } => {
                assert_eq!(credential_key, &profile_password_key(created.id));
            }
            other => panic!("expected password auth, got {other:?}"),
        }
    }

    // --- Command_Snippet management (Requirement 19) -----------------------

    fn snippet_draft(name: &str, command: &str) -> SnippetDraft {
        SnippetDraft {
            name: name.to_string(),
            command: command.to_string(),
            protocol: None,
        }
    }

    #[test]
    fn snippet_validation_requires_name_and_command() {
        // Empty name is rejected naming `name`.
        let err = validate_snippet_draft(&snippet_draft("", "ls")).unwrap_err();
        assert_eq!(err.field, "name");
        // Whitespace-only name is likewise rejected.
        let err = validate_snippet_draft(&snippet_draft("   ", "ls")).unwrap_err();
        assert_eq!(err.field, "name");
        // Empty command is rejected naming `command`.
        let err = validate_snippet_draft(&snippet_draft("restart", "")).unwrap_err();
        assert_eq!(err.field, "command");
        // A fully specified draft is accepted.
        assert!(
            validate_snippet_draft(&snippet_draft("restart nginx", "systemctl restart nginx"))
                .is_ok()
        );
    }

    #[test]
    fn create_snippet_rejects_invalid_and_persists_valid() {
        let manager = manager();
        // Invalid: nothing stored.
        assert!(manager.create_snippet(snippet_draft("", "x")).is_err());
        assert_eq!(manager.list_snippets().unwrap().len(), 0);
        // Valid: stored and listed, with a trimmed name.
        let created = manager
            .create_snippet(SnippetDraft {
                name: "  deploy  ".to_string(),
                command: "make deploy".to_string(),
                protocol: Some(ProtocolKind::Ssh),
            })
            .unwrap();
        assert_eq!(created.name, "deploy");
        assert_eq!(created.protocol, Some(ProtocolKind::Ssh));
        let all = manager.list_snippets().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].command, "make deploy");
    }

    #[test]
    fn update_and_delete_snippet_round_trip() {
        let manager = manager();
        let created = manager
            .create_snippet(snippet_draft("tmp", "echo hi"))
            .unwrap();

        // Update changes the stored fields.
        let updated = manager
            .update_snippet(
                created.id,
                SnippetDraft {
                    name: "greet".to_string(),
                    command: "echo hello".to_string(),
                    protocol: Some(ProtocolKind::Telnet),
                },
            )
            .unwrap();
        assert_eq!(updated.name, "greet");
        assert_eq!(updated.command, "echo hello");
        assert_eq!(updated.protocol, Some(ProtocolKind::Telnet));

        // Unconfirmed delete removes nothing.
        let err = manager.delete_snippet(created.id, false).unwrap_err();
        assert_eq!(err.code, CONFIRMATION_REQUIRED_CODE);
        assert_eq!(manager.list_snippets().unwrap().len(), 1);

        // Confirmed delete removes it.
        manager.delete_snippet(created.id, true).unwrap();
        assert_eq!(manager.list_snippets().unwrap().len(), 0);
    }

    // --- Folder name validation (task 3.8) ---------------------------------

    #[test]
    fn validate_folder_name_accepts_in_range_unique_name() {
        assert!(validate_folder_name("servers", &[]).is_ok());
        assert!(validate_folder_name("servers", &["clients".to_string()]).is_ok());
    }

    #[test]
    fn validate_folder_name_accepts_boundary_lengths() {
        let one = "x";
        let max: String = "x".repeat(FOLDER_NAME_MAX_CHARS);
        assert!(validate_folder_name(one, &[]).is_ok());
        assert!(validate_folder_name(&max, &[]).is_ok());
    }

    #[test]
    fn validate_folder_name_rejects_empty_naming_name() {
        let err = validate_folder_name("", &[]).unwrap_err();
        assert_eq!(err.field, "name");
    }

    #[test]
    fn validate_folder_name_rejects_over_255_naming_name() {
        let too_long: String = "x".repeat(FOLDER_NAME_MAX_CHARS + 1);
        let err = validate_folder_name(&too_long, &[]).unwrap_err();
        assert_eq!(err.field, "name");
    }

    #[test]
    fn validate_folder_name_measures_length_in_chars_not_bytes() {
        // 255 multi-byte characters is 255 chars (accepted) but >255 bytes.
        let multibyte: String = "é".repeat(FOLDER_NAME_MAX_CHARS);
        assert!(multibyte.len() > FOLDER_NAME_MAX_CHARS); // more bytes than chars
        assert!(validate_folder_name(&multibyte, &[]).is_ok());
    }

    #[test]
    fn validate_folder_name_rejects_duplicate_sibling_naming_name() {
        let siblings = vec!["servers".to_string(), "clients".to_string()];
        let err = validate_folder_name("servers", &siblings).unwrap_err();
        assert_eq!(err.field, "name");
    }

    #[test]
    fn create_folder_persists_root_folder_at_depth_zero() {
        let manager = manager();
        let folder = manager.create_folder("servers".to_string(), None).unwrap();

        assert_eq!(folder.name, "servers");
        assert_eq!(folder.parent_id, None);
        assert_eq!(folder.depth, 0);

        let read = manager.database().get_folder(folder.id).unwrap().unwrap();
        assert_eq!(read, folder);
    }

    #[test]
    fn create_folder_derives_child_depth_from_parent() {
        let manager = manager();
        let root = manager.create_folder("root".to_string(), None).unwrap();
        let child = manager
            .create_folder("child".to_string(), Some(root.id))
            .unwrap();

        assert_eq!(child.parent_id, Some(root.id));
        assert_eq!(child.depth, 1);
    }

    #[test]
    fn create_folder_rejects_duplicate_name_in_same_parent_without_persisting() {
        let manager = manager();
        manager.create_folder("servers".to_string(), None).unwrap();

        let err = manager
            .create_folder("servers".to_string(), None)
            .unwrap_err();
        assert_eq!(err.code, "validation.name");

        // Only the first folder was stored.
        assert_eq!(manager.database().list_folders().unwrap().len(), 1);
    }

    #[test]
    fn create_folder_allows_same_name_in_different_parents() {
        let manager = manager();
        let a = manager.create_folder("a".to_string(), None).unwrap();
        let b = manager.create_folder("b".to_string(), None).unwrap();

        // "shared" under two different parents is fine (uniqueness is per-parent).
        manager
            .create_folder("shared".to_string(), Some(a.id))
            .unwrap();
        manager
            .create_folder("shared".to_string(), Some(b.id))
            .unwrap();
    }

    #[test]
    fn create_folder_rejects_empty_name_without_persisting() {
        let manager = manager();
        let err = manager.create_folder(String::new(), None).unwrap_err();
        assert_eq!(err.code, "validation.name");
        assert_eq!(manager.database().list_folders().unwrap().len(), 0);
    }

    #[test]
    fn rename_folder_persists_new_name() {
        let manager = manager();
        let folder = manager.create_folder("old".to_string(), None).unwrap();

        let renamed = manager.rename_folder(folder.id, "new".to_string()).unwrap();
        assert_eq!(renamed.id, folder.id);
        assert_eq!(renamed.name, "new");
        assert_eq!(renamed.parent_id, folder.parent_id);
        assert_eq!(renamed.depth, folder.depth);

        let read = manager.database().get_folder(folder.id).unwrap().unwrap();
        assert_eq!(read.name, "new");
    }

    #[test]
    fn rename_folder_allows_keeping_same_name() {
        let manager = manager();
        let folder = manager.create_folder("keep".to_string(), None).unwrap();
        // Renaming to its own name must not be treated as a self-collision.
        let renamed = manager
            .rename_folder(folder.id, "keep".to_string())
            .unwrap();
        assert_eq!(renamed.name, "keep");
    }

    #[test]
    fn rename_folder_rejects_duplicate_sibling_retaining_prior_name() {
        let manager = manager();
        manager.create_folder("servers".to_string(), None).unwrap();
        let other = manager.create_folder("clients".to_string(), None).unwrap();

        let err = manager
            .rename_folder(other.id, "servers".to_string())
            .unwrap_err();
        assert_eq!(err.code, "validation.name");

        // The prior name is retained.
        let read = manager.database().get_folder(other.id).unwrap().unwrap();
        assert_eq!(read.name, "clients");
    }

    #[test]
    fn rename_folder_rejects_over_255_name() {
        let manager = manager();
        let folder = manager.create_folder("ok".to_string(), None).unwrap();
        let too_long: String = "x".repeat(FOLDER_NAME_MAX_CHARS + 1);

        let err = manager.rename_folder(folder.id, too_long).unwrap_err();
        assert_eq!(err.code, "validation.name");
    }

    #[test]
    fn rename_missing_folder_reports_not_found() {
        let manager = manager();
        let err = manager
            .rename_folder(FolderId::new(), "x".to_string())
            .unwrap_err();
        assert_eq!(err.code, "persistence.notFound");
    }

    // --- Profile & folder moves (task 3.10) --------------------------------

    #[test]
    fn move_profile_sets_destination_and_drops_prior_folder() {
        let manager = manager();
        let a = manager.create_folder("a".to_string(), None).unwrap();
        let b = manager.create_folder("b".to_string(), None).unwrap();

        // Create a profile in folder `a`...
        let mut draft = valid_draft();
        draft.folder_id = Some(a.id);
        let created = manager.create_profile(draft).unwrap();

        // ...move it into folder `b`.
        manager.move_profile(created.id, Some(b.id)).unwrap();

        let read = manager.database().get_profile(created.id).unwrap().unwrap();
        // Exactly one association, and it is the destination (Requirement 3.3).
        assert_eq!(read.folder_id, Some(b.id));
    }

    #[test]
    fn move_profile_to_root_clears_folder() {
        let manager = manager();
        let a = manager.create_folder("a".to_string(), None).unwrap();
        let mut draft = valid_draft();
        draft.folder_id = Some(a.id);
        let created = manager.create_profile(draft).unwrap();

        manager.move_profile(created.id, None).unwrap();

        let read = manager.database().get_profile(created.id).unwrap().unwrap();
        assert_eq!(read.folder_id, None);
    }

    #[test]
    fn move_profile_rejects_missing_destination_without_change() {
        let manager = manager();
        let created = manager.create_profile(valid_draft()).unwrap();

        let err = manager
            .move_profile(created.id, Some(FolderId::new()))
            .unwrap_err();
        assert_eq!(err.code, "persistence.notFound");

        // Unchanged.
        let read = manager.database().get_profile(created.id).unwrap().unwrap();
        assert_eq!(read.folder_id, None);
    }

    #[test]
    fn move_profile_missing_profile_reports_not_found() {
        let manager = manager();
        let err = manager.move_profile(ProfileId::new(), None).unwrap_err();
        assert_eq!(err.code, "persistence.notFound");
    }

    #[test]
    fn move_folder_reparents_and_updates_subtree_depths() {
        let manager = manager();
        // root(0) -> child(1) -> grandchild(2); and a separate target(0).
        let root = manager.create_folder("root".to_string(), None).unwrap();
        let child = manager
            .create_folder("child".to_string(), Some(root.id))
            .unwrap();
        let grandchild = manager
            .create_folder("grandchild".to_string(), Some(child.id))
            .unwrap();
        let target = manager.create_folder("target".to_string(), None).unwrap();

        // Move `child` (and its subtree) under `target`.
        manager.move_folder(child.id, Some(target.id)).unwrap();

        let moved_child = manager.database().get_folder(child.id).unwrap().unwrap();
        assert_eq!(moved_child.parent_id, Some(target.id));
        assert_eq!(moved_child.depth, 1);

        // The grandchild keeps its parent (child) but shifts to the new depth.
        let moved_grandchild = manager
            .database()
            .get_folder(grandchild.id)
            .unwrap()
            .unwrap();
        assert_eq!(moved_grandchild.parent_id, Some(child.id));
        assert_eq!(moved_grandchild.depth, 2);
    }

    #[test]
    fn move_folder_to_root_shallows_subtree() {
        let manager = manager();
        let root = manager.create_folder("root".to_string(), None).unwrap();
        let child = manager
            .create_folder("child".to_string(), Some(root.id))
            .unwrap();
        let grandchild = manager
            .create_folder("grandchild".to_string(), Some(child.id))
            .unwrap();

        // Promote `child` to the root.
        manager.move_folder(child.id, None).unwrap();

        let moved_child = manager.database().get_folder(child.id).unwrap().unwrap();
        assert_eq!(moved_child.parent_id, None);
        assert_eq!(moved_child.depth, 0);
        let moved_grandchild = manager
            .database()
            .get_folder(grandchild.id)
            .unwrap()
            .unwrap();
        assert_eq!(moved_grandchild.depth, 1);
    }

    #[test]
    fn move_folder_rejects_moving_into_own_descendant() {
        let manager = manager();
        let root = manager.create_folder("root".to_string(), None).unwrap();
        let child = manager
            .create_folder("child".to_string(), Some(root.id))
            .unwrap();

        // Moving `root` into its own child would create a cycle.
        let err = manager.move_folder(root.id, Some(child.id)).unwrap_err();
        assert_eq!(err.code, "validation.destination");

        // Nothing changed.
        let read_root = manager.database().get_folder(root.id).unwrap().unwrap();
        assert_eq!(read_root.parent_id, None);
        assert_eq!(read_root.depth, 0);
    }

    #[test]
    fn move_folder_rejects_moving_into_itself() {
        let manager = manager();
        let f = manager.create_folder("f".to_string(), None).unwrap();
        let err = manager.move_folder(f.id, Some(f.id)).unwrap_err();
        assert_eq!(err.code, "validation.destination");
    }

    #[test]
    fn move_folder_rejects_exceeding_max_depth() {
        let manager = manager();
        // Build a chain root=0,1,2,...,9 (10 levels, the maximum).
        let mut parent: Option<FolderId> = None;
        let mut ids = Vec::new();
        for i in 0..=MAX_FOLDER_DEPTH {
            let folder = manager.create_folder(format!("level{i}"), parent).unwrap();
            parent = Some(folder.id);
            ids.push(folder.id);
        }
        // The deepest folder is at depth 9. A fresh root sits at depth 0.
        let other_root = manager.create_folder("other".to_string(), None).unwrap();

        // Moving `other_root` (and it has no children) under the depth-9 folder
        // would place it at depth 10 — one past the cap.
        let deepest = *ids.last().unwrap();
        let err = manager
            .move_folder(other_root.id, Some(deepest))
            .unwrap_err();
        assert_eq!(err.code, "validation.destination");
        assert!(err.message.to_lowercase().contains("depth"));
    }

    #[test]
    fn move_folder_missing_folder_reports_not_found() {
        let manager = manager();
        let err = manager.move_folder(FolderId::new(), None).unwrap_err();
        assert_eq!(err.code, "persistence.notFound");
    }

    #[test]
    fn validate_folder_move_is_pure_and_rejects_descendant() {
        use std::collections::HashMap;
        let root = Folder {
            id: FolderId::new(),
            name: "root".to_string(),
            parent_id: None,
            depth: 0,
        };
        let child = Folder {
            id: FolderId::new(),
            name: "child".to_string(),
            parent_id: Some(root.id),
            depth: 1,
        };
        let mut folders = HashMap::new();
        folders.insert(root.id, root.clone());
        folders.insert(child.id, child.clone());

        // Moving root under its child is a cycle: rejected.
        let err = validate_folder_move(&folders, root.id, Some(child.id)).unwrap_err();
        assert_eq!(err.code, "validation.destination");

        // Moving child to the root is fine, and the plan re-depths it to 0.
        let plan = validate_folder_move(&folders, child.id, None).unwrap();
        assert_eq!(plan.new_parent, None);
        assert_eq!(plan.new_depths, vec![(child.id, 0)]);
    }

    // --- Folder deletion with confirmation and cascade (task 3.13) ---------

    /// Create a profile inside `folder` and seed a credential for it, returning
    /// the stored profile. Keeps the deletion tests concise.
    async fn profile_in_folder(
        manager: &ConnectionManager<MockCredentialStore>,
        folder: FolderId,
    ) -> ConnectionProfile {
        let mut draft = valid_draft();
        draft.folder_id = Some(folder);
        let created = manager.create_profile(draft).unwrap();
        manager
            .credentials()
            .save(profile_password_key(created.id), Secret::new("secret"))
            .await
            .unwrap();
        created
    }

    #[tokio::test]
    async fn delete_nonempty_folder_without_confirmation_removes_nothing_and_signals() {
        let manager = manager();
        let folder = manager.create_folder("servers".to_string(), None).unwrap();
        let profile = profile_in_folder(&manager, folder.id).await;

        let err = manager.delete_folder(folder.id, false).await.unwrap_err();

        // Requirement 3.6/3.8: the gate is signalled and nothing is removed.
        assert_eq!(err.code, CONFIRMATION_REQUIRED_CODE);
        assert!(manager.database().get_folder(folder.id).unwrap().is_some());
        assert!(manager
            .database()
            .get_profile(profile.id)
            .unwrap()
            .is_some());
        assert!(manager
            .credentials()
            .contains(&profile_password_key(profile.id)));
    }

    #[tokio::test]
    async fn confirmed_delete_removes_subtree_and_credentials_leaving_outside_intact() {
        let manager = manager();
        // Target subtree: root -> child, each holding a profile.
        let root = manager.create_folder("root".to_string(), None).unwrap();
        let child = manager
            .create_folder("child".to_string(), Some(root.id))
            .unwrap();
        let p_root = profile_in_folder(&manager, root.id).await;
        let p_child = profile_in_folder(&manager, child.id).await;

        // Outside the subtree: a sibling folder with its own profile.
        let outside = manager.create_folder("outside".to_string(), None).unwrap();
        let p_outside = profile_in_folder(&manager, outside.id).await;

        let summary = manager.delete_folder(root.id, true).await.unwrap();

        // Requirement 3.7: the whole subtree is gone.
        assert!(manager.database().get_folder(root.id).unwrap().is_none());
        assert!(manager.database().get_folder(child.id).unwrap().is_none());
        assert!(manager.database().get_profile(p_root.id).unwrap().is_none());
        assert!(manager
            .database()
            .get_profile(p_child.id)
            .unwrap()
            .is_none());
        // Requirement 11.6: the contained profiles' credentials are cleaned.
        assert!(!manager
            .credentials()
            .contains(&profile_password_key(p_root.id)));
        assert!(!manager
            .credentials()
            .contains(&profile_password_key(p_child.id)));

        // Everything outside the subtree is untouched.
        assert!(manager.database().get_folder(outside.id).unwrap().is_some());
        assert!(manager
            .database()
            .get_profile(p_outside.id)
            .unwrap()
            .is_some());
        assert!(manager
            .credentials()
            .contains(&profile_password_key(p_outside.id)));

        // The summary names exactly what was removed.
        assert_eq!(summary.folders.len(), 2);
        assert!(summary.folders.contains(&root.id));
        assert!(summary.folders.contains(&child.id));
        assert_eq!(summary.profiles.len(), 2);
        assert!(summary.profiles.contains(&p_root.id));
        assert!(summary.profiles.contains(&p_child.id));
    }

    #[tokio::test]
    async fn cancelled_delete_of_nonempty_folder_is_a_no_op() {
        let manager = manager();
        let root = manager.create_folder("root".to_string(), None).unwrap();
        let child = manager
            .create_folder("child".to_string(), Some(root.id))
            .unwrap();
        let p_child = profile_in_folder(&manager, child.id).await;

        // Cancel == request without confirmation over a profile-bearing subtree.
        let err = manager.delete_folder(root.id, false).await.unwrap_err();
        assert_eq!(err.code, CONFIRMATION_REQUIRED_CODE);

        // Requirement 3.8: folder, nested folder, profile, and credential all
        // remain unchanged.
        assert!(manager.database().get_folder(root.id).unwrap().is_some());
        assert!(manager.database().get_folder(child.id).unwrap().is_some());
        assert!(manager
            .database()
            .get_profile(p_child.id)
            .unwrap()
            .is_some());
        assert!(manager
            .credentials()
            .contains(&profile_password_key(p_child.id)));
    }

    #[tokio::test]
    async fn empty_folder_is_deleted_without_requiring_confirmation() {
        let manager = manager();
        // A folder with a nested empty folder but no profiles anywhere.
        let root = manager.create_folder("root".to_string(), None).unwrap();
        let child = manager
            .create_folder("child".to_string(), Some(root.id))
            .unwrap();

        // confirmed = false, yet an empty subtree deletes directly (3.6 gates
        // confirmation on *containing profiles*).
        let summary = manager.delete_folder(root.id, false).await.unwrap();

        assert!(manager.database().get_folder(root.id).unwrap().is_none());
        assert!(manager.database().get_folder(child.id).unwrap().is_none());
        assert_eq!(summary.folders.len(), 2);
        assert!(summary.profiles.is_empty());
    }

    #[tokio::test]
    async fn confirmed_delete_of_missing_folder_is_a_benign_no_op() {
        let manager = manager();
        let summary = manager.delete_folder(FolderId::new(), true).await.unwrap();
        assert!(summary.folders.is_empty());
        assert!(summary.profiles.is_empty());
    }

    // --- Export / import (task 18.1) ---------------------------------------

    /// A unique temp file path for an export, removed on drop.
    struct TempFile {
        path: std::path::PathBuf,
    }

    impl TempFile {
        fn new(tag: &str) -> Self {
            let unique = format!(
                "novrix-export-{tag}-{}-{}.json",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            Self {
                path: std::env::temp_dir().join(unique),
            }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn export_writes_versioned_file_with_accurate_counts() {
        let manager = manager();
        let root = manager.create_folder("servers".to_string(), None).unwrap();
        let mut draft = valid_draft();
        draft.folder_id = Some(root.id);
        manager.create_profile(draft).unwrap();

        let file = TempFile::new("counts");
        let summary = manager.export_config(&file.path).unwrap();

        assert_eq!(summary.folders_exported, 1);
        assert_eq!(summary.profiles_exported, 1);

        // The file is a parseable, current-version export.
        let bytes = std::fs::read(&file.path).unwrap();
        let export = parse_export(&bytes).unwrap();
        assert_eq!(export.schema_version, EXPORT_SCHEMA_VERSION);
        assert_eq!(export.folders.len(), 1);
        assert_eq!(export.profiles.len(), 1);
    }

    #[test]
    fn import_adds_items_without_disturbing_existing() {
        // Source manager with a folder + profile; export it.
        let source = manager();
        let folder = source.create_folder("servers".to_string(), None).unwrap();
        let mut draft = valid_draft();
        draft.folder_id = Some(folder.id);
        source.create_profile(draft).unwrap();

        let file = TempFile::new("additive");
        source.export_config(&file.path).unwrap();

        // Target manager with its own pre-existing folder + profile.
        let target = manager();
        let existing_folder = target.create_folder("existing".to_string(), None).unwrap();
        let existing_profile = target.create_profile(valid_draft()).unwrap();

        let summary = target.import_config(&file.path).unwrap();
        assert_eq!(summary.folders_added, 1);
        assert_eq!(summary.profiles_added, 1);

        // Pre-existing items are untouched (Requirement 16.2).
        assert!(target
            .database()
            .get_folder(existing_folder.id)
            .unwrap()
            .is_some());
        assert!(target
            .database()
            .get_profile(existing_profile.id)
            .unwrap()
            .is_some());

        // Total is existing + imported.
        assert_eq!(target.database().list_folders().unwrap().len(), 2);
        assert_eq!(target.database().list_profiles().unwrap().len(), 2);
    }

    #[test]
    fn import_of_colliding_ids_adds_new_entries() {
        // Export from source, then import into a target that already holds the
        // SAME ids. Fresh ids on import mean both survive as distinct rows.
        let source = manager();
        let folder = source.create_folder("shared".to_string(), None).unwrap();

        let file = TempFile::new("collide");
        source.export_config(&file.path).unwrap();

        // Build a target containing a folder with the exact same id.
        let target = manager();
        target
            .database()
            .insert_folder(&Folder {
                id: folder.id,
                name: "shared".to_string(),
                parent_id: None,
                depth: 0,
            })
            .unwrap();

        let summary = target.import_config(&file.path).unwrap();
        assert_eq!(summary.folders_added, 1);

        // The colliding id did not overwrite: two folders now exist, and the
        // original id is still present.
        let folders = target.database().list_folders().unwrap();
        assert_eq!(folders.len(), 2);
        assert!(folders.iter().any(|f| f.id == folder.id));
    }

    #[test]
    fn malformed_import_is_rejected_leaving_config_unchanged() {
        let target = manager();
        let existing = target.create_profile(valid_draft()).unwrap();

        let file = TempFile::new("malformed");
        std::fs::write(&file.path, b"this is not a valid export").unwrap();

        let err = target.import_config(&file.path).unwrap_err();
        assert!(matches!(err, ImportError::Malformed(_)));

        // Nothing added, existing profile intact.
        assert_eq!(target.database().list_profiles().unwrap().len(), 1);
        assert!(target
            .database()
            .get_profile(existing.id)
            .unwrap()
            .is_some());
    }

    #[test]
    fn export_then_import_round_trip_reproduces_hierarchy() {
        let source = manager();
        let root = source.create_folder("root".to_string(), None).unwrap();
        let child = source
            .create_folder("child".to_string(), Some(root.id))
            .unwrap();
        let mut draft = valid_draft();
        draft.folder_id = Some(child.id);
        source.create_profile(draft).unwrap();

        let file = TempFile::new("roundtrip");
        source.export_config(&file.path).unwrap();

        let target = manager();
        target.import_config(&file.path).unwrap();

        let folders = target.database().list_folders().unwrap();
        let profiles = target.database().list_profiles().unwrap();
        assert_eq!(folders.len(), 2);
        assert_eq!(profiles.len(), 1);

        // The imported child points at the imported root (hierarchy preserved
        // under fresh ids).
        let new_root = folders.iter().find(|f| f.name == "root").unwrap();
        let new_child = folders.iter().find(|f| f.name == "child").unwrap();
        assert_eq!(new_child.parent_id, Some(new_root.id));
        // And the profile sits in the imported child.
        assert_eq!(profiles[0].folder_id, Some(new_child.id));
    }

    // --- Deletion-confirmation unit tests (tasks 3.7, 3.16) ----------------

    /// Task 3.7 (Requirement 2.7): deleting a profile must *request*
    /// confirmation before anything is removed. We model "request confirmation"
    /// as the manager refusing to delete when `confirmed = false`: the call
    /// returns the stable confirmation-required code and the profile is still
    /// present afterward, proving no removal happened ahead of confirmation.
    #[tokio::test]
    async fn profile_delete_requests_confirmation_before_removing() {
        let manager = manager();
        let created = manager.create_profile(valid_draft()).unwrap();

        // An unconfirmed delete is the "confirmation requested" state: it must
        // signal the gate and remove nothing.
        let err = manager.delete_profile(created.id, false).await.unwrap_err();
        assert_eq!(err.code, CONFIRMATION_REQUIRED_CODE);
        assert!(
            manager
                .database()
                .get_profile(created.id)
                .unwrap()
                .is_some(),
            "the profile must still exist until deletion is confirmed"
        );

        // Confirming then removes it, proving the gate was the only thing
        // standing between request and removal (Requirement 2.8).
        manager.delete_profile(created.id, true).await.unwrap();
        assert!(manager
            .database()
            .get_profile(created.id)
            .unwrap()
            .is_none());
    }

    /// Task 3.16 (Requirement 3.6): deleting a folder that *contains profiles*
    /// must request confirmation first. We model "request confirmation" as the
    /// manager refusing the unconfirmed delete over a profile-bearing folder:
    /// it returns the confirmation-required code and leaves the folder and its
    /// profile intact until the deletion is confirmed.
    #[tokio::test]
    async fn nonempty_folder_delete_requests_confirmation_before_removing() {
        let manager = manager();
        let folder = manager.create_folder("servers".to_string(), None).unwrap();
        let profile = profile_in_folder(&manager, folder.id).await;

        // The folder holds a profile, so an unconfirmed delete must request
        // confirmation and remove nothing.
        let err = manager.delete_folder(folder.id, false).await.unwrap_err();
        assert_eq!(err.code, CONFIRMATION_REQUIRED_CODE);
        assert!(
            manager.database().get_folder(folder.id).unwrap().is_some(),
            "the folder must still exist until deletion is confirmed"
        );
        assert!(
            manager
                .database()
                .get_profile(profile.id)
                .unwrap()
                .is_some(),
            "the contained profile must still exist until deletion is confirmed"
        );

        // Confirming then removes the folder and its contents (Requirement 3.7).
        let summary = manager.delete_folder(folder.id, true).await.unwrap();
        assert!(summary.folders.contains(&folder.id));
        assert!(summary.profiles.contains(&profile.id));
        assert!(manager.database().get_folder(folder.id).unwrap().is_none());
    }

    /// Property-based tests for the Connection_Manager profile and folder
    /// operations (tasks 3.2–3.4, 3.6, 3.9, 3.11, 3.12, 3.14, 3.15).
    ///
    /// Several properties exercise the async manager surface (`delete_profile`,
    /// `delete_folder`) and the async [`CredentialStore`]. proptest bodies are
    /// synchronous, so — exactly as the Credential_Store properties do — each
    /// such case builds a single-threaded Tokio runtime and `block_on`s the
    /// operations, with the in-memory [`MockCredentialStore`] standing in for
    /// the OS keystore. The pure validators (`validate_draft`,
    /// `validate_folder_name`, `validate_folder_move`) need no runtime and are
    /// driven directly.
    mod properties {
        use super::*;
        use proptest::collection::vec;
        use proptest::prelude::*;
        use tokio::runtime::Runtime;

        /// A fresh current-thread runtime for driving one async property case.
        fn runtime() -> Runtime {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("build tokio runtime")
        }

        /// The four Supported_Protocols, as a strategy.
        fn any_protocol() -> impl Strategy<Value = ProtocolKind> {
            prop_oneof![
                Just(ProtocolKind::Ssh),
                Just(ProtocolKind::Telnet),
                Just(ProtocolKind::Serial),
                Just(ProtocolKind::Rdp),
            ]
        }

        /// Protocol-specific params matching a chosen [`ProtocolKind`], so a
        /// generated draft is internally consistent and round-trips through
        /// persistence unchanged.
        fn params_for(kind: ProtocolKind) -> ProtocolParams {
            match kind {
                ProtocolKind::Ssh => ProtocolParams::Ssh {
                    known_host_policy: HostKeyPolicy::PromptOnUnknown,
                },
                ProtocolKind::Telnet => ProtocolParams::Telnet {},
                ProtocolKind::Serial => ProtocolParams::Serial {
                    port_name: "/dev/ttyUSB0".to_string(),
                    baud: crate::domain::BaudRate::B9600,
                    data_bits: crate::domain::DataBits::Eight,
                    parity: crate::domain::Parity::None,
                    stop_bits: crate::domain::StopBits::One,
                },
                ProtocolKind::Rdp => ProtocolParams::Rdp {
                    resolution: None,
                    cert_policy: crate::domain::HostKeyPolicy::PromptOnUnknown,
                },
            }
        }

        /// Build a draft from generated field values. Auth is a fixed password
        /// reference (never a secret) so the draft is a faithful, persistable
        /// profile shape without entangling the Credential_Store.
        fn draft_from(
            name: String,
            protocol: ProtocolKind,
            host: String,
            port: u16,
            timeout: u16,
        ) -> ProfileDraft {
            ProfileDraft {
                name,
                protocol,
                host,
                port,
                username: None,
                domain: None,
                folder_id: None,
                auth: AuthConfig::Password {
                    credential_key: CredentialKey::new("profile/x/password"),
                },
                protocol_params: params_for(protocol),
                logging_enabled: false,
                log_path: None,
                connect_timeout_secs: timeout,
            }
        }

        /// Assert two profiles agree on every *submitted* field (i.e. everything
        /// but the manager-assigned id), used to prove faithful persistence.
        fn assert_fields_eq(
            stored: &ConnectionProfile,
            draft: &ProfileDraft,
        ) -> Result<(), TestCaseError> {
            prop_assert_eq!(&stored.name, &draft.name);
            prop_assert_eq!(stored.protocol, draft.protocol);
            prop_assert_eq!(&stored.host, &draft.host);
            prop_assert_eq!(stored.port, draft.port);
            prop_assert_eq!(&stored.username, &draft.username);
            prop_assert_eq!(&stored.domain, &draft.domain);
            prop_assert_eq!(stored.folder_id, draft.folder_id);
            // Auth is carried through faithfully, except that a password
            // profile's credential key is normalized to the profile's own
            // canonical key (`profile/{id}/password`) — the UI no longer
            // submits a raw credential-key reference (Requirement 17). So for
            // password auth we assert the key is the canonical one; for every
            // other auth kind we assert exact equality with the draft.
            match (&draft.auth, &stored.auth) {
                (AuthConfig::Password { .. }, AuthConfig::Password { credential_key }) => {
                    prop_assert_eq!(credential_key, &profile_password_key(stored.id));
                }
                _ => prop_assert_eq!(&stored.auth, &draft.auth),
            }
            prop_assert_eq!(&stored.protocol_params, &draft.protocol_params);
            prop_assert_eq!(stored.logging_enabled, draft.logging_enabled);
            prop_assert_eq!(&stored.log_path, &draft.log_path);
            prop_assert_eq!(stored.connect_timeout_secs, draft.connect_timeout_secs);
            Ok(())
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(200))]

            // Feature: remote-connection-manager, Property 3: Valid profile is saved faithfully
            //
            // *For any* profile draft with a non-empty host, a port in 1..=65535,
            // and a Supported_Protocol, saving succeeds and the stored profile's
            // fields equal the submitted values; the same rule applies on edit.
            //
            // Validates: Requirements 2.1, 2.5
            #[test]
            fn valid_profile_is_saved_faithfully(
                name in "[a-zA-Z0-9 _-]{0,32}",
                protocol in any_protocol(),
                host in "[a-zA-Z0-9.-]{1,40}",
                port in 1u16..=65535,
                timeout in 1u16..=300,
                // A second, distinct set of fields to exercise the edit path.
                host2 in "[a-zA-Z0-9.-]{1,40}",
                port2 in 1u16..=65535,
            ) {
                let manager = manager();
                let draft = draft_from(name.clone(), protocol, host, port, timeout);

                // Create: saving a valid draft succeeds and the stored profile
                // mirrors every submitted field (Requirement 2.1).
                let stored = manager
                    .create_profile(draft.clone())
                    .expect("a valid draft must save");
                assert_fields_eq(&stored, &draft)?;
                // And it is actually persisted, byte-for-byte.
                let read = manager
                    .database()
                    .get_profile(stored.id)
                    .unwrap()
                    .expect("the profile must be persisted");
                prop_assert_eq!(&read, &stored);

                // Edit: the identical rules apply and the updated values persist
                // under the same id (Requirement 2.5).
                let mut edited = draft.clone();
                edited.host = host2;
                edited.port = port2;
                let updated = manager
                    .update_profile(stored.id, edited.clone())
                    .expect("a valid edit must save");
                prop_assert_eq!(updated.id, stored.id);
                assert_fields_eq(&updated, &edited)?;
                let read_back = manager
                    .database()
                    .get_profile(stored.id)
                    .unwrap()
                    .expect("the edit must be persisted");
                prop_assert_eq!(&read_back, &updated);
            }

            // Feature: remote-connection-manager, Property 4: Profile validation rejects invalid fields
            //
            // *For any* profile draft, the submission is rejected if and only if
            // the host is missing, the port is outside 1..=65535, or the protocol
            // is not a Supported_Protocol, and the rejection message identifies
            // the offending field. (This single port-range predicate also governs
            // Telnet connect-time validation.)
            //
            // Validates: Requirements 2.2, 2.3, 2.4, 5.5
            #[test]
            fn profile_validation_rejects_invalid_fields(
                // host may be empty / whitespace-only (invalid) or non-empty.
                host in "[ a-zA-Z0-9.-]{0,20}",
                // port spans the invalid 0 as well as the whole valid range.
                port in 0u16..=65535,
                protocol in any_protocol(),
                // A restricted supported set lets us model a protocol being
                // "not a Supported_Protocol" without an out-of-enum value.
                supported in prop::collection::hash_set(any_protocol(), 0..=4),
            ) {
                let supported_vec: Vec<ProtocolKind> = supported.iter().copied().collect();
                let draft = draft_from(
                    "p".to_string(),
                    protocol,
                    host.clone(),
                    port,
                    30,
                );

                // The reference predicate: exactly the three rejection reasons,
                // evaluated in the same field order the validator uses.
                let host_bad = host.trim().is_empty();
                let port_bad = !(1..=65535).contains(&port);
                let proto_bad = !supported_vec.contains(&protocol);
                let expected_field = if host_bad {
                    Some("host")
                } else if port_bad {
                    Some("port")
                } else if proto_bad {
                    Some("protocol")
                } else {
                    None
                };

                match validate_draft(&draft, &supported_vec) {
                    Ok(()) => {
                        // Accepted iff none of the three predicates fired.
                        prop_assert!(expected_field.is_none());
                    }
                    Err(e) => {
                        // Rejected iff some predicate fired, and the error names
                        // exactly the offending field.
                        prop_assert!(expected_field.is_some());
                        prop_assert_eq!(e.field.as_str(), expected_field.unwrap());
                    }
                }
            }

            // Feature: remote-connection-manager, Property 5: Profile save failure preserves prior state
            //
            // *For any* existing profile, when persistence fails during
            // save/update, the prior stored state is retained unchanged and an
            // error is returned.
            //
            // Validates: Requirements 2.6
            //
            // Modeling the failure: we provoke a *real* persistence failure
            // rather than a mocked one. After storing a profile, we attempt to
            // insert a second profile that reuses the stored id. The primary-key
            // collision makes the repository's write transaction fail and roll
            // back (see `with_write_tx`), which is exactly the save-failure path
            // Requirement 2.6 governs. We then assert the originally stored
            // profile is byte-for-byte unchanged and that an error was returned.
            #[test]
            fn profile_save_failure_preserves_prior_state(
                protocol in any_protocol(),
                host in "[a-zA-Z0-9.-]{1,40}",
                port in 1u16..=65535,
                // The clashing insert carries different field values, so if the
                // transaction *didn't* roll back we would observe a mutation.
                host2 in "[a-zA-Z0-9.-]{1,40}",
                port2 in 1u16..=65535,
            ) {
                let manager = manager();
                let draft = draft_from("original".to_string(), protocol, host, port, 30);
                let stored = manager.create_profile(draft).expect("seed save succeeds");

                // A second profile that deliberately reuses the stored id.
                let clashing = ConnectionProfile {
                    id: stored.id,
                    name: "intruder".to_string(),
                    host: host2,
                    port: port2,
                    ..stored.clone()
                };

                // The write must fail (duplicate primary key)...
                let result = manager.database().insert_profile(&clashing);
                prop_assert!(result.is_err(), "a duplicate-id insert must fail");

                // ...and the prior stored state is retained unchanged.
                let read = manager
                    .database()
                    .get_profile(stored.id)
                    .unwrap()
                    .expect("the original profile must still exist");
                prop_assert_eq!(read, stored);
            }

            // Feature: remote-connection-manager, Property 6: Confirmed profile deletion removes the profile
            //
            // *For any* stored profile, confirmed deletion removes it so that it
            // is no longer present.
            //
            // Validates: Requirements 2.8
            #[test]
            fn confirmed_profile_deletion_removes_the_profile(
                protocol in any_protocol(),
                host in "[a-zA-Z0-9.-]{1,40}",
                port in 1u16..=65535,
            ) {
                let manager = manager();
                let draft = draft_from("victim".to_string(), protocol, host, port, 30);
                let stored = manager.create_profile(draft).expect("seed save succeeds");

                runtime().block_on(async {
                    // Precondition: the profile exists before deletion.
                    prop_assert!(manager.database().get_profile(stored.id).unwrap().is_some());

                    manager
                        .delete_profile(stored.id, true)
                        .await
                        .expect("confirmed deletion succeeds");

                    // Postcondition: it is no longer present (Requirement 2.8).
                    prop_assert!(manager.database().get_profile(stored.id).unwrap().is_none());
                    Ok(())
                })?;
            }

            // Feature: remote-connection-manager, Property 7: Folder name validity
            //
            // *For any* folder name and set of sibling names, the create or
            // rename operation is accepted if and only if the name is 1..=255
            // characters and unique among siblings; otherwise it is rejected with
            // the specific reason, and on acceptance the name is persisted.
            //
            // Validates: Requirements 3.1, 3.2, 3.9, 3.10
            #[test]
            fn folder_name_validity(
                // Generate names across the empty / in-range / over-255 regimes.
                name in prop_oneof![
                    Just(String::new()),
                    "[a-zA-Z0-9 ]{1,10}",
                    "x{1,260}",
                ],
                siblings in vec("[a-zA-Z0-9 ]{1,10}", 0..6),
            ) {
                let len = name.chars().count();
                let empty = len < FOLDER_NAME_MIN_CHARS;
                let too_long = len > FOLDER_NAME_MAX_CHARS;
                let duplicate = siblings.iter().any(|s| s == &name);
                let should_accept = !empty && !too_long && !duplicate;

                match validate_folder_name(&name, &siblings) {
                    Ok(()) => {
                        prop_assert!(should_accept);
                    }
                    Err(e) => {
                        prop_assert!(!should_accept);
                        // Every folder-name rejection is reported on `name`.
                        prop_assert_eq!(e.field.as_str(), "name");
                    }
                }
            }

            // Feature: remote-connection-manager, Property 8: Moving a profile yields exactly one folder association
            //
            // *For any* profile and destination folder, after a move the profile
            // is associated with exactly the destination and with no previous
            // folder.
            //
            // Validates: Requirements 3.3
            #[test]
            fn moving_a_profile_yields_exactly_one_folder_association(
                protocol in any_protocol(),
                host in "[a-zA-Z0-9.-]{1,40}",
                port in 1u16..=65535,
                // Whether the profile starts in a folder, and whether the move
                // targets a folder or the root.
                start_in_folder in any::<bool>(),
                move_to_root in any::<bool>(),
            ) {
                let manager = manager();
                // Two distinct destination folders to move among.
                let origin = manager.create_folder("origin".to_string(), None).unwrap();
                let dest = manager.create_folder("dest".to_string(), None).unwrap();

                let mut draft = draft_from("p".to_string(), protocol, host, port, 30);
                draft.folder_id = if start_in_folder { Some(origin.id) } else { None };
                let created = manager.create_profile(draft).unwrap();

                let target = if move_to_root { None } else { Some(dest.id) };
                manager.move_profile(created.id, target).unwrap();

                let read = manager.database().get_profile(created.id).unwrap().unwrap();
                // The association is exactly the destination; the single
                // `folder_id` field means no prior association can linger
                // (Requirement 3.3).
                prop_assert_eq!(read.folder_id, target);
            }

            // Feature: remote-connection-manager, Property 9: Hierarchy integrity (depth and acyclicity)
            //
            // *For any* folder hierarchy and any move operation, the result keeps
            // nesting depth at most 10 levels and remains acyclic; an operation
            // that would exceed the depth cap or move a folder into one of its
            // own descendants is rejected.
            //
            // Validates: Requirements 3.4, 3.5
            #[test]
            fn hierarchy_integrity_depth_and_acyclicity(
                // A linear chain of this many folders (root=0 .. chain_len-1),
                // spanning well past the depth cap so moves can breach it.
                chain_len in 1usize..=12,
                // Index of the folder to move and the index of the destination
                // (or root when `to_root`).
                move_idx in 0usize..12,
                dest_idx in 0usize..12,
                to_root in any::<bool>(),
            ) {
                use std::collections::HashMap;

                // Build a linear chain of folders with correct depths.
                let mut folders: HashMap<FolderId, Folder> = HashMap::new();
                let mut ids: Vec<FolderId> = Vec::new();
                let mut parent: Option<FolderId> = None;
                for depth in 0..chain_len {
                    let id = FolderId::new();
                    folders.insert(id, Folder {
                        id,
                        name: format!("f{depth}"),
                        parent_id: parent,
                        depth: depth as u8,
                    });
                    ids.push(id);
                    parent = Some(id);
                }

                let move_id = ids[move_idx % chain_len];
                let dest = if to_root {
                    None
                } else {
                    Some(ids[dest_idx % chain_len])
                };

                // Reference expectations for the two rejection rules.
                let subtree = subtree_ids(&folders, move_id);
                let is_cycle = dest.map(|d| subtree.contains(&d)).unwrap_or(false);

                match validate_folder_move(&folders, move_id, dest) {
                    Ok(plan) => {
                        // Acceptance must not have been a cyclic move...
                        prop_assert!(!is_cycle);
                        // ...and every resulting depth stays within the cap.
                        for (_, new_depth) in &plan.new_depths {
                            prop_assert!(*new_depth <= MAX_FOLDER_DEPTH);
                        }
                        // The plan re-depths exactly the moved subtree.
                        prop_assert_eq!(plan.new_depths.len(), subtree.len());
                        // The moved folder's own new depth is 0 at the root or
                        // one deeper than the destination.
                        let moved_new_depth = plan
                            .new_depths
                            .iter()
                            .find(|(fid, _)| *fid == move_id)
                            .map(|(_, d)| *d)
                            .unwrap();
                        let expected_moved_depth = match dest {
                            None => 0,
                            Some(d) => folders.get(&d).unwrap().depth + 1,
                        };
                        prop_assert_eq!(moved_new_depth, expected_moved_depth);
                    }
                    Err(e) => {
                        // Rejection is either a cycle/descendant move or a
                        // depth-cap breach — both reported on `destination`.
                        prop_assert_eq!(e.code, "validation.destination".to_string());
                        // If it was not a cycle, then it must have breached the
                        // depth cap: the moved folder's new depth exceeds the cap.
                        if !is_cycle {
                            let moved_depth = folders.get(&move_id).unwrap().depth as i32;
                            let base = match dest {
                                None => 0i32,
                                Some(d) => folders.get(&d).unwrap().depth as i32 + 1,
                            };
                            let delta = base - moved_depth;
                            let max_subtree_depth = subtree
                                .iter()
                                .map(|fid| folders.get(fid).unwrap().depth as i32)
                                .max()
                                .unwrap();
                            prop_assert!(max_subtree_depth + delta > MAX_FOLDER_DEPTH as i32);
                        }
                    }
                }
            }

            // Feature: remote-connection-manager, Property 10: Confirmed folder deletion removes exactly its subtree
            //
            // *For any* folder hierarchy, confirmed deletion of a folder removes
            // that folder and all nested folders and profiles it contains, and
            // leaves everything outside that subtree unchanged.
            //
            // Validates: Requirements 3.7
            #[test]
            fn confirmed_folder_deletion_removes_exactly_its_subtree(
                // Depth of the target subtree chain (target -> ... nested).
                subtree_depth in 1usize..=4,
                // Whether each level of the subtree carries a profile.
                profiles_per_level in prop::collection::vec(any::<bool>(), 1..=4),
                // An outside sibling folder with its own profile that must
                // survive untouched.
                outside_has_profile in any::<bool>(),
            ) {
                let manager = manager();

                runtime().block_on(async {
                    // Build the target subtree as a chain under the root.
                    let mut subtree_folders: Vec<FolderId> = Vec::new();
                    let mut subtree_profiles: Vec<ProfileId> = Vec::new();
                    let mut parent: Option<FolderId> = None;
                    for level in 0..subtree_depth {
                        let folder = manager
                            .create_folder(format!("sub{level}"), parent)
                            .unwrap();
                        subtree_folders.push(folder.id);
                        if *profiles_per_level.get(level).unwrap_or(&false) {
                            let p = profile_in_folder(&manager, folder.id).await;
                            subtree_profiles.push(p.id);
                        }
                        parent = Some(folder.id);
                    }
                    let target = subtree_folders[0];

                    // An outside folder, outside the target subtree entirely.
                    let outside = manager.create_folder("outside".to_string(), None).unwrap();
                    let outside_profile = if outside_has_profile {
                        Some(profile_in_folder(&manager, outside.id).await.id)
                    } else {
                        None
                    };

                    // Confirmed deletion of the target.
                    let summary = manager.delete_folder(target, true).await.unwrap();

                    // The whole subtree — folders and profiles — is gone.
                    for fid in &subtree_folders {
                        prop_assert!(manager.database().get_folder(*fid).unwrap().is_none());
                        prop_assert!(summary.folders.contains(fid));
                    }
                    for pid in &subtree_profiles {
                        prop_assert!(manager.database().get_profile(*pid).unwrap().is_none());
                        prop_assert!(summary.profiles.contains(pid));
                        prop_assert!(!manager
                            .credentials()
                            .contains(&profile_password_key(*pid)));
                    }

                    // Everything outside the subtree is untouched.
                    prop_assert!(manager.database().get_folder(outside.id).unwrap().is_some());
                    if let Some(op) = outside_profile {
                        prop_assert!(manager.database().get_profile(op).unwrap().is_some());
                        prop_assert!(manager
                            .credentials()
                            .contains(&profile_password_key(op)));
                    }

                    // The summary names exactly the subtree, nothing extra.
                    prop_assert_eq!(summary.folders.len(), subtree_folders.len());
                    prop_assert_eq!(summary.profiles.len(), subtree_profiles.len());
                    Ok(())
                })?;
            }

            // Feature: remote-connection-manager, Property 11: Cancelled deletion is a no-op
            //
            // *For any* folder and its contents, cancelling the deletion
            // confirmation leaves the folder and all its contents unchanged.
            //
            // Validates: Requirements 3.8
            #[test]
            fn cancelled_deletion_is_a_no_op(
                subtree_depth in 1usize..=4,
                // At least one profile somewhere so the confirmation gate engages
                // (an empty subtree deletes without confirmation by design).
                profile_level in 0usize..4,
            ) {
                let manager = manager();

                runtime().block_on(async {
                    let mut folders: Vec<FolderId> = Vec::new();
                    let mut profiles: Vec<ProfileId> = Vec::new();
                    let mut parent: Option<FolderId> = None;
                    let gated_level = profile_level % subtree_depth;
                    for level in 0..subtree_depth {
                        let folder = manager
                            .create_folder(format!("sub{level}"), parent)
                            .unwrap();
                        folders.push(folder.id);
                        if level == gated_level {
                            let p = profile_in_folder(&manager, folder.id).await;
                            profiles.push(p.id);
                        }
                        parent = Some(folder.id);
                    }
                    let target = folders[0];

                    // Cancelling == requesting deletion without confirmation over
                    // a profile-bearing subtree: it must signal the gate...
                    let err = manager.delete_folder(target, false).await.unwrap_err();
                    prop_assert_eq!(err.code, CONFIRMATION_REQUIRED_CODE.to_string());

                    // ...and leave every folder, profile, and credential intact
                    // (Requirement 3.8).
                    for fid in &folders {
                        prop_assert!(manager.database().get_folder(*fid).unwrap().is_some());
                    }
                    for pid in &profiles {
                        prop_assert!(manager.database().get_profile(*pid).unwrap().is_some());
                        prop_assert!(manager
                            .credentials()
                            .contains(&profile_password_key(*pid)));
                    }
                    Ok(())
                })?;
            }
        }
    }
}
