//! SSH sessions backed by russh, with known_hosts verification (TOFU),
//! automatic per-session logging, and keychain-backed saved sessions.

use std::io::Write as IoWrite;
use std::path::PathBuf;
use std::sync::Arc;

use russh::client::{self, AuthResult};
use russh::keys::known_hosts::learn_known_hosts;
use russh::keys::{check_known_hosts, load_secret_key, HashAlg, PrivateKeyWithHashAlg};
use russh::{ChannelMsg, Disconnect};
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::secrets;
use crate::session::{SessionCmd, SessionExit, SessionHandle, SessionManager};
use crate::store::{self, Store};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshConfig {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub key_path: Option<String>,
    pub key_passphrase: Option<String>,
}

fn default_port() -> u16 {
    22
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostKeyEvent {
    host: String,
    port: u16,
    fingerprint: String,
    level: String, // "info" | "danger"
    message: String,
}

struct ClientHandler {
    app: AppHandle,
    host: String,
    port: u16,
}

impl ClientHandler {
    fn notify(&self, level: &str, fingerprint: String, message: String) {
        let _ = self.app.emit(
            "host-key-event",
            HostKeyEvent {
                host: self.host.clone(),
                port: self.port,
                fingerprint,
                level: level.into(),
                message,
            },
        );
    }
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        let fp = key.fingerprint(HashAlg::Sha256).to_string();
        match check_known_hosts(&self.host, self.port, key) {
            Ok(true) => Ok(true),
            Ok(false) => match learn_known_hosts(&self.host, self.port, key) {
                Ok(()) => {
                    self.notify(
                        "info",
                        fp.clone(),
                        format!(
                            "First contact with {}: host key {} added to known_hosts",
                            self.host, fp
                        ),
                    );
                    Ok(true)
                }
                Err(e) => {
                    self.notify(
                        "danger",
                        fp,
                        format!("Could not record host key for {}: {e}", self.host),
                    );
                    Ok(false)
                }
            },
            Err(russh::keys::Error::KeyChanged { line }) => {
                self.notify(
                    "danger",
                    fp,
                    format!(
                        "HOST KEY CHANGED for {} (known_hosts line {line}). \
                         Possible MITM — connection refused. If the device was \
                         legitimately reinstalled, remove that line from ~/.ssh/known_hosts.",
                        self.host
                    ),
                );
                Ok(false)
            }
            Err(e) => {
                self.notify(
                    "danger",
                    fp,
                    format!("known_hosts check failed for {}: {e}", self.host),
                );
                Ok(false)
            }
        }
    }
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

// --- Session logging --------------------------------------------------------

pub fn logs_dir() -> PathBuf {
    store::data_dir().join("logs")
}

fn open_log(label: &str) -> Option<std::fs::File> {
    let now = chrono::Local::now();
    let dir = logs_dir().join(now.format("%Y-%m-%d").to_string());
    std::fs::create_dir_all(&dir).ok()?;
    let safe: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' })
        .collect();
    let path = dir.join(format!("{}_{}.log", safe, now.format("%H%M%S")));
    let mut f = std::fs::File::create(path).ok()?;
    let _ = writeln!(
        f,
        "=== Switchboard Term session log — {} — {} ===",
        label,
        now.format("%Y-%m-%d %H:%M:%S")
    );
    Some(f)
}

// --- Connection -------------------------------------------------------------

pub async fn spawn_ssh(
    app: AppHandle,
    manager: &SessionManager,
    config: SshConfig,
    label: String,
    cols: u16,
    rows: u16,
    on_data: Channel<Vec<u8>>,
) -> Result<String, String> {
    let ssh_config = Arc::new(client::Config::default());
    let handler = ClientHandler {
        app: app.clone(),
        host: config.host.clone(),
        port: config.port,
    };

    let mut handle = client::connect(ssh_config, (config.host.as_str(), config.port), handler)
        .await
        .map_err(|e| format!("connection failed: {e}"))?;

    // --- Authentication -----------------------------------------------------
    let authenticated: bool = if let Some(key_path) = &config.key_path {
        let path = expand_tilde(key_path);
        let key = load_secret_key(&path, config.key_passphrase.as_deref())
            .map_err(|e| format!("could not load key {path}: {e}"))?;
        let rsa_hash = handle
            .best_supported_rsa_hash()
            .await
            .map_err(|e| e.to_string())?
            .flatten();
        let r = handle
            .authenticate_publickey(
                &config.username,
                PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash),
            )
            .await
            .map_err(|e| format!("key auth failed: {e}"))?;
        matches!(r, AuthResult::Success)
    } else if let Some(password) = &config.password {
        // Try plain password first. Many appliances (F5 BIG-IP and similar)
        // don't accept the `password` method and only offer keyboard-
        // interactive, so fall back to that — the same thing the OpenSSH CLI
        // does automatically. We answer every prompt with the password.
        let direct = handle
            .authenticate_password(&config.username, password)
            .await
            .map_err(|e| format!("password auth failed: {e}"))?;
        if matches!(direct, AuthResult::Success) {
            true
        } else {
            use russh::client::KeyboardInteractiveAuthResponse as Kbi;
            let mut resp = handle
                .authenticate_keyboard_interactive_start(config.username.clone(), None)
                .await
                .map_err(|e| format!("keyboard-interactive failed: {e}"))?;
            loop {
                match resp {
                    Kbi::Success => break true,
                    Kbi::Failure { .. } => break false,
                    Kbi::InfoRequest { prompts, .. } => {
                        let answers: Vec<String> =
                            prompts.iter().map(|_| password.clone()).collect();
                        resp = handle
                            .authenticate_keyboard_interactive_respond(answers)
                            .await
                            .map_err(|e| format!("keyboard-interactive failed: {e}"))?;
                    }
                }
            }
        }
    } else {
        return Err("no authentication method provided".into());
    };

    if !authenticated {
        return Err("authentication rejected by server".into());
    }

    // --- Interactive shell --------------------------------------------------
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| e.to_string())?;
    channel
        .request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
        .await
        .map_err(|e| e.to_string())?;
    // Advertise an iTerm2-class identity to the remote so tools like Claude Code
    // emit OSC 8 hyperlinks (full URL in the escape) instead of raw URLs that
    // break across wrapped lines. LC_* is forwarded by default sshd
    // (`AcceptEnv LANG LC_*`); TERM_PROGRAM only arrives if the server also
    // accepts it (`AcceptEnv TERM_PROGRAM TERM_PROGRAM_VERSION`). want_reply is
    // false so a server that rejects an env request doesn't fail the session.
    for (name, value) in [
        ("LC_TERMINAL", "iTerm2"),
        ("LC_TERMINAL_VERSION", "3.5.0"),
        ("TERM_PROGRAM", "iTerm.app"),
        ("TERM_PROGRAM_VERSION", "3.5.0"),
        ("COLORTERM", "truecolor"),
    ] {
        let _ = channel.set_env(false, name, value).await;
    }
    channel
        .request_shell(false)
        .await
        .map_err(|e| e.to_string())?;

    let id = Uuid::new_v4().to_string();
    let (tx, mut rx) = mpsc::unbounded_channel::<SessionCmd>();
    manager.insert(id.clone(), SessionHandle::Async { tx });

    // --- Pump task ----------------------------------------------------------
    let task_id = id.clone();
    tokio::spawn(async move {
        let mut log = open_log(&label);
        let mut reason = String::from("connection closed");
        loop {
            tokio::select! {
                msg = channel.wait() => {
                    match msg {
                        Some(ChannelMsg::Data { data }) => {
                            if let Some(f) = log.as_mut() { let _ = f.write_all(&data); }
                            if on_data.send(data.to_vec()).is_err() { break; }
                        }
                        Some(ChannelMsg::ExtendedData { data, .. }) => {
                            if let Some(f) = log.as_mut() { let _ = f.write_all(&data); }
                            if on_data.send(data.to_vec()).is_err() { break; }
                        }
                        Some(ChannelMsg::ExitStatus { exit_status }) => {
                            reason = format!("exited with status {exit_status}");
                        }
                        Some(ChannelMsg::Close) | Some(ChannelMsg::Eof) | None => break,
                        _ => {}
                    }
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(SessionCmd::Write(bytes)) => {
                            if channel.data(&bytes[..]).await.is_err() { break; }
                        }
                        Some(SessionCmd::Resize { cols, rows }) => {
                            let _ = channel
                                .window_change(cols as u32, rows as u32, 0, 0)
                                .await;
                        }
                        Some(SessionCmd::Close) | None => {
                            reason = "closed by user".into();
                            let _ = channel.eof().await;
                            break;
                        }
                    }
                }
            }
        }

        if let Some(f) = log.as_mut() {
            let _ = writeln!(f, "\n=== session ended: {reason} ===");
        }
        let _ = handle
            .disconnect(Disconnect::ByApplication, "switchboard: bye", "en")
            .await;
        if let Some(state) = app.try_state::<SessionManager>() {
            state.remove(&task_id);
        }
        let _ = app.emit("session-exit", SessionExit { id: task_id, reason });
    });

    Ok(id)
}

/// Connect to a saved session, pulling credentials from the OS keychain.
#[tauri::command]
pub async fn connect_saved_session(
    app: AppHandle,
    manager: State<'_, SessionManager>,
    store: State<'_, Store>,
    id: String,
    cols: u16,
    rows: u16,
    on_data: Channel<Vec<u8>>,
) -> Result<String, String> {
    let saved = store.get_session(&id)?;
    let mut config = SshConfig {
        host: saved.host.clone(),
        port: saved.port,
        username: saved.username.clone(),
        password: None,
        key_path: None,
        key_passphrase: None,
    };
    if saved.auth_method == "key" {
        config.key_path = saved.key_path.clone();
        config.key_passphrase = secrets::get(&secrets::passphrase_account(&id));
    } else {
        config.password = Some(
            secrets::get(&secrets::password_account(&id)).ok_or_else(|| {
                "no password stored in the keychain for this session — \
                 edit it and re-enter the password"
                    .to_string()
            })?,
        );
    }
    spawn_ssh(app, &manager, config, saved.name.clone(), cols, rows, on_data).await
}
