/**
 * FileTransferPanel — SFTP/SCP file transfer UI for one SSH session (task 21.3).
 *
 * Covers the File_Transfer_Service surface for the active session:
 *
 * - **Remote listing** (`list_remote`, Requirement 13.1): shows each entry's
 *   name, size, and modification time; directories can be drilled into. On a
 *   failed navigation the previously shown listing is **preserved** on screen
 *   and an error is surfaced (Requirement 13.8).
 * - **Upload / download** (`upload_file` / `download_file`, Requirements 13.2,
 *   13.3): the user supplies the local and remote paths.
 * - **Progress** (Requirement 13.4): a progress bar driven by the
 *   `transfer://{id}/progress` event via {@link listenTransferProgress}.
 * - **Cancel** (`cancel_transfer`, Requirement 13.6): a button that requests
 *   cooperative cancellation of the in-flight transfer.
 * - **Overwrite confirmation** (Requirement 13.7): the first attempt is made
 *   with `overwrite = false`; if the backend reports the destination already
 *   exists, a ConfirmDialog asks the user, and only on confirm is the transfer
 *   retried with `overwrite = true`.
 *
 * ## Wiring status
 *
 * The live SFTP/SCP backend binds to a session's russh channel, which is part
 * of the still-unfinished transport wiring, so every transfer command rejects
 * today with {@link NOT_WIRED_CODE}. This panel detects that code and shows a
 * clear "transfer in integration" notice without breaking — and is structured
 * so that, once the backend is wired, the exact same calls stream listings and
 * progress with no component changes.
 *
 * ## Path entry (no native dialog)
 *
 * The app does not bundle the Tauri dialog plugin, so local/remote paths are
 * entered as text. When a native file picker is later added, swap the text
 * inputs for it; the transfer calls stay the same.
 *
 * Branding is task 22; neutral token-driven styling only.
 */

import { useEffect, useRef, useState } from "react";
import ConfirmDialog from "./ConfirmDialog";
import {
  asDomainError,
  cancelTransfer,
  downloadFile,
  listenTransferDone,
  listRemote,
  listenTransferProgress,
  NOT_WIRED_CODE,
  uploadFile,
  type RemoteEntry,
  type SessionId,
  type TransferProgress,
} from "../ipc";

interface FileTransferPanelProps {
  sessionId: SessionId;
  onClose: () => void;
}

/** A transfer the user asked for, pending an overwrite confirmation. */
type PendingOverwrite =
  | { kind: "upload"; local: string; remote: string }
  | { kind: "download"; remote: string; local: string };

/** Normalise a remote directory path join (keeps a single leading slash). */
function joinRemote(base: string, name: string): string {
  if (base.endsWith("/")) return `${base}${name}`;
  return `${base}/${name}`;
}

/** The parent of a remote path, or "/" at the root. */
function parentRemote(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const idx = trimmed.lastIndexOf("/");
  if (idx <= 0) return "/";
  return trimmed.slice(0, idx);
}

/** Human-readable byte size. */
function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let i = 0;
  while (value >= 1024 && i < units.length - 1) {
    value /= 1024;
    i += 1;
  }
  return `${value.toFixed(1)} ${units[i]}`;
}

/** Unix-seconds timestamp → locale date-time, or a dash when absent. */
function formatModified(modified: number | null): string {
  if (modified == null) return "—";
  return new Date(modified * 1000).toLocaleString();
}

/** A code the backend uses for an existing-destination conflict (Req 13.7). */
function isOverwriteConflict(code: string): boolean {
  // The transfer layer reports an existing destination that needs confirmation
  // as a transfer failure; match the documented conflict codes defensively.
  return (
    code === "transfer.destinationExists" ||
    code === "transfer.confirmationRequired"
  );
}

function FileTransferPanel({ sessionId, onClose }: FileTransferPanelProps) {
  const [path, setPath] = useState("/");
  const [entries, setEntries] = useState<RemoteEntry[]>([]);
  const [listing, setListing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // Set when the backend reports the transport is not yet wired, so the panel
  // explains itself rather than looking broken.
  const [notWired, setNotWired] = useState(false);

  // Upload/download inputs.
  const [localPath, setLocalPath] = useState("");
  const [remoteName, setRemoteName] = useState("");

  // In-flight transfer progress + overwrite confirmation.
  const [progress, setProgress] = useState<TransferProgress | null>(null);
  const [transferring, setTransferring] = useState(false);
  const [pendingOverwrite, setPendingOverwrite] =
    useState<PendingOverwrite | null>(null);

  // Keep the latest listed path to preserve the on-screen listing on a failed
  // navigation (Requirement 13.8).
  const lastGoodPath = useRef("/");

  // The in-flight transfer's label, set when a transfer starts and consumed by
  // the done-event handler to report the final outcome. Because the transfer
  // is non-blocking, this bridges "started" (the command resolved) to
  // "finished" (the done event).
  const pendingTransfer = useRef<{
    verb: string;
    target: string;
    refresh: boolean;
  } | null>(null);

  // Subscribe to the session's transfer-progress stream (Requirement 13.4).
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;
    listenTransferProgress(sessionId, (sample) => {
      setProgress(sample);
    })
      .then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch(() => {
        // A failed subscription must not break the panel; progress just won't
        // animate until the backend path is live.
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [sessionId]);

  // Subscribe to the session's transfer-done stream (Requirement 13.5/13.6):
  // the single terminal outcome of a non-blocking transfer. It settles the
  // panel — clears the in-flight flag, and reports success or the mapped error
  // (including `transfer.canceled`).
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;
    listenTransferDone(sessionId, (done) => {
      const pending = pendingTransfer.current;
      pendingTransfer.current = null;
      setTransferring(false);
      setProgress(null);
      if (done.ok) {
        setError(null);
        setNotice(
          pending ? `${pending.verb} to ${pending.target}.` : "Transfer complete.",
        );
        // A completed upload may have added a remote entry: refresh the listing.
        if (pending?.refresh) void navigate(lastGoodPath.current);
      } else if (done.code === "transfer.canceled") {
        setNotice("Transfer cancelled.");
      } else {
        setError(done.message ?? "Transfer failed.");
      }
    })
      .then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch(() => {
        // A failed subscription must not break the panel.
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  /** List `target`, preserving the current listing on failure (Req 13.8). */
  async function navigate(target: string) {
    setListing(true);
    setError(null);
    setNotice(null);
    try {
      const result = await listRemote(sessionId, target);
      setEntries(result);
      setPath(target);
      lastGoodPath.current = target;
      setNotWired(false);
    } catch (err) {
      const domainErr = asDomainError(err);
      if (domainErr.code === NOT_WIRED_CODE) {
        setNotWired(true);
        setNotice(
          "File transfer is still being integrated with the connection " +
            "transport. The panel is ready and will list files once that lands.",
        );
      } else {
        // Preserve the prior listing (Requirement 13.8): do not clear entries
        // or move `path` away from the last good location.
        setError(domainErr.message);
      }
    } finally {
      setListing(false);
    }
  }

  async function runUpload(local: string, remote: string, overwrite: boolean) {
    setTransferring(true);
    setProgress(null);
    setError(null);
    setNotice(null);
    // The transfer is non-blocking: `uploadFile` resolves once it has *started*
    // (so Cancel can reach it); the terminal result arrives on the done event,
    // which flips `transferring` off and reports success/failure. Remember what
    // we are transferring so the done handler can label the outcome.
    pendingTransfer.current = {
      verb: "Uploaded",
      target: remote,
      refresh: true,
    };
    try {
      await uploadFile(sessionId, local, remote, overwrite);
      setNotice(`Uploading to ${remote}…`);
    } catch (err) {
      // Failed to even start (e.g. an overwrite conflict): no done event will
      // fire, so settle the UI here.
      pendingTransfer.current = null;
      setTransferring(false);
      handleTransferError(err, { kind: "upload", local, remote }, overwrite);
    }
  }

  async function runDownload(
    remote: string,
    local: string,
    overwrite: boolean,
  ) {
    setTransferring(true);
    setProgress(null);
    setError(null);
    setNotice(null);
    pendingTransfer.current = {
      verb: "Downloaded",
      target: local,
      refresh: false,
    };
    try {
      await downloadFile(sessionId, remote, local, overwrite);
      setNotice(`Downloading to ${local}…`);
    } catch (err) {
      pendingTransfer.current = null;
      setTransferring(false);
      handleTransferError(err, { kind: "download", remote, local }, overwrite);
    }
  }

  /** Route a transfer error: wiring, overwrite-conflict, or a plain failure. */
  function handleTransferError(
    err: unknown,
    attempt: PendingOverwrite,
    wasOverwrite: boolean,
  ) {
    const domainErr = asDomainError(err);
    if (domainErr.code === NOT_WIRED_CODE) {
      setNotWired(true);
      setNotice(
        "File transfer is still being integrated with the connection " +
          "transport. Your request is valid and will run once that lands.",
      );
      return;
    }
    // Only prompt to overwrite if we have not already retried with overwrite.
    if (!wasOverwrite && isOverwriteConflict(domainErr.code)) {
      setPendingOverwrite(attempt);
      return;
    }
    setError(domainErr.message);
  }

  function handleUploadClick() {
    const local = localPath.trim();
    const name = remoteName.trim();
    if (!local || !name) {
      setError("Enter both a local file path and a remote file name.");
      return;
    }
    const remote = name.startsWith("/") ? name : joinRemote(path, name);
    void runUpload(local, remote, false);
  }

  function handleDownloadClick(entry: RemoteEntry) {
    const local = localPath.trim();
    if (!local) {
      setError("Enter a local destination path first.");
      return;
    }
    const remote = joinRemote(path, entry.name);
    void runDownload(remote, local, false);
  }

  async function handleCancel() {
    try {
      await cancelTransfer(sessionId);
      setNotice("Cancellation requested.");
    } catch (err) {
      const domainErr = asDomainError(err);
      if (domainErr.code !== NOT_WIRED_CODE) setError(domainErr.message);
    }
  }

  /** Confirm an overwrite and retry with overwrite = true (Req 13.7). */
  function confirmOverwrite() {
    if (!pendingOverwrite) return;
    const attempt = pendingOverwrite;
    setPendingOverwrite(null);
    if (attempt.kind === "upload") {
      void runUpload(attempt.local, attempt.remote, true);
    } else {
      void runDownload(attempt.remote, attempt.local, true);
    }
  }

  const atRoot = path === "/" || path === "";

  return (
    <div className="session-panel" aria-label="File transfer">
      <div className="session-panel-header">
        <h3 className="session-panel-title">File transfer</h3>
        <button
          type="button"
          className="icon-btn"
          aria-label="Close file transfer"
          onClick={onClose}
        >
          ✕
        </button>
      </div>

      {notWired ? (
        <p className="form-notice" role="status">
          Transfer in integration — the UI is wired; the SFTP/SCP transport
          lands with the connection seam.
        </p>
      ) : null}
      {error ? (
        <p className="form-error" role="alert">
          {error}
        </p>
      ) : null}
      {notice && !notWired ? (
        <p className="form-notice" role="status">
          {notice}
        </p>
      ) : null}

      {/* Remote path navigation */}
      <div className="transfer-nav">
        <button
          type="button"
          className="btn btn-small"
          disabled={atRoot || listing}
          onClick={() => void navigate(parentRemote(path))}
        >
          ↑ Up
        </button>
        <input
          type="text"
          aria-label="Remote path"
          value={path}
          onChange={(e) => setPath(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") void navigate(path);
          }}
        />
        <button
          type="button"
          className="btn btn-small"
          disabled={listing}
          onClick={() => void navigate(path)}
        >
          {listing ? "Listing…" : "List"}
        </button>
      </div>

      {/* Remote listing (Requirement 13.1) */}
      <div className="transfer-listing" role="table" aria-label="Remote entries">
        {entries.length === 0 ? (
          <p className="field-empty">
            {listing ? "Listing…" : "No entries to show. List a directory."}
          </p>
        ) : (
          <ul className="transfer-rows">
            {entries.map((entry) => (
              <li className="transfer-row" key={entry.name} role="row">
                <button
                  type="button"
                  className="transfer-name"
                  disabled={!entry.isDir || listing}
                  onClick={() =>
                    entry.isDir
                      ? void navigate(joinRemote(path, entry.name))
                      : undefined
                  }
                  title={entry.isDir ? "Open directory" : entry.name}
                >
                  <span className="transfer-icon">
                    {entry.isDir ? "📁" : "📄"}
                  </span>
                  <span className="transfer-filename">{entry.name}</span>
                </button>
                <span className="transfer-size">
                  {entry.isDir ? "" : formatSize(entry.size)}
                </span>
                <span className="transfer-modified">
                  {formatModified(entry.modified)}
                </span>
                <span className="transfer-actions">
                  {!entry.isDir ? (
                    <button
                      type="button"
                      className="btn btn-small"
                      disabled={transferring}
                      onClick={() => handleDownloadClick(entry)}
                    >
                      Download
                    </button>
                  ) : null}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>

      {/* Upload + download target paths */}
      <div className="transfer-io">
        <div className="field">
          <label htmlFor="transfer-local">Local file path</label>
          <input
            id="transfer-local"
            type="text"
            value={localPath}
            placeholder="/Users/you/file.txt"
            onChange={(e) => setLocalPath(e.target.value)}
          />
          <p className="field-hint">
            Used as the upload source and the download destination.
          </p>
        </div>
        <div className="field">
          <label htmlFor="transfer-remote-name">Upload as (remote name)</label>
          <div className="field-inline">
            <input
              id="transfer-remote-name"
              type="text"
              value={remoteName}
              placeholder="file.txt"
              onChange={(e) => setRemoteName(e.target.value)}
            />
            <button
              type="button"
              className="btn btn-primary btn-small"
              disabled={transferring}
              onClick={handleUploadClick}
            >
              Upload
            </button>
          </div>
        </div>
      </div>

      {/* Progress bar (Requirement 13.4) + cancel (Requirement 13.6) */}
      {transferring || progress ? (
        <div className="transfer-progress" aria-label="Transfer progress">
          <div
            className="progress-track"
            role="progressbar"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={progress?.percentage ?? 0}
          >
            <div
              className="progress-fill"
              style={{ width: `${progress?.percentage ?? 0}%` }}
            />
          </div>
          <div className="transfer-progress-row">
            <span className="transfer-progress-label">
              {progress
                ? `${progress.percentage}% (${formatSize(
                    progress.bytesTransferred,
                  )} / ${formatSize(progress.total)})`
                : "Starting…"}
            </span>
            {transferring ? (
              <button
                type="button"
                className="btn btn-small btn-danger"
                onClick={() => void handleCancel()}
              >
                Cancel
              </button>
            ) : null}
          </div>
        </div>
      ) : null}

      {pendingOverwrite ? (
        <ConfirmDialog
          title="Overwrite destination?"
          message={
            pendingOverwrite.kind === "upload"
              ? `“${pendingOverwrite.remote}” already exists. Overwrite it?`
              : `“${pendingOverwrite.local}” already exists. Overwrite it?`
          }
          confirmLabel="Overwrite"
          onConfirm={confirmOverwrite}
          onCancel={() => setPendingOverwrite(null)}
        />
      ) : null}
    </div>
  );
}

export default FileTransferPanel;
