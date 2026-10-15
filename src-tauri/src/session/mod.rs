//! Session_Manager — tabs, per-session isolation, and bounded concurrency.
//!
//! This module implements task 13.1. The [`SessionManager`] opens a Session in
//! a new, active Tab backed by its own set of `tokio` tasks, enforces the
//! concurrency limit [`MAX_CONCURRENT`] (≥20), activates tabs so that exactly
//! one is active at a time, and closes tabs — tearing down the Session and
//! releasing its resources — with a confirmation gate for an active Session.
//!
//! It implements the design's `SessionManager` trait shape (design:
//! "Session_Manager", "Threading and Session Isolation"):
//!
//! ```ignore
//! trait SessionManager {
//!     async fn open_session(&self, source: SessionSource) -> Result<SessionId, _>;
//!     async fn activate_tab(&self, id: SessionId) -> Result<(), _>;
//!     async fn close_tab(&self, id: SessionId, confirmed: bool) -> Result<(), _>;
//!     fn session_state(&self, id: SessionId) -> Option<SessionState>;
//!     fn open_count(&self) -> usize;
//! }
//! ```
//!
//! # Threading and Session Isolation (design: "Threading and Session Isolation")
//!
//! Each open Session runs its **own** async task set on the shared `tokio`
//! runtime:
//!
//! - a **read loop** that pulls output off the remote via
//!   [`SessionConn::recv`](crate::protocol::SessionConn::recv) and appends it to
//!   *that session's* independent output buffer, and
//! - a **write path** that forwards UI/script input to the remote via
//!   [`SessionConn::send`](crate::protocol::SessionConn::send).
//!
//! Sessions share no mutable state. The manager holds each Session behind its
//! own [`SessionHandle`], and the per-session buffer and state live behind that
//! handle's own locks. This is the structural basis for **Property 22**
//! (isolation): activity on one Session cannot read or mutate another's buffer
//! or state, because there is simply no shared path between them.
//!
//! # Capacity (Property 23)
//!
//! [`open_session`](SessionManager::open_session) refuses to open past
//! [`MAX_CONCURRENT`] and leaves every existing Session untouched when it does
//! (Requirements 9.3, 9.4).
//!
//! # Exactly one active tab (Property 24)
//!
//! Opening a Session makes its tab the active one (Requirement 9.1), and
//! [`activate_tab`](SessionManager::activate_tab) moves the single active marker
//! so that after it returns exactly one tab — the selected one — is active
//! (Requirement 9.5).
//!
//! # Closing and resource release (Property 25)
//!
//! [`close_tab`](SessionManager::close_tab) aborts the session's tasks, calls
//! [`SessionConn::close`](crate::protocol::SessionConn::close) to release the
//! network transport, drops the buffer, and removes the tab (Requirement 9.6).
//! An **active (connected)** Session requires `confirmed = true`; without it the
//! Session and its tab are preserved and the gate is signalled by the stable
//! code [`CONFIRMATION_REQUIRED_CODE`] (Requirement 9.7). The flag + code
//! pattern is the same one the Connection_Manager uses for profile/folder
//! deletion.
//!
//! # Integration with the Protocol_Engine
//!
//! For testability (Property 22/25 want a no-network mock) and to decouple tab
//! lifecycle from protocol establishment, the manager takes an already
//! established connection via [`SessionSource`]: a `Box<dyn SessionConn>` plus
//! the originating [`ProtocolKind`] and optional [`ProfileId`]. The **wiring**
//! that turns a [`ConnectionProfile`](crate::domain::ConnectionProfile) into an
//! established `SessionConn` — resolving credentials through the
//! Credential_Store and calling
//! [`Protocol::establish`](crate::protocol::Protocol::establish) — belongs to a
//! later IPC/wiring task (see tasks 7–11 and 20). That task constructs the
//! `SessionSource` and hands it to [`open_session`]; nothing in this module
//! opens a socket itself.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::task::JoinHandle;

use crate::domain::{DomainError, ProfileId, ProtocolKind, SessionId, SessionState};
use crate::protocol::SessionConn;

/// Maximum number of concurrently open Sessions.
///
/// Requirement 9.3 requires support for *at least* 20 concurrent Sessions, and
/// Requirement 9.4 rejects opens beyond the limit. We set the limit to exactly
/// 20 — the smallest value that satisfies "at least 20".
pub const MAX_CONCURRENT: usize = 20;

/// Stable error code returned when a destructive action (closing an active
/// Session) is requested without confirmation (Requirement 9.7).
///
/// Mirrors the Connection_Manager's confirmation gate so the IPC layer, the UI,
/// and the tests can recognise the "confirmation required" outcome by code
/// rather than by matching message text.
pub const CONFIRMATION_REQUIRED_CODE: &str = "session.confirmationRequired";

/// Stable error code returned when an open is refused for exceeding
/// [`MAX_CONCURRENT`] (Requirement 9.4).
pub const CAPACITY_REACHED_CODE: &str = "session.capacityReached";

/// Stable error code returned when an operation names a Session/tab that does
/// not exist.
pub const SESSION_NOT_FOUND_CODE: &str = "session.notFound";

/// A sink the per-session read loop feeds every output chunk to, tagged with
/// the originating [`SessionId`] (task 20.2).
///
/// This is the **push** seam that lets the IPC layer stream a session's remote
/// output to the frontend as a `session://{id}/output` event without the
/// frontend ever touching I/O. The `SessionManager` keeps its own per-session
/// output buffer *and* forwards each chunk here, so the sink is a pure fan-out
/// of what the read loop already produces.
///
/// # Isolation (Requirement 9.2)
///
/// Each session's read loop forwards **only its own** chunks, always tagged
/// with that session's id. There is no shared path between sessions, so one
/// session's output can never surface under another session's id — the stream
/// isolation is structural, mirroring the per-session buffer isolation.
///
/// Implementations must not block (the read loop awaits nothing on `emit`); a
/// Tauri `emit` is a cheap, non-blocking enqueue, and the default
/// [`NullOutputSink`] does nothing.
pub trait SessionOutputSink: Send + Sync {
    /// Forward one output chunk produced by `session`'s read loop.
    fn emit(&self, session: SessionId, chunk: &[u8]);
}

/// A no-op [`SessionOutputSink`]: the default when no streaming consumer is
/// wired (e.g. in unit tests), so the read loop still fills its buffer but
/// emits nowhere.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullOutputSink;

impl SessionOutputSink for NullOutputSink {
    fn emit(&self, _session: SessionId, _chunk: &[u8]) {}
}

/// An established connection ready to be hosted in a Tab.
///
/// This is the hand-off point from the Protocol_Engine to the Session_Manager.
/// The `conn` is a live [`SessionConn`] produced by
/// [`Protocol::establish`](crate::protocol::Protocol::establish) (or, in tests,
/// a mock). Carrying the connection pre-established keeps the manager free of
/// protocol/credential concerns and lets the property/smoke tests inject a
/// no-network connection.
pub struct SessionSource {
    /// The originating profile, or `None` for an ad hoc session.
    pub profile_id: Option<ProfileId>,
    /// The protocol this session speaks (for display / routing).
    pub kind: ProtocolKind,
    /// The live, already-established connection.
    pub conn: Box<dyn SessionConn>,
}

impl SessionSource {
    /// Wrap an established connection and its origin.
    pub fn new(kind: ProtocolKind, profile_id: Option<ProfileId>, conn: Box<dyn SessionConn>) -> Self {
        Self {
            profile_id,
            kind,
            conn,
        }
    }
}

/// A single Session's private, independent state.
///
/// Every field here belongs to exactly one Session. Nothing in this struct is
/// shared with another Session, which is what makes isolation (Property 22)
/// structural rather than merely conventional.
struct SessionHandle {
    /// Origin profile, or `None` for an ad hoc session. (The session's id is
    /// the key under which this handle is stored in the manager.)
    profile_id: Option<ProfileId>,
    kind: ProtocolKind,
    /// Current connection state, updated by the read loop (and on close).
    state: Arc<RwLock<SessionState>>,
    /// This session's independent output buffer (remote → UI). Owned solely by
    /// this handle; the read loop appends, queries read.
    output: Arc<Mutex<Vec<u8>>>,
    /// Sender into the write path (UI/script input → remote).
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// The spawned `tokio` tasks for this session (read loop + write path).
    /// Aborted on close to release the tasks promptly.
    tasks: Vec<JoinHandle<()>>,
    /// The live connection, shared with the write-path task so both the task
    /// and `close_tab` can drive it. Behind its own `Mutex` so send/recv/close
    /// are serialized for this session only.
    conn: Arc<Mutex<Box<dyn SessionConn>>>,
}

/// The Session_Manager: owns the open Sessions, the ordered tabs, and the
/// single active-tab marker.
///
/// Cloneable-free: hold it behind an `Arc` if shared across the IPC layer. All
/// mutating operations take `&self` and lock internally, matching the design's
/// `async fn ... (&self, ...)` trait signatures so a single shared instance can
/// serve concurrent IPC calls.
pub struct SessionManager {
    inner: Mutex<ManagerState>,
    /// The push sink every session's read loop forwards its output to (task
    /// 20.2). Shared with each spawned read-loop task so output streams out as
    /// `session://{id}/output` events. Defaults to [`NullOutputSink`] when no
    /// streaming consumer is wired.
    output_sink: Arc<dyn SessionOutputSink>,
}

/// The manager's mutable bookkeeping, guarded as a unit so open/close/activate
/// never interleave into an inconsistent tab set.
struct ManagerState {
    /// Open sessions keyed by id.
    sessions: HashMap<SessionId, SessionHandle>,
    /// Tab order (insertion order); each entry hosts one Session.
    tab_order: Vec<SessionId>,
    /// The single active tab, or `None` when no tabs are open.
    active: Option<SessionId>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    /// Create an empty manager with no open sessions and no output streaming
    /// (output still accumulates in each session's buffer).
    pub fn new() -> Self {
        Self::with_output_sink(Arc::new(NullOutputSink))
    }

    /// Create an empty manager whose per-session read loops forward every
    /// output chunk to `output_sink` (task 20.2).
    ///
    /// The production app injects a Tauri-backed sink here so session output
    /// streams to the frontend as `session://{id}/output` events; tests inject
    /// a recording sink (or rely on [`new`](Self::new)'s no-op default).
    pub fn with_output_sink(output_sink: Arc<dyn SessionOutputSink>) -> Self {
        Self {
            inner: Mutex::new(ManagerState {
                sessions: HashMap::new(),
                tab_order: Vec::new(),
                active: None,
            }),
            output_sink,
        }
    }

    /// Open a Session from an established connection in a new, active Tab.
    ///
    /// Spawns this session's own read loop and write path (its independent task
    /// set), records an independent output buffer and state, appends the tab,
    /// and makes the new tab the active one (Requirement 9.1). Returns the new
    /// [`SessionId`].
    ///
    /// Refuses — leaving every existing Session and the active tab untouched —
    /// when [`MAX_CONCURRENT`] Sessions are already open, returning a
    /// [`DomainError`] coded [`CAPACITY_REACHED_CODE`] (Requirements 9.3, 9.4;
    /// Property 23).
    pub async fn open_session(&self, source: SessionSource) -> Result<SessionId, DomainError> {
        let mut state = self.inner.lock().await;

        // Property 23 / Requirement 9.4: reject at the limit *before* taking
        // ownership of the connection or spawning anything, so existing
        // sessions are provably untouched. The rejected `source` (and its
        // connection) is dropped here, releasing the would-be resources.
        if state.sessions.len() >= MAX_CONCURRENT {
            return Err(DomainError::new(
                CAPACITY_REACHED_CODE,
                format!(
                    "The maximum number of concurrent sessions ({MAX_CONCURRENT}) has been reached."
                ),
            ));
        }

        let id = SessionId::new();
        let SessionSource {
            profile_id,
            kind,
            conn,
        } = source;

        // Seed state from the connection's own reported state so a session that
        // arrives already connected is reflected faithfully.
        let initial_state = conn.state();
        let shared_state = Arc::new(RwLock::new(initial_state));
        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let conn = Arc::new(Mutex::new(conn));

        // Write path: UI/script input → remote. An unbounded channel decouples
        // callers from the remote's pace; the task owns the send side of the
        // connection lock for the duration of each send.
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        // --- Write-path task (this session's own) ---------------------------
        let write_conn = Arc::clone(&conn);
        let write_task = tokio::spawn(async move {
            while let Some(bytes) = input_rx.recv().await {
                let mut guard = write_conn.lock().await;
                if guard.send(&bytes).await.is_err() {
                    // A send failure means the transport is gone; stop the
                    // write path. The read loop will observe and record the
                    // disconnection.
                    break;
                }
            }
        });

        // --- Read-loop task (this session's own) ----------------------------
        // Remote → this session's independent output buffer. On EOF or error it
        // records the disconnection into this session's own state and exits.
        let read_conn = Arc::clone(&conn);
        let read_output = Arc::clone(&output);
        let read_state = Arc::clone(&shared_state);
        // The read loop forwards every chunk to the push sink tagged with this
        // session's id (task 20.2). Cloning the shared sink per task keeps each
        // read loop forwarding only its own output under its own id, preserving
        // isolation (Requirement 9.2).
        let read_sink = Arc::clone(&self.output_sink);
        let read_task = tokio::spawn(async move {
            loop {
                // Scope the lock so recv does not hold it across the buffer
                // append; this keeps the per-session lock contention minimal
                // and never touches any other session's lock.
                let chunk = {
                    let mut guard = read_conn.lock().await;
                    guard.recv().await
                };
                match chunk {
                    Ok(Some(bytes)) => {
                        {
                            let mut buf = read_output.lock().await;
                            buf.extend_from_slice(&bytes);
                        }
                        // Fan the same chunk out to the streaming sink. The
                        // buffer (above) remains the source of truth for
                        // `output_snapshot`; the sink is a non-blocking
                        // forward for the live `session://{id}/output` stream.
                        read_sink.emit(id, &bytes);
                    }
                    Ok(None) | Err(_) => {
                        // Clean EOF or transport error: this session is done.
                        let mut s = read_state.write().await;
                        *s = SessionState::Disconnected;
                        break;
                    }
                }
            }
        });

        let handle = SessionHandle {
            profile_id,
            kind,
            state: shared_state,
            output,
            input_tx,
            tasks: vec![read_task, write_task],
            conn,
        };

        state.sessions.insert(id, handle);
        state.tab_order.push(id);
        // Requirement 9.1: the newly opened tab becomes the active tab.
        state.active = Some(id);

        Ok(id)
    }

    /// Activate the tab hosting `id` so that **exactly one** tab is active and
    /// it is the selected one (Requirement 9.5; Property 24).
    ///
    /// Because the active marker is a single `Option<SessionId>`, setting it to
    /// the selected tab both activates that tab and deactivates whatever was
    /// active before, with no window in which two tabs are active.
    pub async fn activate_tab(&self, id: SessionId) -> Result<(), DomainError> {
        let mut state = self.inner.lock().await;
        if !state.sessions.contains_key(&id) {
            return Err(DomainError::new(
                SESSION_NOT_FOUND_CODE,
                "No open session hosts the requested tab.",
            ));
        }
        state.active = Some(id);
        Ok(())
    }

    /// Close the tab hosting `id`, terminating the Session and releasing its
    /// memory and network resources (Requirement 9.6; Property 25).
    ///
    /// # Confirmation gate (Requirement 9.7)
    ///
    /// If the hosted Session is **active** — i.e. in [`SessionState::Connected`]
    /// — closing requires `confirmed = true`. When `confirmed` is `false` the
    /// method performs no teardown at all: the Session and its tab are
    /// preserved and the method returns a [`DomainError`] coded
    /// [`CONFIRMATION_REQUIRED_CODE`]. The UI shows the confirmation prompt and,
    /// on approval, calls again with `confirmed = true`. A Session that is not
    /// connected (connecting or already disconnected) carries no live work to
    /// lose and closes without a confirmation round-trip.
    ///
    /// # Teardown
    ///
    /// On a permitted close the session's tasks are aborted (releasing the read
    /// loop and write path), its connection is closed via
    /// [`SessionConn::close`](crate::protocol::SessionConn::close) (releasing
    /// the network transport), its buffer is dropped, and the tab is removed. If
    /// the closed tab was the active one, the active marker moves to another
    /// open tab (or `None` when none remain), preserving the exactly-one-active
    /// invariant.
    pub async fn close_tab(&self, id: SessionId, confirmed: bool) -> Result<(), DomainError> {
        let mut state = self.inner.lock().await;

        let handle = state
            .sessions
            .get(&id)
            .ok_or_else(|| DomainError::new(SESSION_NOT_FOUND_CODE, "No open session hosts the requested tab."))?;

        // Requirement 9.7: an active (connected) session requires confirmation.
        let is_active = { *handle.state.read().await == SessionState::Connected };
        if is_active && !confirmed {
            return Err(DomainError::new(
                CONFIRMATION_REQUIRED_CODE,
                "Closing an active session requires confirmation.",
            ));
        }

        // Remove the handle so no further query can observe a half-torn-down
        // session; teardown then happens on the owned handle.
        let handle = state
            .sessions
            .remove(&id)
            .expect("handle present under lock");

        // Release this session's tasks (read loop + write path). Dropping the
        // input sender also unblocks the write task's recv().
        for task in &handle.tasks {
            task.abort();
        }
        drop(handle.input_tx);

        // Release the network transport. `close` is idempotent and may error if
        // the transport is already gone; that is not fatal to removing the tab,
        // so the error is intentionally not propagated — the goal (resources
        // released, tab removed) is achieved either way.
        {
            let mut conn = handle.conn.lock().await;
            let _ = conn.close().await;
        }
        // `handle.output` and `handle.conn` drop here, freeing the buffer and
        // the connection's owned memory.

        // Remove the tab and fix up the active marker.
        state.tab_order.retain(|t| *t != id);
        if state.active == Some(id) {
            state.active = state.tab_order.last().copied();
        }

        Ok(())
    }

    /// Send input (keystrokes / script commands) to the Session's remote via its
    /// own write path. Returns an error if the Session does not exist.
    ///
    /// This routes through the per-session input channel, so input for one
    /// Session never reaches another's write path (part of the isolation
    /// guarantee, Property 22).
    pub async fn send_input(&self, id: SessionId, bytes: Vec<u8>) -> Result<(), DomainError> {
        let state = self.inner.lock().await;
        let handle = state
            .sessions
            .get(&id)
            .ok_or_else(|| DomainError::new(SESSION_NOT_FOUND_CODE, "No open session hosts the requested tab."))?;
        handle
            .input_tx
            .send(bytes)
            .map_err(|_| DomainError::new("session.writeClosed", "The session's write path is closed."))
    }

    /// The current state of the Session hosted by `id`, or `None` if there is no
    /// such Session (design: `session_state`).
    pub async fn session_state(&self, id: SessionId) -> Option<SessionState> {
        // Clone the per-session Arc, then release the manager lock before
        // awaiting the session's own lock (avoids holding the outer guard
        // across the nested await).
        let state_arc = {
            let state = self.inner.lock().await;
            Arc::clone(&state.sessions.get(&id)?.state)
        };
        let current = *state_arc.read().await;
        Some(current)
    }

    /// The number of currently open Sessions (design: `open_count`).
    pub async fn open_count(&self) -> usize {
        self.inner.lock().await.sessions.len()
    }

    /// The id of the currently active tab, or `None` when no tabs are open
    /// (Property 24 query surface).
    pub async fn active_tab(&self) -> Option<SessionId> {
        self.inner.lock().await.active
    }

    /// A read-only snapshot of this Session's independent output buffer.
    ///
    /// Exposed for the IPC/terminal layer and for the isolation property test to
    /// assert that activity on one Session leaves another's buffer unchanged.
    pub async fn output_snapshot(&self, id: SessionId) -> Option<Vec<u8>> {
        let output_arc = {
            let state = self.inner.lock().await;
            Arc::clone(&state.sessions.get(&id)?.output)
        };
        let snapshot = output_arc.lock().await.clone();
        Some(snapshot)
    }

    /// The protocol kind and origin profile of a Session, if open.
    pub async fn session_meta(&self, id: SessionId) -> Option<(ProtocolKind, Option<ProfileId>)> {
        let state = self.inner.lock().await;
        let handle = state.sessions.get(&id)?;
        Some((handle.kind, handle.profile_id))
    }

    /// Run an automation script against a session's **live** connection
    /// (Requirement 15.x).
    ///
    /// Resolves `id` to its [`SessionConn`], locks that session's own `conn`
    /// mutex, and drives `engine` over it. The engine sends the script's
    /// commands in order, honours any wait-for-prompt gating, and returns a
    /// [`ScriptOutcome`](crate::script::ScriptOutcome).
    ///
    /// # Serialization with `send_input` (per-session isolation)
    ///
    /// The connection is held under its per-session `conn` mutex for the whole
    /// run, so a script and interactive keystrokes for *that one session* are
    /// serialized against each other — a script never interleaves its commands
    /// with the user's typing on the same remote. This touches only the one
    /// session's lock; every other session is completely unaffected, matching
    /// the per-session isolation model (Property 22). Note the write-path task
    /// still owns the send side via the same mutex, so queued input simply
    /// waits until the script releases the lock.
    ///
    /// An unknown `id` returns a [`DomainError`] coded [`SESSION_NOT_FOUND_CODE`]
    /// without running anything.
    pub async fn run_script(
        &self,
        id: SessionId,
        engine: &dyn crate::script::ScriptEngine,
        script: &crate::script::Script,
    ) -> Result<crate::script::ScriptOutcome, DomainError> {
        // Clone the per-session connection Arc, then release the manager lock
        // before awaiting the session's own lock (same pattern as the other
        // query methods: never hold the outer guard across the nested await).
        let conn = {
            let state = self.inner.lock().await;
            let handle = state.sessions.get(&id).ok_or_else(|| {
                DomainError::new(
                    SESSION_NOT_FOUND_CODE,
                    "No open session hosts the requested tab.",
                )
            })?;
            Arc::clone(&handle.conn)
        };

        let mut guard = conn.lock().await;
        engine
            .run(&mut **guard, script)
            .await
            .map_err(DomainError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ProtocolError;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::mpsc as tmpsc;

    /// A no-network mock `SessionConn` for the smoke tests. It reports a
    /// configurable state, records what was sent, and delivers scripted recv
    /// chunks through a channel so a test can drive the read loop deterministically.
    struct MockConn {
        state: SessionState,
        closed: Arc<AtomicBool>,
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        recv_rx: Mutex<tmpsc::UnboundedReceiver<Option<Vec<u8>>>>,
    }

    impl MockConn {
        fn new(
            state: SessionState,
        ) -> (Self, tmpsc::UnboundedSender<Option<Vec<u8>>>, Arc<AtomicBool>, Arc<Mutex<Vec<Vec<u8>>>>) {
            let (tx, rx) = tmpsc::unbounded_channel();
            let closed = Arc::new(AtomicBool::new(false));
            let sent = Arc::new(Mutex::new(Vec::new()));
            let conn = MockConn {
                state,
                closed: Arc::clone(&closed),
                sent: Arc::clone(&sent),
                recv_rx: Mutex::new(rx),
            };
            (conn, tx, closed, sent)
        }
    }

    #[async_trait]
    impl SessionConn for MockConn {
        async fn send(&mut self, data: &[u8]) -> Result<(), ProtocolError> {
            self.sent.lock().await.push(data.to_vec());
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
            // Resolve the next scripted chunk; a closed channel is treated as EOF.
            match self.recv_rx.lock().await.recv().await {
                Some(chunk) => Ok(chunk),
                None => Ok(None),
            }
        }

        async fn resize(&mut self, _cols: u16, _rows: u16) -> Result<(), ProtocolError> {
            Ok(())
        }

        async fn close(&mut self) -> Result<(), ProtocolError> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn state(&self) -> SessionState {
            self.state
        }
    }

    fn source(state: SessionState) -> (SessionSource, tmpsc::UnboundedSender<Option<Vec<u8>>>, Arc<AtomicBool>, Arc<Mutex<Vec<Vec<u8>>>>) {
        let (conn, recv_tx, closed, sent) = MockConn::new(state);
        let src = SessionSource::new(ProtocolKind::Ssh, None, Box::new(conn));
        (src, recv_tx, closed, sent)
    }

    #[tokio::test]
    async fn open_adds_an_active_tab_and_tracks_state() {
        let mgr = SessionManager::new();
        let (src, _rx, _closed, _sent) = source(SessionState::Connected);

        let id = mgr.open_session(src).await.unwrap();

        assert_eq!(mgr.open_count().await, 1);
        assert_eq!(mgr.active_tab().await, Some(id));
        assert_eq!(mgr.session_state(id).await, Some(SessionState::Connected));
    }

    #[tokio::test]
    async fn activation_leaves_exactly_one_active_tab() {
        let mgr = SessionManager::new();
        let (a, _r1, _c1, _s1) = source(SessionState::Connecting);
        let (b, _r2, _c2, _s2) = source(SessionState::Connecting);
        let id_a = mgr.open_session(a).await.unwrap();
        let id_b = mgr.open_session(b).await.unwrap();

        // Opening b made b active; activating a flips the single marker.
        assert_eq!(mgr.active_tab().await, Some(id_b));
        mgr.activate_tab(id_a).await.unwrap();
        assert_eq!(mgr.active_tab().await, Some(id_a));
    }

    #[tokio::test]
    async fn capacity_is_bounded_and_rejection_preserves_existing() {
        let mgr = SessionManager::new();
        let mut ids = Vec::new();
        for _ in 0..MAX_CONCURRENT {
            let (src, _r, _c, _s) = source(SessionState::Connecting);
            ids.push(mgr.open_session(src).await.unwrap());
        }
        assert_eq!(mgr.open_count().await, MAX_CONCURRENT);

        // One over the limit is rejected...
        let (over, _r, _c, _s) = source(SessionState::Connecting);
        let err = mgr.open_session(over).await.unwrap_err();
        assert_eq!(err.code, CAPACITY_REACHED_CODE);

        // ...and every existing session is preserved.
        assert_eq!(mgr.open_count().await, MAX_CONCURRENT);
        for id in ids {
            assert!(mgr.session_state(id).await.is_some());
        }
    }

    #[tokio::test]
    async fn closing_active_requires_confirmation_and_preserves_on_decline() {
        let mgr = SessionManager::new();
        let (src, _rx, closed, _sent) = source(SessionState::Connected);
        let id = mgr.open_session(src).await.unwrap();

        // Decline: nothing is torn down.
        let err = mgr.close_tab(id, false).await.unwrap_err();
        assert_eq!(err.code, CONFIRMATION_REQUIRED_CODE);
        assert_eq!(mgr.open_count().await, 1);
        assert!(!closed.load(Ordering::SeqCst));

        // Confirm: the session is closed and its connection released.
        mgr.close_tab(id, true).await.unwrap();
        assert_eq!(mgr.open_count().await, 0);
        assert_eq!(mgr.active_tab().await, None);
        // Give the aborted tasks a moment; close() ran synchronously above.
        assert!(closed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn closing_inactive_session_needs_no_confirmation() {
        let mgr = SessionManager::new();
        let (src, _rx, closed, _sent) = source(SessionState::Disconnected);
        let id = mgr.open_session(src).await.unwrap();

        mgr.close_tab(id, false).await.unwrap();
        assert_eq!(mgr.open_count().await, 0);
        assert!(closed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn read_loop_fills_the_sessions_own_buffer_in_isolation() {
        let mgr = SessionManager::new();
        let (a, tx_a, _ca, _sa) = source(SessionState::Connected);
        let (b, _tx_b, _cb, _sb) = source(SessionState::Connected);
        let id_a = mgr.open_session(a).await.unwrap();
        let id_b = mgr.open_session(b).await.unwrap();

        // Drive only session A's remote output.
        tx_a.send(Some(b"hello".to_vec())).unwrap();

        // Wait until A's buffer reflects the chunk (read loop is async).
        let mut got = Vec::new();
        for _ in 0..50 {
            got = mgr.output_snapshot(id_a).await.unwrap();
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(got, b"hello");

        // Property 22: B's buffer is untouched by activity on A.
        assert!(mgr.output_snapshot(id_b).await.unwrap().is_empty());
    }

    /// A recording [`SessionOutputSink`] that captures every `(session, chunk)`
    /// it is handed, so a test can assert what streamed out and under which id.
    #[derive(Default)]
    struct RecordingOutputSink {
        emitted: Mutex<Vec<(SessionId, Vec<u8>)>>,
    }

    impl SessionOutputSink for RecordingOutputSink {
        fn emit(&self, session: SessionId, chunk: &[u8]) {
            // `try_lock` keeps `emit` non-blocking as the contract requires;
            // under the single-threaded test driver it never contends.
            if let Ok(mut v) = self.emitted.try_lock() {
                v.push((session, chunk.to_vec()));
            }
        }
    }

    #[tokio::test]
    async fn read_loop_streams_output_to_the_sink_tagged_by_session_and_isolated() {
        // Task 20.2 / Requirement 9.2: the read loop forwards each chunk to the
        // output sink tagged with its own session id, and one session's output
        // never surfaces under another session's id.
        let sink = Arc::new(RecordingOutputSink::default());
        let mgr = SessionManager::with_output_sink(Arc::clone(&sink) as Arc<dyn SessionOutputSink>);

        let (a, tx_a, _ca, _sa) = source(SessionState::Connected);
        let (b, _tx_b, _cb, _sb) = source(SessionState::Connected);
        let id_a = mgr.open_session(a).await.unwrap();
        let id_b = mgr.open_session(b).await.unwrap();

        // Drive only session A's remote output.
        tx_a.send(Some(b"hello".to_vec())).unwrap();

        // Wait until the sink has recorded A's chunk (read loop is async).
        let mut recorded = Vec::new();
        for _ in 0..50 {
            recorded = sink.emitted.lock().await.clone();
            if !recorded.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Exactly A's chunk streamed, tagged with A's id — never B's.
        assert_eq!(recorded, vec![(id_a, b"hello".to_vec())]);
        assert!(recorded.iter().all(|(s, _)| *s != id_b));
    }

    #[tokio::test]
    async fn send_input_routes_to_the_sessions_write_path() {
        let mgr = SessionManager::new();
        let (src, _rx, _closed, sent) = source(SessionState::Connected);
        let id = mgr.open_session(src).await.unwrap();

        mgr.send_input(id, b"ls\n".to_vec()).await.unwrap();

        // Wait for the write-path task to drain the channel.
        let mut recorded = Vec::new();
        for _ in 0..50 {
            recorded = sent.lock().await.clone();
            if !recorded.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(recorded, vec![b"ls\n".to_vec()]);
    }

    // --- run_script against a live session (task 26.5) --------------------

    #[tokio::test]
    async fn run_script_drives_the_live_session_and_returns_the_outcome() {
        use crate::script::{DefaultScriptEngine, Script, ScriptOutcome, ScriptStep};

        let mgr = SessionManager::new();
        // Keep the recv sender so the connection's read loop stays alive while
        // the script sends its (ungated) commands.
        let (src, _rx, _closed, sent) = source(SessionState::Connected);
        let id = mgr.open_session(src).await.unwrap();

        let engine = DefaultScriptEngine::with_monotonic_clock();
        let script = Script::new(vec![
            ScriptStep::command("uname -a"),
            ScriptStep::command("whoami"),
        ]);

        let outcome = mgr.run_script(id, &engine, &script).await.unwrap();
        assert_eq!(outcome, ScriptOutcome::Success { sent: 2 });

        // Each command reached the session's connection (newline-terminated),
        // in order. The script sends directly over the locked conn, so these
        // appear on the same `sent` recorder the write path uses.
        let recorded: Vec<String> = sent
            .lock()
            .await
            .iter()
            .map(|b| String::from_utf8_lossy(b).trim_end().to_string())
            .collect();
        assert_eq!(recorded, vec!["uname -a", "whoami"]);
    }

    #[tokio::test]
    async fn run_script_unknown_session_is_a_coded_error() {
        use crate::script::{DefaultScriptEngine, Script, ScriptStep};

        let mgr = SessionManager::new();
        let engine = DefaultScriptEngine::with_monotonic_clock();
        let script = Script::new(vec![ScriptStep::command("noop")]);

        let err = mgr
            .run_script(SessionId::new(), &engine, &script)
            .await
            .unwrap_err();
        assert_eq!(err.code, SESSION_NOT_FOUND_CODE);
    }

    // Feature: remote-connection-manager, Property 25 (unit) / Requirement 9.7:
    // explicit coverage of the active-tab close confirmation prompt. Closing a
    // tab that hosts an *active* (connected) Session must request confirmation;
    // declining preserves the Session, its tab, its state, and its connection,
    // and leaves it closable later once confirmed.
    #[tokio::test]
    async fn active_tab_close_prompts_for_confirmation_and_decline_preserves_everything() {
        let mgr = SessionManager::new();
        let (src, _rx, closed, _sent) = source(SessionState::Connected);
        let id = mgr.open_session(src).await.unwrap();

        // Precondition: the session is active and is the active tab.
        assert_eq!(mgr.session_state(id).await, Some(SessionState::Connected));
        assert_eq!(mgr.active_tab().await, Some(id));

        // Attempting to close without confirmation is gated: the confirmation
        // prompt is signalled by the stable code and NO teardown occurs.
        let err = mgr.close_tab(id, false).await.unwrap_err();
        assert_eq!(err.code, CONFIRMATION_REQUIRED_CODE);

        // Declining preserves the Session, its tab, its active marker, its
        // state, and — crucially — its network connection (close() not called).
        assert_eq!(mgr.open_count().await, 1);
        assert_eq!(mgr.active_tab().await, Some(id));
        assert_eq!(mgr.session_state(id).await, Some(SessionState::Connected));
        assert!(!closed.load(Ordering::SeqCst), "declining must not release the connection");

        // A second decline is just as inert — the gate is stable across retries.
        let err2 = mgr.close_tab(id, false).await.unwrap_err();
        assert_eq!(err2.code, CONFIRMATION_REQUIRED_CODE);
        assert_eq!(mgr.open_count().await, 1);
        assert!(!closed.load(Ordering::SeqCst));

        // Once confirmed, the same close now tears the Session down.
        mgr.close_tab(id, true).await.unwrap();
        assert_eq!(mgr.open_count().await, 0);
        assert_eq!(mgr.active_tab().await, None);
        assert!(closed.load(Ordering::SeqCst), "confirming must release the connection");
    }

    /// Property-based tests for the Session_Manager (tasks 13.2–13.5).
    ///
    /// The `SessionManager` surface is async; proptest bodies are sync, so each
    /// property builds a single-threaded Tokio runtime and `block_on`s the
    /// operations. The in-module [`MockConn`] is reused as a no-network
    /// connection so no socket is ever opened. All work is in-memory and fast,
    /// so each property runs the default-sized case budget (≥100).
    mod properties {
        use super::*;
        use proptest::prelude::*;
        use tokio::runtime::Runtime;

        /// A fresh current-thread runtime for driving one property case.
        fn runtime() -> Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime")
        }

        /// Poll `output_snapshot(id)` until it reaches `expected_len` bytes or
        /// the poll budget is exhausted; returns the final snapshot. The read
        /// loop is async, so output arrives after `open_session` returns.
        async fn await_output(mgr: &SessionManager, id: SessionId, expected_len: usize) -> Vec<u8> {
            let mut got = Vec::new();
            for _ in 0..200 {
                got = mgr.output_snapshot(id).await.unwrap_or_default();
                if got.len() >= expected_len {
                    break;
                }
                tokio::task::yield_now().await;
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            got
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            // Feature: remote-connection-manager, Property 22: Session isolation
            //
            // *For any* set of open Sessions and any activity performed on one
            // Session, the connection state and output buffer of every other
            // Session remain unchanged.
            //
            // Validates: Requirements 9.2
            #[test]
            fn session_isolation(
                // Number of sibling sessions (plus the one we drive); kept well
                // within MAX_CONCURRENT so no case is rejected for capacity.
                siblings in 1usize..6,
                payload in proptest::collection::vec(any::<u8>(), 1..64),
            ) {
                runtime().block_on(async {
                    let mgr = SessionManager::new();

                    // Open the session we will drive, plus a set of siblings.
                    // Hold each sibling's recv sender so its read loop stays
                    // alive and idle (its buffer must remain empty).
                    let (driven_src, driven_tx, _dc, _ds) = source(SessionState::Connected);
                    let driven = mgr.open_session(driven_src).await.unwrap();

                    let mut others = Vec::new();
                    for _ in 0..siblings {
                        let (src, tx, _c, _s) = source(SessionState::Connected);
                        let id = mgr.open_session(src).await.unwrap();
                        // Keep tx alive so the sibling read loop blocks on recv.
                        others.push((id, tx));
                    }

                    // Snapshot every sibling's state + buffer BEFORE the activity.
                    let mut before = Vec::new();
                    for (id, _tx) in &others {
                        let st = mgr.session_state(*id).await;
                        let buf = mgr.output_snapshot(*id).await.unwrap();
                        before.push((*id, st, buf));
                    }

                    // Activity on exactly one session: push output into it.
                    driven_tx.send(Some(payload.clone())).unwrap();
                    let driven_buf = await_output(&mgr, driven, payload.len()).await;
                    prop_assert_eq!(driven_buf, payload, "driven session must receive its own output");

                    // Property 22: every other session's state and buffer are
                    // unchanged by the activity on `driven`.
                    for (id, st_before, buf_before) in before {
                        let st_after = mgr.session_state(id).await;
                        let buf_after = mgr.output_snapshot(id).await.unwrap();
                        prop_assert_eq!(st_after, st_before, "sibling state must be unchanged");
                        prop_assert!(buf_after.is_empty(), "sibling buffer must stay empty");
                        prop_assert_eq!(buf_after, buf_before, "sibling buffer must be unchanged");
                    }

                    Ok(())
                })?;
            }

            // Feature: remote-connection-manager, Property 23: Concurrent-session capacity is bounded
            //
            // *For any* sequence of open and close operations, the number of
            // open Sessions never exceeds the maximum (at least 20 supported);
            // when an open is rejected for exceeding the limit, all existing
            // Sessions are preserved.
            //
            // Validates: Requirements 9.3, 9.4
            #[test]
            fn concurrent_capacity_is_bounded(
                // A sequence of operations: true = open, false = close.
                // Length comfortably exceeds MAX_CONCURRENT so the limit is hit.
                ops in proptest::collection::vec(any::<bool>(), 1..(MAX_CONCURRENT * 2 + 5)),
            ) {
                runtime().block_on(async {
                    let mgr = SessionManager::new();
                    // Track live ids as a stack so a "close" removes a real one.
                    let mut live: Vec<SessionId> = Vec::new();

                    for open in ops {
                        if open {
                            // Non-connected sessions close without confirmation,
                            // keeping the sequence deterministic.
                            let (src, _tx, _c, _s) = source(SessionState::Disconnected);
                            let count_before = mgr.open_count().await;
                            // Snapshot existing ids to prove preservation on reject.
                            let existing = live.clone();

                            match mgr.open_session(src).await {
                                Ok(id) => {
                                    prop_assert!(count_before < MAX_CONCURRENT);
                                    live.push(id);
                                }
                                Err(e) => {
                                    // Rejection only happens at the limit...
                                    prop_assert_eq!(e.code, CAPACITY_REACHED_CODE);
                                    prop_assert_eq!(count_before, MAX_CONCURRENT);
                                    // ...and every existing session is preserved.
                                    prop_assert_eq!(mgr.open_count().await, MAX_CONCURRENT);
                                    for id in &existing {
                                        prop_assert!(mgr.session_state(*id).await.is_some());
                                    }
                                }
                            }
                        } else if let Some(id) = live.pop() {
                            mgr.close_tab(id, false).await.unwrap();
                        }

                        // Invariant after every step: never over the limit, and
                        // the count agrees with the live set.
                        let count = mgr.open_count().await;
                        prop_assert!(count <= MAX_CONCURRENT, "open count must never exceed the limit");
                        prop_assert_eq!(count, live.len());
                    }

                    Ok(())
                })?;
            }

            // Feature: remote-connection-manager, Property 24: Exactly one active tab after selection
            //
            // *For any* set of open tabs, after activating a tab exactly one tab
            // is active and it is the selected tab.
            //
            // Validates: Requirements 9.5
            #[test]
            fn exactly_one_active_tab_after_selection(
                tab_count in 1usize..(MAX_CONCURRENT + 1),
                // Which open tab to select, chosen by index into the open set.
                select_seed in any::<usize>(),
            ) {
                runtime().block_on(async {
                    let mgr = SessionManager::new();
                    let mut ids = Vec::new();
                    for _ in 0..tab_count {
                        let (src, _tx, _c, _s) = source(SessionState::Connecting);
                        ids.push(mgr.open_session(src).await.unwrap());
                    }

                    let chosen = ids[select_seed % ids.len()];
                    mgr.activate_tab(chosen).await.unwrap();

                    // Exactly one tab is active and it is the selected one. The
                    // active marker is a single Option<SessionId>, so "exactly
                    // one active" is witnessed by it equalling the selection and
                    // by the selection being an open tab.
                    prop_assert_eq!(mgr.active_tab().await, Some(chosen));
                    prop_assert!(ids.contains(&chosen));
                    // Re-selecting the same tab is idempotent — still exactly one.
                    mgr.activate_tab(chosen).await.unwrap();
                    prop_assert_eq!(mgr.active_tab().await, Some(chosen));

                    Ok(())
                })?;
            }

            // Feature: remote-connection-manager, Property 25: Closing a tab releases its session resources
            //
            // *For any* open Session, closing its tab terminates the Session,
            // releases its memory and network resources, and removes the tab.
            //
            // Validates: Requirements 9.6
            #[test]
            fn closing_a_tab_releases_its_session_resources(
                // A set of open sessions; we close one and assert the rest live.
                extra in 0usize..5,
                close_seed in any::<usize>(),
            ) {
                runtime().block_on(async {
                    let mgr = SessionManager::new();

                    // Open the target (Disconnected so it closes without a
                    // confirmation round-trip) and record its connection's
                    // close() flag so we can prove the network resource is
                    // released.
                    let mut ids = Vec::new();
                    let mut flags = Vec::new();
                    for _ in 0..(extra + 1) {
                        let (src, _tx, closed, _s) = source(SessionState::Disconnected);
                        let id = mgr.open_session(src).await.unwrap();
                        ids.push(id);
                        flags.push(closed);
                    }

                    let idx = close_seed % ids.len();
                    let target = ids[idx];
                    let target_flag = Arc::clone(&flags[idx]);

                    let count_before = mgr.open_count().await;
                    prop_assert!(!target_flag.load(Ordering::SeqCst));

                    mgr.close_tab(target, false).await.unwrap();

                    // The tab is removed: it no longer appears and the count drops.
                    prop_assert_eq!(mgr.open_count().await, count_before - 1);
                    prop_assert!(mgr.session_state(target).await.is_none(), "closed session must be gone");
                    prop_assert!(mgr.output_snapshot(target).await.is_none(), "closed session's buffer must be gone");
                    // Network resource released: close() was invoked on its conn.
                    prop_assert!(target_flag.load(Ordering::SeqCst), "closing must call SessionConn::close");

                    // Every other session is untouched (still present).
                    for (i, id) in ids.iter().enumerate() {
                        if i == idx {
                            continue;
                        }
                        prop_assert!(mgr.session_state(*id).await.is_some(), "sibling must remain open");
                    }

                    Ok(())
                })?;
            }
        }
    }
}
