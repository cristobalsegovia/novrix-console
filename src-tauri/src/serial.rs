//! Native Serial Console sessions (FTDI, Cisco USB-Rollover, CP210x, CH340…)
//! backed by `serial2`, with automatic per-session logging.

use std::io::Write as IoWrite;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serial2::SerialPort;
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

use crate::session::{SessionExit, SessionHandle, SessionManager};
use crate::ssh;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerialConfig {
    pub port_name: String,
    #[serde(default = "default_baud")]
    pub baud_rate: u32,
}

fn default_baud() -> u32 {
    115200
}

#[tauri::command]
pub fn list_serial_ports() -> Vec<String> {
    let mut ports: Vec<String> = SerialPort::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    // On macOS, prefer `/dev/cu.*` callout devices over `/dev/tty.*` so opening
    // doesn't block waiting for carrier detect (DCD) on console cables.
    if let Ok(entries) = std::fs::read_dir("/dev") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("cu.") {
                let full = format!("/dev/{name}");
                if !ports.contains(&full) {
                    ports.push(full);
                }
            }
        }
    }

    // Filter out duplicate tty.* if corresponding cu.* exists, and sort USB
    // serial adapters to the top.
    let port_set: std::collections::HashSet<String> = ports.iter().cloned().collect();
    ports.retain(|p| {
        if let Some(rest) = p.strip_prefix("/dev/tty.") {
            !port_set.contains(&format!("/dev/cu.{rest}"))
        } else {
            true
        }
    });

    ports.sort_by_key(|p| {
        let lower = p.to_lowercase();
        let is_usb = lower.contains("usb")
            || lower.contains("serial")
            || lower.contains("slab")
            || lower.contains("ftdi")
            || lower.contains("modem");
        (!is_usb, p.clone())
    });

    ports
}

pub fn spawn_serial(
    app: AppHandle,
    manager: &SessionManager,
    port_name: String,
    baud_rate: u32,
    label: String,
    on_data: Channel<Vec<u8>>,
) -> Result<String, String> {
    let mut port = SerialPort::open(&port_name, baud_rate)
        .map_err(|e| format!("could not open {port_name} at {baud_rate} baud: {e}"))?;
    let _ = port.set_read_timeout(Duration::from_millis(200));

    let reader_port = port
        .try_clone()
        .map_err(|e| format!("could not clone serial handle: {e}"))?;

    let id = Uuid::new_v4().to_string();
    manager.insert(
        id.clone(),
        SessionHandle::Serial {
            port: Mutex::new(port),
        },
    );

    let banner = format!(
        "\x1b[36m=== Serial console connected: {} @ {} baud (8N1) ===\x1b[0m\r\n",
        port_name, baud_rate
    );
    let _ = on_data.send(banner.into_bytes());

    let task_id = id.clone();
    std::thread::spawn(move || {
        let mut log = ssh::open_log(&label);
        let mut buf = [0u8; 4096];
        let mut reason = String::from("serial port closed");

        loop {
            match reader_port.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(f) = log.as_mut() {
                        let _ = f.write_all(&buf[..n]);
                    }
                    if on_data.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::TimedOut
                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    // Check if session was removed from manager (closed by user)
                    if let Some(state) = app.try_state::<SessionManager>() {
                        if state.write(&task_id, &[]).is_err() {
                            reason = "closed by user".into();
                            break;
                        }
                    }
                }
                Err(e) => {
                    reason = format!("serial error: {e}");
                    break;
                }
            }
        }

        if let Some(f) = log.as_mut() {
            let _ = writeln!(f, "\n=== serial session ended: {reason} ===");
        }
        if let Some(state) = app.try_state::<SessionManager>() {
            state.remove(&task_id);
        }
        let _ = app.emit(
            "session-exit",
            SessionExit {
                id: task_id,
                reason,
            },
        );
    });

    Ok(id)
}

#[tauri::command]
pub fn create_serial_session(
    app: AppHandle,
    state: State<'_, SessionManager>,
    config: SerialConfig,
    on_data: Channel<Vec<u8>>,
) -> Result<String, String> {
    let label = format!(
        "{}@{}",
        config.port_name.trim_start_matches("/dev/"),
        config.baud_rate
    );
    spawn_serial(
        app,
        &state,
        config.port_name,
        config.baud_rate,
        label,
        on_data,
    )
}
