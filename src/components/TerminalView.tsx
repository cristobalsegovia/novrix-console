/**
 * TerminalView — xterm.js front end for a text-based session (task 21.2).
 *
 * Hosts an `@xterm/xterm` Terminal for one session and bridges it to the Rust
 * core over IPC:
 *
 * - Output (Requirement 10.1): subscribes to `session://{id}/output` and writes
 *   every received chunk into the terminal. xterm.js interprets the ANSI escape
 *   sequences (cursor positioning, colors, bold/underline) and discards
 *   malformed ones (Requirements 10.3, 10.4) as part of its own VT parser, so
 *   this view just forwards bytes.
 * - Input (Requirement 10.2): user keystrokes (`onData`) are sent to the remote
 *   via `send_input`.
 * - Resize (Requirement 10.5): a FitAddon re-fits the terminal to its container
 *   on viewport/container changes, computing the new cols/rows. See the resize
 *   note below for the integration point.
 *
 * Branding (full Novrix palette, Inter) is task 22; the terminal theme here is
 * neutral and reads the CSS tokens so task 22 can reskin without touching this
 * component.
 *
 * ## Resize integration point (Requirement 10.5)
 *
 * The backend command surface (`src-tauri/src/ipc/mod.rs`) exposes `send_input`
 * for data but has **no dedicated resize command** today: the remote-side PTY
 * resize is owned by the core Terminal_Emulator and its transport seam, which is
 * part of the unfinished SSH wiring. So this view does the half it can do
 * locally — it re-fits with the FitAddon so the grid matches the viewport — and
 * records the computed `{ cols, rows }` for the moment a `resize_session`
 * command lands. When that command exists, wire it where `notifyResize` is
 * called (marked with TODO) and the UI side needs no other change.
 */

import { useEffect, useRef } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import {
  asDomainError,
  listenSessionOutput,
  sendInput,
  sessionOutput,
  type SessionId,
} from "../ipc";

interface TerminalViewProps {
  /** The session whose output stream this terminal renders. */
  sessionId: SessionId;
  /** Reports a non-fatal error (e.g. a failed `send_input`) to the host. */
  onError?: (message: string) => void;
}

/** Read a CSS custom property off :root, falling back to a neutral default. */
function cssVar(name: string, fallback: string): string {
  if (typeof window === "undefined") return fallback;
  const value = getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
  return value || fallback;
}

function TerminalView({ sessionId, onError }: TerminalViewProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  // Keep the latest onError in a ref so the mount effect need not depend on it
  // (a changing callback must not tear down and rebuild the terminal).
  const onErrorRef = useRef(onError);
  onErrorRef.current = onError;

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    // Neutral theme driven by the shared CSS tokens so task 22 reskins for free.
    const term = new Terminal({
      // Requirement 10.6: retain at least 1000 lines of scrollback; xterm.js
      // discards the oldest lines first once the limit is exceeded.
      scrollback: 2000,
      cursorBlink: true,
      fontFamily: cssVar("--font-ui", "monospace"),
      fontSize: 13,
      theme: {
        background: cssVar("--surface-0", "#0f172a"),
        foreground: cssVar("--text", "#e8eef7"),
        cursor: cssVar("--accent", "#3b82f6"),
      },
    });

    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(container);

    // Fit once mounted, then whenever the container resizes.
    const notifyResize = () => {
      try {
        fit.fit();
      } catch {
        // fit() can throw if the element is detached mid-teardown; ignore.
        return;
      }
      // Local fit done. Dimensions available as term.cols / term.rows.
      // TODO(resize wiring): when a `resize_session` IPC command exists, call
      // it here with { id: sessionId, cols: term.cols, rows: term.rows } to
      // satisfy Requirement 10.5's "notify the remote host" within 500 ms.
    };
    notifyResize();

    // Observe the container so a layout change (tab switch, window resize,
    // sidebar toggle) re-fits the grid.
    const resizeObserver = new ResizeObserver(() => notifyResize());
    resizeObserver.observe(container);
    window.addEventListener("resize", notifyResize);

    // Requirement 10.2: forward keystrokes to the remote as UTF-8 bytes.
    const dataDisposable = term.onData((data) => {
      const bytes = Array.from(new TextEncoder().encode(data));
      sendInput(sessionId, bytes).catch((err) => {
        onErrorRef.current?.(asDomainError(err).message);
      });
    });

    // Requirement 10.1: write every remote-output chunk into the terminal.
    //
    // Mount races the backend: the read loop may have already emitted the login
    // banner (and early prompts) onto `session://{id}/output` before this
    // listener is registered, so a naive subscribe-only view shows a black
    // screen. We fix the race with a subscribe-first, then-replay handshake:
    //
    //   1. Register the live listener immediately. Until the snapshot has been
    //      written, live chunks are *queued* instead of written, so ordering is
    //      preserved and nothing is dropped.
    //   2. Fetch `sessionOutput` — the full buffer the read loop has
    //      accumulated so far — and write it. Any live chunk already in that
    //      snapshot would otherwise be written twice.
    //   3. Flush the queue, skipping exactly the prefix the snapshot already
    //      covered (tracked by byte length), so no byte is duplicated or lost.
    let disposed = false;
    let unlisten: (() => void) | null = null;
    let replayed = false;
    // Live chunks seen before the snapshot replay completes, buffered in order.
    const pending: number[][] = [];
    // Bytes already shown via the snapshot; the queue flush skips this prefix.
    let snapshotLen = 0;

    const writeChunk = (data: number[]) => {
      // xterm accepts a Uint8Array and runs it through its VT parser, which
      // handles ANSI sequences and discards malformed ones (Req 10.3/10.4).
      term.write(new Uint8Array(data));
    };

    listenSessionOutput(sessionId, (data) => {
      if (replayed) {
        writeChunk(data);
      } else {
        // Not replayed yet: queue so these land after the snapshot, in order.
        pending.push(data);
      }
    })
      .then((fn) => {
        if (disposed) {
          // Unmounted before the listener resolved: tear it down immediately.
          fn();
          return;
        }
        unlisten = fn;
        // Snapshot replay (step 2). The listener is already live, so any chunk
        // arriving during this await is captured in `pending`.
        return sessionOutput(sessionId).then((snapshot) => {
          if (disposed) return;
          snapshotLen = snapshot.length;
          if (snapshotLen > 0) writeChunk(snapshot);

          // Step 3: flush queued live chunks, skipping the `snapshotLen`-byte
          // prefix the snapshot already rendered. The queue and the snapshot
          // share the same leading bytes (same read loop, same order), so we
          // drop whole/partial chunks until the prefix is consumed, then write
          // the remainder.
          let skip = snapshotLen;
          for (const chunk of pending) {
            if (skip >= chunk.length) {
              skip -= chunk.length;
              continue;
            }
            writeChunk(skip > 0 ? chunk.slice(skip) : chunk);
            skip = 0;
          }
          pending.length = 0;
          replayed = true;
        });
      })
      .catch((err) => {
        onErrorRef.current?.(asDomainError(err).message);
      });

    term.focus();

    return () => {
      disposed = true;
      unlisten?.();
      resizeObserver.disconnect();
      window.removeEventListener("resize", notifyResize);
      dataDisposable.dispose();
      term.dispose();
    };
  }, [sessionId]);

  return <div className="terminal-view" ref={containerRef} />;
}

export default TerminalView;
