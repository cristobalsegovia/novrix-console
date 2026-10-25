import { useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { X } from "lucide-react";

interface Props {
  onImported: () => void;
  onNotice: (level: "info" | "danger", text: string) => void;
  onClose: () => void;
}

export default function ImportModal({ onImported, onNotice, onClose }: Props) {
  const [format, setFormat] = useState<
    "auto" | "mobaxterm" | "securecrt" | "csv"
  >("auto");
  const [content, setContent] = useState("");
  const [busy, setBusy] = useState(false);
  const fileRef = useRef<HTMLInputElement>(null);

  const handleSshConfig = async () => {
    setBusy(true);
    try {
      const count = await invoke<number>("import_ssh_config");
      onImported();
      onNotice(
        "info",
        count > 0
          ? `Se importaron ${count} host${count === 1 ? "" : "s"} desde ~/.ssh/config.`
          : "No se encontraron hosts nuevos en ~/.ssh/config.",
      );
      if (count > 0) onClose();
    } catch (e) {
      onNotice("danger", String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleFileChange = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    if (!file) return;
    const text = await file.text();
    setContent(text);
    const lower = file.name.toLowerCase();
    if (lower.endsWith(".mxtsessions")) setFormat("mobaxterm");
    else if (lower.endsWith(".xml")) setFormat("securecrt");
    else if (lower.endsWith(".csv")) setFormat("csv");
  };

  const handleImportContent = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!content.trim()) return;
    setBusy(true);
    try {
      const count = await invoke<number>("import_sessions_content", {
        format,
        content,
      });
      onImported();
      onNotice(
        "info",
        count > 0
          ? `Se importaron ${count} sesi${count === 1 ? "ón" : "ones"}.`
          : "No se encontraron sesiones nuevas en el archivo.",
      );
      if (count > 0) onClose();
    } catch (err) {
      onNotice("danger", String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="backdrop" onMouseDown={onClose}>
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Importar sesiones"
        className="dialog"
        style={{ width: 560 }}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <header className="dialog-head">
          <h2>Importar sesiones</h2>
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
          <div className="ident-row" style={{ justifyContent: "space-between" }}>
            <div className="ident-meta">
              <span className="ident-name">Configuración OpenSSH local</span>
              <span className="ident-sub">~/.ssh/config</span>
            </div>
            <button
              type="button"
              className="btn ghost sm"
              disabled={busy}
              onClick={() => void handleSshConfig()}
            >
              Importar desde ~/.ssh/config
            </button>
          </div>

          <form onSubmit={handleImportContent} className="ident-form">
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
              }}
            >
              <span className="ident-form-title">
                Desde archivo externo
              </span>
              <button
                type="button"
                className="btn ghost sm"
                onClick={() => fileRef.current?.click()}
              >
                Seleccionar archivo…
              </button>
              <input
                ref={fileRef}
                type="file"
                accept=".mxtsessions,.xml,.csv,.txt"
                style={{ display: "none" }}
                onChange={(e) => void handleFileChange(e)}
              />
            </div>

            <label>
              <span>Formato</span>
              <div className="seg">
                <button
                  type="button"
                  className={format === "auto" ? "on" : ""}
                  onClick={() => setFormat("auto")}
                >
                  Automático
                </button>
                <button
                  type="button"
                  className={format === "mobaxterm" ? "on" : ""}
                  onClick={() => setFormat("mobaxterm")}
                >
                  MobaXterm
                </button>
                <button
                  type="button"
                  className={format === "securecrt" ? "on" : ""}
                  onClick={() => setFormat("securecrt")}
                >
                  SecureCRT XML
                </button>
                <button
                  type="button"
                  className={format === "csv" ? "on" : ""}
                  onClick={() => setFormat("csv")}
                >
                  CSV
                </button>
              </div>
            </label>

            <label>
              <span>Contenido</span>
              <textarea
                rows={5}
                value={content}
                onChange={(e) => setContent(e.target.value)}
                placeholder={
                  "Core-Switch-01,10.10.0.1,22,admin,ssh,Red Core\nFW-Edge-01,10.10.0.254,22,admin,ssh,Firewalls"
                }
              />
              <span className="field-help">
                Formato CSV:{" "}
                <code className="mono-inline">
                  nombre,host,puerto,usuario,protocolo,grupo
                </code>
              </span>
            </label>

            <div className="dialog-foot" style={{ padding: "8px 0 0" }}>
              <div className="spacer" />
              <button type="button" className="btn ghost" onClick={onClose}>
                Cancelar
              </button>
              <button
                type="submit"
                className="btn accent"
                disabled={busy || !content.trim()}
              >
                Importar sesiones
              </button>
            </div>
          </form>
        </div>
      </div>
    </div>
  );
}
