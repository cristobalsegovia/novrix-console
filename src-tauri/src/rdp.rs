//! High-Definition RDP client integration for macOS.
//! Supports FreeRDP (`sdl-freerdp` / `sdl3-freerdp` / `xfreerdp`) with 32-bit TrueColor,
//! ClearType font smoothing (`+fonts`), RemoteFX / H.264 GFX acceleration, Retina HiDPI scaling,
//! macOS audio redirection, and automatic RDP-over-SSH bastion tunneling, plus automatic
//! fallback to macOS Windows App / Microsoft Remote Desktop via `.rdp` session profiles.

use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::secrets;
use crate::store::Store;

const SEARCH_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];
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

fn find_macos_rdp_app() -> Option<PathBuf> {
    for app_name in &[
        "/Applications/Windows App.app",
        "/Applications/Microsoft Remote Desktop.app",
        "/Applications/Royal TSX.app",
    ] {
        let p = PathBuf::from(app_name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

#[tauri::command]
pub fn rdp_availability() -> RdpAvailability {
    if let Some(path) = find_binary() {
        return RdpAvailability {
            available: true,
            binary: Some(path.to_string_lossy().into_owned()),
        };
    }
    if let Some(app) = find_macos_rdp_app() {
        return RdpAvailability {
            available: true,
            binary: Some(app.to_string_lossy().into_owned()),
        };
    }
    RdpAvailability {
        available: false,
        binary: None,
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
    /// "fit" | "retina" | "fullscreen" | "2560x1440" | "1920x1080" | "1920x1200" | "1600x900"
    pub resolution: Option<String>,
    /// "ultra" (32-bit + ClearType + GFX) | "balanced" (24-bit + ClearType) | "wan" (16-bit compressed)
    pub quality: Option<String>,
    #[serde(default = "default_true")]
    pub clipboard: bool,
    #[serde(default = "default_true")]
    pub audio: bool,
    #[serde(default)]
    pub multimon: bool,
    pub jump_session_id: Option<String>,
}

fn default_rdp_port() -> u16 {
    3389
}
fn default_true() -> bool {
    true
}

fn maybe_open_ssh_tunnel(
    store: &Store,
    jump_id: &str,
    target_host: &str,
    target_port: u16,
) -> Result<(String, u16), String> {
    let jump = store.get_session(jump_id)?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("could not allocate local port for RDP SSH tunnel: {e}"))?;
    let local_port = listener.local_addr().map_err(|e| e.to_string())?.port();
    drop(listener);

    let mut args = vec![
        "-f".to_string(),
        "-N".to_string(),
        "-o".to_string(),
        "ExitOnForwardFailure=yes".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-p".to_string(),
        jump.port.to_string(),
        "-L".to_string(),
        format!("127.0.0.1:{local_port}:{target_host}:{target_port}"),
    ];
    if jump.auth_method == "key" {
        if let Some(kp) = jump.key_path.as_deref().filter(|s| !s.is_empty()) {
            let expanded = if let Some(rest) = kp.strip_prefix("~/") {
                if let Ok(home) = std::env::var("HOME") {
                    format!("{home}/{rest}")
                } else {
                    kp.to_string()
                }
            } else {
                kp.to_string()
            };
            args.push("-i".to_string());
            args.push(expanded);
        }
    }
    args.push(format!("{}@{}", jump.username, jump.host));

    let status = Command::new("/usr/bin/ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("failed to start SSH tunnel for RDP: {e}"))?;

    if !status.success() {
        return Err(format!(
            "SSH tunnel through {}@{} failed (verify SSH key authentication on jump host)",
            jump.username, jump.host
        ));
    }

    Ok(("127.0.0.1".to_string(), local_port))
}

fn spawn_freerdp(binary: &str, host: &str, port: u16, config: &RdpConfig) -> Result<(), String> {
    let mut args: Vec<String> = Vec::new();
    args.push(format!("/v:{host}:{port}"));
    args.push(format!("/u:{}", config.username));
    if let Some(d) = config
        .domain
        .as_deref()
        .filter(|d| !d.is_empty() && !["ultra", "balanced", "wan"].contains(d))
    {
        args.push(format!("/d:{d}"));
    }
    args.push(format!("/title:Novrix RDP — {}", config.host));
    args.push("/cert:tofu".into());
    args.push("/auto-reconnect".into());
    args.push("/auto-reconnect-max-retries:10".into());

    // Quality profile: ClearType font smoothing (+fonts), color depth, and GFX pipeline
    let quality = config
        .quality
        .as_deref()
        .or_else(|| {
            config
                .domain
                .as_deref()
                .filter(|d| ["ultra", "balanced", "wan"].contains(d))
        })
        .unwrap_or("ultra");

    match quality {
        "wan" => {
            args.push("/bpp:16".into());
            args.push("+fonts".into());
            args.push("/compression-level:2".into());
            args.push("/network:modem".into());
        }
        "balanced" => {
            args.push("/bpp:24".into());
            args.push("+fonts".into());
            args.push("+window-drag".into());
            args.push("/gfx".into());
            args.push("/network:broadband".into());
        }
        _ => {
            // Ultra (default): 32-bit TrueColor + ClearType + Aero + RemoteFX/GFX
            args.push("/bpp:32".into());
            args.push("+fonts".into());
            args.push("+aero".into());
            args.push("+window-drag".into());
            args.push("+menu-anims".into());
            args.push("/gfx".into());
            args.push("/rfx".into());
            args.push("/network:auto".into());
        }
    }

    // Resolution & Retina scaling
    match config.resolution.as_deref() {
        Some("retina") => {
            args.push("/dynamic-resolution".into());
            args.push("/scale:180".into());
            args.push("/scale-desktop:180".into());
        }
        Some("fullscreen") => {
            args.push("/f".into());
            args.push("/dynamic-resolution".into());
        }
        Some("fit") | None => {
            args.push("/dynamic-resolution".into());
        }
        Some(res) => {
            args.push(format!("/size:{res}"));
            args.push("/dynamic-resolution".into());
        }
    }

    if config.clipboard {
        args.push("+clipboard".into());
    }
    if config.audio {
        args.push("/sound:sys:mac".into());
    }
    if config.multimon {
        args.push("/multimon".into());
    }

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
    Ok(())
}

fn spawn_macos_rdp_file(host: &str, port: u16, config: &RdpConfig) -> Result<(), String> {
    let quality = config.quality.as_deref().unwrap_or("ultra");
    let bpp = match quality {
        "wan" => 16,
        "balanced" => 24,
        _ => 32,
    };
    let (width, height, fullscreen, dynamic_res) = match config.resolution.as_deref() {
        Some("fullscreen") => (2560, 1440, 2, 1),
        Some("2560x1440") => (2560, 1440, 1, 1),
        Some("1920x1200") => (1920, 1200, 1, 1),
        Some("1920x1080") => (1920, 1080, 1, 1),
        Some("1600x900") => (1600, 900, 1, 1),
        _ => (1920, 1200, 1, 1),
    };

    let rdp_content = format!(
        "full address:s:{host}:{port}\r\n\
         username:s:{}\r\n\
         session bpp:i:{bpp}\r\n\
         screen mode id:i:{fullscreen}\r\n\
         desktopwidth:i:{width}\r\n\
         desktopheight:i:{height}\r\n\
         dynamic resolution:i:{dynamic_res}\r\n\
         allow font smoothing:i:1\r\n\
         allow desktop composition:i:1\r\n\
         disable full window drag:i:0\r\n\
         compression:i:1\r\n\
         audiomode:i:0\r\n\
         redirectclipboard:i:1\r\n\
         autoreconnection enabled:i:1\r\n",
        config.username
    );

    let tmp_path = std::env::temp_dir().join(format!("novrix-{}-{port}.rdp", config.host));
    std::fs::write(&tmp_path, rdp_content).map_err(|e| e.to_string())?;

    Command::new("/usr/bin/open")
        .arg(&tmp_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not open .rdp profile: {e}"))?;
    Ok(())
}

pub fn launch_rdp_internal(
    app: &AppHandle,
    store: Option<&Store>,
    config: &RdpConfig,
) -> Result<(), String> {
    let (target_host, target_port) = if let (Some(st), Some(jump_id)) = (
        store,
        config.jump_session_id.as_deref().filter(|s| !s.is_empty()),
    ) {
        maybe_open_ssh_tunnel(st, jump_id, &config.host, config.port)?
    } else {
        (config.host.clone(), config.port)
    };

    if let Some(binary) = find_binary() {
        spawn_freerdp(&binary.to_string_lossy(), &target_host, target_port, config)?;
        let _ = app.emit(
            "rdp-launched",
            format!("Iniciando RDP HD hacia {}:{}…", config.host, config.port),
        );
        return Ok(());
    }

    if find_macos_rdp_app().is_some() {
        spawn_macos_rdp_file(&target_host, target_port, config)?;
        let _ = app.emit(
            "rdp-launched",
            format!("Iniciando Windows App RDP hacia {}:{}…", config.host, config.port),
        );
        return Ok(());
    }

    Err("Instala FreeRDP (`brew install freerdp`) o Windows App para macOS".to_string())
}

#[tauri::command]
pub fn launch_rdp(
    app: AppHandle,
    store: State<'_, Store>,
    config: RdpConfig,
) -> Result<(), String> {
    launch_rdp_internal(&app, Some(&store), &config)
}

#[tauri::command]
pub fn connect_saved_rdp(
    app: AppHandle,
    store: State<'_, Store>,
    id: String,
) -> Result<(), String> {
    let saved = store.get_session(&id)?;
    if saved.protocol != "rdp" {
        return Err("Esta sesión guardada no es una sesión RDP".into());
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
        quality: saved
            .domain
            .clone()
            .filter(|d| ["ultra", "balanced", "wan"].contains(&d.as_str())),
        clipboard: true,
        audio: true,
        multimon: saved.rdp_resolution.as_deref() == Some("fullscreen"),
        jump_session_id: saved.jump_session_id.clone(),
    };
    launch_rdp_internal(&app, Some(&store), &config)
}
