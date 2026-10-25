import { useState } from "react";
import { Pencil, Trash2, X } from "lucide-react";
import type { ExpectRule } from "../types";

interface Props {
  rules: ExpectRule[];
  onSave: (input: {
    id?: string;
    name: string;
    expectPattern: string;
    sendText: string;
    enabled: boolean;
  }) => void;
  onToggle: (id: string, enabled: boolean) => void;
  onDelete: (id: string) => void;
  onClose: () => void;
}

export default function TriggersModal({
  rules,
  onSave,
  onToggle,
  onDelete,
  onClose,
}: Props) {
  const [editingId, setEditingId] = useState<string | undefined>(undefined);
  const [name, setName] = useState("");
  const [expectPattern, setExpectPattern] = useState("");
  const [sendText, setSendText] = useState("");
  const [enabled, setEnabled] = useState(true);

  const resetForm = () => {
    setEditingId(undefined);
    setName("");
    setExpectPattern("");
    setSendText("");
    setEnabled(true);
  };

  const startEdit = (r: ExpectRule) => {
    setEditingId(r.id);
    setName(r.name);
    setExpectPattern(r.expectPattern);
    setSendText(r.sendText);
    setEnabled(r.enabled);
  };

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!name.trim() || !expectPattern) return;
    onSave({
      id: editingId,
      name: name.trim(),
      expectPattern,
      sendText,
      enabled,
    });
    resetForm();
  };

  return (
    <div className="backdrop" onMouseDown={onClose}>
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Respuestas automáticas"
        className="dialog"
        style={{ width: 560 }}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <header className="dialog-head">
          <h2>Respuestas automáticas (Expect)</h2>
          <button
            type="button"
            className="btn-icon"
            onClick={onClose}
            aria-label="Cerrar"
          >
            <X size={14} strokeWidth={1.5} />
          </button>
        </header>

        <div className="dialog-body">
          <p className="ident-intro">
            Envía una respuesta cuando la salida de la consola coincide con un
            patrón (usa <code className="mono-inline">\r</code> para Enter).
          </p>

          {rules.length === 0 ? (
            <p className="empty-inline">Sin reglas configuradas.</p>
          ) : (
            <div className="ident-list">
              {rules.map((r) => (
                <div key={r.id} className="ident-row">
                  <input
                    type="checkbox"
                    checked={r.enabled}
                    onChange={(e) => onToggle(r.id, e.target.checked)}
                    style={{ width: 15, height: 15, cursor: "pointer" }}
                    aria-label={`Activar ${r.name}`}
                  />
                  <div className="ident-meta">
                    <span className="ident-name">{r.name}</span>
                    <span className="ident-sub">
                      {r.expectPattern} →{" "}
                      {r.sendText === " " ? "<espacio>" : r.sendText}
                    </span>
                  </div>
                  <span className={`proto-pill ${r.enabled ? "ssh" : ""}`}>
                    {r.enabled ? "Activa" : "En pausa"}
                  </span>
                  <div className="ident-actions">
                    <button
                      type="button"
                      className="btn-icon"
                      aria-label={`Editar ${r.name}`}
                      title="Editar"
                      onClick={() => startEdit(r)}
                    >
                      <Pencil size={13} strokeWidth={1.5} />
                    </button>
                    <button
                      type="button"
                      className="btn-icon danger"
                      aria-label={`Eliminar ${r.name}`}
                      title="Eliminar"
                      onClick={() => onDelete(r.id)}
                    >
                      <Trash2 size={13} strokeWidth={1.5} />
                    </button>
                  </div>
                </div>
              ))}
            </div>
          )}

          <form onSubmit={handleSubmit} className="ident-form">
            <div className="ident-form-title">
              {editingId ? "Editar regla" : "Nueva regla"}
            </div>
            <label>
              <span>Nombre</span>
              <input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="por ejemplo, Paginación Cisco"
              />
            </label>
            <div
              className="row-split"
              style={{ gridTemplateColumns: "1fr 1fr" }}
            >
              <label>
                <span>Texto recibido (Expect)</span>
                <input
                  value={expectPattern}
                  onChange={(e) => setExpectPattern(e.target.value)}
                  placeholder="--More--"
                  className="mono-input"
                />
              </label>
              <label>
                <span>Respuesta a enviar</span>
                <input
                  value={sendText}
                  onChange={(e) => setSendText(e.target.value)}
                  placeholder="enable\r"
                  className="mono-input"
                />
              </label>
            </div>
            <div className="dialog-foot" style={{ padding: "8px 0 0" }}>
              {editingId && (
                <button
                  type="button"
                  className="btn ghost"
                  onClick={resetForm}
                >
                  Cancelar edición
                </button>
              )}
              <div className="spacer" />
              <button type="button" className="btn ghost" onClick={onClose}>
                Cerrar
              </button>
              <button
                type="submit"
                className="btn accent"
                disabled={!name.trim() || !expectPattern}
              >
                {editingId ? "Guardar cambios" : "Guardar regla"}
              </button>
            </div>
          </form>
        </div>
      </div>
    </div>
  );
}
