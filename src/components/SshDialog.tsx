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
} from "../types";

interface Props {
  groups: GroupNode[];
  identities: Identity[];
  rdpAvailable: boolean;
  initial?: SavedSession | null;
  presetGroupId?: string | null;
  onCancel: () => void;
  onConnectSsh: (config: SshConfig) => void;
  onConnectRdp: (config: RdpConfig) => void;
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
  onCancel,
  onConnectSsh,
  onConnectRdp,
  onConnectSerial,
  onSave,
  onInstallHint,
  onManageIdentities,
}: Props) {
  const editing = Boolean(initial);
  const [protocol, setProtocol] = useState<Protocol>(
    initial?.protocol ?? "ssh",
  );
  const [name, setName] = useState(initial?.name ?? "");
  const [host, setHost] = useState(initial?.host ?? "");
  const [port, setPort] = useState(
    String(initial?.port ?? (initial?.protocol === "rdp" ? 3389 : 22)),
  );
  const [username, setUsername] = useState(initial?.username ?? "root");
  const domain = initial?.domain ?? "";
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
    if (p === "rdp" && (port === "22" || port === "")) setPort("3389");
    if (p === "ssh" && (port === "3389" || port === "")) setPort("22");
  };

  const selectedIdentity = identities.find((i) => i.id === identityId);
  const effectiveUsername =
    authMode === "identity" && selectedIdentity
      ? selectedIdentity.username
      : username.trim();

  const ready = isSerial
    ? serialPort.trim() !== "" &&
      (!save || groupId !== NEW_GROUP || newGroupName.trim() !== "")
    : host.trim() !== "" &&
      (authMode === "identity" || effectiveUsername !== "") &&
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
    domain: domain.trim() || undefined,
    password: password || undefined,
    resolution: "fit",
    clipboard: true,
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
    port: Number(port) || (isRdp ? 3389 : 22),
    username: isSerial ? "serial" : effectiveUsername || "root",
    authMethod: isSerial ? "password" : authMode,
    keyPath: isSsh && authMode === "key" ? keyPath.trim() : undefined,
    protocol,
    domain: isRdp ? domain.trim() || undefined : undefined,
    rdpResolution: isRdp ? "fit" : undefined,
    identityId: authMode === "identity" ? identityId || undefined : undefined,
    jumpSessionId: isSsh && jumpSessionId ? jumpSessionId : undefined,
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
                  <span>Puerto</span>
                  <input
                    value={port}
                    onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))}
                    style={{ fontFamily: "var(--font-mono)" }}
                  />
                </label>
              </div>

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
                    <span>Usuario</span>
                    <input
                      value={username}
                      onChange={(e) => setUsername(e.target.value)}
                      placeholder="root"
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
          {isSsh && (
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
                Opciones avanzadas (Salto Bastión, equipos antiguos, passphrase)
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
                  {authMode === "key" && (
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
                    <span>Host de salto (ProxyJump / Bastión)</span>
                    <select
                      value={jumpSessionId}
                      onChange={(e) => setJumpSessionId(e.target.value)}
                    >
                      <option value="">Conexión directa (sin bastión)</option>
                      {sshSessions.map((s) => (
                        <option key={s.id} value={s.id}>
                          {s.label}
                        </option>
                      ))}
                    </select>
                  </label>
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
