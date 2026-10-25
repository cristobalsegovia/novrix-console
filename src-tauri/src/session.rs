//! Session registry shared by every connector (local PTY, SSH, and later
//! serial / telnet / RDP / VNC). Each live session is identified by a UUID
//! and exposes a uniform way to receive user input, resize, and close.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use portable_pty::{MasterPty, PtySize};
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

/// Commands that can be sent to an async (tokio-driven) session task.
#[derive(Debug)]
pub enum SessionCmd {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

/// Handle to a running session. Local PTYs are driven by blocking threads,
/// so we keep their writer + master directly; async connectors (SSH, and
/// later telnet) are driven by a tokio task we talk to through a channel.
pub enum SessionHandle {
    LocalPty {
        writer: Mutex<Box<dyn Write + Send>>,
        master: Mutex<Box<dyn MasterPty + Send>>,
    },
    Serial {
        port: Mutex<serial2::SerialPort>,
    },
    Async {
        tx: UnboundedSender<SessionCmd>,
    },
}

pub type SharedSshHandle = Arc<tokio::sync::Mutex<russh::client::Handle<crate::ssh::ClientHandler>>>;

#[derive(Default)]
pub struct SessionManager {
    sessions: Mutex<HashMap<String, SessionHandle>>,
    ssh_handles: Mutex<HashMap<String, SharedSshHandle>>,
}

impl SessionManager {
    pub fn insert(&self, id: String, handle: SessionHandle) {
        self.sessions.lock().unwrap().insert(id, handle);
    }

    pub fn insert_ssh(&self, id: String, ssh: SharedSshHandle) {
        self.ssh_handles.lock().unwrap().insert(id, ssh);
    }

    pub fn get_ssh(&self, id: &str) -> Option<SharedSshHandle> {
        self.ssh_handles.lock().unwrap().get(id).cloned()
    }

    pub fn remove(&self, id: &str) -> Option<SessionHandle> {
        self.ssh_handles.lock().unwrap().remove(id);
        self.sessions.lock().unwrap().remove(id)
    }

    pub fn write(&self, id: &str, data: &[u8]) -> Result<(), String> {
        let sessions = self.sessions.lock().unwrap();
        match sessions.get(id) {
            Some(SessionHandle::LocalPty { writer, .. }) => {
                let mut w = writer.lock().unwrap();
                w.write_all(data).map_err(|e| e.to_string())?;
                w.flush().map_err(|e| e.to_string())
            }
            Some(SessionHandle::Serial { port }) => {
                let p = port.lock().unwrap();
                p.write_all(data).map_err(|e| e.to_string())?;
                p.flush().map_err(|e| e.to_string())
            }
            Some(SessionHandle::Async { tx }) => tx
                .send(SessionCmd::Write(data.to_vec()))
                .map_err(|e| e.to_string()),
            None => Err(format!("unknown session {id}")),
        }
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let sessions = self.sessions.lock().unwrap();
        match sessions.get(id) {
            Some(SessionHandle::LocalPty { master, .. }) => master
                .lock()
                .unwrap()
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| e.to_string()),
            Some(SessionHandle::Serial { .. }) => Ok(()),
            Some(SessionHandle::Async { tx }) => tx
                .send(SessionCmd::Resize { cols, rows })
                .map_err(|e| e.to_string()),
            None => Err(format!("unknown session {id}")),
        }
    }

    pub fn close(&self, id: &str) {
        if let Some(handle) = self.remove(id) {
            if let SessionHandle::Async { tx } = handle {
                let _ = tx.send(SessionCmd::Close);
            }
            // LocalPty: dropping writer + master hangs up the PTY and the
            // child shell receives SIGHUP.
        }
    }
}

/// Payload for the global `session-exit` event the frontend listens to.
#[derive(Clone, Serialize)]
pub struct SessionExit {
    pub id: String,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

use tauri::{ipc::Channel, AppHandle, State};

#[tauri::command]
pub fn create_local_session(
    app: AppHandle,
    state: State<'_, SessionManager>,
    cols: u16,
    rows: u16,
    on_data: Channel<Vec<u8>>,
) -> Result<String, String> {
    crate::pty::spawn_local(app, &state, cols, rows, on_data)
}

#[tauri::command]
pub async fn create_ssh_session(
    app: AppHandle,
    state: State<'_, SessionManager>,
    store: State<'_, crate::store::Store>,
    config: crate::ssh::SshConfig,
    cols: u16,
    rows: u16,
    on_data: Channel<Vec<u8>>,
) -> Result<String, String> {
    let label = format!("{}@{}", config.username, config.host);
    crate::ssh::spawn_ssh(app, &state, Some(&store), config, label, cols, rows, on_data).await
}

#[tauri::command]
pub fn write_to_session(
    state: State<'_, SessionManager>,
    id: String,
    data: String,
) -> Result<(), String> {
    state.write(&id, data.as_bytes())
}

#[tauri::command]
pub fn resize_session(
    state: State<'_, SessionManager>,
    id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    state.resize(&id, cols, rows)
}

#[tauri::command]
pub fn close_session(state: State<'_, SessionManager>, id: String) {
    state.close(&id);
}
