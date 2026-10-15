/**
 * TabBar — the open-session tab strip (task 21.2).
 *
 * Shows every open session as a tab and tracks which one is active
 * (Requirements 9.1, 9.5). It is presentational: the parent `App` owns the open
 * sessions list and the active id, and this component reports intent through
 * callbacks:
 *
 * - Click a tab → `onActivate(id)` (parent calls `activate_tab`).
 * - Click a tab's close affordance → `onClose(id)` (parent decides whether a
 *   confirmation is needed for an active/connected session, per Requirement
 *   9.7, then calls `close_tab`).
 *
 * Branding is task 22; neutral token-driven styles only.
 */

import type { ProtocolKind, SessionId, SessionState } from "../ipc";

/** One open session as the tab strip needs to see it. */
export interface OpenSession {
  id: SessionId;
  title: string;
  kind: ProtocolKind;
  state: SessionState;
}

interface TabBarProps {
  sessions: OpenSession[];
  activeId: SessionId | null;
  onActivate: (id: SessionId) => void;
  onClose: (id: SessionId) => void;
}

/** A tiny state dot so the user can tell connecting / connected / dropped apart. */
function stateLabel(state: SessionState): string {
  switch (state) {
    case "connecting":
      return "Connecting";
    case "connected":
      return "Connected";
    case "disconnected":
      return "Disconnected";
  }
}

function TabBar({ sessions, activeId, onActivate, onClose }: TabBarProps) {
  if (sessions.length === 0) return null;

  return (
    <div className="tabbar" role="tablist" aria-label="Open sessions">
      {sessions.map((session) => {
        const active = session.id === activeId;
        return (
          <div
            key={session.id}
            className={"tab" + (active ? " active" : "")}
            role="tab"
            aria-selected={active}
          >
            <button
              type="button"
              className="tab-label"
              onClick={() => onActivate(session.id)}
              title={`${session.title} · ${session.kind} · ${stateLabel(
                session.state,
              )}`}
            >
              <span
                className={"tab-state tab-state-" + session.state}
                aria-hidden="true"
              />
              <span className="tab-title">{session.title}</span>
            </button>
            <button
              type="button"
              className="tab-close"
              aria-label={`Close ${session.title}`}
              title="Close tab"
              onClick={() => onClose(session.id)}
            >
              ✕
            </button>
          </div>
        );
      })}
    </div>
  );
}

export default TabBar;
