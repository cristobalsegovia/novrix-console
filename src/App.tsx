import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Ellipsis, Plus, X } from "lucide-react";
import { dragWindow } from "./drag";
import CommandPalette from "./components/CommandPalette";
import IdentitiesModal from "./components/IdentitiesModal";
import ImportModal from "./components/ImportModal";
import Logo from "./components/Logo";
import SftpPanel from "./components/SftpPanel";
import Sidebar from "./components/Sidebar";
import SnippetBar from "./components/SnippetBar";
import SshDialog from "./components/SshDialog";
import TerminalView from "./components/TerminalView";
import TriggersModal from "./components/TriggersModal";
import TunnelsModal from "./components/TunnelsModal";
import { initialTheme, persistTheme, type ThemeMode } from "./theme";
import type {
  ExpectRule,
  GroupNode,
  HostKeyEvent,
  Identity,
  IdentityDraft,
  Notice,
  PaneState,
  RdpAvailability,
  RdpConfig,
  SavedSession,
  SerialConfig,
  ServerMetrics,
  SessionDraft,
  SessionSpec,
  Snippet,
  SshConfig,
  Tab,
} from "./types";

let tabCounter = 0;
const nextTabId = () => `tab-${++tabCounter}`;
let paneCounter = 0;
const nextPaneId = () => `pane-${++paneCounter}`;
let noticeCounter = 0;

interface DialogState {
  open: boolean;
  initial?: SavedSession | null;
  presetGroupId?: string | null;
}

export default function App() {
  const [tree, setTree] = useState<GroupNode[]>([]);
  const [identities, setIdentities] = useState<Identity[]>([]);
  const [tabs, setTabs] = useState<Tab[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [dialog, setDialog] = useState<DialogState>({ open: false });
  const [identitiesOpen, setIdentitiesOpen] = useState(false);
  const [tunnelsOpen, setTunnelsOpen] = useState(false);
  const [triggersOpen, setTriggersOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [sftpOpen, setSftpOpen] = useState(false);
  const [toolsMenuOpen, setToolsMenuOpen] = useState(false);
  const [snippetsVisible, setSnippetsVisible] = useState(false);
  const [notices, setNotices] = useState<Notice[]>([]);
  const [broadcastOpen, setBroadcastOpen] = useState(false);
  const [broadcastSel, setBroadcastSel] = useState<Set<string>>(new Set());
  const [broadcastText, setBroadcastText] = useState("");
  const [theme, setTheme] = useState<ThemeMode>(initialTheme);
  const [rdpAvailable, setRdpAvailable] = useState(true);
  const [snippets, setSnippets] = useState<Snippet[]>([]);
  const [expectRules, setExpectRules] = useState<ExpectRule[]>([]);
  const [metrics, setMetrics] = useState<ServerMetrics | null>(null);
  const [snippetRun, setSnippetRun] = useState(false);
  const [highlightEnabled, setHighlightEnabled] = useState<boolean>(() => {
    return window.localStorage.getItem("sbt-highlight") !== "off";
  });
  const [searchTrigger, setSearchTrigger] = useState(0);

  useEffect(() => {
    invoke<RdpAvailability>("rdp_availability")
      .then((a) => setRdpAvailable(a.available))
      .catch(() => setRdpAvailable(false));
  }, []);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    persistTheme(theme);
  }, [theme]);

  useEffect(() => {
    window.localStorage.setItem(
      "sbt-highlight",
      highlightEnabled ? "on" : "off",
    );
  }, [highlightEnabled]);

  useEffect(() => {
    if (!toolsMenuOpen) return;
    const close = () => setToolsMenuOpen(false);
    window.addEventListener("mousedown", close);
    return () => window.removeEventListener("mousedown", close);
  }, [toolsMenuOpen]);

  const tabsRef = useRef(tabs);
  tabsRef.current = tabs;
  const activeRef = useRef(activeId);
  activeRef.current = activeId;

  // --- Notices --------------------------------------------------------------
  const pushNotice = useCallback(
    (
      level: Notice["level"],
      text: string,
      actionLabel?: string,
      onAction?: () => void,
    ) => {
      const id = ++noticeCounter;
      setNotices((prev) => [
        ...prev,
        { id, level, text, actionLabel, onAction },
      ]);
      if (level === "info" && !actionLabel) {
        window.setTimeout(
          () => setNotices((prev) => prev.filter((n) => n.id !== id)),
          5000,
        );
      }
      return id;
    },
    [],
  );

  const dismissNotice = useCallback(
    (id: number) => setNotices((prev) => prev.filter((n) => n.id !== id)),
    [],
  );

  // --- Saved-session tree & Identities --------------------------------------
  const loadTree = useCallback(async () => {
    try {
      setTree(await invoke<GroupNode[]>("list_tree"));
    } catch (e) {
      pushNotice("danger", `Could not load saved sessions: ${String(e)}`);
    }
  }, [pushNotice]);

  const loadIdentities = useCallback(async () => {
    try {
      setIdentities(await invoke<Identity[]>("list_identities"));
    } catch (e) {
      pushNotice("danger", `Could not load identities: ${String(e)}`);
    }
  }, [pushNotice]);

  useEffect(() => {
    void loadTree();
    void loadIdentities();
  }, [loadTree, loadIdentities]);

  const loadSnippets = useCallback(() => {
    invoke<Snippet[]>("list_snippets")
      .then(setSnippets)
      .catch((e) =>
        pushNotice("danger", `Could not load snippets: ${String(e)}`),
      );
  }, [pushNotice]);

  const loadExpectRules = useCallback(() => {
    invoke<ExpectRule[]>("list_expect_rules")
      .then(setExpectRules)
      .catch(() => {});
  }, []);

  useEffect(() => {
    loadSnippets();
    loadExpectRules();
  }, [loadSnippets, loadExpectRules]);

  // --- Tabs & Split Panes ---------------------------------------------------
  const openTab = useCallback((spec: SessionSpec, title: string) => {
    const paneId = nextPaneId();
    const initialPane: PaneState = {
      paneId,
      title,
      spec,
      status: "connecting",
      reconnectEpoch: 0,
    };
    const tab: Tab = {
      tabId: nextTabId(),
      title,
      spec,
      status: "connecting",
      panes: [initialPane],
      activePaneId: paneId,
      splitDirection: "vertical",
    };
    setTabs((prev) => [...prev, tab]);
    setActiveId(tab.tabId);
  }, []);

  const openLocal = useCallback(
    () => openTab({ kind: "local" }, "zsh"),
    [openTab],
  );

  const openSaved = useCallback(
    (s: SavedSession) => {
      if (s.protocol === "rdp") {
        invoke("connect_saved_rdp", { id: s.id }).catch((e) =>
          pushNotice("danger", String(e)),
        );
      } else {
        openTab(
          { kind: "saved", savedId: s.id, protocol: s.protocol },
          s.name,
        );
      }
    },
    [openTab, pushNotice],
  );

  const openSavedInSplit = useCallback(
    (s: SavedSession) => {
      if (s.protocol === "rdp") {
        openSaved(s);
        return;
      }
      const spec: SessionSpec = {
        kind: "saved",
        savedId: s.id,
        protocol: s.protocol,
      };
      const tabId = activeRef.current;
      if (!tabId) {
        openTab(spec, s.name);
        return;
      }
      setTabs((prev) =>
        prev.map((t) => {
          if (t.tabId !== tabId) return t;
          const newPaneId = nextPaneId();
          const newPane: PaneState = {
            paneId: newPaneId,
            title: s.name,
            spec,
            status: "connecting",
            reconnectEpoch: 0,
          };
          return {
            ...t,
            panes: [...t.panes, newPane],
            activePaneId: newPaneId,
          };
        }),
      );
    },
    [openSaved, openTab],
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

  const closeActivePaneOrTab = useCallback(() => {
    const tabId = activeRef.current;
    if (!tabId) return;
    const currentTab = tabsRef.current.find((t) => t.tabId === tabId);
    if (!currentTab) return;
    if (currentTab.panes.length <= 1) {
      closeTab(tabId);
      return;
    }
    setTabs((prev) =>
      prev.map((t) => {
        if (t.tabId !== tabId) return t;
        const remaining = t.panes.filter((p) => p.paneId !== t.activePaneId);
        const nextActive = remaining[remaining.length - 1]?.paneId ?? "";
        return {
          ...t,
          panes: remaining,
          activePaneId: nextActive,
        };
      }),
    );
  }, [closeTab]);

  const splitActiveTab = useCallback(
    (direction: "vertical" | "horizontal") => {
      const tabId = activeRef.current;
      if (!tabId) return;
      setTabs((prev) =>
        prev.map((t) => {
          if (t.tabId !== tabId) return t;
          const activePane =
            t.panes.find((p) => p.paneId === t.activePaneId) ?? t.panes[0];
          const newPaneId = nextPaneId();
          const newPane: PaneState = {
            paneId: newPaneId,
            title: activePane ? activePane.title : t.title,
            spec: activePane ? activePane.spec : t.spec,
            status: "connecting",
            reconnectEpoch: 0,
          };
          return {
            ...t,
            splitDirection: direction,
            panes: [...t.panes, newPane],
            activePaneId: newPaneId,
          };
        }),
      );
    },
    [],
  );

  const patchPane = useCallback(
    (tabId: string, paneId: string, patch: Partial<PaneState>) => {
      setTabs((prev) =>
        prev.map((t) => {
          if (t.tabId !== tabId) return t;
          const panes = t.panes.map((p) =>
            p.paneId === paneId ? { ...p, ...patch } : p,
          );
          const primary =
            panes.find((p) => p.paneId === t.activePaneId) ?? panes[0];
          const anyLive = panes.some((p) => p.status === "live");
          const anyConnecting = panes.some((p) => p.status === "connecting");
          return {
            ...t,
            panes,
            sessionId: primary?.sessionId,
            status: anyLive ? "live" : anyConnecting ? "connecting" : "dead",
            statusDetail: primary?.statusDetail,
          };
        }),
      );
    },
    [],
  );

  const reconnectPane = useCallback((tabId: string, paneId: string) => {
    setTabs((prev) =>
      prev.map((t) => {
        if (t.tabId !== tabId) return t;
        const panes = t.panes.map((p) =>
          p.paneId === paneId
            ? {
                ...p,
                status: "connecting" as const,
                statusDetail: undefined,
                sessionId: undefined,
                reconnectEpoch: p.reconnectEpoch + 1,
              }
            : p,
        );
        return { ...t, panes, status: "connecting" };
      }),
    );
  }, []);

  // --- Backend events -------------------------------------------------------
  useEffect(() => {
    const un1 = listen<{ id: string; reason: string }>(
      "session-exit",
      (event) => {
        setTabs((prev) =>
          prev.map((t) => {
            if (!t.panes.some((p) => p.sessionId === event.payload.id)) {
              return t;
            }
            const panes = t.panes.map((p) =>
              p.sessionId === event.payload.id
                ? {
                    ...p,
                    status: "dead" as const,
                    statusDetail: event.payload.reason,
                  }
                : p,
            );
            const anyLive = panes.some((p) => p.status === "live");
            const anyConnecting = panes.some((p) => p.status === "connecting");
            return {
              ...t,
              panes,
              status: anyLive ? "live" : anyConnecting ? "connecting" : "dead",
              statusDetail: event.payload.reason,
            };
          }),
        );
      },
    );
    const un2 = listen<HostKeyEvent>("host-key-event", (event) => {
      const payload = event.payload;
      if (payload.level === "danger" && payload.changedLine != null) {
        const noticeId = noticeCounter + 1;
        pushNotice(
          "danger",
          payload.message,
          "Trust New Key & Reconnect",
          () => {
            invoke("accept_changed_host_key", {
              host: payload.host,
              port: payload.port,
              line: payload.changedLine,
            })
              .then(() => {
                dismissNotice(noticeId);
                pushNotice(
                  "info",
                  `Updated known_hosts for ${payload.host}. Reconnecting…`,
                );
                const currentTab = tabsRef.current.find(
                  (t) => t.tabId === activeRef.current,
                );
                if (currentTab) {
                  for (const p of currentTab.panes) {
                    if (p.status === "dead") {
                      reconnectPane(currentTab.tabId, p.paneId);
                    }
                  }
                }
              })
              .catch((e) => pushNotice("danger", String(e)));
          },
        );
      } else {
        pushNotice(payload.level, payload.message);
      }
    });
    const un3 = listen<string>("rdp-launched", (event) =>
      pushNotice("info", event.payload),
    );
    return () => {
      void un1.then((fn) => fn());
      void un2.then((fn) => fn());
      void un3.then((fn) => fn());
    };
  }, [pushNotice, dismissNotice, reconnectPane]);

  // First launch: open a local shell so the app never starts empty.
  useEffect(() => {
    openLocal();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // --- Keyboard shortcuts ---------------------------------------------------
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const meta = e.metaKey && !e.ctrlKey && !e.altKey;
      if (meta && (e.key === "k" || e.key === "K")) {
        e.preventDefault();
        setPaletteOpen((v) => !v);
      } else if (meta && !e.shiftKey && (e.key === "t" || e.key === "T")) {
        e.preventDefault();
        openLocal();
      } else if (meta && !e.shiftKey && (e.key === "n" || e.key === "N")) {
        e.preventDefault();
        setDialog({ open: true });
      } else if (meta && !e.shiftKey && (e.key === "w" || e.key === "W")) {
        e.preventDefault();
        closeActivePaneOrTab();
      } else if (meta && (e.key === "d" || e.key === "D")) {
        e.preventDefault();
        splitActiveTab(e.shiftKey ? "horizontal" : "vertical");
      } else if (meta && !e.shiftKey && (e.key === "e" || e.key === "E")) {
        e.preventDefault();
        setSftpOpen((v) => !v);
      } else if (meta && !e.shiftKey && (e.key === "b" || e.key === "B")) {
        e.preventDefault();
        setBroadcastOpen((v) => !v);
      } else if (e.ctrlKey && e.key === "Tab") {
        e.preventDefault();
        const list = tabsRef.current;
        if (list.length < 2) return;
        const idx = list.findIndex((t) => t.tabId === activeRef.current);
        const next =
          list[(idx + (e.shiftKey ? list.length - 1 : 1)) % list.length];
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
  }, [openLocal, closeActivePaneOrTab, splitActiveTab]);

  // --- RDP & Serial ---------------------------------------------------------
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

  const handleConnectSerial = (config: SerialConfig) => {
    setDialog({ open: false });
    openTab(
      { kind: "serial", config },
      `${config.portName.replace("/dev/", "")}@${config.baudRate}`,
    );
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
    } catch (e) {
      pushNotice("danger", `Could not save session: ${String(e)}`);
    }
  };

  // --- Identities (Credential Vault) ----------------------------------------
  const handleSaveIdentity = async (draft: IdentityDraft) => {
    try {
      await invoke("save_identity", { input: draft });
      await loadIdentities();
      pushNotice("info", `Saved identity "${draft.name}".`);
    } catch (e) {
      pushNotice("danger", `Could not save identity: ${String(e)}`);
    }
  };

  const handleDeleteIdentity = async (id: string) => {
    try {
      await invoke("delete_identity", { id });
      await loadIdentities();
      await loadTree();
    } catch (e) {
      pushNotice("danger", `Could not delete identity: ${String(e)}`);
    }
  };

  // --- Sidebar actions ------------------------------------------------------
  const sidebarActions = {
    onConnect: openSaved,
    onConnectInSplit: openSavedInSplit,
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
        invoke("create_group", { name: name.trim(), parentId }).then(
          loadTree,
          (e) => pushNotice("danger", String(e)),
        );
      }
    },
    onRenameGroup: (id: string, name: string) =>
      invoke("rename_group", { id, name }).then(loadTree, (e) =>
        pushNotice("danger", String(e)),
      ),
    onSetGroupIdentity: (id: string, identityId: string | null) =>
      invoke("set_group_identity", { id, identityId }).then(
        () => {
          void loadTree();
          pushNotice(
            "info",
            identityId
              ? "Default group identity updated."
              : "Default group identity cleared.",
          );
        },
        (e) => pushNotice("danger", String(e)),
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
          setDialog({ open: true, initial: copy });
          pushNotice("info", "Session duplicated — edit host and IP.");
        },
        (e) => pushNotice("danger", String(e)),
      ),
    onMoveGroup: (id: string, parentId: string | null) =>
      invoke("move_group", { id, parentId }).then(
        () => {
          void loadTree();
          pushNotice(
            "info",
            parentId ? "Group nested." : "Group moved to top level.",
          );
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
    onOpenIdentities: () => setIdentitiesOpen(true),
    onOpenPalette: () => setPaletteOpen(true),
    onImportSshConfig: () => setImportOpen(true),
  };

  // --- Snippets & Broadcast -------------------------------------------------
  const getBroadcastSessionIds = (): string[] => {
    return tabs
      .filter((t) => broadcastSel.has(t.tabId))
      .flatMap((t) =>
        t.panes
          .filter((p) => p.status === "live" && p.sessionId)
          .map((p) => p.sessionId!),
      );
  };

  const getActivePaneSessionId = (): string | undefined => {
    const activeTab = tabs.find((t) => t.tabId === activeId);
    if (!activeTab) return undefined;
    const activePane =
      activeTab.panes.find((p) => p.paneId === activeTab.activePaneId) ??
      activeTab.panes[0];
    return activePane?.status === "live" ? activePane.sessionId : undefined;
  };

  const sendSnippet = (command: string) => {
    const payload = snippetRun ? `${command}\r` : command;
    const sessionIds =
      broadcastOpen && broadcastSel.size > 0
        ? getBroadcastSessionIds()
        : (() => {
            const sid = getActivePaneSessionId();
            return sid ? [sid] : [];
          })();

    if (sessionIds.length === 0) {
      pushNotice("info", "No live session to send to.");
      return;
    }
    for (const id of sessionIds) {
      void invoke("write_to_session", { id, data: payload });
    }
  };

  const saveSnippet = (input: {
    id?: string;
    name: string;
    command: string;
  }) => {
    invoke<Snippet>("save_snippet", { input }).then(loadSnippets, (e) =>
      pushNotice("danger", String(e)),
    );
  };

  const deleteSnippet = (id: string) => {
    invoke("delete_snippet", { id }).then(loadSnippets, (e) =>
      pushNotice("danger", String(e)),
    );
  };

  const saveExpectRule = (input: {
    id?: string;
    name: string;
    expectPattern: string;
    sendText: string;
    enabled: boolean;
  }) => {
    invoke("save_expect_rule", { input }).then(loadExpectRules, (e) =>
      pushNotice("danger", String(e)),
    );
  };

  const toggleExpectRule = (id: string, enabled: boolean) => {
    invoke("toggle_expect_rule", { id, enabled }).then(loadExpectRules, (e) =>
      pushNotice("danger", String(e)),
    );
  };

  const deleteExpectRule = (id: string) => {
    invoke("delete_expect_rule", { id }).then(loadExpectRules, (e) =>
      pushNotice("danger", String(e)),
    );
  };

  const activePaneSessionId = getActivePaneSessionId();
  const activeTab = tabs.find((t) => t.tabId === activeId);
  const activePane =
    activeTab?.panes.find((p) => p.paneId === activeTab.activePaneId) ??
    activeTab?.panes[0];
  const isActiveSsh =
    activePane?.spec.kind === "ssh" ||
    (activePane?.spec.kind === "saved" &&
      (activePane.spec.protocol ?? "ssh") === "ssh");

  // Live Server Metrics HUD polling over active SSH session
  useEffect(() => {
    if (!isActiveSsh || !activePaneSessionId) {
      setMetrics(null);
      return;
    }
    let cancelled = false;
    let supported = true;

    const poll = async () => {
      if (!supported || cancelled) return;
      try {
        const m = await invoke<ServerMetrics>("poll_ssh_metrics", {
          sessionId: activePaneSessionId,
        });
        if (cancelled) return;
        if (!m.supported) {
          supported = false;
          setMetrics(null);
        } else {
          setMetrics(m);
        }
      } catch {
        if (!cancelled) setMetrics(null);
      }
    };

    void poll();
    const timer = window.setInterval(() => void poll(), 6000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [isActiveSsh, activePaneSessionId]);

  const liveSshSessions = tabs.flatMap((t) =>
    t.panes
      .filter(
        (p) =>
          p.status === "live" &&
          p.sessionId &&
          (p.spec.kind === "ssh" ||
            (p.spec.kind === "saved" &&
              (p.spec.protocol ?? "ssh") === "ssh")),
      )
      .map((p) => ({
        sessionId: p.sessionId!,
        label: p.title,
      })),
  );

  const activeProtocolLabel = !activePane
    ? "Sin sesión"
    : activePane.spec.kind === "local"
      ? "PTY local"
      : activePane.spec.kind === "serial" ||
          (activePane.spec.kind === "saved" &&
            activePane.spec.protocol === "serial")
        ? "Serial 8N1"
        : "SSH-2.0";

  const activeStatusLabel =
    activePane?.status === "live"
      ? "Conectado"
      : activePane?.status === "connecting"
        ? "Conectando…"
        : "Desconectado";

  const toggleBroadcast = (tabId: string) => {
    setBroadcastSel((prev) => {
      const next = new Set(prev);
      if (next.has(tabId)) next.delete(tabId);
      else next.add(tabId);
      return next;
    });
  };

  const liveBroadcastIds = getBroadcastSessionIds();

  const sendBroadcast = () => {
    const text = broadcastText;
    if (!text.trim() || liveBroadcastIds.length === 0) return;
    for (const id of liveBroadcastIds) {
      void invoke("write_to_session", { id, data: `${text}\r` });
    }
    setBroadcastText("");
  };

  const livePaneCount = tabs.reduce(
    (acc, t) => acc + t.panes.filter((p) => p.status === "live").length,
    0,
  );

  return (
    <div className="app">
      <Sidebar
        tree={tree}
        identities={identities}
        liveCount={livePaneCount}
        {...sidebarActions}
      />

      <div className="main">
        <header
          className="tabbar"
          data-tauri-drag-region
          onMouseDown={dragWindow}
        >
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
                <span className="tab-title">
                  {tab.title}
                  {tab.panes.length > 1 ? ` · ${tab.panes.length}` : ""}
                </span>
                {broadcastOpen && (
                  <button
                    type="button"
                    className={`bcast-toggle ${
                      broadcastSel.has(tab.tabId) ? "on" : ""
                    }`}
                    title="Incluir en envío múltiple"
                    onMouseDown={(e) => e.stopPropagation()}
                    onClick={() => toggleBroadcast(tab.tabId)}
                  >
                    {broadcastSel.has(tab.tabId) ? "●" : "○"}
                  </button>
                )}
                <button
                  type="button"
                  className="tab-close"
                  aria-label={`Cerrar ${tab.title}`}
                  onMouseDown={(e) => e.stopPropagation()}
                  onClick={() => closeTab(tab.tabId)}
                >
                  ×
                </button>
              </div>
            ))}
            <button
              type="button"
              className="tab-new-btn"
              aria-label="Nueva consola local"
              title="Nueva consola local (⌘T)"
              onClick={openLocal}
            >
              <Plus size={14} strokeWidth={1.5} />
            </button>
          </div>

          <div className="actions">
            {isActiveSsh && (
              <button
                type="button"
                className={`btn ghost sm ${sftpOpen ? "accent-on" : ""}`}
                title="Archivos SFTP (⌘E)"
                onClick={() => setSftpOpen((v) => !v)}
              >
                Archivos
              </button>
            )}

            <button
              type="button"
              className="btn accent sm"
              title="Nueva conexión (⌘N)"
              onClick={() => setDialog({ open: true })}
            >
              Nueva conexión
            </button>

            <div
              style={{ position: "relative" }}
              onMouseDown={(e) => e.stopPropagation()}
            >
              <button
                type="button"
                className={`btn-icon ${toolsMenuOpen ? "accent-on" : ""}`}
                aria-label="Más acciones"
                title="Más acciones"
                onClick={() => setToolsMenuOpen((v) => !v)}
              >
                <Ellipsis size={15} strokeWidth={1.5} />
              </button>
              {toolsMenuOpen && (
                <div
                  role="menu"
                  aria-label="Opciones de consola"
                  className="context-menu"
                  style={{
                    position: "absolute",
                    top: "calc(100% + 6px)",
                    right: 0,
                    width: 248,
                  }}
                >
                  <button
                    type="button"
                    role="menuitem"
                    className="context-item"
                    onClick={() => {
                      setSearchTrigger((n) => n + 1);
                      setToolsMenuOpen(false);
                    }}
                  >
                    <span>Buscar en consola</span>
                    <span className="menu-right-kbd">⌘F</span>
                  </button>
                  <button
                    type="button"
                    role="menuitem"
                    className="context-item"
                    onClick={() => {
                      splitActiveTab("vertical");
                      setToolsMenuOpen(false);
                    }}
                  >
                    <span>Dividir panel</span>
                    <span className="menu-right-kbd">⌘D</span>
                  </button>
                  <button
                    type="button"
                    role="menuitem"
                    className="context-item"
                    onClick={() => {
                      setBroadcastOpen((v) => !v);
                      setToolsMenuOpen(false);
                    }}
                  >
                    <span>Envío múltiple</span>
                    <span className="menu-right-kbd">⌘B</span>
                  </button>
                  <div className="context-sep" />
                  <button
                    type="button"
                    role="menuitem"
                    className="context-item"
                    onClick={() => {
                      setTunnelsOpen(true);
                      setToolsMenuOpen(false);
                    }}
                  >
                    <span>Túneles SSH y SOCKS5…</span>
                  </button>
                  <button
                    type="button"
                    role="menuitem"
                    className="context-item"
                    onClick={() => {
                      setTriggersOpen(true);
                      setToolsMenuOpen(false);
                    }}
                  >
                    <span>Respuestas automáticas…</span>
                  </button>
                  <button
                    type="button"
                    role="menuitem"
                    className="context-item"
                    onClick={() => {
                      sidebarActions.onRevealLogs();
                      setToolsMenuOpen(false);
                    }}
                  >
                    <span>Registros de sesión</span>
                  </button>
                  <div className="context-sep" />
                  <button
                    type="button"
                    role="menuitemcheckbox"
                    aria-checked={snippetsVisible}
                    className="context-item"
                    onClick={() => setSnippetsVisible((v) => !v)}
                  >
                    <span>Barra de snippets</span>
                    <span
                      className={`novrix-switch ${snippetsVisible ? "on" : ""}`}
                    >
                      <span className="novrix-switch-thumb" />
                    </span>
                  </button>
                  <button
                    type="button"
                    role="menuitemcheckbox"
                    aria-checked={highlightEnabled}
                    className="context-item"
                    onClick={() => setHighlightEnabled((v) => !v)}
                  >
                    <span>Resaltado en consola</span>
                    <span
                      className={`novrix-switch ${highlightEnabled ? "on" : ""}`}
                    >
                      <span className="novrix-switch-thumb" />
                    </span>
                  </button>
                  <button
                    type="button"
                    role="menuitemcheckbox"
                    aria-checked={theme === "dark"}
                    className="context-item"
                    onClick={() =>
                      setTheme((t) => (t === "dark" ? "light" : "dark"))
                    }
                  >
                    <span>Tema oscuro</span>
                    <span
                      className={`novrix-switch ${
                        theme === "dark" ? "on" : ""
                      }`}
                    >
                      <span className="novrix-switch-thumb" />
                    </span>
                  </button>
                </div>
              )}
            </div>
          </div>
        </header>

        <div className="workspace-row">
          <main className="board">
            {tabs.length === 0 && (
              <div className="empty">
                <span className="empty-logo">
                  <Logo size={44} />
                </span>
                <p className="empty-title">Novrix Console</p>
                <p className="empty-hint">
                  Presiona <kbd className="palette-kbd">⌘K</kbd> para ir a un
                  nodo, <kbd className="palette-kbd">⌘T</kbd> para una consola
                  local o <kbd className="palette-kbd">⌘N</kbd> para una nueva
                  conexión.
                </p>
              </div>
            )}
            {tabs.map((tab) => {
              const isTabActive = tab.tabId === activeId;
              return (
                <div
                  key={tab.tabId}
                  className={`tab-panes-grid split-${tab.splitDirection} ${
                    tab.panes.length > 1 ? "multi-pane" : "single-pane"
                  }`}
                  style={{ display: isTabActive ? "flex" : "none" }}
                >
                  {tab.panes.map((pane) => (
                    <TerminalView
                      key={pane.paneId}
                      spec={pane.spec}
                      active={isTabActive}
                      focusedPane={tab.activePaneId === pane.paneId}
                      theme={theme}
                      status={pane.status}
                      statusDetail={pane.statusDetail}
                      highlightEnabled={highlightEnabled}
                      expectRules={expectRules}
                      searchTrigger={searchTrigger}
                      reconnectEpoch={pane.reconnectEpoch}
                      onFocusPane={() =>
                        setTabs((prev) =>
                          prev.map((t) =>
                            t.tabId === tab.tabId
                              ? { ...t, activePaneId: pane.paneId }
                              : t,
                          ),
                        )
                      }
                      onSession={(sessionId) =>
                        patchPane(tab.tabId, pane.paneId, {
                          sessionId,
                          status: "live",
                        })
                      }
                      onError={(message) =>
                        patchPane(tab.tabId, pane.paneId, {
                          status: "dead",
                          statusDetail: message,
                        })
                      }
                      onReconnect={() => reconnectPane(tab.tabId, pane.paneId)}
                    />
                  ))}
                </div>
              );
            })}

            <div className="notices">
              {notices.map((n) => (
                <div key={n.id} className={`notice ${n.level}`}>
                  <div className="notice-body">
                    <span>{n.text}</span>
                    {n.actionLabel && n.onAction && (
                      <button
                        type="button"
                        className="btn accent sm notice-action"
                        onClick={n.onAction}
                      >
                        {n.actionLabel}
                      </button>
                    )}
                  </div>
                  <button
                    onClick={() => dismissNotice(n.id)}
                    aria-label="Dismiss"
                  >
                    ×
                  </button>
                </div>
              ))}
            </div>
          </main>

          {sftpOpen && (
            <SftpPanel
              sessionId={isActiveSsh ? activePaneSessionId : undefined}
              sessionTitle={activePane?.title ?? "No active SSH session"}
              onClose={() => setSftpOpen(false)}
              onNotice={pushNotice}
            />
          )}
        </div>

        {snippetsVisible && (
          <SnippetBar
            snippets={snippets}
            runMode={snippetRun}
            hasActiveSession={
              Boolean(activePaneSessionId) ||
              (broadcastOpen && liveBroadcastIds.length > 0)
            }
            onToggleRun={() => setSnippetRun((v) => !v)}
            onSend={sendSnippet}
            onSave={saveSnippet}
            onDelete={deleteSnippet}
            onClose={() => setSnippetsVisible(false)}
          />
        )}

        {broadcastOpen && (
          <div className="broadcast">
            <span className="snippets-label">Envío múltiple</span>
            <input
              autoFocus
              placeholder={
                liveBroadcastIds.length === 0
                  ? "Marca las pestañas que recibirán el comando…"
                  : `Comando para ${liveBroadcastIds.length} ${
                      liveBroadcastIds.length === 1 ? "sesión" : "sesiones"
                    }…`
              }
              value={broadcastText}
              onChange={(e) => setBroadcastText(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && sendBroadcast()}
            />
            <button
              type="button"
              className="btn accent sm"
              disabled={liveBroadcastIds.length === 0 || !broadcastText.trim()}
              onClick={sendBroadcast}
            >
              Enviar ({liveBroadcastIds.length})
            </button>
            <button
              type="button"
              className="btn-icon"
              aria-label="Cerrar envío múltiple"
              title="Ocultar barra (⌘B)"
              onClick={() => setBroadcastOpen(false)}
            >
              <X size={14} strokeWidth={1.5} />
            </button>
          </div>
        )}

        <footer className="statusbar">
          <div className="status-left">
            <span className="status-conn">
              <span className={`status-dot ${activePane?.status ?? "dead"}`} />
              {activeStatusLabel}
            </span>
            <span className="status-proto">{activeProtocolLabel}</span>
            {metrics && metrics.supported && (
              <>
                {metrics.loadAvg && (
                  <span className="status-kv">
                    Carga{" "}
                    <span className="status-val">
                      {metrics.loadAvg.replace(".", ",")}
                    </span>
                  </span>
                )}
                {metrics.memTotalMb > 0 && (
                  <span className="status-kv">
                    Memoria{" "}
                    <span className="status-val">{metrics.memPercent} %</span>
                  </span>
                )}
                {metrics.diskPercent > 0 && (
                  <span className="status-kv">
                    Disco{" "}
                    <span className="status-val">{metrics.diskPercent} %</span>
                  </span>
                )}
              </>
            )}
          </div>
          <div className="status-right">
            {activePane?.status === "live" &&
              activePane.spec.kind !== "local" && (
                <span className="status-kv">
                  Registro <span className="status-val">activo</span>
                </span>
              )}
            {activePane && (
              <span className="status-kv">
                Sesión <span className="status-val">{activePane.title}</span>
              </span>
            )}
          </div>
        </footer>
      </div>

      {dialog.open && (
        <SshDialog
          groups={tree}
          identities={identities}
          rdpAvailable={rdpAvailable}
          initial={dialog.initial}
          presetGroupId={dialog.presetGroupId}
          onCancel={() => setDialog({ open: false })}
          onConnectSsh={handleConnectSsh}
          onConnectRdp={handleConnectRdp}
          onConnectSerial={handleConnectSerial}
          onSave={handleSave}
          onInstallHint={copyInstall}
          onManageIdentities={() => setIdentitiesOpen(true)}
        />
      )}

      {identitiesOpen && (
        <IdentitiesModal
          identities={identities}
          onSave={handleSaveIdentity}
          onDelete={handleDeleteIdentity}
          onClose={() => setIdentitiesOpen(false)}
        />
      )}

      {tunnelsOpen && (
        <TunnelsModal
          sshSessions={liveSshSessions}
          defaultSessionId={isActiveSsh ? activePaneSessionId : undefined}
          onClose={() => setTunnelsOpen(false)}
          onNotice={pushNotice}
        />
      )}

      {triggersOpen && (
        <TriggersModal
          rules={expectRules}
          onSave={saveExpectRule}
          onToggle={toggleExpectRule}
          onDelete={deleteExpectRule}
          onClose={() => setTriggersOpen(false)}
        />
      )}

      {importOpen && (
        <ImportModal
          onImported={() => void loadTree()}
          onNotice={pushNotice}
          onClose={() => setImportOpen(false)}
        />
      )}

      {paletteOpen && (
        <CommandPalette
          tree={tree}
          snippets={snippets}
          highlightEnabled={highlightEnabled}
          onClose={() => setPaletteOpen(false)}
          onConnectSession={openSaved}
          onRunSnippet={sendSnippet}
          onNewLocal={openLocal}
          onNewConnection={() => setDialog({ open: true })}
          onSplitVertical={() => splitActiveTab("vertical")}
          onSplitHorizontal={() => splitActiveTab("horizontal")}
          onToggleSftp={() => setSftpOpen((v) => !v)}
          onOpenIdentities={() => setIdentitiesOpen(true)}
          onOpenTunnels={() => setTunnelsOpen(true)}
          onOpenTriggers={() => setTriggersOpen(true)}
          onOpenImport={() => setImportOpen(true)}
          onToggleHighlight={() => setHighlightEnabled((v) => !v)}
          onFindInTerminal={() => setSearchTrigger((n) => n + 1)}
          onRevealLogs={sidebarActions.onRevealLogs}
        />
      )}
    </div>
  );
}
