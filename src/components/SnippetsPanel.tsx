/**
 * SnippetsPanel — command snippets manager (Requirement 19).
 *
 * A global modal (opened from the app header) that lets the user manage a
 * library of reusable commands and send one to the active text-based session
 * with a single action:
 *
 * - **List / create / edit / delete** snippets (`list_snippets`, `save_snippet`,
 *   `update_snippet`, `delete_snippet`). Name and command are required; delete
 *   is confirmed (Requirement 19.1, 19.2, 19.3).
 * - **Send** a snippet's command to the active session via `send_input`
 *   (newline-terminated). Sending is available only when there is an active
 *   *text-based* session (SSH / Telnet / Serial) whose protocol the snippet
 *   permits; otherwise the Send control is disabled with an explanation
 *   (Requirement 19.4, 19.5, 19.6). RDP is graphical and has no send path.
 *
 * Styling reuses the shared modal/field/button classes; no bespoke CSS.
 */

import { useEffect, useState } from "react";
import ConfirmDialog from "./ConfirmDialog";
import {
  asDomainError,
  deleteSnippet,
  listSnippets,
  saveSnippet,
  sendInput,
  updateSnippet,
  type CommandSnippet,
  type ProtocolKind,
  type SessionId,
  type SnippetId,
} from "../ipc";

interface SnippetsPanelProps {
  /** The active session's id and protocol, or null when none is active. */
  activeSession: { id: SessionId; protocol: ProtocolKind } | null;
  onClose: () => void;
}

/** Protocols a snippet can be sent to (text-based; RDP is graphical). */
const TEXT_PROTOCOLS: ProtocolKind[] = ["ssh", "telnet", "serial"];

const PROTOCOL_LABELS: Record<ProtocolKind, string> = {
  ssh: "SSH",
  telnet: "Telnet",
  serial: "Serial",
  rdp: "RDP",
};

/** Can `snippet` be sent to `active` right now? Returns a reason when not. */
function sendability(
  snippet: CommandSnippet,
  active: { id: SessionId; protocol: ProtocolKind } | null,
): { ok: true } | { ok: false; reason: string } {
  if (!active) {
    return { ok: false, reason: "Open a text-based session to send this snippet." };
  }
  if (!TEXT_PROTOCOLS.includes(active.protocol)) {
    return { ok: false, reason: "The active session is not text-based." };
  }
  if (snippet.protocol && snippet.protocol !== active.protocol) {
    return {
      ok: false,
      reason: `This snippet is restricted to ${PROTOCOL_LABELS[snippet.protocol]}.`,
    };
  }
  return { ok: true };
}

/** Encode a command as newline-terminated UTF-8 bytes for `send_input`. */
function commandBytes(command: string): number[] {
  const text = command.endsWith("\n") ? command : `${command}\n`;
  return Array.from(new TextEncoder().encode(text));
}

function SnippetsPanel({ activeSession, onClose }: SnippetsPanelProps) {
  const [snippets, setSnippets] = useState<CommandSnippet[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Editor state for the create/edit form.
  const [editingId, setEditingId] = useState<SnippetId | null>(null);
  const [name, setName] = useState("");
  const [command, setCommand] = useState("");
  const [protocol, setProtocol] = useState<ProtocolKind | "">("");
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});

  const [pendingDelete, setPendingDelete] = useState<CommandSnippet | null>(null);

  useEffect(() => {
    let cancelled = false;
    listSnippets()
      .then((list) => {
        if (!cancelled) setSnippets(list);
      })
      .catch((err) => {
        if (!cancelled) setError(asDomainError(err).message);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  function resetEditor() {
    setEditingId(null);
    setName("");
    setCommand("");
    setProtocol("");
    setFieldErrors({});
  }

  function startEdit(s: CommandSnippet) {
    setEditingId(s.id);
    setName(s.name);
    setCommand(s.command);
    setProtocol(s.protocol ?? "");
    setFieldErrors({});
  }

  async function handleSave(e: React.FormEvent) {
    e.preventDefault();
    setError(null);
    setNotice(null);
    setFieldErrors({});
    setBusy(true);
    const draft = {
      name: name.trim(),
      command,
      protocol: protocol === "" ? null : protocol,
    };
    try {
      if (editingId) {
        const updated = await updateSnippet(editingId, draft);
        setSnippets((prev) => prev.map((s) => (s.id === updated.id ? updated : s)));
        setNotice(`Updated “${updated.name}”.`);
      } else {
        const created = await saveSnippet(draft);
        setSnippets((prev) => [...prev, created].sort((a, b) => a.name.localeCompare(b.name)));
        setNotice(`Saved “${created.name}”.`);
      }
      resetEditor();
    } catch (err) {
      const de = asDomainError(err);
      if (de.code.startsWith("validation.")) {
        setFieldErrors({ [de.code.slice("validation.".length)]: de.message });
      } else {
        setError(de.message);
      }
    } finally {
      setBusy(false);
    }
  }

  async function handleSend(s: CommandSnippet) {
    const can = sendability(s, activeSession);
    if (!can.ok || !activeSession) {
      setError(can.ok ? "No active session." : can.reason);
      return;
    }
    setError(null);
    try {
      await sendInput(activeSession.id, commandBytes(s.command));
      setNotice(`Sent “${s.name}” to the active session.`);
    } catch (err) {
      setError(asDomainError(err).message);
    }
  }

  async function confirmDelete() {
    if (!pendingDelete) return;
    const target = pendingDelete;
    setPendingDelete(null);
    try {
      await deleteSnippet(target.id, true);
      setSnippets((prev) => prev.filter((s) => s.id !== target.id));
      if (editingId === target.id) resetEditor();
      setNotice(`Deleted “${target.name}”.`);
    } catch (err) {
      setError(asDomainError(err).message);
    }
  }

  return (
    <div className="modal-overlay" role="presentation" onClick={onClose}>
      <div
        className="modal snippets-modal"
        role="dialog"
        aria-modal="true"
        aria-label="Command snippets"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="session-panel-header">
          <h2 className="modal-title">Command snippets</h2>
          <button
            type="button"
            className="icon-btn"
            aria-label="Close snippets"
            onClick={onClose}
          >
            ✕
          </button>
        </div>

        {error ? (
          <p className="form-error" role="alert">
            {error}
          </p>
        ) : null}
        {notice && !error ? (
          <p className="form-notice" role="status">
            {notice}
          </p>
        ) : null}

        {/* Editor */}
        <form className="field-group" onSubmit={handleSave}>
          <div className="field">
            <label htmlFor="snip-name">Name</label>
            <input
              id="snip-name"
              type="text"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="e.g. Restart nginx"
              aria-invalid={Boolean(fieldErrors.name)}
            />
            {fieldErrors.name ? (
              <p className="field-error" role="alert">
                {fieldErrors.name}
              </p>
            ) : null}
          </div>
          <div className="field">
            <label htmlFor="snip-command">Command</label>
            <textarea
              id="snip-command"
              rows={2}
              value={command}
              onChange={(e) => setCommand(e.target.value)}
              placeholder="systemctl restart nginx"
              aria-invalid={Boolean(fieldErrors.command)}
            />
            {fieldErrors.command ? (
              <p className="field-error" role="alert">
                {fieldErrors.command}
              </p>
            ) : null}
          </div>
          <div className="field">
            <label htmlFor="snip-protocol">Restrict to protocol (optional)</label>
            <select
              id="snip-protocol"
              value={protocol}
              onChange={(e) => setProtocol(e.target.value as ProtocolKind | "")}
            >
              <option value="">Any text-based protocol</option>
              {TEXT_PROTOCOLS.map((p) => (
                <option key={p} value={p}>
                  {PROTOCOL_LABELS[p]}
                </option>
              ))}
            </select>
          </div>
          <div className="form-actions">
            {editingId ? (
              <button type="button" className="btn" onClick={resetEditor} disabled={busy}>
                Cancel edit
              </button>
            ) : null}
            <button type="submit" className="btn btn-primary" disabled={busy}>
              {busy ? (
                <>
                  <span className="spinner" aria-hidden="true" /> Saving…
                </>
              ) : editingId ? (
                "Save changes"
              ) : (
                "Add snippet"
              )}
            </button>
          </div>
        </form>

        {/* List */}
        <ul className="snippet-list">
          {snippets.length === 0 ? (
            <li className="field-empty">No snippets yet. Add one above.</li>
          ) : (
            snippets.map((s) => {
              const can = sendability(s, activeSession);
              return (
                <li className="snippet-row" key={s.id}>
                  <div className="snippet-info">
                    <span className="snippet-name">{s.name}</span>
                    <code className="snippet-command">{s.command}</code>
                    {s.protocol ? (
                      <span className="snippet-tag">{PROTOCOL_LABELS[s.protocol]}</span>
                    ) : null}
                  </div>
                  <div className="snippet-actions">
                    <button
                      type="button"
                      className="btn btn-small btn-primary"
                      disabled={!can.ok}
                      title={can.ok ? "Send to the active session" : can.reason}
                      onClick={() => void handleSend(s)}
                    >
                      Send
                    </button>
                    <button
                      type="button"
                      className="btn btn-small"
                      onClick={() => startEdit(s)}
                    >
                      Edit
                    </button>
                    <button
                      type="button"
                      className="btn btn-small btn-danger"
                      onClick={() => setPendingDelete(s)}
                    >
                      Delete
                    </button>
                  </div>
                </li>
              );
            })
          )}
        </ul>
      </div>

      {pendingDelete ? (
        <ConfirmDialog
          title="Delete snippet"
          message={`Delete the snippet “${pendingDelete.name}”? This cannot be undone.`}
          confirmLabel="Delete"
          onConfirm={confirmDelete}
          onCancel={() => setPendingDelete(null)}
        />
      ) : null}
    </div>
  );
}

export default SnippetsPanel;
