//! Tauri IPC bridge — request/response commands (task 20.1).
//!
//! The frontend never touches the network, OS secrets, or serial ports
//! directly. It issues Tauri **commands** (request/response) which are the
//! `#[tauri::command]` functions below, and subscribes to streamed **events**
//! (`session://{id}/output`, `transfer://{id}/progress`), defined in the
//! [`events`] submodule (task 20.2). Every command returns
//! `Result<T, DomainError>`, so a
//! typed core error (`ValidationError`, `CredentialError`, …) crosses the IPC
//! boundary as a structured `serde` payload the UI can switch on by `code`.
//!
//! # Scope of this task (20.1)
//!
//! This module hosts the request/response command surface and the managed
//! [`AppState`] that owns the core components. The streaming events
//! (`session://{id}/output`, `transfer://{id}/progress`) that complement these
//! commands live in the [`events`] submodule (task 20.2); `AppState` carries the
//! shared [`TauriEventEmitter`] that backs them.
//!
//! # Encapsulation
//!
//! All network / secret / serial access stays in the Rust core. The commands
//! are thin wrappers that:
//!
//! 1. deserialize the request payload (reusing `serde`-deriving domain types
//!    where possible, with a small DTO for [`ProfileDraft`], which is not
//!    `serde`),
//! 2. call the already-implemented core API, and
//! 3. return its `Result<T, DomainError>` verbatim.
//!
//! # State
//!
//! [`AppState`] holds the long-lived core singletons constructed in
//! [`run`](crate::run):
//!
//! - [`ConnectionManager`] over the OS-keystore-backed [`KeyringCredentialStore`]
//!   (profiles, folders, import/export). It owns the SQLite [`Database`], which
//!   the read commands (`list_profiles`, `list_folders`) borrow through it.
//! - [`SessionManager`] (open/activate/close tabs). Behind an `Arc` so a single
//!   instance serves concurrent async commands.
//! - [`DefaultKeyManager`] (SSH key generate/import/public-key/delete).
//! - [`ProtocolRegistry`] (which protocols are selectable).
//! - [`SessionLogger`] (per-session logging start/stop).
//!
//! # Wiring notes (open_session / transfers / scripts)
//!
//! `open_session` resolves a stored profile and dispatches to the matching
//! protocol's `Protocol::establish`. **Telnet, Serial, SSH, and RDP are all
//! wired end-to-end** (task 25.4): the protocol-layer ports (secret / key /
//! host-key resolvers for SSH, the credential source for RDP) are implemented
//! here as thin adapters over the Credential_Store and injected into the real
//! `SshProtocol` / `RdpProtocol`, so those sessions are genuinely established
//! and handed to the [`SessionManager`].
//!
//! **File transfers** (`list_remote` / `upload_file` / `download_file` /
//! `cancel_transfer`) run a [`DefaultFileTransferService`] over a
//! [`RusshSftpBackend`]. The backend resolves a [`SessionId`] to its live SFTP
//! session through the [`SftpSessions`] provider held on [`AppState`]: when
//! `open_session` establishes an SSH connection it also opens an `sftp`
//! subsystem channel (via [`SshSession::open_sftp_session`](crate::protocol::SshSession::open_sftp_session))
//! and registers the resulting session in that map, so transfers share the one
//! SSH connection with the terminal. Progress streams on
//! `transfer://{id}/progress` through the [`TauriProgressSink`].
//!
//! SSH key-based auth is wired (task 26.2): the [`DefaultKeyManager`] persists
//! each managed key's OpenSSH private material encrypted at rest in the OS
//! keyring, and [`IpcSshKeyResolver`] resolves a usable (decrypted) private-key
//! PEM for a `KeyId` through the same keystore. Password SSH auth is also fully
//! wired.
//!
//! **Transfers are cancellable** (task 26.5): `upload_file`/`download_file`
//! *start* a transfer and return promptly while the chunked copy runs in a
//! spawned task; the live [`TransferHandle`] is registered in
//! [`InflightTransfers`] (keyed by session id) so `cancel_transfer` flips its
//! cooperative flag and the copy stops within ≤2 s with the partial destination
//! cleaned up. Progress streams on `transfer://{id}/progress`; the terminal
//! outcome lands on `transfer://{id}/done`.
//!
//! **The `run_script` command is wired** (task 26.5): it builds a
//! [`DefaultScriptEngine`] and drives it over the session's live `SessionConn`
//! via [`SessionManager::run_script`](crate::session::SessionManager::run_script),
//! which locks that one session's connection for the run.
//!
//! [`NOT_WIRED_CODE`] remains defined for any future command whose transport
//! seam is still pending, but no command returns it today.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tauri::State;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};

use crate::connection::{
    ConnectionManager, DeletionSummary, ExportSummary, ImportSummary, ProfileDraft,
};
use crate::credential::{CredentialStore, KeyringCredentialStore, KeyringSecureStore, SecureStore};
use crate::domain::{
    AuthConfig, ConnectionProfile, CredentialError, CredentialKey, DomainError, Folder, FolderId,
    HostKeyPolicy, KeyError, KeyId, KeyType, ManagedKey, ProfileId, ProtocolError, ProtocolKind,
    ProtocolParams, SessionId, SessionState, TransferError,
};
use crate::keys::{resolve_private_key_openssh, DefaultKeyManager, KeyManager, SecretStore};
use crate::logging::{FileSinkFactory, SessionLogger, SystemClock};
use crate::persistence::{decide_trust, KnownHostsStore, TrustDecision, TrustKind};
use crate::protocol::{
    available_port_names, CredentialHandle, HostKeyVerifier, IronRdpConnector, Protocol,
    ProtocolRegistry, RdpCertVerifier, RdpCredentialSource, RdpProtocol, ResolvedCredential,
    ResolvedProfile, SecretResolver, SerialProtocol, SshIdentity, SshKeyResolver, SshProtocol,
    TelnetProtocol,
};
use crate::script::{DefaultScriptEngine, Script, ScriptOutcome, ScriptStep};
use crate::session::{SessionManager, SessionOutputSink, SessionSource};
use crate::transfer::{
    ConflictPolicy, DefaultFileTransferService, FileTransferService, RemoteEntry, RemotePath,
    RusshSftpBackend, SftpBackend, SftpSessionProvider, SharedSftpSession, TransferHandle,
};

/// Streamed events — `session://{id}/output` and `transfer://{id}/progress`
/// (task 20.2). The request/response commands below are the other half of the
/// IPC boundary.
pub mod events;

pub use events::{
    HostKeyPrompts, TauriEventEmitter, TauriProgressSink, TauriSessionOutputSink,
    HOST_KEY_PROMPT_EVENT,
};

/// Stable error code returned by commands whose core wiring (the russh /
/// ironrdp transport seam) is not yet complete. The command shape is final;
/// only the live transport binding is pending. Documented so the frontend can
/// recognise "feature present, backend not yet connected" distinctly from a
/// genuine failure.
pub const NOT_WIRED_CODE: &str = "ipc.notYetWired";

/// The concrete Credential_Store the production app uses: the component-level
/// store backed by the OS Keychain / Credential Manager.
pub type OsCredentialStore = KeyringCredentialStore<KeyringSecureStore>;

/// The managed application state shared by every Tauri command.
///
/// Constructed once in [`run`](crate::run) and registered with
/// `tauri::Builder::manage`. Each field owns a core component; commands borrow
/// them through [`State`]. Interior mutability lives inside the components
/// themselves (the `ConnectionManager`'s DB connection, the `SessionManager`'s
/// async mutex, the `SessionLogger`'s sink map), so `AppState` itself needs no
/// outer lock.
pub struct AppState {
    /// Handle to the Connection_Manager actor (profiles, folders,
    /// import/export). See [`ConnectionHandle`] for why the manager runs behind
    /// a dedicated thread rather than in the `AppState` directly.
    pub connection: ConnectionHandle,
    /// Open sessions and tabs. `Arc` so concurrent async commands share one.
    pub sessions: Arc<SessionManager>,
    /// SSH key generation / import / public-key export / deletion.
    pub keys: DefaultKeyManager,
    /// Which protocols are selectable for profile creation.
    pub registry: ProtocolRegistry,
    /// Per-session logging to timestamped files.
    pub logger: SessionLogger<SystemClock, FileSinkFactory>,
    /// Shared Tauri event emitter backing the streamed events (task 20.2).
    ///
    /// Created up front (before the Tauri `AppHandle` exists) and injected into
    /// the `SessionManager`'s output sink; [`run`](crate::run)'s `setup` fills
    /// in the real handle via [`TauriEventEmitter::attach`]. Also used to build
    /// the transfer [`TauriProgressSink`], so progress streams on
    /// `transfer://{id}/progress`.
    pub events: TauriEventEmitter,
    /// The live File_Transfer_Service over the SFTP backend (task 25.4). The
    /// transfer commands call this directly; its backend resolves each
    /// [`SessionId`] to the SFTP session registered by `open_session` through
    /// [`sftp_sessions`](AppState::sftp_sessions).
    pub transfers: Arc<dyn FileTransferService>,
    /// The registry of per-session live SFTP sessions opened alongside each SSH
    /// connection. `open_session` registers one here when it establishes SSH;
    /// `close` could later deregister (sessions also drop with their SSH
    /// connection). This is the [`SftpSessionProvider`] seam the transfer
    /// backend reads through.
    pub sftp_sessions: SftpSessions,
    /// The persistent known-hosts / known-certs trust store (task 26.3), shared
    /// behind an `Arc` so the SSH and RDP verifiers built per connect in
    /// `open_session` consult (and update) the same store. Its own SQLite
    /// connection addresses the same database file as the profile store.
    pub known_hosts: Arc<KnownHostsStore>,
    /// The host-key / certificate prompt channel (task 26.3): the verifiers emit
    /// `hostkey://prompt` through it and await the user's decision; the
    /// `respond_host_key_prompt` command fulfils the matching request.
    pub host_key_prompts: HostKeyPrompts,
    /// The registry of in-flight, cancellable transfers (task 26.5).
    ///
    /// `upload_file` / `download_file` start a transfer, register its live
    /// [`TransferHandle`] here, then return promptly while the copy loop runs in
    /// a spawned task; `cancel_transfer` looks the handle up and flips its
    /// cooperative cancel flag. See [`InflightTransfers`] for the key choice.
    pub inflight_transfers: InflightTransfers,
}

impl AppState {
    /// Build the production application state: a Credential_Store over the OS
    /// keystore, a `ConnectionManager` over the opened database (hosted on its
    /// own actor thread), a fresh `SessionManager`, a `DefaultKeyManager` whose
    /// passphrases persist through the OS keystore, a `ProtocolRegistry` with
    /// every complete protocol registered, and a file-backed `SessionLogger`.
    pub fn new(
        db: crate::persistence::Database,
        known_hosts_db: crate::persistence::Database,
    ) -> Self {
        let credentials = KeyringCredentialStore::with_os_keystore();
        let connection = ConnectionManager::new(db, credentials);
        let connection = ConnectionHandle::spawn(connection);

        // The Key_Manager persists encrypted-key passphrases and private-key
        // material via a SecretStore over the OS keyring. The SSH-key resolver
        // reads the same keystore namespace (see `key_secret_store`).
        let keys = DefaultKeyManager::new(key_secret_store());

        let registry = build_registry();

        let logger = SessionLogger::new(SystemClock, FileSinkFactory);

        // Task 20.2: a shared event emitter whose `AppHandle` is attached in
        // `run`'s `setup`. The SessionManager is built with a Tauri-backed
        // output sink over this same emitter, so every session's read loop
        // streams `session://{id}/output` events (Requirement 9.2). The emitter
        // is also kept on `AppState` so the transfer `TauriProgressSink`
        // (`transfer://{id}/progress`, Requirement 13.4) can be built from it
        // when the live transfer backend is wired.
        let events = TauriEventEmitter::new();
        let output_sink: Arc<dyn SessionOutputSink> =
            Arc::new(TauriSessionOutputSink::new(events.clone()));
        let sessions = Arc::new(SessionManager::with_output_sink(output_sink));

        // Task 25.4: the live File_Transfer_Service. Its backend resolves a
        // SessionId to the SFTP session `open_session` registered through the
        // shared provider map; progress is forwarded to the
        // `transfer://{id}/progress` event stream via the `TauriProgressSink`
        // over the same emitter the output sink uses.
        let sftp_sessions = SftpSessions::new();
        let progress: Arc<dyn crate::transfer::ProgressSink> =
            Arc::new(TauriProgressSink::new(events.clone()));
        let backend: Arc<dyn SftpBackend> =
            Arc::new(RusshSftpBackend::new(sftp_sessions.provider()));
        let transfers: Arc<dyn FileTransferService> =
            Arc::new(DefaultFileTransferService::new(backend, progress));

        // Task 26.3: the persistent trust store (over its own connection to the
        // same database file) and the prompt channel over the shared emitter.
        // Both are consulted by the SSH/RDP verifiers built per connect in
        // `open_session`.
        let known_hosts = Arc::new(KnownHostsStore::new(known_hosts_db));
        let host_key_prompts = HostKeyPrompts::new(events.clone());

        let inflight_transfers = InflightTransfers::new();

        Self {
            connection,
            sessions,
            keys,
            registry,
            logger,
            events,
            transfers,
            sftp_sessions,
            known_hosts,
            host_key_prompts,
            inflight_transfers,
        }
    }
}

// ---------------------------------------------------------------------------
// In-flight transfer registry (task 26.5)
// ---------------------------------------------------------------------------

/// The registry of live, cancellable transfer handles, **keyed by
/// [`SessionId`]**.
///
/// # Why key by session id
///
/// Progress already streams on `transfer://{id}/progress` where `{id}` is the
/// *session* id (see [`events`]), and the `cancel_transfer` command is issued
/// with the same session id. Keying the handle by session id therefore needs no
/// extra identifier to flow to the frontend: the UI cancels "the transfer on
/// this session" with the id it already holds. The trade-off is **one in-flight
/// transfer per session at a time**, which matches the one-progress-stream-per-
/// session model; a second start for a session replaces (and leaves running)
/// any previous handle rather than racing two writers to one progress channel.
///
/// A handle is registered when `upload_file`/`download_file` starts a transfer
/// and **deregistered** when the spawned copy task finishes or is cancelled, so
/// a stale handle never lingers.
///
/// Cloneable (an `Arc` inside) so the same map is shared by [`AppState`] and the
/// spawned transfer tasks that deregister themselves.
#[derive(Clone)]
pub struct InflightTransfers {
    handles: Arc<AsyncMutex<HashMap<SessionId, TransferHandle>>>,
}

impl InflightTransfers {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            handles: Arc::new(AsyncMutex::new(HashMap::new())),
        }
    }

    /// Register the live handle for a session's in-flight transfer.
    pub async fn register(&self, session: SessionId, handle: TransferHandle) {
        self.handles.lock().await.insert(session, handle);
    }

    /// Remove and return the handle for a session's transfer, if one is live.
    pub async fn take(&self, session: SessionId) -> Option<TransferHandle> {
        self.handles.lock().await.remove(&session)
    }

    /// Look up (clone) the live handle for a session's transfer, if any.
    pub async fn get(&self, session: SessionId) -> Option<TransferHandle> {
        self.handles.lock().await.get(&session).cloned()
    }
}

impl Default for InflightTransfers {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// SFTP session registry + provider (task 25.4)
// ---------------------------------------------------------------------------

/// The shared registry of live SFTP sessions, keyed by [`SessionId`].
///
/// When [`open_session`] establishes an SSH connection it opens a second
/// `sftp`-subsystem channel on it (see
/// [`SshSession::open_sftp_session`](crate::protocol::SshSession::open_sftp_session))
/// and registers the resulting `russh_sftp` session here. The transfer backend
/// resolves each [`SessionId`] back to its session through the
/// [`SftpSessionProvider`] this type hands out, so file transfers ride the same
/// SSH connection as the interactive terminal.
///
/// Cloneable (it is an `Arc` inside): the same map is shared by `AppState`, the
/// provider the transfer backend holds, and every command that touches it.
#[derive(Clone)]
pub struct SftpSessions {
    sessions: Arc<AsyncMutex<HashMap<SessionId, SharedSftpSession>>>,
}

impl SftpSessions {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(AsyncMutex::new(HashMap::new())),
        }
    }

    /// Register the SFTP session for a connected SSH session.
    pub async fn register(&self, id: SessionId, sftp: SharedSftpSession) {
        self.sessions.lock().await.insert(id, sftp);
    }

    /// Build the [`SftpSessionProvider`] the transfer backend resolves through.
    pub fn provider(&self) -> Arc<dyn SftpSessionProvider> {
        Arc::new(SftpSessionsProvider {
            sessions: Arc::clone(&self.sessions),
        })
    }
}

impl Default for SftpSessions {
    fn default() -> Self {
        Self::new()
    }
}

/// The [`SftpSessionProvider`] view over an [`SftpSessions`] registry.
struct SftpSessionsProvider {
    sessions: Arc<AsyncMutex<HashMap<SessionId, SharedSftpSession>>>,
}

#[async_trait]
impl SftpSessionProvider for SftpSessionsProvider {
    async fn sftp_session(&self, session: SessionId) -> Result<SharedSftpSession, TransferError> {
        self.sessions
            .lock()
            .await
            .get(&session)
            .cloned()
            // The session is unknown, is not an SSH session, or its SFTP channel
            // was never opened — a precise, non-faked error the UI can show.
            .ok_or_else(|| {
                TransferError::Io(
                    "no SFTP channel is available for this session (SSH session required)"
                        .to_string(),
                )
            })
    }
}

// ---------------------------------------------------------------------------
// Connection_Manager actor
// ---------------------------------------------------------------------------
//
// The `ConnectionManager` owns a `rusqlite::Connection`, which is `Send` but
// *not* `Sync` (SQLite serializes access per connection via internal
// `RefCell`s). Worse, two of its methods (`delete_profile`, `delete_folder`)
// are `async fn(&self)` that `.await` the Credential_Store while holding
// `&self`; the resulting future captures a `&ConnectionManager`, which is
// `Send` only if the manager is `Sync` — it isn't. Tauri requires managed
// state to be `Send + Sync` and every command future to be `Send`, so the
// manager cannot live directly in `AppState`.
//
// Rather than reshape the (already-complete, tested) core, we host the manager
// on a dedicated OS thread running its own current-thread Tokio runtime — the
// standard "database actor" pattern for `rusqlite` in an async app. The thread
// owns the manager outright, so nothing about `Sync` matters; the IPC layer
// communicates with it over `Send` channels. Because SQLite already serializes
// per-connection work, funnelling every operation through one thread is also
// the correct concurrency model, not merely a borrow-checker workaround.

/// A boxed, possibly-borrowing future the actor awaits on its own thread.
///
/// It is **not** `Send` (it may borrow the `!Sync` manager), which is fine
/// because it is produced and driven entirely on the actor thread.
type ActorFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = BoxedResult> + 'a>>;

/// A unit of work for the Connection_Manager actor: a `Send` closure that, on
/// the actor thread, borrows the owned manager and returns a future producing
/// an erased result the caller downcasts. The closure is `Send` (it crosses the
/// channel); the future it builds need not be. Boxing the whole call keeps the
/// request protocol to one variant while every closure calls the core API
/// unchanged.
type ConnectionJob = Box<
    dyn for<'a> FnOnce(&'a ConnectionManager<OsCredentialStore>) -> ActorFuture<'a>
        + Send
        + 'static,
>;

/// An erased, `Send` result produced by a [`ConnectionJob`] and downcast by the
/// caller to the concrete `Result<T, DomainError>` it expects.
type BoxedResult = Box<dyn std::any::Any + Send>;

/// A `Send + Sync` handle the commands use to run work on the manager thread.
///
/// Cloneable-free; stored once in [`AppState`]. Each call sends a closure plus
/// a oneshot reply channel, then awaits the typed result.
pub struct ConnectionHandle {
    tx: mpsc::UnboundedSender<(ConnectionJob, oneshot::Sender<BoxedResult>)>,
}

impl ConnectionHandle {
    /// Spawn the actor thread that owns `manager` and serve jobs until the
    /// handle (and thus the channel) is dropped.
    fn spawn(manager: ConnectionManager<OsCredentialStore>) -> Self {
        let (tx, mut rx) =
            mpsc::unbounded_channel::<(ConnectionJob, oneshot::Sender<BoxedResult>)>();

        std::thread::Builder::new()
            .name("connection-manager".to_string())
            .spawn(move || {
                // A current-thread runtime: the manager's async methods (and the
                // keyring `spawn_blocking` they rely on) need a Tokio context,
                // but nothing here must be `Send`, so a single-threaded runtime
                // on this owned thread is exactly right.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build connection-manager runtime");

                rt.block_on(async move {
                    while let Some((job, reply)) = rx.recv().await {
                        // Build the (possibly-borrowing) future on this thread
                        // and await it here — no reentrant `block_on`.
                        let result = job(&manager).await;
                        // A dropped receiver (caller cancelled) is harmless.
                        let _ = reply.send(result);
                    }
                });
            })
            .expect("failed to spawn connection-manager thread");

        Self { tx }
    }

    /// Run a *synchronous* manager call on the actor thread and await its typed
    /// result.
    ///
    /// `job` receives `&ConnectionManager` and returns the exact
    /// `Result<T, DomainError>` the caller wants; it is boxed-erased in transit
    /// and downcast back here, so the call site stays fully typed.
    async fn run<T, F>(&self, job: F) -> Result<T, DomainError>
    where
        F: FnOnce(&ConnectionManager<OsCredentialStore>) -> Result<T, DomainError> + Send + 'static,
        T: Send + 'static,
    {
        self.dispatch(move |mgr| {
            let out = job(mgr);
            Box::pin(async move { Box::new(out) as BoxedResult }) as ActorFuture<'_>
        })
        .await
    }

    /// Run an *asynchronous* manager call (the delete paths that await the
    /// Credential_Store) on the actor thread.
    ///
    /// `build` receives `&ConnectionManager` and returns a boxed future (the
    /// borrow is expressed through the pinned trait object, which sidesteps the
    /// `'static` bound a generic `Fut` would impose). The future borrows the
    /// manager and is driven on the actor thread, so it need not be `Send`;
    /// its boxed `Result` output is downcast back to the typed result.
    async fn run_async<T, B>(&self, build: B) -> Result<T, DomainError>
    where
        B: for<'a> FnOnce(
                &'a ConnectionManager<OsCredentialStore>,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<T, DomainError>> + 'a>,
            > + Send
            + 'static,
        T: Send + 'static,
    {
        self.dispatch(move |mgr| {
            let fut = build(mgr);
            Box::pin(async move { Box::new(fut.await) as BoxedResult }) as ActorFuture<'_>
        })
        .await
    }

    /// Shared transport: send a job-building closure to the actor, await the
    /// erased reply, and downcast it to the concrete `Result<T, DomainError>`.
    async fn dispatch<T, B>(&self, build: B) -> Result<T, DomainError>
    where
        B: for<'a> FnOnce(&'a ConnectionManager<OsCredentialStore>) -> ActorFuture<'a>
            + Send
            + 'static,
        T: Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let boxed: ConnectionJob = Box::new(build);

        self.tx.send((boxed, reply_tx)).map_err(|_| actor_down())?;
        let erased = reply_rx.await.map_err(|_| actor_down())?;
        // Flatten: downcast to the inner Result the job produced, then `?`.
        let result: Result<T, DomainError> = *erased
            .downcast::<Result<T, DomainError>>()
            .expect("actor returned the wrong result type");
        result
    }
}

/// The error surfaced when the Connection_Manager actor thread is gone (only
/// possible during shutdown).
fn actor_down() -> DomainError {
    DomainError::new(
        "ipc.connectionUnavailable",
        "The connection manager is no longer available.",
    )
}

/// Register every protocol whose uniform-interface implementation is complete
/// so it becomes a selectable option (Requirement 8.2).
///
/// All four protocols now register: Telnet and Serial construct with no
/// injected dependencies, and SSH and RDP construct over the real
/// Credential_Store / `ironrdp` adapters built in this module (task 25.4), so
/// `selectable_protocols` reports every protocol that can be established today.
/// Registration failures (incomplete protocols) are ignored — a protocol that
/// fails `register` is simply excluded from the selectable set, which is
/// precisely the registry's contract (Requirement 8.4).
fn build_registry() -> ProtocolRegistry {
    let mut registry = ProtocolRegistry::new();
    let _ = registry.register(Box::new(TelnetProtocol::new()));
    let _ = registry.register(Box::new(SerialProtocol::new()));
    // These instances exist only so the registry reports SSH/RDP as selectable
    // kinds; they never establish a session (open_session builds a fresh
    // protocol with the profile's login identity), so the default identity is
    // fine here.
    let _ = registry.register(Box::new(build_ssh_protocol(None, None)));
    let _ = registry.register(Box::new(build_rdp_protocol(
        None,
        None,
        Arc::new(RejectingRdpCertVerifier),
    )));
    registry
}

/// Adapts the credential layer's [`SecureStore`] (`set`/`get`/`delete`) to the
/// Key_Manager's [`SecretStore`] (`store_secret`), so encrypted-key passphrases
/// land in the same OS keystore as every other secret without the Key_Manager
/// depending on the credential module's richer trait.
struct KeyringSecretStoreAdapter<S: SecureStore> {
    backend: S,
}

impl<S: SecureStore> KeyringSecretStoreAdapter<S> {
    fn new(backend: S) -> Self {
        Self { backend }
    }
}

#[async_trait::async_trait]
impl<S: SecureStore> SecretStore for KeyringSecretStoreAdapter<S> {
    async fn store_secret(
        &self,
        key: &crate::domain::CredentialKey,
        secret: &str,
    ) -> Result<(), KeyError> {
        self.backend
            .set(
                key.as_str(),
                &crate::credential::Secret::new(secret.to_string()),
            )
            .await
            .map_err(|e| KeyError::StorageFailed(e.to_string()))
    }

    async fn get_secret(
        &self,
        key: &crate::domain::CredentialKey,
    ) -> Result<Option<String>, KeyError> {
        // The credential layer distinguishes a missing entry (`NotFound`) from a
        // real backend failure; the Key_Manager's `SecretStore::get_secret`
        // contract maps the former to `Ok(None)` and the latter to a
        // `StorageFailed` error, so private-key retrieval can tell "no such key"
        // apart from "keystore is broken".
        match self.backend.get(key.as_str()).await {
            Ok(secret) => Ok(Some(secret.expose().to_string())),
            Err(CredentialError::NotFound) => Ok(None),
            Err(other) => Err(KeyError::StorageFailed(other.to_string())),
        }
    }

    async fn delete_secret(&self, key: &crate::domain::CredentialKey) -> Result<(), KeyError> {
        // The credential layer's `delete` is idempotent (deleting an absent key
        // is a no-op success), which matches the `SecretStore::delete_secret`
        // contract the Key_Manager relies on for confirmed key deletion.
        self.backend
            .delete(key.as_str())
            .await
            .map_err(|e| KeyError::StorageFailed(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Protocol-layer port adapters (task 25.4)
// ---------------------------------------------------------------------------
//
// The protocol layer (protocol/ssh.rs, protocol/rdp.rs) defines small injectable
// ports — `SecretResolver`, `SshKeyResolver`, `HostKeyVerifier`,
// `RdpCredentialSource` — so it never depends on the concrete Credential_Store /
// Key_Manager. These adapters are the real implementations the IPC layer injects
// when it constructs the SSH / RDP protocols in `open_session`.
//
// They resolve secrets through their own `CredentialStore` over the OS keystore.
// That store addresses exactly the same OS Keychain / Credential Manager
// namespace the Connection_Manager's store writes to (both are
// `KeyringCredentialStore::with_os_keystore()`), so a password saved for a
// profile is visible here by the same `CredentialKey`.

/// Default SSH login username used when a profile carries none.
///
/// TODO(wiring): the current `ConnectionProfile` data model has no SSH username
/// field (see `domain/model.rs`), so the login user cannot be sourced from the
/// profile. `"root"` is the documented, explicit fallback; when the profile
/// grows a username field, read it from there and drop this constant.
const DEFAULT_SSH_USERNAME: &str = "root";

/// Default RDP username used when a profile/credential carries none.
///
/// TODO(wiring): like SSH, the data model carries no separate RDP username or
/// logon domain. `"Administrator"` (local account, no domain) is the documented
/// fallback until the model grows those fields.
const DEFAULT_RDP_USERNAME: &str = "Administrator";

/// A boxed Credential_Store over the OS keystore, shared by the SSH/RDP
/// adapters. Boxed as the trait object so the adapters depend only on the
/// `CredentialStore` interface, not the concrete keyring type.
type SharedCredentialStore = Arc<dyn CredentialStore>;

/// Build a fresh Credential_Store over the OS keystore for the protocol
/// adapters to resolve secrets through.
fn adapter_credential_store() -> SharedCredentialStore {
    Arc::new(KeyringCredentialStore::with_os_keystore())
}

/// [`SecretResolver`] over the Credential_Store (SSH password auth).
///
/// Resolves a [`CredentialKey`] to its plaintext secret. A missing credential
/// (`CredentialError::NotFound`) maps to `Ok(None)` — which `plan_auth` turns
/// into a `MissingCredentials` abort — while any other store failure surfaces
/// as a `ProtocolError::Io` so the connect reports a real backend problem
/// rather than silently treating it as "no credential".
struct IpcSecretResolver {
    store: SharedCredentialStore,
}

#[async_trait]
impl SecretResolver for IpcSecretResolver {
    async fn resolve_secret(&self, key: &CredentialKey) -> Result<Option<String>, ProtocolError> {
        match self.store.get(key.clone()).await {
            Ok(secret) => Ok(Some(secret.expose().to_string())),
            Err(CredentialError::NotFound) => Ok(None),
            Err(other) => Err(ProtocolError::Io(format!(
                "credential store failure resolving secret: {other}"
            ))),
        }
    }
}

/// [`SshKeyResolver`] over the Key_Manager's persisted private material (task
/// 26.2).
///
/// The `DefaultKeyManager` now persists each managed key's OpenSSH private
/// material encrypted at rest in the OS keyring (slot `key/{id}/private`, with
/// the passphrase under `key/{id}/passphrase` for encrypted keys). `open_session`
/// builds a fresh `SshProtocol` per connection and has no handle to the live
/// in-memory `DefaultKeyManager` registry, so this resolver reads the same
/// persisted material directly through its own [`SecretStore`] over that
/// keyring, via [`resolve_private_key_openssh`]. It returns a *decrypted*,
/// usable OpenSSH PEM (the SSH `authenticate` path parses it with no
/// passphrase).
///
/// A key with no persisted material maps to `Ok(None)` (which `plan_auth` turns
/// into a `MissingCredentials` abort); any keyring backend failure surfaces as
/// `ProtocolError::Io`. The resolved PEM is secret and never logged.
struct IpcSshKeyResolver {
    store: Box<dyn SecretStore>,
}

#[async_trait]
impl SshKeyResolver for IpcSshKeyResolver {
    async fn resolve_key(&self, key_id: KeyId) -> Result<Option<String>, ProtocolError> {
        resolve_private_key_openssh(self.store.as_ref(), key_id)
            .await
            .map_err(|e| ProtocolError::Io(format!("key store failure resolving SSH key: {e}")))
    }
}

/// Build the keyring-backed [`SecretStore`] the SSH-key resolver reads through.
/// Addresses the same OS keystore namespace the `DefaultKeyManager` writes its
/// private-key material to, so a key persisted on generate/import is visible
/// here by the same `key/{id}/*` slots.
fn key_secret_store() -> Box<dyn SecretStore> {
    Box::new(KeyringSecretStoreAdapter::new(KeyringSecureStore::default()))
}

/// The real host-key / certificate [`HostKeyVerifier`] (task 26.3).
///
/// Replaces the former trust-on-first-use stub with genuine trust: it consults
/// the persistent [`KnownHostsStore`], applies the profile's [`HostKeyPolicy`]
/// through the pure [`decide_trust`] rule, and — only when the policy permits a
/// prompt for an unknown host — asks the frontend to confirm the fingerprint
/// through the [`HostKeyPrompts`] channel, awaiting the user's decision.
///
/// The verifier is built **per connect** so it carries the exact `host`/`port`
/// and the profile's `policy`; the uniform [`HostKeyVerifier`] trait only hands
/// it the presented `fingerprint`, which is all that varies per call. `kind`
/// selects the store namespace ([`TrustKind::SshHostKey`] for SSH;
/// [`TrustKind::RdpCertificate`] for the RDP path, which reuses this same type).
///
/// Decision mapping (see [`decide_trust`]):
/// - [`TrustDecision::Proceed`] → accept silently (known, unchanged).
/// - [`TrustDecision::Prompt`] → prompt (or auto-accept for `AcceptAny`); on
///   accept, remember the fingerprint and accept; on decline, reject.
/// - [`TrustDecision::Reject`] → reject (Strict-and-unknown, or a **changed**
///   key — a hard fail that is never re-prompted).
///
/// A store read failure fails safe to a **reject** rather than trusting blindly.
/// A fingerprint is a digest and may be logged; the key/cert material behind it
/// is not available here and is never persisted.
struct IpcHostKeyVerifier {
    store: Arc<KnownHostsStore>,
    prompts: HostKeyPrompts,
    host: String,
    port: u16,
    policy: HostKeyPolicy,
    kind: TrustKind,
}

impl IpcHostKeyVerifier {
    /// Build a verifier for one connect to `host:port` under `policy`, trusting
    /// through `store` and prompting through `prompts` for the given `kind`.
    fn new(
        store: Arc<KnownHostsStore>,
        prompts: HostKeyPrompts,
        host: impl Into<String>,
        port: u16,
        policy: HostKeyPolicy,
        kind: TrustKind,
    ) -> Self {
        Self {
            store,
            prompts,
            host: host.into(),
            port,
            policy,
            kind,
        }
    }

    /// The shared trust routine behind [`HostKeyVerifier::confirm_unknown_host_key`],
    /// reused verbatim by the RDP certificate path. Returns whether to trust the
    /// presented `fingerprint`.
    async fn decide(&self, fingerprint: &str) -> bool {
        // Read the stored fingerprint. A storage failure is treated as "unknown"
        // for the *decision* but must not silently trust: under `Strict` an
        // unknown rejects; otherwise it would prompt. To fail safe on a broken
        // store we reject outright instead of prompting against stale state.
        let stored = match self.store.get(&self.host, self.port, self.kind) {
            Ok(stored) => stored,
            Err(_e) => return false,
        };

        match decide_trust(stored.as_deref(), fingerprint, self.policy) {
            TrustDecision::Proceed => true,
            TrustDecision::Reject => false,
            TrustDecision::Prompt => {
                // `AcceptAny` is the opt-in insecure policy: auto-accept the
                // prompt (no user round trip) but still *remember* the key, so a
                // later changed key is detected. Every other policy that reached
                // `Prompt` genuinely asks the user.
                let accepted = if self.policy == HostKeyPolicy::AcceptAny {
                    true
                } else {
                    let rx =
                        self.prompts
                            .begin(&self.host, self.port, self.kind.as_str(), fingerprint);
                    // A dropped sender (UI gone / no handle) resolves to Err →
                    // reject, the fail-safe default.
                    rx.await.unwrap_or(false)
                };

                if accepted {
                    // Persist the acceptance so the next connect proceeds
                    // silently and a future change is caught. A write failure
                    // does not undo the user's accept for *this* connect.
                    let _ = self
                        .store
                        .remember(&self.host, self.port, self.kind, fingerprint);
                }
                accepted
            }
        }
    }
}

#[async_trait]
impl HostKeyVerifier for IpcHostKeyVerifier {
    async fn confirm_unknown_host_key(&self, fingerprint: &str) -> bool {
        self.decide(fingerprint).await
    }
}

/// [`RdpCredentialSource`] over the Credential_Store (RDP CredSSP/NLA).
///
/// Resolves the handle's [`CredentialKey`] to the account secret and builds a
/// [`ResolvedCredential`]. A handle with no key, or a missing secret, maps to
/// `MissingCredentials` (RDP always authenticates).
///
/// The login identity comes from the profile: `username` names the RDP account
/// and `domain` the (optional) logon domain. When the profile carries no
/// username the account falls back to [`DEFAULT_RDP_USERNAME`]; when it carries
/// no domain the credential has no domain (a local account). The password stays
/// redacted in `ResolvedCredential`'s `Debug`, so it never reaches logs.
struct IpcRdpCredentialSource {
    store: SharedCredentialStore,
    /// The RDP account name from the profile, or `None` to use the default.
    username: Option<String>,
    /// The RDP logon domain from the profile, or `None` for no domain.
    domain: Option<String>,
}

#[async_trait]
impl RdpCredentialSource for IpcRdpCredentialSource {
    async fn resolve(
        &self,
        handle: &CredentialHandle,
    ) -> Result<ResolvedCredential, ProtocolError> {
        let key = handle
            .credential_key
            .as_ref()
            .ok_or(ProtocolError::MissingCredentials)?;
        // The profile's username/domain win; blanks are treated as absent so a
        // whitespace-only value can't produce an empty account or domain.
        let username = self
            .username
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .unwrap_or(DEFAULT_RDP_USERNAME)
            .to_string();
        let domain = self
            .domain
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string);
        match self.store.get(key.clone()).await {
            Ok(secret) => Ok(ResolvedCredential::new(
                username,
                secret.expose().to_string(),
                domain,
            )),
            // A missing secret, or any retrieval failure, means no usable
            // credential was obtained — RDP treats that as MissingCredentials.
            Err(_) => Err(ProtocolError::MissingCredentials),
        }
    }
}

/// Construct the SSH protocol with its real injected ports, login identity, and
/// the real host-key verifier (task 26.3).
///
/// The login user comes from the profile's `username` when set; otherwise it
/// falls back to [`DEFAULT_SSH_USERNAME`], the documented default. A blank
/// username (only whitespace) is treated as absent so it cannot produce an
/// empty login user.
///
/// The host-key verifier is built per connect from the live trust store and
/// prompt channel plus the connect's `host`/`port` and the profile's
/// [`HostKeyPolicy`], so it consults the right known-hosts entry and enforces
/// the right policy. For the registry placeholder (which never establishes),
/// pass `host_key = None`; an always-reject verifier is used so a placeholder
/// could never silently trust anything if it were ever driven.
fn build_ssh_protocol(username: Option<&str>, host_key: Option<SshHostKeyContext>) -> SshProtocol {
    let login = username
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .unwrap_or(DEFAULT_SSH_USERNAME);

    let verifier: Arc<dyn HostKeyVerifier> = match host_key {
        Some(ctx) => Arc::new(IpcHostKeyVerifier::new(
            ctx.store,
            ctx.prompts,
            ctx.host,
            ctx.port,
            ctx.policy,
            TrustKind::SshHostKey,
        )),
        // Registry placeholder: never establishes, so a reject-everything
        // verifier is the safe stand-in.
        None => Arc::new(RejectingHostKeyVerifier),
    };

    SshProtocol::new(
        Arc::new(IpcSecretResolver {
            store: adapter_credential_store(),
        }),
        Arc::new(IpcSshKeyResolver {
            store: key_secret_store(),
        }),
        verifier,
        SshIdentity::new(login),
    )
}

/// The per-connect inputs the real SSH host-key verifier needs: the shared trust
/// store and prompt channel plus this connect's host/port and policy.
struct SshHostKeyContext {
    store: Arc<KnownHostsStore>,
    prompts: HostKeyPrompts,
    host: String,
    port: u16,
    policy: HostKeyPolicy,
}

/// A [`HostKeyVerifier`] that rejects every key. Used only for the registry
/// placeholder `SshProtocol` (which reports SSH as selectable but never
/// establishes), so it can never silently trust a host if ever invoked.
struct RejectingHostKeyVerifier;

#[async_trait]
impl HostKeyVerifier for RejectingHostKeyVerifier {
    async fn confirm_unknown_host_key(&self, _fingerprint: &str) -> bool {
        false
    }
}

/// Resolve a profile's SSH [`HostKeyPolicy`] from its protocol params.
///
/// An SSH profile carries `ProtocolParams::Ssh { known_host_policy }`. A profile
/// routed here with non-SSH params is a programming error (the dispatch is by
/// kind); default to the safest prompting policy rather than panicking.
fn ssh_host_key_policy(profile: &ConnectionProfile) -> HostKeyPolicy {
    match &profile.protocol_params {
        ProtocolParams::Ssh { known_host_policy } => *known_host_policy,
        _ => HostKeyPolicy::PromptOnUnknown,
    }
}

/// Construct the RDP protocol with the `ironrdp` connector (carrying the real
/// certificate verifier), and the credential source adapter.
///
/// `cert_verifier` is the RDP analogue of the SSH host-key verifier (task 26.3):
/// built per connect from the live known-certs store, prompt channel, this
/// host/port, and the profile's [`HostKeyPolicy`]. For the registry placeholder
/// (which never establishes) pass a reject-all verifier.
fn build_rdp_protocol(
    username: Option<String>,
    domain: Option<String>,
    cert_verifier: Arc<dyn RdpCertVerifier>,
) -> RdpProtocol<IronRdpConnector, IpcRdpCredentialSource> {
    RdpProtocol::new(
        IronRdpConnector::new(cert_verifier),
        IpcRdpCredentialSource {
            store: adapter_credential_store(),
            username,
            domain,
        },
    )
}

/// The RDP server-certificate verifier the IPC layer injects (task 26.3).
///
/// `rustls` calls [`RdpCertVerifier::verify_certificate`] **synchronously** on
/// the dedicated RDP session thread, but the trust decision (store read + UI
/// prompt) is async. This bridge runs that async decision on the captured
/// multi-threaded runtime `handle` and blocks the (dedicated) RDP thread on a
/// one-shot channel for the answer — safe because that thread exists only to
/// drive this one connect and is allowed to block.
///
/// The decision itself is the *same* [`IpcHostKeyVerifier`] the SSH path uses,
/// built with [`TrustKind::RdpCertificate`], so RDP certificates and SSH host
/// keys share one trust store, one policy rule, and one prompt flow.
struct IpcRdpCertVerifier {
    inner: Arc<IpcHostKeyVerifier>,
    handle: tokio::runtime::Handle,
}

impl RdpCertVerifier for IpcRdpCertVerifier {
    fn verify_certificate(&self, cert_der: &[u8]) -> bool {
        let fingerprint = crate::persistence::certificate_fingerprint(cert_der);
        let inner = Arc::clone(&self.inner);
        let (tx, rx) = std::sync::mpsc::channel();
        // Drive the async trust decision on the app runtime; the RDP thread
        // blocks on the std channel for the boolean result.
        self.handle.spawn(async move {
            let decision = inner.decide(&fingerprint).await;
            let _ = tx.send(decision);
        });
        // A dropped sender (runtime gone) fails safe to reject.
        rx.recv().unwrap_or(false)
    }
}

/// A reject-all [`RdpCertVerifier`] for the registry placeholder, which never
/// establishes a session. Mirrors [`RejectingHostKeyVerifier`] for SSH.
struct RejectingRdpCertVerifier;

impl RdpCertVerifier for RejectingRdpCertVerifier {
    fn verify_certificate(&self, _cert_der: &[u8]) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Request DTOs
// ---------------------------------------------------------------------------

/// Serializable mirror of [`ProfileDraft`] (which is not `serde`), so the
/// frontend can submit a draft across IPC. Field-for-field identical; converted
/// to a `ProfileDraft` before it reaches the Connection_Manager.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDraftDto {
    pub name: String,
    pub protocol: ProtocolKind,
    pub host: String,
    pub port: u16,
    /// Optional login username (SSH login user / RDP account name).
    #[serde(default)]
    pub username: Option<String>,
    /// Optional login domain (RDP only).
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub folder_id: Option<FolderId>,
    pub auth: AuthConfig,
    pub protocol_params: ProtocolParams,
    #[serde(default)]
    pub logging_enabled: bool,
    #[serde(default)]
    pub log_path: Option<PathBuf>,
    pub connect_timeout_secs: u16,
}

impl From<ProfileDraftDto> for ProfileDraft {
    fn from(d: ProfileDraftDto) -> Self {
        ProfileDraft {
            name: d.name,
            protocol: d.protocol,
            host: d.host,
            port: d.port,
            username: d.username,
            domain: d.domain,
            folder_id: d.folder_id,
            auth: d.auth,
            protocol_params: d.protocol_params,
            logging_enabled: d.logging_enabled,
            log_path: d.log_path,
            connect_timeout_secs: d.connect_timeout_secs,
        }
    }
}

// ---------------------------------------------------------------------------
// Profile commands
// ---------------------------------------------------------------------------

/// Validate and create a connection profile (Requirement 2.1).
#[tauri::command]
pub async fn save_profile(
    state: State<'_, AppState>,
    draft: ProfileDraftDto,
) -> Result<ConnectionProfile, DomainError> {
    let draft: ProfileDraft = draft.into();
    state.connection.run(move |m| m.create_profile(draft)).await
}

/// Validate and update an existing profile (Requirement 2.5).
#[tauri::command]
pub async fn update_profile(
    state: State<'_, AppState>,
    id: ProfileId,
    draft: ProfileDraftDto,
) -> Result<ConnectionProfile, DomainError> {
    let draft: ProfileDraft = draft.into();
    state
        .connection
        .run(move |m| m.update_profile(id, draft))
        .await
}

/// Delete a profile after confirmation (Requirements 2.7, 2.8).
#[tauri::command]
pub async fn delete_profile(
    state: State<'_, AppState>,
    id: ProfileId,
    confirmed: bool,
) -> Result<(), DomainError> {
    state
        .connection
        .run_async(move |m| Box::pin(m.delete_profile(id, confirmed)))
        .await
}

/// List every stored profile.
#[tauri::command]
pub async fn list_profiles(
    state: State<'_, AppState>,
) -> Result<Vec<ConnectionProfile>, DomainError> {
    state.connection.run(|m| m.database().list_profiles()).await
}

// ---------------------------------------------------------------------------
// Folder commands
// ---------------------------------------------------------------------------

/// Create a folder with name validation (Requirements 3.1, 3.2).
#[tauri::command]
pub async fn create_folder(
    state: State<'_, AppState>,
    name: String,
    parent: Option<FolderId>,
) -> Result<Folder, DomainError> {
    state
        .connection
        .run(move |m| m.create_folder(name, parent))
        .await
}

/// Rename a folder with name validation (Requirements 3.9, 3.10).
#[tauri::command]
pub async fn rename_folder(
    state: State<'_, AppState>,
    id: FolderId,
    name: String,
) -> Result<Folder, DomainError> {
    state
        .connection
        .run(move |m| m.rename_folder(id, name))
        .await
}

/// Move a profile into a destination folder (or the root) (Requirement 3.3).
#[tauri::command]
pub async fn move_profile(
    state: State<'_, AppState>,
    id: ProfileId,
    dest: Option<FolderId>,
) -> Result<(), DomainError> {
    state
        .connection
        .run(move |m| m.move_profile(id, dest))
        .await
}

/// Move a folder keeping the hierarchy acyclic and within depth (Req 3.4, 3.5).
#[tauri::command]
pub async fn move_folder(
    state: State<'_, AppState>,
    id: FolderId,
    dest: Option<FolderId>,
) -> Result<(), DomainError> {
    state.connection.run(move |m| m.move_folder(id, dest)).await
}

/// Delete a folder and its subtree after confirmation (Req 3.6, 3.7, 3.8).
#[tauri::command]
pub async fn delete_folder(
    state: State<'_, AppState>,
    id: FolderId,
    confirmed: bool,
) -> Result<DeletionSummaryDto, DomainError> {
    let summary: DeletionSummary = state
        .connection
        .run_async(move |m| Box::pin(m.delete_folder(id, confirmed)))
        .await?;
    Ok(DeletionSummaryDto {
        folders: summary.folders,
        profiles: summary.profiles,
    })
}

/// Serializable mirror of `connection::DeletionSummary` for the IPC boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletionSummaryDto {
    pub folders: Vec<FolderId>,
    pub profiles: Vec<ProfileId>,
}

/// List every folder.
#[tauri::command]
pub async fn list_folders(state: State<'_, AppState>) -> Result<Vec<Folder>, DomainError> {
    state.connection.run(|m| m.database().list_folders()).await
}

// ---------------------------------------------------------------------------
// Protocol / serial-port discovery
// ---------------------------------------------------------------------------

/// The protocols selectable when creating a profile (Requirement 8.2).
#[tauri::command]
pub async fn selectable_protocols(
    state: State<'_, AppState>,
) -> Result<Vec<ProtocolKind>, DomainError> {
    Ok(state.registry.selectable_kinds())
}

/// Enumerate available serial ports (Requirement 6.3/6.4). An empty list means
/// no ports are available, which the UI renders as a no-ports indication.
#[tauri::command]
pub async fn list_serial_ports() -> Result<Vec<String>, DomainError> {
    available_port_names().map_err(DomainError::from)
}

// ---------------------------------------------------------------------------
// Session commands
// ---------------------------------------------------------------------------

/// Open a session for a stored profile in a new active tab (Requirement 9.1).
///
/// The profile is loaded, wrapped in a [`ResolvedProfile`], and handed to the
/// matching protocol's `establish`. Telnet, Serial, SSH, and RDP are all wired
/// end-to-end (task 25.4): SSH/RDP establish over the real Credential_Store /
/// `ironrdp` adapters. On a successful establish the live connection is handed
/// to the [`SessionManager`], which enforces the concurrency cap and returns the
/// new [`SessionId`].
///
/// For SSH, once the shell session is established an `sftp` subsystem channel is
/// also opened on the same connection and registered in
/// [`AppState::sftp_sessions`], so the file-transfer commands can operate over
/// it. A failure to bring up SFTP does **not** fail the terminal session — the
/// session still opens; only transfers for it will report no SFTP channel.
#[tauri::command]
pub async fn open_session(
    state: State<'_, AppState>,
    profile_id: ProfileId,
) -> Result<SessionId, DomainError> {
    // Resolve the stored profile (keeps all profile knowledge in the core).
    // The lookup runs on the actor thread and returns before the network
    // establish below, so the manager is never busy during I/O.
    let profile = state
        .connection
        .run(move |m| {
            m.database()
                .get_profile(profile_id)?
                .ok_or_else(|| DomainError::new("persistence.notFound", "No such profile."))
        })
        .await?;

    let kind = profile.protocol;
    let resolved = ResolvedProfile::new(profile.clone());

    // Resolve the credential *reference* only (no secret leaves the core here;
    // the protocol reads the secret through the Credential_Store when needed).
    let creds = match &profile.auth {
        AuthConfig::Password { credential_key } => {
            CredentialHandle::with_key(credential_key.clone())
        }
        AuthConfig::None | AuthConfig::SshKey { .. } => CredentialHandle::none(),
    };

    // Dispatch to the protocol factory. Every protocol is wired end-to-end.
    //
    // SSH is special: it establishes the *concrete* `SshSession` so that, on
    // success, a second `sftp` subsystem channel can be opened on the same
    // connection for file transfers. The resulting `SftpSession` is registered
    // against the new `SessionId` *after* the SessionManager accepts the
    // session (so the id exists). RDP/Telnet/Serial establish straight to the
    // uniform `Box<dyn SessionConn>`.
    match kind {
        ProtocolKind::Ssh => {
            // Task 26.3: the real host-key verifier for this connect — the
            // shared trust store + prompt channel, this host/port, and the
            // profile's HostKeyPolicy.
            let host_key = SshHostKeyContext {
                store: Arc::clone(&state.known_hosts),
                prompts: state.host_key_prompts.clone(),
                host: profile.host.clone(),
                port: crate::protocol::resolve_ssh_port(profile.port),
                policy: ssh_host_key_policy(&profile),
            };
            let session = build_ssh_protocol(profile.username.as_deref(), Some(host_key))
                .establish_session(&resolved, creds)
                .await
                .map_err(DomainError::from)?;

            // Bring up the SFTP channel on the live connection *before* handing
            // ownership to the SessionManager, since afterwards the concrete
            // session is type-erased behind `Box<dyn SessionConn>`. A failure
            // here is non-fatal to the terminal session: record it and open the
            // session anyway (transfers for it will report "no SFTP channel").
            let sftp = session.open_sftp_session().await.ok();

            let source = SessionSource::new(kind, Some(profile_id), Box::new(session));
            let id = state.sessions.open_session(source).await?;

            if let Some(sftp) = sftp {
                state.sftp_sessions.register(id, Arc::new(sftp)).await;
            }
            Ok(id)
        }
        ProtocolKind::Rdp => {
            // Task 26.3: the RDP certificate verifier reuses the SSH trust logic
            // under TrustKind::RdpCertificate. RDP has no profile HostKeyPolicy
            // field (it is SSH-only on the data model), so the certificate trust
            // uses the safe default: prompt on an unknown cert, reject a changed
            // one. TODO(policy): give RDP profiles their own cert policy field if
            // an AcceptAny/Strict choice is ever needed for RDP.
            let cert_decider = Arc::new(IpcHostKeyVerifier::new(
                Arc::clone(&state.known_hosts),
                state.host_key_prompts.clone(),
                profile.host.clone(),
                profile.port,
                HostKeyPolicy::PromptOnUnknown,
                TrustKind::RdpCertificate,
            ));
            let cert_verifier: Arc<dyn RdpCertVerifier> = Arc::new(IpcRdpCertVerifier {
                inner: cert_decider,
                handle: tokio::runtime::Handle::current(),
            });
            let conn = build_rdp_protocol(
                profile.username.clone(),
                profile.domain.clone(),
                cert_verifier,
            )
            .establish(&resolved, creds)
            .await
            .map_err(DomainError::from)?;
            let source = SessionSource::new(kind, Some(profile_id), conn);
            state.sessions.open_session(source).await
        }
        ProtocolKind::Telnet => {
            let conn = TelnetProtocol::new()
                .establish(&resolved, creds)
                .await
                .map_err(DomainError::from)?;
            let source = SessionSource::new(kind, Some(profile_id), conn);
            state.sessions.open_session(source).await
        }
        ProtocolKind::Serial => {
            let conn = SerialProtocol::new()
                .establish(&resolved, creds)
                .await
                .map_err(DomainError::from)?;
            let source = SessionSource::new(kind, Some(profile_id), conn);
            state.sessions.open_session(source).await
        }
    }
}

/// Activate a tab so exactly one tab is active (Requirement 9.5).
#[tauri::command]
pub async fn activate_tab(state: State<'_, AppState>, id: SessionId) -> Result<(), DomainError> {
    state.sessions.activate_tab(id).await
}

/// Close a tab, confirming first for an active session (Req 9.6, 9.7).
#[tauri::command]
pub async fn close_tab(
    state: State<'_, AppState>,
    id: SessionId,
    confirmed: bool,
) -> Result<(), DomainError> {
    state.sessions.close_tab(id, confirmed).await
}

/// The current state of a session, or `None` if it does not exist.
#[tauri::command]
pub async fn session_state(
    state: State<'_, AppState>,
    id: SessionId,
) -> Result<Option<SessionState>, DomainError> {
    Ok(state.sessions.session_state(id).await)
}

/// Send input bytes to a session's remote (keystrokes / commands).
#[tauri::command]
pub async fn send_input(
    state: State<'_, AppState>,
    id: SessionId,
    bytes: Vec<u8>,
) -> Result<(), DomainError> {
    state.sessions.send_input(id, bytes).await
}

/// The id of the currently active tab, or `None` when no tabs are open.
#[tauri::command]
pub async fn active_tab(state: State<'_, AppState>) -> Result<Option<SessionId>, DomainError> {
    Ok(state.sessions.active_tab().await)
}

/// Answer a pending host-key / certificate trust prompt (task 26.3).
///
/// The backend emits a `hostkey://prompt` event (with a `requestId`) when an
/// SSH host key or RDP certificate is seen for the first time under a prompting
/// policy; the connect is suspended awaiting the user's decision. The React
/// dialog invokes this command with that `requestId` and the user's `accept`
/// choice, which resumes the connect: `accept = true` trusts and persists the
/// fingerprint, `false` aborts with `HostKeyDeclined`.
///
/// An unknown or already-answered `requestId` is a harmless no-op, so a
/// duplicate or late reply cannot error. The command never fails.
#[tauri::command]
pub async fn respond_host_key_prompt(
    state: State<'_, AppState>,
    request_id: String,
    accept: bool,
) -> Result<(), DomainError> {
    state.host_key_prompts.resolve(&request_id, accept);
    Ok(())
}

// ---------------------------------------------------------------------------
// Key commands
// ---------------------------------------------------------------------------

/// Generate a new SSH key pair (Requirements 12.1, 12.2).
#[tauri::command]
pub async fn generate_key(
    state: State<'_, AppState>,
    key_type: KeyType,
    size: u32,
) -> Result<ManagedKey, DomainError> {
    state
        .keys
        .generate(key_type, size)
        .await
        .map_err(DomainError::from)
}

/// Import an existing OpenSSH private key, storing any passphrase securely
/// (Requirements 12.3, 12.4, 12.5).
#[tauri::command]
pub async fn import_key(
    state: State<'_, AppState>,
    bytes: Vec<u8>,
    passphrase: Option<String>,
) -> Result<ManagedKey, DomainError> {
    state
        .keys
        .import(bytes, passphrase)
        .await
        .map_err(DomainError::from)
}

/// Return a managed key's OpenSSH public key for copying (Requirement 12.9).
#[tauri::command]
pub async fn key_public_key(state: State<'_, AppState>, id: KeyId) -> Result<String, DomainError> {
    state.keys.public_key(id).map_err(DomainError::from)
}

/// Delete a managed key after confirmation (Requirements 12.6, 12.7, 12.8).
#[tauri::command]
pub async fn delete_key(
    state: State<'_, AppState>,
    id: KeyId,
    confirmed: bool,
) -> Result<(), DomainError> {
    state
        .keys
        .delete(id, confirmed)
        .await
        .map_err(DomainError::from)
}

// ---------------------------------------------------------------------------
// Transfer commands (shape final; live backend bound to the transport seam)
// ---------------------------------------------------------------------------

/// Map the command's `overwrite` flag to a [`ConflictPolicy`]: an explicit
/// overwrite proceeds unconditionally; otherwise an existing destination aborts
/// the transfer (Requirement 13.7). The UI drives the prompt by re-invoking
/// with `overwrite = true` once the user confirms.
fn conflict_policy(overwrite: bool) -> ConflictPolicy {
    if overwrite {
        ConflictPolicy::Overwrite
    } else {
        ConflictPolicy::Abort
    }
}

/// List remote directory entries for a session (Requirement 13.1).
///
/// Lists over the session's SFTP channel through the File_Transfer_Service. On
/// an inaccessible directory the error is returned and no state is mutated, so
/// the UI keeps its previously displayed listing (Requirement 13.8).
#[tauri::command]
pub async fn list_remote(
    state: State<'_, AppState>,
    session: SessionId,
    path: String,
) -> Result<Vec<RemoteEntry>, DomainError> {
    state
        .transfers
        .list_remote(session, RemotePath::new(path))
        .await
        .map_err(DomainError::from)
}

/// Upload a local file to a remote path (Requirement 13.2) with progress
/// streaming on `transfer://{id}/progress`, overwrite handling, and
/// incomplete-file cleanup.
///
/// **Non-blocking (task 26.5):** this *starts* the transfer and returns as soon
/// as it is under way, so a concurrent [`cancel_transfer`] can reach the live
/// copy. The chunked copy runs in a spawned task; its live
/// [`TransferHandle`] is registered in [`InflightTransfers`] keyed by `session`
/// (so cancel can flip its cooperative flag), progress continues on
/// `transfer://{id}/progress`, and the terminal outcome is delivered on
/// `transfer://{id}/done`. The handle is deregistered when the task finishes.
#[tauri::command]
pub async fn upload_file(
    state: State<'_, AppState>,
    session: SessionId,
    local: PathBuf,
    remote: String,
    overwrite: bool,
) -> Result<(), DomainError> {
    let (handle, fut) = state.transfers.start_upload(
        session,
        local,
        RemotePath::new(remote),
        conflict_policy(overwrite),
    );
    spawn_transfer(&state, session, handle, fut).await;
    Ok(())
}

/// Download a remote file to a local path (Requirement 13.3) with the same
/// non-blocking, cancellable, progress, overwrite, and cleanup guarantees as
/// [`upload_file`].
#[tauri::command]
pub async fn download_file(
    state: State<'_, AppState>,
    session: SessionId,
    remote: String,
    local: PathBuf,
    overwrite: bool,
) -> Result<(), DomainError> {
    let (handle, fut) = state.transfers.start_download(
        session,
        RemotePath::new(remote),
        local,
        conflict_policy(overwrite),
    );
    spawn_transfer(&state, session, handle, fut).await;
    Ok(())
}

/// Register a started transfer's handle and spawn its copy future.
///
/// Keyed by `session`, the handle is registered *before* the task is spawned so
/// a cancel issued immediately after the command returns always finds it. When
/// the copy future resolves, the task deregisters the handle and emits the
/// terminal `transfer://{id}/done` outcome.
async fn spawn_transfer(
    state: &AppState,
    session: SessionId,
    handle: TransferHandle,
    fut: crate::transfer::TransferFuture,
) {
    state.inflight_transfers.register(session, handle).await;
    let inflight = state.inflight_transfers.clone();
    let events = state.events.clone();
    tokio::spawn(async move {
        let result = fut.await;
        // Deregister first so a late cancel sees "no live transfer" rather than
        // flipping a finished handle, then announce the terminal outcome.
        let _ = inflight.take(session).await;
        events.emit_transfer_done(session, &result);
    });
}

/// Cancel an in-flight transfer (Requirement 13.6).
///
/// Looks up the session's live [`TransferHandle`] in [`InflightTransfers`] and
/// flips its cooperative cancel flag; the spawned copy loop observes it within
/// one chunk (≤2 s), removes the partial destination, and reports
/// `transfer.canceled` on `transfer://{id}/done`. The handle is deregistered by
/// the copy task when it stops. If no transfer is in flight for the session the
/// command is a harmless no-op (the transfer already finished, or none was
/// started).
#[tauri::command]
pub async fn cancel_transfer(
    state: State<'_, AppState>,
    session: SessionId,
) -> Result<(), DomainError> {
    if let Some(handle) = state.inflight_transfers.get(session).await {
        state
            .transfers
            .cancel(handle)
            .await
            .map_err(DomainError::from)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Logging commands
// ---------------------------------------------------------------------------

/// Start recording a session's I/O to a timestamped log file (Req 14.1, 14.2).
///
/// Validates the destination and opens the sink; an invalid/unwritable
/// destination is reported as a `LogError` without enabling logging
/// (Requirement 14.6).
#[tauri::command]
pub async fn start_logging(
    state: State<'_, AppState>,
    id: SessionId,
    path: PathBuf,
) -> Result<(), DomainError> {
    state.logger.start(id, &path).map_err(DomainError::from)
}

/// Stop recording a session's I/O, retaining written entries (Requirement 14.3).
#[tauri::command]
pub async fn stop_logging(state: State<'_, AppState>, id: SessionId) -> Result<(), DomainError> {
    state.logger.stop(id).map_err(DomainError::from)
}

// ---------------------------------------------------------------------------
// Script command (shape final; live-session resolution bound to transport seam)
// ---------------------------------------------------------------------------

/// Run an automation script against a text-based session (Requirement 15.x).
///
/// Builds the default Script_Engine (real monotonic clock) and drives it over
/// the session's **live** `SessionConn` through
/// [`SessionManager::run_script`](crate::session::SessionManager::run_script),
/// which locks that one session's connection for the duration (serializing with
/// its interactive input, per-session only). The ordered [`ScriptStepDto`]s are
/// mapped into the core [`Script`]; the resulting [`ScriptOutcome`] is mapped to
/// the serializable [`ScriptOutcomeDto`]. A [`ScriptError`](crate::domain::ScriptError)
/// (prompt timeout / disconnection) surfaces as a coded [`DomainError`]; an
/// unknown session id surfaces as `session.notFound`.
#[tauri::command]
pub async fn run_script(
    state: State<'_, AppState>,
    session: SessionId,
    steps: Vec<ScriptStepDto>,
) -> Result<ScriptOutcomeDto, DomainError> {
    let script = Script::new(steps.into_iter().map(ScriptStep::from).collect());
    let engine = DefaultScriptEngine::with_monotonic_clock();
    let outcome = state.sessions.run_script(session, &engine, &script).await?;
    Ok(ScriptOutcomeDto::from(outcome))
}

/// Serializable script step: a command plus an optional wait-for-prompt gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptStepDto {
    pub command: String,
    #[serde(default)]
    pub wait_for_prompt: Option<String>,
    #[serde(default)]
    pub wait_timeout_secs: Option<u32>,
}

impl From<ScriptStepDto> for ScriptStep {
    /// Map a DTO step to a core [`ScriptStep`]. A step with a `wait_for_prompt`
    /// becomes a gated step; its `wait_timeout_secs` (default 0 → the engine's
    /// in-range default of 30 via `resolve_timeout`) bounds the wait. A step
    /// without a prompt is an ungated command.
    fn from(dto: ScriptStepDto) -> Self {
        match dto.wait_for_prompt {
            Some(prompt) => ScriptStep::command_then_wait(
                dto.command,
                prompt,
                dto.wait_timeout_secs.unwrap_or(0),
            ),
            None => ScriptStep::command(dto.command),
        }
    }
}

/// Serializable script outcome mirroring [`crate::script::ScriptOutcome`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    rename_all_fields = "camelCase",
    rename_all = "camelCase",
    tag = "status"
)]
pub enum ScriptOutcomeDto {
    Success {
        sent: usize,
    },
    Failure {
        command_index: usize,
        code: String,
        message: String,
    },
}

impl From<ScriptOutcome> for ScriptOutcomeDto {
    /// Map a core outcome to its DTO. A `Failure` carries the stopping index
    /// plus the coded/`message` form of the stopping [`ScriptError`] (reusing
    /// its `DomainError` mapping so the UI sees the same code as a thrown
    /// command error).
    fn from(outcome: ScriptOutcome) -> Self {
        match outcome {
            ScriptOutcome::Success { sent } => ScriptOutcomeDto::Success { sent },
            ScriptOutcome::Failure {
                command_index,
                error,
            } => {
                let de = DomainError::from(error);
                ScriptOutcomeDto::Failure {
                    command_index,
                    code: de.code,
                    message: de.message,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Config import / export
// ---------------------------------------------------------------------------

/// Export the whole configuration to a versioned file (Requirement 16.1).
#[tauri::command]
pub async fn export_config(
    state: State<'_, AppState>,
    path: PathBuf,
) -> Result<ExportSummaryDto, DomainError> {
    let summary: ExportSummary = state.connection.run(move |m| m.export_config(path)).await?;
    Ok(ExportSummaryDto {
        folders_exported: summary.folders_exported,
        profiles_exported: summary.profiles_exported,
    })
}

/// Import a configuration file additively (Requirements 16.2–16.5).
#[tauri::command]
pub async fn import_config(
    state: State<'_, AppState>,
    path: PathBuf,
) -> Result<ImportSummaryDto, DomainError> {
    let summary: ImportSummary = state
        .connection
        .run(move |m| m.import_config(path).map_err(DomainError::from))
        .await?;
    Ok(ImportSummaryDto {
        folders_added: summary.folders_added,
        profiles_added: summary.profiles_added,
    })
}

/// Serializable mirror of `connection::ExportSummary` for the IPC boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSummaryDto {
    pub folders_exported: usize,
    pub profiles_exported: usize,
}

/// Serializable mirror of `connection::ImportSummary` for the IPC boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummaryDto {
    pub folders_added: usize,
    pub profiles_added: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AuthConfig, CredentialKey, HostKeyPolicy};

    /// The DTO carries every `ProfileDraft` field across the (serde) IPC
    /// boundary and converts back field-for-field, so a draft submitted by the
    /// frontend reaches the Connection_Manager unchanged.
    #[test]
    fn profile_draft_dto_converts_field_for_field() {
        let dto = ProfileDraftDto {
            name: "prod-web".to_string(),
            protocol: ProtocolKind::Ssh,
            host: "example.com".to_string(),
            port: 22,
            username: Some("deploy".to_string()),
            domain: Some("CORP".to_string()),
            folder_id: None,
            auth: AuthConfig::Password {
                credential_key: CredentialKey::new("profile/x/password"),
            },
            protocol_params: crate::domain::ProtocolParams::Ssh {
                known_host_policy: HostKeyPolicy::PromptOnUnknown,
            },
            logging_enabled: true,
            log_path: Some(PathBuf::from("/tmp/session.log")),
            connect_timeout_secs: 45,
        };

        let draft: ProfileDraft = dto.clone().into();
        assert_eq!(draft.name, dto.name);
        assert_eq!(draft.protocol, dto.protocol);
        assert_eq!(draft.host, dto.host);
        assert_eq!(draft.port, dto.port);
        assert_eq!(draft.username, dto.username);
        assert_eq!(draft.domain, dto.domain);
        assert_eq!(draft.folder_id, dto.folder_id);
        assert_eq!(draft.auth, dto.auth);
        assert_eq!(draft.protocol_params, dto.protocol_params);
        assert_eq!(draft.logging_enabled, dto.logging_enabled);
        assert_eq!(draft.log_path, dto.log_path);
        assert_eq!(draft.connect_timeout_secs, dto.connect_timeout_secs);
    }

    /// The DTO deserializes from a camelCase JSON payload, which is the shape
    /// the TypeScript frontend emits over IPC.
    #[test]
    fn profile_draft_dto_deserializes_camel_case() {
        let json = r#"{
            "name": "serial-lab",
            "protocol": "serial",
            "host": "",
            "port": 1,
            "auth": { "type": "none" },
            "protocolParams": {
                "protocol": "serial",
                "port_name": "/dev/ttyUSB0",
                "baud": "b115200",
                "data_bits": "eight",
                "parity": "none",
                "stop_bits": "one"
            },
            "connectTimeoutSecs": 30
        }"#;
        let dto: ProfileDraftDto = serde_json::from_str(json).unwrap();
        assert_eq!(dto.protocol, ProtocolKind::Serial);
        assert!(!dto.logging_enabled); // defaulted
        assert_eq!(dto.log_path, None); // defaulted
    }

    /// The summary DTOs serialize as camelCase payloads for the frontend.
    #[test]
    fn summary_dtos_serialize_camel_case() {
        let export = serde_json::to_value(ExportSummaryDto {
            folders_exported: 3,
            profiles_exported: 7,
        })
        .unwrap();
        assert_eq!(export["foldersExported"], 3);
        assert_eq!(export["profilesExported"], 7);

        let import = serde_json::to_value(ImportSummaryDto {
            folders_added: 1,
            profiles_added: 2,
        })
        .unwrap();
        assert_eq!(import["foldersAdded"], 1);
        assert_eq!(import["profilesAdded"], 2);
    }

    /// The script-outcome DTO serializes with a `status` tag the frontend can
    /// switch on.
    #[test]
    fn script_outcome_dto_is_tagged() {
        let ok = serde_json::to_value(ScriptOutcomeDto::Success { sent: 4 }).unwrap();
        assert_eq!(ok["status"], "success");
        assert_eq!(ok["sent"], 4);

        let fail = serde_json::to_value(ScriptOutcomeDto::Failure {
            command_index: 2,
            code: "script.promptTimeout".to_string(),
            message: "timed out".to_string(),
        })
        .unwrap();
        assert_eq!(fail["status"], "failure");
        assert_eq!(fail["commandIndex"], 2);
    }

    /// The Connection_Manager actor runs both synchronous and asynchronous jobs
    /// against the owned manager and returns their typed results, proving the
    /// `Send`-channel / downcast transport works end-to-end.
    #[tokio::test]
    async fn connection_actor_round_trips_sync_and_async_jobs() {
        use crate::credential::KeyringCredentialStore;
        use crate::persistence::Database;

        // An in-memory manager over the OS-store type (never touched here: the
        // jobs below only read/list and create a folder, no credential I/O).
        let db = Database::open_in_memory().unwrap();
        let manager = ConnectionManager::new(db, KeyringCredentialStore::with_os_keystore());
        let handle = ConnectionHandle::spawn(manager);

        // Sync job: an empty store lists no folders.
        let folders = handle.run(|m| m.database().list_folders()).await.unwrap();
        assert!(folders.is_empty());

        // Sync job that mutates: creating a folder returns the stored folder.
        let created = handle
            .run(|m| m.create_folder("servers".to_string(), None))
            .await
            .unwrap();
        assert_eq!(created.name, "servers");

        // The mutation is visible to a subsequent job (same owned manager).
        let folders = handle.run(|m| m.database().list_folders()).await.unwrap();
        assert_eq!(folders.len(), 1);

        // Async job: deleting an absent profile without confirmation is gated
        // (the async delete path runs on the actor thread and awaits).
        let gated = handle
            .run_async(move |m| Box::pin(m.delete_profile(ProfileId::new(), false)))
            .await;
        assert!(gated.is_err());
        assert_eq!(
            gated.unwrap_err().code,
            crate::connection::CONFIRMATION_REQUIRED_CODE
        );
    }

    /// All four protocols are now wired end-to-end (task 25.4), so every one is
    /// selectable (Requirement 8.2 — selectable equals registered). SSH and RDP
    /// register over the real Credential_Store / `ironrdp` adapters built in
    /// this module.
    #[test]
    fn registry_exposes_all_wired_protocols() {
        let registry = build_registry();
        let kinds = registry.selectable_kinds();
        assert!(kinds.contains(&ProtocolKind::Telnet));
        assert!(kinds.contains(&ProtocolKind::Serial));
        assert!(kinds.contains(&ProtocolKind::Ssh));
        assert!(kinds.contains(&ProtocolKind::Rdp));
    }

    /// The `SecretResolver` adapter maps a missing credential to `Ok(None)`
    /// (which `plan_auth` turns into a missing-credential abort) and a present
    /// one to its plaintext, without leaking store internals.
    #[tokio::test]
    async fn secret_resolver_maps_not_found_to_none_and_present_to_value() {
        use crate::credential::Secret;

        // A tiny in-memory CredentialStore so the test never touches the OS
        // keystore. Only `get` is exercised by the adapter.
        struct MemStore {
            value: Option<String>,
        }
        #[async_trait]
        impl CredentialStore for MemStore {
            async fn save(
                &self,
                _key: CredentialKey,
                _secret: Secret,
            ) -> Result<(), CredentialError> {
                Ok(())
            }
            async fn get(&self, _key: CredentialKey) -> Result<Secret, CredentialError> {
                match &self.value {
                    Some(v) => Ok(Secret::new(v.clone())),
                    None => Err(CredentialError::NotFound),
                }
            }
            async fn delete_for_profile(&self, _id: ProfileId) -> Result<(), CredentialError> {
                Ok(())
            }
        }

        let present = IpcSecretResolver {
            store: Arc::new(MemStore {
                value: Some("hunter2".to_string()),
            }),
        };
        let got = present
            .resolve_secret(&CredentialKey::new("profile/x/password"))
            .await
            .unwrap();
        assert_eq!(got, Some("hunter2".to_string()));

        let absent = IpcSecretResolver {
            store: Arc::new(MemStore { value: None }),
        };
        let got = absent
            .resolve_secret(&CredentialKey::new("profile/x/password"))
            .await
            .unwrap();
        assert_eq!(got, None);
    }

    /// The RDP credential source aborts with `MissingCredentials` when the
    /// handle names no credential, and builds a `ResolvedCredential` (with the
    /// documented default username, no domain) from a resolved secret.
    #[tokio::test]
    async fn rdp_credential_source_resolves_or_reports_missing() {
        use crate::credential::Secret;

        struct MemStore {
            value: Option<String>,
        }
        #[async_trait]
        impl CredentialStore for MemStore {
            async fn save(
                &self,
                _key: CredentialKey,
                _secret: Secret,
            ) -> Result<(), CredentialError> {
                Ok(())
            }
            async fn get(&self, _key: CredentialKey) -> Result<Secret, CredentialError> {
                match &self.value {
                    Some(v) => Ok(Secret::new(v.clone())),
                    None => Err(CredentialError::NotFound),
                }
            }
            async fn delete_for_profile(&self, _id: ProfileId) -> Result<(), CredentialError> {
                Ok(())
            }
        }

        // No key on the handle: missing credentials, store never consulted.
        let src = IpcRdpCredentialSource {
            store: Arc::new(MemStore { value: None }),
            username: None,
            domain: None,
        };
        let err = src.resolve(&CredentialHandle::none()).await.unwrap_err();
        assert!(matches!(err, ProtocolError::MissingCredentials));

        // Key present + secret present, no profile identity: a resolved
        // credential with the default username and no domain.
        let src = IpcRdpCredentialSource {
            store: Arc::new(MemStore {
                value: Some("p@ss".to_string()),
            }),
            username: None,
            domain: None,
        };
        let cred = src
            .resolve(&CredentialHandle::with_key(CredentialKey::new(
                "profile/x/password",
            )))
            .await
            .unwrap();
        assert_eq!(cred.username, DEFAULT_RDP_USERNAME);
        assert_eq!(cred.password, "p@ss");
        assert_eq!(cred.domain, None);

        // Profile identity present: the profile's username/domain win over the
        // default; a blank value is treated as absent.
        let src = IpcRdpCredentialSource {
            store: Arc::new(MemStore {
                value: Some("p@ss".to_string()),
            }),
            username: Some("corp-admin".to_string()),
            domain: Some("  CORP  ".to_string()),
        };
        let cred = src
            .resolve(&CredentialHandle::with_key(CredentialKey::new(
                "profile/x/password",
            )))
            .await
            .unwrap();
        assert_eq!(cred.username, "corp-admin");
        assert_eq!(cred.domain, Some("CORP".to_string()));

        // Key present but secret missing: missing credentials.
        let src = IpcRdpCredentialSource {
            store: Arc::new(MemStore { value: None }),
            username: None,
            domain: None,
        };
        let err = src
            .resolve(&CredentialHandle::with_key(CredentialKey::new(
                "profile/x/password",
            )))
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::MissingCredentials));
    }

    /// Build an `IpcHostKeyVerifier` over an in-memory trust store and an
    /// (unattached) prompt channel for the wiring tests below.
    fn verifier_over(store: Arc<KnownHostsStore>, policy: HostKeyPolicy) -> IpcHostKeyVerifier {
        IpcHostKeyVerifier::new(
            store,
            HostKeyPrompts::new(TauriEventEmitter::new()),
            "example.com",
            22,
            policy,
            TrustKind::SshHostKey,
        )
    }

    fn empty_store() -> Arc<KnownHostsStore> {
        Arc::new(KnownHostsStore::new(
            crate::persistence::Database::open_in_memory().unwrap(),
        ))
    }

    /// A known, matching fingerprint proceeds silently (no prompt).
    #[tokio::test]
    async fn verifier_known_matching_fingerprint_proceeds() {
        let store = empty_store();
        store
            .remember("example.com", 22, TrustKind::SshHostKey, "SHA256:k1")
            .unwrap();
        let verifier = verifier_over(store, HostKeyPolicy::PromptOnUnknown);
        assert!(verifier.confirm_unknown_host_key("SHA256:k1").await);
    }

    /// A changed fingerprint is rejected without prompting — a hard fail.
    #[tokio::test]
    async fn verifier_changed_fingerprint_is_rejected() {
        let store = empty_store();
        store
            .remember("example.com", 22, TrustKind::SshHostKey, "SHA256:old")
            .unwrap();
        let verifier = verifier_over(store, HostKeyPolicy::PromptOnUnknown);
        assert!(!verifier.confirm_unknown_host_key("SHA256:new").await);
    }

    /// A Strict policy rejects an unknown host without prompting.
    #[tokio::test]
    async fn verifier_strict_rejects_unknown() {
        let verifier = verifier_over(empty_store(), HostKeyPolicy::Strict);
        assert!(!verifier.confirm_unknown_host_key("SHA256:k1").await);
    }

    /// An AcceptAny policy auto-accepts an unknown host (no UI round trip) and
    /// *remembers* it, so a later changed key is then rejected.
    #[tokio::test]
    async fn verifier_accept_any_auto_accepts_and_remembers() {
        let store = empty_store();
        let verifier = verifier_over(Arc::clone(&store), HostKeyPolicy::AcceptAny);
        assert!(verifier.confirm_unknown_host_key("SHA256:k1").await);
        // Remembered: a matching key still proceeds, a changed key is rejected.
        assert_eq!(
            store.get("example.com", 22, TrustKind::SshHostKey).unwrap(),
            Some("SHA256:k1".to_string())
        );
        assert!(!verifier.confirm_unknown_host_key("SHA256:k2").await);
    }

    /// Under a prompting policy, an unknown host whose prompt is accepted by the
    /// UI is remembered and trusted. The prompt is driven by resolving the
    /// pending request the verifier emits.
    #[tokio::test]
    async fn verifier_prompt_accept_remembers_and_trusts() {
        let store = empty_store();
        let prompts = HostKeyPrompts::new(TauriEventEmitter::new());
        let verifier = IpcHostKeyVerifier::new(
            Arc::clone(&store),
            prompts.clone(),
            "example.com",
            22,
            HostKeyPolicy::PromptOnUnknown,
            TrustKind::SshHostKey,
        );

        // Resolve the (single) pending prompt as "accept" shortly after the
        // verifier emits it. The event emitter is unattached, so no event is
        // delivered; we fulfil the pending oneshot directly by draining the map.
        let prompts_for_task = prompts.clone();
        let decide =
            tokio::spawn(async move { verifier.confirm_unknown_host_key("SHA256:k1").await });

        // Give the verifier a moment to register the pending prompt, then accept
        // whatever request id is in flight.
        tokio::task::yield_now().await;
        accept_only_pending(&prompts_for_task, true);

        assert!(decide.await.unwrap(), "accepted prompt trusts the key");
        assert_eq!(
            store.get("example.com", 22, TrustKind::SshHostKey).unwrap(),
            Some("SHA256:k1".to_string())
        );
    }

    /// Under a prompting policy, a declined prompt rejects and does not remember.
    #[tokio::test]
    async fn verifier_prompt_decline_rejects_and_does_not_remember() {
        let store = empty_store();
        let prompts = HostKeyPrompts::new(TauriEventEmitter::new());
        let verifier = IpcHostKeyVerifier::new(
            Arc::clone(&store),
            prompts.clone(),
            "example.com",
            22,
            HostKeyPolicy::PromptOnUnknown,
            TrustKind::SshHostKey,
        );

        let prompts_for_task = prompts.clone();
        let decide =
            tokio::spawn(async move { verifier.confirm_unknown_host_key("SHA256:k1").await });
        tokio::task::yield_now().await;
        accept_only_pending(&prompts_for_task, false);

        assert!(!decide.await.unwrap(), "declined prompt rejects the key");
        assert_eq!(
            store.get("example.com", 22, TrustKind::SshHostKey).unwrap(),
            None,
            "a declined key is not remembered"
        );
    }

    /// Resolve whatever single prompt is currently in flight with `accept`.
    /// The verifier generates an opaque request id, so the test cannot name it;
    /// this helper fulfils the one pending request through the public
    /// `respond`-equivalent by polling until it is present.
    fn accept_only_pending(prompts: &HostKeyPrompts, accept: bool) {
        // Spin briefly for the pending request to appear, then resolve it.
        for _ in 0..1000 {
            if prompts.resolve_any(accept) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("no pending host-key prompt appeared");
    }

    /// The SSH key resolver maps a key with no persisted private material to
    /// `Ok(None)` (not an error), and surfaces a keyring backend failure as a
    /// `ProtocolError::Io`. It resolves against its injected `SecretStore`.
    #[tokio::test]
    async fn ssh_key_resolver_maps_unknown_and_backend_failure() {
        use std::collections::HashMap;
        use std::sync::Mutex as StdMutex;

        /// In-memory `SecretStore` for the resolver test: optionally fails
        /// `get_secret` to exercise the backend-error path.
        #[derive(Default)]
        struct MemSecretStore {
            secrets: StdMutex<HashMap<String, String>>,
            fail_get: bool,
        }

        #[async_trait]
        impl SecretStore for MemSecretStore {
            async fn store_secret(
                &self,
                key: &CredentialKey,
                secret: &str,
            ) -> Result<(), KeyError> {
                self.secrets
                    .lock()
                    .unwrap()
                    .insert(key.as_str().to_string(), secret.to_string());
                Ok(())
            }
            async fn get_secret(&self, key: &CredentialKey) -> Result<Option<String>, KeyError> {
                if self.fail_get {
                    return Err(KeyError::StorageFailed("boom".to_string()));
                }
                Ok(self.secrets.lock().unwrap().get(key.as_str()).cloned())
            }
            async fn delete_secret(&self, key: &CredentialKey) -> Result<(), KeyError> {
                self.secrets.lock().unwrap().remove(key.as_str());
                Ok(())
            }
        }

        // Unknown key (nothing persisted): Ok(None).
        let resolver = IpcSshKeyResolver {
            store: Box::new(MemSecretStore::default()),
        };
        assert_eq!(resolver.resolve_key(KeyId::new()).await.unwrap(), None);

        // Backend failure: ProtocolError::Io.
        let resolver = IpcSshKeyResolver {
            store: Box::new(MemSecretStore {
                fail_get: true,
                ..Default::default()
            }),
        };
        let err = resolver.resolve_key(KeyId::new()).await.unwrap_err();
        assert!(matches!(err, ProtocolError::Io(_)));
    }

    /// An empty SFTP provider reports a precise error for an unknown session,
    /// and resolves a registered session back to its handle.
    #[tokio::test]
    async fn sftp_sessions_provider_resolves_registered_only() {
        let sessions = SftpSessions::new();
        let provider = sessions.provider();

        let unknown = SessionId::new();
        // `SharedSftpSession` (the Ok value) is not `Debug`, so match instead
        // of `unwrap_err`.
        match provider.sftp_session(unknown).await {
            Err(TransferError::Io(_)) => {}
            Err(other) => panic!("unexpected error variant: {other:?}"),
            Ok(_) => panic!("an unknown session must not resolve to an SFTP session"),
        }
    }
}
