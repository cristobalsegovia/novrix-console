import { useState } from "react";
import { Pencil, Trash2, X } from "lucide-react";
import type { Identity, IdentityDraft } from "../types";

interface Props {
  identities: Identity[];
  onSave: (draft: IdentityDraft) => Promise<void>;
  onDelete: (id: string) => Promise<void>;
  onClose: () => void;
}

export default function IdentitiesModal({
  identities,
  onSave,
  onDelete,
  onClose,
}: Props) {
  const [editing, setEditing] = useState<Identity | null>(null);
  const [creating, setCreating] = useState(false);

  const [name, setName] = useState("");
  const [username, setUsername] = useState("");
  const [authMethod, setAuthMethod] = useState<"password" | "key">("password");
  const [password, setPassword] = useState("");
  const [keyPath, setKeyPath] = useState("~/.ssh/id_ed25519");
  const [keyPassphrase, setKeyPassphrase] = useState("");

  const startNew = () => {
    setEditing(null);
    setName("");
    setUsername("");
    setAuthMethod("password");
    setPassword("");
    setKeyPath("~/.ssh/id_ed25519");
    setKeyPassphrase("");
    setCreating(true);
  };

  const startEdit = (ident: Identity) => {
    setEditing(ident);
    setName(ident.name);
    setUsername(ident.username);
    setAuthMethod(ident.authMethod);
    setPassword("");
    setKeyPath(ident.keyPath ?? "~/.ssh/id_ed25519");
    setKeyPassphrase("");
    setCreating(true);
  };

  const submit = async (e?: React.FormEvent) => {
    e?.preventDefault();
    if (!name.trim() || !username.trim()) return;
    await onSave({
      id: editing?.id,
      name: name.trim(),
      username: username.trim(),
      authMethod,
      keyPath: authMethod === "key" ? keyPath.trim() : undefined,
      password: authMethod === "password" && password ? password : undefined,
      keyPassphrase:
        authMethod === "key" && keyPassphrase ? keyPassphrase : undefined,
    });
    setCreating(false);
    setEditing(null);
  };

  return (
    <div className="backdrop" onMouseDown={onClose}>
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Bóveda de identidades"
        className="dialog"
        style={{ width: 520 }}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <header className="dialog-head">
          <h2>Bóveda de identidades</h2>
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
            Credenciales reutilizables en el Llavero de macOS para asignar a
            grupos o sesiones.
          </p>

          {!creating ? (
            <>
              {identities.length === 0 ? (
                <p className="empty-inline">Sin identidades registradas.</p>
              ) : (
                <div className="ident-list">
                  {identities.map((ident) => (
                    <div key={ident.id} className="ident-row">
                      <span className="host-status-dot ssh" aria-hidden="true" />
                      <div className="ident-meta">
                        <span className="ident-name">{ident.name}</span>
                        <span className="ident-sub">{ident.username}</span>
                      </div>
                      <span className="proto-pill">
                        {ident.authMethod === "key"
                          ? ident.keyPath || "Clave SSH"
                          : "Contraseña"}
                      </span>
                      <div className="ident-actions">
                        <button
                          type="button"
                          className="btn-icon"
                          aria-label={`Editar ${ident.name}`}
                          title="Editar"
                          onClick={() => startEdit(ident)}
                        >
                          <Pencil size={13} strokeWidth={1.5} />
                        </button>
                        <button
                          type="button"
                          className="btn-icon danger"
                          aria-label={`Eliminar ${ident.name}`}
                          title="Eliminar"
                          onClick={() => void onDelete(ident.id)}
                        >
                          <Trash2 size={13} strokeWidth={1.5} />
                        </button>
                      </div>
                    </div>
                  ))}
                </div>
              )}

              <footer className="dialog-foot" style={{ padding: "10px 0 0" }}>
                <div className="spacer" />
                <button type="button" className="btn ghost" onClick={onClose}>
                  Cerrar
                </button>
                <button
                  type="button"
                  className="btn accent"
                  onClick={startNew}
                >
                  Nueva identidad
                </button>
              </footer>
            </>
          ) : (
            <form className="ident-form" onSubmit={submit}>
              <div className="ident-form-title">
                {editing ? "Editar identidad" : "Nueva identidad"}
              </div>
              <div
                className="row-split"
                style={{ gridTemplateColumns: "1fr 1fr" }}
              >
                <label>
                  <span>Nombre</span>
                  <input
                    autoFocus
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    placeholder="por ejemplo, Admin Linux"
                  />
                </label>
                <label>
                  <span>Usuario</span>
                  <input
                    value={username}
                    onChange={(e) => setUsername(e.target.value)}
                    placeholder="root"
                    className="mono-input"
                  />
                </label>
              </div>

              <label>
                <span>Autenticación</span>
                <div className="seg" role="radiogroup">
                  <button
                    type="button"
                    className={authMethod === "password" ? "on" : ""}
                    onClick={() => setAuthMethod("password")}
                  >
                    Contraseña
                  </button>
                  <button
                    type="button"
                    className={authMethod === "key" ? "on" : ""}
                    onClick={() => setAuthMethod("key")}
                  >
                    Clave privada SSH
                  </button>
                </div>
              </label>

              {authMethod === "password" ? (
                <label>
                  <span>Contraseña</span>
                  <input
                    type="password"
                    value={password}
                    onChange={(e) => setPassword(e.target.value)}
                    placeholder={editing ? "Sin cambios" : ""}
                  />
                </label>
              ) : (
                <>
                  <label>
                    <span>Ruta de clave privada</span>
                    <input
                      value={keyPath}
                      onChange={(e) => setKeyPath(e.target.value)}
                      placeholder="~/.ssh/id_ed25519"
                      className="mono-input"
                    />
                  </label>
                  <label>
                    <span>Passphrase (opcional)</span>
                    <input
                      type="password"
                      value={keyPassphrase}
                      onChange={(e) => setKeyPassphrase(e.target.value)}
                      placeholder={editing ? "Sin cambios" : ""}
                    />
                  </label>
                </>
              )}

              <footer className="dialog-foot" style={{ padding: "10px 0 0" }}>
                <div className="spacer" />
                <button
                  type="button"
                  className="btn ghost"
                  onClick={() => {
                    setCreating(false);
                    setEditing(null);
                  }}
                >
                  Cancelar
                </button>
                <button
                  type="submit"
                  className="btn accent"
                  disabled={!name.trim() || !username.trim()}
                >
                  Guardar identidad
                </button>
              </footer>
            </form>
          )}
        </div>
      </div>
    </div>
  );
}
