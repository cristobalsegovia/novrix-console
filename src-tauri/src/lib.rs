mod pty;
mod rdp;
mod secrets;
mod session;
mod ssh;
mod store;

use session::SessionManager;
use store::Store;

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
        .manage(store)
        .invoke_handler(tauri::generate_handler![
            session::create_local_session,
            session::create_ssh_session,
            session::write_to_session,
            session::resize_session,
            session::close_session,
            ssh::connect_saved_session,
            rdp::rdp_availability,
            rdp::launch_rdp,
            rdp::connect_saved_rdp,
            store::list_tree,
            store::create_group,
            store::rename_group,
            store::delete_group,
            store::move_group,
            store::move_session,
            store::duplicate_session,
            store::list_snippets,
            store::save_snippet,
            store::delete_snippet,
            store::save_session,
            store::delete_session,
            reveal_logs,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Switchboard Term");
}
