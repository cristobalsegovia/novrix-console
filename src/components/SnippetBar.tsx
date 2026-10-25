import { useState } from "react";
import { Pencil, X } from "lucide-react";
import type { Snippet } from "../types";

interface Props {
  snippets: Snippet[];
  runMode: boolean;
  hasActiveSession: boolean;
  onToggleRun: () => void;
  onSend: (command: string) => void;
  onSave: (input: { id?: string; name: string; command: string }) => void;
  onDelete: (id: string) => void;
  onClose?: () => void;
}

function extractVariables(cmd: string): string[] {
  const vars: string[] = [];
  const re = /\{\{\s*([a-zA-Z0-9_-]+)\s*\}\}/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(cmd)) !== null) {
    if (!vars.includes(m[1])) vars.push(m[1]);
  }
  return vars;
}

export default function SnippetBar({
  snippets,
  runMode,
  hasActiveSession,
  onToggleRun,
  onSend,
  onSave,
  onDelete,
  onClose,
}: Props) {
  const [editing, setEditing] = useState(false);
  const [draftId, setDraftId] = useState<string | null>(null);
  const [draftName, setDraftName] = useState("");
  const [draftCmd, setDraftCmd] = useState("");

  const [paramSnippet, setParamSnippet] = useState<Snippet | null>(null);
  const [paramVars, setParamVars] = useState<string[]>([]);
  const [paramValues, setParamValues] = useState<Record<string, string>>({});

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

  const save = (e?: React.FormEvent) => {
    e?.preventDefault();
    const name = draftName.trim();
    const command = draftCmd.trim();
    if (!name || !command) return;
    onSave({ id: draftId ?? undefined, name, command });
    setEditing(false);
  };

  const handleTrigger = (s: Snippet) => {
    const vars = extractVariables(s.command);
    if (vars.length === 0) {
      onSend(s.command);
      return;
    }
    const initial: Record<string, string> = {};
    for (const v of vars) initial[v] = "";
    setParamSnippet(s);
    setParamVars(vars);
    setParamValues(initial);
  };

  const submitParams = (e?: React.FormEvent) => {
    e?.preventDefault();
    if (!paramSnippet) return;
    let resolved = paramSnippet.command;
    for (const v of paramVars) {
      const val = paramValues[v] ?? "";
      resolved = resolved.replace(
        new RegExp(`\\{\\{\\s*${v}\\s*\\}\\}`, "g"),
        val,
      );
    }
    setParamSnippet(null);
    onSend(resolved);
  };

  return (
    <div className="snippets" role="toolbar" aria-label="Snippets de comandos">
      <span className="snippets-label">Snippets</span>

      <button
        type="button"
        className={`snippet-run ${runMode ? "on" : ""}`}
        title={
          runMode
            ? "Modo Ejecutar: envía el comando con Enter"
            : "Modo Insertar: pega el comando sin ejecutarlo"
        }
        onClick={onToggleRun}
      >
        {runMode ? "Ejecutar ↵" : "Insertar"}
      </button>

      <span className="snippets-sep" aria-hidden="true" />

      <div className="snippets-list">
        {snippets.length === 0 && (
          <span className="snippets-empty">Sin comandos guardados</span>
        )}
        {snippets.map((s) => (
          <span
            key={s.id}
            className="snippet-chip"
            onContextMenu={(e) => {
              e.preventDefault();
              openEdit(s);
            }}
          >
            <button
              type="button"
              className="snippet-send"
              disabled={!hasActiveSession}
              title={`${s.command} · Clic derecho para editar`}
              onClick={() => handleTrigger(s)}
            >
              {s.name}
            </button>
            <button
              type="button"
              className="snippet-Action"
              aria-label={`Editar ${s.name}`}
              title="Editar snippet"
              onClick={() => openEdit(s)}
            >
              <Pencil size={11} strokeWidth={1.5} />
            </button>
          </span>
        ))}
      </div>

      <button type="button" className="snippet-add" onClick={openNew}>
        Nuevo snippet
      </button>

      {onClose && (
        <button
          type="button"
          className="btn-icon"
          aria-label="Cerrar barra de snippets"
          title="Ocultar barra"
          onClick={onClose}
        >
          <X size={14} strokeWidth={1.5} />
        </button>
      )}

      {paramSnippet && (
        <div className="backdrop" onMouseDown={() => setParamSnippet(null)}>
          <form
            className="dialog"
            style={{ width: 420 }}
            onMouseDown={(e) => e.stopPropagation()}
            onSubmit={submitParams}
          >
            <header className="dialog-head">
              <h2>{paramSnippet.name}</h2>
              <button
                type="button"
                className="btn-icon"
                onClick={() => setParamSnippet(null)}
                aria-label="Cerrar"
              >
                <X size={14} strokeWidth={1.5} />
              </button>
            </header>
            <div className="dialog-body">
              <code className="snippet-cmd-preview">
                {paramSnippet.command}
              </code>
              {paramVars.map((v, idx) => (
                <label key={v}>
                  <span>{v}</span>
                  <input
                    autoFocus={idx === 0}
                    value={paramValues[v] ?? ""}
                    onChange={(e) =>
                      setParamValues((prev) => ({
                        ...prev,
                        [v]: e.target.value,
                      }))
                    }
                    placeholder={`Valor para ${v}`}
                    className="mono-input"
                  />
                </label>
              ))}
            </div>
            <footer className="dialog-foot">
              <div className="spacer" />
              <button
                type="button"
                className="btn ghost"
                onClick={() => setParamSnippet(null)}
              >
                Cancelar
              </button>
              <button type="submit" className="btn accent">
                {runMode ? "Ejecutar comando" : "Insertar comando"}
              </button>
            </footer>
          </form>
        </div>
      )}

      {editing && (
        <div className="backdrop" onMouseDown={() => setEditing(false)}>
          <form
            className="dialog"
            style={{ width: 460 }}
            onMouseDown={(e) => e.stopPropagation()}
            onSubmit={save}
          >
            <header className="dialog-head">
              <h2>{draftId ? "Editar snippet" : "Nuevo snippet"}</h2>
              <button
                type="button"
                className="btn-icon"
                onClick={() => setEditing(false)}
                aria-label="Cerrar"
              >
                <X size={14} strokeWidth={1.5} />
              </button>
            </header>
            <div className="dialog-body">
              <label>
                <span>Nombre</span>
                <input
                  autoFocus
                  value={draftName}
                  onChange={(e) => setDraftName(e.target.value)}
                  placeholder="por ejemplo, show int status"
                />
              </label>
              <label>
                <span>Comando</span>
                <textarea
                  value={draftCmd}
                  onChange={(e) => setDraftCmd(e.target.value)}
                  placeholder="show interface {{interfaz}} status"
                  rows={3}
                />
                <span className="field-help">
                  Usa <code className="mono-inline">{"{{variable}}"}</code> si
                  necesitas pedir un valor antes de enviarlo.
                </span>
              </label>
            </div>
            <footer className="dialog-foot">
              {draftId && (
                <button
                  type="button"
                  className="btn danger"
                  onClick={() => {
                    onDelete(draftId);
                    setEditing(false);
                  }}
                >
                  Eliminar snippet
                </button>
              )}
              <div className="spacer" />
              <button
                type="button"
                className="btn ghost"
                onClick={() => setEditing(false)}
              >
                Cancelar
              </button>
              <button
                type="submit"
                className="btn accent"
                disabled={!draftName.trim() || !draftCmd.trim()}
              >
                Guardar snippet
              </button>
            </footer>
          </form>
        </div>
      )}
    </div>
  );
}
