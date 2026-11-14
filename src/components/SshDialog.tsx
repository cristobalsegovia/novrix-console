import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ChevronDown, ChevronRight, RefreshCw, X } from "lucide-react";
import type {
  AuthMethod,
  GroupNode,
  Identity,
  Protocol,
  RdpConfig,
  SavedSession,
  SerialConfig,
  SessionDraft,
  SshConfig,
  VncConfig,
} from "../types";

interface Props {
  groups: GroupNode[];
  identities: Identity[];
  rdpAvailable: boolean;
  initial?: SavedSession | null;
  presetGroupId?: string | null;
  presetProtocol?: Protocol;
  onCancel: () => void;
  onConnectSsh: (config: SshConfig) => void;
  onConnectRdp: (config: RdpConfig) => void;
  onConnectVnc: (config: VncConfig) => void;
  onConnectSerial: (config: SerialConfig) => void;
  onSave: (
    draft: SessionDraft,
    connectAfter: boolean,
    newGroupName?: string,
  ) => void;
  onInstallHint: () => void;
  onManageIdentities: () => void;
}

const NEW_GROUP = "__new__";

function flattenSshSessions(
  nodes: GroupNode[],
  prefix = "",
): { id: string; label: string }[] {
  return nodes.flatMap((g) => {
    const path = prefix ? `${prefix} › ${g.name}` : g.name;
    return [
      ...g.sessions
        .filter((s) => s.protocol === "ssh")
        .map((s) => ({
          id: s.id,
          label: `${path} › ${s.name} (${s.host})`,
        })),
      ...flattenSshSessions(g.children, path),
    ];
  });
}

function flattenGroupOptions(
  nodes: GroupNode[],
  prefix = "",
): { id: string; label: string }[] {
  return nodes.flatMap((g) => {
    const label = prefix ? `${prefix} › ${g.name}` : g.name;
    return [
      { id: g.id, label },
      ...flattenGroupOptions(g.children, label),
    ];
  });
}

export default function SshDialog({
  groups,
  identities,
  rdpAvailable,
  initial,
  presetGroupId,
  presetProtocol,
  onCancel,
  onConnectSsh,
  onConnectRdp,
  onConnectVnc,
  onConnectSerial,
  onSave,
  onInstallHint,
  onManageIdentities,
}: Props) {
  const editing = Boolean(initial);
  const startProto = initial?.protocol ?? presetProtocol ?? "ssh";
  const [protocol, setProtocol] = useState<Protocol>(startProto);
  const [name, setName] = useState(initial?.name ?? "");
  const [host, setHost] = useState(initial?.host ?? "");
  const [port, setPort] = useState(
    String(
      initial?.port ??
        (startProto === "rdp" ? 3389 : startProto === "vnc" ? 5900 : 22),
    ),
  );
  const [username, setUsername] = useState(
    initial?.username ?? (startProto === "vnc" ? "" : "root"),
  );
  const domain = initial?.domain ?? "";
  const [rdpResolution, setRdpResolution] = useState(
    initial?.protocol === "rdp" ? initial?.rdpResolution || "fit" : "fit",
  );
  const [rdpQuality, setRdpQuality] = useState(
    initial?.protocol === "rdp" &&
      initial?.domain &&
      ["ultra", "balanced", "wan"].includes(initial.domain)
      ? initial.domain
      : "ultra",
  );
  const [vncVariant, setVncVariant] = useState(
    initial?.protocol === "vnc" ? initial?.domain || "auto" : "auto",
  );
  const [vncMode, setVncMode] = useState(
    initial?.protocol === "vnc"
      ? (initial?.rdpResolution?.split("|")[0] || "shared")
      : "shared",
  );
  const [vncQuality, setVncQuality] = useState(
    initial?.protocol === "vnc" && initial?.rdpResolution?.includes("|")
      ? initial.rdpResolution.split("|")[1]
      : "ultra",
  );
  const [authMode, setAuthMode] = useState<AuthMethod>(
    initial?.authMethod ?? "password",
  );
  const [password, setPassword] = useState("");
  const [keyPath, setKeyPath] = useState(
    initial?.keyPath ?? "~/.ssh/id_ed25519",
  );
  const [keyPassphrase, setKeyPassphrase] = useState("");
  const [identityId, setIdentityId] = useState(
    initial?.identityId ?? identities[0]?.id ?? "",
  );
  const [jumpSessionId, setJumpSessionId] = useState(
    initial?.jumpSessionId ?? "",
  );
  const [legacyCrypto, setLegacyCrypto] = useState(
    initial?.legacyCrypto ?? false,
  );
  const [showAdvanced, setShowAdvanced] = useState(
    Boolean(initial?.jumpSessionId || initial?.legacyCrypto),
  );

  // Serial state
  const [serialPorts, setSerialPorts] = useState<string[]>([]);
  const [serialPort, setSerialPort] = useState(
    initial?.serialPort ?? initial?.host ?? "",
  );
  const [baudRate, setBaudRate] = useState(
    String(initial?.baudRate ?? 115200),
  );

  const flatGroups = useMemo(() => flattenGroupOptions(groups), [groups]);
  const [save, setSave] = useState(true);
  const [groupId, setGroupId] = useState(
    initial?.groupId ?? presetGroupId ?? flatGroups[0]?.id ?? NEW_GROUP,
  );
  const [newGroupName, setNewGroupName] = useState(
    flatGroups.length === 0 ? "General" : "",
  );

  const isRdp = protocol === "rdp";
  const isVnc = protocol === "vnc";
  const isSerial = protocol === "serial";
  const isSsh = protocol === "ssh";

  const sshSessions = useMemo(
    () => flattenSshSessions(groups).filter((s) => s.id !== initial?.id),
    [groups, initial?.id],
  );

  const refreshSerialPorts = () => {
    invoke<string[]>("list_serial_ports")
      .then((ports) => {
        setSerialPorts(ports);
        if (!serialPort && ports.length > 0) {
          setSerialPort(ports[0]);
        }
      })
      .catch(() => setSerialPorts([]));
  };

  useEffect(() => {
    if (isSerial) {
      refreshSerialPorts();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isSerial]);

  const switchProtocol = (p: Protocol) => {
    setProtocol(p);
    if (p === "rdp" && (port === "22" || port === "5900" || port === ""))
      setPort("3389");
    if (p === "vnc" && (port === "22" || port === "3389" || port === "")) {
      setPort("5900");
      if (username === "root") setUsername("");
      if (authMode === "key") setAuthMode("password");
    }
    if (p === "ssh" && (port === "3389" || port === "5900" || port === "")) {
      setPort("22");
      if (!username) setUsername("root");
    }
  };

  const selectedIdentity = identities.find((i) => i.id === identityId);
  const effectiveUsername =
    authMode === "identity" && selectedIdentity
      ? selectedIdentity.username
      : username.trim();

  const resolveVncPort = (raw: string): number => {
    const n = Number(raw);
    if (!Number.isFinite(n) || n <= 0) return 5900;
    if (n < 100) return 5900 + n; // Display number :1 -> 5901
    return n;
  };

  const ready = isSerial
    ? serialPort.trim() !== "" &&
      (!save || groupId !== NEW_GROUP || newGroupName.trim() !== "")
    : host.trim() !== "" &&
      (isVnc || authMode === "identity" || effectiveUsername !== "") &&
      (!save || groupId !== NEW_GROUP || newGroupName.trim() !== "");

  const buildSsh = (): SshConfig => {
    const config: SshConfig = {
      host: host.trim(),
      port: Number(port) || 22,
      username: effectiveUsername || "root",
      jumpSessionId: jumpSessionId || undefined,
      legacyCrypto,
    };
    if (authMode === "identity") {
      config.identityId = identityId || undefined;
    } else if (authMode === "password") {
      if (password) config.password = password;
    } else {
      config.keyPath = keyPath.trim();
      if (keyPassphrase) config.keyPassphrase = keyPassphrase;
    }
    return config;
  };

  const buildRdp = (): RdpConfig => ({
    host: host.trim(),
    port: Number(port) || 3389,
    username: effectiveUsername,
    domain: domain.trim() || rdpQuality,
    password: password || undefined,
    resolution: rdpResolution,
    quality: rdpQuality,
    clipboard: true,
    audio: true,
    multimon: rdpResolution === "fullscreen",
    jumpSessionId: jumpSessionId || undefined,
  });

  const buildVnc = (): VncConfig => ({
    host: host.trim(),
    port: resolveVncPort(port),
    username: effectiveUsername || undefined,
    password: password || undefined,
    variant: vncVariant,
    mode: vncMode,
    quality: vncQuality,
    jumpSessionId: jumpSessionId || undefined,
  });

  const buildSerial = (): SerialConfig => ({
    portName: serialPort.trim(),
    baudRate: Number(baudRate) || 115200,
  });

  const buildDraft = (): SessionDraft => ({
    id: initial?.id,
    groupId: groupId === NEW_GROUP ? "" : groupId,
    name:
      name.trim() ||
      (isSerial
        ? `${serialPort.trim().replace("/dev/", "")}`
        : host.trim()),
    host: isSerial ? serialPort.trim() : host.trim(),
    port: isVnc
      ? resolveVncPort(port)
      : Number(port) || (isRdp ? 3389 : 22),
    username: isSerial
      ? "serial"
      : isVnc
        ? effectiveUsername
        : effectiveUsername || "root",
    authMethod: isSerial ? "password" : authMode,
    keyPath: isSsh && authMode === "key" ? keyPath.trim() : undefined,
    protocol,
    domain: isRdp
      ? rdpQuality
      : isVnc
        ? vncVariant
        : undefined,
    rdpResolution: isRdp
      ? rdpResolution
      : isVnc
        ? `${vncMode}|${vncQuality}`
        : undefined,
    identityId: authMode === "identity" ? identityId || undefined : undefined,
    jumpSessionId:
      (isSsh || isVnc || isRdp) && jumpSessionId ? jumpSessionId : undefined,
    legacyCrypto: isSsh ? legacyCrypto : false,
    serialPort: isSerial ? serialPort.trim() : undefined,
    baudRate: isSerial ? Number(baudRate) || 115200 : undefined,
    password:
      !isSerial && authMode === "password" && password ? password : undefined,
    keyPassphrase:
      isSsh && authMode === "key" && keyPassphrase ? keyPassphrase : undefined,
  });

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!ready) return;
    if (!editing && isRdp && !rdpAvailable) {
      onInstallHint();
      return;
    }
    if (editing) {
      onSave(
        buildDraft(),
        false,
        groupId === NEW_GROUP ? newGroupName.trim() : undefined,
      );
    } else if (save) {
      onSave(
        buildDraft(),
        true,
        groupId === NEW_GROUP ? newGroupName.trim() : undefined,
      );
    } else if (isRdp) {
      onConnectRdp(buildRdp());
    } else if (isVnc) {
      onConnectVnc(buildVnc());
    } else if (isSerial) {
      onConnectSerial(buildSerial());
    } else {
      onConnectSsh(buildSsh());
    }
  };

  return (
    <div className="backdrop" onMouseDown={onCancel}>
      <form
        className="dialog"
        style={{ width: 480 }}
        onMouseDown={(e) => e.stopPropagation()}
        onSubmit={handleSubmit}
      >
        <header className="dialog-head">
          <h2>{editing ? "Editar conexión" : "Nueva conexión"}</h2>
          <button
            type="button"
            className="btn-icon"
            onClick={onCancel}
            aria-label="Cerrar"
          >
            <X size={15} />
          </button>
        </header>

        <div className="dialog-body">
          <div className="seg" role="radiogroup" aria-label="Protocolo">
            <button
              type="button"
              className={isSsh ? "on" : ""}
              onClick={() => switchProtocol("ssh")}
            >
              SSH
            </button>
            <button
              type="button"
              className={isSerial ? "on" : ""}
              onClick={() => switchProtocol("serial")}
            >
              Consola Serial
            </button>
            <button
              type="button"
              className={isRdp ? "on" : ""}
              onClick={() => switchProtocol("rdp")}
            >
              RDP
            </button>
            <button
              type="button"
              className={isVnc ? "on" : ""}
              onClick={() => switchProtocol("vnc")}
            >
              VNC
            </button>
          </div>

          {isSerial ? (
            <div className="row-split">
              <label>
                <span>Puerto USB-Serial</span>
                <div style={{ display: "flex", gap: 6 }}>
                  <input
                    style={{ flex: 1 }}
                    autoFocus={!editing}
                    list="serial-ports-list"
                    value={serialPort}
                    onChange={(e) => setSerialPort(e.target.value)}
                    placeholder="/dev/cu.usbserial-1410"
                  />
                  <datalist id="serial-ports-list">
                    {serialPorts.map((p) => (
                      <option key={p} value={p} />
                    ))}
                  </datalist>
                  <button
                    type="button"
                    className="btn ghost sm"
                    title="Actualizar puertos USB"
                    onClick={refreshSerialPorts}
                  >
                    <RefreshCw size={13} />
                  </button>
                </div>
              </label>
              <label>
                <span>Velocidad (Baud)</span>
                <select
                  value={baudRate}
                  onChange={(e) => setBaudRate(e.target.value)}
                >
                  <option value="9600">9600 (Cisco)</option>
                  <option value="19200">19200</option>
                  <option value="38400">38400</option>
                  <option value="57600">57600</option>
                  <option value="115200">115200</option>
                </select>
              </label>
            </div>
          ) : (
            <>
              <div className="row-split">
                <label>
                  <span>Host o dirección IP</span>
                  <input
                    autoFocus={!editing}
                    value={host}
                    onChange={(e) => setHost(e.target.value)}
                    placeholder="10.0.0.10 o nvx-01.local"
                    style={{ fontFamily: "var(--font-mono)" }}
                  />
                </label>
                <label>
                  <span>{isVnc ? "Puerto o Display (:0)" : "Puerto"}</span>
                  <input
                    value={port}
                    onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))}
                    placeholder={isVnc ? "5900" : undefined}
                    style={{ fontFamily: "var(--font-mono)" }}
                  />
                </label>
              </div>

              {isRdp && (
                <div
                  className="row-split"
                  style={{ gridTemplateColumns: "1fr 1fr" }}
                >
                  <label>
                    <span>Calidad de Imagen RDP</span>
                    <select
                      value={rdpQuality}
                      onChange={(e) => setRdpQuality(e.target.value)}
                    >
                      <option value="ultra">
                        Ultra HD (32-bit TrueColor + ClearType + GFX)
                      </option>
                      <option value="balanced">
                        Equilibrada (24-bit + ClearType · VPN)
                      </option>
                      <option value="wan">
                        Velocidad WAN (16-bit · Enlaces lentos)
                      </option>
                    </select>
                  </label>
                  <label>
                    <span>Resolución y Escalado</span>
                    <select
                      value={rdpResolution}
                      onChange={(e) => setRdpResolution(e.target.value)}
                    >
                      <option value="fit">
                        Dinámica (Ajustar a ventana)
                      </option>
                      <option value="retina">
                        Retina HiDPI (Escalado nítido)
                      </option>
                      <option value="2560x1440">2560 × 1440 (QHD)</option>
                      <option value="1920x1200">1920 × 1200 (16:10)</option>
                      <option value="1920x1080">1920 × 1080 (Full HD)</option>
                      <option value="fullscreen">
                        Pantalla completa / Multi-monitor
                      </option>
                    </select>
                  </label>
                </div>
              )}

              {isVnc && (
                <>
                  <div
                    className="row-split"
                    style={{ gridTemplateColumns: "1fr 1fr" }}
                  >
                    <label>
                      <span>Variante / Codificación VNC</span>
                      <select
                        value={vncVariant}
                        onChange={(e) => setVncVariant(e.target.value)}
                      >
                        <option value="auto">
                          Automático (Screen Sharing / TigerVNC)
                        </option>
                        <option value="screensharing">
                          macOS Screen Sharing / ARD (Nativo)
                        </option>
                        <option value="tight">TightVNC (Tight Encoding)</option>
                        <option value="zrle">TigerVNC / RealVNC (ZRLE)</option>
                        <option value="ultra">UltraVNC / Hextile</option>
                        <option value="raw">Raw (LAN sin compresión)</option>
                      </select>
                    </label>
                    <label>
                      <span>Modo de sesión</span>
                      <select
                        value={vncMode}
                        onChange={(e) => setVncMode(e.target.value)}
                      >
                        <option value="shared">
                          Escritorio compartido (Shared)
                        </option>
                        <option value="viewonly">
                          Solo lectura (Monitorización)
                        </option>
                        <option value="fullscreen">
                          Pantalla completa (Fullscreen)
                        </option>
                      </select>
                    </label>
                  </div>
                  <label>
                    <span>Calidad de Imagen VNC</span>
                    <select
                      value={vncQuality}
                      onChange={(e) => setVncQuality(e.target.value)}
                    >
                      <option value="ultra">
                        Ultra Nitidez (TrueColor 24-bit · Calidad 9 · Sin pérdida)
                      </option>
                      <option value="balanced">
                        Equilibrada (Calidad 7 · Compresión media · VPN)
                      </option>
                      <option value="wan">
                        Velocidad WAN (Calidad 4 · Compresión máxima)
                      </option>
                    </select>
                  </label>
                </>
              )}

              <label>
                <span>Autenticación</span>
                <div className="seg">
                  <button
                    type="button"
                    className={authMode === "password" ? "on" : ""}
                    onClick={() => setAuthMode("password")}
                  >
                    Contraseña
                  </button>
                  {isSsh && (
                    <button
                      type="button"
                      className={authMode === "key" ? "on" : ""}
                      onClick={() => setAuthMode("key")}
                    >
                      Clave SSH
                    </button>
                  )}
                  <button
                    type="button"
                    className={authMode === "identity" ? "on" : ""}
                    onClick={() => setAuthMode("identity")}
                  >
                    Bóveda
                  </button>
                </div>
              </label>

              {authMode === "identity" ? (
                <div
                  style={{ display: "flex", gap: 8, alignItems: "flex-end" }}
                >
                  <label style={{ flex: 1 }}>
                    <span>Identidad de la bóveda</span>
                    <select
                      value={identityId}
                      onChange={(e) => setIdentityId(e.target.value)}
                    >
                      <option value="">(Heredar del grupo)</option>
                      {identities.map((ident) => (
                        <option key={ident.id} value={ident.id}>
                          {ident.name} ({ident.username})
                        </option>
                      ))}
                    </select>
                  </label>
                  <button
                    type="button"
                    className="btn ghost"
                    onClick={onManageIdentities}
                  >
                    Editar bóveda…
                  </button>
                </div>
              ) : (
                <div className="row-split" style={{ gridTemplateColumns: "1fr 1fr" }}>
                  <label>
                    <span>{isVnc ? "Usuario (opcional en VNC)" : "Usuario"}</span>
                    <input
                      value={username}
                      onChange={(e) => setUsername(e.target.value)}
                      placeholder={isVnc ? "(Sin usuario / ARD)" : "root"}
                      style={{ fontFamily: "var(--font-mono)" }}
                    />
                  </label>
                  {authMode === "password" ? (
                    <label>
                      <span>Contraseña</span>
                      <input
                        type="password"
                        value={password}
                        onChange={(e) => setPassword(e.target.value)}
                        placeholder={editing ? "Sin cambios" : "••••••••"}
                      />
                    </label>
                  ) : (
                    <label>
                      <span>Archivo de clave privada</span>
                      <input
                        value={keyPath}
                        onChange={(e) => setKeyPath(e.target.value)}
                        placeholder="~/.ssh/id_ed25519"
                        style={{ fontFamily: "var(--font-mono)" }}
                      />
                    </label>
                  )}
                </div>
              )}
            </>
          )}

          {/* Guardar en grupo + Nombre en una sola fila limpia */}
          <div className="row-split" style={{ gridTemplateColumns: "1fr 1fr" }}>
            <label>
              <span>Nombre visible (opcional)</span>
              <input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder={host.trim() || "ej. nvx-core-01"}
              />
            </label>
            <label>
              <span>Grupo</span>
              {groupId === NEW_GROUP ? (
                <input
                  value={newGroupName}
                  onChange={(e) => setNewGroupName(e.target.value)}
                  placeholder="Nombre del grupo…"
                />
              ) : (
                <select
                  value={groupId}
                  onChange={(e) => setGroupId(e.target.value)}
                >
                  {flatGroups.map((g) => (
                    <option key={g.id} value={g.id}>
                      {g.label}
                    </option>
                  ))}
                  <option value={NEW_GROUP}>+ Crear nuevo grupo…</option>
                </select>
              )}
            </label>
          </div>

          {/* Opciones avanzadas colapsadas para no saturar */}
          {(isSsh || isVnc || isRdp) && (
            <div>
              <button
                type="button"
                onClick={() => setShowAdvanced((v) => !v)}
                style={{
                  border: 0,
                  background: "none",
                  color: "var(--ink-muted)",
                  font: "inherit",
                  fontSize: 12,
                  padding: "2px 0",
                  cursor: "pointer",
                  display: "inline-flex",
                  alignItems: "center",
                  gap: 4,
                }}
              >
                {showAdvanced ? (
                  <ChevronDown size={13} />
                ) : (
                  <ChevronRight size={13} />
                )}
                {isVnc || isRdp
                  ? `Opciones avanzadas (Túnel ${isRdp ? "RDP" : "VNC"} sobre SSH / Bastión)`
                  : "Opciones avanzadas (Salto Bastión, equipos antiguos, passphrase)"}
              </button>

              {showAdvanced && (
                <div
                  style={{
                    marginTop: 8,
                    padding: 10,
                    borderRadius: 6,
                    background: "var(--surface-sunken)",
                    border: "1px solid var(--border)",
                    display: "flex",
                    flexDirection: "column",
                    gap: 10,
                  }}
                >
                  {isSsh && authMode === "key" && (
                    <label>
                      <span>Passphrase de la clave (opcional)</span>
                      <input
                        type="password"
                        value={keyPassphrase}
                        onChange={(e) => setKeyPassphrase(e.target.value)}
                      />
                    </label>
                  )}
                  <label>
                    <span>
                      {isVnc || isRdp
                        ? `Tunelizar ${isRdp ? "RDP" : "VNC"} a través de servidor SSH (Bastión)`
                        : "Host de salto (ProxyJump / Bastión)"}
                    </span>
                    <select
                      value={jumpSessionId}
                      onChange={(e) => setJumpSessionId(e.target.value)}
                    >
                      <option value="">Conexión directa (sin túnel SSH)</option>
                      {sshSessions.map((s) => (
                        <option key={s.id} value={s.id}>
                          {s.label}
                        </option>
                      ))}
                    </select>
                  </label>
                  {isSsh && (
                    <label className="checkbox-row">
                      <input
                        type="checkbox"
                        checked={legacyCrypto}
                        onChange={(e) => setLegacyCrypto(e.target.checked)}
                      />
                      <span style={{ fontWeight: 400, fontSize: 12.5 }}>
                        Cifrado compatible con switches/routers antiguos (diffie-hellman-group14-sha1)
                      </span>
                    </label>
                  )}
                  {!editing && (
                    <label className="checkbox-row">
                      <input
                        type="checkbox"
                        checked={save}
                        onChange={(e) => setSave(e.target.checked)}
                      />
                      <span style={{ fontWeight: 400, fontSize: 12.5 }}>
                        Guardar sesión en el árbol de infraestructura
                      </span>
                    </label>
                  )}
                </div>
              )}
            </div>
          )}
        </div>

        <footer className="dialog-foot">
          <div className="spacer" />
          <button type="button" className="btn ghost" onClick={onCancel}>
            Cancelar
          </button>
          <button type="submit" className="btn accent" disabled={!ready}>
            {editing
              ? "Guardar cambios"
              : save
                ? "Guardar y conectar"
                : "Conectar"}
          </button>
        </footer>
      </form>
    </div>
  );
}
