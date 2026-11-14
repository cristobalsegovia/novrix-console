//! SSH sessions backed by russh, with known_hosts verification (TOFU),
//! interactive host-key change recovery, keep-alive, legacy crypto support for
//! older network appliances, bastion/jump-host tunneling, shared identities,
//! and automatic per-session logging.

use std::borrow::Cow;
use std::io::Write as IoWrite;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, AuthResult};
use russh::keys::known_hosts::learn_known_hosts;
use russh::keys::{check_known_hosts, load_secret_key, Algorithm, EcdsaCurve, HashAlg, PrivateKeyWithHashAlg};
use russh::{cipher, kex, mac, ChannelMsg, Disconnect, Preferred};
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
    pub identity_id: Option<String>,
    pub jump_session_id: Option<String>,
    #[serde(default)]
    pub legacy_crypto: bool,
}

fn default_port() -> u16 {
    22
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostKeyEvent {
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
    pub level: String, // "info" | "danger"
    pub message: String,
    pub changed_line: Option<usize>,
}

pub struct ClientHandler {
    pub app: AppHandle,
    pub host: String,
    pub port: u16,
}

impl ClientHandler {
    fn notify(
        &self,
        level: &str,
        fingerprint: String,
        message: String,
        changed_line: Option<usize>,
    ) {
        let _ = self.app.emit(
            "host-key-event",
            HostKeyEvent {
                host: self.host.clone(),
                port: self.port,
                fingerprint,
                level: level.into(),
                message,
                changed_line,
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
                        None,
                    );
                    Ok(true)
                }
                Err(e) => {
                    self.notify(
                        "danger",
                        fp,
                        format!("Could not record host key for {}: {e}", self.host),
                        None,
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
                         Connection refused to prevent MITM. If the device was \
                         reinstalled, click 'Trust New Key' to update known_hosts.",
                        self.host
                    ),
                    Some(line),
                );
                Ok(false)
            }
            Err(e) => {
                self.notify(
                    "danger",
                    fp,
                    format!("known_hosts check failed for {}: {e}", self.host),
                    None,
                );
                Ok(false)
            }
        }
    }
}

/// Remove a changed host key line (or matching host entry) from ~/.ssh/known_hosts
/// so the next connection attempt can learn the new key via TOFU.
#[tauri::command]
pub fn accept_changed_host_key(
    host: String,
    port: u16,
    line: Option<usize>,
) -> Result<(), String> {
    let home = dirs::home_dir().ok_or_else(|| "Could not locate home directory".to_string())?;
    let kh_path = home.join(".ssh").join("known_hosts");
    if !kh_path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&kh_path).map_err(|e| e.to_string())?;
    let bracketed = format!("[{host}]:{port}");
    let mut kept = Vec::new();
    for (idx, raw_line) in content.lines().enumerate() {
        let line_no = idx + 1;
        if let Some(target_line) = line {
            if line_no == target_line {
                continue;
            }
        }
        let first_token = raw_line.split_whitespace().next().unwrap_or("");
        let matches_host = first_token
            .split(',')
            .any(|h| h == host || (port != 22 && h == bracketed));
        if line.is_none() && matches_host {
            continue;
        }
        kept.push(raw_line);
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    std::fs::write(&kh_path, out).map_err(|e| e.to_string())
}

pub fn expand_tilde(path: &str) -> String {
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

pub fn open_log(label: &str) -> Option<std::fs::File> {
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

// --- SSH client configuration (Keep-Alive + Legacy Crypto) ------------------

fn build_ssh_client_config(legacy_crypto: bool) -> Arc<client::Config> {
    let mut cfg = client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..client::Config::default()
    };

    if legacy_crypto {
        cfg.preferred = Preferred {
            kex: Cow::Borrowed(&[
                kex::CURVE25519,
                kex::CURVE25519_PRE_RFC_8731,
                kex::ECDH_SHA2_NISTP256,
                kex::ECDH_SHA2_NISTP384,
                kex::ECDH_SHA2_NISTP521,
                kex::DH_GEX_SHA256,
                kex::DH_G16_SHA512,
                kex::DH_G14_SHA256,
                kex::DH_G14_SHA1,
                kex::DH_GEX_SHA1,
                kex::DH_G1_SHA1,
                kex::EXTENSION_SUPPORT_AS_CLIENT,
                kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT,
            ]),
            key: Cow::Borrowed(&[
                Algorithm::Ed25519,
                Algorithm::Ecdsa {
                    curve: EcdsaCurve::NistP256,
                },
                Algorithm::Ecdsa {
                    curve: EcdsaCurve::NistP384,
                },
                Algorithm::Ecdsa {
                    curve: EcdsaCurve::NistP521,
                },
                Algorithm::Rsa {
                    hash: Some(HashAlg::Sha512),
                },
                Algorithm::Rsa {
                    hash: Some(HashAlg::Sha256),
                },
                Algorithm::Rsa { hash: None },
            ]),
            cipher: Cow::Borrowed(&[
                cipher::CHACHA20_POLY1305,
                cipher::AES_256_GCM,
                cipher::AES_128_GCM,
                cipher::AES_256_CTR,
                cipher::AES_192_CTR,
                cipher::AES_128_CTR,
                cipher::AES_256_CBC,
                cipher::AES_192_CBC,
                cipher::AES_128_CBC,
            ]),
            mac: Cow::Borrowed(&[
                mac::HMAC_SHA512_ETM,
                mac::HMAC_SHA256_ETM,
                mac::HMAC_SHA512,
                mac::HMAC_SHA256,
                mac::HMAC_SHA1_ETM,
                mac::HMAC_SHA1,
            ]),
            ..Preferred::DEFAULT
        };
    }

    Arc::new(cfg)
}

async fn authenticate_handle(
    handle: &mut client::Handle<ClientHandler>,
    config: &SshConfig,
) -> Result<bool, String> {
    if let Some(key_path) = &config.key_path {
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
        Ok(matches!(r, AuthResult::Success))
    } else if let Some(password) = &config.password {
        let direct = handle
            .authenticate_password(&config.username, password)
            .await
            .map_err(|e| format!("password auth failed: {e}"))?;
        if matches!(direct, AuthResult::Success) {
            Ok(true)
        } else {
            use russh::client::KeyboardInteractiveAuthResponse as Kbi;
            let mut resp = handle
                .authenticate_keyboard_interactive_start(config.username.clone(), None)
                .await
                .map_err(|e| format!("keyboard-interactive failed: {e}"))?;
            loop {
                match resp {
                    Kbi::Success => break Ok(true),
                    Kbi::Failure { .. } => break Ok(false),
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
        Err("no authentication method provided".into())
    }
}

pub fn resolve_saved_ssh_config(store: &Store, id: &str) -> Result<(SshConfig, String), String> {
    let saved = store.get_session(id)?;
    let has_own_secret = if saved.auth_method == "key" {
        saved.key_path.as_deref().is_some_and(|s| !s.is_empty())
    } else {
        secrets::get(&secrets::password_account(id)).is_some()
    };

    let effective_identity = saved
        .identity_id
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if saved.auth_method == "identity" || !has_own_secret {
                store.resolve_group_identity(&saved.group_id)
            } else {
                None
            }
        });

    let mut config = SshConfig {
        host: saved.host.clone(),
        port: saved.port,
        username: saved.username.clone(),
        password: None,
        key_path: None,
        key_passphrase: None,
        identity_id: effective_identity.clone(),
        jump_session_id: saved.jump_session_id.clone(),
        legacy_crypto: saved.legacy_crypto,
    };

    if let Some(ident_id) = effective_identity {
        let ident = store.get_identity(&ident_id)?;
        if !ident.username.is_empty() {
            config.username = ident.username;
        }
        if ident.auth_method == "key" {
            config.key_path = ident.key_path;
            config.key_passphrase = secrets::get(&secrets::identity_passphrase_account(&ident_id));
        } else {
            config.password = Some(
                secrets::get(&secrets::identity_password_account(&ident_id)).ok_or_else(|| {
                    format!(
                        "no password stored in the keychain for identity '{}' — edit it in Identities",
                        ident.name
                    )
                })?,
            );
        }
    } else if saved.auth_method == "key" {
        config.key_path = saved.key_path.clone();
        config.key_passphrase = secrets::get(&secrets::passphrase_account(id));
    } else {
        config.password = Some(
            secrets::get(&secrets::password_account(id)).ok_or_else(|| {
                "no password stored in the encrypted vault for this session — \
                 edit the session (or assign a Group Identity in Vault) to save it once"
                    .to_string()
            })?,
        );
    }

    Ok((config, saved.name))
}

pub fn apply_direct_identity(store: &Store, config: &mut SshConfig) -> Result<(), String> {
    if let Some(ident_id) = config.identity_id.clone().filter(|s| !s.is_empty()) {
        let ident = store.get_identity(&ident_id)?;
        if !ident.username.is_empty() {
            config.username = ident.username;
        }
        if ident.auth_method == "key" {
            config.key_path = ident.key_path;
            config.key_passphrase = secrets::get(&secrets::identity_passphrase_account(&ident_id));
        } else {
            config.password = secrets::get(&secrets::identity_password_account(&ident_id));
        }
    }
    Ok(())
}

// --- Connection -------------------------------------------------------------

pub async fn spawn_ssh(
    app: AppHandle,
    manager: &SessionManager,
    store: Option<&Store>,
    mut config: SshConfig,
    label: String,
    cols: u16,
    rows: u16,
    on_data: Channel<Vec<u8>>,
) -> Result<String, String> {
    if let Some(st) = store {
        apply_direct_identity(st, &mut config)?;
    }

    let ssh_config = build_ssh_client_config(config.legacy_crypto);
    let handler = ClientHandler {
        app: app.clone(),
        host: config.host.clone(),
        port: config.port,
    };

    // Optional Bastion / Jump Host support
    let mut _jump_handle_guard: Option<client::Handle<ClientHandler>> = None;
    let mut handle = if let (Some(jump_id), Some(st)) = (
        config.jump_session_id.as_deref().filter(|s| !s.is_empty()),
        store,
    ) {
        let (jump_cfg, _jump_name) = resolve_saved_ssh_config(st, jump_id)?;
        let jump_ssh_config = build_ssh_client_config(jump_cfg.legacy_crypto);
        let jump_handler = ClientHandler {
            app: app.clone(),
            host: jump_cfg.host.clone(),
            port: jump_cfg.port,
        };
        let mut jh = client::connect(
            jump_ssh_config,
            (jump_cfg.host.as_str(), jump_cfg.port),
            jump_handler,
        )
        .await
        .map_err(|e| format!("jump host ({}) connection failed: {e}", jump_cfg.host))?;

        let j_auth = authenticate_handle(&mut jh, &jump_cfg).await?;
        if !j_auth {
            return Err(format!(
                "jump host ({}) authentication rejected",
                jump_cfg.host
            ));
        }
        let direct_channel = jh
            .channel_open_direct_tcpip(&config.host, config.port as u32, "127.0.0.1", 0)
            .await
            .map_err(|e| {
                format!(
                    "jump host ({}) could not tunnel to {}:{}: {e}",
                    jump_cfg.host, config.host, config.port
                )
            })?;
        _jump_handle_guard = Some(jh);
        client::connect_stream(ssh_config, direct_channel.into_stream(), handler)
            .await
            .map_err(|e| format!("connection through jump host failed: {e}"))?
    } else {
        client::connect(ssh_config, (config.host.as_str(), config.port), handler)
            .await
            .map_err(|e| format!("connection failed: {e}"))?
    };

    // --- Authentication -----------------------------------------------------
    let authenticated = authenticate_handle(&mut handle, &config).await?;
    if !authenticated {
        return Err("authentication rejected by server".into());
    }

    // --- Interactive shell --------------------------------------------------
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| e.to_string())?;
    channel
        .request_pty(
            false,
            "xterm-256color",
            cols as u32,
            rows as u32,
            (cols as u32).saturating_mul(8),
            (rows as u32).saturating_mul(16),
            &[],
        )
        .await
        .map_err(|e| e.to_string())?;
    for (name, value) in [
        ("LC_TERMINAL", "iTerm2"),
        ("LC_TERMINAL_VERSION", "3.6.6"),
        ("TERM_PROGRAM", "iTerm.app"),
        ("TERM_PROGRAM_VERSION", "3.6.6"),
        ("COLORTERM", "truecolor"),
        ("LANG", "en_US.UTF-8"),
        ("LC_CTYPE", "UTF-8"),
    ] {
        let _ = channel.set_env(false, name, value).await;
    }
    channel
        .request_shell(false)
        .await
        .map_err(|e| e.to_string())?;

    let id = Uuid::new_v4().to_string();
    let (tx, mut rx) = mpsc::unbounded_channel::<SessionCmd>();
    let shared_handle = Arc::new(tokio::sync::Mutex::new(handle));
    manager.insert(id.clone(), SessionHandle::Async { tx });
    manager.insert_ssh(id.clone(), shared_handle.clone());

    // --- Pump task ----------------------------------------------------------
    let task_id = id.clone();
    tokio::spawn(async move {
        let _keep_jump_alive = _jump_handle_guard;
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
        {
            let h = shared_handle.lock().await;
            let _ = h
                .disconnect(Disconnect::ByApplication, "switchboard: bye", "en")
                .await;
        }
        if let Some(state) = app.try_state::<SessionManager>() {
            state.remove(&task_id);
        }
        if let Some(tm) = app.try_state::<crate::tunnels::TunnelManager>() {
            tm.stop_for_session(&task_id);
        }
        let _ = app.emit("session-exit", SessionExit { id: task_id, reason });
    });

    Ok(id)
}

/// Connect to a saved session, pulling credentials from the OS keychain or shared Identity.
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
    if saved.protocol == "serial" {
        let port = saved
            .serial_port
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| saved.host.clone());
        return crate::serial::spawn_serial(
            app,
            &manager,
            port,
            saved.baud_rate,
            saved.name,
            on_data,
        );
    }
    let (config, name) = resolve_saved_ssh_config(&store, &id)?;
    spawn_ssh(app, &manager, Some(&store), config, name, cols, rows, on_data).await
}
