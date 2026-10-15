/**
 * LoggingToggle — per-session logging control (task 21.3).
 *
 * A single control in the session action bar that enables/disables logging for
 * the active session:
 *
 * - **Enable** (`start_logging`, Requirement 14.2): prompts for a destination
 *   **file path** (the backend's `start_logging` takes a file path and writes
 *   timestamped I/O there) and starts recording. An invalid/unwritable path is
 *   reported by the backend as a `LogError` and does not enable logging
 *   (Requirement 14.6) — the error is shown in the prompt.
 * - **Disable** (`stop_logging`, Requirement 14.3): stops recording; the
 *   backend closes the file retaining what was written.
 *
 * The enabled/disabled state is frontend-owned here (the backend exposes no
 * "is logging?" query); it reflects what this control last did. If a profile
 * has logging enabled on connect, that is driven by the backend at connect time
 * and is independent of this manual toggle.
 *
 * Branding is task 22; neutral token-driven styling only.
 */

import { useState } from "react";
import PromptDialog from "./PromptDialog";
import {
  asDomainError,
  startLogging,
  stopLogging,
  type SessionId,
} from "../ipc";

interface LoggingToggleProps {
  sessionId: SessionId;
  onError?: (message: string) => void;
}

function LoggingToggle({ sessionId, onError }: LoggingToggleProps) {
  const [enabled, setEnabled] = useState(false);
  const [busy, setBusy] = useState(false);
  const [prompting, setPrompting] = useState(false);
  const [promptError, setPromptError] = useState<string | null>(null);

  async function enable(path: string) {
    const trimmed = path.trim();
    if (!trimmed) {
      setPromptError("Enter a log file path.");
      return;
    }
    setBusy(true);
    setPromptError(null);
    try {
      await startLogging(sessionId, trimmed);
      setEnabled(true);
      setPrompting(false);
    } catch (err) {
      // Invalid/unwritable destination: keep the prompt open with the reason
      // (Requirement 14.6).
      setPromptError(asDomainError(err).message);
    } finally {
      setBusy(false);
    }
  }

  async function disable() {
    setBusy(true);
    try {
      await stopLogging(sessionId);
      setEnabled(false);
    } catch (err) {
      onError?.(asDomainError(err).message);
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <button
        type="button"
        className={enabled ? "btn btn-small btn-active" : "btn btn-small"}
        aria-pressed={enabled}
        disabled={busy}
        onClick={() => {
          if (enabled) void disable();
          else {
            setPromptError(null);
            setPrompting(true);
          }
        }}
        title={enabled ? "Logging to file (click to stop)" : "Start logging"}
      >
        {enabled ? "● Logging on" : "Logging off"}
      </button>

      {prompting ? (
        <PromptDialog
          title="Start session logging"
          label="Log file path"
          placeholder="/Users/you/session.log"
          confirmLabel="Start logging"
          busy={busy}
          error={promptError}
          onConfirm={enable}
          onCancel={() => {
            if (!busy) {
              setPrompting(false);
              setPromptError(null);
            }
          }}
        />
      ) : null}
    </>
  );
}

export default LoggingToggle;
