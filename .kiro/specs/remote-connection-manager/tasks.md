# Implementation Plan: Novrix Console (remote-connection-manager)

## Overview

This plan builds Novrix Console incrementally on the approved Tauri 2 stack: a memory-safe **Rust core** (`tokio`) that owns all protocol, secret, and I/O logic, and a **TypeScript/React** frontend rendered in the system webview, bridged by Tauri commands and events. The language choice follows directly from the design (Rust for the core, TypeScript/React for the UI, named crates `russh`/`russh-sftp`, `serialport`, `ironrdp`, `keyring-rs`, `vte` + xterm.js, `rusqlite`), so no implementation-language question is required.

Work proceeds from the foundation outward: scaffold the Tauri workspace and shared error/domain types first, then the persistence layer, then each core component (Connection_Manager, Credential_Store, Key_Manager, the uniform Protocol_Engine and its four protocols, Session_Manager, Terminal_Emulator, File_Transfer_Service, Session_Logger, Script_Engine), then the IPC bridge and React UI, and finally branding, bundling, and the signed release pipeline. Property-based tests (`proptest`, ≥100 iterations, one test per correctness property, tagged `// Feature: remote-connection-manager, Property N`) are placed next to the code they validate so logic errors surface early. Each property subtask references its design property number and the requirements it checks.

## Tasks

- [x] 1. Scaffold Tauri 2 workspace and shared core foundation
  - [x] 1.1 Create the Tauri 2 project skeleton and Rust/TS workspace layout
    - Initialize the Tauri 2 app at the workspace root: `src-tauri/` (Rust core, `tokio` runtime) and `src/` (TypeScript/React frontend via Vite)
    - Add core crates to `Cargo.toml`: `tokio`, `russh`, `russh-sftp`, `serialport`, `ironrdp`, `keyring`, `vte`, `rusqlite`, `uuid`, `serde`/`serde_json`, `thiserror`, and `proptest` (dev-dependency)
    - Add frontend deps to `package.json`: React, `@tauri-apps/api`, `xterm`, and the Tauri CLI; wire a Vite + TypeScript config
    - Confirm `cargo build` and `npm run build` both succeed on an empty app shell
    - _Requirements: 1.1, 1.2_
  - [x] 1.2 Define shared domain types and the structured error model
    - Implement `DomainError` with machine-readable `code`, user-facing `message`, optional `cause`, plus the typed error variants (`ValidationError`, `CredentialError`, `ProtocolError`, `RegistrationError`, `KeyError`, `TransferError`, `LogError`, `ScriptError`, `ImportError`)
    - Implement the data-model enums and structs from the design: `ProtocolKind`, `ConnectionProfile`, `AuthConfig`, `ProtocolParams`, `Folder`, `Session`, `SessionState`, `BaudRate`, `DataBits`, `Parity`, `StopBits`, `Resolution`, `KeyType`, `ManagedKey`, `ExportFile`, and id newtypes (`ProfileId`, `FolderId`, `SessionId`, `KeyId`, `CredentialKey`)
    - Derive `serde` serialization so errors and models cross the IPC boundary as structured payloads
    - _Requirements: 2.1, 8.1_
  - [x] 1.3 Implement the minimum-OS-version startup gate
    - Detect the host OS version at startup; allow startup iff macOS ≥ 12 or Windows ≥ 10, otherwise block and surface a minimum-version error message
    - _Requirements: 1.6_
  - [x]* 1.4 Write property test for the OS version gate
    - **Property 1: Minimum OS version gate**
    - **Validates: Requirements 1.6**

- [x] 2. Implement SQLite persistence layer for profiles, folders, and settings
  - [x] 2.1 Create the SQLite schema and connection/migration bootstrap
    - Define `rusqlite`-backed tables for folders, profiles, and settings with stable UUID identifiers; ensure secrets are never stored here (only credential references)
    - Implement open/create-on-first-run and a simple schema-version migration step
    - _Requirements: 2.1, 3.1_
  - [x] 2.2 Implement profile and folder repository functions
    - CRUD read/write helpers for profiles and folders used by the Connection_Manager, returning `Result<_, DomainError>` and leaving prior state intact on write failure
    - _Requirements: 2.1, 2.6, 3.1_
  - [x]* 2.3 Write unit tests for the repository layer
    - Cover insert/read/update/delete round-trips and write-failure state preservation
    - _Requirements: 2.6_

- [x] 3. Implement Connection_Manager validation, profiles, and folder hierarchy
  - [x] 3.1 Implement profile validation and create/update
    - Validate host present, port in 1..=65535, protocol is a Supported_Protocol; on success persist and return the stored profile; apply identical rules on edit
    - On persistence failure, retain prior stored state and return an error naming the save failure
    - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.5, 2.6_
  - [x]* 3.2 Write property test for faithful profile save
    - **Property 3: Valid profile is saved faithfully**
    - **Validates: Requirements 2.1, 2.5**
  - [x]* 3.3 Write property test for profile validation rejection
    - **Property 4: Profile validation rejects invalid fields** (also governs Telnet connect-time port validation 5.5)
    - **Validates: Requirements 2.2, 2.3, 2.4, 5.5**
  - [x]* 3.4 Write property test for save-failure state preservation
    - **Property 5: Profile save failure preserves prior state**
    - **Validates: Requirements 2.6**
  - [x] 3.5 Implement profile deletion with confirmation
    - Require confirmation before removal; on confirm remove the profile (credential cleanup wired in task 5)
    - _Requirements: 2.7, 2.8_
  - [x]* 3.6 Write property test for confirmed profile deletion
    - **Property 6: Confirmed profile deletion removes the profile**
    - **Validates: Requirements 2.8**
  - [x]* 3.7 Write unit test for the profile-delete confirmation prompt
    - Verify deletion requests confirmation before removing
    - _Requirements: 2.7_
  - [x] 3.8 Implement folder create/rename with name validation
    - Accept iff name is 1..=255 chars and unique among siblings; otherwise reject with the specific reason; persist name on acceptance and display in the hierarchy
    - _Requirements: 3.1, 3.2, 3.9, 3.10_
  - [x]* 3.9 Write property test for folder name validity
    - **Property 7: Folder name validity**
    - **Validates: Requirements 3.1, 3.2, 3.9, 3.10**
  - [x] 3.10 Implement profile move and folder move with hierarchy integrity
    - Move profile: associate with destination and drop any prior association
    - Move folder: reject operations that exceed depth 10 or move a folder into its own descendant; keep hierarchy acyclic
    - _Requirements: 3.3, 3.4, 3.5_
  - [x]* 3.11 Write property test for single-folder association after move
    - **Property 8: Moving a profile yields exactly one folder association**
    - **Validates: Requirements 3.3**
  - [x]* 3.12 Write property test for hierarchy depth and acyclicity
    - **Property 9: Hierarchy integrity (depth and acyclicity)**
    - **Validates: Requirements 3.4, 3.5**
  - [x] 3.13 Implement folder deletion with confirmation and cascade
    - Request confirmation when a folder contains profiles; on confirm delete the folder and its entire subtree (nested folders and profiles), leaving everything outside the subtree unchanged; on cancel leave contents unchanged
    - _Requirements: 3.6, 3.7, 3.8_
  - [x]* 3.14 Write property test for confirmed subtree deletion
    - **Property 10: Confirmed folder deletion removes exactly its subtree**
    - **Validates: Requirements 3.7**
  - [x]* 3.15 Write property test for cancelled deletion no-op
    - **Property 11: Cancelled deletion is a no-op**
    - **Validates: Requirements 3.8**
  - [x]* 3.16 Write unit test for non-empty folder delete confirmation
    - Verify confirmation is requested before deleting a folder that contains profiles
    - _Requirements: 3.6_

- [x] 4. Checkpoint - core domain and persistence
  - Ensure all tests pass, ask the user if questions arise.

- [x] 5. Implement Credential_Store over OS Secure_Storage
  - [x] 5.1 Implement the Credential_Store wrapper over keyring-rs
    - `save`, `get`, `delete_for_profile` keyed by profile/credential key; never persist plaintext to disk; return structured `CredentialError` on failure
    - On save failure, keep the caller's in-memory value and leave any previously stored value unchanged; on missing/unretrievable get, return an error and no value
    - Wire `delete_for_profile` into profile deletion (task 3.5)
    - _Requirements: 1.3, 1.4, 1.5, 11.1, 11.2, 11.3, 11.4, 11.6_
  - [x]* 5.2 Write property test for credential-storage failure handling
    - **Property 2: Credential-storage failure preserves data and reports error** (mock store/clock)
    - **Validates: Requirements 1.5, 11.2**
  - [x]* 5.3 Write property test for missing-credential retrieval
    - **Property 30: Missing credential retrieval returns an error, never a value**
    - **Validates: Requirements 11.4**
  - [x]* 5.4 Write property test for credential removal on profile delete
    - **Property 32: Deleting a profile removes its credentials**
    - **Validates: Requirements 11.6**
  - [ ]* 5.5 Write integration tests for OS secure storage
    - macOS Keychain and Windows Credential Manager save/retrieve round-trips (1–3 representative cases)
    - _Requirements: 1.3, 1.4, 11.1, 11.3_

- [x] 6. Implement Key_Manager for SSH key generation and import
  - [x] 6.1 Implement key generation and import with validation
    - Generate iff key type supported and size within the type's valid range; otherwise reject with error and create no key
    - Import iff input is a valid SSH private key ≤ 1 MB; otherwise reject with error and store nothing; store encrypted-key passphrases via the Credential_Store
    - Expose the public key for copying
    - _Requirements: 12.1, 12.2, 12.3, 12.4, 12.5, 12.9_
  - [x]* 6.2 Write property test for key generation validity
    - **Property 33: SSH key generation validity**
    - **Validates: Requirements 12.1, 12.2**
  - [x]* 6.3 Write property test for key import validity
    - **Property 34: SSH key import validity**
    - **Validates: Requirements 12.3, 12.4**
  - [x]* 6.4 Write property test for encrypted-key passphrase storage
    - **Property 35: Encrypted key import stores the passphrase securely** (mock Secure_Storage)
    - **Validates: Requirements 12.5**
  - [x]* 6.5 Write property test for public-key correspondence
    - **Property 38: Public key corresponds to the managed pair**
    - **Validates: Requirements 12.9**
  - [x] 6.6 Implement key deletion with confirmation
    - Request confirmation before removal; on confirm remove the key and any associated passphrase from Secure_Storage; on cancel leave key and passphrase unchanged
    - _Requirements: 12.6, 12.7, 12.8_
  - [x]* 6.7 Write property test for confirmed key deletion
    - **Property 36: Confirmed key deletion removes key and passphrase**
    - **Validates: Requirements 12.7**
  - [x]* 6.8 Write property test for cancelled key deletion no-op
    - **Property 37: Cancelled key deletion is a no-op**
    - **Validates: Requirements 12.8**
  - [x]* 6.9 Write unit test for the key-delete confirmation prompt
    - Verify deletion requests confirmation before any removal
    - _Requirements: 12.6_

- [x] 7. Implement the uniform Protocol_Engine interface and registry
  - [x] 7.1 Define the Protocol / SessionConn traits and ProtocolRegistry
    - Implement the uniform `Protocol` trait (`kind`, `establish`) and `SessionConn` trait (`send`, `recv`, `resize`, `close`, `state`) applying unchanged to every Supported_Protocol
    - Implement `ProtocolRegistry` with `register` (rejects incomplete implementations, naming missing operations and excluding them from selectable options) and `selectable_kinds`
    - Ensure `establish` returning `Err` releases all allocated resources (RAII guards)
    - _Requirements: 8.1, 8.2, 8.3, 8.4_
  - [x]* 7.2 Write property test for registered-equals-selectable
    - **Property 19: Registered protocols equal selectable options**
    - **Validates: Requirements 8.2**
  - [x]* 7.3 Write property test for resource release on failed establish
    - **Property 20: Failed protocol establishment releases all resources** (mock Protocol/SessionConn)
    - **Validates: Requirements 8.3**
  - [x]* 7.4 Write property test for incomplete-registration rejection
    - **Property 21: Incomplete protocol registration is rejected and excluded**
    - **Validates: Requirements 8.4**
  - [x]* 7.5 Write unit test for the uniform-trait structural contract
    - Verify every protocol implements the uniform trait (8.1)
    - _Requirements: 8.1_

- [x] 8. Implement SSH protocol over russh
  - [x] 8.1 Implement SSH establish with port/timeout resolution and auth
    - Resolve port (configured or 22, in 1..=65535) and timeout (configured or 30, in 1..=300 s); authenticate via stored password creds or referenced SSH key
    - Abort with missing-credential error (disconnected, no session) when password auth is required but no credential exists; bound auth attempts to 3 then fail; abort on timeout
    - Present unknown host-key fingerprint for confirmation; on decline, terminate leaving the Session disconnected
    - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5, 4.6, 4.7, 4.8_
  - [x]* 8.2 Write property test for SSH port/timeout resolution
    - **Property 12: SSH port and timeout resolution**
    - **Validates: Requirements 4.1, 4.6**
  - [x]* 8.3 Write property test for missing-credentials abort
    - **Property 13: Missing credentials abort connection in disconnected state** (SSH + RDP, mock store)
    - **Validates: Requirements 4.3, 7.4**
  - [x]* 8.4 Write property test for bounded auth attempts
    - **Property 14: SSH authentication attempts are bounded** (mock auth responses)
    - **Validates: Requirements 4.5**
  - [x]* 8.5 Write property test for declined host-key abort
    - **Property 15: Declining an unknown host key aborts the connection** (mock host-key prompt)
    - **Validates: Requirements 4.8**
  - [x]* 8.6 Write unit tests for SSH auth wiring and host-key prompt
    - Password auth uses stored creds (4.2); key-ref profile uses key auth (4.4); unknown host-key fingerprint is presented (4.7)
    - _Requirements: 4.2, 4.4, 4.7_

- [x] 9. Implement Telnet protocol
  - [x] 9.1 Implement Telnet establish with timeout and error handling
    - Resolve timeout (configured or 30, in 1..=120 s); on success set state connected and surface host output; on timeout/refused/unresolved set disconnected with the appropriate error; reject invalid port (1..=65535)
    - _Requirements: 5.1, 5.2, 5.3, 5.4, 5.5_
  - [x]* 9.2 Write property test for Telnet timeout resolution
    - **Property 16: Telnet timeout resolution** (mock clock)
    - **Validates: Requirements 5.3**
  - [ ]* 9.3 Write integration tests for Telnet connect outcomes
    - Successful connect/output and connection-refused handling (5.1, 5.2, 5.4)
    - _Requirements: 5.1, 5.2, 5.4_

- [x] 10. Implement Serial protocol over serialport
  - [x] 10.1 Implement serial open with parameter validation and port enumeration
    - Open with configured baud/data-bits/parity/stop-bits; reject (retaining the profile) when any parameter is unset or out of its supported set, naming the offending parameter; abort when the configured port is unavailable
    - Enumerate available serial ports for profile configuration; show an empty list with a no-ports indication when none exist
    - _Requirements: 6.1, 6.2, 6.3, 6.4, 6.5_
  - [x]* 10.2 Write property test for serial parameter validity
    - **Property 17: Serial parameter validity**
    - **Validates: Requirements 6.2**
  - [x]* 10.3 Write unit test for the no-ports-available state
    - Verify an empty list and no-ports indication (6.4)
    - _Requirements: 6.4_
  - [ ]* 10.4 Write integration tests for serial open and enumeration
    - Open success, port enumeration, and unavailable-port abort (6.1, 6.3, 6.5)
    - _Requirements: 6.1, 6.3, 6.5_

- [x] 11. Implement RDP protocol over ironrdp
  - [x] 11.1 Implement RDP establish with resolution bounds, timeout, and auth
    - Validate resolution within 640x480..=7680x4320 (reject out-of-range with no connection); connect within 30 s or abort with no active Session; authenticate via stored creds, aborting (no Session) on missing creds or auth failure; request the specified resolution
    - _Requirements: 7.1, 7.2, 7.3, 7.4, 7.7, 7.8, 7.9_
  - [x]* 11.2 Write property test for RDP resolution bounds
    - **Property 18: RDP resolution bounds**
    - **Validates: Requirements 7.8, 7.9**
  - [x]* 11.3 Write unit test for RDP auth wiring
    - RDP auth uses stored creds (7.3)
    - _Requirements: 7.3_
  - [ ]* 11.4 Write integration tests for RDP connect/render/input
    - Connect/failure/auth-failure and render + input forwarding (7.1, 7.2, 7.5, 7.6, 7.7)
    - _Requirements: 7.1, 7.2, 7.5, 7.6, 7.7_

- [x] 12. Checkpoint - credentials, keys, and all four protocols
  - Ensure all tests pass, ask the user if questions arise.

- [x] 13. Implement Session_Manager with tabs, isolation, and capacity
  - [x] 13.1 Implement session open/activate/close with per-session tasks
    - Open a Session in a new active Tab via its own `tokio` read/write task set; enforce MAX_CONCURRENT (≥20) and reject over-limit opens while preserving existing Sessions; track each Session's independent state and output buffer
    - Activate a tab so exactly one tab is active; close a tab terminating the Session and releasing memory/network resources; require confirmation to close an active Session and preserve it on decline
    - _Requirements: 9.1, 9.2, 9.3, 9.4, 9.5, 9.6, 9.7_
  - [x]* 13.2 Write property test for session isolation
    - **Property 22: Session isolation** (mock SessionConn)
    - **Validates: Requirements 9.2**
  - [x]* 13.3 Write property test for bounded concurrency
    - **Property 23: Concurrent-session capacity is bounded**
    - **Validates: Requirements 9.3, 9.4**
  - [x]* 13.4 Write property test for exactly-one active tab
    - **Property 24: Exactly one active tab after selection**
    - **Validates: Requirements 9.5**
  - [x]* 13.5 Write property test for resource release on tab close
    - **Property 25: Closing a tab releases its session resources** (mock SessionConn)
    - **Validates: Requirements 9.6**
  - [x]* 13.6 Write unit test for active-tab close confirmation
    - Verify confirmation is requested and the Session preserved on decline (9.7)
    - _Requirements: 9.7_

- [x] 14. Implement Terminal_Emulator (ANSI parser + scrollback)
  - [x] 14.1 Implement the ANSI state machine, screen grid, and scrollback buffer
    - Parse the byte stream with `vte`, maintaining a screen grid that applies cursor positioning and foreground/background/bold/underline styling; discard malformed/unrecognized sequences and continue without corrupting state
    - Maintain a FIFO scrollback of ≥1000 lines, discarding oldest first; emit render instructions and report resize dimensions (cols/rows) to the remote
    - _Requirements: 10.3, 10.4, 10.5, 10.6_
  - [x]* 14.2 Write property test for ANSI interpretation vs reference model
    - **Property 26: ANSI interpretation matches the model**
    - **Validates: Requirements 10.3**
  - [x]* 14.3 Write property test for malformed-sequence tolerance
    - **Property 27: Malformed escape sequences are ignored without corruption**
    - **Validates: Requirements 10.4**
  - [x]* 14.4 Write property test for resize dimension reporting
    - **Property 28: Resize reports the new dimensions**
    - **Validates: Requirements 10.5**
  - [x]* 14.5 Write property test for bounded FIFO scrollback
    - **Property 29: Scrollback is a bounded FIFO buffer**
    - **Validates: Requirements 10.6**

- [x] 15. Implement File_Transfer_Service over SFTP/SCP
  - [x] 15.1 Implement remote listing, upload/download, progress, cancel, cleanup
    - List remote entries (name, size, modification time); upload/download via SFTP or SCP with progress (bytes, total, percentage) emitted at ≤1 s cadence; cancel within 2 s; on failure or cancel remove the incomplete destination file and report failed/canceled; request confirmation before overwriting an existing destination; on inaccessible remote directory report an error and keep the prior listing
    - _Requirements: 13.1, 13.2, 13.3, 13.4, 13.5, 13.6, 13.7, 13.8_
  - [x]* 15.2 Write property test for transfer progress correctness
    - **Property 39: Transfer progress is correct and monotonic**
    - **Validates: Requirements 13.4**
  - [x]* 15.3 Write property test for incomplete-transfer cleanup
    - **Property 40: Incomplete transfers clean up the destination**
    - **Validates: Requirements 13.5, 13.6**
  - [x]* 15.4 Write property test for failed-navigation listing preservation
    - **Property 41: Failed remote navigation preserves the current listing**
    - **Validates: Requirements 13.8**
  - [x]* 15.5 Write unit test for overwrite confirmation
    - Verify confirmation is requested before overwriting an existing destination (13.7)
    - _Requirements: 13.7_
  - [ ]* 15.6 Write integration tests for SFTP/SCP transfers
    - Remote listing with metadata, upload, and download (13.1, 13.2, 13.3)
    - _Requirements: 13.1, 13.2, 13.3_

- [x] 16. Implement Session_Logger
  - [x] 16.1 Implement timestamped session logging with enable/disable and validation
    - Record session I/O prefixed with an ISO-8601 timestamp (date, time to the second, timezone offset); start on connect when logging is profile-enabled; support enable/disable mid-session capturing only post-enable I/O; on disable close the file retaining written entries; on write failure retain prior entries and report an error; on invalid/unwritable destination report an error and do not open the Session with logging enabled
    - _Requirements: 14.1, 14.2, 14.3, 14.4, 14.5, 14.6_
  - [x]* 16.2 Write property test for post-enable-only capture
    - **Property 42: Mid-session logging captures only post-enable I/O**
    - **Validates: Requirements 14.2**
  - [x]* 16.3 Write property test for disable-preserves-entries
    - **Property 43: Disabling logging preserves written entries**
    - **Validates: Requirements 14.3**
  - [x]* 16.4 Write property test for ISO-8601 timestamp prefix
    - **Property 44: Log entries carry an ISO-8601 timestamp prefix**
    - **Validates: Requirements 14.4**
  - [x]* 16.5 Write property test for write-failure entry preservation
    - **Property 45: Log write failure preserves prior entries**
    - **Validates: Requirements 14.5**
  - [x]* 16.6 Write property test for invalid-destination handling
    - **Property 46: Invalid log destination prevents logging-enabled start**
    - **Validates: Requirements 14.6**
  - [ ]* 16.7 Write integration test for logging start on connect
    - Verify recording begins within 1 s of reaching connected (14.1)
    - _Requirements: 14.1_

- [x] 17. Implement Script_Engine
  - [x] 17.1 Implement sequential command sending with wait-for-prompt and outcomes
    - Send commands one at a time in defined order; for wait-for-prompt steps, hold the next command until the prompt text is received; resolve the wait timeout (configured or 30, in 1..=3600 s) and stop on timeout with remaining commands unsent and a timeout error; stop on disconnection with a disconnection error; report success with sent-count or failure with the stopping command index
    - _Requirements: 15.1, 15.2, 15.3, 15.4, 15.5, 15.6_
  - [x]* 17.2 Write property test for in-order command sending
    - **Property 47: Scripts send commands in order** (mock SessionConn)
    - **Validates: Requirements 15.1**
  - [x]* 17.3 Write property test for wait-for-prompt gating
    - **Property 48: Wait-for-prompt gates the next command**
    - **Validates: Requirements 15.2**
  - [x]* 17.4 Write property test for script timeout behavior
    - **Property 49: Script timeout stops execution with remaining commands unsent** (mock clock)
    - **Validates: Requirements 15.3**
  - [x]* 17.5 Write property test for disconnection-stops-execution
    - **Property 50: Disconnection during a script stops execution**
    - **Validates: Requirements 15.4**
  - [x]* 17.6 Write property test for completion reporting
    - **Property 51: Script completion reporting is accurate**
    - **Validates: Requirements 15.5, 15.6**

- [x] 18. Implement configuration import/export in Connection_Manager
  - [x] 18.1 Implement versioned export and additive import
    - Export all folders and profiles to a single versioned file excluding plaintext credentials (references only); import adds the file's folders/profiles with freshly generated identifiers (so colliding ids are added as new entries) without modifying or deleting existing items; reject malformed/non-conforming files leaving the configuration unchanged; report accurate added counts on success
    - _Requirements: 11.5, 16.1, 16.2, 16.3, 16.4, 16.5_
  - [x]* 18.2 Write property test for export containing no plaintext credentials
    - **Property 31: Exports contain no plaintext credentials**
    - **Validates: Requirements 11.5**
  - [x]* 18.3 Write property test for export/import round-trip and additivity
    - **Property 52: Export then import preserves and adds without disturbing existing data**
    - **Validates: Requirements 16.1, 16.2**
  - [x]* 18.4 Write property test for malformed-import rejection
    - **Property 53: Malformed import is rejected and leaves configuration unchanged**
    - **Validates: Requirements 16.3**
  - [x]* 18.5 Write property test for colliding-id import as new entries
    - **Property 54: Colliding identifiers are imported as new entries**
    - **Validates: Requirements 16.4**
  - [x]* 18.6 Write property test for accurate added counts
    - **Property 55: Import reports accurate added counts**
    - **Validates: Requirements 16.5**

- [x] 19. Checkpoint - all core components implemented
  - Ensure all tests pass, ask the user if questions arise.

- [x] 20. Implement the Tauri IPC bridge
  - [x] 20.1 Implement Tauri commands for all request/response operations
    - Expose commands (e.g., `save_profile`, `update_profile`, `delete_profile`, folder ops, `list_serial_ports`, `open_session`, `activate_tab`, `close_tab`, key ops, transfer ops, logging ops, `run_script`, `export_config`, `import_config`, `selectable_protocols`) returning structured `DomainError` payloads; keep all network/secret/serial access in the core
    - _Requirements: 8.1, 8.2_
  - [x] 20.2 Implement Tauri event streams for session output and transfer progress
    - Emit streamed events (`session://{id}/output`, `transfer://{id}/progress`) so the frontend renders output and progress without direct I/O access
    - _Requirements: 9.2, 13.4_

- [x] 21. Implement the React frontend
  - [x] 21.1 Implement the profile/folder tree and profile create/edit/validation UI
    - Render the folder hierarchy; profile create/edit forms with validation messages that identify the offending field; protocol selector populated from `selectable_protocols`; serial-port picker with no-ports indication; delete confirmations for profiles and folders
    - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.7, 3.1, 3.2, 3.6, 6.3, 6.4, 8.2_
  - [x] 21.2 Implement the tab bar and session views (terminal + RDP canvas)
    - Tab bar with active-tab tracking and active-session close confirmation; xterm.js terminal view fed by output events with resize wiring; RDP canvas view rendering IronRDP frames and forwarding keyboard/pointer input
    - _Requirements: 9.1, 9.5, 9.7, 10.1, 10.2, 10.5, 7.5, 7.6_
  - [x] 21.3 Implement file transfer, key management, logging, and script panels
    - File transfer panel (remote listing, upload/download, progress bars from progress events, cancel, overwrite confirmation); key manager UI (generate/import/view public key/delete with confirmation); per-session logging toggle; script runner with completion status display
    - _Requirements: 12.6, 12.9, 13.1, 13.4, 13.7, 14.2, 14.3, 15.5, 15.6_

- [x] 22. Apply Novrix branding, theme, and WCAG-compliant palette
  - [x] 22.1 Integrate brand fonts, logos, and the two-theme palette with contrast rules
    - Package the six Inter OTF weights (300–800) and the SVG logo variants as frontend assets; apply the dark/light palette (navy `#0F172A`, light BG `#F8FAFC`, white, steel gray) with the brand `N_` isotype; keep the UI flat (no shadows/bevels/3D)
    - Enforce the WCAG contrast rules: never use brand cyan `#06B6D4` as text or as a button fill under white text; use navy-on-cyan or white-on-`#0E7490` for action buttons on light backgrounds; restrict cyan accent to the navy background; use `#0E7490` for cyan text/affordances in the light theme
    - _Requirements: 1.1, 1.2_
  - [x] 22.2 Configure Tauri bundler application icons from brand assets
    - Wire `novrix-icon.ico` (16–256) and `novrix-icon.icns` (`ic07`–`ic14`) plus the loose app PNGs into the Tauri bundler icon configuration for Windows and macOS packaging
    - _Requirements: 1.1, 1.2_

- [x] 23. Configure signed, notarized release pipeline
  - [x] 23.1 Configure the tauri-action CI/CD release workflow with signing and notarization
    - Add a GitHub Actions workflow using `tauri-action` with a macOS runner (Developer ID signing + notarization + stapling, reading `APPLE_CERTIFICATE`/`APPLE_CERTIFICATE_PASSWORD`/`APPLE_SIGNING_IDENTITY` and the App Store Connect API key or `APPLE_ID`/`APPLE_PASSWORD`/`APPLE_TEAM_ID`) and a Windows runner (Azure Artifact Signing or commercial OV certificate) producing per-platform signed `.dmg`/`.msi`/`.exe` artifacts
    - Declare macOS entitlements for network client, serial/USB access, and Keychain so they are included in the signature
    - _Requirements: 1.1, 1.2, 1.3, 1.4_

- [x] 24. Final checkpoint, smoke and performance verification
  - [ ]* 24.1 Write smoke tests for platform startup
    - Application startup on macOS 12 and Windows 10 (1.1, 1.2)
    - _Requirements: 1.1, 1.2_
  - [ ]* 24.2 Write performance tests for timing and capacity bounds
    - Terminal render/keystroke ≤100 ms and resize notify ≤500 ms (10.1, 10.2, 10.5); new tab ≤2 s and tab switch ≤1 s (9.1, 9.5); credential save/retrieve ≤2 s (11.1, 11.3); newly registered protocol selectable ≤1 s (8.2); export of up to 10,000 profiles ≤5 s (16.1); transfer progress cadence ≤1 s (13.4); ≥20 concurrent Sessions sustained (9.3)
    - _Requirements: 8.2, 9.1, 9.3, 9.5, 10.1, 10.2, 10.5, 11.1, 11.3, 13.4, 16.1_
  - [x] 24.3 Final checkpoint - ensure all tests pass
    - Ensure all tests pass, ask the user if questions arise.

- [x] 25. Wire live transport backends (SSH, RDP, SFTP) and assemble into IPC
  - [x] 25.1 Wire the real russh SSH transport
    - Replace the `TODO(wiring)` stubs in `protocol/ssh.rs`: implement `SshProtocol::establish` over russh (config + `Handler::check_server_key` driving the `HostKeyVerifier`, timeout-bounded connect, auth per `AuthPlan` inside the 3-attempt budget, PTY channel), and `SshSession` send/recv/resize/close over the live channel; only hand the transport to the session on the success path
    - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5, 4.6, 4.7, 4.8, 8.3, 9.2_
  - [x] 25.2 Wire the real ironrdp RDP transport
    - Replace the stubbed `IronRdpConnector`/`RdpTransport` in `protocol/rdp.rs` with the real ironrdp connect + security negotiation + authentication under the 30 s budget, and a live transport whose send_input/next_update/request_resolution/shutdown drive the ironrdp active session; preserve the resolution-bounds and no-half-open-session guarantees
    - _Requirements: 7.1, 7.2, 7.4, 7.7, 7.8, 7.9, 8.3_
  - [x] 25.3 Wire the real russh-sftp file-transfer backend
    - Replace the `TODO(wiring)` transfer seam with a russh-sftp backend over an SSH channel that lists remote directories and uploads/downloads with byte-level progress feeding the existing copy_loop; keep overwrite/conflict policy and incomplete-destination cleanup intact
    - _Requirements: 13.1, 13.4, 13.7, 15.5, 15.6_
  - [x] 25.4 Assemble transports into the IPC layer
    - In `ipc/mod.rs`: implement real `SecretResolver`/`SshKeyResolver`/`HostKeyVerifier` adapters over the Credential_Store/Key_Manager (and a known-hosts store), the real `RdpCredentialSource`/`RdpConnector`, and the SFTP backend binding; register SSH and RDP in `build_registry` and route them in `open_session` so SSH/RDP/SFTP no longer return the not-wired error
    - _Requirements: 8.2, 4.2, 7.4, 13.1_

- [x] 26. Close transport-wiring gaps (key auth, login identity, host trust, RDP NLA/resize, transfer cancel & scripts)
  - [x] 26.1 Add login identity (SSH username, RDP username/domain) to the data model
    - Extend `ConnectionProfile`/`ProfileDraft` and the `ProfileDraftDto` with an optional SSH/RDP `username` (and optional RDP `domain`); thread it through persistence (schema + migration), Connection_Manager validation, and the IPC adapters so `SshIdentity` and the RDP `ResolvedCredential` read the real login identity instead of the `"root"`/`"Administrator"` fallbacks; surface the fields in the React profile form
    - _Requirements: 2.1, 2.5, 4.2, 7.3_
  - [x] 26.2 Persist SSH private-key material and expose it to the key resolver
    - Store imported/generated private-key material at rest (encrypted via the SecretStore/keyring, not in the clear) in the Key_Manager, add a private-key retrieval accessor, and wire `IpcSshKeyResolver` to it so key-ref SSH profiles authenticate; keep passphrase handling and the no-partial-state-on-failure guarantees
    - _Requirements: 4.4, 12.3, 12.4, 12.5_
  - [x] 26.3 Enforce host-key / certificate trust with a prompt channel and known-hosts store
    - Add a persistent known-hosts (SSH) / known-certs (RDP) store and a UI prompt event seam; replace the trust-on-first-use `TofuHostKeyVerifier` and the accept-any RDP TLS verifier with real verification that prompts on an unknown key/cert, persists the decision, and surfaces a declined/changed key as `HostKeyDeclined`; honor the profile `HostKeyPolicy`
    - _Requirements: 4.7, 4.8_
  - [x] 26.4 Complete RDP NLA network client and mid-session resize
    - Provide a real `AsyncNetworkClient` adapter so domain Kerberos/KDC-proxy CredSSP works (not just NTLM password), and implement mid-session resolution renegotiation (deactivation-reactivation sequence) so `request_resolution` is no longer best-effort-only; keep the dedicated-session-thread architecture and no-half-open guarantee
    - _Requirements: 7.1, 7.4, 7.7_
  - [x] 26.5 Make transfers cancellable and wire the live script runner
    - Make `upload_file`/`download_file` non-blocking with a per-session in-flight transfer-handle registry so `cancel_transfer` really cancels (≤2 s) and cleans up; wire `run_script` to drive a live session's `SessionConn` through the Session_Manager so scripts execute instead of returning the not-wired error
    - _Requirements: 13.6, 14.2, 14.3_

- [ ] 27. Credential entry, visual polish, and command snippets (Requirements 17, 18, 19)
  - [ ] 27.1 Add a save_credential IPC command and store entered passwords
    - Expose a `save_credential(profile_id, password: Option<String>)` Tauri command over the Credential_Store: `Some(non-empty)` saves to Secure_Storage under the profile's password key, `Some("")`/cleared removes the stored credential, `None` leaves it unchanged; add a `has_credential(profile_id)` presence check (never returns the secret); on storage failure retain the entered value and surface the error, leaving the prior stored secret unchanged; never write the secret to SQLite or exports
    - _Requirements: 17.1, 17.4, 17.5, 17.6, 17.7_
  - [ ] 27.2 Add a masked password field to the profile editor
    - In the React profile form, replace the raw "credential key reference" input with a real password field (masked, reveal toggle) shown for password auth; on save call `save_credential`; when editing a profile with a stored credential show a "credential stored" indicator (from `has_credential`) without displaying the secret; wire the TS IPC client
    - _Requirements: 17.1, 17.2, 17.3, 17.4, 17.5_
  - [ ] 27.3 Polish the visual experience across the shell
    - Unify the sidebar/editor/session layout and the control/field/dialog styles against the Novrix palette+typography tokens; standardize the dismissible error banner and a uniform async busy state; ensure visible focus indicators and WCAG-AA contrast in both themes; keep the persisted dark/light toggle; add an empty-state guidance panel
    - _Requirements: 18.1, 18.2, 18.3, 18.4, 18.5, 18.6_
  - [ ] 27.4 Implement the Command_Snippet model, persistence, and manager
    - Add `CommandSnippet`/`SnippetDraft`/`SnippetId` to the domain model; a `snippets` table with a schema migration; a `SnippetManager` (create/update/delete-with-confirmation/list) over the SQLite repository with name/command non-empty validation; unit tests for validation and round-trip persistence
    - _Requirements: 19.1, 19.2, 19.3, 19.7_
  - [ ] 27.5 Wire snippet IPC commands and the snippet UI
    - Add `save_snippet`/`update_snippet`/`delete_snippet`/`list_snippets` Tauri commands on the Connection_Manager actor thread and their TS wrappers; add a snippets panel/affordance in the session area that, for an active text-based session, sends the snippet's command via `send_input` (newline-terminated); offer a protocol-restricted snippet only for matching SSH/Telnet/Serial sessions; show the required-active-session indication when none applies
    - _Requirements: 19.3, 19.4, 19.5, 19.6_

## Notes

- Tasks marked with `*` are optional and can be skipped for a faster MVP; they are the property-based, unit, integration, smoke, and performance tests.
- Each property-based test uses `proptest` with a minimum of 100 iterations and is tagged `// Feature: remote-connection-manager, Property N: {property_text}`. Protocol-, store-, and clock-dependent properties use in-memory mocks per the design's Testing Strategy.
- Every one of the 55 correctness properties maps to exactly one property-test subtask; discrete interactions map to unit tests, OS/network boundaries to integration/smoke tests, and timing/capacity to performance tests.
- Each task references the specific requirement clauses it implements for traceability.
- Checkpoints provide incremental validation at natural boundaries (domain/persistence, protocols, full core, and final verification).

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1"] },
    { "id": 1, "tasks": ["1.2"] },
    { "id": 2, "tasks": ["1.3", "2.1", "7.1"] },
    { "id": 3, "tasks": ["1.4", "2.2", "5.1", "6.1", "7.2", "7.3", "7.4", "7.5", "14.1"] },
    { "id": 4, "tasks": ["2.3", "3.1", "3.8", "3.10", "5.2", "5.3", "5.4", "5.5", "6.2", "6.6", "8.1", "9.1", "10.1", "11.1", "14.2", "14.3", "14.4", "14.5"] },
    { "id": 5, "tasks": ["3.2", "3.3", "3.4", "3.5", "3.9", "3.11", "3.12", "6.3", "6.4", "6.5", "6.7", "6.8", "6.9", "8.2", "8.3", "8.4", "8.5", "8.6", "9.2", "9.3", "10.2", "10.3", "10.4", "11.2", "11.3", "11.4", "13.1", "15.1", "16.1", "18.1"] },
    { "id": 6, "tasks": ["3.6", "3.7", "3.13", "13.2", "15.2", "16.2", "18.2"] },
    { "id": 7, "tasks": ["3.14", "3.15", "3.16", "13.3", "13.4", "13.5", "13.6", "15.3", "15.4", "15.5", "15.6", "16.3", "16.4", "16.5", "16.6", "16.7", "18.3", "18.4", "18.5", "18.6"] },
    { "id": 8, "tasks": ["20.1", "20.2"] },
    { "id": 9, "tasks": ["21.1", "21.2", "21.3"] },
    { "id": 10, "tasks": ["22.1", "22.2"] },
    { "id": 11, "tasks": ["23.1"] },
    { "id": 12, "tasks": ["24.1", "24.2"] },
    { "id": 13, "tasks": ["25.1", "25.2", "25.3", "25.4"] },
    { "id": 14, "tasks": ["26.1", "26.2", "26.3", "26.4", "26.5"] },
    { "id": 15, "tasks": ["27.1", "27.2", "27.3", "27.4", "27.5"] }
  ]
}
```
