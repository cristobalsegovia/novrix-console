import { useEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { readText, writeText } from "@tauri-apps/plugin-clipboard-manager";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { SearchAddon } from "@xterm/addon-search";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { ChevronDown, ChevronUp, RotateCcw, Search, X } from "lucide-react";
import "@xterm/xterm/css/xterm.css";
import type { ExpectRule, SessionSpec } from "../types";
import { TERM_THEMES, type ThemeMode } from "../theme";

interface Props {
  spec: SessionSpec;
  active: boolean;
  focusedPane: boolean;
  theme: ThemeMode;
  status: "connecting" | "live" | "dead";
  statusDetail?: string;
  highlightEnabled: boolean;
  expectRules: ExpectRule[];
  searchTrigger: number;
  reconnectEpoch: number;
  onFocusPane: () => void;
  onSession: (sessionId: string) => void;
  onError: (message: string) => void;
  onReconnect: () => void;
}

/**
 * Apply lightweight ANSI color highlighting to plain text output chunks when the
 * terminal is in normal line-scroll mode (not inside an alternate-screen TUI
 * like vim, htop, or Claude Code, and not inside an escape sequence).
 */
function applyKeywordHighlight(bytes: number[]): Uint8Array {
  // If chunk contains ESC (0x1b), pass through untouched so we never corrupt
  // escape sequences, OSC 8 hyperlinks, or TUI cursor positioning.
  if (bytes.includes(0x1b)) {
    return new Uint8Array(bytes);
  }
  const text = new TextDecoder().decode(new Uint8Array(bytes));
  const highlighted = text
    // Critical / Error / Down keywords -> bright red
    .replace(
      /\b(ERROR|CRITICAL|FATAL|FAIL|FAILED|DOWN|err-disabled|notconnect|denied|dropped|CRC|collision|unreachable)\b/gi,
      "\x1b[1;31m$1\x1b[0m",
    )
    // Warning / Flapping keywords -> amber/yellow
    .replace(
      /\b(WARN|WARNING|TIMEOUT|flapping|administratively down|half-duplex|mismatch)\b/gi,
      "\x1b[1;33m$1\x1b[0m",
    )
    // Up / Healthy keywords -> green
    .replace(
      /\b(CONNECTED|ESTABLISHED|FULL|up\/up|forwarding)\b/gi,
      "\x1b[1;32m$1\x1b[0m",
    )
    // Cisco / Network interfaces (Gi0/1, Te1/0/1, Fa0/1, Eth1/1, Po1, Vl100) -> cyan
    .replace(
      /\b((?:Gi|Te|Fa|Eth|Po|Vl|Lo|Tu|Mgmt|GigabitEthernet|TenGigabitEthernet|FastEthernet|Vlan|Port-channel)\d+(?:\/\d+)*(?:\.\d+)?)\b/g,
      "\x1b[36m$1\x1b[0m",
    )
    // IPv4 addresses (with optional /CIDR) -> bright blue
    .replace(
      /\b((?:25[0-5]|2[0-4]\d|1?\d?\d)(?:\.(?:25[0-5]|2[0-4]\d|1?\d?\d)){3}(?:\/\d{1,2})?)\b/g,
      "\x1b[94m$1\x1b[0m",
    );
  return new TextEncoder().encode(highlighted);
}

export default function TerminalView({
  spec,
  active,
  focusedPane,
  theme,
  status,
  statusDetail,
  highlightEnabled,
  expectRules,
  searchTrigger,
  reconnectEpoch,
  onFocusPane,
  onSession,
  onError,
  onReconnect,
}: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const sessionRef = useRef<string | null>(null);
  const isTabActiveRef = useRef(active);
  isTabActiveRef.current = active;
  const activeRef = useRef(active && focusedPane);
  activeRef.current = active && focusedPane;
  const statusRef = useRef(status);
  statusRef.current = status;
  const highlightRef = useRef(highlightEnabled);
  highlightRef.current = highlightEnabled;
  const expectRulesRef = useRef(expectRules);
  expectRulesRef.current = expectRules;
  const reconnectRef = useRef(onReconnect);
  reconnectRef.current = onReconnect;

  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const searchAddonRef = useRef<SearchAddon | null>(null);
  const searchInputRef = useRef<HTMLInputElement>(null);

  const [searchOpen, setSearchOpen] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const [caseSensitive, setCaseSensitive] = useState(false);
  const [useRegex, setUseRegex] = useState(false);

  // Open search bar when triggered via ⌘F or Command Palette
  useEffect(() => {
    if (searchTrigger > 0 && active && focusedPane) {
      setSearchOpen(true);
      window.setTimeout(() => searchInputRef.current?.focus(), 20);
    }
  }, [searchTrigger, active, focusedPane]);

  useEffect(() => {
    if (!containerRef.current) return;
    containerRef.current.innerHTML = "";

    const term = new Terminal({
      fontFamily:
        '"JetBrains Mono", "SF Mono", Menlo, ui-monospace, monospace',
      fontSize: 13,
      lineHeight: 1.25,
      cursorBlink: true,
      scrollback: 10_000,
      allowProposedApi: true,
      macOptionClickForcesSelection: true,
      theme: TERM_THEMES[theme],
    });
    termRef.current = term;
    const fit = new FitAddon();
    fitRef.current = fit;
    const search = new SearchAddon();
    searchAddonRef.current = search;
    term.loadAddon(fit);
    term.loadAddon(search);

    // Support OSC 52 clipboard writes so `/copy` in Claude Code (both locally
    // and over remote SSH sessions) copies directly to the macOS clipboard.
    const osc52Disposable = term.parser.registerOscHandler(52, (data) => {
      const semi = data.indexOf(";");
      if (semi === -1) return false;
      const b64 = data.slice(semi + 1).trim();
      if (!b64 || b64 === "?") return true;
      try {
        const binary = window.atob(b64.replace(/\s+/g, ""));
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) {
          bytes[i] = binary.charCodeAt(i);
        }
        const decoded = new TextDecoder("utf-8").decode(bytes);
        if (decoded) {
          void writeText(decoded);
        }
      } catch (e) {
        console.error("[sbt] OSC 52 decode failed:", e);
      }
      return true;
    });

    let lastLinkUri: string | null = null;

    term.options.linkHandler = {
      activate: (event, text) => {
        if (event.metaKey || event.ctrlKey) {
          openUrl(text).catch((e) =>
            console.error("[sbt] openUrl (osc8) failed:", e),
          );
        }
      },
      hover: (_event, text) => {
        lastLinkUri = text;
      },
    };

    term.loadAddon(
      new WebLinksAddon((event, uri) => {
        if (event.metaKey || event.ctrlKey) {
          openUrl(uri).catch((e) =>
            console.error("[sbt] openUrl (weblink) failed:", e),
          );
        }
      }),
    );
    term.open(containerRef.current);
    fit.fit();

    const smartSelectionText = (): string => {
      const pos = term.getSelectionPosition();
      if (!pos) return "";
      const buf = term.buffer.active;
      const startRow = pos.start.y;
      const endRow = pos.end.y;

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
          current += text;
        } else {
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
        text = term.getSelection();
        if (!text) {
          try {
            text = smartSelectionText();
          } catch {
            text = "";
          }
        }
      } else if (lastLinkUri) {
        text = lastLinkUri;
      }
      if (!text) return;
      try {
        await writeText(text);
      } catch (e) {
        console.error("[sbt] clipboard writeText failed:", e);
      }
    };

    const pasteClipboard = async () => {
      if (!sessionRef.current) return;
      try {
        const text = await readText();
        // Use term.paste(text) so Bracketed Paste Mode (?2004h -> \x1b[200~...\x1b[201~)
        // is respected by Claude Code, vim, and zsh instead of executing line-by-line.
        if (text) term.paste(text);
      } catch (e) {
        console.error("[sbt] clipboard readText failed:", e);
      }
    };

    const URL_RE = /https?:\/\/[^\s]+/;
    const logicalLineAtCursor = (): string => {
      const buf = term.buffer.active;
      const cursorAbs = buf.baseY + buf.cursorY;
      let start = cursorAbs;
      while (start > 0 && buf.getLine(start)?.isWrapped) start--;
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
      if (lastLinkUri) {
        try {
          await openUrl(lastLinkUri);
        } catch (e) {
          console.error("[sbt] openUrl (⌘⇧O osc8) failed:", e);
        }
        return;
      }
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
      if (!match) return;
      const uri = match[0].replace(/[)\].,;:'"]+$/, "");
      try {
        await openUrl(uri);
      } catch (e) {
        console.error("[sbt] openUrl (⌘⇧O) failed:", e);
      }
    };

    const copyLinkUri = async () => {
      if (!lastLinkUri) return;
      try {
        await writeText(lastLinkUri);
      } catch (e) {
        console.error("[sbt] ⌘⇧C writeText failed:", e);
      }
    };

    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== "keydown") return true;
      // When disconnected, pressing Enter reconnects immediately!
      if (statusRef.current === "dead" && e.key === "Enter") {
        reconnectRef.current();
        return false;
      }

      const id = sessionRef.current;

      // Claude Code & Zsh multi-line prompt newline: Shift+Enter or Option+Enter
      // sends ESC+CR (\x1b\r) which Claude Code & ZLE treat as insert-newline
      // without submitting the prompt.
      if (
        e.key === "Enter" &&
        !e.metaKey &&
        !e.ctrlKey &&
        (e.shiftKey || e.altKey)
      ) {
        if (id) void invoke("write_to_session", { id, data: "\x1b\r" });
        return false;
      }

      // macOS Option+Arrow / Option+Backspace word navigation in Claude Code & Zsh
      if (e.altKey && !e.metaKey && !e.ctrlKey && !e.shiftKey) {
        if (e.key === "ArrowLeft") {
          if (id) void invoke("write_to_session", { id, data: "\x1bb" });
          return false;
        }
        if (e.key === "ArrowRight") {
          if (id) void invoke("write_to_session", { id, data: "\x1bf" });
          return false;
        }
        if (e.key === "Backspace") {
          if (id) void invoke("write_to_session", { id, data: "\x1b\x7f" });
          return false;
        }
      }

      if (e.metaKey && !e.ctrlKey && !e.altKey) {
        // Cmd+Left / Cmd+Right / Cmd+Backspace line navigation & kill-line
        if (!e.shiftKey && e.key === "ArrowLeft") {
          if (id) void invoke("write_to_session", { id, data: "\x01" });
          return false;
        }
        if (!e.shiftKey && e.key === "ArrowRight") {
          if (id) void invoke("write_to_session", { id, data: "\x05" });
          return false;
        }
        if (!e.shiftKey && e.key === "Backspace") {
          if (id) void invoke("write_to_session", { id, data: "\x15" });
          return false;
        }
        if (e.key === "c" || e.key === "C") {
          if (e.shiftKey) void copyLinkUri();
          else void copySelection();
          return false;
        }
        if (e.key === "v" || e.key === "V") {
          void pasteClipboard();
          return false;
        }
        if (e.key === "f" || e.key === "F") {
          setSearchOpen(true);
          window.setTimeout(() => searchInputRef.current?.focus(), 20);
          return false;
        }
        if (e.shiftKey && (e.key === "o" || e.key === "O")) {
          void openUrlFromContext();
          return false;
        }
      }
      return true;
    });

    const onWindowKey = (e: KeyboardEvent) => {
      if (!activeRef.current) return;
      if (!e.metaKey || e.ctrlKey || e.altKey) return;
      const activeEl = document.activeElement;
      const tag = activeEl?.tagName;
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
      } else if (e.key === "f" || e.key === "F") {
        e.preventDefault();
        e.stopPropagation();
        setSearchOpen(true);
        window.setTimeout(() => searchInputRef.current?.focus(), 20);
      } else if (e.shiftKey && (e.key === "o" || e.key === "O")) {
        e.preventDefault();
        e.stopPropagation();
        void openUrlFromContext();
      }
    };
    window.addEventListener("keydown", onWindowKey, true);

    const onContextMenu = (e: MouseEvent) => {
      e.preventDefault();
      if (term.hasSelection()) void copySelection();
      else void pasteClipboard();
    };
    containerRef.current.addEventListener("contextmenu", onContextMenu);
    const onAuxClick = (e: MouseEvent) => {
      if (e.button === 1) {
        e.preventDefault();
        void pasteClipboard();
      }
    };
    containerRef.current.addEventListener("auxclick", onAuxClick);

    let rollingBuffer = "";
    const lastFired = new Map<string, number>();

    const onData = new Channel<number[]>();
    onData.onmessage = (bytes) => {
      if (
        highlightRef.current &&
        term.buffer.active.type === "normal" &&
        spec.kind !== "local"
      ) {
        term.write(applyKeywordHighlight(bytes));
      } else {
        term.write(new Uint8Array(bytes));
      }

      // Evaluate Expect / Auto-Responder triggers on remote sessions
      if (spec.kind !== "local" && sessionRef.current) {
        const activeRules = expectRulesRef.current.filter((r) => r.enabled);
        if (activeRules.length > 0) {
          const chunk = new TextDecoder().decode(new Uint8Array(bytes));
          rollingBuffer = (rollingBuffer + chunk).slice(-320);
          const now = Date.now();
          for (const rule of activeRules) {
            if (
              rule.expectPattern &&
              rollingBuffer.includes(rule.expectPattern)
            ) {
              const prev = lastFired.get(rule.id) ?? 0;
              if (now - prev > 800) {
                lastFired.set(rule.id, now);
                rollingBuffer = "";
                const payload = rule.sendText
                  .replace(/\\r/g, "\r")
                  .replace(/\\n/g, "\n");
                void invoke("write_to_session", {
                  id: sessionRef.current,
                  data: payload,
                });
                break;
              }
            }
          }
        }
      }
    };

    const start = async () => {
      try {
        const args = { cols: term.cols, rows: term.rows, onData };
        const id: string =
          spec.kind === "local"
            ? await invoke("create_local_session", args)
            : spec.kind === "ssh"
              ? await invoke("create_ssh_session", { ...args, config: spec.config })
              : spec.kind === "serial"
                ? await invoke("create_serial_session", {
                    config: spec.config,
                    onData,
                  })
                : await invoke("connect_saved_session", {
                    ...args,
                    id: spec.savedId,
                  });
        sessionRef.current = id;
        onSession(id);

        if (spec.kind === "local" && spec.command) {
          const initialCmd = spec.command;
          window.setTimeout(() => {
            void invoke("write_to_session", { id, data: `${initialCmd}\r` });
          }, 150);
        }

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
        if (!isTabActiveRef.current) return;
        try {
          fit.fit();
          const id = sessionRef.current;
          if (id) {
            void invoke("resize_session", {
              id,
              cols: term.cols,
              rows: term.rows,
            });
          }
        } catch {
          /* ignore transient fit errors */
        }
      }, 16);
    });
    observer.observe(containerRef.current);

    const hostEl = containerRef.current;
    return () => {
      osc52Disposable.dispose();
      observer.disconnect();
      window.removeEventListener("keydown", onWindowKey, true);
      hostEl?.removeEventListener("contextmenu", onContextMenu);
      hostEl?.removeEventListener("auxclick", onAuxClick);
      const id = sessionRef.current;
      if (id) void invoke("close_session", { id });
      term.dispose();
    };
    // Re-run effect when reconnectEpoch increments to reconnect in place!
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reconnectEpoch]);

  // Live theme switching.
  useEffect(() => {
    if (termRef.current) termRef.current.options.theme = TERM_THEMES[theme];
  }, [theme]);

  // Refit whenever this tab becomes visible again.
  useEffect(() => {
    if (active && fitRef.current && termRef.current) {
      window.requestAnimationFrame(() => {
        try {
          fitRef.current?.fit();
          const id = sessionRef.current;
          const term = termRef.current;
          if (id && term) {
            void invoke("resize_session", {
              id,
              cols: term.cols,
              rows: term.rows,
            });
          }
        } catch {
          /* ignore */
        }
      });
    }
  }, [active]);

  const runSearch = (direction: "next" | "prev") => {
    if (!searchAddonRef.current || !searchQuery) return;
    const opts = { caseSensitive, regex: useRegex };
    if (direction === "next") {
      searchAddonRef.current.findNext(searchQuery, opts);
    } else {
      searchAddonRef.current.findPrevious(searchQuery, opts);
    }
  };

  return (
    <div
      className={`pane-wrap ${focusedPane ? "focused" : ""}`}
      onMouseDown={onFocusPane}
    >
      {searchOpen && (
        <div className="term-search-bar" onMouseDown={(e) => e.stopPropagation()}>
          <Search size={13} className="term-search-icon" />
          <input
            ref={searchInputRef}
            value={searchQuery}
            onChange={(e) => {
              setSearchQuery(e.target.value);
              if (e.target.value) {
                searchAddonRef.current?.findNext(e.target.value, {
                  caseSensitive,
                  regex: useRegex,
                });
              }
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                runSearch(e.shiftKey ? "prev" : "next");
              } else if (e.key === "Escape") {
                e.preventDefault();
                setSearchOpen(false);
                termRef.current?.focus();
              }
            }}
            placeholder="Buscar en consola…"
          />
          <button
            type="button"
            className={`term-search-opt ${caseSensitive ? "on" : ""}`}
            title="Coincidir mayúsculas/minúsculas"
            onClick={() => setCaseSensitive((v) => !v)}
          >
            Aa
          </button>
          <button
            type="button"
            className={`term-search-opt ${useRegex ? "on" : ""}`}
            title="Expresión regular"
            onClick={() => setUseRegex((v) => !v)}
          >
            .*
          </button>
          <button
            type="button"
            className="btn-icon"
            title="Coincidencia anterior (⇧Enter)"
            onClick={() => runSearch("prev")}
          >
            <ChevronUp size={14} />
          </button>
          <button
            type="button"
            className="btn-icon"
            title="Siguiente coincidencia (Enter)"
            onClick={() => runSearch("next")}
          >
            <ChevronDown size={14} />
          </button>
          <button
            type="button"
            className="btn-icon"
            title="Cerrar búsqueda (Esc)"
            onClick={() => {
              setSearchOpen(false);
              termRef.current?.focus();
            }}
          >
            <X size={14} />
          </button>
        </div>
      )}

      <div ref={containerRef} className="terminal-host" />

      {status === "dead" && (
        <div className="reconnect-bar" onMouseDown={(e) => e.stopPropagation()}>
          <span className="reconnect-msg">
            Sesión finalizada{statusDetail ? ` (${statusDetail})` : ""}
          </span>
          <button
            type="button"
            className="btn accent sm"
            onClick={onReconnect}
          >
            <RotateCcw size={12} /> Reconectar (Enter)
          </button>
        </div>
      )}
    </div>
  );
}
