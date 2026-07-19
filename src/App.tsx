import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Moon, Send, Sun, Zap } from "lucide-react";
import { dragWindow } from "./drag";
import Logo from "./components/Logo";
import { initialTheme, persistTheme, type ThemeMode } from "./theme";
import Sidebar from "./components/Sidebar";
import SnippetBar from "./components/SnippetBar";
import SshDialog from "./components/SshDialog";
import TerminalView from "./components/TerminalView";
import type {
  GroupNode,
  Notice,
  RdpAvailability,
  RdpConfig,
  Snippet,
  SavedSession,
  SessionDraft,
  SessionSpec,
  SshConfig,
  Tab,
} from "./types";

let tabCounter = 0;
const nextTabId = () => `tab-${++tabCounter}`;
let noticeCounter = 0;

interface DialogState {
  open: boolean;
  initial?: SavedSession | null;
  presetGroupId?: string | null;
}

export default function App() {
  const [tree, setTree] = useState<GroupNode[]>([]);
  const [tabs, setTabs] = useState<Tab[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [dialog, setDialog] = useState<DialogState>({ open: false });
  const [notices, setNotices] = useState<Notice[]>([]);
  const [broadcastOpen, setBroadcastOpen] = useState(false);
  const [broadcastSel, setBroadcastSel] = useState<Set<string>>(new Set());
  const [broadcastText, setBroadcastText] = useState("");
  const [theme, setTheme] = useState<ThemeMode>(initialTheme);
  const [rdpAvailable, setRdpAvailable] = useState(true);
  const [snippets, setSnippets] = useState<Snippet[]>([]);
  const [snippetRun, setSnippetRun] = useState(false);

  useEffect(() => {
    invoke<RdpAvailability>("rdp_availability")
      .then((a) => setRdpAvailable(a.available))
      .catch(() => setRdpAvailable(false));
  }, []);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    persistTheme(theme);
  }, [theme]);

  const tabsRef = useRef(tabs);
  tabsRef.current = tabs;
  const activeRef = useRef(activeId);
  activeRef.current = activeId;

  // --- Notices --------------------------------------------------------------
  const pushNotice = useCallback((level: Notice["level"], text: string) => {
    const id = ++noticeCounter;
    setNotices((prev) => [...prev, { id, level, text }]);
    if (level === "info") {
      window.setTimeout(
        () => setNotices((prev) => prev.filter((n) => n.id !== id)),
        6000,
      );
    }
  }, []);

  const dismissNotice = (id: number) =>
    setNotices((prev) => prev.filter((n) => n.id !== id));

  // --- Saved-session tree ---------------------------------------------------
  const loadTree = useCallback(async () => {
    try {
      setTree(await invoke<GroupNode[]>("list_tree"));
    } catch (e) {
      pushNotice("danger", `Could not load saved sessions: ${String(e)}`);
    }
  }, [pushNotice]);

  useEffect(() => {
    void loadTree();
  }, [loadTree]);

  const loadSnippets = useCallback(() => {
    invoke<Snippet[]>("list_snippets")
      .then(setSnippets)
      .catch((e) => pushNotice("danger", `Could not load snippets: ${String(e)}`));
  }, [pushNotice]);

  useEffect(() => {
    loadSnippets();
  }, [loadSnippets]);

  // --- Tabs -----------------------------------------------------------------
  const openTab = useCallback((spec: SessionSpec, title: string) => {
    const tab: Tab = { tabId: nextTabId(), title, spec, status: "connecting" };
    setTabs((prev) => [...prev, tab]);
    setActiveId(tab.tabId);
  }, []);

  const openLocal = useCallback(() => openTab({ kind: "local" }, "local"), [openTab]);

  const openSaved = useCallback(
    (s: SavedSession) => {
      if (s.protocol === "rdp") {
        invoke("connect_saved_rdp", { id: s.id }).catch((e) =>
          setNotices((prev) => [
            ...prev,
            { id: ++noticeCounter, level: "danger", text: String(e) },
          ]),
        );
      } else {
        openTab({ kind: "saved", savedId: s.id }, s.name);
      }
    },
    [openTab],
  );

  const closeTab = useCallback((tabId: string) => {
    setTabs((prev) => {
      const idx = prev.findIndex((t) => t.tabId === tabId);
      const next = prev.filter((t) => t.tabId !== tabId);
      setActiveId((current) => {
        if (current !== tabId) return current;
        const neighbor = next[idx] ?? next[idx - 1];
        return neighbor ? neighbor.tabId : null;
      });
      return next;
    });
    setBroadcastSel((prev) => {
      if (!prev.has(tabId)) return prev;
      const next = new Set(prev);
      next.delete(tabId);
      return next;
    });
  }, []);

  const patchTab = useCallback((tabId: string, patch: Partial<Tab>) => {
    setTabs((prev) => prev.map((t) => (t.tabId === tabId ? { ...t, ...patch } : t)));
  }, []);

  // --- Backend events -------------------------------------------------------
  useEffect(() => {
    const un1 = listen<{ id: string; reason: string }>("session-exit", (event) => {
      setTabs((prev) =>
        prev.map((t) =>
          t.sessionId === event.payload.id
            ? { ...t, status: "dead", statusDetail: event.payload.reason }
            : t,
        ),
      );
    });
    const un2 = listen<{ level: "info" | "danger"; message: string }>(
      "host-key-event",
      (event) => pushNotice(event.payload.level, event.payload.message),
    );
    const un3 = listen<string>("rdp-launched", (event) =>
      pushNotice("info", event.payload),
    );
    return () => {
      void un1.then((fn) => fn());
      void un2.then((fn) => fn());
      void un3.then((fn) => fn());
    };
  }, [pushNotice]);

  // First launch: open a local shell so the app never starts empty.
  useEffect(() => {
    openLocal();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // --- Keyboard shortcuts ---------------------------------------------------
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const meta = e.metaKey && !e.ctrlKey && !e.altKey;
      if (meta && e.key === "t") {
        e.preventDefault();
        openLocal();
      } else if (meta && e.key === "n") {
        e.preventDefault();
        setDialog({ open: true });
      } else if (meta && e.key === "w") {
        e.preventDefault();
        if (activeRef.current) closeTab(activeRef.current);
      } else if (meta && e.key === "b") {
        e.preventDefault();
        setBroadcastOpen((v) => !v);
      } else if (e.ctrlKey && e.key === "Tab") {
        e.preventDefault();
        const list = tabsRef.current;
        if (list.length < 2) return;
        const idx = list.findIndex((t) => t.tabId === activeRef.current);
        const next = list[(idx + (e.shiftKey ? list.length - 1 : 1)) % list.length];
        setActiveId(next.tabId);
      } else if (meta && /^[1-9]$/.test(e.key)) {
        const target = tabsRef.current[Number(e.key) - 1];
        if (target) {
          e.preventDefault();
          setActiveId(target.tabId);
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [openLocal, closeTab]);

  // --- RDP -----------------------------------------------------------------
  const copyInstall = () => {
    void navigator.clipboard?.writeText("brew install freerdp");
    pushNotice("info", "Copied: brew install freerdp");
  };

  const launchRdp = (config: RdpConfig) => {
    invoke("launch_rdp", { config }).catch((e) =>
      pushNotice("danger", String(e)),
    );
  };

  // --- Dialog actions -------------------------------------------------------
  const handleConnectSsh = (config: SshConfig) => {
    setDialog({ open: false });
    openTab({ kind: "ssh", config }, `${config.username}@${config.host}`);
  };

  const handleConnectRdp = (config: RdpConfig) => {
    setDialog({ open: false });
    launchRdp(config);
  };

  const handleSave = async (
    draft: SessionDraft,
    connectAfter: boolean,
    newGroupName?: string,
  ) => {
    setDialog({ open: false });
    try {
      let groupId = draft.groupId;
      if (newGroupName) {
        groupId = await invoke<string>("create_group", { name: newGroupName });
      }
      const saved = await invoke<SavedSession>("save_session", {
        input: { ...draft, groupId },
      });
      await loadTree();
      if (connectAfter) openSaved(saved);
      // openSaved already routes RDP vs SSH by saved.protocol
    } catch (e) {
      pushNotice("danger", `Could not save session: ${String(e)}`);
    }
  };

  // --- Sidebar actions ------------------------------------------------------
  const sidebarActions = {
    onConnect: openSaved,
    onEdit: (s: SavedSession) => setDialog({ open: true, initial: s }),
    onNewInGroup: (groupId: string) =>
      setDialog({ open: true, presetGroupId: groupId }),
    onCreateGroup: (name: string, parentId?: string) =>
      invoke("create_group", { name, parentId }).then(loadTree, (e) =>
        pushNotice("danger", String(e)),
      ),
    onNewSubgroup: (parentId: string) => {
      const name = window.prompt("Subgroup name");
      if (name && name.trim()) {
        invoke("create_group", { name: name.trim(), parentId }).then(loadTree, (e) =>
          pushNotice("danger", String(e)),
        );
      }
    },
    onRenameGroup: (id: string, name: string) =>
      invoke("rename_group", { id, name }).then(loadTree, (e) =>
        pushNotice("danger", String(e)),
      ),
    onDeleteGroup: (id: string) =>
      invoke("delete_group", { id }).then(loadTree, (e) =>
        pushNotice("danger", String(e)),
      ),
    onDeleteSession: (id: string) =>
      invoke("delete_session", { id }).then(loadTree, (e) =>
        pushNotice("danger", String(e)),
      ),
    onDuplicateSession: (id: string) =>
      invoke<SavedSession>("duplicate_session", { id }).then(
        async (copy) => {
          await loadTree();
          // Open the clone right away so the user can change host/IP.
          setDialog({ open: true, initial: copy });
          pushNotice("info", "Session duplicated — edit host and IP.");
        },
        (e) => pushNotice("danger", String(e)),
      ),
    onMoveGroup: (id: string, parentId: string | null) =>
      invoke("move_group", { id, parentId }).then(
        () => {
          void loadTree();
          pushNotice("info", parentId ? "Group nested." : "Group moved to top level.");
        },
        (e) => pushNotice("danger", String(e)),
      ),
    onMoveSession: (id: string, groupId: string) =>
      invoke("move_session", { id, groupId }).then(
        () => {
          void loadTree();
          pushNotice("info", "Session moved.");
        },
        (e) => pushNotice("danger", String(e)),
      ),
    onRevealLogs: () =>
      invoke("reveal_logs").catch((e) => pushNotice("info", String(e))),
  };

  // --- Snippets -------------------------------------------------------------
  const sendSnippet = (command: string) => {
    const payload = snippetRun ? `${command}\r` : command;
    const targets =
      broadcastOpen && broadcastSel.size > 0
        ? tabs.filter(
            (t) => broadcastSel.has(t.tabId) && t.status === "live" && t.sessionId,
          )
        : tabs.filter((t) => t.tabId === activeId && t.status === "live" && t.sessionId);
    if (targets.length === 0) {
      pushNotice("info", "No live session to send to.");
      return;
    }
    for (const t of targets) {
      void invoke("write_to_session", { id: t.sessionId, data: payload });
    }
  };

  const saveSnippet = (input: { id?: string; name: string; command: string }) => {
    invoke<Snippet>("save_snippet", { input }).then(loadSnippets, (e) =>
      pushNotice("danger", String(e)),
    );
  };

  const deleteSnippet = (id: string) => {
    invoke("delete_snippet", { id }).then(loadSnippets, (e) =>
      pushNotice("danger", String(e)),
    );
  };

  const activeLive = tabs.some(
    (t) => t.tabId === activeId && t.status === "live" && t.sessionId,
  );

  // --- Broadcast ------------------------------------------------------------
  const toggleBroadcast = (tabId: string) => {
    setBroadcastSel((prev) => {
      const next = new Set(prev);
      if (next.has(tabId)) next.delete(tabId);
      else next.add(tabId);
      return next;
    });
  };

  const liveTargets = tabs.filter(
    (t) => broadcastSel.has(t.tabId) && t.status === "live" && t.sessionId,
  );

  const sendBroadcast = () => {
    const text = broadcastText;
    if (!text.trim() || liveTargets.length === 0) return;
    for (const t of liveTargets) {
      void invoke("write_to_session", { id: t.sessionId, data: `${text}\r` });
    }
    setBroadcastText("");
  };

  return (
    <div className="app">
      <Sidebar tree={tree} {...sidebarActions} />

      <div className="main">
        <header className="tabbar" data-tauri-drag-region onMouseDown={dragWindow}>
          <div className="tabs" role="tablist" onMouseDown={dragWindow}>
            {tabs.map((tab) => (
              <div
                key={tab.tabId}
                role="tab"
                aria-selected={tab.tabId === activeId}
                className={`tab ${tab.tabId === activeId ? "active" : ""} ${
                  broadcastOpen && broadcastSel.has(tab.tabId) ? "bcast" : ""
                }`}
                onMouseDown={() => setActiveId(tab.tabId)}
                title={tab.statusDetail ?? tab.title}
              >
                <span className={`jack ${tab.status}`} aria-hidden="true" />
                <span className="tab-title">{tab.title}</span>
                {broadcastOpen && (
                  <button
                    className={`bcast-toggle ${broadcastSel.has(tab.tabId) ? "on" : ""}`}
                    title="Include in broadcast"
                    onMouseDown={(e) => e.stopPropagation()}
                    onClick={() => toggleBroadcast(tab.tabId)}
                  >
                    <Zap size={11} />
                  </button>
                )}
                <button
                  className="tab-close"
                  aria-label={`Close ${tab.title}`}
                  onMouseDown={(e) => e.stopPropagation()}
                  onClick={() => closeTab(tab.tabId)}
                >
                  ×
                </button>
              </div>
            ))}
          </div>
          <div className="actions">
            <button
              className="btn-icon"
              title={theme === "dark" ? "Switch to light mode" : "Switch to dark mode"}
              onClick={() => setTheme((t) => (t === "dark" ? "light" : "dark"))}
            >
              {theme === "dark" ? <Sun size={15} /> : <Moon size={15} />}
            </button>
            <button
              className={`btn-icon ${broadcastOpen ? "accent-on" : ""}`}
              title="Multi-exec broadcast (⌘B)"
              onClick={() => setBroadcastOpen((v) => !v)}
            >
              <Zap size={15} />
            </button>
            <button className="btn ghost" title="New local shell (⌘T)" onClick={openLocal}>
              + local
            </button>
            <button
              className="btn accent"
              title="New SSH connection (⌘N)"
              onClick={() => setDialog({ open: true })}
            >
              + ssh
            </button>
          </div>
        </header>

        <main className="board">
          {tabs.length === 0 && (
            <div className="empty">
              <span className="empty-logo"><Logo size={44} /></span>
              <p className="empty-title">Switchboard</p>
              <p className="empty-hint">
                Open a local shell (⌘T), connect over SSH (⌘N), or double-click a
                saved session in the sidebar.
              </p>
            </div>
          )}
          {tabs.map((tab) => (
            <TerminalView
              key={tab.tabId}
              spec={tab.spec}
              active={tab.tabId === activeId}
              theme={theme}
              onSession={(sessionId) =>
                patchTab(tab.tabId, { sessionId, status: "live" })
              }
              onError={(message) =>
                patchTab(tab.tabId, { status: "dead", statusDetail: message })
              }
            />
          ))}

          <div className="notices">
            {notices.map((n) => (
              <div key={n.id} className={`notice ${n.level}`}>
                <span>{n.text}</span>
                <button onClick={() => dismissNotice(n.id)} aria-label="Dismiss">
                  ×
                </button>
              </div>
            ))}
          </div>
        </main>

        <SnippetBar
          snippets={snippets}
          runMode={snippetRun}
          hasActiveSession={activeLive || (broadcastOpen && broadcastSel.size > 0)}
          onToggleRun={() => setSnippetRun((v) => !v)}
          onSend={sendSnippet}
          onSave={saveSnippet}
          onDelete={deleteSnippet}
        />

        {broadcastOpen && (
          <div className="broadcast">
            <Zap size={14} className="broadcast-icon" />
            <input
              autoFocus
              placeholder={
                liveTargets.length === 0
                  ? "Mark tabs with the ⚡ toggle, then type a command…"
                  : `Send to ${liveTargets.length} session${liveTargets.length > 1 ? "s" : ""} — Enter to run`
              }
              value={broadcastText}
              onChange={(e) => setBroadcastText(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && sendBroadcast()}
            />
            <button
              className="btn accent sm"
              disabled={liveTargets.length === 0 || !broadcastText.trim()}
              onClick={sendBroadcast}
            >
              <Send size={13} /> {liveTargets.length}
            </button>
          </div>
        )}
      </div>

      {dialog.open && (
        <SshDialog
          groups={tree}
          rdpAvailable={rdpAvailable}
          initial={dialog.initial}
          presetGroupId={dialog.presetGroupId}
          onCancel={() => setDialog({ open: false })}
          onConnectSsh={handleConnectSsh}
          onConnectRdp={handleConnectRdp}
          onSave={handleSave}
          onInstallHint={copyInstall}
        />
      )}
    </div>
  );
}
