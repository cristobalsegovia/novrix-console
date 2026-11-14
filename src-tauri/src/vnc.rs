//! High-Definition VNC / RFB support for macOS Screen Sharing (`Screen Sharing.app`),
//! TigerVNC (`vncviewer`), TightVNC, UltraVNC, VNC-over-SSH tunneling, and live
//! endpoint latency / RFB banner probing.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::secrets;
use crate::store::Store;

const SEARCH_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];
const CANDIDATES: &[&str] = &["vncviewer", "tigervnc", "xtigervncviewer"];

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VncAvailability {
    pub available: bool,
    pub native_screen_sharing: bool,
    pub tigervnc_binary: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteProbeResult {
    pub reachable: bool,
    pub latency_ms: Option<u64>,
    pub banner: Option<String>,
}

fn find_tigervnc() -> Option<PathBuf> {
    for name in CANDIDATES {
        for dir in SEARCH_DIRS {
            let path = PathBuf::from(dir).join(name);
            if path.exists() {
                return Some(path);
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir("/Applications") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("TigerVNC") && name.ends_with(".app") {
                let bin = entry.path().join("Contents/MacOS/TigerVNC Viewer");
                if bin.exists() {
                    return Some(bin);
                }
                let bin2 = entry.path().join("Contents/MacOS/vncviewer");
                if bin2.exists() {
                    return Some(bin2);
                }
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
pub fn vnc_availability() -> VncAvailability {
    let tiger = find_tigervnc().map(|p| p.to_string_lossy().into_owned());
    VncAvailability {
        available: true,
        native_screen_sharing: true,
        tigervnc_binary: tiger,
    }
}

#[tauri::command]
pub async fn probe_remote_endpoint(host: String, port: u16) -> RemoteProbeResult {
    tauri::async_runtime::spawn_blocking(move || {
        let addr_str = format!("{host}:{port}");
        let Ok(mut addrs) = addr_str.to_socket_addrs() else {
            return RemoteProbeResult {
                reachable: false,
                latency_ms: None,
                banner: None,
            };
        };
        let Some(addr) = addrs.next() else {
            return RemoteProbeResult {
                reachable: false,
                latency_ms: None,
                banner: None,
            };
        };

        let start = Instant::now();
        match TcpStream::connect_timeout(&addr, Duration::from_millis(1800)) {
            Ok(mut stream) => {
                let ms = start.elapsed().as_millis() as u64;
                let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                let mut buf = [0u8; 32];
                let banner = match stream.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        let s = String::from_utf8_lossy(&buf[..n]).trim().to_string();
                        if s.starts_with("RFB ") {
                            Some(s)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                RemoteProbeResult {
                    reachable: true,
                    latency_ms: Some(ms.max(1)),
                    banner,
                }
            }
            Err(_) => RemoteProbeResult {
                reachable: false,
                latency_ms: None,
                banner: None,
            },
        }
    })
    .await
    .unwrap_or(RemoteProbeResult {
        reachable: false,
        latency_ms: None,
        banner: None,
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VncConfig {
    pub host: String,
    #[serde(default = "default_vnc_port")]
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// "auto" | "screensharing" | "tight" | "zrle" | "ultra" | "hextile" | "raw"
    pub variant: Option<String>,
    /// "shared" | "viewonly" | "fullscreen" (or "shared|ultra")
    pub mode: Option<String>,
    /// "ultra" | "balanced" | "wan"
    pub quality: Option<String>,
    pub jump_session_id: Option<String>,
}

fn default_vnc_port() -> u16 {
    5900
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn maybe_open_ssh_tunnel(
    store: &Store,
    jump_id: &str,
    target_host: &str,
    target_port: u16,
) -> Result<(String, u16), String> {
    let jump = store.get_session(jump_id)?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("could not allocate local port for VNC SSH tunnel: {e}"))?;
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
        .map_err(|e| format!("failed to start SSH tunnel for VNC: {e}"))?;

    if !status.success() {
        return Err(format!(
            "SSH tunnel through {}@{} failed (verify SSH key authentication on jump host)",
            jump.username, jump.host
        ));
    }

    Ok(("127.0.0.1".to_string(), local_port))
}

fn parse_mode_and_quality(config: &VncConfig) -> (&str, &str) {
    let raw_mode = config.mode.as_deref().unwrap_or("shared");
    if let Some((m, q)) = raw_mode.split_once('|') {
        let qual = config.quality.as_deref().unwrap_or(q);
        (m, qual)
    } else {
        let qual = config.quality.as_deref().unwrap_or("ultra");
        (raw_mode, qual)
    }
}

fn spawn_tigervnc(binary: &PathBuf, host: &str, port: u16, config: &VncConfig) -> Result<(), String> {
    let mut args: Vec<String> = Vec::new();
    let variant = config.variant.as_deref().unwrap_or("auto");
    let (mode, quality) = parse_mode_and_quality(config);

    if mode != "exclusive" {
        args.push("-Shared".into());
    }
    if mode == "viewonly" {
        args.push("-ViewOnly".into());
    } else if mode == "fullscreen" {
        args.push("-FullScreen".into());
    }

    // Dynamic remote desktop resizing
    args.push("-RemoteResize=1".into());
    args.push("-AutoSelect=0".into());

    // Color depth & quality tuning
    match quality {
        "wan" => {
            args.push("-FullColor=1".into());
            args.push("-QualityLevel".into());
            args.push("4".into());
            args.push("-CompressLevel".into());
            args.push("6".into());
        }
        "balanced" => {
            args.push("-FullColor=1".into());
            args.push("-QualityLevel".into());
            args.push("7".into());
            args.push("-CompressLevel".into());
            args.push("4".into());
        }
        _ => {
            // Ultra HD TrueColor (lossless/near-lossless sharpness)
            args.push("-FullColor=1".into());
            args.push("-QualityLevel".into());
            args.push("9".into());
            args.push("-CompressLevel".into());
            args.push("2".into());
        }
    }

    match variant {
        "tight" => {
            args.push("-PreferredEncoding".into());
            args.push("Tight".into());
        }
        "zrle" => {
            args.push("-PreferredEncoding".into());
            args.push("ZRLE".into());
        }
        "ultra" | "hextile" => {
            args.push("-PreferredEncoding".into());
            args.push("Hextile".into());
        }
        "raw" => {
            args.push("-PreferredEncoding".into());
            args.push("Raw".into());
        }
        _ => {
            args.push("-PreferredEncoding".into());
            args.push("ZRLE".into());
        }
    }

    let has_pw = config.password.as_deref().map(|p| !p.is_empty()).unwrap_or(false);
    if has_pw {
        args.push("-passwd".into());
        args.push("/dev/stdin".into());
    }

    args.push(format!("{host}::{port}"));

    let mut child = Command::new(binary)
        .args(&args)
        .stdin(if has_pw { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not launch TigerVNC: {e}"))?;

    if has_pw {
        if let Some(mut stdin) = child.stdin.take() {
            let pw = config.password.as_deref().unwrap_or("");
            let _ = stdin.write_all(format!("{pw}\n").as_bytes());
        }
    }
    Ok(())
}

fn spawn_macos_screen_sharing(host: &str, port: u16, config: &VncConfig) -> Result<(), String> {
    let user = config
        .username
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());
    let pass = config
        .password
        .as_deref()
        .filter(|p| !p.is_empty());

    let auth_part = match (user, pass) {
        (Some(u), Some(p)) => format!("{}:{}@", percent_encode(u), percent_encode(p)),
        (Some(u), None) => format!("{}@", percent_encode(u)),
        (None, Some(p)) => format!(":{}@", percent_encode(p)),
        (None, None) => String::new(),
    };

    let url = format!("vnc://{auth_part}{host}:{port}");
    Command::new("/usr/bin/open")
        .arg("-a")
        .arg("Screen Sharing")
        .arg(&url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not launch macOS Screen Sharing: {e}"))?;
    Ok(())
}

pub fn launch_vnc_internal(
    app: &AppHandle,
    store: Option<&Store>,
    config: &VncConfig,
) -> Result<(), String> {
    let (target_host, target_port) = if let (Some(st), Some(jump_id)) = (
        store,
        config.jump_session_id.as_deref().filter(|s| !s.is_empty()),
    ) {
        maybe_open_ssh_tunnel(st, jump_id, &config.host, config.port)?
    } else {
        (config.host.clone(), config.port)
    };

    let variant = config.variant.as_deref().unwrap_or("auto");
    let tiger = find_tigervnc();

    if variant != "screensharing" {
        if let Some(ref bin) = tiger {
            spawn_tigervnc(bin, &target_host, target_port, config)?;
            let _ = app.emit(
                "vnc-launched",
                format!("Iniciando VNC HD ({variant}) hacia {}:{}…", config.host, config.port),
            );
            return Ok(());
        }
    }

    spawn_macos_screen_sharing(&target_host, target_port, config)?;
    let _ = app.emit(
        "vnc-launched",
        format!("Iniciando VNC HD hacia {}:{}…", config.host, config.port),
    );
    Ok(())
}

#[tauri::command]
pub fn launch_vnc(
    app: AppHandle,
    store: State<'_, Store>,
    config: VncConfig,
) -> Result<(), String> {
    launch_vnc_internal(&app, Some(&store), &config)
}

#[tauri::command]
pub fn connect_saved_vnc(
    app: AppHandle,
    store: State<'_, Store>,
    id: String,
) -> Result<(), String> {
    let saved = store.get_session(&id)?;
    if saved.protocol != "vnc" {
        return Err("Esta sesión guardada no es una conexión VNC".into());
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

    let raw_mode = saved.rdp_resolution.clone().unwrap_or_else(|| "shared|ultra".into());
    let (mode, quality) = if let Some((m, q)) = raw_mode.split_once('|') {
        (Some(m.to_string()), Some(q.to_string()))
    } else {
        (Some(raw_mode), Some("ultra".to_string()))
    };

    let config = VncConfig {
        host: saved.host.clone(),
        port: saved.port,
        username: if username.trim().is_empty() {
            None
        } else {
            Some(username)
        },
        password,
        variant: saved.domain.clone(),
        mode,
        quality,
        jump_session_id: saved.jump_session_id.clone(),
    };
    launch_vnc_internal(&app, Some(&store), &config)
}
