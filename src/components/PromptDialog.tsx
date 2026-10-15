/**
 * A small modal text-prompt dialog, used for folder creation (name entry).
 *
 * Folder name validation (empty / >255 chars / duplicate within parent) is
 * enforced by the backend `create_folder` command (Requirement 3.2); when it
 * rejects with a `validation.name` DomainError the caller passes the message in
 * via `error`, shown beneath the input.
 *
 * Branding/theme is task 22; neutral CSS-variable-driven styling only here.
 */

import { useState } from "react";

interface PromptDialogProps {
  title: string;
  label: string;
  initialValue?: string;
  confirmLabel?: string;
  placeholder?: string;
  busy?: boolean;
  /** A validation/error message to show under the input (e.g. from the backend). */
  error?: string | null;
  onConfirm: (value: string) => void;
  onCancel: () => void;
}

function PromptDialog({
  title,
  label,
  initialValue = "",
  confirmLabel = "Create",
  placeholder,
  busy = false,
  error,
  onConfirm,
  onCancel,
}: PromptDialogProps) {
  const [value, setValue] = useState(initialValue);

  return (
    <div
      className="modal-overlay"
      role="presentation"
      onClick={() => {
        if (!busy) onCancel();
      }}
    >
      <form
        className="modal"
        onClick={(e) => e.stopPropagation()}
        onSubmit={(e) => {
          e.preventDefault();
          onConfirm(value);
        }}
      >
        <h2 className="modal-title">{title}</h2>
        <div className="field">
          <label htmlFor="prompt-input">{label}</label>
          <input
            id="prompt-input"
            type="text"
            value={value}
            placeholder={placeholder}
            autoFocus
            onChange={(e) => setValue(e.target.value)}
            aria-invalid={Boolean(error)}
            aria-describedby={error ? "prompt-error" : undefined}
          />
          {error ? (
            <p id="prompt-error" className="field-error" role="alert">
              {error}
            </p>
          ) : null}
        </div>
        <div className="modal-actions">
          <button
            type="button"
            className="btn"
            onClick={onCancel}
            disabled={busy}
          >
            Cancel
          </button>
          <button type="submit" className="btn btn-primary" disabled={busy}>
            {busy ? "Working…" : confirmLabel}
          </button>
        </div>
      </form>
    </div>
  );
}

export default PromptDialog;
