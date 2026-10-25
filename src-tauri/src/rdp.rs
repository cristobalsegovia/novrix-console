//! RDP via an installed FreeRDP client. We shell out to `sdl-freerdp`
//! (preferred on macOS — no X11 needed) or `xfreerdp`, which opens its own
//! native window. The password is passed over stdin with `/from-stdin`,
//! never on the command line (argv is world-readable through `ps`).

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::secrets;
use crate::store::Store;

/// Where Homebrew drops binaries, ahead of a PATH lookup.
const SEARCH_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];
/// Preferred first: sdl-freerdp works natively on macOS; xfreerdp needs X11.
const CANDIDATES: &[&str] = &["sdl-freerdp", "sdl3-freerdp", "xfreerdp", "xfreerdp3"];

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RdpAvailability {
    pub available: bool,
    pub binary: Option<String>,
}

fn find_binary() -> Option<PathBuf> {
    for name in CANDIDATES {
        for dir in SEARCH_DIRS {
            let path = PathBuf::from(dir).join(name);
            if path.exists() {
                return Some(path);
            }
        }
    }
    for name in CANDIDATES {
        if let Ok(output) = Command::new("which").arg(name).output() {
            if output.status.success() {
                let p = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !p.is_empty() {
                    return Some(PathBuf::from(p));
                }
            }
        }
    }
    None
}

#[tauri::command]
pub fn rdp_availability() -> RdpAvailability {
    match find_binary() {
        Some(path) => RdpAvailability {
            available: true,
            binary: Some(path.to_string_lossy().into_owned()),
        },
        None => RdpAvailability {
            available: false,
            binary: None,
        },
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RdpConfig {
    pub host: String,
    #[serde(default = "default_rdp_port")]
    pub port: u16,
    pub username: String,
    pub domain: Option<String>,
    pub password: Option<String>,
    /// "fit" (dynamic resolution) or "WIDTHxHEIGHT" like "1600x900".
    pub resolution: Option<String>,
    #[serde(default = "default_true")]
    pub clipboard: bool,
    #[serde(default)]
    pub multimon: bool,
}

fn default_rdp_port() -> u16 {
    3389
}
fn default_true() -> bool {
    true
}

fn spawn_freerdp(binary: &str, config: &RdpConfig) -> Result<(), String> {
    let mut args: Vec<String> = Vec::new();
    args.push(format!("/v:{}:{}", config.host, config.port));
    args.push(format!("/u:{}", config.username));
    if let Some(d) = config.domain.as_deref().filter(|d| !d.is_empty()) {
        args.push(format!("/d:{d}"));
    }
    // TOFU: trust on first use, then pin — the RDP analogue of known_hosts.
    args.push("/cert:tofu".into());
    args.push("/auto-reconnect".into());

    match config.resolution.as_deref() {
        Some("fit") | None => args.push("/dynamic-resolution".into()),
        Some(res) => args.push(format!("/size:{res}")),
    }
    if config.clipboard {
        args.push("+clipboard".into());
    }
    if config.multimon {
        args.push("/multimon".into());
    }
    // Read the password from stdin instead of argv.
    args.push("/from-stdin".into());

    let mut child = Command::new(binary)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not launch FreeRDP: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let line = format!("{}\n", config.password.as_deref().unwrap_or(""));
        stdin
            .write_all(line.as_bytes())
            .map_err(|e| format!("could not send credentials: {e}"))?;
    }
    // Child runs detached in its own window; we don't wait on it.
    Ok(())
}

#[tauri::command]
pub fn launch_rdp(app: AppHandle, config: RdpConfig) -> Result<(), String> {
    let binary = find_binary()
        .ok_or_else(|| "FreeRDP is not installed. Run: brew install freerdp".to_string())?;
    spawn_freerdp(&binary.to_string_lossy(), &config)?;
    let _ = app.emit("rdp-launched", format!("Launching RDP to {}…", config.host));
    Ok(())
}

/// Launch a saved RDP session, pulling the password from the keychain or shared Identity.
#[tauri::command]
pub fn connect_saved_rdp(
    app: AppHandle,
    store: State<'_, Store>,
    id: String,
) -> Result<(), String> {
    let saved = store.get_session(&id)?;
    if saved.protocol != "rdp" {
        return Err("this saved session is not an RDP session".into());
    }
    let effective_identity = saved
        .identity_id
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if saved.auth_method == "identity" {
                store.resolve_group_identity(&saved.group_id)
            } else {
                None
            }
        });

    let (username, password) = if let Some(ident_id) = effective_identity {
        let ident = store.get_identity(&ident_id)?;
        let user = if !ident.username.is_empty() {
            ident.username
        } else {
            saved.username.clone()
        };
        let pw = secrets::get(&secrets::identity_password_account(&ident_id));
        (user, pw)
    } else {
        (
            saved.username.clone(),
            secrets::get(&secrets::password_account(&id)),
        )
    };

    let config = RdpConfig {
        host: saved.host.clone(),
        port: saved.port,
        username,
        domain: saved.domain.clone(),
        password,
        resolution: saved.rdp_resolution.clone(),
        clipboard: true,
        multimon: false,
    };
    launch_rdp(app, config)
}
