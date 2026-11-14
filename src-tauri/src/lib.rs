mod metrics;
mod pty;
mod rdp;
mod secrets;
mod serial;
mod session;
mod sftp;
mod ssh;
mod store;
mod tunnels;
mod vnc;

use session::SessionManager;
use store::Store;
use tunnels::TunnelManager;

/// Open the session-logs folder in Finder.
#[tauri::command]
fn reveal_logs() -> Result<(), String> {
    let dir = ssh::logs_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    return std::process::Command::new("open")
        .arg(&dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string());
    #[cfg(not(target_os = "macos"))]
    return Err(format!("logs are stored at {}", dir.display()));
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let store = Store::open().expect("failed to open the sessions database");
    tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_opener::init())
        .manage(SessionManager::default())
        .manage(TunnelManager::default())
        .manage(store)
        .invoke_handler(tauri::generate_handler![
            session::create_local_session,
            session::create_ssh_session,
            session::write_to_session,
            session::resize_session,
            session::close_session,
            ssh::connect_saved_session,
            ssh::accept_changed_host_key,
            serial::list_serial_ports,
            serial::create_serial_session,
            sftp::sftp_list_dir,
            sftp::sftp_download,
            sftp::sftp_upload_bytes,
            sftp::sftp_mkdir,
            sftp::sftp_delete,
            tunnels::list_tunnels,
            tunnels::start_tunnel,
            tunnels::stop_tunnel,
            metrics::poll_ssh_metrics,
            rdp::rdp_availability,
            rdp::launch_rdp,
            rdp::connect_saved_rdp,
            vnc::vnc_availability,
            vnc::launch_vnc,
            vnc::connect_saved_vnc,
            vnc::probe_remote_endpoint,
            store::list_tree,
            store::create_group,
            store::rename_group,
            store::set_group_identity,
            store::delete_group,
            store::move_group,
            store::move_session,
            store::duplicate_session,
            store::list_identities,
            store::save_identity,
            store::delete_identity,
            store::list_snippets,
            store::save_snippet,
            store::delete_snippet,
            store::list_expect_rules,
            store::save_expect_rule,
            store::toggle_expect_rule,
            store::delete_expect_rule,
            store::save_session,
            store::delete_session,
            store::import_ssh_config,
            store::import_sessions_content,
            reveal_logs,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Switchboard Term");
}
