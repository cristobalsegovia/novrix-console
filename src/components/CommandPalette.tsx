import { useEffect, useMemo, useState } from "react";
import { Search, Terminal } from "lucide-react";
import type { GroupNode, SavedSession, Snippet } from "../types";

interface Props {
  tree: GroupNode[];
  snippets: Snippet[];
  highlightEnabled: boolean;
  onClose: () => void;
  onConnectSession: (session: SavedSession) => void;
  onRunSnippet: (command: string) => void;
  onNewLocal: () => void;
  onNewConnection: () => void;
  onSplitVertical: () => void;
  onSplitHorizontal: () => void;
  onToggleSftp: () => void;
  onOpenIdentities: () => void;
  onOpenTunnels: () => void;
  onOpenTriggers: () => void;
  onOpenImport: () => void;
  onToggleHighlight: () => void;
  onFindInTerminal: () => void;
  onRevealLogs: () => void;
}

interface PaletteItem {
  id: string;
  category: "Sesiones" | "Acciones" | "Snippets";
  title: string;
  monoDetail?: string;
  rightMeta?: string;
  shortcut?: string;
  dotKind?: "ssh" | "serial" | "rdp";
  action: () => void;
}

function collectSessions(
  nodes: GroupNode[],
  prefix = "",
): { session: SavedSession; groupPath: string }[] {
  return nodes.flatMap((g) => {
    const path = prefix ? `${prefix} / ${g.name}` : g.name;
    return [
      ...g.sessions.map((session) => ({ session, groupPath: path })),
      ...collectSessions(g.children, path),
    ];
  });
}

export default function CommandPalette({
  tree,
  snippets,
  highlightEnabled,
  onClose,
  onConnectSession,
  onRunSnippet,
  onNewLocal,
  onNewConnection,
  onSplitVertical,
  onSplitHorizontal,
  onToggleSftp,
  onOpenIdentities,
  onOpenTunnels,
  onOpenTriggers,
  onOpenImport,
  onToggleHighlight,
  onFindInTerminal,
  onRevealLogs,
}: Props) {
  const [query, setQuery] = useState("");
  const [selectedIndex, setSelectedIndex] = useState(0);

  const items = useMemo<PaletteItem[]>(() => {
    const allSessions: PaletteItem[] = collectSessions(tree).map(
      ({ session, groupPath }) => ({
        id: `sess-${session.id}`,
        category: "Sesiones",
        title: session.name,
        monoDetail:
          session.protocol === "serial"
            ? session.serialPort || session.host
            : `${session.username}@${session.host}`,
        rightMeta: groupPath,
        dotKind: session.protocol,
        action: () => {
          onConnectSession(session);
          onClose();
        },
      }),
    );

    const actions: PaletteItem[] = [
      {
        id: "act-conn",
        category: "Acciones",
        title: "Nueva conexión…",
        rightMeta: "SSH, Serial o RDP",
        shortcut: "⌘N",
        action: () => {
          onNewConnection();
          onClose();
        },
      },
      {
        id: "act-local",
        category: "Acciones",
        title: "Nueva consola local",
        monoDetail: "zsh",
        shortcut: "⌘T",
        action: () => {
          onNewLocal();
          onClose();
        },
      },
      {
        id: "act-sftp",
        category: "Acciones",
        title: "Archivos SFTP",
        shortcut: "⌘E",
        action: () => {
          onToggleSftp();
          onClose();
        },
      },
      {
        id: "act-tunnels",
        category: "Acciones",
        title: "Túneles SSH y proxy SOCKS5…",
        monoDetail: "-L / -D",
        action: () => {
          onOpenTunnels();
          onClose();
        },
      },
      {
        id: "act-triggers",
        category: "Acciones",
        title: "Respuestas automáticas (Expect)…",
        action: () => {
          onOpenTriggers();
          onClose();
        },
      },
      {
        id: "act-identities",
        category: "Acciones",
        title: "Bóveda de identidades…",
        action: () => {
          onOpenIdentities();
          onClose();
        },
      },
      {
        id: "act-import",
        category: "Acciones",
        title: "Importar sesiones…",
        monoDetail: "~/.ssh/config · CSV · XML",
        action: () => {
          onOpenImport();
          onClose();
        },
      },
      {
        id: "act-split-v",
        category: "Acciones",
        title: "Dividir panel a la derecha",
        shortcut: "⌘D",
        action: () => {
          onSplitVertical();
          onClose();
        },
      },
      {
        id: "act-split-h",
        category: "Acciones",
        title: "Dividir panel abajo",
        shortcut: "⌘⇧D",
        action: () => {
          onSplitHorizontal();
          onClose();
        },
      },
      {
        id: "act-find",
        category: "Acciones",
        title: "Buscar en consola",
        shortcut: "⌘F",
        action: () => {
          onFindInTerminal();
          onClose();
        },
      },
      {
        id: "act-highlight",
        category: "Acciones",
        title: highlightEnabled
          ? "Desactivar resaltado de sintaxis"
          : "Activar resaltado de sintaxis",
        action: () => {
          onToggleHighlight();
          onClose();
        },
      },
      {
        id: "act-logs",
        category: "Acciones",
        title: "Abrir carpeta de registros en Finder",
        action: () => {
          onRevealLogs();
          onClose();
        },
      },
    ];

    const snippetItems: PaletteItem[] = snippets.map((s) => ({
      id: `snip-${s.id}`,
      category: "Snippets",
      title: s.name,
      monoDetail: s.command,
      action: () => {
        onRunSnippet(s.command);
        onClose();
      },
    }));

    const all = [...allSessions, ...actions, ...snippetItems];
    const raw = query.trim();
    const onlyActions = raw.startsWith(">");
    const q = (onlyActions ? raw.slice(1) : raw).trim().toLowerCase();
    const pool = onlyActions
      ? all.filter((i) => i.category === "Acciones")
      : all;
    if (!q) return pool;
    return pool.filter(
      (item) =>
        item.title.toLowerCase().includes(q) ||
        (item.monoDetail?.toLowerCase().includes(q) ?? false) ||
        (item.rightMeta?.toLowerCase().includes(q) ?? false),
    );
  }, [
    tree,
    snippets,
    highlightEnabled,
    query,
    onConnectSession,
    onRunSnippet,
    onNewLocal,
    onNewConnection,
    onSplitVertical,
    onSplitHorizontal,
    onToggleSftp,
    onOpenIdentities,
    onOpenTunnels,
    onOpenTriggers,
    onOpenImport,
    onToggleHighlight,
    onFindInTerminal,
    onRevealLogs,
    onClose,
  ]);

  useEffect(() => {
    setSelectedIndex(0);
  }, [query]);

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      if (items.length > 0) {
        setSelectedIndex((i) => (i + 1) % items.length);
      }
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      if (items.length > 0) {
        setSelectedIndex((i) => (i - 1 + items.length) % items.length);
      }
    } else if (e.key === "Enter") {
      e.preventDefault();
      const item = items[selectedIndex];
      if (item) item.action();
    }
  };

  let lastCategory: string | null = null;

  return (
    <div className="backdrop palette-backdrop" onMouseDown={onClose}>
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Paleta de comandos"
        className="palette-modal"
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={handleKeyDown}
      >
        <div className="palette-input-row">
          <Search
            size={16}
            strokeWidth={1.5}
            className="palette-search-icon"
          />
          <input
            autoFocus
            className="palette-input"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Ir a nodo, sesión o acción…"
          />
          <kbd className="palette-kbd">Esc</kbd>
        </div>

        <div className="palette-list" role="listbox">
          {items.length === 0 && (
            <div className="palette-empty">
              Sin resultados para “{query}”.
            </div>
          )}
          {items.map((item, idx) => {
            const showGroup = item.category !== lastCategory;
            lastCategory = item.category;
            return (
              <div key={item.id}>
                {showGroup && (
                  <div className="palette-group-head">{item.category}</div>
                )}
                <button
                  type="button"
                  role="option"
                  aria-selected={idx === selectedIndex}
                  className={`palette-item ${
                    idx === selectedIndex ? "selected" : ""
                  }`}
                  onMouseEnter={() => setSelectedIndex(idx)}
                  onClick={item.action}
                >
                  <span className="palette-item-icon">
                    {item.dotKind ? (
                      <span className={`host-status-dot ${item.dotKind}`} />
                    ) : (
                      <Terminal size={14} strokeWidth={1.5} />
                    )}
                  </span>
                  <span className="palette-item-body">
                    <span className="palette-item-title">{item.title}</span>
                    {item.monoDetail && (
                      <span className="palette-item-sub">
                        {item.monoDetail}
                      </span>
                    )}
                  </span>
                  {item.shortcut ? (
                    <kbd className="palette-kbd">{item.shortcut}</kbd>
                  ) : item.rightMeta ? (
                    <span className="palette-item-meta">{item.rightMeta}</span>
                  ) : null}
                </button>
              </div>
            );
          })}
        </div>

        <div className="palette-foot">
          <span>
            <kbd className="palette-kbd">↑</kbd>{" "}
            <kbd className="palette-kbd">↓</kbd> moverse
          </span>
          <span>
            <kbd className="palette-kbd">Enter</kbd> ejecutar
          </span>
          <span style={{ marginLeft: "auto" }}>
            Empieza con <kbd className="palette-kbd">&gt;</kbd> para ver solo
            acciones
          </span>
        </div>
      </div>
    </div>
  );
}
