/**
 * The host-key / certificate trust prompt dialog (task 26.3).
 *
 * When a Session connects to a host whose SSH host key (or RDP server
 * certificate) is not yet known, the backend emits a `hostkey://prompt` event
 * carrying the presented fingerprint and suspends the connect awaiting the
 * user's decision (Requirement 4.7, 4.8). This dialog shows the fingerprint and
 * routes the Accept/Reject choice back through `respondHostKeyPrompt`.
 *
 * It is deliberately minimal but **real**: the decision is the user's, never
 * faked. Accepting trusts and persists the fingerprint (so later connects are
 * silent and a *changed* key is caught); rejecting aborts the connect. A
 * changed key never reaches this dialog — the backend rejects it outright — so
 * this prompt only ever confirms a genuinely *unknown* host.
 *
 * Styling reuses the shared `.modal` / `.btn` classes (see `ConfirmDialog`), so
 * it matches the app's theme without bespoke CSS.
 */

import type { HostKeyPromptEvent } from "../ipc";

interface HostKeyPromptDialogProps {
  prompt: HostKeyPromptEvent;
  busy?: boolean;
  onAccept: () => void;
  onReject: () => void;
}

/** Human-readable label for the kind of credential being confirmed. */
function kindLabel(kind: HostKeyPromptEvent["kind"]): string {
  return kind === "rdp-certificate"
    ? "server certificate"
    : "host key";
}

function HostKeyPromptDialog({
  prompt,
  busy = false,
  onAccept,
  onReject,
}: HostKeyPromptDialogProps) {
  const label = kindLabel(prompt.kind);
  return (
    <div
      className="modal-overlay"
      role="presentation"
      onClick={() => {
        // Dismissing the overlay is a reject: an unconfirmed key must not be
        // trusted by default.
        if (!busy) onReject();
      }}
    >
      <div
        className="modal"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="hostkey-title"
        aria-describedby="hostkey-message"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 id="hostkey-title" className="modal-title">
          Unknown {label}
        </h2>
        <p id="hostkey-message" className="modal-message">
          The {label} for{" "}
          <strong>
            {prompt.host}:{prompt.port}
          </strong>{" "}
          is not recognized. Verify the fingerprint below out of band before
          trusting it. Accepting remembers this {label}; a later change will be
          rejected automatically.
        </p>
        <pre className="hostkey-fingerprint" aria-label="fingerprint">
          {prompt.fingerprint}
        </pre>
        <div className="modal-actions">
          <button
            type="button"
            className="btn"
            onClick={onReject}
            disabled={busy}
          >
            Reject
          </button>
          <button
            type="button"
            className="btn btn-primary"
            onClick={onAccept}
            disabled={busy}
          >
            {busy ? "Working…" : "Accept & Trust"}
          </button>
        </div>
      </div>
    </div>
  );
}

export default HostKeyPromptDialog;
