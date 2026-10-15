/**
 * SessionView — the content area for one open session (tasks 21.2 and 21.3).
 *
 * Routes by protocol: text-based protocols (SSH, Telnet, Serial) render in the
 * {@link TerminalView} (xterm.js); RDP renders in the {@link RdpView} (canvas).
 * The parent `App` keeps every open session mounted and toggles visibility, so
 * a background session's terminal keeps receiving and buffering its own output
 * independently (Requirement 9.2) rather than being torn down on each tab
 * switch.
 *
 * ## Session action bar (task 21.3)
 *
 * The top strip hosts the per-session action triggers task 21.3 adds:
 *
 * - **Logging toggle** ({@link LoggingToggle}) — enable/disable per-session
 *   logging to a file (Requirements 14.2, 14.3). Available for every session.
 * - **File transfer** ({@link FileTransferPanel}) — SFTP/SCP listing, transfer,
 *   progress, cancel, overwrite confirmation (Requirements 13.1, 13.4, 13.7).
 *   Offered for SSH sessions (the protocol that carries file transfer).
 * - **Script runner** ({@link ScriptRunnerPanel}) — compose and run a command
 *   script, with completion status (Requirements 15.5, 15.6). Offered for the
 *   text-based protocols (SSH / Telnet / Serial); RDP has no command stream.
 *
 * Each panel opens inline below the action bar; only one is shown at a time.
 *
 * Branding is task 22; neutral token-driven styles only.
 */

import { useState } from "react";
import TerminalView from "./TerminalView";
import RdpView from "./RdpView";
import LoggingToggle from "./LoggingToggle";
import FileTransferPanel from "./FileTransferPanel";
import ScriptRunnerPanel from "./ScriptRunnerPanel";
import type { OpenSession } from "./TabBar";

interface SessionViewProps {
  session: OpenSession;
  /** Hidden sessions stay mounted (Req 9.2) but are visually inert. */
  hidden: boolean;
  onError?: (message: string) => void;
}

/** Which inline panel (if any) is open in this session's action area. */
type ActivePanel = "none" | "transfer" | "script";

function SessionView({ session, hidden, onError }: SessionViewProps) {
  const isRdp = session.kind === "rdp";
  // File transfer rides SSH; the script runner drives any text-based session.
  const supportsTransfer = session.kind === "ssh";
  const supportsScript = !isRdp;

  const [panel, setPanel] = useState<ActivePanel>("none");

  /** Toggle a panel open/closed (clicking an open panel's trigger closes it). */
  function toggle(next: Exclude<ActivePanel, "none">) {
    setPanel((current) => (current === next ? "none" : next));
  }

  return (
    <div
      className="session-view"
      role="tabpanel"
      hidden={hidden}
      aria-label={session.title}
    >
      <div className="session-actionbar" aria-label="Session actions">
        <span className="session-actionbar-title">{session.title}</span>

        <div className="session-actionbar-tools">
          <LoggingToggle sessionId={session.id} onError={onError} />

          {supportsTransfer ? (
            <button
              type="button"
              className={
                panel === "transfer" ? "btn btn-small btn-active" : "btn btn-small"
              }
              aria-pressed={panel === "transfer"}
              onClick={() => toggle("transfer")}
            >
              Files
            </button>
          ) : null}

          {supportsScript ? (
            <button
              type="button"
              className={
                panel === "script" ? "btn btn-small btn-active" : "btn btn-small"
              }
              aria-pressed={panel === "script"}
              onClick={() => toggle("script")}
            >
              Script
            </button>
          ) : null}
        </div>
      </div>

      {panel === "transfer" && supportsTransfer ? (
        <FileTransferPanel
          sessionId={session.id}
          onClose={() => setPanel("none")}
        />
      ) : null}

      {panel === "script" && supportsScript ? (
        <ScriptRunnerPanel
          sessionId={session.id}
          onClose={() => setPanel("none")}
        />
      ) : null}

      <div className="session-body">
        {isRdp ? (
          <RdpView
            sessionId={session.id}
            state={session.state}
            onError={onError}
          />
        ) : (
          <TerminalView sessionId={session.id} onError={onError} />
        )}
      </div>
    </div>
  );
}

export default SessionView;
