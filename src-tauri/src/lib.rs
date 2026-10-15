//! Novrix Console core library.
//!
//! This is the scaffolding entry point created in task 1.1. The memory-safe
//! Rust core (Connection_Manager, Credential_Store, Protocol_Engine, etc.)
//! is implemented in later tasks. For now it exposes a single sample command
//! so the Tauri IPC bridge compiles and can be smoke-tested from the frontend.

/// Shared domain types and the structured error model (task 1.2). These cross
/// the Tauri IPC boundary as `serde` payloads and underpin every core
/// component implemented in later tasks.
pub mod domain;

/// Protocol_Engine: the uniform `Protocol`/`SessionConn` interface and the
/// `ProtocolRegistry` (task 7.1). Concrete protocols land in later tasks.
pub mod protocol;

/// Local SQLite persistence for profiles, folders, and settings (task 2.1):
/// schema, connection bootstrap, and schema-version migration. Secrets never
/// enter this layer — only credential references.
pub mod persistence;

/// Connection_Manager (tasks 3.x, 18.1): profile validation and persistence
/// over the SQLite repository — `ProfileDraft` validation, profile and folder
/// create/update/move/delete, and versioned configuration export with additive,
/// id-remapping import.
pub mod connection;

/// Credential_Store (task 5.1): an auditable wrapper over OS Secure_Storage
/// (`keyring-rs`). Stores/retrieves/deletes secrets keyed by profile/credential
/// key, never persists plaintext to disk, and returns structured errors.
pub mod credential;

/// Minimum-OS-version startup gate (task 1.3). Blocks startup on hosts below
/// macOS 12 / Windows 10 and surfaces a minimum-version error.
pub mod startup;

/// Terminal_Emulator (task 14.1): the ANSI state machine, screen grid, and
/// bounded scrollback. Parses the byte stream from text-based Sessions,
/// maintains the queryable grid + cursor + cell styling, discards malformed
/// sequences, and reports viewport resizes.
pub mod terminal;

/// Key_Manager: SSH key generation, import, and public-key export (task 6.1).
/// Validates key type/size, parses OpenSSH private keys, and routes encrypted-
/// key passphrases to Secure_Storage via an injected secret-store port.
pub mod keys;

/// File_Transfer_Service (task 15.1): SFTP/SCP remote listing and
/// upload/download with ≤1 s progress reporting, cooperative cancel, overwrite
/// confirmation, and incomplete-destination cleanup over an injectable backend.
pub mod transfer;

/// Session_Logger (task 16.1): timestamped (ISO-8601) recording of session I/O
/// with enable/disable mid-session, destination validation, and retain-on-
/// failure, over injectable clock and write-sink ports.
pub mod logging;

/// Session_Manager (task 13.1): opens Sessions in Tabs backed by per-session
/// `tokio` task sets, enforces the concurrency limit (≥20), keeps each
/// Session's state and output buffer isolated, tracks the single active tab,
/// and tears down Sessions on tab close with a confirmation gate for active
/// Sessions.
pub mod session;

/// Script_Engine (task 17.1): runs an automation `Script` against a text-based
/// Session over the uniform `SessionConn` interface — sequential sending,
/// wait-for-prompt gating with a bounded timeout, stop-on-disconnect, and
/// accurate success/failure reporting. The session and clock seams are
/// injectable so the engine is testable without a network.
pub mod script;

/// Tauri IPC bridge (task 20.1): the request/response `#[tauri::command]`
/// surface and the managed `AppState` that owns the core components. Every
/// command wraps an already-implemented core API and returns
/// `Result<T, DomainError>`, so network/secret/serial access stays in the core
/// and errors cross the boundary as structured `serde` payloads. Streaming
/// events are task 20.2 — now implemented in its `events` submodule, which
/// streams `session://{id}/output` and `transfer://{id}/progress`.
pub mod ipc;

/// Build the production application state, opening the SQLite database under the
/// OS-appropriate application data directory.
///
/// The database path is resolved from the platform data dir with a stable
/// `novrix-console` namespace, falling back to the current directory if no data
/// dir is available. Opening is create-on-first-run (task 2.1), so a fresh
/// install starts with an empty, migrated schema.
fn build_app_state() -> Result<ipc::AppState, domain::DomainError> {
    let data_dir = dirs_data_dir().join("novrix-console");
    std::fs::create_dir_all(&data_dir).map_err(|e| {
        domain::DomainError::new(
            "persistence.dataDir",
            "Failed to create the application data directory.",
        )
        .with_cause(e.to_string())
    })?;
    let db_path = data_dir.join("novrix.sqlite3");
    let db = persistence::Database::open(&db_path)?;
    // Task 26.3: the known-hosts trust store needs its own SQLite connection to
    // the same file — the profile `Database` is moved onto the Connection_Manager
    // actor thread, and SQLite serializes per connection. Opening a second handle
    // migrates the shared file idempotently (both run the same, version-gated
    // migration), so the `known_hosts` table exists exactly once.
    let known_hosts_db = persistence::Database::open(&db_path)?;
    Ok(ipc::AppState::new(db, known_hosts_db))
}

/// Resolve the platform application-data directory without pulling in an extra
/// crate: `$HOME/Library/Application Support` on macOS, `%APPDATA%` on Windows,
/// `$XDG_DATA_HOME` (or `$HOME/.local/share`) elsewhere, falling back to `.`.
fn dirs_data_dir() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home)
                .join("Library")
                .join("Application Support");
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return std::path::PathBuf::from(appdata);
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            return std::path::PathBuf::from(xdg);
        }
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home).join(".local").join("share");
        }
    }
    std::path::PathBuf::from(".")
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Requirement 1.6: gate startup on the host OS version. Detect the real
    // host version, then apply the pure decision rule. On an unsupported host
    // we surface the minimum-version message and prevent the app from starting.
    if let Err(err) = startup::evaluate_startup(startup::detect_host_os()) {
        eprintln!("{}", err.message);
        std::process::exit(1);
    }

    // Construct the core application state (DB + components). A failure here is
    // fatal: without persistence the app cannot operate.
    let state = match build_app_state() {
        Ok(state) => state,
        Err(err) => {
            eprintln!("{}: {}", err.code, err.message);
            std::process::exit(1);
        }
    };

    // Task 20.2: capture the shared event emitter before `state` is moved into
    // the builder, so `setup` can attach the real `AppHandle` to it. Attaching
    // completes the streaming-event path — the SessionManager's output sink and
    // any transfer progress sink built from this emitter can now emit
    // `session://{id}/output` and `transfer://{id}/progress` events to the UI.
    let events = state.events.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .setup(move |app| {
            // The `AppHandle` only exists now that the app is built; hand it to
            // the emitter so every streamed event has a live channel to the UI.
            events.attach(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // Profiles
            ipc::save_profile,
            ipc::update_profile,
            ipc::delete_profile,
            ipc::list_profiles,
            ipc::save_credential,
            ipc::has_credential,
            // Snippets
            ipc::save_snippet,
            ipc::update_snippet,
            ipc::delete_snippet,
            ipc::list_snippets,
            // Folders
            ipc::create_folder,
            ipc::rename_folder,
            ipc::move_profile,
            ipc::move_folder,
            ipc::delete_folder,
            ipc::list_folders,
            // Protocol / discovery
            ipc::selectable_protocols,
            ipc::list_serial_ports,
            // Sessions
            ipc::open_session,
            ipc::activate_tab,
            ipc::close_tab,
            ipc::session_state,
            ipc::send_input,
            ipc::active_tab,
            ipc::session_output,
            ipc::respond_host_key_prompt,
            ipc::forget_host_key,
            // Keys
            ipc::generate_key,
            ipc::import_key,
            ipc::key_public_key,
            ipc::delete_key,
            // Transfers
            ipc::list_remote,
            ipc::upload_file,
            ipc::download_file,
            ipc::cancel_transfer,
            // Logging
            ipc::start_logging,
            ipc::stop_logging,
            // Script
            ipc::run_script,
            // Config
            ipc::export_config,
            ipc::import_config,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
