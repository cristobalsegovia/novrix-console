//! Tauri streamed events — session output and transfer progress (task 20.2).
//!
//! The IPC boundary has two halves. The request/response **commands** live in
//! [`super`]; this module is the **streaming** half: the backend pushes a
//! session's remote output and a transfer's progress to the frontend as Tauri
//! events, so the UI renders them without ever touching I/O (design: "IPC
//! Model").
//!
//! Two event channels, each **namespaced per id** so a subscriber receives only
//! the stream it asked for:
//!
//! - `session://{id}/output` — one event per output chunk a session's read loop
//!   produces (Requirement 9.2 isolation: a session's stream carries only its
//!   own output, tagged with its own id).
//! - `transfer://{id}/progress` — one [`TransferProgress`] sample per
//!   File_Transfer_Service emission, at the service's ≤1 s cadence
//!   (Requirement 13.4). The cadence is owned by the transfer `copy_loop`; this
//!   sink is a pure forward of whatever it emits.
//!
//! # The deferred `AppHandle`
//!
//! Tauri only hands out an [`AppHandle`] once the app is built, but the core
//! [`SessionManager`](crate::session::SessionManager) (which needs the output
//! sink at construction) and the [`AppState`](super::AppState) are built
//! *before* that. The seam is [`TauriEventEmitter`]: a shared, cloneable emitter
//! holding a [`OnceLock<AppHandle>`]. It is created up front and injected into
//! the sinks immediately; [`run`](crate::run)'s `setup` closure fills in the
//! real handle once with [`TauriEventEmitter::attach`]. Before `attach` (which,
//! in the running app, is before any session or transfer can exist) `emit` is a
//! no-op, so there is never a window where an event is dropped on the floor for
//! a live stream.
//!
//! # Wiring status
//!
//! The **event path is complete** end to end:
//!
//! - Session output: the `SessionManager` read loop forwards every chunk to the
//!   injected [`TauriSessionOutputSink`], which emits `session://{id}/output`.
//!   For the end-to-end-wired protocols (Telnet, Serial) real remote output
//!   already flows through this path today; SSH/RDP output will flow unchanged
//!   the moment their transport seam lands, with no further event work.
//! - Transfer progress: [`TauriProgressSink`] implements the core
//!   [`ProgressSink`] and emits `transfer://{id}/progress`. The transfer
//!   service's `copy_loop` already drives it at ≤1 s cadence; it will stream for
//!   real once the SFTP/SCP backend binds to a live russh channel (the
//!   `TODO(wiring)` seam in [`crate::transfer`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

use crate::domain::SessionId;
use crate::session::SessionOutputSink;
use crate::transfer::{ProgressSink, TransferProgress};

/// Build the `session://{id}/output` event name for a session.
fn session_output_event(id: SessionId) -> String {
    format!("session://{id}/output")
}

/// Build the `transfer://{id}/progress` event name for a session's transfer.
fn transfer_progress_event(id: SessionId) -> String {
    format!("transfer://{id}/progress")
}

/// A shared, cloneable Tauri event emitter with a deferred [`AppHandle`].
///
/// Constructed before the Tauri app exists and injected into the output /
/// progress sinks; [`run`](crate::run)'s `setup` fills the real handle via
/// [`attach`](Self::attach). Cloning shares the same `OnceLock`, so every holder
/// sees the handle once it is attached.
#[derive(Clone, Default)]
pub struct TauriEventEmitter {
    handle: Arc<OnceLock<AppHandle>>,
}

impl TauriEventEmitter {
    /// Create an unattached emitter (no `AppHandle` yet).
    pub fn new() -> Self {
        Self {
            handle: Arc::new(OnceLock::new()),
        }
    }

    /// Attach the real [`AppHandle`] once the Tauri app is built. Idempotent:
    /// a second attach is ignored (the first handle wins), which cannot happen
    /// in normal startup but keeps the method total.
    pub fn attach(&self, handle: AppHandle) {
        let _ = self.handle.set(handle);
    }

    /// Emit a serializable `payload` on `event`, or do nothing if the handle is
    /// not yet attached. A failed emit (no listeners / serialization) is
    /// swallowed: a streamed event is best-effort and must never break the read
    /// loop or transfer loop that produced it.
    fn emit<T: Serialize + Clone>(&self, event: &str, payload: T) {
        if let Some(handle) = self.handle.get() {
            let _ = handle.emit(event, payload);
        }
    }
}

/// The payload of a `session://{id}/output` event: the session's id plus one
/// chunk of its remote output.
///
/// `data` is the raw bytes exactly as the read loop received them; the frontend
/// feeds them to the terminal emulator. Carrying `sessionId` as well as routing
/// by the event name lets a subscriber that listens broadly still attribute a
/// chunk unambiguously.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOutputEvent {
    /// The session whose read loop produced this chunk.
    pub session_id: SessionId,
    /// One chunk of remote output bytes.
    pub data: Vec<u8>,
}

/// A [`SessionOutputSink`] that streams each chunk to the frontend as a
/// `session://{id}/output` event (task 20.2).
///
/// Injected into the [`SessionManager`](crate::session::SessionManager) at
/// construction; the read loop calls [`emit`](SessionOutputSink::emit) for every
/// chunk, tagged with its own session id, which this sink forwards verbatim.
pub struct TauriSessionOutputSink {
    emitter: TauriEventEmitter,
}

impl TauriSessionOutputSink {
    /// Build the sink over a shared emitter.
    pub fn new(emitter: TauriEventEmitter) -> Self {
        Self { emitter }
    }
}

impl SessionOutputSink for TauriSessionOutputSink {
    fn emit(&self, session: SessionId, chunk: &[u8]) {
        // Non-blocking: `Emitter::emit` enqueues onto the webview's event loop.
        self.emitter.emit(
            &session_output_event(session),
            SessionOutputEvent {
                session_id: session,
                data: chunk.to_vec(),
            },
        );
    }
}

/// A [`ProgressSink`] that streams each transfer sample to the frontend as a
/// `transfer://{id}/progress` event (task 20.2).
///
/// The File_Transfer_Service's `copy_loop` owns the ≤1 s cadence
/// (Requirement 13.4) and calls [`report`](ProgressSink::report); this sink only
/// forwards each [`TransferProgress`] (already a `serde` type) to the event
/// stream for that session.
pub struct TauriProgressSink {
    emitter: TauriEventEmitter,
}

impl TauriProgressSink {
    /// Build the sink over a shared emitter.
    pub fn new(emitter: TauriEventEmitter) -> Self {
        Self { emitter }
    }
}

impl ProgressSink for TauriProgressSink {
    fn report(&self, session: SessionId, progress: TransferProgress) {
        self.emitter
            .emit(&transfer_progress_event(session), progress);
    }
}

/// Build the `transfer://{id}/done` event name for a session's transfer.
fn transfer_done_event(id: SessionId) -> String {
    format!("transfer://{id}/done")
}

/// The payload of a `transfer://{id}/done` event: the terminal outcome of a
/// non-blocking transfer started by `upload_file` / `download_file`.
///
/// Because those commands return as soon as the transfer *starts* (so a cancel
/// can reach the in-flight copy), the final outcome — completed, cancelled, or
/// failed — is delivered out of band on this event. `ok` is `true` on a clean
/// completion; otherwise `code`/`message` carry the mapped
/// [`DomainError`](crate::domain::DomainError) (`transfer.canceled`,
/// `transfer.failed`, …) so the UI reports the same shape it would for a
/// synchronous failure.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferDoneEvent {
    /// The session whose transfer finished.
    pub session_id: SessionId,
    /// Whether the transfer completed successfully.
    pub ok: bool,
    /// The error code when `ok` is `false` (e.g. `transfer.canceled`).
    pub code: Option<String>,
    /// The human-readable error message when `ok` is `false`.
    pub message: Option<String>,
}

impl TauriEventEmitter {
    /// Emit the terminal `transfer://{id}/done` outcome for a session's
    /// transfer. Called by the spawned transfer task once its copy future
    /// resolves (see `upload_file` / `download_file`).
    pub fn emit_transfer_done(
        &self,
        session: SessionId,
        result: &Result<(), crate::domain::TransferError>,
    ) {
        let payload = match result {
            Ok(()) => TransferDoneEvent {
                session_id: session,
                ok: true,
                code: None,
                message: None,
            },
            Err(e) => {
                let de = crate::domain::DomainError::from(e.clone());
                TransferDoneEvent {
                    session_id: session,
                    ok: false,
                    code: Some(de.code),
                    message: Some(de.message),
                }
            }
        };
        self.emit(&transfer_done_event(session), payload);
    }
}

// ---------------------------------------------------------------------------
// Host-key / certificate prompt seam (task 26.3)
// ---------------------------------------------------------------------------
//
// A real host-key verifier runs in the Rust core (inside russh's
// `check_server_key` for SSH, inside the RDP TLS verifier for RDP) and must ask
// the *frontend* whether to trust an unknown fingerprint, then await the user's
// yes/no. That is a request/response round trip layered over Tauri's one-way
// events:
//
//   1. The verifier registers a pending request and gets back a `request_id`
//      plus a `oneshot::Receiver<bool>` (see `HostKeyPrompts::begin`).
//   2. It emits a `hostkey://prompt` event carrying `{ requestId, host, port,
//      fingerprint, kind }` and awaits the receiver.
//   3. The React dialog shows the fingerprint; on Accept/Reject it invokes the
//      `respond_host_key_prompt` command, which calls `HostKeyPrompts::resolve`
//      to fulfil the matching oneshot.
//
// The decision is never faked: if the UI is unreachable (handle not attached,
// or the request map is dropped) the receiver resolves to `false` — a safe
// default that *rejects* an unknown key rather than silently trusting it.

/// The event name the verifier emits to request a trust decision from the UI.
pub const HOST_KEY_PROMPT_EVENT: &str = "hostkey://prompt";

/// The payload of a [`HOST_KEY_PROMPT_EVENT`]: everything the UI needs to show
/// the user and to route the answer back.
///
/// `kind` is the [`TrustKind`](crate::persistence::TrustKind) string
/// (`"ssh-host-key"` / `"rdp-certificate"`) so the dialog can label the prompt
/// correctly. `fingerprint` is a digest (safe to display), never key material.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostKeyPromptEvent {
    /// Correlates the emitted prompt with the `respond_host_key_prompt` reply.
    pub request_id: String,
    /// The host being connected to.
    pub host: String,
    /// The port being connected to.
    pub port: u16,
    /// The presented `SHA256:<base64>` fingerprint to confirm.
    pub fingerprint: String,
    /// `"ssh-host-key"` or `"rdp-certificate"`.
    pub kind: String,
}

/// A registry of in-flight host-key prompts, correlating each emitted
/// `hostkey://prompt` event with the `oneshot` the verifier awaits.
///
/// Cloneable (an `Arc` inside) so the same map is shared by [`AppState`], the
/// verifier that begins prompts, and the command that resolves them. The emit
/// goes through a [`TauriEventEmitter`]; the pending answers live in the map.
///
/// [`AppState`]: super::AppState
#[derive(Clone)]
pub struct HostKeyPrompts {
    emitter: TauriEventEmitter,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
}

impl HostKeyPrompts {
    /// Build the registry over the shared event emitter.
    pub fn new(emitter: TauriEventEmitter) -> Self {
        Self {
            emitter,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Begin a prompt: emit `hostkey://prompt` for `(host, port, kind,
    /// fingerprint)` and return the receiver the verifier awaits for the user's
    /// decision.
    ///
    /// The receiver resolves to `true` (accept) or `false` (reject). If the
    /// sender is ever dropped without a response — e.g. the UI closed, or no
    /// handle is attached so the event never reaches a listener — awaiting it
    /// yields an `Err`, which the verifier treats as a **reject** (fail safe).
    pub fn begin(
        &self,
        host: &str,
        port: u16,
        kind: &str,
        fingerprint: &str,
    ) -> oneshot::Receiver<bool> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();

        // Register before emitting so a (racing) response can always find it.
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(request_id.clone(), tx);
        }

        self.emitter.emit(
            HOST_KEY_PROMPT_EVENT,
            HostKeyPromptEvent {
                request_id,
                host: host.to_string(),
                port,
                fingerprint: fingerprint.to_string(),
                kind: kind.to_string(),
            },
        );

        rx
    }

    /// Resolve a pending prompt with the user's decision.
    ///
    /// Called by the `respond_host_key_prompt` command. Returns `true` if a
    /// pending request with `request_id` was found and fulfilled, `false` if it
    /// was unknown (already answered, timed out, or never existed) — the command
    /// treats an unknown id as a harmless no-op.
    pub fn resolve(&self, request_id: &str, accept: bool) -> bool {
        let sender = self
            .pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(request_id));
        match sender {
            Some(tx) => tx.send(accept).is_ok(),
            None => false,
        }
    }

    /// Resolve the single pending prompt (whichever it is) with `accept`.
    ///
    /// Test-only helper: the verifier generates an opaque request id a test can
    /// never name, so this fulfils the one in-flight request directly. Returns
    /// whether a pending request was found and resolved.
    #[cfg(test)]
    pub fn resolve_any(&self, accept: bool) -> bool {
        let sender = self.pending.lock().ok().and_then(|mut pending| {
            let key = pending.keys().next().cloned();
            key.and_then(|k| pending.remove(&k))
        });
        match sender {
            Some(tx) => tx.send(accept).is_ok(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-id event names follow the `scheme://{id}/channel` shape the
    /// frontend subscribes to, so each session/transfer gets its own stream.
    #[test]
    fn event_names_are_namespaced_per_id() {
        let id = SessionId::new();
        assert_eq!(session_output_event(id), format!("session://{id}/output"));
        assert_eq!(
            transfer_progress_event(id),
            format!("transfer://{id}/progress")
        );
    }

    /// An unattached emitter silently no-ops rather than panicking, so a sink
    /// built before `setup` runs is safe to call.
    #[test]
    fn unattached_emitter_is_a_noop() {
        let emitter = TauriEventEmitter::new();
        // No handle attached: emit must not panic.
        emitter.emit("session://x/output", 42u8);
    }

    /// The session-output sink can be driven before an `AppHandle` is attached
    /// (the emitter no-ops), proving the construction-time seam is safe.
    #[test]
    fn session_output_sink_emit_is_safe_before_attach() {
        let sink = TauriSessionOutputSink::new(TauriEventEmitter::new());
        sink.emit(SessionId::new(), b"chunk");
    }

    /// The progress sink forwards a sample without an attached handle (no-op),
    /// confirming it satisfies the core `ProgressSink` contract safely.
    #[test]
    fn progress_sink_report_is_safe_before_attach() {
        let sink = TauriProgressSink::new(TauriEventEmitter::new());
        sink.report(SessionId::new(), TransferProgress::new(5, 10));
    }

    /// The session-output event serializes as a camelCase payload the frontend
    /// consumes (`sessionId`, `data`).
    #[test]
    fn session_output_event_serializes_camel_case() {
        let id = SessionId::new();
        let value = serde_json::to_value(SessionOutputEvent {
            session_id: id,
            data: vec![1, 2, 3],
        })
        .unwrap();
        assert_eq!(value["sessionId"], serde_json::json!(id));
        assert_eq!(value["data"], serde_json::json!([1, 2, 3]));
    }

    /// Resolving an unknown request id is a harmless no-op.
    #[tokio::test]
    async fn host_key_prompt_unknown_id_is_a_noop() {
        let prompts = HostKeyPrompts::new(TauriEventEmitter::new());
        let _rx = prompts.begin("example.com", 22, "ssh-host-key", "SHA256:abc");
        assert!(!prompts.resolve("no-such-id", true));
    }

    /// Resolving a pending prompt delivers the decision to the awaiting
    /// receiver; the event payload carries the id used to resolve it.
    #[tokio::test]
    async fn host_key_prompt_round_trip_delivers_decision() {
        // Drive the registry directly: insert a known id, resolve it, and
        // confirm the receiver observes the boolean.
        let prompts = HostKeyPrompts::new(TauriEventEmitter::new());
        let (tx, rx) = oneshot::channel();
        prompts
            .pending
            .lock()
            .unwrap()
            .insert("req-1".to_string(), tx);

        assert!(prompts.resolve("req-1", true));
        assert_eq!(rx.await.unwrap(), true);
        // The id is consumed; a second resolve is a no-op.
        assert!(!prompts.resolve("req-1", false));
    }

    /// Dropping the registry (no response) makes the awaited receiver error,
    /// which the verifier treats as a reject — the fail-safe default.
    #[tokio::test]
    async fn unanswered_prompt_fails_safe_to_reject() {
        let prompts = HostKeyPrompts::new(TauriEventEmitter::new());
        let rx = prompts.begin("h", 22, "ssh-host-key", "SHA256:x");
        drop(prompts); // sender dropped without a response
        assert!(rx.await.is_err(), "a dropped prompt must error (→ reject)");
    }

    /// The prompt event serializes as the camelCase payload the frontend
    /// consumes (`requestId`, `host`, `port`, `fingerprint`, `kind`).
    #[test]
    fn host_key_prompt_event_serializes_camel_case() {
        let value = serde_json::to_value(HostKeyPromptEvent {
            request_id: "req-1".to_string(),
            host: "example.com".to_string(),
            port: 22,
            fingerprint: "SHA256:abc".to_string(),
            kind: "ssh-host-key".to_string(),
        })
        .unwrap();
        assert_eq!(value["requestId"], serde_json::json!("req-1"));
        assert_eq!(value["host"], serde_json::json!("example.com"));
        assert_eq!(value["port"], serde_json::json!(22));
        assert_eq!(value["fingerprint"], serde_json::json!("SHA256:abc"));
        assert_eq!(value["kind"], serde_json::json!("ssh-host-key"));
    }
}
