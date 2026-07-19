import { useEffect, useRef } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { readText, writeText } from "@tauri-apps/plugin-clipboard-manager";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";
import type { SessionSpec } from "../types";
import { TERM_THEMES, type ThemeMode } from "../theme";

interface Props {
  spec: SessionSpec;
  active: boolean;
  theme: ThemeMode;
  onSession: (sessionId: string) => void;
  onError: (message: string) => void;
}

export default function TerminalView({ spec, active, theme, onSession, onError }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const started = useRef(false);
  const sessionRef = useRef<string | null>(null);
  const activeRef = useRef(active);
  activeRef.current = active;
  const termRef = useRef<Terminal | null>(null);

  useEffect(() => {
    if (started.current || !containerRef.current) return;
    started.current = true;

    const term = new Terminal({
      fontFamily:
        '"JetBrains Mono", "SF Mono", Menlo, ui-monospace, monospace',
      fontSize: 13,
      lineHeight: 1.25,
      cursorBlink: true,
      scrollback: 10_000,
      allowProposedApi: true,
      // Hold Option (⌥) and drag to select text even when the running app
      // (Claude Code, vim, htop…) has grabbed the mouse for its own use.
      macOptionClickForcesSelection: true,
      theme: TERM_THEMES[theme],
    });
    termRef.current = term;
    const fit = new FitAddon();
    term.loadAddon(fit);
    // The last OSC 8 hyperlink the mouse hovered. Claude Code (and any Ink/OSC 8
    // app) emits long URLs as OSC 8 hyperlinks that carry the *whole* URL in the
    // escape sequence, independent of how the visible text is hard-wrapped. We
    // capture that full URL on hover — hover fires even under mouse reporting
    // (no button pressed), unlike the click — so ⌘⇧O can open it reliably.
    let lastLinkUri: string | null = null;

    // OSC 8 hyperlinks: this is how iTerm follows a wrapped login URL. The
    // `text` handed to us is the complete URL from the escape, never the broken
    // visible text. ⌘/Ctrl-click opens it; hover remembers it for ⌘⇧O.
    term.options.linkHandler = {
      activate: (event, text) => {
        if (event.metaKey || event.ctrlKey) {
          console.debug("[sbt] osc8 activate ->", text);
          openUrl(text).catch((e) =>
            console.error("[sbt] openUrl (osc8) failed:", e),
          );
        }
      },
      hover: (_event, text) => {
        lastLinkUri = text;
      },
    };

    // Plain-text URLs (curl output, logs…): regex-matched and joined across
    // soft-wrapped rows. Note this canNOT recover a URL that a TUI hard-wrapped
    // into separate buffer lines — for those, the OSC 8 handler above / ⌘⇧O win.
    term.loadAddon(
      new WebLinksAddon((event, uri) => {
        if (event.metaKey || event.ctrlKey) {
          console.debug("[sbt] weblink click ->", uri);
          openUrl(uri).catch((e) =>
            console.error("[sbt] openUrl (weblink) failed:", e),
          );
        }
      }),
    );
    term.open(containerRef.current);
    fit.fit();

    // --- Copy / paste --------------------------------------------------------
    // ⌘C copies the current selection; ⌘V pastes. When a full-screen app
    // (Claude Code, vim…) has grabbed the mouse, hold ⌥ (Option) and drag to
    // select text, then ⌘C.
    //
    // Smart copy: xterm's getSelection() inserts a newline at every *visual*
    // row break, which mangles long wrapped lines like OAuth URLs. We instead
    // read the buffer and rejoin rows whose `isWrapped` flag says they're the
    // continuation of the line above — reconstructing the logical lines the way
    // Terminal.app does. Falls back to getSelection() if anything looks off.
    const smartSelectionText = (): string => {
      const pos = term.getSelectionPosition();
      if (!pos) return "";
      const buf = term.buffer.active;
      const startRow = pos.start.y;
      const endRow = pos.end.y;

      // Walk the selected rows, joining continuation rows (isWrapped) to the
      // line above instead of inserting a break at each visual wrap.
      const logical: string[] = [];
      let current = "";
      for (let row = startRow; row <= endRow; row++) {
        const line = buf.getLine(row);
        if (!line) continue;
        const from = row === startRow ? pos.start.x : 0;
        const to = row === endRow ? pos.end.x : line.length;
        const text = line.translateToString(true, from, to);

        if (row === startRow) {
          current = text;
        } else if (line.isWrapped) {
          // Continuation of the previous visual row: no break.
          current += text;
        } else {
          // Real new logical line.
          logical.push(current);
          current = text;
        }
      }
      logical.push(current);

      return logical.map((l) => l.replace(/\s+$/, "")).join("\n");
    };

    const copySelection = async () => {
      let text = "";
      if (term.hasSelection()) {
        // xterm 6's native getSelection() already rejoins soft-wrapped rows (it
        // honours isWrapped internally), so it's correct for long URLs on its
        // own. smartSelectionText() stays as a fallback if it comes back empty.
        text = term.getSelection();
        if (!text) {
          try {
            text = smartSelectionText();
          } catch {
            text = "";
          }
        }
      } else if (lastLinkUri) {
        // No text selected, but the mouse is (or was) over an OSC 8 link —
        // clicking a link only highlights it, it doesn't create a selection, so
        // ⌘C would otherwise copy nothing. Copy the link's full URL instead.
        text = lastLinkUri;
      }
      if (!text) return;
      try {
        await writeText(text);
        console.debug(`[sbt] copied ${text.length} chars`);
      } catch (e) {
        console.error("[sbt] clipboard writeText failed:", e);
      }
    };
    const pasteClipboard = async () => {
      const id = sessionRef.current;
      if (!id) return;
      try {
        const text = await readText();
        if (text) void invoke("write_to_session", { id, data: text });
      } catch (e) {
        console.error("[sbt] clipboard readText failed:", e);
      }
    };

    // --- Open a URL without the mouse -----------------------------------------
    // Reliable inside mouse-tracking TUIs (Claude Code, vim): find a URL in the
    // current selection, or failing that in the logical line under the cursor,
    // and open it in the browser. Bound to ⌘⇧O below.
    const URL_RE = /https?:\/\/[^\s]+/;
    const logicalLineAtCursor = (): string => {
      const buf = term.buffer.active;
      const cursorAbs = buf.baseY + buf.cursorY;
      // Walk up to the first row of this logical (possibly wrapped) line.
      let start = cursorAbs;
      while (start > 0 && buf.getLine(start)?.isWrapped) start--;
      // Collect this row plus any wrapped continuations below it.
      let text = "";
      for (let row = start; row < buf.length; row++) {
        const line = buf.getLine(row);
        if (!line) break;
        if (row > start && !line.isWrapped) break;
        text += line.translateToString(true);
      }
      return text;
    };
    const openUrlFromContext = async () => {
      // 1) Best source: the full URL from the OSC 8 link last under the mouse.
      // This is the whole URL even when Claude Code hard-wrapped its display.
      if (lastLinkUri) {
        console.debug("[sbt] ⌘⇧O open (osc8) ->", lastLinkUri);
        try {
          await openUrl(lastLinkUri);
        } catch (e) {
          console.error("[sbt] openUrl (⌘⇧O osc8) failed:", e);
        }
        return;
      }
      // 2) Fallback: regex over the selection / cursor line (plain-text URLs).
      let source = "";
      if (term.hasSelection()) {
        source = term.getSelection();
        if (!source) {
          try {
            source = smartSelectionText();
          } catch {
            source = "";
          }
        }
      }
      if (!source) source = logicalLineAtCursor();
      const match = source.match(URL_RE);
      if (!match) {
        console.warn("[sbt] ⌘⇧O: no URL found in selection or cursor line");
        return;
      }
      // Trim trailing punctuation that often clings to a URL in prose.
      const uri = match[0].replace(/[)\].,;:'"]+$/, "");
      console.debug("[sbt] ⌘⇧O open ->", uri);
      try {
        await openUrl(uri);
      } catch (e) {
        console.error("[sbt] openUrl (⌘⇧O) failed:", e);
      }
    };

    // ⌘⇧C: copy the full OSC 8 URL last under the mouse to the clipboard. Use
    // this when you want the whole (un-wrapped) URL on the clipboard rather than
    // opening it — hover the link first so it's captured, then ⌘⇧C.
    const copyLinkUri = async () => {
      if (!lastLinkUri) {
        console.warn(
          "[sbt] ⌘⇧C: no URL captured — hover the link first, then ⌘⇧C",
        );
        return;
      }
      try {
        await writeText(lastLinkUri);
        console.debug(`[sbt] ⌘⇧C copied URL (${lastLinkUri.length} chars)`);
      } catch (e) {
        console.error("[sbt] ⌘⇧C writeText failed:", e);
      }
    };

    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== "keydown") return true;
      // ⌘C / ⌘V are Mac copy/paste and must NOT reach the shell. Ctrl-C/Ctrl-V
      // are left alone so Ctrl-C still interrupts the running process.
      if (e.metaKey && !e.ctrlKey && !e.altKey) {
        if (e.key === "c" || e.key === "C") {
          if (e.shiftKey) void copyLinkUri();
          else void copySelection();
          return false;
        }
        if (e.key === "v" || e.key === "V") {
          void pasteClipboard();
          return false;
        }
        // ⌘⇧O: open a URL from the selection / cursor line, mouse-free.
        if (e.shiftKey && (e.key === "o" || e.key === "O")) {
          void openUrlFromContext();
          return false;
        }
      }
      return true;
    });

    // Backstop: intercept ⌘C / ⌘V at the window level in the capture phase, so
    // they're handled even when a full-screen app (Claude Code, vim) has the
    // keyboard focus and xterm's own handler never fires. Only the active tab's
    // terminal responds, and only when the focus is inside this terminal.
    const onWindowKey = (e: KeyboardEvent) => {
      if (!activeRef.current) return;
      if (!e.metaKey || e.ctrlKey || e.altKey) return;
      // Don't hijack ⌘C/⌘V while the user is typing in a real input field
      // (the SSH dialog, the snippet editor, the filter box…).
      const active = document.activeElement;
      const tag = active?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;
      if (e.key === "c" || e.key === "C") {
        if (e.shiftKey) {
          e.preventDefault();
          e.stopPropagation();
          void copyLinkUri();
        } else if (term.hasSelection() || lastLinkUri) {
          e.preventDefault();
          e.stopPropagation();
          void copySelection();
        }
      } else if (e.key === "v" || e.key === "V") {
        e.preventDefault();
        e.stopPropagation();
        void pasteClipboard();
      } else if (e.shiftKey && (e.key === "o" || e.key === "O")) {
        e.preventDefault();
        e.stopPropagation();
        void openUrlFromContext();
      }
    };
    window.addEventListener("keydown", onWindowKey, true);

    // Right-click: copy the selection if any, otherwise paste.
    const onContextMenu = (e: MouseEvent) => {
      e.preventDefault();
      if (term.hasSelection()) void copySelection();
      else void pasteClipboard();
    };
    containerRef.current.addEventListener("contextmenu", onContextMenu);
    // Middle-click paste (X11-style), handy for many terminal users.
    const onAuxClick = (e: MouseEvent) => {
      if (e.button === 1) {
        e.preventDefault();
        void pasteClipboard();
      }
    };
    containerRef.current.addEventListener("auxclick", onAuxClick);

    const onData = new Channel<number[]>();
    onData.onmessage = (bytes) => term.write(new Uint8Array(bytes));

    const start = async () => {
      try {
        const args = { cols: term.cols, rows: term.rows, onData };
        const id: string =
          spec.kind === "local"
            ? await invoke("create_local_session", args)
            : spec.kind === "ssh"
              ? await invoke("create_ssh_session", { ...args, config: spec.config })
              : await invoke("connect_saved_session", { ...args, id: spec.savedId });
        sessionRef.current = id;
        onSession(id);

        term.onData((data) => {
          void invoke("write_to_session", { id, data });
        });
      } catch (e) {
        term.writeln(`\r\n\x1b[31m✕ ${String(e)}\x1b[0m`);
        onError(String(e));
      }
    };
    void start();

    let resizeTimer: number | undefined;
    const observer = new ResizeObserver(() => {
      window.clearTimeout(resizeTimer);
      resizeTimer = window.setTimeout(() => {
        if (!activeRef.current) return;
        fit.fit();
        const id = sessionRef.current;
        if (id) {
          void invoke("resize_session", { id, cols: term.cols, rows: term.rows });
        }
      }, 80);
    });
    observer.observe(containerRef.current);

    const hostEl = containerRef.current;
    return () => {
      observer.disconnect();
      window.removeEventListener("keydown", onWindowKey, true);
      hostEl?.removeEventListener("contextmenu", onContextMenu);
      hostEl?.removeEventListener("auxclick", onAuxClick);
      const id = sessionRef.current;
      if (id) void invoke("close_session", { id });
      term.dispose();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Live theme switching.
  useEffect(() => {
    if (termRef.current) termRef.current.options.theme = TERM_THEMES[theme];
  }, [theme]);

  // Refit whenever this tab becomes visible again.
  useEffect(() => {
    if (active && containerRef.current) {
      const el = containerRef.current.querySelector(".xterm");
      if (el) window.dispatchEvent(new Event("resize"));
    }
  }, [active]);

  return (
    <div
      ref={containerRef}
      className="terminal-host"
      style={{ display: active ? "block" : "none" }}
    />
  );
}
