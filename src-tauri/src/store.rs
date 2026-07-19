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

        // Migrate older databases that predate the RDP columns. Each ALTER is
        // ignored if the column already exists, so this is safe to re-run.
        for stmt in [
            "ALTER TABLE saved_sessions ADD COLUMN protocol TEXT NOT NULL DEFAULT 'ssh'",
            "ALTER TABLE saved_sessions ADD COLUMN domain TEXT",
            "ALTER TABLE saved_sessions ADD COLUMN rdp_resolution TEXT",
            "ALTER TABLE groups ADD COLUMN parent_id TEXT REFERENCES groups(id) ON DELETE CASCADE",
        ] {
            let _ = conn.execute(stmt, []);
        }

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
        Ok(Store(Mutex::new(conn)))
    }

    pub fn get_session(&self, id: &str) -> Result<SavedSession, String> {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT id, group_id, name, host, port, username, auth_method, key_path,
                    protocol, domain, rdp_resolution
             FROM saved_sessions WHERE id = ?1",
            params![id],
            row_to_session,
        )
        .map_err(|e| e.to_string())
    }
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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupNode {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
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

fn row_to_session(r: &rusqlite::Row) -> rusqlite::Result<SavedSession> {
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
            "SELECT id, name, parent_id FROM groups
             ORDER BY position, name COLLATE NOCASE",
        )
        .map_err(|e| e.to_string())?;
    let groups: Vec<(String, String, Option<String>)> = gstmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;

    let mut sstmt = conn
        .prepare(
            "SELECT id, group_id, name, host, port, username, auth_method, key_path,
                    protocol, domain, rdp_resolution
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
        groups: &[(String, String, Option<String>)],
        sessions: &[SavedSession],
    ) -> Vec<GroupNode> {
        groups
            .iter()
            .filter(|(_, _, pid)| pid.as_deref() == parent)
            .map(|(id, name, pid)| GroupNode {
                sessions: sessions
                    .iter()
                    .filter(|s| &s.group_id == id)
                    .cloned()
                    .collect(),
                children: build(Some(id), groups, sessions),
                id: id.clone(),
                name: name.clone(),
                parent_id: pid.clone(),
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
    {
        let conn = store.0.lock().unwrap();
        conn.execute(
            "INSERT INTO saved_sessions
               (id, group_id, name, host, port, username, auth_method, key_path,
                protocol, domain, rdp_resolution)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
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
               rdp_resolution = excluded.rdp_resolution",
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
                input.rdp_resolution
            ],
        )
        .map_err(|e| e.to_string())?;
    }

    // Keychain bookkeeping: store what was typed, drop what no longer applies.
    if input.auth_method == "password" {
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
                protocol, domain, rdp_resolution)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                src.rdp_resolution
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
