import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Activity,
  ExternalLink,
  Monitor,
  RefreshCw,
  ShieldCheck,
  Sliders,
  Terminal,
} from "lucide-react";
import type {
  RdpConfig,
  RemoteProbeResult,
  SavedSession,
  SshConfig,
  VncConfig,
} from "../types";

interface Props {
  protocol: "rdp" | "vnc";
  title: string;
  rdpConfig?: RdpConfig;
  vncConfig?: VncConfig;
  savedSession?: SavedSession;
  onStatusChange: (status: "connecting" | "live" | "dead", detail?: string) => void;
  onOpenSshToHost: (config: SshConfig) => void;
  onNotice: (level: "info" | "danger", text: string) => void;
}

export default function RemoteDesktopView({
  protocol,
  title,
  rdpConfig,
  vncConfig,
  savedSession,
  onStatusChange,
  onOpenSshToHost,
  onNotice,
}: Props) {
  const host = savedSession?.host ?? rdpConfig?.host ?? vncConfig?.host ?? "";
  const port =
    savedSession?.port ??
    rdpConfig?.port ??
    vncConfig?.port ??
    (protocol === "rdp" ? 3389 : 5900);
  const username =
    savedSession?.username ??
    rdpConfig?.username ??
    vncConfig?.username ??
    "";

  const initialQuality =
    rdpConfig?.quality ??
    vncConfig?.quality ??
    (savedSession?.rdpResolution?.includes("|")
      ? savedSession.rdpResolution.split("|")[1]
      : "ultra");

  const initialResOrMode =
    protocol === "rdp"
      ? rdpConfig?.resolution ?? savedSession?.rdpResolution ?? "fit"
      : vncConfig?.mode ??
        (savedSession?.rdpResolution?.split("|")[0] || "shared");

  const [quality, setQuality] = useState<string>(initialQuality || "ultra");
  const [resOrMode, setResOrMode] = useState<string>(initialResOrMode);
  const [probe, setProbe] = useState<RemoteProbeResult | null>(null);
  const [probing, setProbing] = useState(false);
  const [launchCount, setLaunchCount] = useState(1);

  const runProbe = async () => {
    if (!host) return;
    setProbing(true);
    try {
      const res = await invoke<RemoteProbeResult>("probe_remote_endpoint", {
        host,
        port,
      });
      setProbe(res);
      if (res.reachable) {
        onStatusChange(
          "live",
          `${host}:${port} (${res.latencyMs ?? 1} ms)`,
        );
      } else {
        onStatusChange(
          "live",
          `${host}:${port} · Visor externo activo`,
        );
      }
    } catch {
      onStatusChange("live", `${host}:${port}`);
    } finally {
      setProbing(false);
    }
  };

  useEffect(() => {
    void runProbe();
    const timer = window.setInterval(() => void runProbe(), 5000);
    return () => window.clearInterval(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [host, port]);

  const handleRelaunch = async () => {
    try {
      if (protocol === "rdp") {
        if (savedSession && !rdpConfig?.password) {
          await invoke("connect_saved_rdp", { id: savedSession.id });
        } else {
          const cfg: RdpConfig = {
            host,
            port,
            username: username || "Administrator",
            domain: rdpConfig?.domain ?? savedSession?.domain ?? undefined,
            password: rdpConfig?.password,
            resolution: resOrMode,
            quality,
            clipboard: true,
            audio: true,
            multimon: resOrMode === "fullscreen",
            jumpSessionId:
              rdpConfig?.jumpSessionId ??
              savedSession?.jumpSessionId ??
              undefined,
          };
          await invoke("launch_rdp", { config: cfg });
        }
      } else {
        if (savedSession && !vncConfig?.password) {
          await invoke("connect_saved_vnc", { id: savedSession.id });
        } else {
          const cfg: VncConfig = {
            host,
            port,
            username: username || undefined,
            password: vncConfig?.password,
            variant: vncConfig?.variant ?? savedSession?.domain ?? "auto",
            mode: resOrMode,
            quality,
            jumpSessionId:
              vncConfig?.jumpSessionId ??
              savedSession?.jumpSessionId ??
              undefined,
          };
          await invoke("launch_vnc", { config: cfg });
        }
      }
      setLaunchCount((c) => c + 1);
      void runProbe();
    } catch (e) {
      onNotice("danger", String(e));
    }
  };

  const isRdp = protocol === "rdp";
  const variantLabel = isRdp
    ? "MS-RDPBCGR · FreeRDP / Windows App"
    : vncConfig?.variant === "tight" || savedSession?.domain === "tight"
      ? "TightVNC · RFB 3.8"
      : vncConfig?.variant === "zrle" || savedSession?.domain === "zrle"
        ? "TigerVNC / RealVNC · ZRLE"
        : vncConfig?.variant === "ultra" || savedSession?.domain === "ultra"
          ? "UltraVNC · Hextile"
          : "Apple Screen Sharing / TigerVNC Auto";

  const qualityLabel =
    quality === "ultra"
      ? isRdp
        ? "Ultra HD (32-bit TrueColor · ClearType · H.264 GFX)"
        : "Ultra Nitidez (TrueColor 24-bit · Nivel 9 · Sin pérdida)"
      : quality === "balanced"
        ? isRdp
          ? "Equilibrada (24-bit · ClearType · VPN)"
          : "Equilibrada (Calidad 7 · Compresión 4)"
        : isRdp
          ? "Velocidad WAN (16-bit · Compresión alta)"
          : "Baja Latencia WAN (Calidad 4 · Compresión 6)";

  return (
    <div className="rdp-vnc-workspace">
      <div className="rdp-vnc-card">
        <div className="rdp-vnc-head">
          <div className="rdp-vnc-icon-wrap">
            <Monitor size={22} strokeWidth={1.6} />
          </div>
          <div className="rdp-vnc-title-group">
            <div className="rdp-vnc-badges">
              <span className={`proto-pill ${protocol}`}>
                {isRdp ? "RDP 32-BIT HD" : "VNC RFB HD"}
              </span>
              <span
                className={`rdp-vnc-ping ${
                  probe?.reachable ? "online" : "standby"
                }`}
              >
                <Activity size={12} />
                {probe?.reachable
                  ? `${probe.latencyMs ?? 1} ms · Puerto ${port} activo`
                  : `Puerto ${port} · Visor nativo`}
              </span>
              {probe?.banner && (
                <span className="rdp-vnc-banner-pill">{probe.banner}</span>
              )}
            </div>
            <h3>{title}</h3>
            <p className="rdp-vnc-endpoint">
              {username ? `${username}@` : ""}
              {host}:{port}
            </p>
          </div>
        </div>

        <div className="rdp-vnc-specs-grid">
          <div className="rdp-vnc-spec-box">
            <span className="spec-label">Motor y Protocolo</span>
            <span className="spec-val">{variantLabel}</span>
          </div>
          <div className="rdp-vnc-spec-box">
            <span className="spec-label">Perfil de Calidad Visual</span>
            <span className="spec-val">{qualityLabel}</span>
          </div>
          <div className="rdp-vnc-spec-box">
            <span className="spec-label">
              {isRdp ? "Resolución y Escalado" : "Modo de Ventana VNC"}
            </span>
            <span className="spec-val">
              {resOrMode === "fit"
                ? "Dinámica (Auto-ajuste HiDPI)"
                : resOrMode === "retina"
                  ? "Retina HiDPI (Escalado 180%)"
                  : resOrMode === "shared"
                    ? "Escritorio compartido (Shared)"
                    : resOrMode === "viewonly"
                      ? "Solo lectura (Monitorización)"
                      : resOrMode === "fullscreen"
                        ? "Pantalla completa"
                        : resOrMode}
            </span>
          </div>
          <div className="rdp-vnc-spec-box">
            <span className="spec-label">Seguridad y Cifrado</span>
            <span className="spec-val">
              <ShieldCheck
                size={13}
                style={{
                  display: "inline",
                  verticalAlign: "-2px",
                  marginRight: 4,
                  color: "var(--ok)",
                }}
              />
              {(rdpConfig?.jumpSessionId ||
                vncConfig?.jumpSessionId ||
                savedSession?.jumpSessionId)
                ? "Túnel SSH Cifrado Activo"
                : isRdp
                  ? "TLS / NLA + Cert TOFU"
                  : "RFB Auth + Keychain Vault"}
            </span>
          </div>
        </div>

        <div className="rdp-vnc-controls">
          <div className="rdp-vnc-controls-head">
            <Sliders size={13} />
            <span>Ajuste rápido de calidad y resolución en caliente</span>
          </div>
          <div className="row-split" style={{ gridTemplateColumns: "1fr 1fr" }}>
            <label>
              <span>Calidad de Imagen</span>
              <select
                value={quality}
                onChange={(e) => setQuality(e.target.value)}
              >
                <option value="ultra">
                  {isRdp
                    ? "Ultra HD (32-bit TrueColor + ClearType + GFX)"
                    : "Ultra Nitidez (TrueColor 24-bit · Calidad 9)"}
                </option>
                <option value="balanced">
                  {isRdp
                    ? "Equilibrada (24-bit + ClearType · VPN)"
                    : "Equilibrada (Calidad 7 · Compresión media)"}
                </option>
                <option value="wan">
                  {isRdp
                    ? "Velocidad WAN (16-bit · Enlaces lentos)"
                    : "Velocidad WAN (Calidad 4 · Compresión máxima)"}
                </option>
              </select>
            </label>

            <label>
              <span>{isRdp ? "Resolución / Retina" : "Modo de Sesión"}</span>
              {isRdp ? (
                <select
                  value={resOrMode}
                  onChange={(e) => setResOrMode(e.target.value)}
                >
                  <option value="fit">Dinámica (Ajustar a ventana)</option>
                  <option value="retina">Retina HiDPI (Escalado nítido)</option>
                  <option value="2560x1440">2560 × 1440 (QHD)</option>
                  <option value="1920x1200">1920 × 1200 (16:10)</option>
                  <option value="1920x1080">1920 × 1080 (Full HD)</option>
                  <option value="fullscreen">
                    Pantalla completa / Multi-monitor
                  </option>
                </select>
              ) : (
                <select
                  value={resOrMode}
                  onChange={(e) => setResOrMode(e.target.value)}
                >
                  <option value="shared">Escritorio compartido (Shared)</option>
                  <option value="viewonly">
                    Solo lectura (Monitorización)
                  </option>
                  <option value="fullscreen">
                    Pantalla completa (Fullscreen)
                  </option>
                </select>
              )}
            </label>
          </div>
        </div>

        <div className="rdp-vnc-actions">
          <button
            type="button"
            className="btn accent"
            onClick={() => void handleRelaunch()}
          >
            <ExternalLink size={14} />
            <span>
              {launchCount > 0
                ? `Reabrir visor ${isRdp ? "RDP" : "VNC"} con estos ajustes`
                : `Iniciar visor ${isRdp ? "RDP" : "VNC"}`}
            </span>
          </button>

          <button
            type="button"
            className="btn ghost"
            onClick={() =>
              onOpenSshToHost({
                host,
                port: 22,
                username: username || "root",
              })
            }
          >
            <Terminal size={14} />
            <span>Abrir consola SSH a {host}</span>
          </button>

          <button
            type="button"
            className="btn ghost"
            disabled={probing}
            onClick={() => void runProbe()}
          >
            <RefreshCw size={13} className={probing ? "spin" : ""} />
            <span>Medir latencia</span>
          </button>
        </div>
      </div>
    </div>
  );
}
