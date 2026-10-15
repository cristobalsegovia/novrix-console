/**
 * Create / edit a connection profile.
 *
 * Covers:
 * - Protocol selector populated from `selectable_protocols` (Requirement 8.2).
 * - name / host / port / auth / protocol-specific params.
 * - Serial port selector populated from `list_serial_ports`, with a clear
 *   "no serial ports available" indication when the list is empty
 *   (Requirements 6.3, 6.4).
 * - Save via `save_profile` (create) or `update_profile` (edit). When the
 *   backend rejects with a `validation.<field>` DomainError the message is
 *   shown next to the offending field, identifying it (Requirements 2.1–2.4).
 *
 * The form reports success/cancel to the parent, which owns the data reload.
 * Branding/theme is task 22; neutral CSS-variable-driven styling only here.
 */

import { useEffect, useMemo, useState } from "react";
import {
  asDomainError,
  saveProfile,
  updateProfile,
  type AuthConfig,
  type BaudRate,
  type ConnectionProfile,
  type DataBits,
  type HostKeyPolicy,
  type Parity,
  type ProfileDraft,
  type ProtocolKind,
  type ProtocolParams,
  type StopBits,
} from "../ipc";

interface ProfileFormProps {
  /** Protocols the backend reports as selectable (Requirement 8.2). */
  selectableProtocols: ProtocolKind[];
  /** Serial ports currently available on the host (Requirements 6.3, 6.4). */
  serialPorts: string[];
  /** The profile being edited, or null when creating a new one. */
  editing: ConnectionProfile | null;
  /** Folder to place a newly created profile under (ignored when editing). */
  targetFolderId: string | null;
  onSaved: (profile: ConnectionProfile) => void;
  onCancel: () => void;
}

/** Default per-protocol parameters so the form always has a valid shape. */
function defaultParams(
  protocol: ProtocolKind,
  firstSerialPort: string | undefined,
): ProtocolParams {
  switch (protocol) {
    case "ssh":
      return { protocol: "ssh", known_host_policy: "promptOnUnknown" };
    case "telnet":
      return { protocol: "telnet" };
    case "serial":
      return {
        protocol: "serial",
        port_name: firstSerialPort ?? "",
        baud: "b9600",
        data_bits: "eight",
        parity: "none",
        stop_bits: "one",
      };
    case "rdp":
      return { protocol: "rdp", resolution: null };
  }
}

/** A sensible default port for a freshly selected protocol. */
function defaultPort(protocol: ProtocolKind): number {
  switch (protocol) {
    case "ssh":
      return 22;
    case "telnet":
      return 23;
    case "rdp":
      return 3389;
    case "serial":
      return 0; // Serial has no network port; backend still requires 1..=65535,
    // so we keep a placeholder the user can leave — see note below.
  }
}

const PROTOCOL_LABELS: Record<ProtocolKind, string> = {
  ssh: "SSH",
  telnet: "Telnet",
  serial: "Serial",
  rdp: "RDP",
};

const BAUD_RATES: BaudRate[] = [
  "b300",
  "b1200",
  "b2400",
  "b4800",
  "b9600",
  "b19200",
  "b38400",
  "b57600",
  "b115200",
];
const BAUD_LABELS: Record<BaudRate, string> = {
  b300: "300",
  b1200: "1200",
  b2400: "2400",
  b4800: "4800",
  b9600: "9600",
  b19200: "19200",
  b38400: "38400",
  b57600: "57600",
  b115200: "115200",
};

const DATA_BITS: DataBits[] = ["five", "six", "seven", "eight"];
const DATA_BITS_LABELS: Record<DataBits, string> = {
  five: "5",
  six: "6",
  seven: "7",
  eight: "8",
};

const PARITIES: Parity[] = ["none", "even", "odd"];
const STOP_BITS: StopBits[] = ["one", "two"];
const STOP_BITS_LABELS: Record<StopBits, string> = { one: "1", two: "2" };

const HOST_KEY_POLICIES: HostKeyPolicy[] = [
  "promptOnUnknown",
  "acceptAny",
  "strict",
];
const HOST_KEY_LABELS: Record<HostKeyPolicy, string> = {
  promptOnUnknown: "Prompt on unknown",
  acceptAny: "Accept any",
  strict: "Strict",
};

type AuthKind = AuthConfig["type"];

function ProfileForm(props: ProfileFormProps) {
  const {
    selectableProtocols,
    serialPorts,
    editing,
    targetFolderId,
    onSaved,
    onCancel,
  } = props;

  const isEditing = editing !== null;

  // The protocol choices to offer: whatever the backend reports selectable.
  // When editing a profile whose protocol is (for any reason) not selectable,
  // still include it so the form can represent the stored value.
  const protocolChoices = useMemo<ProtocolKind[]>(() => {
    const set = new Set<ProtocolKind>(selectableProtocols);
    if (editing) set.add(editing.protocol);
    return Array.from(set);
  }, [selectableProtocols, editing]);

  const [name, setName] = useState(editing?.name ?? "");
  const [protocol, setProtocol] = useState<ProtocolKind>(
    editing?.protocol ?? protocolChoices[0] ?? "ssh",
  );
  const [host, setHost] = useState(editing?.host ?? "");
  const [port, setPort] = useState<string>(
    editing ? String(editing.port) : String(defaultPort(protocol)),
  );
  // Optional login identity: username applies to SSH and RDP; domain to RDP.
  const [username, setUsername] = useState(editing?.username ?? "");
  const [domain, setDomain] = useState(editing?.domain ?? "");
  const [authKind, setAuthKind] = useState<AuthKind>(
    editing?.auth.type ?? "none",
  );
  const [credentialKey, setCredentialKey] = useState(
    editing?.auth.type === "password" ? editing.auth.credential_key : "",
  );
  const [keyId, setKeyId] = useState(
    editing?.auth.type === "sshKey" ? editing.auth.key_id : "",
  );
  const [params, setParams] = useState<ProtocolParams>(
    editing?.protocolParams ??
      defaultParams(editing?.protocol ?? protocol, serialPorts[0]),
  );
  const [connectTimeout, setConnectTimeout] = useState<string>(
    editing ? String(editing.connectTimeoutSecs) : "30",
  );
  const [loggingEnabled, setLoggingEnabled] = useState(
    editing?.loggingEnabled ?? false,
  );

  // Per-field validation messages keyed by backend field name (host/port/...).
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});
  // A non-field save failure (Requirement 2.6 error surface).
  const [formError, setFormError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  // When the user switches protocol, reset the protocol-specific params and the
  // default port (only when creating; editing keeps the stored values until the
  // user actively changes protocol).
  function changeProtocol(next: ProtocolKind) {
    setProtocol(next);
    setParams(defaultParams(next, serialPorts[0]));
    setPort(String(defaultPort(next)));
    // Clearing a prior protocol/port error avoids a stale message.
    setFieldErrors((prev) => {
      const { protocol: _p, port: _port, ...rest } = prev;
      return rest;
    });
  }

  // Keep a serial params object's port filled with the first discovered port
  // if it is currently empty and ports have since loaded.
  useEffect(() => {
    setParams((prev) => {
      if (prev.protocol === "serial" && !prev.port_name && serialPorts[0]) {
        return { ...prev, port_name: serialPorts[0] };
      }
      return prev;
    });
  }, [serialPorts]);

  function buildAuth(): AuthConfig {
    switch (authKind) {
      case "none":
        return { type: "none" };
      case "password":
        return { type: "password", credential_key: credentialKey };
      case "sshKey":
        return { type: "sshKey", key_id: keyId };
    }
  }

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    setFieldErrors({});
    setFormError(null);

    const parsedPort = Number.parseInt(port, 10);
    const parsedTimeout = Number.parseInt(connectTimeout, 10);

    // Username applies to SSH/RDP; domain only to RDP. A blank value (or a
    // field hidden for the current protocol) is sent as null so the backend
    // falls back to its documented per-protocol default.
    const trimmedUser = username.trim();
    const trimmedDomain = domain.trim();
    const usesUsername = protocol === "ssh" || protocol === "rdp";

    const draft: ProfileDraft = {
      name: name.trim(),
      protocol,
      host,
      port: Number.isNaN(parsedPort) ? 0 : parsedPort,
      username: usesUsername && trimmedUser !== "" ? trimmedUser : null,
      domain: protocol === "rdp" && trimmedDomain !== "" ? trimmedDomain : null,
      folderId: editing ? editing.folderId : targetFolderId,
      auth: buildAuth(),
      protocolParams: params,
      loggingEnabled,
      logPath: editing?.logPath ?? null,
      connectTimeoutSecs: Number.isNaN(parsedTimeout) ? 0 : parsedTimeout,
    };

    setSaving(true);
    try {
      const result = editing
        ? await updateProfile(editing.id, draft)
        : await saveProfile(draft);
      onSaved(result);
    } catch (err) {
      const domainErr = asDomainError(err);
      // A `validation.<field>` code attaches the message to that field so the
      // UI identifies the offending input (Requirements 2.1–2.4).
      if (domainErr.code.startsWith("validation.")) {
        const field = domainErr.code.slice("validation.".length);
        setFieldErrors({ [field]: domainErr.message });
      } else {
        // Save failure that is not field-specific (Requirement 2.6).
        setFormError(domainErr.message);
      }
    } finally {
      setSaving(false);
    }
  }

  return (
    <form className="profile-form" onSubmit={handleSubmit} noValidate>
      <header className="form-header">
        <h2>{isEditing ? "Edit profile" : "New profile"}</h2>
      </header>

      {formError ? (
        <div className="form-error" role="alert">
          {formError}
        </div>
      ) : null}

      <div className="field">
        <label htmlFor="pf-name">Name</label>
        <input
          id="pf-name"
          type="text"
          value={name}
          onChange={(e) => setName(e.target.value)}
          aria-invalid={Boolean(fieldErrors.name)}
          aria-describedby={fieldErrors.name ? "pf-name-err" : undefined}
        />
        {fieldErrors.name ? (
          <p id="pf-name-err" className="field-error" role="alert">
            {fieldErrors.name}
          </p>
        ) : null}
      </div>

      <div className="field">
        <label htmlFor="pf-protocol">Protocol</label>
        <select
          id="pf-protocol"
          value={protocol}
          onChange={(e) => changeProtocol(e.target.value as ProtocolKind)}
          aria-invalid={Boolean(fieldErrors.protocol)}
          aria-describedby={
            fieldErrors.protocol ? "pf-protocol-err" : undefined
          }
        >
          {protocolChoices.map((p) => (
            <option key={p} value={p}>
              {PROTOCOL_LABELS[p]}
            </option>
          ))}
        </select>
        {fieldErrors.protocol ? (
          <p id="pf-protocol-err" className="field-error" role="alert">
            {fieldErrors.protocol}
          </p>
        ) : null}
      </div>

      <div className="field-row">
        <div className="field">
          <label htmlFor="pf-host">Host</label>
          <input
            id="pf-host"
            type="text"
            value={host}
            onChange={(e) => setHost(e.target.value)}
            placeholder={protocol === "serial" ? "(not used for serial)" : ""}
            aria-invalid={Boolean(fieldErrors.host)}
            aria-describedby={fieldErrors.host ? "pf-host-err" : undefined}
          />
          {fieldErrors.host ? (
            <p id="pf-host-err" className="field-error" role="alert">
              {fieldErrors.host}
            </p>
          ) : null}
        </div>

        <div className="field field-port">
          <label htmlFor="pf-port">Port</label>
          <input
            id="pf-port"
            type="number"
            min={1}
            max={65535}
            value={port}
            onChange={(e) => setPort(e.target.value)}
            aria-invalid={Boolean(fieldErrors.port)}
            aria-describedby={fieldErrors.port ? "pf-port-err" : undefined}
          />
          {fieldErrors.port ? (
            <p id="pf-port-err" className="field-error" role="alert">
              {fieldErrors.port}
            </p>
          ) : null}
        </div>
      </div>

      {/* Login identity: username for SSH and RDP, domain for RDP only. */}
      {protocol === "ssh" || protocol === "rdp" ? (
        <div className="field-row">
          <div className="field">
            <label htmlFor="pf-username">Username</label>
            <input
              id="pf-username"
              type="text"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              placeholder={
                protocol === "ssh"
                  ? "Login user (optional, defaults to root)"
                  : "Account (optional, defaults to Administrator)"
              }
              autoComplete="off"
              aria-invalid={Boolean(fieldErrors.username)}
              aria-describedby={
                fieldErrors.username ? "pf-username-err" : undefined
              }
            />
            {fieldErrors.username ? (
              <p id="pf-username-err" className="field-error" role="alert">
                {fieldErrors.username}
              </p>
            ) : null}
          </div>

          {protocol === "rdp" ? (
            <div className="field">
              <label htmlFor="pf-domain">Domain</label>
              <input
                id="pf-domain"
                type="text"
                value={domain}
                onChange={(e) => setDomain(e.target.value)}
                placeholder="Logon domain (optional)"
                autoComplete="off"
                aria-invalid={Boolean(fieldErrors.domain)}
                aria-describedby={
                  fieldErrors.domain ? "pf-domain-err" : undefined
                }
              />
              {fieldErrors.domain ? (
                <p id="pf-domain-err" className="field-error" role="alert">
                  {fieldErrors.domain}
                </p>
              ) : null}
            </div>
          ) : null}
        </div>
      ) : null}

      {/* Protocol-specific parameters */}
      {params.protocol === "serial" ? (
        <SerialParamsFields
          params={params}
          serialPorts={serialPorts}
          onChange={setParams}
        />
      ) : null}

      {params.protocol === "ssh" ? (
        <div className="field">
          <label htmlFor="pf-hostkey">Host key policy</label>
          <select
            id="pf-hostkey"
            value={params.known_host_policy}
            onChange={(e) =>
              setParams({
                protocol: "ssh",
                known_host_policy: e.target.value as HostKeyPolicy,
              })
            }
          >
            {HOST_KEY_POLICIES.map((p) => (
              <option key={p} value={p}>
                {HOST_KEY_LABELS[p]}
              </option>
            ))}
          </select>
        </div>
      ) : null}

      {/* Authentication */}
      <div className="field">
        <label htmlFor="pf-auth">Authentication</label>
        <select
          id="pf-auth"
          value={authKind}
          onChange={(e) => setAuthKind(e.target.value as AuthKind)}
        >
          <option value="none">None</option>
          <option value="password">Password</option>
          <option value="sshKey">SSH key</option>
        </select>
      </div>

      {authKind === "password" ? (
        <div className="field">
          <label htmlFor="pf-cred">Credential key reference</label>
          <input
            id="pf-cred"
            type="text"
            value={credentialKey}
            onChange={(e) => setCredentialKey(e.target.value)}
            placeholder="e.g. profile/prod-web/password"
          />
          <p className="field-hint">
            A reference to the secret in OS storage, not the password itself.
          </p>
        </div>
      ) : null}

      {authKind === "sshKey" ? (
        <div className="field">
          <label htmlFor="pf-key">Managed key id</label>
          <input
            id="pf-key"
            type="text"
            value={keyId}
            onChange={(e) => setKeyId(e.target.value)}
            placeholder="Managed SSH key identifier"
          />
        </div>
      ) : null}

      <div className="field-row">
        <div className="field field-port">
          <label htmlFor="pf-timeout">Connect timeout (s)</label>
          <input
            id="pf-timeout"
            type="number"
            min={1}
            max={300}
            value={connectTimeout}
            onChange={(e) => setConnectTimeout(e.target.value)}
            aria-invalid={Boolean(fieldErrors.connectTimeoutSecs)}
          />
          {fieldErrors.connectTimeoutSecs ? (
            <p className="field-error" role="alert">
              {fieldErrors.connectTimeoutSecs}
            </p>
          ) : null}
        </div>

        <div className="field field-checkbox">
          <label htmlFor="pf-logging">
            <input
              id="pf-logging"
              type="checkbox"
              checked={loggingEnabled}
              onChange={(e) => setLoggingEnabled(e.target.checked)}
            />
            Enable session logging
          </label>
        </div>
      </div>

      <div className="form-actions">
        <button
          type="button"
          className="btn"
          onClick={onCancel}
          disabled={saving}
        >
          Cancel
        </button>
        <button type="submit" className="btn btn-primary" disabled={saving}>
          {saving ? "Saving…" : isEditing ? "Save changes" : "Create profile"}
        </button>
      </div>
    </form>
  );
}

interface SerialParamsFieldsProps {
  params: Extract<ProtocolParams, { protocol: "serial" }>;
  serialPorts: string[];
  onChange: (next: ProtocolParams) => void;
}

/** Serial-specific inputs, including the port selector / no-ports indication. */
function SerialParamsFields({
  params,
  serialPorts,
  onChange,
}: SerialParamsFieldsProps) {
  const noPorts = serialPorts.length === 0;

  return (
    <fieldset className="field-group">
      <legend>Serial parameters</legend>

      <div className="field">
        <label htmlFor="pf-serial-port">Serial port</label>
        {noPorts ? (
          // Requirement 6.4: empty list + a clear no-ports indication.
          <p className="field-empty" role="status">
            No serial ports available on this computer.
          </p>
        ) : (
          <select
            id="pf-serial-port"
            value={params.port_name}
            onChange={(e) =>
              onChange({ ...params, port_name: e.target.value })
            }
          >
            {/* If the stored port is no longer present, keep it visible. */}
            {params.port_name &&
            !serialPorts.includes(params.port_name) ? (
              <option value={params.port_name}>
                {params.port_name} (not detected)
              </option>
            ) : null}
            {serialPorts.map((portName) => (
              <option key={portName} value={portName}>
                {portName}
              </option>
            ))}
          </select>
        )}
      </div>

      <div className="field-row">
        <div className="field">
          <label htmlFor="pf-baud">Baud</label>
          <select
            id="pf-baud"
            value={params.baud}
            onChange={(e) =>
              onChange({ ...params, baud: e.target.value as BaudRate })
            }
          >
            {BAUD_RATES.map((b) => (
              <option key={b} value={b}>
                {BAUD_LABELS[b]}
              </option>
            ))}
          </select>
        </div>

        <div className="field">
          <label htmlFor="pf-databits">Data bits</label>
          <select
            id="pf-databits"
            value={params.data_bits}
            onChange={(e) =>
              onChange({ ...params, data_bits: e.target.value as DataBits })
            }
          >
            {DATA_BITS.map((d) => (
              <option key={d} value={d}>
                {DATA_BITS_LABELS[d]}
              </option>
            ))}
          </select>
        </div>
      </div>

      <div className="field-row">
        <div className="field">
          <label htmlFor="pf-parity">Parity</label>
          <select
            id="pf-parity"
            value={params.parity}
            onChange={(e) =>
              onChange({ ...params, parity: e.target.value as Parity })
            }
          >
            {PARITIES.map((p) => (
              <option key={p} value={p}>
                {p[0].toUpperCase() + p.slice(1)}
              </option>
            ))}
          </select>
        </div>

        <div className="field">
          <label htmlFor="pf-stopbits">Stop bits</label>
          <select
            id="pf-stopbits"
            value={params.stop_bits}
            onChange={(e) =>
              onChange({ ...params, stop_bits: e.target.value as StopBits })
            }
          >
            {STOP_BITS.map((s) => (
              <option key={s} value={s}>
                {STOP_BITS_LABELS[s]}
              </option>
            ))}
          </select>
        </div>
      </div>
    </fieldset>
  );
}

export default ProfileForm;
