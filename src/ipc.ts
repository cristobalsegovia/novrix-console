/**
 * Thin typed client over the Tauri IPC bridge for Novrix Console.
 *
 * Every function here wraps a single `#[tauri::command]` from
 * `src-tauri/src/ipc/mod.rs`. The TypeScript types mirror the backend's serde
 * payloads exactly, so what crosses `invoke()` is what the Rust core expects.
 *
 * serde alignment notes (do not "normalize" these casings):
 * - Top-level domain structs derive `#[serde(rename_all = "camelCase")]`, so
 *   their fields are camelCase (`folderId`, `protocolParams`, ...).
 * - The tagged enums `AuthConfig` (tag "type") and `ProtocolParams`
 *   (tag "protocol") have NO `rename_all` on the enum itself, so only the
 *   *variant tags* are camelCase (`"password"`, `"sshKey"`, `"serial"`, ...)
 *   while the *struct-variant fields* stay snake_case (`credential_key`,
 *   `key_id`, `port_name`, `known_host_policy`, `data_bits`, `stop_bits`).
 * - Unit-variant enums (ProtocolKind, BaudRate, ...) serialize as camelCase
 *   strings (`"telnet"`, `"b115200"`, `"eight"`, ...).
 *
 * A rejected `invoke()` promise carries a `DomainError` payload (`{ code,
 * message, cause? }`); callers switch on `code` (e.g. `validation.host`,
 * `connection.confirmationRequired`).
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// ---------------------------------------------------------------------------
// Id aliases (backend ids are transparent UUID strings)
// ---------------------------------------------------------------------------

export type ProfileId = string;
export type FolderId = string;
export type SessionId = string;
export type KeyId = string;
export type CredentialKey = string;

// ---------------------------------------------------------------------------
// Enums (unit variants serialize as camelCase strings)
// ---------------------------------------------------------------------------

export type ProtocolKind = "ssh" | "telnet" | "serial" | "rdp";

/**
 * Connection state of a session, mirroring the backend `SessionState`
 * (`#[serde(rename_all = "camelCase")]` unit variants).
 */
export type SessionState = "connecting" | "connected" | "disconnected";

export type HostKeyPolicy = "promptOnUnknown" | "acceptAny" | "strict";

export type BaudRate =
  | "b300"
  | "b1200"
  | "b2400"
  | "b4800"
  | "b9600"
  | "b19200"
  | "b38400"
  | "b57600"
  | "b115200";

export type DataBits = "five" | "six" | "seven" | "eight";

export type Parity = "none" | "even" | "odd";

export type StopBits = "one" | "two";

// ---------------------------------------------------------------------------
// Tagged enums — AuthConfig (tag "type") and ProtocolParams (tag "protocol").
// Struct-variant fields keep snake_case (no rename_all on these enums).
// ---------------------------------------------------------------------------

export type AuthConfig =
  | { type: "none" }
  | { type: "password"; credential_key: string }
  | { type: "sshKey"; key_id: KeyId };

export interface Resolution {
  width: number;
  height: number;
}

export type ProtocolParams =
  | { protocol: "ssh"; known_host_policy: HostKeyPolicy }
  | { protocol: "telnet" }
  | {
      protocol: "serial";
      port_name: string;
      baud: BaudRate;
      data_bits: DataBits;
      parity: Parity;
      stop_bits: StopBits;
    }
  | {
      protocol: "rdp";
      resolution: Resolution | null;
      /** Trust policy for the RDP server's TLS certificate. */
      cert_policy: HostKeyPolicy;
    };

// ---------------------------------------------------------------------------
// Structs (camelCase fields)
// ---------------------------------------------------------------------------

export interface ConnectionProfile {
  id: ProfileId;
  name: string;
  protocol: ProtocolKind;
  host: string;
  port: number;
  /** Optional login username (SSH login user / RDP account name). */
  username: string | null;
  /** Optional login domain (RDP only). */
  domain: string | null;
  folderId: FolderId | null;
  auth: AuthConfig;
  protocolParams: ProtocolParams;
  loggingEnabled: boolean;
  logPath: string | null;
  connectTimeoutSecs: number;
}

export interface Folder {
  id: FolderId;
  name: string;
  parentId: FolderId | null;
  depth: number;
}

/**
 * Mirror of the backend `ProfileDraftDto` (camelCase). This is the payload the
 * `save_profile` / `update_profile` commands accept.
 */
export interface ProfileDraft {
  name: string;
  protocol: ProtocolKind;
  host: string;
  port: number;
  /** Optional login username (SSH login user / RDP account name). */
  username: string | null;
  /** Optional login domain (RDP only). */
  domain: string | null;
  folderId: FolderId | null;
  auth: AuthConfig;
  protocolParams: ProtocolParams;
  loggingEnabled: boolean;
  logPath: string | null;
  connectTimeoutSecs: number;
}

export interface DeletionSummary {
  folders: FolderId[];
  profiles: ProfileId[];
}

// ---------------------------------------------------------------------------
// SSH keys (task 21.3) — mirror `domain::{KeyType, ManagedKey}`
// ---------------------------------------------------------------------------

/** Supported SSH key types (unit variants serialize as camelCase strings). */
export type KeyType = "rsa" | "ecdsa" | "ed25519";

/**
 * A managed SSH key pair (camelCase fields, see `domain::ManagedKey`). Only
 * metadata and the public component cross IPC; the private key and any
 * passphrase live in OS Secure_Storage.
 */
export interface ManagedKey {
  id: KeyId;
  keyType: KeyType;
  /** OpenSSH public key, safe to display and copy (Requirement 12.9). */
  publicKey: string;
  /** Whether the private key is protected by a passphrase in Secure_Storage. */
  hasPassphrase: boolean;
}

// ---------------------------------------------------------------------------
// File transfer (task 21.3) — mirror `transfer::{RemoteEntry, TransferProgress}`
// ---------------------------------------------------------------------------

/**
 * One entry in a remote directory listing (camelCase, see
 * `transfer::RemoteEntry`). `size` is bytes; `modified` is a Unix timestamp in
 * seconds (or `null` when the server reports none); `isDir` lets the UI allow
 * drilling into directories (Requirement 13.1).
 */
export interface RemoteEntry {
  name: string;
  size: number;
  modified: number | null;
  isDir: boolean;
}

/**
 * One progress sample for a transfer (camelCase, see
 * `transfer::TransferProgress`). Emitted on `transfer://{id}/progress` at the
 * backend's ≤1 s cadence (Requirement 13.4). `percentage` is `0..=100`.
 */
export interface TransferProgress {
  bytesTransferred: number;
  total: number;
  percentage: number;
}

/**
 * The terminal outcome of a non-blocking transfer (camelCase, see
 * `ipc::events::TransferDoneEvent`). Emitted once on `transfer://{id}/done`.
 *
 * Because `uploadFile`/`downloadFile` now resolve as soon as the transfer
 * *starts* (so {@link cancelTransfer} can reach the live copy), the real
 * result arrives here: `ok` is `true` on a clean completion; otherwise
 * `code`/`message` carry the mapped {@link DomainError} (e.g.
 * `transfer.canceled`, `transfer.failed`).
 */
export interface TransferDone {
  sessionId: SessionId;
  ok: boolean;
  code?: string | null;
  message?: string | null;
}

// ---------------------------------------------------------------------------
// Scripts (task 21.3) — mirror `ipc::{ScriptStepDto, ScriptOutcomeDto}`
// ---------------------------------------------------------------------------

/**
 * A single script step the backend's `run_script` accepts (camelCase, see
 * `ipc::ScriptStepDto`). `waitForPrompt`, when set, holds the next command
 * until that prompt text is seen; `waitTimeoutSecs` overrides the wait timeout.
 */
export interface ScriptStep {
  command: string;
  waitForPrompt?: string | null;
  waitTimeoutSecs?: number | null;
}

/**
 * The tagged outcome of a script run (see `ipc::ScriptOutcomeDto`, tag
 * `status`). `success` reports the number of commands sent (Requirement 15.5);
 * `failure` reports the 0-based `commandIndex` where it stopped plus the cause
 * (Requirement 15.6).
 */
export type ScriptOutcome =
  | { status: "success"; sent: number }
  | {
      status: "failure";
      commandIndex: number;
      code: string;
      message: string;
    };

/**
 * The structured error payload every command rejects with. `code` is a stable
 * machine-readable identifier (e.g. `validation.port`,
 * `connection.confirmationRequired`); `message` is safe to show the user.
 */
export interface DomainError {
  code: string;
  message: string;
  cause?: string | null;
}

/** Narrow an unknown rejection into a {@link DomainError}. */
export function asDomainError(err: unknown): DomainError {
  if (
    err &&
    typeof err === "object" &&
    "code" in err &&
    "message" in err &&
    typeof (err as Record<string, unknown>).code === "string" &&
    typeof (err as Record<string, unknown>).message === "string"
  ) {
    return err as DomainError;
  }
  // Fall back to a generic shape so callers can still render *something*.
  return {
    code: "ipc.unknown",
    message: typeof err === "string" ? err : "An unexpected error occurred.",
  };
}

/** Stable code the backend returns when a destructive op needs confirmation. */
export const CONFIRMATION_REQUIRED_CODE = "connection.confirmationRequired";

/**
 * Stable code the backend returns from `open_session` (and the transfer /
 * script commands) when a protocol's live transport seam is not yet wired —
 * today SSH and RDP. The command shape is final; only the live transport
 * binding is pending. The UI treats this distinctly from a genuine failure and
 * shows a clear "not yet available" message (see `src-tauri/src/ipc/mod.rs`,
 * `NOT_WIRED_CODE`).
 */
export const NOT_WIRED_CODE = "ipc.notYetWired";

// ---------------------------------------------------------------------------
// Command wrappers
// ---------------------------------------------------------------------------

// Profiles
export function listProfiles(): Promise<ConnectionProfile[]> {
  return invoke("list_profiles");
}

export function saveProfile(draft: ProfileDraft): Promise<ConnectionProfile> {
  return invoke("save_profile", { draft });
}

export function updateProfile(
  id: ProfileId,
  draft: ProfileDraft,
): Promise<ConnectionProfile> {
  return invoke("update_profile", { id, draft });
}

export function deleteProfile(
  id: ProfileId,
  confirmed: boolean,
): Promise<void> {
  return invoke("delete_profile", { id, confirmed });
}

/**
 * Store, replace, or clear a profile's password credential (Requirement 17).
 * `password` as a non-empty string saves it to OS secure storage; the empty
 * string clears the stored credential; `null` leaves it unchanged. The secret
 * only ever reaches the Credential_Store, never the connection database.
 */
export function saveCredential(
  profileId: ProfileId,
  password: string | null,
): Promise<void> {
  return invoke("save_credential", { profileId, password });
}

/**
 * Whether a profile has a stored password credential (Requirement 17.3).
 * Returns presence only, never the secret, so the editor can show a
 * "credential stored" indicator.
 */
export function hasCredential(profileId: ProfileId): Promise<boolean> {
  return invoke("has_credential", { profileId });
}

// Folders
export function listFolders(): Promise<Folder[]> {
  return invoke("list_folders");
}

export function createFolder(
  name: string,
  parent: FolderId | null,
): Promise<Folder> {
  return invoke("create_folder", { name, parent });
}

export function renameFolder(id: FolderId, name: string): Promise<Folder> {
  return invoke("rename_folder", { id, name });
}

export function moveProfile(
  id: ProfileId,
  dest: FolderId | null,
): Promise<void> {
  return invoke("move_profile", { id, dest });
}

export function moveFolder(id: FolderId, dest: FolderId | null): Promise<void> {
  return invoke("move_folder", { id, dest });
}

export function deleteFolder(
  id: FolderId,
  confirmed: boolean,
): Promise<DeletionSummary> {
  return invoke("delete_folder", { id, confirmed });
}

// Protocol / serial-port discovery
export function selectableProtocols(): Promise<ProtocolKind[]> {
  return invoke("selectable_protocols");
}

export function listSerialPorts(): Promise<string[]> {
  return invoke("list_serial_ports");
}

// ---------------------------------------------------------------------------
// Session commands (task 21.2)
// ---------------------------------------------------------------------------

/**
 * Open a session for a stored profile; the backend places it in a new active
 * tab and returns its {@link SessionId} (Requirement 9.1).
 *
 * Telnet and Serial are wired end-to-end and genuinely establish; SSH and RDP
 * reject with a {@link DomainError} carrying {@link NOT_WIRED_CODE} until their
 * transport seam lands — callers surface that as a "not yet available" message.
 */
export function openSession(profileId: ProfileId): Promise<SessionId> {
  return invoke("open_session", { profileId });
}

/** Activate a tab so exactly one tab is active (Requirement 9.5). */
export function activateTab(id: SessionId): Promise<void> {
  return invoke("activate_tab", { id });
}

/**
 * Close a tab. For a tab hosting an active (connected) session the backend
 * requires `confirmed = true`; calling with `false` rejects with
 * {@link CONFIRMATION_REQUIRED_CODE} (Requirement 9.7). The caller confirms in
 * the UI first, then invokes with `confirmed = true`.
 */
export function closeTab(id: SessionId, confirmed: boolean): Promise<void> {
  return invoke("close_tab", { id, confirmed });
}

/** The current state of a session, or `null` if it no longer exists. */
export function sessionState(id: SessionId): Promise<SessionState | null> {
  return invoke("session_state", { id });
}

/** Send input bytes (keystrokes / commands) to a session's remote. */
export function sendInput(id: SessionId, bytes: number[]): Promise<void> {
  return invoke("send_input", { id, bytes });
}

/** The id of the currently active tab, or `null` when no tabs are open. */
export function activeTab(): Promise<SessionId | null> {
  return invoke("active_tab");
}

// ---------------------------------------------------------------------------
// Host-key / certificate trust prompt (task 26.3)
// ---------------------------------------------------------------------------

/** The kind of trusted fingerprint a prompt is about. */
export type HostKeyKind = "ssh-host-key" | "rdp-certificate";

/**
 * Payload of the `hostkey://prompt` event (camelCase, see
 * `src-tauri/src/ipc/events.rs::HostKeyPromptEvent`). The backend emits this
 * the first time an SSH host key / RDP certificate is seen under a prompting
 * policy and suspends the connect until {@link respondHostKeyPrompt} is called
 * with the matching `requestId`.
 */
export interface HostKeyPromptEvent {
  requestId: string;
  host: string;
  port: number;
  /** The presented `SHA256:<base64>` fingerprint to confirm. */
  fingerprint: string;
  kind: HostKeyKind;
}

/** The event name the backend emits host-key / certificate trust prompts on. */
export const HOST_KEY_PROMPT_EVENT = "hostkey://prompt";

/**
 * Subscribe to host-key / certificate trust prompts. Each emitted
 * {@link HostKeyPromptEvent} invokes `onPrompt`; the UI shows the fingerprint
 * and calls {@link respondHostKeyPrompt} with the user's decision. Returns the
 * Tauri unlisten function.
 */
export function listenHostKeyPrompt(
  onPrompt: (prompt: HostKeyPromptEvent) => void,
): Promise<UnlistenFn> {
  return listen<HostKeyPromptEvent>(HOST_KEY_PROMPT_EVENT, (event) => {
    onPrompt(event.payload);
  });
}

/**
 * Answer a pending host-key / certificate trust prompt (Requirement 4.7/4.8).
 * `accept = true` trusts and persists the fingerprint and resumes the connect;
 * `false` aborts it with a host-key-declined error. An unknown or
 * already-answered `requestId` is a harmless no-op.
 */
export function respondHostKeyPrompt(
  requestId: string,
  accept: boolean,
): Promise<void> {
  return invoke("respond_host_key_prompt", { requestId, accept });
}

/**
 * Forget a previously-trusted SSH host key / RDP certificate for `host:port`
 * (Requirement 4.8 recovery path). When a server's key or certificate changes,
 * the connection is rejected outright and is never silently re-prompted; this
 * clears the stored fingerprint so the next connect treats the host as unknown
 * again and can accept the new key via {@link listenHostKeyPrompt}. `kind` is
 * `"ssh-host-key"` or `"rdp-certificate"`. Idempotent — forgetting an absent
 * entry succeeds.
 */
export function forgetHostKey(
  host: string,
  port: number,
  kind: HostKeyKind,
): Promise<void> {
  return invoke("forget_host_key", { host, port, kind });
}

// ---------------------------------------------------------------------------
// Session output stream (task 20.2 event `session://{id}/output`)
// ---------------------------------------------------------------------------

/**
 * Payload of a `session://{id}/output` event (camelCase, see
 * `src-tauri/src/ipc/events.rs::SessionOutputEvent`). `data` is the raw remote
 * output bytes exactly as the read loop received them; the terminal view feeds
 * them to xterm.js verbatim.
 */
export interface SessionOutputEvent {
  sessionId: SessionId;
  data: number[];
}

/** Build the per-session output event name the backend namespaces by id. */
export function sessionOutputEventName(id: SessionId): string {
  return `session://${id}/output`;
}

/**
 * Subscribe to a session's remote-output stream. Each emitted chunk invokes
 * `onOutput` with the raw bytes. Returns the Tauri unlisten function; call it
 * (e.g. on unmount) to stop receiving events.
 */
export function listenSessionOutput(
  id: SessionId,
  onOutput: (data: number[]) => void,
): Promise<UnlistenFn> {
  return listen<SessionOutputEvent>(sessionOutputEventName(id), (event) => {
    // Attribute defensively: a broad listener could in principle see other
    // ids, though the per-id event name already isolates this stream.
    if (event.payload.sessionId === id) {
      onOutput(event.payload.data);
    }
  });
}

// ---------------------------------------------------------------------------
// Key commands (task 21.3)
// ---------------------------------------------------------------------------
//
// Note: the backend exposes no `list_keys` command — the Key_Manager surface is
// generate / import / public_key / delete. The frontend is therefore the source
// of truth for the list of managed keys, seeding it from each `generateKey` /
// `importKey` result and trimming it on `deleteKey`, the same frontend-owned
// pattern the open-sessions list uses.

/** Generate a new SSH key pair (Requirements 12.1, 12.2). */
export function generateKey(
  keyType: KeyType,
  size: number,
): Promise<ManagedKey> {
  return invoke("generate_key", { keyType, size });
}

/**
 * Import an existing OpenSSH private key (Requirements 12.3, 12.4). `bytes` is
 * the raw file content as a byte array; `passphrase` decrypts an
 * encrypted key and is stored securely (Requirement 12.5).
 */
export function importKey(
  bytes: number[],
  passphrase?: string | null,
): Promise<ManagedKey> {
  return invoke("import_key", { bytes, passphrase: passphrase ?? null });
}

/** Return a managed key's OpenSSH public key for copying (Requirement 12.9). */
export function keyPublicKey(id: KeyId): Promise<string> {
  return invoke("key_public_key", { id });
}

/**
 * Delete a managed key after confirmation (Requirements 12.6, 12.7). The UI
 * confirms first, then invokes with `confirmed = true`; a `false` call rejects
 * with {@link CONFIRMATION_REQUIRED_CODE}.
 */
export function deleteKey(id: KeyId, confirmed: boolean): Promise<void> {
  return invoke("delete_key", { id, confirmed });
}

// ---------------------------------------------------------------------------
// Transfer commands (task 21.3)
// ---------------------------------------------------------------------------
//
// The command shapes are final. The live SFTP/SCP backend binds to a session's
// russh channel, which is part of the still-unfinished transport wiring, so
// these reject today with {@link NOT_WIRED_CODE}; the UI surfaces that as a
// clear "transfer in integration" notice and otherwise works unchanged once the
// seam lands.

/** List remote directory entries for a session (Requirement 13.1). */
export function listRemote(
  session: SessionId,
  path: string,
): Promise<RemoteEntry[]> {
  return invoke("list_remote", { session, path });
}

/**
 * Start uploading a local file to a remote path (Requirement 13.2).
 * `overwrite = true` is passed only after the user confirms an overwrite
 * (Requirement 13.7).
 *
 * **Non-blocking:** this resolves as soon as the transfer *starts* (so
 * {@link cancelTransfer} can reach the in-flight copy), not when it finishes.
 * A rejection here means the transfer could not even start (e.g. an
 * overwrite conflict). Watch progress on {@link listenTransferProgress} and
 * the terminal outcome on {@link listenTransferDone}.
 */
export function uploadFile(
  session: SessionId,
  local: string,
  remote: string,
  overwrite: boolean,
): Promise<void> {
  return invoke("upload_file", { session, local, remote, overwrite });
}

/**
 * Start downloading a remote file to a local path (Requirement 13.3), with the
 * same non-blocking, cancellable semantics as {@link uploadFile}.
 * `overwrite = true` is passed only after the user confirms an overwrite
 * (Requirement 13.7).
 */
export function downloadFile(
  session: SessionId,
  remote: string,
  local: string,
  overwrite: boolean,
): Promise<void> {
  return invoke("download_file", { session, remote, local, overwrite });
}

/**
 * Cancel the in-flight transfer for a session (Requirement 13.6). The copy
 * stops within ≤2 s, its partial destination is cleaned up, and the terminal
 * `transfer.canceled` outcome arrives on {@link listenTransferDone}. A no-op if
 * no transfer is in flight.
 */
export function cancelTransfer(session: SessionId): Promise<void> {
  return invoke("cancel_transfer", { session });
}

// ---------------------------------------------------------------------------
// Logging commands (task 21.3)
// ---------------------------------------------------------------------------

/**
 * Start recording a session's I/O to a timestamped log file at `path`
 * (Requirements 14.1, 14.2). The backend expects a destination **file path**;
 * an invalid/unwritable path rejects with a `LogError` without enabling logging
 * (Requirement 14.6).
 */
export function startLogging(id: SessionId, path: string): Promise<void> {
  return invoke("start_logging", { id, path });
}

/** Stop recording a session's I/O, retaining written entries (Req 14.3). */
export function stopLogging(id: SessionId): Promise<void> {
  return invoke("stop_logging", { id });
}

// ---------------------------------------------------------------------------
// Script command (task 21.3)
// ---------------------------------------------------------------------------

/**
 * Run an automation script against a text-based session (Requirement 15.x).
 * Returns the {@link ScriptOutcome}: `success` with the sent-count
 * (Requirement 15.5) or `failure` with the stopping command index and cause
 * (Requirement 15.6). Resolving a live session binds to the transport seam, so
 * this may reject with {@link NOT_WIRED_CODE} today — the UI surfaces that
 * clearly.
 */
export function runScript(
  session: SessionId,
  steps: ScriptStep[],
): Promise<ScriptOutcome> {
  return invoke("run_script", { session, steps });
}

// ---------------------------------------------------------------------------
// Transfer progress stream (task 20.2 event `transfer://{id}/progress`)
// ---------------------------------------------------------------------------

/** Build the per-session transfer-progress event name. */
export function transferProgressEventName(id: SessionId): string {
  return `transfer://${id}/progress`;
}

/**
 * Subscribe to a session's transfer-progress stream (Requirement 13.4). Each
 * emitted {@link TransferProgress} sample invokes `onProgress`. Returns the
 * Tauri unlisten function; call it (e.g. on unmount or transfer completion) to
 * stop receiving events. Mirrors {@link listenSessionOutput}.
 */
export function listenTransferProgress(
  id: SessionId,
  onProgress: (progress: TransferProgress) => void,
): Promise<UnlistenFn> {
  return listen<TransferProgress>(transferProgressEventName(id), (event) => {
    onProgress(event.payload);
  });
}

/** Build the per-session transfer-done event name. */
export function transferDoneEventName(id: SessionId): string {
  return `transfer://${id}/done`;
}

/**
 * Subscribe to a session's transfer-done stream (Requirement 13.5/13.6). The
 * single {@link TransferDone} emitted when the non-blocking transfer finishes,
 * is cancelled, or fails invokes `onDone`. Returns the Tauri unlisten function.
 */
export function listenTransferDone(
  id: SessionId,
  onDone: (done: TransferDone) => void,
): Promise<UnlistenFn> {
  return listen<TransferDone>(transferDoneEventName(id), (event) => {
    onDone(event.payload);
  });
}
