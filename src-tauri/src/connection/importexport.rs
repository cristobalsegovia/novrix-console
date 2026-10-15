//! Versioned configuration export and additive import — pure core (task 18.1).
//!
//! This module holds the *pure*, persistence-free heart of the
//! Connection_Manager's import/export feature: building the versioned
//! [`ExportFile`] payload, parsing/validating an incoming file, and — the
//! subtle part — **remapping identifiers** so an imported configuration is
//! always grafted onto the existing one as brand-new entries. The
//! `ConnectionManager::export_config` / `import_config` methods in [`super`]
//! wrap these functions with the repository I/O (reading the whole
//! configuration, writing the file, inserting the remapped items in one
//! transaction).
//!
//! Keeping the logic here, free of any [`Database`](crate::persistence::Database)
//! handle, is deliberate: the id-remapping, round-trip, malformed-rejection,
//! colliding-id, and count-reporting behaviours (design Properties 31, 52, 53,
//! 54, 55 — subtasks 18.2–18.6) can then be exercised across many inputs
//! without touching SQLite or the filesystem.
//!
//! # What export guarantees (Requirement 11.5, design Property 31)
//!
//! [`build_export`] copies the live folders and profiles verbatim into an
//! [`ExportFile`]. Because the domain model never carries a secret — password
//! auth is an [`AuthConfig::Password`](crate::domain::AuthConfig::Password)
//! holding only a [`CredentialKey`](crate::domain::CredentialKey) *name* and
//! key auth an [`AuthConfig::SshKey`](crate::domain::AuthConfig::SshKey)
//! holding only a [`KeyId`](crate::domain::KeyId) — the export is *structurally
//! incapable* of containing a plaintext credential. Export therefore never
//! consults the Credential_Store; it only serializes references. The schema is
//! stamped with [`EXPORT_SCHEMA_VERSION`] so a future format change can be
//! detected on import.
//!
//! # What import guarantees (Requirements 16.2, 16.3, 16.4, 16.5)
//!
//! [`parse_export`] turns raw bytes into an [`ExportFile`] or an
//! [`ImportError`], rejecting invalid JSON ([`ImportError::Malformed`]) and an
//! unsupported `schema_version` ([`ImportError::UnsupportedVersion`]) *without*
//! the caller touching persistence — so a bad file leaves the configuration
//! unchanged (Requirement 16.3).
//!
//! [`remap_import`] then rewrites every incoming folder and profile to carry a
//! **fresh** [`FolderId`]/[`ProfileId`]. This is what makes import purely
//! additive (Requirement 16.4): even an item whose id collides with an existing
//! one gets a new id, so it lands as a *new* entry and can never overwrite what
//! is already stored.
//!
//! ## Internal-reference remapping (the tricky part)
//!
//! Regenerating ids would break the imported subtree's internal wiring if done
//! naïvely: a folder's `parent_id` and a profile's `folder_id` reference *other
//! folders in the same file*. So [`remap_import`] works in two passes:
//!
//! 1. **Allocate.** Walk the file's folders and profiles and build a map from
//!    each old [`FolderId`] to a freshly generated one, and likewise fresh
//!    [`ProfileId`]s for profiles. (Profile ids are not referenced by anything,
//!    so only folder ids need a lookup map, but both are regenerated.)
//! 2. **Rewrite.** Rebuild each folder with its new id and its `parent_id`
//!    translated through the folder map; rebuild each profile with its new id
//!    and its `folder_id` translated through the same map.
//!
//! A `parent_id`/`folder_id` that points at a folder **present in the file** is
//! translated to that folder's new id, so a folder imported under another
//! imported folder stays under it. A reference that points at a folder **not in
//! the file** (a dangling reference, or — defensively — one that happened to
//! match an *existing* folder) is dropped to `None` (promoted to the root)
//! rather than silently pointing into the existing configuration: the import
//! must not graft itself onto, or depend on, pre-existing folders, and must
//! never overwrite or re-parent them. This keeps the imported subtree
//! self-consistent and strictly additive.
//!
//! [`import_counts`] reports the number of folders and profiles the remap
//! produced, which is exactly what gets inserted — the accurate added counts
//! Requirement 16.5 demands.

use std::collections::HashMap;

use crate::domain::{ConnectionProfile, ExportFile, Folder, FolderId, ImportError, ProfileId};

/// The export-file format version this build writes and accepts on import.
///
/// Stamped into [`ExportFile::schema_version`] by [`build_export`] and checked
/// by [`parse_export`]. A file carrying any other version is rejected as
/// [`ImportError::UnsupportedVersion`] so the configuration is left untouched
/// rather than imported under assumptions that may not hold. Bumping this
/// constant (and teaching [`parse_export`] to accept the old value with a
/// migration) is how a future format change would ship.
pub const EXPORT_SCHEMA_VERSION: u32 = 1;

/// What an export produced: the counts written to the file.
///
/// Returned by `ConnectionManager::export_config` so the caller/UI can report
/// precisely how much was exported, mirroring the shape of [`ImportSummary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExportSummary {
    /// Number of folders written to the export file.
    pub folders_exported: usize,
    /// Number of profiles written to the export file.
    pub profiles_exported: usize,
}

/// What an import added: the accurate counts of newly inserted items
/// (Requirement 16.5, design Property 55).
///
/// Because import is strictly additive — every incoming item is inserted under
/// a fresh id — these counts equal the number of folders and profiles in the
/// (remapped) file. Returned by `ConnectionManager::import_config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImportSummary {
    /// Number of folders added to the existing configuration.
    pub folders_added: usize,
    /// Number of profiles added to the existing configuration.
    pub profiles_added: usize,
}

/// Assemble a versioned [`ExportFile`] from the live `folders` and `profiles`.
///
/// This is a verbatim copy stamped with [`EXPORT_SCHEMA_VERSION`]; it performs
/// no credential lookup and so cannot introduce a plaintext secret (Requirement
/// 11.5, design Property 31). The domain types it copies only ever hold
/// credential *references*, so the resulting file — however it is later
/// serialized — contains references alone.
pub fn build_export(folders: Vec<Folder>, profiles: Vec<ConnectionProfile>) -> ExportFile {
    ExportFile {
        schema_version: EXPORT_SCHEMA_VERSION,
        folders,
        profiles,
    }
}

/// Serialize an [`ExportFile`] to the on-disk JSON byte representation.
///
/// Kept here (next to [`parse_export`]) so the write format and the read format
/// are defined together. Pretty-printed for human inspectability; the content
/// is references only, never secrets.
pub fn serialize_export(export: &ExportFile) -> Result<Vec<u8>, ImportError> {
    serde_json::to_vec_pretty(export)
        .map_err(|e| ImportError::Malformed(format!("failed to serialize export: {e}")))
}

/// Parse raw export-file `bytes` into a validated [`ExportFile`], or the
/// specific reason it is unacceptable.
///
/// This is the pure gate that keeps a bad import from ever reaching persistence
/// (Requirement 16.3, design Property 53):
///
/// - invalid JSON, or JSON that does not match the [`ExportFile`] shape, is
///   rejected as [`ImportError::Malformed`];
/// - a well-formed file whose `schema_version` is not [`EXPORT_SCHEMA_VERSION`]
///   is rejected as [`ImportError::UnsupportedVersion`].
///
/// On `Ok` the file is structurally valid and version-compatible; the caller
/// may then [`remap_import`] it and insert the result.
pub fn parse_export(bytes: &[u8]) -> Result<ExportFile, ImportError> {
    let export: ExportFile =
        serde_json::from_slice(bytes).map_err(|e| ImportError::Malformed(e.to_string()))?;

    if export.schema_version != EXPORT_SCHEMA_VERSION {
        return Err(ImportError::UnsupportedVersion(export.schema_version));
    }

    Ok(export)
}

/// Rewrite an imported [`ExportFile`]'s folders and profiles to carry freshly
/// generated identifiers, remapping internal references so the imported subtree
/// stays self-consistent (Requirement 16.4, design Property 54).
///
/// Every folder and profile receives a brand-new id, so an incoming id that
/// collides with an existing one is added as a *new* entry rather than
/// overwriting it. Internal references are translated through a map built from
/// the file's own folders:
///
/// - a folder's `parent_id` pointing at another folder **in the file** is
///   rewritten to that folder's new id (the imported hierarchy is preserved);
/// - a profile's `folder_id` pointing at a folder **in the file** is rewritten
///   likewise (profiles stay in their imported folder);
/// - any reference **not** found among the file's folders is dropped to `None`
///   (promoted to the root), so the import never attaches to — or depends on —
///   a pre-existing folder.
///
/// Folder `depth` is preserved from the file; the imported hierarchy keeps the
/// same shape it had when exported. The returned vectors are exactly what the
/// caller inserts, and their lengths are the added counts ([`import_counts`]).
pub fn remap_import(export: ExportFile) -> (Vec<Folder>, Vec<ConnectionProfile>) {
    // Pass 1: allocate a fresh id for every folder in the file and remember the
    // old -> new mapping so references can be translated in pass 2.
    let folder_id_map: HashMap<FolderId, FolderId> = export
        .folders
        .iter()
        .map(|folder| (folder.id, FolderId::new()))
        .collect();

    // Pass 2a: rebuild folders with their new id and a translated parent.
    let folders = export
        .folders
        .into_iter()
        .map(|folder| {
            let new_id = folder_id_map[&folder.id];
            Folder {
                id: new_id,
                name: folder.name,
                // Translate the parent through the file's folder map; a parent
                // not in the file (dangling, or matching an existing folder) is
                // dropped to the root so the import stays self-contained.
                parent_id: folder
                    .parent_id
                    .and_then(|old_parent| folder_id_map.get(&old_parent).copied()),
                depth: folder.depth,
            }
        })
        .collect();

    // Pass 2b: rebuild profiles with a fresh id and a translated folder_id.
    let profiles = export
        .profiles
        .into_iter()
        .map(|profile| ConnectionProfile {
            id: ProfileId::new(),
            folder_id: profile
                .folder_id
                .and_then(|old_folder| folder_id_map.get(&old_folder).copied()),
            ..profile
        })
        .collect();

    (folders, profiles)
}

/// The accurate added counts for a remapped import (Requirement 16.5, design
/// Property 55).
///
/// Because import is strictly additive, every folder and profile in the
/// remapped vectors is inserted, so the counts are simply their lengths.
pub fn import_counts(folders: &[Folder], profiles: &[ConnectionProfile]) -> ImportSummary {
    ImportSummary {
        folders_added: folders.len(),
        profiles_added: profiles.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AuthConfig, CredentialKey, HostKeyPolicy, ProtocolKind, ProtocolParams};

    fn folder(name: &str, parent: Option<FolderId>, depth: u8) -> Folder {
        Folder {
            id: FolderId::new(),
            name: name.to_string(),
            parent_id: parent,
            depth,
        }
    }

    fn profile(name: &str, folder: Option<FolderId>) -> ConnectionProfile {
        ConnectionProfile {
            id: ProfileId::new(),
            name: name.to_string(),
            protocol: ProtocolKind::Ssh,
            host: "example.com".to_string(),
            port: 22,
            username: None,
            domain: None,
            folder_id: folder,
            auth: AuthConfig::Password {
                credential_key: CredentialKey::new("profile/pw"),
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
    fn build_export_stamps_current_version_and_copies_items() {
        let f = folder("servers", None, 0);
        let p = profile("web", Some(f.id));
        let export = build_export(vec![f.clone()], vec![p.clone()]);
        assert_eq!(export.schema_version, EXPORT_SCHEMA_VERSION);
        assert_eq!(export.folders, vec![f]);
        assert_eq!(export.profiles, vec![p]);
    }

    #[test]
    fn serialize_then_parse_round_trips() {
        let f = folder("servers", None, 0);
        let export = build_export(vec![f], vec![]);
        let bytes = serialize_export(&export).unwrap();
        let parsed = parse_export(&bytes).unwrap();
        assert_eq!(parsed, export);
    }

    #[test]
    fn serialized_export_contains_no_plaintext_secret() {
        // The reference ("profile/pw") may appear, but no secret value does,
        // because the model never carries one. This is a smoke check of the
        // structural guarantee behind Property 31.
        let f = folder("servers", None, 0);
        let p = profile("web", Some(f.id));
        let bytes = serialize_export(&build_export(vec![f], vec![p])).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("profile/pw")); // the reference key name
        assert!(!text.to_lowercase().contains("password\":\"")); // no literal secret field
    }

    #[test]
    fn parse_rejects_invalid_json_as_malformed() {
        let err = parse_export(b"not json at all").unwrap_err();
        assert!(matches!(err, ImportError::Malformed(_)));
    }

    #[test]
    fn parse_rejects_wrong_shape_as_malformed() {
        // Valid JSON, but not an ExportFile.
        let err = parse_export(br#"{"unexpected": true}"#).unwrap_err();
        assert!(matches!(err, ImportError::Malformed(_)));
    }

    #[test]
    fn parse_rejects_unsupported_version() {
        let export = ExportFile {
            schema_version: EXPORT_SCHEMA_VERSION + 1,
            folders: vec![],
            profiles: vec![],
        };
        let bytes = serde_json::to_vec(&export).unwrap();
        let err = parse_export(&bytes).unwrap_err();
        assert!(
            matches!(err, ImportError::UnsupportedVersion(v) if v == EXPORT_SCHEMA_VERSION + 1)
        );
    }

    #[test]
    fn remap_assigns_fresh_ids_to_every_item() {
        let f = folder("servers", None, 0);
        let p = profile("web", Some(f.id));
        let old_folder_id = f.id;
        let old_profile_id = p.id;

        let export = build_export(vec![f], vec![p]);
        let (folders, profiles) = remap_import(export);

        assert_ne!(folders[0].id, old_folder_id);
        assert_ne!(profiles[0].id, old_profile_id);
    }

    #[test]
    fn remap_preserves_internal_folder_hierarchy() {
        // root -> child; child.parent_id points at root (both in the file).
        let root = folder("root", None, 0);
        let child = folder("child", Some(root.id), 1);
        let export = build_export(vec![root.clone(), child.clone()], vec![]);

        let (folders, _) = remap_import(export);
        let new_root = folders.iter().find(|f| f.name == "root").unwrap();
        let new_child = folders.iter().find(|f| f.name == "child").unwrap();

        // The child now points at the root's NEW id, not the old one.
        assert_eq!(new_child.parent_id, Some(new_root.id));
        assert_ne!(new_child.parent_id, Some(root.id));
        assert_eq!(new_child.depth, 1);
    }

    #[test]
    fn remap_preserves_profile_folder_association() {
        let f = folder("servers", None, 0);
        let p = profile("web", Some(f.id));
        let export = build_export(vec![f], vec![p]);

        let (folders, profiles) = remap_import(export);
        // The profile points at the folder's new id.
        assert_eq!(profiles[0].folder_id, Some(folders[0].id));
    }

    #[test]
    fn remap_drops_dangling_references_to_root() {
        // A folder whose parent is not present in the file, and a profile whose
        // folder is not present either: both are promoted to the root.
        let orphan_parent = FolderId::new();
        let orphan_folder = FolderId::new();
        let f = folder("lonely", Some(orphan_parent), 1);
        let p = profile("web", Some(orphan_folder));
        let export = build_export(vec![f], vec![p]);

        let (folders, profiles) = remap_import(export);
        assert_eq!(folders[0].parent_id, None);
        assert_eq!(profiles[0].folder_id, None);
    }

    #[test]
    fn import_counts_match_lengths() {
        let folders = vec![folder("a", None, 0), folder("b", None, 0)];
        let profiles = vec![profile("p", None)];
        let summary = import_counts(&folders, &profiles);
        assert_eq!(summary.folders_added, 2);
        assert_eq!(summary.profiles_added, 1);
    }
}

#[cfg(test)]
mod property_tests {
    //! Property-based coverage for the import/export core (subtasks 18.2–18.6,
    //! design Properties 31, 52, 53, 54, 55).
    //!
    //! These exercise the *pure* functions in this module across many inputs,
    //! which is where the correctness lives. Two cases also drive a real
    //! in-memory [`Database`](crate::persistence::Database) (via
    //! [`Database::import_items`]) to assert the persistence-level guarantee
    //! that a pre-existing configuration is left untouched — this needs no
    //! Credential_Store because import only ever writes freshly-remapped
    //! references, never secrets.
    //!
    //! ## Strategy design: internally-consistent export files
    //!
    //! The round-trip and preservation claims (Property 52) only make sense for
    //! files whose internal references are valid — a folder's `parent_id` and a
    //! profile's `folder_id` must point at a folder *in the same file*, or
    //! `remap_import` would (correctly) drop the dangling reference to the root
    //! and the "structure preserved" comparison would be meaningless. So
    //! [`consistent_export`] builds a forest: each folder may only parent to an
    //! *earlier* folder in the list (guaranteeing acyclicity), its `depth` is
    //! derived from that parent, and each profile's `folder_id` is either `None`
    //! or one of the generated folders. Auth is drawn across all three
    //! [`AuthConfig`] shapes — `None`, `Password { credential_key }`, and
    //! `SshKey { key_id }` — so the credential-reference-only guarantee
    //! (Property 31) is checked against every variant.

    use super::*;
    use crate::domain::{
        AuthConfig, BaudRate, ConnectionProfile, CredentialKey, DataBits, ExportFile, Folder,
        FolderId, HostKeyPolicy, KeyId, Parity, ProfileId, ProtocolKind, ProtocolParams, StopBits,
    };
    use crate::persistence::Database;
    use proptest::collection::vec;
    use proptest::prelude::*;

    /// An [`AuthConfig`] across all three shapes, including the two that carry a
    /// credential *reference* (never a secret).
    fn any_auth() -> impl Strategy<Value = AuthConfig> {
        prop_oneof![
            Just(AuthConfig::None),
            "[a-zA-Z0-9/_-]{1,24}".prop_map(|k| AuthConfig::Password {
                credential_key: CredentialKey::new(k),
            }),
            Just(()).prop_map(|_| AuthConfig::SshKey {
                key_id: KeyId::new()
            }),
        ]
    }

    /// A protocol kind and a matching, internally-consistent params payload.
    fn any_protocol_params() -> impl Strategy<Value = (ProtocolKind, ProtocolParams)> {
        prop_oneof![
            Just((
                ProtocolKind::Ssh,
                ProtocolParams::Ssh {
                    known_host_policy: HostKeyPolicy::PromptOnUnknown,
                },
            )),
            Just((ProtocolKind::Telnet, ProtocolParams::Telnet {})),
            Just((
                ProtocolKind::Serial,
                ProtocolParams::Serial {
                    port_name: "/dev/ttyUSB0".to_string(),
                    baud: BaudRate::B9600,
                    data_bits: DataBits::Eight,
                    parity: Parity::None,
                    stop_bits: StopBits::One,
                },
            )),
            Just((ProtocolKind::Rdp, ProtocolParams::Rdp { resolution: None })),
        ]
    }

    /// A specification for a single profile, independent of which folder it will
    /// land in (the folder is chosen when assembling the forest below).
    #[derive(Debug, Clone)]
    struct ProfileSpec {
        name: String,
        host: String,
        port: u16,
        auth: AuthConfig,
        protocol: ProtocolKind,
        params: ProtocolParams,
        logging_enabled: bool,
        timeout: u16,
        /// Index into the generated folder list, or `None` for the root. Taken
        /// modulo the folder count when the forest is assembled.
        folder_slot: Option<usize>,
    }

    fn any_profile_spec() -> impl Strategy<Value = ProfileSpec> {
        (
            "[a-zA-Z0-9 _-]{0,24}",
            "[a-zA-Z0-9.-]{1,32}",
            1u16..=65535,
            any_auth(),
            any_protocol_params(),
            any::<bool>(),
            1u16..=600,
            proptest::option::of(0usize..32),
        )
            .prop_map(
                |(
                    name,
                    host,
                    port,
                    auth,
                    (protocol, params),
                    logging_enabled,
                    timeout,
                    folder_slot,
                )| {
                    ProfileSpec {
                        name,
                        host,
                        port,
                        auth,
                        protocol,
                        params,
                        logging_enabled,
                        timeout,
                        folder_slot,
                    }
                },
            )
    }

    /// A specification for a single folder: its name and the slot of an
    /// *earlier* folder to parent to (or root). Resolving against earlier slots
    /// only guarantees the forest is acyclic.
    #[derive(Debug, Clone)]
    struct FolderSpec {
        name: String,
        /// `None` = root; `Some(k)` parents to generated folder at index
        /// `k % position` (always an earlier folder), so no cycles arise.
        parent_slot: Option<usize>,
    }

    fn any_folder_spec() -> impl Strategy<Value = FolderSpec> {
        ("[a-zA-Z0-9 _-]{1,16}", proptest::option::of(0usize..32))
            .prop_map(|(name, parent_slot)| FolderSpec { name, parent_slot })
    }

    /// Assemble an internally-consistent [`ExportFile`] from folder/profile
    /// specs: a forest with correct depths and every reference pointing at a
    /// folder present in the file.
    fn assemble_export(
        folder_specs: Vec<FolderSpec>,
        profile_specs: Vec<ProfileSpec>,
    ) -> ExportFile {
        let mut folders: Vec<Folder> = Vec::with_capacity(folder_specs.len());
        for (position, spec) in folder_specs.iter().enumerate() {
            // A parent may only be an *earlier* folder, which keeps the forest
            // acyclic and lets depth be computed from the already-built parent.
            let (parent_id, depth) = match spec.parent_slot {
                Some(k) if position > 0 => {
                    let parent = &folders[k % position];
                    (Some(parent.id), parent.depth.saturating_add(1))
                }
                _ => (None, 0),
            };
            folders.push(Folder {
                id: FolderId::new(),
                name: spec.name.clone(),
                parent_id,
                depth,
            });
        }

        let profiles: Vec<ConnectionProfile> = profile_specs
            .into_iter()
            .map(|spec| {
                let folder_id = match spec.folder_slot {
                    Some(k) if !folders.is_empty() => Some(folders[k % folders.len()].id),
                    _ => None,
                };
                ConnectionProfile {
                    id: ProfileId::new(),
                    name: spec.name,
                    protocol: spec.protocol,
                    host: spec.host,
                    port: spec.port,
                    username: None,
                    domain: None,
                    folder_id,
                    auth: spec.auth,
                    protocol_params: spec.params,
                    logging_enabled: spec.logging_enabled,
                    log_path: None,
                    connect_timeout_secs: spec.timeout,
                }
            })
            .collect();

        build_export(folders, profiles)
    }

    /// A strategy for an internally-consistent export file (forest + profiles).
    fn consistent_export() -> impl Strategy<Value = ExportFile> {
        (vec(any_folder_spec(), 0..8), vec(any_profile_spec(), 0..8))
            .prop_map(|(folder_specs, profile_specs)| assemble_export(folder_specs, profile_specs))
    }

    /// A fingerprint of a folder's *content and shape* independent of its id:
    /// `(name, depth, parent-by-name)`. Comparing multisets of these across a
    /// round-trip proves structure is preserved even though ids are regenerated.
    fn folder_shape(folders: &[Folder]) -> Vec<(String, u8, Option<String>)> {
        let name_by_id: std::collections::HashMap<FolderId, String> =
            folders.iter().map(|f| (f.id, f.name.clone())).collect();
        let mut shapes: Vec<(String, u8, Option<String>)> = folders
            .iter()
            .map(|f| {
                (
                    f.name.clone(),
                    f.depth,
                    f.parent_id.and_then(|p| name_by_id.get(&p).cloned()),
                )
            })
            .collect();
        shapes.sort();
        shapes
    }

    /// A fingerprint of a profile's content and shape independent of its id:
    /// every submitted field plus its folder-by-name. Comparing multisets of
    /// these proves profiles are preserved across a round-trip.
    fn profile_shape(
        folders: &[Folder],
        profiles: &[ConnectionProfile],
    ) -> Vec<(
        String,
        ProtocolKind,
        String,
        u16,
        Option<String>,
        AuthConfig,
        bool,
    )> {
        let name_by_id: std::collections::HashMap<FolderId, String> =
            folders.iter().map(|f| (f.id, f.name.clone())).collect();
        let mut shapes: Vec<_> = profiles
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    p.protocol,
                    p.host.clone(),
                    p.port,
                    p.folder_id.and_then(|f| name_by_id.get(&f).cloned()),
                    p.auth.clone(),
                    p.logging_enabled,
                )
            })
            .collect();
        // `ProtocolKind`/`AuthConfig` are not `Ord`, so sort by a stable
        // serialized key to get a canonical ordering for multiset comparison.
        shapes.sort_by_cached_key(|s| {
            (
                s.0.clone(),
                serde_json::to_string(&s.1).unwrap_or_default(),
                s.2.clone(),
                s.3,
                s.4.clone(),
                serde_json::to_string(&s.5).unwrap_or_default(),
                s.6,
            )
        });
        shapes
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        // Feature: remote-connection-manager, Property 31: Exports contain no plaintext credentials
        //
        // *For any* configuration, the generated export file contains no
        // plaintext credential value.
        //
        // Validates: Requirements 11.5
        //
        // Approach: the domain model is structurally incapable of carrying a
        // secret — password auth holds only a `CredentialKey` *name* and key
        // auth only a `KeyId`, so the Credential_Store is never consulted by
        // export. The property therefore asserts the *structural* guarantee: in
        // the serialized JSON, every profile's `auth` is one of the three
        // reference-only shapes (`none`, `password` with a `credential_key`
        // string, `sshKey` with a `key_id` string) and no secret-bearing field
        // name — `password`, `passphrase`, `privateKey`, `secret` as a value —
        // ever appears. Even when a profile is built with a credential_key that
        // *looks* like a reference ("profile/x/password"), it travels only as a
        // reference string, never as a stored secret value.
        #[test]
        fn exports_contain_no_plaintext_credentials(export in consistent_export()) {
            let bytes = serialize_export(&export).expect("serialize must succeed");
            let text = String::from_utf8(bytes).expect("export is utf-8 json");
            let json: serde_json::Value = serde_json::from_str(&text).expect("export is json");

            // The export never grows a secret-bearing *field*: there is no key
            // named like a secret anywhere in the document, only reference keys
            // (`credential_key`, `key_id`). We check the structural auth shapes.
            for profile in json["profiles"].as_array().expect("profiles array") {
                let auth = &profile["auth"];
                let kind = auth["type"].as_str().expect("auth has a type tag");
                match kind {
                    "none" => {
                        // No reference and certainly no secret.
                        prop_assert!(auth.get("credential_key").is_none());
                        prop_assert!(auth.get("key_id").is_none());
                    }
                    "password" => {
                        // Exactly a reference string, never a secret value.
                        prop_assert!(auth["credential_key"].is_string());
                        prop_assert!(auth.get("password").is_none());
                        prop_assert!(auth.get("passphrase").is_none());
                    }
                    "sshKey" => {
                        // Exactly a key *id* reference, never key material.
                        prop_assert!(auth["key_id"].is_string());
                        prop_assert!(auth.get("privateKey").is_none());
                        prop_assert!(auth.get("private_key").is_none());
                        prop_assert!(auth.get("passphrase").is_none());
                    }
                    other => prop_assert!(false, "unexpected auth type {other}"),
                }
            }

            // Document-wide: no field key names a secret. (The reference key
            // names `credential_key` / `key_id` are allowed; a bare `password`
            // or `passphrase` field would indicate a leak.)
            fn has_secret_field(v: &serde_json::Value) -> bool {
                match v {
                    serde_json::Value::Object(map) => {
                        map.keys().any(|k| {
                            let k = k.as_str();
                            k == "password"
                                || k == "passphrase"
                                || k == "privateKey"
                                || k == "private_key"
                                || k == "secret"
                        }) || map.values().any(has_secret_field)
                    }
                    serde_json::Value::Array(items) => items.iter().any(has_secret_field),
                    _ => false,
                }
            }
            prop_assert!(!has_secret_field(&json), "export must not carry a secret-bearing field");
        }

        // Feature: remote-connection-manager, Property 52: Export then import preserves and adds without disturbing existing data
        //
        // *For any* existing configuration and any valid export file, importing
        // adds exactly the file's folders and profiles while leaving all
        // pre-existing folders and profiles unchanged; a round-trip export of a
        // configuration followed by import reproduces that configuration's
        // folders and profiles.
        //
        // Validates: Requirements 16.1, 16.2
        //
        // Two halves, both over the pure core and a real in-memory database:
        //   (a) round-trip: remapping a consistent file preserves its folder
        //       forest shape and every profile's content (ids change, structure
        //       does not);
        //   (b) additive-into-existing: inserting the remapped items into a DB
        //       that already holds a seeded configuration leaves every seeded
        //       folder/profile byte-for-byte unchanged and grows the store by
        //       exactly the imported counts.
        #[test]
        fn export_then_import_preserves_and_adds(
            existing in consistent_export(),
            incoming in consistent_export(),
        ) {
            // (a) Round-trip shape preservation on the incoming file.
            let before_folders = incoming.folders.clone();
            let before_profiles = incoming.profiles.clone();
            let (remapped_folders, remapped_profiles) = remap_import(incoming);

            prop_assert_eq!(
                folder_shape(&remapped_folders),
                folder_shape(&before_folders),
                "folder forest shape must survive the round-trip"
            );
            prop_assert_eq!(
                profile_shape(&remapped_folders, &remapped_profiles),
                profile_shape(&before_folders, &before_profiles),
                "every profile's content must survive the round-trip"
            );

            // (b) Seed an existing configuration, then import on top of it.
            let db = Database::open_in_memory().expect("open in-memory db");
            // Seed folders parent-before-child (sorted by depth) so FKs resolve.
            let mut seed_folders = existing.folders.clone();
            seed_folders.sort_by_key(|f| f.depth);
            for folder in &seed_folders {
                db.insert_folder(folder).expect("seed folder");
            }
            for profile in &existing.profiles {
                db.insert_profile(profile).expect("seed profile");
            }

            let existing_folder_count = existing.folders.len();
            let existing_profile_count = existing.profiles.len();

            // Import the remapped incoming items (fresh ids, so no collisions).
            db.import_items(&remapped_folders, &remapped_profiles)
                .expect("import must succeed");

            // Every seeded item is still present and unchanged.
            for folder in &existing.folders {
                let read = db.get_folder(folder.id).expect("read folder");
                prop_assert_eq!(read.as_ref(), Some(folder), "seeded folder must be unchanged");
            }
            for profile in &existing.profiles {
                let read = db.get_profile(profile.id).expect("read profile");
                prop_assert_eq!(read.as_ref(), Some(profile), "seeded profile must be unchanged");
            }

            // The store grew by exactly the imported counts.
            prop_assert_eq!(
                db.list_folders().unwrap().len(),
                existing_folder_count + remapped_folders.len()
            );
            prop_assert_eq!(
                db.list_profiles().unwrap().len(),
                existing_profile_count + remapped_profiles.len()
            );
        }

        // Feature: remote-connection-manager, Property 53: Malformed import is rejected and leaves configuration unchanged
        //
        // *For any* malformed or non-conforming import file, the import is
        // rejected, the existing configuration is left unchanged, and a
        // malformed-file error is reported.
        //
        // Validates: Requirements 16.3
        //
        // We generate three families of bad input: arbitrary non-JSON bytes,
        // valid JSON of the wrong shape, and a well-formed export carrying an
        // unsupported schema version. `parse_export` must reject each (Malformed
        // or UnsupportedVersion), and — since rejection happens before any
        // write — a seeded in-memory configuration is left byte-for-byte intact
        // (modelled by never reaching `import_items`).
        #[test]
        fn malformed_import_is_rejected_and_leaves_config_unchanged(
            garbage in "[^{}\\[\\]\"]{1,40}",
            wrong_shape_key in "[a-zA-Z_]{1,16}",
            bad_version in prop_oneof![Just(0u32), (2u32..=1000)],
            seed in consistent_export(),
        ) {
            // Seed an existing configuration in a real database.
            let db = Database::open_in_memory().expect("open in-memory db");
            let mut seed_folders = seed.folders.clone();
            seed_folders.sort_by_key(|f| f.depth);
            for folder in &seed_folders {
                db.insert_folder(folder).expect("seed folder");
            }
            for profile in &seed.profiles {
                db.insert_profile(profile).expect("seed profile");
            }
            let folders_before = db.list_folders().unwrap();
            let profiles_before = db.list_profiles().unwrap();

            // Family 1: arbitrary non-JSON bytes -> Malformed.
            let err = parse_export(garbage.as_bytes()).unwrap_err();
            prop_assert!(matches!(err, ImportError::Malformed(_)));

            // Family 2: valid JSON, wrong shape -> Malformed.
            let wrong = format!("{{\"{wrong_shape_key}\": 123}}");
            let err = parse_export(wrong.as_bytes()).unwrap_err();
            prop_assert!(matches!(err, ImportError::Malformed(_)));

            // Family 3: well-formed export, unsupported version -> UnsupportedVersion.
            let bad = ExportFile {
                schema_version: bad_version,
                folders: vec![],
                profiles: vec![],
            };
            let bad_bytes = serialize_export(&bad).unwrap();
            match parse_export(&bad_bytes) {
                Err(ImportError::UnsupportedVersion(v)) => prop_assert_eq!(v, bad_version),
                // Guard: if a generated "bad" version happened to equal the
                // supported one it would parse Ok — exclude that by construction.
                other => prop_assert!(
                    bad_version == EXPORT_SCHEMA_VERSION,
                    "unexpected parse result for bad version: {:?}",
                    other
                ),
            }

            // Because every rejection happens in `parse_export` — before any
            // write — the seeded configuration is untouched.
            prop_assert_eq!(db.list_folders().unwrap(), folders_before);
            prop_assert_eq!(db.list_profiles().unwrap(), profiles_before);
        }

        // Feature: remote-connection-manager, Property 54: Colliding identifiers are imported as new entries
        //
        // *For any* import whose folder or profile identifiers collide with
        // existing ones, the imported items are added as new entries with fresh
        // identifiers and the existing items are preserved unchanged.
        //
        // Validates: Requirements 16.4
        //
        // We force a worst-case collision: the incoming file reuses the *exact*
        // ids of a seeded configuration (same folders and profiles). After
        // remap every imported id must differ from every existing id, importing
        // must succeed, every seeded item stays unchanged, and the store ends up
        // with both copies.
        #[test]
        fn colliding_identifiers_are_imported_as_new_entries(seed in consistent_export()) {
            // Build an incoming file that collides on every id with the seed.
            let incoming = ExportFile {
                schema_version: EXPORT_SCHEMA_VERSION,
                folders: seed.folders.clone(),
                profiles: seed.profiles.clone(),
            };

            let existing_folder_ids: std::collections::HashSet<FolderId> =
                seed.folders.iter().map(|f| f.id).collect();
            let existing_profile_ids: std::collections::HashSet<ProfileId> =
                seed.profiles.iter().map(|p| p.id).collect();

            let (remapped_folders, remapped_profiles) = remap_import(incoming);

            // Every remapped id is fresh — none collides with an existing one.
            for folder in &remapped_folders {
                prop_assert!(
                    !existing_folder_ids.contains(&folder.id),
                    "remapped folder id must not collide with an existing id"
                );
            }
            for profile in &remapped_profiles {
                prop_assert!(
                    !existing_profile_ids.contains(&profile.id),
                    "remapped profile id must not collide with an existing id"
                );
            }

            // Persist seed then import: both coexist, seed unchanged.
            let db = Database::open_in_memory().expect("open in-memory db");
            let mut seed_folders = seed.folders.clone();
            seed_folders.sort_by_key(|f| f.depth);
            for folder in &seed_folders {
                db.insert_folder(folder).expect("seed folder");
            }
            for profile in &seed.profiles {
                db.insert_profile(profile).expect("seed profile");
            }

            db.import_items(&remapped_folders, &remapped_profiles)
                .expect("colliding import still succeeds as additive");

            // Seed preserved unchanged.
            for folder in &seed.folders {
                let read = db.get_folder(folder.id).unwrap();
                prop_assert_eq!(read.as_ref(), Some(folder));
            }
            for profile in &seed.profiles {
                let read = db.get_profile(profile.id).unwrap();
                prop_assert_eq!(read.as_ref(), Some(profile));
            }

            // Both copies now coexist in the store.
            prop_assert_eq!(
                db.list_folders().unwrap().len(),
                seed.folders.len() + remapped_folders.len()
            );
            prop_assert_eq!(
                db.list_profiles().unwrap().len(),
                seed.profiles.len() + remapped_profiles.len()
            );
        }

        // Feature: remote-connection-manager, Property 55: Import reports accurate added counts
        //
        // *For any* successful import, the reported number of folders and
        // profiles added equals the actual number added.
        //
        // Validates: Requirements 16.5
        //
        // The reported counts come from `import_counts` over the remapped
        // vectors; we assert they equal both the remapped lengths and the actual
        // growth of a real in-memory store after `import_items`.
        #[test]
        fn import_reports_accurate_added_counts(
            seed in consistent_export(),
            incoming in consistent_export(),
        ) {
            let incoming_folder_count = incoming.folders.len();
            let incoming_profile_count = incoming.profiles.len();

            let (remapped_folders, remapped_profiles) = remap_import(incoming);
            let summary = import_counts(&remapped_folders, &remapped_profiles);

            // The reported counts match the remapped vector lengths, which in
            // turn equal the incoming item counts (import is 1:1 additive).
            prop_assert_eq!(summary.folders_added, remapped_folders.len());
            prop_assert_eq!(summary.profiles_added, remapped_profiles.len());
            prop_assert_eq!(summary.folders_added, incoming_folder_count);
            prop_assert_eq!(summary.profiles_added, incoming_profile_count);

            // And they equal the real growth of a seeded store after import.
            let db = Database::open_in_memory().expect("open in-memory db");
            let mut seed_folders = seed.folders.clone();
            seed_folders.sort_by_key(|f| f.depth);
            for folder in &seed_folders {
                db.insert_folder(folder).expect("seed folder");
            }
            for profile in &seed.profiles {
                db.insert_profile(profile).expect("seed profile");
            }
            let folders_before = db.list_folders().unwrap().len();
            let profiles_before = db.list_profiles().unwrap().len();

            db.import_items(&remapped_folders, &remapped_profiles)
                .expect("import must succeed");

            let folders_after = db.list_folders().unwrap().len();
            let profiles_after = db.list_profiles().unwrap().len();

            prop_assert_eq!(folders_after - folders_before, summary.folders_added);
            prop_assert_eq!(profiles_after - profiles_before, summary.profiles_added);
        }
    }
}
