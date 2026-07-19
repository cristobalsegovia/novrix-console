import { useState } from "react";
import type {
  GroupNode,
  Protocol,
  RdpConfig,
  SavedSession,
  SessionDraft,
  SshConfig,
} from "../types";

interface Props {
  groups: GroupNode[];
  rdpAvailable: boolean;
  initial?: SavedSession | null;
  presetGroupId?: string | null;
  onCancel: () => void;
  onConnectSsh: (config: SshConfig) => void;
  onConnectRdp: (config: RdpConfig) => void;
  onSave: (draft: SessionDraft, connectAfter: boolean, newGroupName?: string) => void;
  onInstallHint: () => void;
}

const NEW_GROUP = "__new__";

export default function SshDialog({
  groups,
  rdpAvailable,
  initial,
  presetGroupId,
  onCancel,
  onConnectSsh,
  onConnectRdp,
  onSave,
  onInstallHint,
}: Props) {
  const editing = Boolean(initial);
  const [protocol, setProtocol] = useState<Protocol>(initial?.protocol ?? "ssh");
  const [name, setName] = useState(initial?.name ?? "");
  const [host, setHost] = useState(initial?.host ?? "");
  const [port, setPort] = useState(
    String(initial?.port ?? (initial?.protocol === "rdp" ? 3389 : 22)),
  );
  const [username, setUsername] = useState(initial?.username ?? "");
  const [domain, setDomain] = useState(initial?.domain ?? "");
  const [authMode, setAuthMode] = useState<"password" | "key">(
    initial?.authMethod ?? "password",
  );
  const [password, setPassword] = useState("");
  const [keyPath, setKeyPath] = useState(initial?.keyPath ?? "~/.ssh/id_ed25519");
  const [keyPassphrase, setKeyPassphrase] = useState("");
  const [resolution, setResolution] = useState(initial?.rdpResolution ?? "fit");
  const [save, setSave] = useState(editing || Boolean(presetGroupId));
  const [groupId, setGroupId] = useState(
    initial?.groupId ?? presetGroupId ?? groups[0]?.id ?? NEW_GROUP,
  );
  const [newGroupName, setNewGroupName] = useState("");

  const isRdp = protocol === "rdp";

  const switchProtocol = (p: Protocol) => {
    setProtocol(p);
    // Keep the default port sensible when the user hasn't overridden it.
    if (p === "rdp" && (port === "22" || port === "")) setPort("3389");
    if (p === "ssh" && (port === "3389" || port === "")) setPort("22");
  };

  const ready =
    host.trim() !== "" &&
    username.trim() !== "" &&
    (!save || groupId !== NEW_GROUP || newGroupName.trim() !== "");

  const buildSsh = (): SshConfig => {
    const config: SshConfig = {
      host: host.trim(),
      port: Number(port) || 22,
      username: username.trim(),
    };
    if (authMode === "password") {
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
    username: username.trim(),
    domain: domain.trim() || undefined,
    password: password || undefined,
    resolution,
    clipboard: true,
  });

  const buildDraft = (): SessionDraft => ({
    id: initial?.id,
    groupId: groupId === NEW_GROUP ? "" : groupId,
    name: name.trim() || `${username.trim()}@${host.trim()}`,
    host: host.trim(),
    port: Number(port) || (isRdp ? 3389 : 22),
    username: username.trim(),
    authMethod: isRdp ? "password" : authMode,
    keyPath: !isRdp && authMode === "key" ? keyPath.trim() : undefined,
    protocol,
    domain: isRdp ? domain.trim() || undefined : undefined,
    rdpResolution: isRdp ? resolution : undefined,
    password:
      (isRdp || authMode === "password") && password ? password : undefined,
    keyPassphrase:
      !isRdp && authMode === "key" && keyPassphrase ? keyPassphrase : undefined,
  });

  const submit = (connectAfter: boolean) => {
    if (!ready) return;
    if (connectAfter && isRdp && !rdpAvailable) {
      onInstallHint();
      return;
    }
    if (save || editing) {
      onSave(
        buildDraft(),
        connectAfter,
        groupId === NEW_GROUP ? newGroupName.trim() : undefined,
      );
    } else if (isRdp) {
      onConnectRdp(buildRdp());
    } else {
      onConnectSsh(buildSsh());
    }
  };

  return (
    <div className="dialog-backdrop" onMouseDown={onCancel}>
      <div className="dialog" onMouseDown={(e) => e.stopPropagation()}>
        <header className="dialog-title">
          {editing ? "Edit session" : "New connection"}
        </header>

        {!editing && (
          <div className="proto-toggle" role="radiogroup" aria-label="Protocol">
            <button
              type="button"
              className={!isRdp ? "on" : ""}
              onClick={() => switchProtocol("ssh")}
            >
              SSH
            </button>
            <button
              type="button"
              className={isRdp ? "on" : ""}
              onClick={() => switchProtocol("rdp")}
            >
              RDP
            </button>
          </div>
        )}

        {isRdp && !rdpAvailable && (
          <div className="inline-warn">
            FreeRDP isn't installed. Install it, then reconnect:
            <code>brew install freerdp</code>
            <button className="btn ghost sm" onClick={onInstallHint}>
              Copy command
            </button>
          </div>
        )}

        <div className="field-row">
          <label className="field grow">
            <span>Host</span>
            <input
              autoFocus={!editing}
              value={host}
              onChange={(e) => setHost(e.target.value)}
              placeholder={isRdp ? "10.0.0.5 or dc01.corp" : "192.168.1.10 or nx-core-01.lab"}
            />
          </label>
          <label className="field port">
            <span>Port</span>
            <input
              value={port}
              onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))}
            />
          </label>
        </div>

        <div className="field-row">
          <label className="field grow">
            <span>Username</span>
            <input
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              placeholder={isRdp ? "administrator" : "admin"}
            />
          </label>
          {isRdp && (
            <label className="field grow">
              <span>Domain (optional)</span>
              <input
                value={domain}
                onChange={(e) => setDomain(e.target.value)}
                placeholder="CORP"
              />
            </label>
          )}
        </div>

        {!isRdp && (
          <div className="auth-toggle" role="radiogroup" aria-label="Authentication">
            <button
              type="button"
              className={authMode === "password" ? "on" : ""}
              onClick={() => setAuthMode("password")}
            >
              Password
            </button>
            <button
              type="button"
              className={authMode === "key" ? "on" : ""}
              onClick={() => setAuthMode("key")}
            >
              Private key
            </button>
          </div>
        )}

        {isRdp || authMode === "password" ? (
          <label className="field">
            <span>Password</span>
            <input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder={editing ? "Unchanged — type to replace" : ""}
            />
          </label>
        ) : (
          <>
            <label className="field">
              <span>Key file</span>
              <input
                value={keyPath}
                onChange={(e) => setKeyPath(e.target.value)}
                placeholder="~/.ssh/id_ed25519"
              />
            </label>
            <label className="field">
              <span>Passphrase (optional)</span>
              <input
                type="password"
                value={keyPassphrase}
                onChange={(e) => setKeyPassphrase(e.target.value)}
                placeholder={editing ? "Unchanged — type to replace" : ""}
              />
            </label>
          </>
        )}

        {isRdp && (
          <label className="field">
            <span>Display</span>
            <select value={resolution} onChange={(e) => setResolution(e.target.value)}>
              <option value="fit">Fit window (dynamic)</option>
              <option value="1920x1080">1920 × 1080</option>
              <option value="1600x900">1600 × 900</option>
              <option value="1440x900">1440 × 900</option>
              <option value="1280x800">1280 × 800</option>
            </select>
          </label>
        )}

        {!editing && (
          <label className="save-toggle">
            <input
              type="checkbox"
              checked={save}
              onChange={(e) => setSave(e.target.checked)}
            />
            <span>Save to sidebar (password goes to the macOS Keychain)</span>
          </label>
        )}

        {(save || editing) && (
          <>
            <div className="field-row">
              <label className="field grow">
                <span>Session name</span>
                <input
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  placeholder={`${username || "user"}@${host || "host"}`}
                />
              </label>
              <label className="field grow">
                <span>Group</span>
                <select value={groupId} onChange={(e) => setGroupId(e.target.value)}>
                  {groups.map((g) => (
                    <option key={g.id} value={g.id}>
                      {g.name}
                    </option>
                  ))}
                  <option value={NEW_GROUP}>+ New group…</option>
                </select>
              </label>
            </div>
            {groupId === NEW_GROUP && (
              <label className="field">
                <span>New group name</span>
                <input
                  value={newGroupName}
                  onChange={(e) => setNewGroupName(e.target.value)}
                  placeholder="Client, site, lab…"
                />
              </label>
            )}
          </>
        )}

        <footer className="dialog-actions">
          <button type="button" className="btn ghost" onClick={onCancel}>
            Cancel
          </button>
          {save || editing ? (
            <>
              <button
                type="button"
                className="btn ghost"
                disabled={!ready}
                onClick={() => submit(false)}
              >
                Save
              </button>
              <button
                type="button"
                className="btn accent"
                disabled={!ready}
                onClick={() => submit(true)}
              >
                Save &amp; connect
              </button>
            </>
          ) : (
            <button
              type="button"
              className="btn accent"
              disabled={!ready}
              onClick={() => submit(true)}
            >
              Connect
            </button>
          )}
        </footer>
      </div>
    </div>
  );
}
