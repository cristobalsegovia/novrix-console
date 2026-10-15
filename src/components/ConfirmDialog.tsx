/**
 * A small modal confirmation dialog.
 *
 * Used for the destructive-action confirmations required by the spec:
 * deleting a profile (Requirement 2.7) and deleting a non-empty folder
 * (Requirement 3.6). The backend rejects a delete called with
 * `confirmed = false`; the correct flow is UI-confirm first, then invoke with
 * `confirmed = true`. This component only gathers the user's decision.
 *
 * Branding/theme (full Novrix palette, Inter font, WCAG contrast) is applied in
 * task 22; styling here is neutral and driven by CSS variables so task 22 can
 * reskin without touching this component.
 */

interface ConfirmDialogProps {
  title: string;
  message: string;
  confirmLabel?: string;
  cancelLabel?: string;
  /** When true the confirm button uses the destructive styling. */
  destructive?: boolean;
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

function ConfirmDialog({
  title,
  message,
  confirmLabel = "Delete",
  cancelLabel = "Cancel",
  destructive = true,
  busy = false,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  return (
    <div
      className="modal-overlay"
      role="presentation"
      onClick={() => {
        if (!busy) onCancel();
      }}
    >
      <div
        className="modal"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="confirm-title"
        aria-describedby="confirm-message"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 id="confirm-title" className="modal-title">
          {title}
        </h2>
        <p id="confirm-message" className="modal-message">
          {message}
        </p>
        <div className="modal-actions">
          <button
            type="button"
            className="btn"
            onClick={onCancel}
            disabled={busy}
          >
            {cancelLabel}
          </button>
          <button
            type="button"
            className={destructive ? "btn btn-danger" : "btn btn-primary"}
            onClick={onConfirm}
            disabled={busy}
          >
            {busy ? "Working…" : confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}

export default ConfirmDialog;
