import { useState } from "react";
import { Check, HelpCircle, Pencil, Plus, Send, Trash2, X, Zap } from "lucide-react";
import type { Snippet } from "../types";

interface Props {
  snippets: Snippet[];
  runMode: boolean;
  hasActiveSession: boolean;
  onToggleRun: () => void;
  onSend: (command: string) => void;
  onSave: (input: { id?: string; name: string; command: string }) => void;
  onDelete: (id: string) => void;
}

export default function SnippetBar({
  snippets,
  runMode,
  hasActiveSession,
  onToggleRun,
  onSend,
  onSave,
  onDelete,
}: Props) {
  const [editing, setEditing] = useState(false);
  const [draftId, setDraftId] = useState<string | null>(null);
  const [draftName, setDraftName] = useState("");
  const [draftCmd, setDraftCmd] = useState("");

  const openNew = () => {
    setDraftId(null);
    setDraftName("");
    setDraftCmd("");
    setEditing(true);
  };

  const openEdit = (s: Snippet) => {
    setDraftId(s.id);
    setDraftName(s.name);
    setDraftCmd(s.command);
    setEditing(true);
  };

  const save = () => {
    const name = draftName.trim();
    const command = draftCmd.trim();
    if (!name || !command) return;
    onSave({ id: draftId ?? undefined, name, command });
    setEditing(false);
  };

  return (
    <div className="snippetbar">
      <button
        className={`btn-icon run-toggle ${runMode ? "on" : ""}`}
        title={
          runMode
            ? "Run mode: click sends the command with Enter"
            : "Insert mode: click types the command without running it"
        }
        onClick={onToggleRun}
      >
        <Zap size={14} />
        <span className="run-label">{runMode ? "Run" : "Insert"}</span>
      </button>

      <div className="snippet-scroll">
        {snippets.length === 0 && (
          <span className="snippet-empty">No snippets yet — add one →</span>
        )}
        {snippets.map((s) => (
          <span key={s.id} className="snippet-chip">
            <button
              className="snippet-run"
              disabled={!hasActiveSession}
              title={hasActiveSession ? s.command : "Open a session first"}
              onClick={() => onSend(s.command)}
            >
              {runMode ? <Send size={11} /> : null}
              {s.name}
            </button>
            <button
              className="snippet-edit"
              title="Edit snippet"
              onClick={() => openEdit(s)}
            >
              <Pencil size={11} />
            </button>
          </span>
        ))}
      </div>

      <button className="btn-icon" title="New snippet" onClick={openNew}>
        <Plus size={15} />
      </button>

      <button
        className="btn-icon"
        title={
          "Copy / paste:\n" +
          "  ⌘C copy · ⌘V paste · right-click copies or pastes\n" +
          "  Long URLs copy in full (wrapped lines are rejoined)\n" +
          "  ⌘-click a URL to open it in your browser\n" +
          "  In Claude Code / vim: hold ⌥ (Option) and drag to select"
        }
        onClick={() => undefined}
        aria-label="Copy and paste help"
      >
        <HelpCircle size={15} />
      </button>

      {editing && (
        <div className="snippet-editor-backdrop" onMouseDown={() => setEditing(false)}>
          <div className="snippet-editor" onMouseDown={(e) => e.stopPropagation()}>
            <header className="dialog-title">
              {draftId ? "Edit snippet" : "New snippet"}
            </header>
            <label className="field">
              <span>Label (shown on the button)</span>
              <input
                autoFocus
                value={draftName}
                onChange={(e) => setDraftName(e.target.value)}
                placeholder="show run"
                onKeyDown={(e) => e.key === "Enter" && save()}
              />
            </label>
            <label className="field">
              <span>Command to send</span>
              <textarea
                value={draftCmd}
                onChange={(e) => setDraftCmd(e.target.value)}
                placeholder="show running-config"
                rows={3}
              />
            </label>
            <footer className="dialog-actions">
              {draftId && (
                <button
                  className="btn ghost danger-text"
                  onClick={() => {
                    onDelete(draftId);
                    setEditing(false);
                  }}
                >
                  <Trash2 size={13} /> Delete
                </button>
              )}
              <span className="spacer" />
              <button className="btn ghost" onClick={() => setEditing(false)}>
                <X size={13} /> Cancel
              </button>
              <button
                className="btn accent"
                disabled={!draftName.trim() || !draftCmd.trim()}
                onClick={save}
              >
                <Check size={13} /> Save
              </button>
            </footer>
          </div>
        </div>
      )}
    </div>
  );
}
