import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { ExternalLink, Square, X } from "lucide-react";
import type { CreateTunnelInput, TunnelInfo } from "../types";

interface SshSessionOption {
  sessionId: string;
  label: string;
}

interface Props {
  sshSessions: SshSessionOption[];
  defaultSessionId?: string;
  onClose: () => void;
  onNotice: (level: "info" | "danger", text: string) => void;
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1).replace(".", ",")} KB`;
  return `${(n / (1024 * 1024)).toFixed(2).replace(".", ",")} MB`;
}

export default function TunnelsModal({
  sshSessions,
  defaultSessionId,
  onClose,
  onNotice,
}: Props) {
  const [tunnels, setTunnels] = useState<TunnelInfo[]>([]);
  const [sessionId, setSessionId] = useState<string>(
    defaultSessionId ?? sshSessions[0]?.sessionId ?? "",
  );
  const [name, setName] = useState("");
  const [kind, setKind] = useState<"local" | "socks5">("local");
  const [bindHost, setBindHost] = useState("127.0.0.1");
  const [localPort, setLocalPort] = useState("8080");
  const [remoteHost, setRemoteHost] = useState("127.0.0.1");
  const [remotePort, setRemotePort] = useState("80");
  const [busy, setBusy] = useState(false);

  const refresh = async () => {
    try {
      setTunnels(await invoke<TunnelInfo[]>("list_tunnels"));
    } catch {
      /* ignore */
    }
  };

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), 1500);
    return () => window.clearInterval(timer);
  }, []);

  const handleStart = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!sessionId) {
      onNotice("danger", "Conéctate a una sesión SSH activa primero.");
      return;
    }
    setBusy(true);
    try {
      const input: CreateTunnelInput = {
        sessionId,
        name: name.trim(),
        kind,
        bindHost: bindHost.trim() || "127.0.0.1",
        localPort: Number(localPort) || 8080,
        remoteHost: kind === "socks5" ? "" : remoteHost.trim() || "127.0.0.1",
        remotePort: kind === "socks5" ? 0 : Number(remotePort) || 80,
      };
      const created = await invoke<TunnelInfo>("start_tunnel", { input });
      setName("");
      await refresh();
      onNotice(
        "info",
        `Túnel activo en ${created.bindHost}:${created.localPort}`,
      );
    } catch (err) {
      onNotice("danger", String(err));
    } finally {
      setBusy(false);
    }
  };

  const handleStop = async (id: string) => {
    try {
      await invoke("stop_tunnel", { id });
      await refresh();
    } catch (err) {
      onNotice("danger", String(err));
    }
  };

  return (
    <div className="backdrop" onMouseDown={onClose}>
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Túneles SSH y proxy SOCKS5"
        className="dialog"
        style={{ width: 560 }}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <header className="dialog-head">
          <h2>Túneles SSH y proxy SOCKS5</h2>
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
            Reenvío de puertos locales (<code className="mono-inline">-L</code>)
            y proxy dinámico SOCKS5 (<code className="mono-inline">-D</code>)
            sobre una sesión SSH en curso.
          </p>

          {tunnels.length > 0 ? (
            <div className="ident-list">
              {tunnels.map((t) => {
                const sessLabel =
                  sshSessions.find((s) => s.sessionId === t.sessionId)?.label ??
                  "SSH";
                return (
                  <div key={t.id} className="ident-row">
                    <span className="jack live" aria-hidden="true" />
                    <div className="ident-meta">
                      <span className="ident-name">{t.name}</span>
                      <span className="ident-sub">
                        {t.kind === "socks5"
                          ? `SOCKS5 ${t.bindHost}:${t.localPort} · ${sessLabel}`
                          : `${t.bindHost}:${t.localPort} → ${t.remoteHost}:${t.remotePort} · ${sessLabel}`}
                      </span>
                    </div>
                    <span className="proto-pill">
                      {t.activeConnections} conex ·{" "}
                      {formatBytes(t.bytesTransferred)}
                    </span>
                    <div className="ident-actions">
                      {t.kind === "local" && (
                        <button
                          type="button"
                          className="btn-icon"
                          aria-label="Abrir en navegador"
                          title={`Abrir http://${t.bindHost}:${t.localPort}`}
                          onClick={() =>
                            void openUrl(`http://${t.bindHost}:${t.localPort}`)
                          }
                        >
                          <ExternalLink size={13} strokeWidth={1.5} />
                        </button>
                      )}
                      <button
                        type="button"
                        className="btn-icon danger"
                        aria-label="Detener túnel"
                        title="Detener túnel"
                        onClick={() => void handleStop(t.id)}
                      >
                        <Square size={12} strokeWidth={1.5} />
                      </button>
                    </div>
                  </div>
                );
              })}
            </div>
          ) : (
            <p className="empty-inline">Sin túneles activos.</p>
          )}

          <form onSubmit={handleStart} className="ident-form">
            <div className="ident-form-title">Nuevo túnel</div>

            <div
              className="row-split"
              style={{ gridTemplateColumns: "1fr 1fr" }}
            >
              <label>
                <span>Sesión SSH</span>
                <select
                  value={sessionId}
                  onChange={(e) => setSessionId(e.target.value)}
                >
                  {sshSessions.length === 0 && (
                    <option value="">Sin sesión SSH conectada</option>
                  )}
                  {sshSessions.map((s) => (
                    <option key={s.sessionId} value={s.sessionId}>
                      {s.label}
                    </option>
                  ))}
                </select>
              </label>

              <label>
                <span>Tipo</span>
                <div className="seg">
                  <button
                    type="button"
                    className={kind === "local" ? "on" : ""}
                    onClick={() => {
                      setKind("local");
                      if (localPort === "1080") setLocalPort("8080");
                    }}
                  >
                    Local (-L)
                  </button>
                  <button
                    type="button"
                    className={kind === "socks5" ? "on" : ""}
                    onClick={() => {
                      setKind("socks5");
                      if (localPort === "8080") setLocalPort("1080");
                    }}
                  >
                    SOCKS5 (-D)
                  </button>
                </div>
              </label>
            </div>

            <div className="row-split">
              <label>
                <span>Dirección local</span>
                <input
                  value={bindHost}
                  onChange={(e) => setBindHost(e.target.value)}
                  placeholder="127.0.0.1"
                  className="mono-input"
                />
              </label>
              <label>
                <span>Puerto local</span>
                <input
                  type="number"
                  value={localPort}
                  onChange={(e) => setLocalPort(e.target.value)}
                  placeholder="8080"
                  className="mono-input"
                />
              </label>
            </div>

            {kind === "local" && (
              <div className="row-split">
                <label>
                  <span>Host destino (desde el nodo remoto)</span>
                  <input
                    value={remoteHost}
                    onChange={(e) => setRemoteHost(e.target.value)}
                    placeholder="127.0.0.1"
                    className="mono-input"
                  />
                </label>
                <label>
                  <span>Puerto destino</span>
                  <input
                    type="number"
                    value={remotePort}
                    onChange={(e) => setRemotePort(e.target.value)}
                    placeholder="443"
                    className="mono-input"
                  />
                </label>
              </div>
            )}

            <label>
              <span>Nombre (opcional)</span>
              <input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder={
                  kind === "socks5"
                    ? "por ejemplo, Proxy corporativo"
                    : "por ejemplo, Consola web interna"
                }
              />
            </label>

            <div className="dialog-foot" style={{ padding: "8px 0 0" }}>
              <div className="spacer" />
              <button type="button" className="btn ghost" onClick={onClose}>
                Cerrar
              </button>
              <button
                type="submit"
                className="btn accent"
                disabled={busy || sshSessions.length === 0}
              >
                Iniciar túnel
              </button>
            </div>
          </form>
        </div>
      </div>
    </div>
  );
}
