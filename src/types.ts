export type AuthMethod = "password" | "key" | "identity";

export interface SshConfig {
  host: string;
  port: number;
  username: string;
  password?: string;
  keyPath?: string;
  keyPassphrase?: string;
  identityId?: string;
  jumpSessionId?: string;
  legacyCrypto?: boolean;
}

export interface SerialConfig {
  portName: string;
  baudRate: number;
}

export type Protocol = "ssh" | "rdp" | "serial";

export interface RdpConfig {
  host: string;
  port: number;
  username: string;
  domain?: string;
  password?: string;
  resolution?: string;
  clipboard?: boolean;
  multimon?: boolean;
}

export interface Identity {
  id: string;
  name: string;
  username: string;
  authMethod: "password" | "key";
  keyPath?: string | null;
}

export interface IdentityDraft {
  id?: string;
  name: string;
  username: string;
  authMethod: "password" | "key";
  keyPath?: string;
  password?: string;
  keyPassphrase?: string;
}

export interface SavedSession {
  id: string;
  groupId: string;
  name: string;
  host: string;
  port: number;
  username: string;
  authMethod: AuthMethod;
  keyPath?: string | null;
  protocol: Protocol;
  domain?: string | null;
  rdpResolution?: string | null;
  identityId?: string | null;
  jumpSessionId?: string | null;
  legacyCrypto?: boolean;
  serialPort?: string | null;
  baudRate?: number;
}

export interface GroupNode {
  id: string;
  name: string;
  parentId?: string | null;
  defaultIdentityId?: string | null;
  sessions: SavedSession[];
  children: GroupNode[];
}

export interface Snippet {
  id: string;
  name: string;
  command: string;
}

export type SessionSpec =
  | { kind: "local" }
  | { kind: "ssh"; config: SshConfig }
  | { kind: "serial"; config: SerialConfig }
  | { kind: "saved"; savedId: string; protocol?: Protocol };

export interface PaneState {
  paneId: string;
  title: string;
  spec: SessionSpec;
  sessionId?: string;
  status: "connecting" | "live" | "dead";
  statusDetail?: string;
  reconnectEpoch: number;
}

export interface Tab {
  tabId: string;
  title: string;
  spec: SessionSpec;
  sessionId?: string;
  status: "connecting" | "live" | "dead";
  statusDetail?: string;
  panes: PaneState[];
  activePaneId: string;
  splitDirection: "horizontal" | "vertical";
}

/** What the session dialog hands back to App when saving. */
export interface SessionDraft {
  id?: string;
  groupId: string;
  name: string;
  host: string;
  port: number;
  username: string;
  authMethod: AuthMethod;
  keyPath?: string;
  protocol: Protocol;
  domain?: string;
  rdpResolution?: string;
  identityId?: string;
  jumpSessionId?: string;
  legacyCrypto?: boolean;
  serialPort?: string;
  baudRate?: number;
  password?: string;
  keyPassphrase?: string;
}

export interface RdpAvailability {
  available: boolean;
  binary?: string | null;
}

export interface SftpEntry {
  name: string;
  path: string;
  isDir: boolean;
  size: number;
  modified: number;
  permissions: number;
}

export interface SftpDirListing {
  path: string;
  entries: SftpEntry[];
}

export interface HostKeyEvent {
  host: string;
  port: number;
  fingerprint: string;
  level: "info" | "danger";
  message: string;
  changedLine?: number | null;
}

export interface Notice {
  id: number;
  level: "info" | "danger";
  text: string;
  actionLabel?: string;
  onAction?: () => void;
}

export interface TunnelInfo {
  id: string;
  sessionId: string;
  name: string;
  kind: "local" | "socks5";
  bindHost: string;
  localPort: number;
  remoteHost: string;
  remotePort: number;
  activeConnections: number;
  bytesTransferred: number;
}

export interface CreateTunnelInput {
  sessionId: string;
  name: string;
  kind: "local" | "socks5";
  bindHost: string;
  localPort: number;
  remoteHost: string;
  remotePort: number;
}

export interface ExpectRule {
  id: string;
  name: string;
  expectPattern: string;
  sendText: string;
  enabled: boolean;
}

export interface ServerMetrics {
  supported: boolean;
  os: string;
  loadAvg: string;
  memUsedMb: number;
  memTotalMb: number;
  memPercent: number;
  diskPercent: number;
}
