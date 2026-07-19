export interface SshConfig {
  host: string;
  port: number;
  username: string;
  password?: string;
  keyPath?: string;
  keyPassphrase?: string;
}

export type Protocol = "ssh" | "rdp";

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

export interface SavedSession {
  id: string;
  groupId: string;
  name: string;
  host: string;
  port: number;
  username: string;
  authMethod: "password" | "key";
  keyPath?: string | null;
  protocol: Protocol;
  domain?: string | null;
  rdpResolution?: string | null;
}

export interface GroupNode {
  id: string;
  name: string;
  parentId?: string | null;
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
  | { kind: "saved"; savedId: string };

export interface Tab {
  tabId: string;
  title: string;
  spec: SessionSpec;
  sessionId?: string;
  status: "connecting" | "live" | "dead";
  statusDetail?: string;
}

/** What the session dialog hands back to App when saving. */
export interface SessionDraft {
  id?: string;
  groupId: string;
  name: string;
  host: string;
  port: number;
  username: string;
  authMethod: "password" | "key";
  keyPath?: string;
  protocol: Protocol;
  domain?: string;
  rdpResolution?: string;
  password?: string;
  keyPassphrase?: string;
}

export interface RdpAvailability {
  available: boolean;
  binary?: string | null;
}

export interface Notice {
  id: number;
  level: "info" | "danger";
  text: string;
}
