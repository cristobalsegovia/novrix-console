//! Graphical SSH Port Forwarding (-L Local Forward and -D Dynamic SOCKS5 Proxy)
//! multiplexed over active SSH sessions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::State;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use uuid::Uuid;

use crate::session::{SessionManager, SharedSshHandle};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelInfo {
    pub id: String,
    pub session_id: String,
    pub name: String,
    pub kind: String, // "local" | "socks5"
    pub bind_host: String,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    pub active_connections: usize,
    pub bytes_transferred: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTunnelInput {
    pub session_id: String,
    pub name: String,
    pub kind: String, // "local" | "socks5"
    #[serde(default = "default_bind_host")]
    pub bind_host: String,
    pub local_port: u16,
    #[serde(default)]
    pub remote_host: String,
    #[serde(default)]
    pub remote_port: u16,
}

fn default_bind_host() -> String {
    "127.0.0.1".into()
}

struct ActiveTunnel {
    info: TunnelInfo,
    active_connections: Arc<AtomicUsize>,
    bytes_transferred: Arc<AtomicU64>,
    stop_tx: watch::Sender<bool>,
}

#[derive(Default)]
pub struct TunnelManager {
    tunnels: Mutex<HashMap<String, ActiveTunnel>>,
}

impl TunnelManager {
    pub fn list(&self) -> Vec<TunnelInfo> {
        let map = self.tunnels.lock().unwrap();
        let mut out: Vec<TunnelInfo> = map
            .values()
            .map(|t| {
                let mut info = t.info.clone();
                info.active_connections = t.active_connections.load(Ordering::Relaxed);
                info.bytes_transferred = t.bytes_transferred.load(Ordering::Relaxed);
                info
            })
            .collect();
        out.sort_by(|a, b| a.local_port.cmp(&b.local_port));
        out
    }

    pub fn stop(&self, id: &str) -> Result<(), String> {
        let removed = self.tunnels.lock().unwrap().remove(id);
        if let Some(t) = removed {
            let _ = t.stop_tx.send(true);
            Ok(())
        } else {
            Err(format!("Tunnel {id} not found"))
        }
    }

    pub fn stop_for_session(&self, session_id: &str) {
        let mut map = self.tunnels.lock().unwrap();
        let ids: Vec<String> = map
            .iter()
            .filter(|(_, t)| t.info.session_id == session_id)
            .map(|(k, _)| k.clone())
            .collect();
        for id in ids {
            if let Some(t) = map.remove(&id) {
                let _ = t.stop_tx.send(true);
            }
        }
    }
}

/// Handle a single SOCKS5 handshake on `stream` and return the requested `(target_host, target_port)`.
async fn socks5_handshake(stream: &mut TcpStream) -> Result<(String, u16), String> {
    let mut header = [0u8; 2];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|e| format!("SOCKS5 greeting failed: {e}"))?;
    if header[0] != 0x05 {
        return Err("Unsupported SOCKS version (expected SOCKS5)".into());
    }
    let nmethods = header[1] as usize;
    let mut methods = vec![0u8; nmethods];
    stream
        .read_exact(&mut methods)
        .await
        .map_err(|e| e.to_string())?;
    // Respond with "No authentication required" (0x05, 0x00)
    stream
        .write_all(&[0x05, 0x00])
        .await
        .map_err(|e| e.to_string())?;

    // Read connection request: VER | CMD | RSV | ATYP
    let mut req = [0u8; 4];
    stream
        .read_exact(&mut req)
        .await
        .map_err(|e| e.to_string())?;
    if req[1] != 0x01 {
        // Only CONNECT (0x01) is supported
        let _ = stream
            .write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await;
        return Err("SOCKS5 only supports CONNECT".into());
    }

    let host = match req[3] {
        0x01 => {
            // IPv4
            let mut ip = [0u8; 4];
            stream.read_exact(&mut ip).await.map_err(|e| e.to_string())?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        0x03 => {
            // Domain name
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(|e| e.to_string())?;
            let mut domain = vec![0u8; len[0] as usize];
            stream
                .read_exact(&mut domain)
                .await
                .map_err(|e| e.to_string())?;
            String::from_utf8(domain).map_err(|e| e.to_string())?
        }
        0x04 => {
            // IPv6
            let mut ip = [0u8; 16];
            stream.read_exact(&mut ip).await.map_err(|e| e.to_string())?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        _ => return Err("Unsupported SOCKS5 address type".into()),
    };

    let mut port_buf = [0u8; 2];
    stream
        .read_exact(&mut port_buf)
        .await
        .map_err(|e| e.to_string())?;
    let port = u16::from_be_bytes(port_buf);

    // Send success reply
    stream
        .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
        .await
        .map_err(|e| e.to_string())?;

    Ok((host, port))
}

async fn handle_tunnel_client(
    mut tcp: TcpStream,
    ssh: SharedSshHandle,
    kind: String,
    remote_host: String,
    remote_port: u16,
    active_conns: Arc<AtomicUsize>,
    bytes_counter: Arc<AtomicU64>,
) {
    active_conns.fetch_add(1, Ordering::Relaxed);
    let _guard = scopeguard(&active_conns);

    let (target_host, target_port) = if kind == "socks5" {
        match socks5_handshake(&mut tcp).await {
            Ok(pair) => pair,
            Err(_) => return,
        }
    } else {
        (remote_host, remote_port)
    };

    let channel_res = {
        let guard = ssh.lock().await;
        guard
            .channel_open_direct_tcpip(&target_host, target_port as u32, "127.0.0.1", 0)
            .await
    };

    if let Ok(channel) = channel_res {
        let mut ssh_stream = channel.into_stream();
        if let Ok((up, down)) = tokio::io::copy_bidirectional(&mut tcp, &mut ssh_stream).await {
            bytes_counter.fetch_add(up + down, Ordering::Relaxed);
        }
    }
}

struct ConnGuard(Arc<AtomicUsize>);
impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
fn scopeguard(counter: &Arc<AtomicUsize>) -> ConnGuard {
    ConnGuard(counter.clone())
}

#[tauri::command]
pub fn list_tunnels(tunnels: State<'_, TunnelManager>) -> Result<Vec<TunnelInfo>, String> {
    Ok(tunnels.list())
}

#[tauri::command]
pub async fn start_tunnel(
    sessions: State<'_, SessionManager>,
    tunnels: State<'_, TunnelManager>,
    input: CreateTunnelInput,
) -> Result<TunnelInfo, String> {
    let ssh = sessions
        .get_ssh(&input.session_id)
        .ok_or_else(|| "Selected SSH session is no longer active".to_string())?;

    let bind_addr = format!("{}:{}", input.bind_host, input.local_port);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| format!("Could not bind local port {bind_addr}: {e}"))?;
    let actual_port = listener
        .local_addr()
        .map(|a| a.port())
        .unwrap_or(input.local_port);

    let id = Uuid::new_v4().to_string();
    let info = TunnelInfo {
        id: id.clone(),
        session_id: input.session_id.clone(),
        name: if input.name.trim().is_empty() {
            if input.kind == "socks5" {
                format!("SOCKS5 :{actual_port}")
            } else {
                format!(
                    ":{actual_port} → {}:{}",
                    input.remote_host, input.remote_port
                )
            }
        } else {
            input.name.trim().to_string()
        },
        kind: input.kind.clone(),
        bind_host: input.bind_host.clone(),
        local_port: actual_port,
        remote_host: input.remote_host.clone(),
        remote_port: input.remote_port,
        active_connections: 0,
        bytes_transferred: 0,
    };

    let active_connections = Arc::new(AtomicUsize::new(0));
    let bytes_transferred = Arc::new(AtomicU64::new(0));
    let (stop_tx, mut stop_rx) = watch::channel(false);

    tunnels.tunnels.lock().unwrap().insert(
        id.clone(),
        ActiveTunnel {
            info: info.clone(),
            active_connections: active_connections.clone(),
            bytes_transferred: bytes_transferred.clone(),
            stop_tx,
        },
    );

    let kind = input.kind;
    let remote_host = input.remote_host;
    let remote_port = input.remote_port;

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        break;
                    }
                }
                accept_res = listener.accept() => {
                    match accept_res {
                        Ok((tcp, _addr)) => {
                            tokio::spawn(handle_tunnel_client(
                                tcp,
                                ssh.clone(),
                                kind.clone(),
                                remote_host.clone(),
                                remote_port,
                                active_connections.clone(),
                                bytes_transferred.clone(),
                            ));
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    });

    Ok(info)
}

#[tauri::command]
pub fn stop_tunnel(tunnels: State<'_, TunnelManager>, id: String) -> Result<(), String> {
    tunnels.stop(&id)
}
