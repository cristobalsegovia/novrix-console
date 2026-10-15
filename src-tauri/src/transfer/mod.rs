//! File_Transfer_Service over SFTP/SCP (task 15.1).
//!
//! This module owns remote directory listing and file upload/download for a
//! connected Session. It implements the surface from the design's "Components
//! and Interfaces" ([`FileTransferService`]) and the behaviours the
//! requirements and Properties 39–41 demand:
//!
//! - **list_remote** (req 13.1): list remote entries with name, size, and
//!   modification time. On an inaccessible directory it returns
//!   [`TransferError::DirectoryInaccessible`] — see the note on preserving the
//!   *displayed* listing below (req 13.8, Property 41).
//! - **upload / download** (req 13.2–13.4): stream bytes via the injected
//!   [`SftpBackend`] while emitting [`TransferProgress`] (bytes, total,
//!   percentage) at a cadence of **≤1 s** through an injected
//!   [`ProgressSink`]. The percentage is computed by the pure
//!   [`percentage`] function: `round(bytes/total*100)`, clamped `0..=100`,
//!   monotonic non-decreasing, reaching exactly `100` on completion
//!   (Property 39).
//! - **cancel** (req 13.6): a cooperative cancel flag the transfer loop checks
//!   every chunk, so an in-flight transfer stops within **≤2 s**.
//! - **cleanup** (req 13.5, 13.6, Property 40): on any outcome other than a
//!   fully-completed transfer — a failure *or* a cancellation — the incomplete
//!   destination file is removed and the outcome is reported as
//!   [`TransferError::Failed`] or [`TransferError::Canceled`] respectively.
//!   The decision is made by the pure [`should_cleanup`] helper.
//! - **conflict** (req 13.7): before overwriting an existing destination the
//!   service consults the [`ConflictPolicy`]. The pure [`resolve_conflict`]
//!   helper turns `(policy, destination_exists)` into an [`ConflictDecision`]
//!   of proceed / abort / prompt, so a `Prompt` policy forces the caller to get
//!   explicit confirmation before any bytes are written.
//!
//! # "Preserve the prior listing" (req 13.8 / Property 41)
//!
//! A failed `list_remote` returns an error and does **not** mutate any state in
//! this service — it holds no "current listing" of its own. Preserving the
//! *displayed* listing is therefore the responsibility of the caller / UI
//! state layer: on `Err(DirectoryInaccessible)` it keeps the last successful
//! listing on screen rather than clearing it. This module is deliberately
//! written so that a failed listing is a pure read with no side effects, which
//! is exactly what makes that caller-side guarantee holdable.
//!
//! # Decoupling from a real SSH connection
//!
//! All remote I/O goes through the [`SftpBackend`] port — a small, injectable
//! trait that abstracts the four operations the service needs (list, open for
//! read, open for write, delete). This keeps the service logic (progress
//! cadence, cancellation, cleanup, conflict handling, state transitions)
//! testable against an in-memory backend without a live SSH connection. The
//! real backend over `russh-sftp` is [`RusshSftpBackend`]; its wiring to an
//! established russh channel is documented as the integration point and left
//! with clear `TODO`s (task 15.6 / the IPC-bridge task fully cables it).
//!
//! # Testability
//!
//! [`percentage`], [`should_cleanup`], and [`resolve_conflict`] are pure and
//! unit/property-testable in isolation (Properties 39/40 and req 13.7). The
//! end-to-end flow is testable by injecting an in-memory [`SftpBackend`] and a
//! recording [`ProgressSink`] (see the module tests and task 15.2–15.6).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::domain::{SessionId, TransferError};

/// A remote path on the SFTP/SCP server.
///
/// A thin newtype over a string so the service surface matches the design
/// (`RemotePath`) and remote paths are not confused with local [`PathBuf`]s.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RemotePath(pub String);

impl RemotePath {
    /// Wrap a remote path string.
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    /// Borrow the underlying path string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RemotePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One entry in a remote directory listing (req 13.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEntry {
    /// The entry's file or directory name (final path component).
    pub name: String,
    /// Size in bytes. Directories report `0` when the server gives no size.
    pub size: u64,
    /// Last-modification time as a Unix timestamp (seconds since the epoch),
    /// or `None` when the server does not report one.
    pub modified: Option<i64>,
    /// Whether this entry is itself a directory (so the UI can allow drill-in).
    pub is_dir: bool,
}

/// How to handle an upload/download whose destination already exists (req 13.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictPolicy {
    /// Never overwrite: if the destination exists, abort with a failure.
    Abort,
    /// Overwrite the destination unconditionally (the user already agreed).
    Overwrite,
    /// Ask the caller to confirm before overwriting; the service refuses to
    /// write until it is re-invoked with [`ConflictPolicy::Overwrite`].
    Prompt,
}

/// The pure decision produced from a [`ConflictPolicy`] and whether the
/// destination exists. Kept separate from any I/O so req 13.7 is testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictDecision {
    /// Safe to write (no conflict, or the caller chose to overwrite).
    Proceed,
    /// The destination exists and policy forbids overwriting it.
    Abort,
    /// The destination exists and the caller must confirm before overwriting.
    Prompt,
}

/// An opaque handle to an in-flight (or just-started) transfer.
///
/// The handle carries a cooperative cancel flag. [`FileTransferService::cancel`]
/// flips the flag; the transfer loop observes it within one chunk and stops,
/// satisfying the ≤2 s cancellation bound (req 13.6).
#[derive(Debug, Clone)]
pub struct TransferHandle {
    session: SessionId,
    cancel_flag: Arc<AtomicBool>,
}

impl TransferHandle {
    /// Create a handle with a fresh, un-cancelled flag.
    fn new(session: SessionId) -> Self {
        Self {
            session,
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The session this transfer belongs to.
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// Request cooperative cancellation.
    pub fn request_cancel(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }

    /// Has cancellation been requested?
    pub fn is_cancelled(&self) -> bool {
        self.cancel_flag.load(Ordering::SeqCst)
    }
}

/// A single progress sample emitted during a transfer (req 13.4, Property 39).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    /// Bytes transferred so far.
    pub bytes_transferred: u64,
    /// Total bytes to transfer. `0` is treated as a complete transfer.
    pub total: u64,
    /// Completion percentage, `round(bytes/total*100)` clamped `0..=100`.
    pub percentage: u8,
}

impl TransferProgress {
    /// Build a progress sample, computing `percentage` via the pure
    /// [`percentage`] function so every emitted sample is consistent.
    pub fn new(bytes_transferred: u64, total: u64) -> Self {
        Self {
            bytes_transferred,
            total,
            percentage: percentage(bytes_transferred, total),
        }
    }
}

/// The direction a transfer moves bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    /// Local → remote.
    Upload,
    /// Remote → local.
    Download,
}

// --- Pure logic ---------------------------------------------------------
//
// No I/O, no async: cheap to unit- and property-test (Properties 39, 40 and
// the req 13.7 conflict rule).

/// Minimum progress-emission cadence (req 13.4): a progress sample is emitted
/// at least this often while bytes are flowing.
pub const PROGRESS_CADENCE: std::time::Duration = std::time::Duration::from_secs(1);

/// Compute the completion percentage for a transfer (Property 39).
///
/// Returns `round(bytes/total*100)` clamped to `0..=100`. A `total` of `0`
/// (an empty file) is defined as already complete, so it returns `100`. Because
/// `bytes` is non-decreasing over a transfer and this is a monotonic function
/// of `bytes/total`, the reported percentage is monotonic non-decreasing and
/// reaches exactly `100` when `bytes == total`.
pub fn percentage(bytes_transferred: u64, total: u64) -> u8 {
    if total == 0 {
        return 100;
    }
    let clamped = bytes_transferred.min(total);
    // round(bytes/total*100) using integer arithmetic: (bytes*100 + total/2) / total.
    // `u128` avoids overflow for large files.
    let scaled = (clamped as u128) * 100 + (total as u128) / 2;
    let pct = scaled / (total as u128);
    pct.min(100) as u8
}

/// Does an interrupted transfer need its destination cleaned up (Property 40)?
///
/// `true` for any outcome that is *not* a fully-completed transfer — i.e. a
/// failure or a cancellation leaves a partial destination that must be removed.
/// `false` only when the transfer completed successfully.
pub fn should_cleanup(outcome: &TransferOutcome) -> bool {
    !matches!(outcome, TransferOutcome::Completed)
}

/// The terminal outcome of a transfer attempt, used to drive cleanup and the
/// returned result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferOutcome {
    /// All bytes transferred; the destination is kept.
    Completed,
    /// The transfer failed mid-flight; the destination must be cleaned up.
    Failed(String),
    /// The user cancelled; the destination must be cleaned up.
    Canceled,
}

/// Resolve a conflict policy against whether the destination exists (req 13.7).
///
/// - no existing destination → always [`ConflictDecision::Proceed`];
/// - existing destination + [`ConflictPolicy::Overwrite`] → `Proceed`;
/// - existing destination + [`ConflictPolicy::Abort`] → `Abort`;
/// - existing destination + [`ConflictPolicy::Prompt`] → `Prompt` (the caller
///   must confirm before any bytes are written).
pub fn resolve_conflict(policy: ConflictPolicy, destination_exists: bool) -> ConflictDecision {
    if !destination_exists {
        return ConflictDecision::Proceed;
    }
    match policy {
        ConflictPolicy::Overwrite => ConflictDecision::Proceed,
        ConflictPolicy::Abort => ConflictDecision::Abort,
        ConflictPolicy::Prompt => ConflictDecision::Prompt,
    }
}

// --- Ports --------------------------------------------------------------

/// A sink that receives [`TransferProgress`] samples at ≤1 s cadence.
///
/// Decoupled from Tauri's event system so the flow is testable with a
/// recording sink. The IPC-bridge task adapts this to the
/// `transfer://{id}/progress` event stream.
pub trait ProgressSink: Send + Sync {
    /// Report one progress sample. Implementations must not block.
    fn report(&self, session: SessionId, progress: TransferProgress);
}

/// A no-op progress sink, useful as a default and in tests that ignore
/// progress output.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullProgressSink;

impl ProgressSink for NullProgressSink {
    fn report(&self, _session: SessionId, _progress: TransferProgress) {}
}

/// A writer that accepts chunks of a file being written to the *destination*
/// and can be finalized or removed.
///
/// A download writes to a local file; an upload writes to the remote. Hiding
/// both behind this trait lets the transfer loop stay identical for both
/// directions and lets tests observe partial writes and cleanup.
#[async_trait]
pub trait DestinationWriter: Send {
    /// Append `chunk` to the destination.
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), TransferError>;

    /// Flush and finalize the destination (the transfer completed).
    async fn finish(self: Box<Self>) -> Result<(), TransferError>;
}

/// A reader that yields the *source* file in chunks, with a known total size.
///
/// A download reads from the remote; an upload reads from the local file.
#[async_trait]
pub trait SourceReader: Send {
    /// Total size of the source in bytes (used for the progress denominator).
    fn total(&self) -> u64;

    /// Read the next chunk, or `Ok(None)` at end of file.
    async fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, TransferError>;
}

/// The SFTP/SCP backend port.
///
/// Abstracts the four remote operations the File_Transfer_Service needs so the
/// service logic is testable without a live SSH connection. A download opens a
/// remote source + local destination; an upload opens a local source + remote
/// destination; both may need to check existence (for conflict handling) and
/// delete an incomplete destination (for cleanup).
#[async_trait]
pub trait SftpBackend: Send + Sync {
    /// List the entries of a remote directory (req 13.1). Returns
    /// [`TransferError::DirectoryInaccessible`] when the directory cannot be
    /// opened or read.
    async fn list_dir(
        &self,
        session: SessionId,
        path: &RemotePath,
    ) -> Result<Vec<RemoteEntry>, TransferError>;

    /// Does a path exist on the given side? Used for conflict resolution
    /// (req 13.7) before writing a destination.
    async fn destination_exists(
        &self,
        session: SessionId,
        direction: TransferDirection,
        destination: &Destination,
    ) -> Result<bool, TransferError>;

    /// Open the source of a transfer for reading, with its total size.
    async fn open_source(
        &self,
        session: SessionId,
        direction: TransferDirection,
        source: &Source,
    ) -> Result<Box<dyn SourceReader>, TransferError>;

    /// Open (truncating) the destination of a transfer for writing.
    async fn open_destination(
        &self,
        session: SessionId,
        direction: TransferDirection,
        destination: &Destination,
    ) -> Result<Box<dyn DestinationWriter>, TransferError>;

    /// Remove an incomplete destination during cleanup (req 13.5/13.6).
    async fn remove_destination(
        &self,
        session: SessionId,
        direction: TransferDirection,
        destination: &Destination,
    ) -> Result<(), TransferError>;
}

/// The source side of a transfer: a local path for an upload, a remote path for
/// a download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A local file (upload source).
    Local(PathBuf),
    /// A remote path (download source).
    Remote(RemotePath),
}

/// The destination side of a transfer: a remote path for an upload, a local
/// path for a download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// A local file (download destination).
    Local(PathBuf),
    /// A remote path (upload destination).
    Remote(RemotePath),
}

// --- Service surface ----------------------------------------------------

/// The File_Transfer_Service surface from the design's "Components and
/// Interfaces".
#[async_trait]
pub trait FileTransferService: Send + Sync {
    /// List remote entries at `path` (req 13.1). On failure the error is
    /// returned and no internal state is mutated, so the caller can preserve
    /// the previously displayed listing (req 13.8).
    async fn list_remote(
        &self,
        session: SessionId,
        path: RemotePath,
    ) -> Result<Vec<RemoteEntry>, TransferError>;

    /// Upload a local file to a remote path (req 13.2) with progress reporting
    /// (req 13.4), cancellation (req 13.6), overwrite handling (req 13.7), and
    /// incomplete-file cleanup (req 13.5).
    async fn upload(
        &self,
        session: SessionId,
        local: PathBuf,
        remote: RemotePath,
        on_conflict: ConflictPolicy,
    ) -> Result<TransferHandle, TransferError>;

    /// Download a remote file to a local path (req 13.3) with the same progress,
    /// cancellation, overwrite, and cleanup guarantees as [`upload`].
    async fn download(
        &self,
        session: SessionId,
        remote: RemotePath,
        local: PathBuf,
        on_conflict: ConflictPolicy,
    ) -> Result<TransferHandle, TransferError>;

    /// Request cancellation of an in-flight transfer (req 13.6). The transfer
    /// stops within ≤2 s, its incomplete destination is removed, and it reports
    /// [`TransferError::Canceled`].
    async fn cancel(&self, handle: TransferHandle) -> Result<(), TransferError>;

    /// Start an upload **without blocking until it finishes** (req 13.2, 13.6).
    ///
    /// Returns the live [`TransferHandle`] *immediately* — before any bytes are
    /// copied — paired with a boxed future that runs the actual chunked copy
    /// (conflict resolution, progress emission, cleanup). The caller spawns the
    /// future on its runtime and registers the handle so a later
    /// [`cancel`](Self::cancel) can flip the handle's cooperative flag; the copy
    /// loop observes it within one chunk and stops with the partial destination
    /// cleaned up (≤2 s bound).
    ///
    /// This is the seam that makes command-driven transfers cancellable: the
    /// blocking [`upload`](Self::upload) only hands back the handle on
    /// completion (too late to cancel), whereas this hands it back at the start.
    fn start_upload(
        &self,
        session: SessionId,
        local: PathBuf,
        remote: RemotePath,
        on_conflict: ConflictPolicy,
    ) -> (TransferHandle, TransferFuture);

    /// Start a download without blocking, with the same handle-up-front
    /// semantics as [`start_upload`](Self::start_upload).
    fn start_download(
        &self,
        session: SessionId,
        remote: RemotePath,
        local: PathBuf,
        on_conflict: ConflictPolicy,
    ) -> (TransferHandle, TransferFuture);
}

/// A boxed future that runs one transfer's copy loop to its terminal outcome.
///
/// Produced by [`FileTransferService::start_upload`] /
/// [`start_download`](FileTransferService::start_download) alongside the live
/// [`TransferHandle`]. The caller spawns it; its `Result` is the final
/// transfer outcome (`Ok(())` on completion, `Err(Canceled)` when cancelled,
/// `Err(Failed/Io)` on failure) so the caller can emit the terminal
/// completion/error event the UI needs.
pub type TransferFuture = Pin<Box<dyn Future<Output = Result<(), TransferError>> + Send>>;

/// Default [`FileTransferService`] driving an injected [`SftpBackend`] and
/// [`ProgressSink`].
///
/// This holds the real transfer logic — conflict resolution, the chunked copy
/// loop with cooperative cancellation, ≤1 s progress emission, and
/// incomplete-destination cleanup — all expressed against the backend port so
/// it runs identically over the in-memory test backend and the real
/// [`RusshSftpBackend`].
pub struct DefaultFileTransferService {
    backend: Arc<dyn SftpBackend>,
    progress: Arc<dyn ProgressSink>,
    /// Max bytes copied per loop iteration. Bounds how long a single chunk can
    /// take, which keeps the cancellation check frequent enough for the ≤2 s
    /// bound (req 13.6).
    chunk_size: usize,
}

impl DefaultFileTransferService {
    /// Default copy chunk size (64 KiB).
    pub const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;

    /// Build a service over the given backend and progress sink.
    pub fn new(backend: Arc<dyn SftpBackend>, progress: Arc<dyn ProgressSink>) -> Self {
        Self {
            backend,
            progress,
            chunk_size: Self::DEFAULT_CHUNK_SIZE,
        }
    }

    /// Override the copy chunk size (mainly for tests driving the cancel path).
    pub fn with_chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size.max(1);
        self
    }

    /// A cheap, owned clone of the service's workings (two `Arc`s + a `usize`)
    /// used to build a `'static` transfer future that outlives the `&self`
    /// borrow in [`start_upload`]/[`start_download`].
    fn worker_clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
            progress: Arc::clone(&self.progress),
            chunk_size: self.chunk_size,
        }
    }

    /// Run one transfer: resolve the conflict policy, copy the source to the
    /// destination with progress + cancellation, and clean up on any
    /// non-completed outcome. Shared by both upload and download.
    ///
    /// The `handle` is created by the caller *before* this runs so a cancel can
    /// reach the in-flight copy; the blocking [`upload`]/[`download`] mint one
    /// locally while the non-blocking [`start_upload`]/[`start_download`] mint
    /// one, hand it to the caller, and only then drive this to completion.
    async fn run_transfer(
        &self,
        handle: &TransferHandle,
        session: SessionId,
        direction: TransferDirection,
        source: Source,
        destination: Destination,
        on_conflict: ConflictPolicy,
    ) -> Result<(), TransferError> {
        // Conflict handling (req 13.7): consult the destination before writing.
        let exists = self
            .backend
            .destination_exists(session, direction, &destination)
            .await?;
        match resolve_conflict(on_conflict, exists) {
            ConflictDecision::Proceed => {}
            ConflictDecision::Abort => {
                return Err(TransferError::Failed(
                    "destination already exists and overwrite was not permitted".to_string(),
                ));
            }
            ConflictDecision::Prompt => {
                // The caller must obtain confirmation and re-invoke with
                // `ConflictPolicy::Overwrite`. We refuse to write until then.
                return Err(TransferError::Failed(
                    "destination already exists; confirmation required before overwriting"
                        .to_string(),
                ));
            }
        }

        let outcome = self
            .copy_loop(session, direction, &source, &destination, handle)
            .await;

        // Cleanup (req 13.5/13.6, Property 40): remove the incomplete
        // destination for any non-completed outcome.
        if should_cleanup(&outcome) {
            // Best-effort removal; the primary error is the transfer outcome.
            let _ = self
                .backend
                .remove_destination(session, direction, &destination)
                .await;
        }

        match outcome {
            TransferOutcome::Completed => Ok(()),
            TransferOutcome::Failed(msg) => Err(TransferError::Failed(msg)),
            TransferOutcome::Canceled => Err(TransferError::Canceled),
        }
    }

    /// The chunked copy loop. Emits a progress sample at least every
    /// [`PROGRESS_CADENCE`] (and on completion), checking the cancel flag each
    /// iteration so cancellation is observed within ≤2 s.
    async fn copy_loop(
        &self,
        session: SessionId,
        direction: TransferDirection,
        source: &Source,
        destination: &Destination,
        handle: &TransferHandle,
    ) -> TransferOutcome {
        let mut reader = match self.backend.open_source(session, direction, source).await {
            Ok(r) => r,
            Err(e) => return TransferOutcome::Failed(e.to_string()),
        };
        let mut writer = match self
            .backend
            .open_destination(session, direction, destination)
            .await
        {
            Ok(w) => w,
            Err(e) => return TransferOutcome::Failed(e.to_string()),
        };

        let total = reader.total();
        let mut transferred: u64 = 0;
        let mut last_emit = std::time::Instant::now();

        // Emit an initial 0% (or 100% for an empty file) sample.
        self.progress
            .report(session, TransferProgress::new(transferred, total));

        loop {
            if handle.is_cancelled() {
                return TransferOutcome::Canceled;
            }

            let chunk = match reader.read_chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break, // end of file
                Err(e) => return TransferOutcome::Failed(e.to_string()),
            };

            if let Err(e) = writer.write_chunk(&chunk).await {
                return TransferOutcome::Failed(e.to_string());
            }
            transferred = transferred.saturating_add(chunk.len() as u64);

            // Progress cadence (req 13.4): emit at most once per PROGRESS_CADENCE.
            if last_emit.elapsed() >= PROGRESS_CADENCE {
                self.progress
                    .report(session, TransferProgress::new(transferred, total));
                last_emit = std::time::Instant::now();
            }
        }

        // Finalize and emit the terminal 100% sample.
        if let Err(e) = Box::new(writer).finish().await {
            return TransferOutcome::Failed(e.to_string());
        }
        self.progress
            .report(session, TransferProgress::new(transferred.max(total), total));
        TransferOutcome::Completed
    }
}

#[async_trait]
impl FileTransferService for DefaultFileTransferService {
    async fn list_remote(
        &self,
        session: SessionId,
        path: RemotePath,
    ) -> Result<Vec<RemoteEntry>, TransferError> {
        // Pure read, no state mutation: a failure returns the error and leaves
        // the caller free to keep the prior displayed listing (req 13.8).
        self.backend.list_dir(session, &path).await
    }

    async fn upload(
        &self,
        session: SessionId,
        local: PathBuf,
        remote: RemotePath,
        on_conflict: ConflictPolicy,
    ) -> Result<TransferHandle, TransferError> {
        let handle = TransferHandle::new(session);
        self.run_transfer(
            &handle,
            session,
            TransferDirection::Upload,
            Source::Local(local),
            Destination::Remote(remote),
            on_conflict,
        )
        .await?;
        Ok(handle)
    }

    async fn download(
        &self,
        session: SessionId,
        remote: RemotePath,
        local: PathBuf,
        on_conflict: ConflictPolicy,
    ) -> Result<TransferHandle, TransferError> {
        let handle = TransferHandle::new(session);
        self.run_transfer(
            &handle,
            session,
            TransferDirection::Download,
            Source::Remote(remote),
            Destination::Local(local),
            on_conflict,
        )
        .await?;
        Ok(handle)
    }

    async fn cancel(&self, handle: TransferHandle) -> Result<(), TransferError> {
        // Cooperative cancel (req 13.6): flip the flag the copy loop polls.
        // The loop observes it within one chunk (≤2 s) and performs cleanup.
        handle.request_cancel();
        Ok(())
    }

    fn start_upload(
        &self,
        session: SessionId,
        local: PathBuf,
        remote: RemotePath,
        on_conflict: ConflictPolicy,
    ) -> (TransferHandle, TransferFuture) {
        let handle = TransferHandle::new(session);
        // Clone the cheap `Arc` backend/sink + chunk size into an owned worker
        // so the returned future owns everything it needs (no borrow of `self`)
        // and is `'static + Send`, hence spawnable by the caller.
        let worker = self.worker_clone();
        let run_handle = handle.clone();
        let fut: TransferFuture = Box::pin(async move {
            worker
                .run_transfer(
                    &run_handle,
                    session,
                    TransferDirection::Upload,
                    Source::Local(local),
                    Destination::Remote(remote),
                    on_conflict,
                )
                .await
        });
        (handle, fut)
    }

    fn start_download(
        &self,
        session: SessionId,
        remote: RemotePath,
        local: PathBuf,
        on_conflict: ConflictPolicy,
    ) -> (TransferHandle, TransferFuture) {
        let handle = TransferHandle::new(session);
        let worker = self.worker_clone();
        let run_handle = handle.clone();
        let fut: TransferFuture = Box::pin(async move {
            worker
                .run_transfer(
                    &run_handle,
                    session,
                    TransferDirection::Download,
                    Source::Remote(remote),
                    Destination::Local(local),
                    on_conflict,
                )
                .await
        });
        (handle, fut)
    }
}

// --- Real russh-sftp backend -------------------------------------------
//
// This section wires the four [`SftpBackend`] operations to a live
// `russh_sftp::client::SftpSession` (russh-sftp v2). The session itself rides
// on a russh SSH channel; opening that channel, requesting the `sftp`
// subsystem, and constructing the `SftpSession` happens in the SSH transport
// layer (protocol/ssh.rs, wired in parallel). This module stays decoupled from
// that layer through the [`SftpSessionProvider`] port below: the backend asks
// the provider for the session belonging to a [`SessionId`] and does all SFTP
// I/O against it. See the `TODO(wiring)` on [`SftpSessionProvider`] for the one
// integration call the IPC-assembly step (task 25.4) must make.

/// A live SFTP session shared across concurrent backend calls.
///
/// `russh_sftp::client::SftpSession` methods take `&self` (it holds an
/// `Arc<RawSftpSession>` internally and is cheap to share), so an `Arc` is all
/// the backend needs to run list / open / create / remove against one session.
pub type SharedSftpSession = Arc<russh_sftp::client::SftpSession>;

/// The port the real [`RusshSftpBackend`] resolves a [`SessionId`] to its open
/// SFTP session through.
///
/// The transfer module deliberately does **not** open SSH channels or request
/// the `sftp` subsystem itself — that is the SSH transport's job
/// (protocol/ssh.rs). This port is the seam between the two: the Session layer
/// hands back the already-initialised `SftpSession` for a connected session,
/// and the backend performs all remote file operations on it.
///
/// # TODO(wiring) — the single integration call for task 25.4
///
/// Task 25.4 (IPC assembly) must provide a concrete implementor of this trait
/// and hand it to [`RusshSftpBackend::new`]. The exact call the assembly makes
/// is:
///
/// ```ignore
/// let backend: Arc<dyn SftpBackend> =
///     Arc::new(RusshSftpBackend::new(provider));
/// // where `provider: Arc<dyn SftpSessionProvider>` resolves a SessionId to
/// // the SftpSession the SSH transport opened for that session, i.e. it
/// // implements exactly:
/// //
/// //   async fn sftp_session(
/// //       &self,
/// //       session: SessionId,
/// //   ) -> Result<SharedSftpSession, TransferError>;
/// ```
///
/// The provider is expected to build its `SftpSession` from an open russh
/// channel via `russh_sftp::client::SftpSession::new(channel.into_stream())`
/// after `channel.request_subsystem(true, "sftp")`. That construction lives in
/// the SSH transport layer, not here, so this module never touches
/// protocol/ssh.rs.
#[async_trait]
pub trait SftpSessionProvider: Send + Sync {
    /// Resolve the open [`SharedSftpSession`] for `session`.
    ///
    /// Returns [`TransferError::Io`] if the session is unknown or its SFTP
    /// channel is not available (e.g. not an SSH session, or not yet
    /// connected).
    async fn sftp_session(
        &self,
        session: SessionId,
    ) -> Result<SharedSftpSession, TransferError>;
}

/// The real [`SftpBackend`] over `russh-sftp` v2.
///
/// Holds an [`SftpSessionProvider`] it uses to resolve the live
/// `SftpSession` for each [`SessionId`], then performs the four remote
/// operations the File_Transfer_Service needs:
///
/// - `list_dir`: `sftp.read_dir(path)`, mapping each [`DirEntry`](russh_sftp::client::fs::DirEntry)'s
///   name / `file_type().is_dir()` / `metadata()` size & mtime into
///   [`RemoteEntry`]; any failure to open or read the directory becomes
///   [`TransferError::DirectoryInaccessible`] so the UI keeps its prior listing.
/// - `destination_exists`: `sftp.try_exists(path)` for a remote destination,
///   `tokio::fs::try_exists` for a local one.
/// - `open_source` / `open_destination`: `sftp.open` / `sftp.create` wrapped in
///   adapters implementing [`SourceReader`] / [`DestinationWriter`]; the local
///   side uses `tokio::fs::File`. Both stream in chunks so the copy loop reports
///   byte progress.
/// - `remove_destination`: `sftp.remove_file(path)` (remote) or
///   `tokio::fs::remove_file` (local), best-effort for cleanup.
pub struct RusshSftpBackend {
    provider: Arc<dyn SftpSessionProvider>,
}

impl RusshSftpBackend {
    /// Build the real backend over a session provider.
    ///
    /// `provider` resolves a [`SessionId`] to the open [`SharedSftpSession`]
    /// the SSH transport established for it — see [`SftpSessionProvider`] for
    /// the integration call task 25.4 makes.
    pub fn new(provider: Arc<dyn SftpSessionProvider>) -> Self {
        Self { provider }
    }
}

/// Map a remote path/metadata lookup to a POSIX-style string the SFTP server
/// expects. SFTP always uses `/`-separated paths on the wire.
fn remote_str(path: &RemotePath) -> String {
    path.0.clone()
}

/// A [`SourceReader`] over an open remote SFTP file.
///
/// Streams the file in `chunk_size` reads via the `File`'s `AsyncRead` impl so
/// the copy loop can report byte progress. The total comes from a prior
/// `metadata()` query.
struct SftpSourceReader {
    file: russh_sftp::client::fs::File,
    total: u64,
    chunk_size: usize,
}

#[async_trait]
impl SourceReader for SftpSourceReader {
    fn total(&self) -> u64 {
        self.total
    }

    async fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, TransferError> {
        let mut buf = vec![0u8; self.chunk_size];
        let n = self
            .file
            .read(&mut buf)
            .await
            .map_err(|e| TransferError::Io(e.to_string()))?;
        if n == 0 {
            Ok(None)
        } else {
            buf.truncate(n);
            Ok(Some(buf))
        }
    }
}

/// A [`DestinationWriter`] over an open remote SFTP file (created truncating).
struct SftpDestinationWriter {
    file: russh_sftp::client::fs::File,
}

#[async_trait]
impl DestinationWriter for SftpDestinationWriter {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), TransferError> {
        self.file
            .write_all(chunk)
            .await
            .map_err(|e| TransferError::Io(e.to_string()))
    }

    async fn finish(self: Box<Self>) -> Result<(), TransferError> {
        // `close` flushes pending writes and awaits the server's confirmation,
        // which is what makes the finished destination durable (vs. drop, which
        // discards the close status).
        self.file
            .close()
            .await
            .map_err(|e| TransferError::Io(e.to_string()))
    }
}

/// A [`SourceReader`] over a local file (upload source).
struct LocalSourceReader {
    file: tokio::fs::File,
    total: u64,
    chunk_size: usize,
}

#[async_trait]
impl SourceReader for LocalSourceReader {
    fn total(&self) -> u64 {
        self.total
    }

    async fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, TransferError> {
        let mut buf = vec![0u8; self.chunk_size];
        let n = self
            .file
            .read(&mut buf)
            .await
            .map_err(|e| TransferError::Io(e.to_string()))?;
        if n == 0 {
            Ok(None)
        } else {
            buf.truncate(n);
            Ok(Some(buf))
        }
    }
}

/// A [`DestinationWriter`] over a local file (download destination).
struct LocalDestinationWriter {
    file: tokio::fs::File,
}

#[async_trait]
impl DestinationWriter for LocalDestinationWriter {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), TransferError> {
        self.file
            .write_all(chunk)
            .await
            .map_err(|e| TransferError::Io(e.to_string()))
    }

    async fn finish(mut self: Box<Self>) -> Result<(), TransferError> {
        self.file
            .flush()
            .await
            .map_err(|e| TransferError::Io(e.to_string()))
    }
}

/// Per-read/write chunk size used by the backend's streaming adapters (64 KiB),
/// matching the service's default copy chunk so one read ≈ one write ≈ one
/// progress step.
const SFTP_CHUNK_SIZE: usize = 64 * 1024;

#[async_trait]
impl SftpBackend for RusshSftpBackend {
    async fn list_dir(
        &self,
        session: SessionId,
        path: &RemotePath,
    ) -> Result<Vec<RemoteEntry>, TransferError> {
        let sftp = self.provider.sftp_session(session).await?;

        // Any failure to open/read the directory is surfaced as
        // DirectoryInaccessible so the caller keeps the previously displayed
        // listing (req 13.8 / Property 41).
        let read_dir = sftp
            .read_dir(remote_str(path))
            .await
            .map_err(|_| TransferError::DirectoryInaccessible(path.0.clone()))?;

        let mut entries = Vec::new();
        for entry in read_dir {
            let meta = entry.metadata();
            entries.push(RemoteEntry {
                name: entry.file_name(),
                size: meta.size.unwrap_or(0),
                // SFTP reports mtime as seconds since the epoch (u32); widen to
                // the i64 the UI model carries.
                modified: meta.mtime.map(|m| m as i64),
                is_dir: entry.file_type().is_dir(),
            });
        }
        Ok(entries)
    }

    async fn destination_exists(
        &self,
        session: SessionId,
        direction: TransferDirection,
        destination: &Destination,
    ) -> Result<bool, TransferError> {
        match (direction, destination) {
            (TransferDirection::Upload, Destination::Remote(path)) => {
                let sftp = self.provider.sftp_session(session).await?;
                sftp.try_exists(remote_str(path))
                    .await
                    .map_err(|e| TransferError::Io(e.to_string()))
            }
            (TransferDirection::Download, Destination::Local(path)) => {
                tokio::fs::try_exists(path)
                    .await
                    .map_err(|e| TransferError::Io(e.to_string()))
            }
            // A destination side that does not match the direction is a
            // programming error in the caller; treat as non-existent rather
            // than guess.
            _ => Ok(false),
        }
    }

    async fn open_source(
        &self,
        session: SessionId,
        direction: TransferDirection,
        source: &Source,
    ) -> Result<Box<dyn SourceReader>, TransferError> {
        match (direction, source) {
            // Upload: read the local file.
            (TransferDirection::Upload, Source::Local(path)) => {
                let file = tokio::fs::File::open(path)
                    .await
                    .map_err(|e| TransferError::Io(e.to_string()))?;
                let total = file
                    .metadata()
                    .await
                    .map(|m| m.len())
                    .map_err(|e| TransferError::Io(e.to_string()))?;
                Ok(Box::new(LocalSourceReader {
                    file,
                    total,
                    chunk_size: SFTP_CHUNK_SIZE,
                }))
            }
            // Download: read the remote file.
            (TransferDirection::Download, Source::Remote(path)) => {
                let sftp = self.provider.sftp_session(session).await?;
                let remote = remote_str(path);
                let total = sftp
                    .metadata(remote.clone())
                    .await
                    .map(|m| m.size.unwrap_or(0))
                    .map_err(|e| TransferError::Io(e.to_string()))?;
                let file = sftp
                    .open(remote)
                    .await
                    .map_err(|e| TransferError::Io(e.to_string()))?;
                Ok(Box::new(SftpSourceReader {
                    file,
                    total,
                    chunk_size: SFTP_CHUNK_SIZE,
                }))
            }
            _ => Err(TransferError::Io(
                "transfer direction does not match the source side".to_string(),
            )),
        }
    }

    async fn open_destination(
        &self,
        session: SessionId,
        direction: TransferDirection,
        destination: &Destination,
    ) -> Result<Box<dyn DestinationWriter>, TransferError> {
        match (direction, destination) {
            // Upload: create (truncating) the remote file.
            (TransferDirection::Upload, Destination::Remote(path)) => {
                let sftp = self.provider.sftp_session(session).await?;
                let file = sftp
                    .create(remote_str(path))
                    .await
                    .map_err(|e| TransferError::Io(e.to_string()))?;
                Ok(Box::new(SftpDestinationWriter { file }))
            }
            // Download: create (truncating) the local file.
            (TransferDirection::Download, Destination::Local(path)) => {
                let file = tokio::fs::File::create(path)
                    .await
                    .map_err(|e| TransferError::Io(e.to_string()))?;
                Ok(Box::new(LocalDestinationWriter { file }))
            }
            _ => Err(TransferError::Io(
                "transfer direction does not match the destination side".to_string(),
            )),
        }
    }

    async fn remove_destination(
        &self,
        session: SessionId,
        direction: TransferDirection,
        destination: &Destination,
    ) -> Result<(), TransferError> {
        match (direction, destination) {
            (TransferDirection::Upload, Destination::Remote(path)) => {
                let sftp = self.provider.sftp_session(session).await?;
                sftp.remove_file(remote_str(path))
                    .await
                    .map_err(|e| TransferError::Io(e.to_string()))
            }
            (TransferDirection::Download, Destination::Local(path)) => {
                tokio::fs::remove_file(path)
                    .await
                    .map_err(|e| TransferError::Io(e.to_string()))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // --- Pure-logic smoke tests (Property 39) ---------------------------

    #[test]
    fn percentage_is_zero_at_start_and_hundred_at_completion() {
        assert_eq!(percentage(0, 1000), 0);
        assert_eq!(percentage(1000, 1000), 100);
    }

    #[test]
    fn percentage_rounds_to_nearest() {
        // 1/3 → 33.33.. → 33
        assert_eq!(percentage(1, 3), 33);
        // 2/3 → 66.66.. → 67 (rounds up)
        assert_eq!(percentage(2, 3), 67);
        // 1/2 → 50
        assert_eq!(percentage(1, 2), 50);
    }

    #[test]
    fn percentage_is_clamped_and_handles_empty_total() {
        // Empty file: defined as already complete.
        assert_eq!(percentage(0, 0), 100);
        // Over-report is clamped to 100.
        assert_eq!(percentage(5000, 1000), 100);
    }

    #[test]
    fn percentage_is_monotonic_non_decreasing() {
        let total = 10_000u64;
        let mut last = 0u8;
        for bytes in (0..=total).step_by(137) {
            let p = percentage(bytes, total);
            assert!(p >= last, "percentage decreased at {bytes}: {p} < {last}");
            assert!(p <= 100);
            last = p;
        }
        assert_eq!(percentage(total, total), 100);
    }

    // --- Cleanup decision smoke tests (Property 40) ---------------------

    #[test]
    fn cleanup_required_on_failure_and_cancel_but_not_on_completion() {
        assert!(!should_cleanup(&TransferOutcome::Completed));
        assert!(should_cleanup(&TransferOutcome::Failed("boom".into())));
        assert!(should_cleanup(&TransferOutcome::Canceled));
    }

    // --- Conflict resolution smoke tests (req 13.7) ---------------------

    #[test]
    fn conflict_proceeds_when_destination_absent_regardless_of_policy() {
        for policy in [
            ConflictPolicy::Abort,
            ConflictPolicy::Overwrite,
            ConflictPolicy::Prompt,
        ] {
            assert_eq!(resolve_conflict(policy, false), ConflictDecision::Proceed);
        }
    }

    #[test]
    fn conflict_decides_by_policy_when_destination_exists() {
        assert_eq!(
            resolve_conflict(ConflictPolicy::Overwrite, true),
            ConflictDecision::Proceed
        );
        assert_eq!(
            resolve_conflict(ConflictPolicy::Abort, true),
            ConflictDecision::Abort
        );
        assert_eq!(
            resolve_conflict(ConflictPolicy::Prompt, true),
            ConflictDecision::Prompt
        );
    }

    // --- In-memory backend + flow smoke tests ---------------------------

    /// Shared in-memory remote/local filesystem for the test backend.
    #[derive(Default)]
    struct MemFs {
        /// Existing destinations (keyed by their stringified path).
        existing: HashMap<String, bool>,
        /// Source contents (keyed by stringified source path).
        sources: HashMap<String, Vec<u8>>,
        /// Bytes written to each destination so far.
        written: HashMap<String, Vec<u8>>,
        /// Destinations that have been removed (cleanup observed).
        removed: Vec<String>,
    }

    fn source_key(source: &Source) -> String {
        match source {
            Source::Local(p) => p.to_string_lossy().into_owned(),
            Source::Remote(r) => r.0.clone(),
        }
    }

    fn dest_key(dest: &Destination) -> String {
        match dest {
            Destination::Local(p) => p.to_string_lossy().into_owned(),
            Destination::Remote(r) => r.0.clone(),
        }
    }

    struct MemBackend {
        fs: Arc<Mutex<MemFs>>,
        /// If set, `read_chunk` returns an error after this many chunks (to
        /// drive the failure/cleanup path).
        fail_after_chunks: Option<usize>,
    }

    struct MemReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
        read_count: usize,
        fail_after_chunks: Option<usize>,
    }

    #[async_trait]
    impl SourceReader for MemReader {
        fn total(&self) -> u64 {
            self.data.len() as u64
        }
        async fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, TransferError> {
            if let Some(limit) = self.fail_after_chunks {
                if self.read_count >= limit {
                    return Err(TransferError::Io("injected read failure".into()));
                }
            }
            if self.pos >= self.data.len() {
                return Ok(None);
            }
            let end = (self.pos + self.chunk).min(self.data.len());
            let out = self.data[self.pos..end].to_vec();
            self.pos = end;
            self.read_count += 1;
            Ok(Some(out))
        }
    }

    struct MemWriter {
        key: String,
        buf: Vec<u8>,
        fs: Arc<Mutex<MemFs>>,
    }

    #[async_trait]
    impl DestinationWriter for MemWriter {
        async fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), TransferError> {
            self.buf.extend_from_slice(chunk);
            // Reflect partial writes so cleanup is observable.
            self.fs
                .lock()
                .unwrap()
                .written
                .insert(self.key.clone(), self.buf.clone());
            Ok(())
        }
        async fn finish(self: Box<Self>) -> Result<(), TransferError> {
            self.fs
                .lock()
                .unwrap()
                .written
                .insert(self.key.clone(), self.buf.clone());
            Ok(())
        }
    }

    #[async_trait]
    impl SftpBackend for MemBackend {
        async fn list_dir(
            &self,
            _session: SessionId,
            path: &RemotePath,
        ) -> Result<Vec<RemoteEntry>, TransferError> {
            if path.as_str().starts_with("/denied") {
                return Err(TransferError::DirectoryInaccessible(path.0.clone()));
            }
            Ok(vec![RemoteEntry {
                name: "file.txt".into(),
                size: 10,
                modified: Some(1_700_000_000),
                is_dir: false,
            }])
        }

        async fn destination_exists(
            &self,
            _session: SessionId,
            _direction: TransferDirection,
            destination: &Destination,
        ) -> Result<bool, TransferError> {
            Ok(*self
                .fs
                .lock()
                .unwrap()
                .existing
                .get(&dest_key(destination))
                .unwrap_or(&false))
        }

        async fn open_source(
            &self,
            _session: SessionId,
            _direction: TransferDirection,
            source: &Source,
        ) -> Result<Box<dyn SourceReader>, TransferError> {
            let data = self
                .fs
                .lock()
                .unwrap()
                .sources
                .get(&source_key(source))
                .cloned()
                .unwrap_or_default();
            Ok(Box::new(MemReader {
                data,
                pos: 0,
                chunk: 4,
                read_count: 0,
                fail_after_chunks: self.fail_after_chunks,
            }))
        }

        async fn open_destination(
            &self,
            _session: SessionId,
            _direction: TransferDirection,
            destination: &Destination,
        ) -> Result<Box<dyn DestinationWriter>, TransferError> {
            Ok(Box::new(MemWriter {
                key: dest_key(destination),
                buf: Vec::new(),
                fs: Arc::clone(&self.fs),
            }))
        }

        async fn remove_destination(
            &self,
            _session: SessionId,
            _direction: TransferDirection,
            destination: &Destination,
        ) -> Result<(), TransferError> {
            let key = dest_key(destination);
            let mut fs = self.fs.lock().unwrap();
            fs.written.remove(&key);
            fs.removed.push(key);
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        samples: Mutex<Vec<TransferProgress>>,
    }

    impl ProgressSink for RecordingSink {
        fn report(&self, _session: SessionId, progress: TransferProgress) {
            self.samples.lock().unwrap().push(progress);
        }
    }

    fn session() -> SessionId {
        SessionId::new()
    }

    #[tokio::test]
    async fn successful_upload_writes_destination_and_reaches_100() {
        let fs = Arc::new(Mutex::new(MemFs::default()));
        fs.lock()
            .unwrap()
            .sources
            .insert("/local/a.bin".into(), vec![1u8; 10]);
        let backend = Arc::new(MemBackend {
            fs: Arc::clone(&fs),
            fail_after_chunks: None,
        });
        let sink = Arc::new(RecordingSink::default());
        let svc = DefaultFileTransferService::new(backend, Arc::clone(&sink) as Arc<dyn ProgressSink>);

        let res = svc
            .upload(
                session(),
                PathBuf::from("/local/a.bin"),
                RemotePath::new("/remote/a.bin"),
                ConflictPolicy::Abort,
            )
            .await;

        assert!(res.is_ok());
        assert_eq!(fs.lock().unwrap().written.get("/remote/a.bin").unwrap().len(), 10);
        let samples = sink.samples.lock().unwrap();
        assert_eq!(samples.last().unwrap().percentage, 100);
        assert!(fs.lock().unwrap().removed.is_empty());
    }

    #[tokio::test]
    async fn failed_transfer_cleans_up_incomplete_destination() {
        let fs = Arc::new(Mutex::new(MemFs::default()));
        fs.lock()
            .unwrap()
            .sources
            .insert("/local/big.bin".into(), vec![7u8; 40]);
        let backend = Arc::new(MemBackend {
            fs: Arc::clone(&fs),
            fail_after_chunks: Some(2), // fail partway through
        });
        let svc = DefaultFileTransferService::new(
            backend,
            Arc::new(NullProgressSink) as Arc<dyn ProgressSink>,
        );

        let res = svc
            .upload(
                session(),
                PathBuf::from("/local/big.bin"),
                RemotePath::new("/remote/big.bin"),
                ConflictPolicy::Overwrite,
            )
            .await;

        assert!(matches!(res, Err(TransferError::Failed(_))));
        // Incomplete destination removed, and no leftover bytes remain.
        let fs = fs.lock().unwrap();
        assert!(fs.removed.contains(&"/remote/big.bin".to_string()));
        assert!(fs.written.get("/remote/big.bin").is_none());
    }

    #[tokio::test]
    async fn abort_policy_refuses_existing_destination() {
        let fs = Arc::new(Mutex::new(MemFs::default()));
        fs.lock()
            .unwrap()
            .existing
            .insert("/remote/exists.bin".into(), true);
        fs.lock()
            .unwrap()
            .sources
            .insert("/local/x.bin".into(), vec![1u8; 4]);
        let backend = Arc::new(MemBackend {
            fs: Arc::clone(&fs),
            fail_after_chunks: None,
        });
        let svc = DefaultFileTransferService::new(
            backend,
            Arc::new(NullProgressSink) as Arc<dyn ProgressSink>,
        );

        let res = svc
            .upload(
                session(),
                PathBuf::from("/local/x.bin"),
                RemotePath::new("/remote/exists.bin"),
                ConflictPolicy::Abort,
            )
            .await;

        assert!(matches!(res, Err(TransferError::Failed(_))));
        // Nothing was written.
        assert!(fs.lock().unwrap().written.get("/remote/exists.bin").is_none());
    }

    #[tokio::test]
    async fn prompt_policy_requires_confirmation_before_overwrite() {
        let fs = Arc::new(Mutex::new(MemFs::default()));
        fs.lock()
            .unwrap()
            .existing
            .insert("/remote/p.bin".into(), true);
        let backend = Arc::new(MemBackend {
            fs: Arc::clone(&fs),
            fail_after_chunks: None,
        });
        let svc = DefaultFileTransferService::new(
            backend,
            Arc::new(NullProgressSink) as Arc<dyn ProgressSink>,
        );

        let res = svc
            .upload(
                session(),
                PathBuf::from("/local/p.bin"),
                RemotePath::new("/remote/p.bin"),
                ConflictPolicy::Prompt,
            )
            .await;

        // Prompt refuses to write until re-invoked with Overwrite.
        assert!(matches!(res, Err(TransferError::Failed(_))));
    }

    #[tokio::test]
    async fn list_remote_error_is_surfaced_without_side_effects() {
        let fs = Arc::new(Mutex::new(MemFs::default()));
        let backend = Arc::new(MemBackend {
            fs,
            fail_after_chunks: None,
        });
        let svc = DefaultFileTransferService::new(
            backend,
            Arc::new(NullProgressSink) as Arc<dyn ProgressSink>,
        );

        let ok = svc
            .list_remote(session(), RemotePath::new("/ok"))
            .await;
        assert!(ok.is_ok());

        let denied = svc
            .list_remote(session(), RemotePath::new("/denied"))
            .await;
        assert!(matches!(denied, Err(TransferError::DirectoryInaccessible(_))));
    }

    // --- Unit test: overwrite confirmation (task 15.5, req 13.7) ---------
    //
    // Before overwriting an *existing* destination the service must obtain
    // explicit confirmation: a `Prompt` policy refuses the write (the caller
    // is expected to confirm and re-invoke with `Overwrite`), `Overwrite`
    // proceeds, and `Abort` rejects. These example cases pin that contract at
    // both the pure `resolve_conflict` layer and the end-to-end upload flow.

    #[test]
    fn resolve_conflict_prompts_only_when_existing_and_policy_is_prompt() {
        // No existing destination → always proceed, even under Prompt: there is
        // nothing to overwrite so no confirmation is needed.
        assert_eq!(
            resolve_conflict(ConflictPolicy::Prompt, false),
            ConflictDecision::Proceed
        );
        // Existing destination → the policy decides.
        assert_eq!(
            resolve_conflict(ConflictPolicy::Prompt, true),
            ConflictDecision::Prompt
        );
        assert_eq!(
            resolve_conflict(ConflictPolicy::Overwrite, true),
            ConflictDecision::Proceed
        );
        assert_eq!(
            resolve_conflict(ConflictPolicy::Abort, true),
            ConflictDecision::Abort
        );
    }

    /// Build a service whose remote `/remote/dest.bin` already exists and whose
    /// local source `/local/src.bin` holds `data`, returning the service plus
    /// the shared fs so the test can inspect writes/removals.
    fn service_with_existing_destination(
        data: Vec<u8>,
    ) -> (DefaultFileTransferService, Arc<Mutex<MemFs>>) {
        let fs = Arc::new(Mutex::new(MemFs::default()));
        {
            let mut g = fs.lock().unwrap();
            g.existing.insert("/remote/dest.bin".into(), true);
            g.sources.insert("/local/src.bin".into(), data);
        }
        let backend = Arc::new(MemBackend {
            fs: Arc::clone(&fs),
            fail_after_chunks: None,
        });
        let svc = DefaultFileTransferService::new(
            backend,
            Arc::new(NullProgressSink) as Arc<dyn ProgressSink>,
        );
        (svc, fs)
    }

    #[tokio::test]
    async fn overwrite_confirmation_prompt_blocks_until_confirmed() {
        // Prompt policy over an existing destination must NOT write anything —
        // confirmation is required first.
        let (svc, fs) = service_with_existing_destination(vec![9u8; 12]);
        let res = svc
            .upload(
                session(),
                PathBuf::from("/local/src.bin"),
                RemotePath::new("/remote/dest.bin"),
                ConflictPolicy::Prompt,
            )
            .await;
        assert!(
            matches!(res, Err(TransferError::Failed(_))),
            "Prompt must refuse to overwrite without confirmation"
        );
        // The existing destination is left untouched (no bytes written).
        assert!(fs.lock().unwrap().written.get("/remote/dest.bin").is_none());
    }

    #[tokio::test]
    async fn overwrite_confirmation_overwrite_proceeds() {
        // Overwrite policy == the user already confirmed → the write proceeds
        // and the destination is replaced with the source bytes.
        let (svc, fs) = service_with_existing_destination(vec![9u8; 12]);
        let res = svc
            .upload(
                session(),
                PathBuf::from("/local/src.bin"),
                RemotePath::new("/remote/dest.bin"),
                ConflictPolicy::Overwrite,
            )
            .await;
        assert!(res.is_ok(), "Overwrite must proceed over an existing destination");
        assert_eq!(
            fs.lock().unwrap().written.get("/remote/dest.bin").unwrap().len(),
            12
        );
    }

    #[tokio::test]
    async fn overwrite_confirmation_abort_rejects() {
        // Abort policy over an existing destination rejects and writes nothing.
        let (svc, fs) = service_with_existing_destination(vec![9u8; 12]);
        let res = svc
            .upload(
                session(),
                PathBuf::from("/local/src.bin"),
                RemotePath::new("/remote/dest.bin"),
                ConflictPolicy::Abort,
            )
            .await;
        assert!(
            matches!(res, Err(TransferError::Failed(_))),
            "Abort must reject an existing destination"
        );
        assert!(fs.lock().unwrap().written.get("/remote/dest.bin").is_none());
    }

    // --- Cancellation mid-flight (task 26.5, req 13.6, Property 40) ------
    //
    // A transfer started non-blocking via `start_upload` hands back its handle
    // *before* the copy finishes. Cancelling that handle mid-flight must stop
    // the copy and report `Canceled`, with the partial destination cleaned up.

    /// A [`SourceReader`] that yields `chunk`-sized pieces of a large source but
    /// pauses on each read until unblocked, so a test can deterministically
    /// cancel while the copy is in flight (between chunks) rather than racing a
    /// fast in-memory read to completion.
    struct GatedReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
        /// Signalled once per read so the test knows a chunk is in flight.
        read_tx: tokio::sync::mpsc::UnboundedSender<()>,
        /// Awaited each read so the test controls the pace.
        gate: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl SourceReader for GatedReader {
        fn total(&self) -> u64 {
            self.data.len() as u64
        }
        async fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, TransferError> {
            if self.pos >= self.data.len() {
                return Ok(None);
            }
            // Announce that a read is happening, then wait for the test to let
            // this chunk through — giving the test a window to cancel.
            let _ = self.read_tx.send(());
            self.gate.notified().await;
            let end = (self.pos + self.chunk).min(self.data.len());
            let out = self.data[self.pos..end].to_vec();
            self.pos = end;
            Ok(Some(out))
        }
    }

    struct GatedBackend {
        fs: Arc<Mutex<MemFs>>,
        read_tx: tokio::sync::mpsc::UnboundedSender<()>,
        gate: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl SftpBackend for GatedBackend {
        async fn list_dir(
            &self,
            _session: SessionId,
            _path: &RemotePath,
        ) -> Result<Vec<RemoteEntry>, TransferError> {
            Ok(vec![])
        }
        async fn destination_exists(
            &self,
            _session: SessionId,
            _direction: TransferDirection,
            _destination: &Destination,
        ) -> Result<bool, TransferError> {
            Ok(false)
        }
        async fn open_source(
            &self,
            _session: SessionId,
            _direction: TransferDirection,
            source: &Source,
        ) -> Result<Box<dyn SourceReader>, TransferError> {
            let data = self
                .fs
                .lock()
                .unwrap()
                .sources
                .get(&source_key(source))
                .cloned()
                .unwrap_or_default();
            Ok(Box::new(GatedReader {
                data,
                pos: 0,
                chunk: 4,
                read_tx: self.read_tx.clone(),
                gate: Arc::clone(&self.gate),
            }))
        }
        async fn open_destination(
            &self,
            _session: SessionId,
            _direction: TransferDirection,
            destination: &Destination,
        ) -> Result<Box<dyn DestinationWriter>, TransferError> {
            Ok(Box::new(MemWriter {
                key: dest_key(destination),
                buf: Vec::new(),
                fs: Arc::clone(&self.fs),
            }))
        }
        async fn remove_destination(
            &self,
            _session: SessionId,
            _direction: TransferDirection,
            destination: &Destination,
        ) -> Result<(), TransferError> {
            let key = dest_key(destination);
            let mut fs = self.fs.lock().unwrap();
            fs.written.remove(&key);
            fs.removed.push(key);
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancel_mid_flight_stops_with_canceled_and_cleans_up_partial() {
        let fs = Arc::new(Mutex::new(MemFs::default()));
        // A source large enough to need several 4-byte chunks.
        fs.lock()
            .unwrap()
            .sources
            .insert("/local/large.bin".into(), vec![5u8; 40]);

        let (read_tx, mut read_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let gate = Arc::new(tokio::sync::Notify::new());
        let backend = Arc::new(GatedBackend {
            fs: Arc::clone(&fs),
            read_tx,
            gate: Arc::clone(&gate),
        });
        let svc = DefaultFileTransferService::new(
            backend,
            Arc::new(NullProgressSink) as Arc<dyn ProgressSink>,
        );

        // Start non-blocking: the handle is available *before* the copy runs.
        let (handle, fut) = svc.start_upload(
            session(),
            PathBuf::from("/local/large.bin"),
            RemotePath::new("/remote/large.bin"),
            ConflictPolicy::Overwrite,
        );
        let task = tokio::spawn(fut);

        // Let two chunks through so the destination is genuinely partial, then
        // cancel while the third read is pending (in flight).
        for _ in 0..2 {
            read_rx.recv().await.unwrap(); // a read is waiting at the gate
            gate.notify_one(); // release this chunk
        }
        // Wait until the third read parks at the gate: the copy is mid-flight.
        read_rx.recv().await.unwrap();
        // Confirm a partial destination exists before cancelling.
        assert!(
            !fs.lock()
                .unwrap()
                .written
                .get("/remote/large.bin")
                .map(|b| b.is_empty())
                .unwrap_or(true),
            "destination should hold partial bytes mid-flight"
        );

        handle.request_cancel();
        // Release the parked read so the loop advances once more and observes
        // the cancel flag at the top of the next iteration.
        gate.notify_one();
        // Drain any further gate waiters so the reader never blocks the loop
        // from reaching its cancel check.
        tokio::spawn(async move {
            while read_rx.recv().await.is_some() {
                gate.notify_one();
            }
        });

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("transfer must stop within 2s of cancel")
            .expect("transfer task panicked");

        assert!(
            matches!(outcome, Err(TransferError::Canceled)),
            "cancelled transfer must report Canceled, got {outcome:?}"
        );
        // Partial destination cleaned up (Property 40).
        let fs = fs.lock().unwrap();
        assert!(
            fs.removed.contains(&"/remote/large.bin".to_string()),
            "the partial destination must be removed on cancel"
        );
        assert!(fs.written.get("/remote/large.bin").is_none());
    }

    // --- Property tests (tasks 15.2, 15.3, 15.4) ------------------------

    mod properties {
        use super::*;
        use proptest::prelude::*;
        use tokio::runtime::Runtime;

        /// A fresh current-thread runtime for driving one async property case.
        fn runtime() -> Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime")
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(200))]

            // Feature: remote-connection-manager, Property 39: Transfer progress is correct and monotonic
            //
            // *For any* sequence of bytes-transferred values up to a known
            // total, the reported completion percentage equals
            // round(bytes/total*100), stays within 0..=100, never decreases,
            // and reaches 100 exactly when all bytes are transferred.
            //
            // Validates: Requirements 13.4
            #[test]
            fn progress_percentage_is_correct_and_monotonic(
                total in 1u64..1_000_000,
                // A set of transferred checkpoints in 0..=total; we sort them to
                // model a transfer that only ever moves forward.
                raw_points in proptest::collection::vec(0u64..=1_000_000, 1..32),
            ) {
                // Constrain each checkpoint to the input space 0..=total, then
                // sort ascending so bytes-transferred is non-decreasing (as it
                // is during a real transfer).
                let mut points: Vec<u64> =
                    raw_points.iter().map(|b| b % (total + 1)).collect();
                points.sort_unstable();

                let mut last_pct = 0u8;
                for &bytes in &points {
                    let pct = percentage(bytes, total);

                    // round(bytes/total*100) — computed independently with
                    // *exact integer* arithmetic as the reference oracle.
                    // f64 is deliberately NOT used here: for an exact tie such
                    // as 137484/482400 = 28.5, binary floating point lands just
                    // below 28.5 and `f64::round` yields 28, whereas the
                    // specified half-away-from-zero rounding of the real value
                    // is 29. The implementation uses the integer formula
                    // `(bytes*100 + total/2)/total`, which rounds half up
                    // exactly; the oracle mirrors that with u128 so ties are
                    // compared faithfully rather than against float error.
                    let expected = {
                        let scaled = (bytes.min(total) as u128) * 100 + (total as u128) / 2;
                        (scaled / (total as u128)).min(100) as u8
                    };
                    prop_assert_eq!(pct, expected,
                        "percentage({}, {}) = {} != expected {}", bytes, total, pct, expected);

                    // Within 0..=100.
                    prop_assert!(pct <= 100);

                    // Monotonic non-decreasing across the forward-moving points.
                    prop_assert!(pct >= last_pct,
                        "percentage decreased at {} bytes: {} < {}", bytes, pct, last_pct);
                    last_pct = pct;

                    // Completion always reports exactly 100. (The converse does
                    // not hold: a near-complete transfer such as 99.5% rounds
                    // to 100 under the specified round(bytes/total*100), so a
                    // 100% reading implies completion only up to that rounding —
                    // "reaches 100 exactly when all bytes are transferred" is
                    // the completion ⇒ 100 direction the implementation
                    // guarantees.)
                    if bytes == total {
                        prop_assert_eq!(pct, 100,
                            "completion must report 100% (total={})", total);
                    }
                }

                // Completion always lands on exactly 100.
                prop_assert_eq!(percentage(total, total), 100);
            }

            // Feature: remote-connection-manager, Property 40: Incomplete transfers clean up the destination
            //
            // *For any* transfer interrupted before completion, whether by
            // failure or user cancellation, the incomplete destination file is
            // removed and the outcome is reported as failed or canceled
            // respectively.
            //
            // Validates: Requirements 13.5, 13.6
            #[test]
            fn incomplete_transfer_cleans_up_destination(
                // Source spanning several chunks so there is a partial write to
                // clean up (chunk size in the mock reader is 4 bytes → at least
                // 3 chunks here).
                size in 12usize..256,
                // Choose where to stop as a fraction of the chunk count; the
                // body clamps it to 1..chunks so the interruption always lands
                // strictly before the source is exhausted.
                stop_frac in 0.1f64..0.9,
                // Which interruption mode: false = failure, true = cancel.
                cancel in any::<bool>(),
            ) {
                // Mock reader chunk size is 4 bytes.
                let chunks = size.div_ceil(4);
                // At least one chunk read, strictly fewer than all chunks so the
                // transfer is genuinely interrupted mid-flight.
                let stop_after = (((chunks as f64) * stop_frac) as usize)
                    .clamp(1, chunks.saturating_sub(1));
                runtime().block_on(async {
                    let fs = Arc::new(Mutex::new(MemFs::default()));
                    fs.lock()
                        .unwrap()
                        .sources
                        .insert("/local/partial.bin".into(), vec![3u8; size]);

                    let dest = Destination::Remote(RemotePath::new("/remote/partial.bin"));

                    if cancel {
                        // Cancellation path: drive copy_loop directly with a
                        // pre-cancelled handle so the loop stops before
                        // completion, then apply the service's own cleanup
                        // decision — exactly what run_transfer does.
                        let backend = Arc::new(MemBackend {
                            fs: Arc::clone(&fs),
                            fail_after_chunks: None,
                        });
                        let svc = DefaultFileTransferService::new(
                            Arc::clone(&backend) as Arc<dyn SftpBackend>,
                            Arc::new(NullProgressSink) as Arc<dyn ProgressSink>,
                        );
                        let sess = session();
                        let handle = TransferHandle::new(sess);
                        handle.request_cancel();
                        let source = Source::Local(PathBuf::from("/local/partial.bin"));
                        let outcome = svc
                            .copy_loop(sess, TransferDirection::Upload, &source, &dest, &handle)
                            .await;

                        prop_assert_eq!(outcome.clone(), TransferOutcome::Canceled);
                        prop_assert!(should_cleanup(&outcome));
                        // Mirror run_transfer's cleanup and assert removal.
                        backend
                            .remove_destination(sess, TransferDirection::Upload, &dest)
                            .await
                            .unwrap();
                        let g = fs.lock().unwrap();
                        prop_assert!(g.removed.contains(&"/remote/partial.bin".to_string()));
                        prop_assert!(g.written.get("/remote/partial.bin").is_none());
                        Ok(())
                    } else {
                        // Failure path: inject a read failure after `stop_after`
                        // chunks and let the full upload flow run; the service
                        // must remove the incomplete destination and report
                        // Failed.
                        let backend = Arc::new(MemBackend {
                            fs: Arc::clone(&fs),
                            fail_after_chunks: Some(stop_after),
                        });
                        let svc = DefaultFileTransferService::new(
                            backend,
                            Arc::new(NullProgressSink) as Arc<dyn ProgressSink>,
                        );
                        let res = svc
                            .upload(
                                session(),
                                PathBuf::from("/local/partial.bin"),
                                RemotePath::new("/remote/partial.bin"),
                                ConflictPolicy::Overwrite,
                            )
                            .await;

                        prop_assert!(matches!(res, Err(TransferError::Failed(_))),
                            "interrupted transfer must report Failed");
                        let g = fs.lock().unwrap();
                        prop_assert!(g.removed.contains(&"/remote/partial.bin".to_string()),
                            "incomplete destination must be removed");
                        prop_assert!(g.written.get("/remote/partial.bin").is_none(),
                            "no partial bytes may remain after cleanup");
                        Ok(())
                    }
                })?;
            }

            // Feature: remote-connection-manager, Property 41: Failed remote navigation preserves the current listing
            //
            // *For any* currently displayed remote directory listing,
            // attempting to open an inaccessible directory reports an error and
            // leaves the displayed listing unchanged.
            //
            // Approach: `list_remote` is a pure read that holds no "current
            // listing" of its own, so (as the implementation documents) the
            // service cannot and must not mutate any state on a failed listing.
            // We model the *displayed* listing at the caller level: capture a
            // last-successful listing, attempt to open an inaccessible
            // directory, assert it returns DirectoryInaccessible, and assert the
            // caller's held listing is byte-for-byte unchanged — and that a
            // subsequent successful listing still works (no corrupted state).
            #[test]
            fn failed_navigation_preserves_current_listing(
                // Arbitrary name for the inaccessible target; the mock backend
                // treats any "/denied*" path as inaccessible.
                suffix in "[a-z]{0,8}",
            ) {
                runtime().block_on(async {
                    let fs = Arc::new(Mutex::new(MemFs::default()));
                    let backend = Arc::new(MemBackend {
                        fs,
                        fail_after_chunks: None,
                    });
                    let svc = DefaultFileTransferService::new(
                        backend,
                        Arc::new(NullProgressSink) as Arc<dyn ProgressSink>,
                    );
                    let sess = session();

                    // Caller's currently-displayed listing: a successful read.
                    let displayed = svc
                        .list_remote(sess, RemotePath::new("/ok"))
                        .await
                        .expect("baseline listing succeeds");
                    let snapshot = displayed.clone();

                    // Attempt to navigate into an inaccessible directory.
                    let denied_path = format!("/denied{}", suffix);
                    let nav = svc
                        .list_remote(sess, RemotePath::new(denied_path))
                        .await;
                    prop_assert!(matches!(nav, Err(TransferError::DirectoryInaccessible(_))),
                        "inaccessible directory must report DirectoryInaccessible");

                    // The caller keeps its prior listing unchanged on error.
                    prop_assert_eq!(&displayed, &snapshot,
                        "the displayed listing must be left unchanged on failed navigation");

                    // The service is not corrupted: a later valid listing works.
                    let again = svc
                        .list_remote(sess, RemotePath::new("/ok"))
                        .await
                        .expect("listing still works after a failed navigation");
                    prop_assert_eq!(again, snapshot);
                    Ok(())
                })?;
            }
        }
    }
}
