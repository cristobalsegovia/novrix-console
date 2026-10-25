//! Real-time Server Metrics HUD multiplexed over an active SSH session's handle.
//! Runs a non-interactive `exec` channel probe to read CPU load, RAM usage,
//! root filesystem usage, and OS kernel name without disturbing the user's PTY.

use std::time::Duration;

use russh::ChannelMsg;
use serde::Serialize;
use tauri::State;

use crate::session::SessionManager;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerMetrics {
    pub supported: bool,
    pub os: String,
    pub load_avg: String,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub mem_percent: u8,
    pub disk_percent: u8,
}

const PROBE_CMD: &str = concat!(
    "sh -c '",
    "echo \"OS:$(uname -sr 2>/dev/null)\"; ",
    "if [ -f /proc/loadavg ]; then awk \"{print \\\"LOAD:\\\"\\$1\\\" \\\"\\$2}\" /proc/loadavg; ",
    "elif command -v sysctl >/dev/null 2>&1; then echo \"LOAD:$(sysctl -n vm.loadavg 2>/dev/null | tr -d \"{}\")\"; fi; ",
    "if [ -f /proc/meminfo ]; then awk \"/MemTotal/{t=\\$2}/MemAvailable/{a=\\$2}END{if(t>0)printf \\\"MEM:%d:%d\\n\\\",(t-a)/1024,t/1024}\" /proc/meminfo; fi; ",
    "df -Pk / 2>/dev/null | awk \"NR==2{gsub(/%/,\\\"\\\",\\$5); print \\\"DISK:\\\"\\$5}\"",
    "'"
);

#[tauri::command]
pub async fn poll_ssh_metrics(
    sessions: State<'_, SessionManager>,
    session_id: String,
) -> Result<ServerMetrics, String> {
    let ssh = sessions
        .get_ssh(&session_id)
        .ok_or_else(|| "Session not active".to_string())?;

    let fut = async {
        let mut channel = {
            let guard = ssh.lock().await;
            guard
                .channel_open_session()
                .await
                .map_err(|e| e.to_string())?
        };
        channel
            .exec(true, PROBE_CMD)
            .await
            .map_err(|e| e.to_string())?;

        let mut output = Vec::new();
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => output.extend_from_slice(&data),
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        let _ = channel.close().await;
        Ok::<String, String>(String::from_utf8_lossy(&output).into_owned())
    };

    let raw = match tokio::time::timeout(Duration::from_secs(3), fut).await {
        Ok(Ok(s)) => s,
        _ => {
            return Ok(ServerMetrics {
                supported: false,
                os: String::new(),
                load_avg: String::new(),
                mem_used_mb: 0,
                mem_total_mb: 0,
                mem_percent: 0,
                disk_percent: 0,
            })
        }
    };

    let mut os = String::new();
    let mut load_avg = String::new();
    let mut mem_used_mb = 0u64;
    let mut mem_total_mb = 0u64;
    let mut disk_percent = 0u8;

    for line in raw.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("OS:") {
            os = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("LOAD:") {
            let tokens: Vec<&str> = rest.split_whitespace().take(2).collect();
            load_avg = tokens.join(" ");
        } else if let Some(rest) = line.strip_prefix("MEM:") {
            let mut parts = rest.split(':');
            if let (Some(u), Some(t)) = (parts.next(), parts.next()) {
                mem_used_mb = u.trim().parse().unwrap_or(0);
                mem_total_mb = t.trim().parse().unwrap_or(0);
            }
        } else if let Some(rest) = line.strip_prefix("DISK:") {
            disk_percent = rest.trim().parse().unwrap_or(0);
        }
    }

    let supported = !os.is_empty() && (!load_avg.is_empty() || mem_total_mb > 0 || disk_percent > 0);
    let mem_percent = if mem_total_mb > 0 {
        ((mem_used_mb.saturating_mul(100)) / mem_total_mb).min(100) as u8
    } else {
        0
    };

    Ok(ServerMetrics {
        supported,
        os,
        load_avg,
        mem_used_mb,
        mem_total_mb,
        mem_percent,
        disk_percent,
    })
}
