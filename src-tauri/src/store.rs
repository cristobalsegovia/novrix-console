//! Saved-session persistence. Groups (one per client, per site, per lab…)
//! contain saved sessions. Only connection metadata lives here — secrets go
//! through the `secrets` module into the OS keychain.

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tauri::State;
use uuid::Uuid;

use crate::secrets;

pub struct Store(Mutex<Connection>);

pub fn data_dir() -> PathBuf {
    let dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("switchboard-term");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

impl Store {
    pub fn open() -> Result<Self, String> {
        let conn = Connection::open(data_dir().join("switchboard.db"))
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS groups (
               id TEXT PRIMARY KEY,
               name TEXT NOT NULL,
               parent_id TEXT REFERENCES groups(id) ON DELETE CASCADE,
               position INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS saved_sessions (
               id TEXT PRIMARY KEY,
               group_id TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
               name TEXT NOT NULL,
               host TEXT NOT NULL,
               port INTEGER NOT NULL DEFAULT 22,
               username TEXT NOT NULL,
               auth_method TEXT NOT NULL DEFAULT 'password',
               key_path TEXT,
               protocol TEXT NOT NULL DEFAULT 'ssh',
               domain TEXT,
               rdp_resolution TEXT,
               position INTEGER NOT NULL DEFAULT 0
             );",
        )
        .map_err(|e| e.to_string())?;

        // Migrate older databases that predate the RDP and v0.7 columns. Each ALTER is
        // ignored if the column already exists, so this is safe to re-run.
        for stmt in [
            "ALTER TABLE saved_sessions ADD COLUMN protocol TEXT NOT NULL DEFAULT 'ssh'",
            "ALTER TABLE saved_sessions ADD COLUMN domain TEXT",
            "ALTER TABLE saved_sessions ADD COLUMN rdp_resolution TEXT",
            "ALTER TABLE groups ADD COLUMN parent_id TEXT REFERENCES groups(id) ON DELETE CASCADE",
            "ALTER TABLE groups ADD COLUMN default_identity_id TEXT",
            "ALTER TABLE saved_sessions ADD COLUMN identity_id TEXT",
            "ALTER TABLE saved_sessions ADD COLUMN jump_session_id TEXT",
            "ALTER TABLE saved_sessions ADD COLUMN legacy_crypto INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE saved_sessions ADD COLUMN serial_port TEXT",
            "ALTER TABLE saved_sessions ADD COLUMN baud_rate INTEGER NOT NULL DEFAULT 115200",
        ] {
            let _ = conn.execute(stmt, []);
        }

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS identities (
               id TEXT PRIMARY KEY,
               name TEXT NOT NULL,
               username TEXT NOT NULL,
               auth_method TEXT NOT NULL DEFAULT 'password',
               key_path TEXT,
               position INTEGER NOT NULL DEFAULT 0
             );",
        )
        .map_err(|e| e.to_string())?;

        // Command snippets (editable name + payload), plus a seed on first run.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS snippets (
               id TEXT PRIMARY KEY,
               name TEXT NOT NULL,
               command TEXT NOT NULL,
               position INTEGER NOT NULL DEFAULT 0
             );",
        )
        .map_err(|e| e.to_string())?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM snippets", [], |r| r.get(0))
            .unwrap_or(0);
        if count == 0 {
            let seed = [
                ("show run", "show running-config"),
                ("show ver", "show version"),
                ("int brief", "show ip interface brief"),
                ("cdp nei", "show cdp neighbors detail"),
                ("int status", "show interface status"),
                ("mac table", "show mac address-table"),
            ];
            for (i, (name, cmd)) in seed.iter().enumerate() {
                let _ = conn.execute(
                    "INSERT INTO snippets (id, name, command, position) VALUES (?1, ?2, ?3, ?4)",
                    params![Uuid::new_v4().to_string(), name, cmd, i as i64],
                );
            }
        }

        // Expect / Auto-Responder triggers (SecureCRT style Send/Expect macros).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS expect_rules (
               id TEXT PRIMARY KEY,
               name TEXT NOT NULL,
               expect_pattern TEXT NOT NULL,
               send_text TEXT NOT NULL,
               enabled INTEGER NOT NULL DEFAULT 1,
               position INTEGER NOT NULL DEFAULT 0
             );",
        )
        .map_err(|e| e.to_string())?;
        let expect_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM expect_rules", [], |r| r.get(0))
            .unwrap_or(0);
        if expect_count == 0 {
            let expect_seed = [
                ("Cisco Pager (--More--)", "--More--", " ", 0i64),
                ("Cisco Console Wakeup", "Press RETURN to get started", "\\r", 0i64),
                ("Cisco Auto-Enable Prompt", "Router>", "enable\\r", 0i64),
            ];
            for (i, (name, pat, send, enabled)) in expect_seed.iter().enumerate() {
                let _ = conn.execute(
                    "INSERT INTO expect_rules (id, name, expect_pattern, send_text, enabled, position)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![Uuid::new_v4().to_string(), name, pat, send, enabled, i as i64],
                );
            }
        }
        Ok(Store(Mutex::new(conn)))
    }

    pub fn get_session(&self, id: &str) -> Result<SavedSession, String> {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT id, group_id, name, host, port, username, auth_method, key_path,
                    protocol, domain, rdp_resolution, identity_id, jump_session_id,
                    legacy_crypto, serial_port, baud_rate
             FROM saved_sessions WHERE id = ?1",
            params![id],
            row_to_session,
        )
        .map_err(|e| e.to_string())
    }

    pub fn get_identity(&self, id: &str) -> Result<Identity, String> {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT id, name, username, auth_method, key_path
             FROM identities WHERE id = ?1",
            params![id],
            |r| {
                Ok(Identity {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    username: r.get(2)?,
                    auth_method: r.get(3)?,
                    key_path: r.get(4)?,
                })
            },
        )
        .map_err(|e| e.to_string())
    }

    /// Walk up the group hierarchy to find the nearest `default_identity_id`.
    pub fn resolve_group_identity(&self, group_id: &str) -> Option<String> {
        let conn = self.0.lock().unwrap();
        let mut cursor = Some(group_id.to_string());
        while let Some(gid) = cursor {
            let row: Option<(Option<String>, Option<String>)> = conn
                .query_row(
                    "SELECT parent_id, default_identity_id FROM groups WHERE id = ?1",
                    params![gid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .ok()
                .flatten();
            match row {
                Some((_parent, Some(ident))) if !ident.is_empty() => return Some(ident),
                Some((parent, _)) => cursor = parent,
                None => break,
            }
        }
        None
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub id: String,
    pub name: String,
    pub username: String,
    pub auth_method: String,
    pub key_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedSession {
    pub id: String,
    pub group_id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_method: String,
    pub key_path: Option<String>,
    pub protocol: String,
    pub domain: Option<String>,
    pub rdp_resolution: Option<String>,
    pub identity_id: Option<String>,
    pub jump_session_id: Option<String>,
    pub legacy_crypto: bool,
    pub serial_port: Option<String>,
    pub baud_rate: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupNode {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub default_identity_id: Option<String>,
    pub sessions: Vec<SavedSession>,
    pub children: Vec<GroupNode>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    pub id: String,
    pub name: String,
    pub command: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInput {
    pub id: Option<String>,
    pub group_id: String,
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    pub auth_method: String,
    pub key_path: Option<String>,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    pub domain: Option<String>,
    pub rdp_resolution: Option<String>,
    pub identity_id: Option<String>,
    pub jump_session_id: Option<String>,
    #[serde(default)]
    pub legacy_crypto: bool,
    pub serial_port: Option<String>,
    #[serde(default = "default_baud")]
    pub baud_rate: u32,
    /// Only sent when the user typed a (new) password; stored in keychain.
    pub password: Option<String>,
    pub key_passphrase: Option<String>,
}

fn default_protocol() -> String {
    "ssh".into()
}

fn default_port() -> u16 {
    22
}

fn default_baud() -> u32 {
    115200
}

fn row_to_session(r: &rusqlite::Row) -> rusqlite::Result<SavedSession> {
    let legacy: i64 = r.get(13).unwrap_or(0);
    let baud: i64 = r.get(15).unwrap_or(115200);
    Ok(SavedSession {
        id: r.get(0)?,
        group_id: r.get(1)?,
        name: r.get(2)?,
        host: r.get(3)?,
        port: r.get(4)?,
        username: r.get(5)?,
        auth_method: r.get(6)?,
        key_path: r.get(7)?,
        protocol: r.get(8)?,
        domain: r.get(9)?,
        rdp_resolution: r.get(10)?,
        identity_id: r.get(11)?,
        jump_session_id: r.get(12)?,
        legacy_crypto: legacy != 0,
        serial_port: r.get(14)?,
        baud_rate: baud as u32,
    })
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_tree(store: State<'_, Store>) -> Result<Vec<GroupNode>, String> {
    let conn = store.0.lock().unwrap();

    let mut gstmt = conn
        .prepare(
            "SELECT id, name, parent_id, default_identity_id FROM groups
             ORDER BY position, name COLLATE NOCASE",
        )
        .map_err(|e| e.to_string())?;
    let groups: Vec<(String, String, Option<String>, Option<String>)> = gstmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;

    let mut sstmt = conn
        .prepare(
            "SELECT id, group_id, name, host, port, username, auth_method, key_path,
                    protocol, domain, rdp_resolution, identity_id, jump_session_id,
                    legacy_crypto, serial_port, baud_rate
             FROM saved_sessions ORDER BY position, name COLLATE NOCASE",
        )
        .map_err(|e| e.to_string())?;
    let sessions: Vec<SavedSession> = sstmt
        .query_map([], row_to_session)
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;

    // Build one node per group, then wire children under parents.
    fn build(
        parent: Option<&str>,
        groups: &[(String, String, Option<String>, Option<String>)],
        sessions: &[SavedSession],
    ) -> Vec<GroupNode> {
        groups
            .iter()
            .filter(|(_, _, pid, _)| pid.as_deref() == parent)
            .map(|(id, name, pid, def_ident)| GroupNode {
                sessions: sessions
                    .iter()
                    .filter(|s| &s.group_id == id)
                    .cloned()
                    .collect(),
                children: build(Some(id), groups, sessions),
                id: id.clone(),
                name: name.clone(),
                parent_id: pid.clone(),
                default_identity_id: def_ident.clone(),
            })
            .collect()
    }

    Ok(build(None, &groups, &sessions))
}

#[tauri::command]
pub fn create_group(
    store: State<'_, Store>,
    name: String,
    parent_id: Option<String>,
) -> Result<String, String> {
    let id = Uuid::new_v4().to_string();
    store
        .0
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO groups (id, name, parent_id) VALUES (?1, ?2, ?3)",
            params![id, name, parent_id],
        )
        .map_err(|e| e.to_string())?;
    Ok(id)
}

/// Re-parent a group. Rejects cycles (a group cannot become its own ancestor).
#[tauri::command]
pub fn move_group(
    store: State<'_, Store>,
    id: String,
    parent_id: Option<String>,
) -> Result<(), String> {
    if let Some(target) = &parent_id {
        if target == &id {
            return Err("a group cannot be nested inside itself".into());
        }
        // Walk up from the target; if we reach `id`, this would form a cycle.
        let conn = store.0.lock().unwrap();
        let mut cursor = Some(target.clone());
        while let Some(cur) = cursor {
            if cur == id {
                return Err("cannot move a group into one of its own subgroups".into());
            }
            cursor = conn
                .query_row(
                    "SELECT parent_id FROM groups WHERE id = ?1",
                    params![cur],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(|e| e.to_string())?
                .flatten();
        }
    }
    store
        .0
        .lock()
        .unwrap()
        .execute(
            "UPDATE groups SET parent_id = ?2 WHERE id = ?1",
            params![id, parent_id],
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn rename_group(store: State<'_, Store>, id: String, name: String) -> Result<(), String> {
    store
        .0
        .lock()
        .unwrap()
        .execute("UPDATE groups SET name = ?2 WHERE id = ?1", params![id, name])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn set_group_identity(
    store: State<'_, Store>,
    id: String,
    identity_id: Option<String>,
) -> Result<(), String> {
    let val = identity_id.filter(|s| !s.is_empty());
    store
        .0
        .lock()
        .unwrap()
        .execute(
            "UPDATE groups SET default_identity_id = ?2 WHERE id = ?1",
            params![id, val],
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn delete_group(store: State<'_, Store>, id: String) -> Result<(), String> {
    let conn = store.0.lock().unwrap();
    // Purge keychain entries for every session in the group first.
    let mut stmt = conn
        .prepare("SELECT id FROM saved_sessions WHERE group_id = ?1")
        .map_err(|e| e.to_string())?;
    let ids: Vec<String> = stmt
        .query_map(params![id], |r| r.get(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    drop(stmt);
    for sid in &ids {
        secrets::purge(sid);
    }
    conn.execute("DELETE FROM groups WHERE id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn save_session(
    store: State<'_, Store>,
    input: SessionInput,
) -> Result<SavedSession, String> {
    let id = input.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let identity_id = input.identity_id.clone().filter(|s| !s.is_empty());
    let jump_session_id = input.jump_session_id.clone().filter(|s| !s.is_empty());
    {
        let conn = store.0.lock().unwrap();
        conn.execute(
            "INSERT INTO saved_sessions
               (id, group_id, name, host, port, username, auth_method, key_path,
                protocol, domain, rdp_resolution, identity_id, jump_session_id,
                legacy_crypto, serial_port, baud_rate)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(id) DO UPDATE SET
               group_id = excluded.group_id,
               name = excluded.name,
               host = excluded.host,
               port = excluded.port,
               username = excluded.username,
               auth_method = excluded.auth_method,
               key_path = excluded.key_path,
               protocol = excluded.protocol,
               domain = excluded.domain,
               rdp_resolution = excluded.rdp_resolution,
               identity_id = excluded.identity_id,
               jump_session_id = excluded.jump_session_id,
               legacy_crypto = excluded.legacy_crypto,
               serial_port = excluded.serial_port,
               baud_rate = excluded.baud_rate",
            params![
                id,
                input.group_id,
                input.name,
                input.host,
                input.port,
                input.username,
                input.auth_method,
                input.key_path,
                input.protocol,
                input.domain,
                input.rdp_resolution,
                identity_id,
                jump_session_id,
                if input.legacy_crypto { 1i64 } else { 0i64 },
                input.serial_port,
                input.baud_rate as i64
            ],
        )
        .map_err(|e| e.to_string())?;
    }

    // Keychain bookkeeping: store what was typed, drop what no longer applies.
    if input.auth_method == "identity" || input.protocol == "serial" {
        secrets::purge(&id);
    } else if input.auth_method == "password" {
        if let Some(p) = input.password.as_deref() {
            if !p.is_empty() {
                secrets::set(&secrets::password_account(&id), p)?;
            }
        }
        secrets::delete(&secrets::passphrase_account(&id));
    } else {
        if let Some(pp) = input.key_passphrase.as_deref() {
            if !pp.is_empty() {
                secrets::set(&secrets::passphrase_account(&id), pp)?;
            }
        }
        secrets::delete(&secrets::password_account(&id));
    }

    store.get_session(&id)
}

#[tauri::command]
pub fn delete_session(store: State<'_, Store>, id: String) -> Result<(), String> {
    store
        .0
        .lock()
        .unwrap()
        .execute("DELETE FROM saved_sessions WHERE id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    secrets::purge(&id);
    Ok(())
}

/// Move a saved session into another group.
#[tauri::command]
pub fn move_session(
    store: State<'_, Store>,
    id: String,
    group_id: String,
) -> Result<(), String> {
    store
        .0
        .lock()
        .unwrap()
        .execute(
            "UPDATE saved_sessions SET group_id = ?2 WHERE id = ?1",
            params![id, group_id],
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Duplicate a saved session (new id, "(copy)" suffix, credentials copied in
/// the keychain). Returns the new session so the UI can open it for editing.
#[tauri::command]
pub fn duplicate_session(
    store: State<'_, Store>,
    id: String,
) -> Result<SavedSession, String> {
    let src = store.get_session(&id)?;
    let new_id = Uuid::new_v4().to_string();
    {
        let conn = store.0.lock().unwrap();
        conn.execute(
            "INSERT INTO saved_sessions
               (id, group_id, name, host, port, username, auth_method, key_path,
                protocol, domain, rdp_resolution, identity_id, jump_session_id,
                legacy_crypto, serial_port, baud_rate)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                new_id,
                src.group_id,
                format!("{} (copy)", src.name),
                src.host,
                src.port,
                src.username,
                src.auth_method,
                src.key_path,
                src.protocol,
                src.domain,
                src.rdp_resolution,
                src.identity_id,
                src.jump_session_id,
                if src.legacy_crypto { 1i64 } else { 0i64 },
                src.serial_port,
                src.baud_rate as i64
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    // Copy any stored secrets to the new id so the clone works out of the box.
    if let Some(pw) = secrets::get(&secrets::password_account(&id)) {
        secrets::set(&secrets::password_account(&new_id), &pw)?;
    }
    if let Some(pp) = secrets::get(&secrets::passphrase_account(&id)) {
        secrets::set(&secrets::passphrase_account(&new_id), &pp)?;
    }
    store.get_session(&new_id)
}


// ---------------------------------------------------------------------------
// Shared Identities (Credential Vault)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityInput {
    pub id: Option<String>,
    pub name: String,
    pub username: String,
    pub auth_method: String,
    pub key_path: Option<String>,
    pub password: Option<String>,
    pub key_passphrase: Option<String>,
}

#[tauri::command]
pub fn list_identities(store: State<'_, Store>) -> Result<Vec<Identity>, String> {
    let conn = store.0.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT id, name, username, auth_method, key_path
             FROM identities ORDER BY position, name COLLATE NOCASE",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Identity {
                id: r.get(0)?,
                name: r.get(1)?,
                username: r.get(2)?,
                auth_method: r.get(3)?,
                key_path: r.get(4)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

#[tauri::command]
pub fn save_identity(store: State<'_, Store>, input: IdentityInput) -> Result<Identity, String> {
    let id = input.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    {
        let conn = store.0.lock().unwrap();
        conn.execute(
            "INSERT INTO identities (id, name, username, auth_method, key_path)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
               name = excluded.name,
               username = excluded.username,
               auth_method = excluded.auth_method,
               key_path = excluded.key_path",
            params![id, input.name, input.username, input.auth_method, input.key_path],
        )
        .map_err(|e| e.to_string())?;
    }

    if input.auth_method == "password" {
        if let Some(p) = input.password.as_deref() {
            if !p.is_empty() {
                secrets::set(&secrets::identity_password_account(&id), p)?;
            }
        }
        secrets::delete(&secrets::identity_passphrase_account(&id));
    } else {
        if let Some(pp) = input.key_passphrase.as_deref() {
            if !pp.is_empty() {
                secrets::set(&secrets::identity_passphrase_account(&id), pp)?;
            }
        }
        secrets::delete(&secrets::identity_password_account(&id));
    }

    store.get_identity(&id)
}

#[tauri::command]
pub fn delete_identity(store: State<'_, Store>, id: String) -> Result<(), String> {
    {
        let conn = store.0.lock().unwrap();
        conn.execute(
            "UPDATE saved_sessions SET identity_id = NULL WHERE identity_id = ?1",
            params![id],
        )
        .ok();
        conn.execute(
            "UPDATE groups SET default_identity_id = NULL WHERE default_identity_id = ?1",
            params![id],
        )
        .ok();
        conn.execute("DELETE FROM identities WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
    }
    secrets::purge_identity(&id);
    Ok(())
}


// ---------------------------------------------------------------------------
// Snippets
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetInput {
    pub id: Option<String>,
    pub name: String,
    pub command: String,
}

#[tauri::command]
pub fn list_snippets(store: State<'_, Store>) -> Result<Vec<Snippet>, String> {
    let conn = store.0.lock().unwrap();
    let mut stmt = conn
        .prepare("SELECT id, name, command FROM snippets ORDER BY position, name COLLATE NOCASE")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Snippet {
                id: r.get(0)?,
                name: r.get(1)?,
                command: r.get(2)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

#[tauri::command]
pub fn save_snippet(store: State<'_, Store>, input: SnippetInput) -> Result<Snippet, String> {
    let conn = store.0.lock().unwrap();
    let id = input.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    if input.id.is_some() {
        conn.execute(
            "UPDATE snippets SET name = ?2, command = ?3 WHERE id = ?1",
            params![id, input.name, input.command],
        )
        .map_err(|e| e.to_string())?;
    } else {
        let next: i64 = conn
            .query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM snippets", [], |r| r.get(0))
            .unwrap_or(0);
        conn.execute(
            "INSERT INTO snippets (id, name, command, position) VALUES (?1, ?2, ?3, ?4)",
            params![id, input.name, input.command, next],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(Snippet {
        id,
        name: input.name,
        command: input.command,
    })
}

#[tauri::command]
pub fn delete_snippet(store: State<'_, Store>, id: String) -> Result<(), String> {
    store
        .0
        .lock()
        .unwrap()
        .execute("DELETE FROM snippets WHERE id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Parse `~/.ssh/config` and import any discovered Host entries into an
/// "SSH Config" group. Returns the number of newly imported sessions.
#[tauri::command]
pub fn import_ssh_config(store: State<'_, Store>) -> Result<usize, String> {
    let home = dirs::home_dir().ok_or_else(|| "Could not find home directory".to_string())?;
    let cfg_path = home.join(".ssh").join("config");
    if !cfg_path.exists() {
        return Err("~/.ssh/config does not exist on this Mac".into());
    }
    let content = std::fs::read_to_string(&cfg_path).map_err(|e| e.to_string())?;
    let default_user = std::env::var("USER").unwrap_or_else(|_| "admin".into());

    #[derive(Default)]
    struct Entry {
        alias: String,
        hostname: Option<String>,
        user: Option<String>,
        port: Option<u16>,
        identity_file: Option<String>,
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut cur: Option<Entry> = None;

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, |c: char| c.is_whitespace() || c == '=');
        let key = parts.next().unwrap_or("").trim().to_lowercase();
        let val = parts
            .next()
            .unwrap_or("")
            .trim()
            .trim_start_matches('=')
            .trim()
            .trim_matches('"');
        if val.is_empty() {
            continue;
        }
        if key == "host" {
            if let Some(prev) = cur.take() {
                if !prev.alias.contains('*') && !prev.alias.contains('?') {
                    entries.push(prev);
                }
            }
            let first_alias = val.split_whitespace().next().unwrap_or("").to_string();
            cur = Some(Entry {
                alias: first_alias,
                ..Entry::default()
            });
        } else if let Some(ref mut e) = cur {
            match key.as_str() {
                "hostname" => e.hostname = Some(val.to_string()),
                "user" => e.user = Some(val.to_string()),
                "port" => e.port = val.parse::<u16>().ok(),
                "identityfile" => e.identity_file = Some(val.to_string()),
                _ => {}
            }
        }
    }
    if let Some(prev) = cur.take() {
        if !prev.alias.contains('*') && !prev.alias.contains('?') {
            entries.push(prev);
        }
    }

    if entries.is_empty() {
        return Ok(0);
    }

    let conn = store.0.lock().unwrap();
    let group_id: String = match conn
        .query_row(
            "SELECT id FROM groups WHERE name = 'SSH Config' AND parent_id IS NULL LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
    {
        Some(gid) => gid,
        None => {
            let gid = Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO groups (id, name, parent_id) VALUES (?1, 'SSH Config', NULL)",
                params![gid],
            )
            .map_err(|e| e.to_string())?;
            gid
        }
    };

    let mut imported = 0usize;
    for e in entries {
        let host = e.hostname.unwrap_or_else(|| e.alias.clone());
        let port = e.port.unwrap_or(22);
        let username = e.user.unwrap_or_else(|| default_user.clone());
        let (auth_method, key_path) = match e.identity_file {
            Some(kp) => ("key", Some(kp)),
            None => ("key", Some("~/.ssh/id_ed25519".to_string())),
        };
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM saved_sessions WHERE group_id = ?1 AND name = ?2 LIMIT 1",
                params![group_id, e.alias],
                |_| Ok(true),
            )
            .optional()
            .map_err(|err| err.to_string())?
            .unwrap_or(false);
        if !exists {
            conn.execute(
                "INSERT INTO saved_sessions
                   (id, group_id, name, host, port, username, auth_method, key_path, protocol)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'ssh')",
                params![
                    Uuid::new_v4().to_string(),
                    group_id,
                    e.alias,
                    host,
                    port,
                    username,
                    auth_method,
                    key_path
                ],
            )
            .map_err(|err| err.to_string())?;
            imported += 1;
        }
    }

    Ok(imported)
}

// ---------------------------------------------------------------------------
// Expect / Auto-Login Triggers (Send / Expect Macros)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectRule {
    pub id: String,
    pub name: String,
    pub expect_pattern: String,
    pub send_text: String,
    pub enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectRuleInput {
    pub id: Option<String>,
    pub name: String,
    pub expect_pattern: String,
    pub send_text: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[tauri::command]
pub fn list_expect_rules(store: State<'_, Store>) -> Result<Vec<ExpectRule>, String> {
    let conn = store.0.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT id, name, expect_pattern, send_text, enabled
             FROM expect_rules ORDER BY position, name COLLATE NOCASE",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            let en: i64 = r.get(4)?;
            Ok(ExpectRule {
                id: r.get(0)?,
                name: r.get(1)?,
                expect_pattern: r.get(2)?,
                send_text: r.get(3)?,
                enabled: en != 0,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

#[tauri::command]
pub fn save_expect_rule(
    store: State<'_, Store>,
    input: ExpectRuleInput,
) -> Result<ExpectRule, String> {
    let conn = store.0.lock().unwrap();
    let id = input.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let en = if input.enabled { 1i64 } else { 0i64 };
    if input.id.is_some() {
        conn.execute(
            "UPDATE expect_rules SET name = ?2, expect_pattern = ?3, send_text = ?4, enabled = ?5 WHERE id = ?1",
            params![id, input.name, input.expect_pattern, input.send_text, en],
        )
        .map_err(|e| e.to_string())?;
    } else {
        let next: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(position), -1) + 1 FROM expect_rules",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        conn.execute(
            "INSERT INTO expect_rules (id, name, expect_pattern, send_text, enabled, position)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, input.name, input.expect_pattern, input.send_text, en, next],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(ExpectRule {
        id,
        name: input.name,
        expect_pattern: input.expect_pattern,
        send_text: input.send_text,
        enabled: input.enabled,
    })
}

#[tauri::command]
pub fn toggle_expect_rule(
    store: State<'_, Store>,
    id: String,
    enabled: bool,
) -> Result<(), String> {
    store
        .0
        .lock()
        .unwrap()
        .execute(
            "UPDATE expect_rules SET enabled = ?2 WHERE id = ?1",
            params![id, if enabled { 1i64 } else { 0i64 }],
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn delete_expect_rule(store: State<'_, Store>, id: String) -> Result<(), String> {
    store
        .0
        .lock()
        .unwrap()
        .execute("DELETE FROM expect_rules WHERE id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Universal Session Importer (CSV, MobaXterm .mxtsessions, SecureCRT XML)
// ---------------------------------------------------------------------------

struct ImportedHost {
    group_name: String,
    name: String,
    host: String,
    port: u16,
    username: String,
    protocol: String,
}

fn ensure_group(conn: &Connection, name: &str) -> Result<String, String> {
    let trimmed = if name.trim().is_empty() {
        "Imported"
    } else {
        name.trim()
    };
    if let Some(gid) = conn
        .query_row(
            "SELECT id FROM groups WHERE name = ?1 AND parent_id IS NULL LIMIT 1",
            params![trimmed],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
    {
        return Ok(gid);
    }
    let gid = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO groups (id, name, parent_id) VALUES (?1, ?2, NULL)",
        params![gid, trimmed],
    )
    .map_err(|e| e.to_string())?;
    Ok(gid)
}

fn parse_mobaxterm(content: &str, default_user: &str) -> Vec<ImportedHost> {
    let mut out = Vec::new();
    let mut current_group = String::from("MobaXterm");
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('[') {
            continue;
        }
        if let Some(sub) = line.strip_prefix("SubRep=") {
            let folder = sub.trim().replace('\\', " / ");
            current_group = if folder.is_empty() {
                "MobaXterm".into()
            } else {
                folder
            };
            continue;
        }
        if line.starts_with("ImgNum=") {
            continue;
        }
        if let Some((name, rhs)) = line.split_once('=') {
            if !rhs.contains('%') {
                continue;
            }
            let parts: Vec<&str> = rhs.split('%').collect();
            if parts.len() >= 3 {
                let header = parts[0]; // e.g. #109#0 (0=SSH, 4=RDP, etc.)
                let host = parts[1].trim();
                if host.is_empty() {
                    continue;
                }
                let is_rdp = header.ends_with("#4") || header.contains("#91#4");
                let protocol = if is_rdp { "rdp" } else { "ssh" };
                let default_p = if is_rdp { 3389 } else { 22 };
                let port = parts
                    .get(2)
                    .and_then(|s| s.trim().parse::<u16>().ok())
                    .unwrap_or(default_p);
                let username = parts
                    .get(3)
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(default_user);
                out.push(ImportedHost {
                    group_name: current_group.clone(),
                    name: name.trim().to_string(),
                    host: host.to_string(),
                    port,
                    username: username.to_string(),
                    protocol: protocol.to_string(),
                });
            }
        }
    }
    out
}

fn parse_securecrt_xml(content: &str, default_user: &str) -> Vec<ImportedHost> {
    let mut out = Vec::new();
    let mut key_stack: Vec<String> = Vec::new();
    let mut cur_host = String::new();
    let mut cur_user = String::new();
    let mut cur_port: u16 = 22;
    let mut cur_proto = String::from("ssh");

    for raw in content.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("<key name=\"") {
            if let Some(end) = rest.find('"') {
                let kname = &rest[..end];
                if !line.ends_with("/>") {
                    key_stack.push(kname.to_string());
                    cur_host.clear();
                    cur_user.clear();
                    cur_port = 22;
                    cur_proto = "ssh".into();
                }
            }
        } else if line.starts_with("</key>") {
            if let Some(session_name) = key_stack.pop() {
                if !cur_host.is_empty() {
                    let group = key_stack
                        .iter()
                        .rev()
                        .find(|k| *k != "Sessions" && *k != "VanDyke")
                        .cloned()
                        .unwrap_or_else(|| "SecureCRT".into());
                    out.push(ImportedHost {
                        group_name: group,
                        name: session_name,
                        host: cur_host.clone(),
                        port: cur_port,
                        username: if cur_user.is_empty() {
                            default_user.to_string()
                        } else {
                            cur_user.clone()
                        },
                        protocol: cur_proto.clone(),
                    });
                    cur_host.clear();
                }
            }
        } else if line.contains("name=\"Hostname\"") {
            if let (Some(gt), Some(lt)) = (line.find('>'), line.rfind("</")) {
                if gt + 1 < lt {
                    cur_host = line[gt + 1..lt].trim().to_string();
                }
            }
        } else if line.contains("name=\"Username\"") {
            if let (Some(gt), Some(lt)) = (line.find('>'), line.rfind("</")) {
                if gt + 1 < lt {
                    cur_user = line[gt + 1..lt].trim().to_string();
                }
            }
        } else if line.contains("name=\"[SSH2] Port\"") || line.contains("name=\"Port\"") {
            if let (Some(gt), Some(lt)) = (line.find('>'), line.rfind("</")) {
                if gt + 1 < lt {
                    if let Ok(p) = line[gt + 1..lt].trim().parse::<u16>() {
                        cur_port = p;
                    }
                }
            }
        } else if line.contains("name=\"Protocol Name\"") && line.contains("RDP") {
            cur_proto = "rdp".into();
        }
    }
    out
}

fn parse_csv_sessions(content: &str, default_user: &str) -> Vec<ImportedHost> {
    let mut out = Vec::new();
    for (idx, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(|s| s.trim().trim_matches('"')).collect();
        if idx == 0
            && (cols[0].eq_ignore_ascii_case("name") || cols[0].eq_ignore_ascii_case("host"))
        {
            continue;
        }
        if cols.len() == 1 {
            // Single host or user@host:port per line
            let item = cols[0];
            let (user, rest) = item.split_once('@').unwrap_or((default_user, item));
            let (host, port_str) = rest.split_once(':').unwrap_or((rest, "22"));
            let port = port_str.parse::<u16>().unwrap_or(22);
            if !host.is_empty() {
                out.push(ImportedHost {
                    group_name: "Imported".into(),
                    name: host.to_string(),
                    host: host.to_string(),
                    port,
                    username: user.to_string(),
                    protocol: "ssh".into(),
                });
            }
            continue;
        }
        let name = cols.first().copied().unwrap_or("");
        let host = cols.get(1).copied().unwrap_or("");
        if host.is_empty() {
            continue;
        }
        let port = cols
            .get(2)
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(22);
        let username = cols
            .get(3)
            .copied()
            .filter(|u| !u.is_empty())
            .unwrap_or(default_user);
        let protocol = cols
            .get(4)
            .copied()
            .filter(|p| !p.is_empty())
            .unwrap_or("ssh")
            .to_lowercase();
        let group_name = cols
            .get(5)
            .copied()
            .filter(|g| !g.is_empty())
            .unwrap_or("Imported");

        out.push(ImportedHost {
            group_name: group_name.to_string(),
            name: if name.is_empty() {
                host.to_string()
            } else {
                name.to_string()
            },
            host: host.to_string(),
            port,
            username: username.to_string(),
            protocol: if protocol == "rdp" || protocol == "serial" {
                protocol
            } else {
                "ssh".into()
            },
        });
    }
    out
}

#[tauri::command]
pub fn import_sessions_content(
    store: State<'_, Store>,
    format: String,
    content: String,
) -> Result<usize, String> {
    let default_user = std::env::var("USER").unwrap_or_else(|_| "admin".into());
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("No content provided to import".into());
    }

    let fmt = if format == "auto" {
        if trimmed.contains("[Bookmarks") || trimmed.contains("#109#") {
            "mobaxterm"
        } else if trimmed.contains("<VanDyke") || trimmed.contains("<key name=\"") {
            "securecrt"
        } else {
            "csv"
        }
    } else {
        format.as_str()
    };

    let hosts = match fmt {
        "mobaxterm" => parse_mobaxterm(trimmed, &default_user),
        "securecrt" => parse_securecrt_xml(trimmed, &default_user),
        _ => parse_csv_sessions(trimmed, &default_user),
    };

    if hosts.is_empty() {
        return Ok(0);
    }

    let conn = store.0.lock().unwrap();
    let mut imported = 0usize;
    for h in hosts {
        let gid = ensure_group(&conn, &h.group_name)?;
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM saved_sessions WHERE group_id = ?1 AND name = ?2 AND host = ?3 LIMIT 1",
                params![gid, h.name, h.host],
                |_| Ok(true),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .unwrap_or(false);
        if !exists {
            conn.execute(
                "INSERT INTO saved_sessions
                   (id, group_id, name, host, port, username, auth_method, protocol)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'password', ?7)",
                params![
                    Uuid::new_v4().to_string(),
                    gid,
                    h.name,
                    h.host,
                    h.port,
                    h.username,
                    h.protocol
                ],
            )
            .map_err(|e| e.to_string())?;
            imported += 1;
        }
    }

    Ok(imported)
}
