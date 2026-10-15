/**
 * ScriptRunnerPanel — automation script runner for one session (task 21.3).
 *
 * Lets the user compose a sequence of commands and run them against the active
 * session via `run_script` (Requirement 15.x). Each line of the editor becomes
 * one {@link ScriptStep}. A step may optionally **wait for a prompt**: lines of
 * the form `command @@ prompt` hold the next command until `prompt` text is
 * received (Requirement 15.2); an optional per-step timeout is set in the field
 * below and applies to every wait step.
 *
 * The outcome is shown as:
 * - **success** with the number of commands sent (Requirement 15.5), or
 * - **failure** with the 0-based index of the command where it stopped and the
 *   cause (Requirement 15.6).
 *
 * ## Wiring status
 *
 * `run_script` is wired to the live session (task 26.5): the backend builds the
 * Script_Engine and drives it over the session's connection, returning the
 * outcome. The legacy {@link NOT_WIRED_CODE} branch below is retained as a
 * harmless fallback but no longer triggers.
 *
 * Branding is task 22; neutral token-driven styling only.
 */

import { useMemo, useState } from "react";
import {
  asDomainError,
  NOT_WIRED_CODE,
  runScript,
  type ScriptOutcome,
  type ScriptStep,
  type SessionId,
} from "../ipc";

interface ScriptRunnerPanelProps {
  sessionId: SessionId;
  onClose: () => void;
}

/** Separator between a command and its wait-for-prompt text in the editor. */
const WAIT_SEP = "@@";

/**
 * Parse the editor text into script steps. One non-empty line per step; a line
 * `cmd @@ prompt` becomes a wait-for-prompt step. `defaultTimeout` is attached
 * to every wait step (or omitted when 0, letting the backend default apply).
 */
function parseScript(text: string, defaultTimeout: number): ScriptStep[] {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0)
    .map((line) => {
      const sep = line.indexOf(WAIT_SEP);
      if (sep === -1) {
        return { command: line } satisfies ScriptStep;
      }
      const command = line.slice(0, sep).trim();
      const prompt = line.slice(sep + WAIT_SEP.length).trim();
      return {
        command,
        waitForPrompt: prompt.length > 0 ? prompt : null,
        waitTimeoutSecs: defaultTimeout > 0 ? defaultTimeout : null,
      } satisfies ScriptStep;
    });
}

function ScriptRunnerPanel({ sessionId, onClose }: ScriptRunnerPanelProps) {
  const [text, setText] = useState("");
  const [timeout, setTimeout] = useState(30);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notWired, setNotWired] = useState(false);
  const [outcome, setOutcome] = useState<ScriptOutcome | null>(null);

  const steps = useMemo(() => parseScript(text, timeout), [text, timeout]);

  async function handleRun() {
    if (steps.length === 0) {
      setError("Add at least one command to run.");
      return;
    }
    setRunning(true);
    setError(null);
    setNotWired(false);
    setOutcome(null);
    try {
      const result = await runScript(sessionId, steps);
      setOutcome(result);
    } catch (err) {
      const domainErr = asDomainError(err);
      if (domainErr.code === NOT_WIRED_CODE) {
        setNotWired(true);
      } else {
        setError(domainErr.message);
      }
    } finally {
      setRunning(false);
    }
  }

  return (
    <div className="session-panel" aria-label="Script runner">
      <div className="session-panel-header">
        <h3 className="session-panel-title">Script runner</h3>
        <button
          type="button"
          className="icon-btn"
          aria-label="Close script runner"
          onClick={onClose}
        >
          ✕
        </button>
      </div>

      <p className="field-hint">
        One command per line. To wait for a prompt before the next command, use{" "}
        <code>command {WAIT_SEP} prompt text</code>.
      </p>

      <div className="field">
        <label htmlFor="script-text">Script</label>
        <textarea
          id="script-text"
          className="script-editor"
          rows={8}
          value={text}
          placeholder={`uptime\nsudo reboot ${WAIT_SEP} password:`}
          onChange={(e) => setText(e.target.value)}
          disabled={running}
        />
      </div>

      <div className="field field-port">
        <label htmlFor="script-timeout">Wait timeout (seconds)</label>
        <input
          id="script-timeout"
          type="number"
          min={1}
          max={3600}
          value={timeout}
          onChange={(e) => setTimeout(Number(e.target.value))}
          disabled={running}
        />
      </div>

      <div className="session-panel-actions">
        <span className="field-hint">
          {steps.length} command{steps.length === 1 ? "" : "s"} parsed
        </span>
        <button
          type="button"
          className="btn btn-primary"
          onClick={handleRun}
          disabled={running || steps.length === 0}
        >
          {running ? "Running…" : "Run script"}
        </button>
      </div>

      {notWired ? (
        <p className="form-notice" role="status">
          Script runner in integration — commands are validated and will run
          against the live session once the connection seam lands.
        </p>
      ) : null}
      {error ? (
        <p className="form-error" role="alert">
          {error}
        </p>
      ) : null}

      {/* Completion status (Requirements 15.5, 15.6) */}
      {outcome ? (
        outcome.status === "success" ? (
          <p className="script-outcome script-outcome-success" role="status">
            ✓ Script completed. {outcome.sent} command
            {outcome.sent === 1 ? "" : "s"} sent.
          </p>
        ) : (
          <div className="script-outcome script-outcome-failure" role="alert">
            <p>
              ✕ Script stopped at command #{outcome.commandIndex + 1} (index{" "}
              {outcome.commandIndex}).
            </p>
            <p className="script-outcome-cause">{outcome.message}</p>
          </div>
        )
      ) : null}
    </div>
  );
}

export default ScriptRunnerPanel;
