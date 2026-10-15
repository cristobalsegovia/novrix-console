/**
 * Novrix Console — application shell (tasks 21.1 and 21.2).
 *
 * Task 21.1 delivered the profile/folder tree, the profile create/edit form
 * with field-level validation, and the delete confirmations. Task 21.2 adds the
 * session tab strip and the terminal/RDP views to the main area; the
 * transfer/keys/logging/scripts panels land in task 21.3, for which SessionView
 * reserves a labelled action bar so those pieces slot in without reshaping this
 * shell.
 *
 * ## Open-sessions state
 *
 * The backend has no `list_sessions` command, so the frontend is the source of
 * truth for which tabs are open. We keep a simple `OpenSession[]` of
 * `{ id, title, kind, state }`, built from the `open_session` call that created
 * each tab and trimmed on `close_tab`. `state` is seeded from the opened
 * session and refreshed via `session_state` when it matters (on open and on
 * activate), which is enough to drive the close-confirmation rule (Req 9.7) and
 * the RDP "not connected" overlay. The active tab id is likewise tracked in the
 * frontend and mirrored to the backend through `activate_tab`.
 *
 * Branding (task 22.1): the full Novrix palette, Inter fonts, and WCAG-contrast
 * rules live in styles.css as CSS tokens, applied without reshaping components.
 * This shell adds the "N_" isotype to the header and a dark/light theme toggle
 * (the brand cyan is only safe over navy, so the light theme downshifts it to
 * #0E7490 in the stylesheet).
 */

import { useCallback, useEffect, useState } from "react";
// Brand isotype ("N_") — imported as asset URLs so Vite bundles/fingerprints
// them. The white variant sits on the dark header; the navy variant on the
// light header. The cyan underscore is baked into each SVG and never recolored.
import isotypeWhiteUrl from "./assets/branding/novrix-isotipo-blanco.svg";
import isotypeNavyUrl from "./assets/branding/novrix-isotipo-navy.svg";
import ProfileTree from "./components/ProfileTree";
import ProfileForm from "./components/ProfileForm";
import ConfirmDialog from "./components/ConfirmDialog";
import HostKeyPromptDialog from "./components/HostKeyPromptDialog";
import PromptDialog from "./components/PromptDialog";
import TabBar, { type OpenSession } from "./components/TabBar";
import SessionView from "./components/SessionView";
import KeyManager from "./components/KeyManager";
import SnippetsPanel from "./components/SnippetsPanel";
import {
  activateTab,
  asDomainError,
  closeTab,
  CONFIRMATION_REQUIRED_CODE,
  createFolder,
  deleteFolder,
  deleteProfile,
  listFolders,
  listenHostKeyPrompt,
  listProfiles,
  listSerialPorts,
  NOT_WIRED_CODE,
  openSession,
  respondHostKeyPrompt,
  selectableProtocols,
  sessionState,
  type ConnectionProfile,
  type HostKeyPromptEvent,
  type Folder,
  type FolderId,
  type ProfileId,
  type ProtocolKind,
  type SessionId,
} from "./ipc";

/** What is shown in the editor pane. */
type EditorState =
  | { kind: "empty" }
  | { kind: "new"; folderId: FolderId | null }
  | { kind: "edit"; profile: ConnectionProfile };

/** The two Novrix themes. Dark is the default (cyan accent is AAA on navy). */
type Theme = "dark" | "light";

const THEME_STORAGE_KEY = "novrix.theme";

/** Read the persisted theme, defaulting to the brand-default dark theme. */
function readStoredTheme(): Theme {
  if (typeof window === "undefined") return "dark";
  return window.localStorage.getItem(THEME_STORAGE_KEY) === "light"
    ? "light"
    : "dark";
}

/** A pending delete awaiting the user's confirmation. */
type PendingDelete =
  | { kind: "profile"; id: ProfileId; name: string }
  | { kind: "folder"; id: FolderId; name: string };

/**
 * A pending tab close awaiting confirmation. Only raised for a tab hosting an
 * active (connected) session (Requirement 9.7).
 */
interface PendingClose {
  id: SessionId;
  title: string;
}

function App() {
  const [folders, setFolders] = useState<Folder[]>([]);
  const [profiles, setProfiles] = useState<ConnectionProfile[]>([]);
  const [protocols, setProtocols] = useState<ProtocolKind[]>([]);
  const [serialPorts, setSerialPorts] = useState<string[]>([]);
  // Whether the initial profile/folder load is still in flight (Req 18.4).
  const [treeLoading, setTreeLoading] = useState(true);

  const [editor, setEditor] = useState<EditorState>({ kind: "empty" });
  const [selectedProfileId, setSelectedProfileId] = useState<ProfileId | null>(
    null,
  );

  // Open sessions (frontend-owned; the backend has no list_sessions).
  const [sessions, setSessions] = useState<OpenSession[]>([]);
  const [activeSessionId, setActiveSessionId] = useState<SessionId | null>(
    null,
  );
  const [pendingClose, setPendingClose] = useState<PendingClose | null>(null);
  const [closeBusy, setCloseBusy] = useState(false);

  // Dialog state.
  const [pendingDelete, setPendingDelete] = useState<PendingDelete | null>(
    null,
  );
  const [deleteBusy, setDeleteBusy] = useState(false);
  const [newFolderParent, setNewFolderParent] = useState<
    { parentId: FolderId | null } | null
  >(null);
  const [folderBusy, setFolderBusy] = useState(false);
  const [folderError, setFolderError] = useState<string | null>(null);

  // A top-level banner for load/op failures that are not field-specific.
  const [banner, setBanner] = useState<string | null>(null);

  // The global SSH key manager modal (task 21.3), opened from the header.
  const [keysOpen, setKeysOpen] = useState(false);
  // The command-snippets modal (task 27.5), opened from the header.
  const [snippetsOpen, setSnippetsOpen] = useState(false);

  // Host-key / certificate trust prompts (task 26.3). The backend emits
  // `hostkey://prompt` when an unknown SSH host key / RDP certificate is seen
  // under a prompting policy and suspends the connect until we answer. We queue
  // prompts (a connect could race another) and show them one at a time.
  const [hostKeyQueue, setHostKeyQueue] = useState<HostKeyPromptEvent[]>([]);
  const [hostKeyBusy, setHostKeyBusy] = useState(false);

  // Active theme (task 22). The brand cyan is only safe over navy, so the
  // palette downshifts the cyan to #0E7490 in the light theme via CSS tokens;
  // here we just flip the data-theme attribute the stylesheet keys off.
  const [theme, setTheme] = useState<Theme>(readStoredTheme);

  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    try {
      window.localStorage.setItem(THEME_STORAGE_KEY, theme);
    } catch {
      // Private-mode storage failures must not break the toggle.
    }
  }, [theme]);

  const reloadTree = useCallback(async () => {
    try {
      const [f, p] = await Promise.all([listFolders(), listProfiles()]);
      setFolders(f);
      setProfiles(p);
    } catch (err) {
      setBanner(asDomainError(err).message);
    } finally {
      setTreeLoading(false);
    }
  }, []);

  // Subscribe to host-key / certificate trust prompts for the whole app
  // (task 26.3). Each prompt is enqueued; the dialog below shows them in order.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    listenHostKeyPrompt((prompt) => {
      setHostKeyQueue((q) => [...q, prompt]);
    })
      .then((fn) => {
        if (cancelled) fn();
        else unlisten = fn;
      })
      .catch((err) => setBanner(asDomainError(err).message));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  // Answer the current host-key prompt, then dequeue it.
  const answerHostKey = useCallback(
    async (prompt: HostKeyPromptEvent, accept: boolean) => {
      setHostKeyBusy(true);
      try {
        await respondHostKeyPrompt(prompt.requestId, accept);
      } catch (err) {
        setBanner(asDomainError(err).message);
      } finally {
        setHostKeyBusy(false);
        setHostKeyQueue((q) => q.filter((p) => p.requestId !== prompt.requestId));
      }
    },
    [],
  );

  // Initial load: tree, selectable protocols (Req 8.2), serial ports (Req 6.3).
  useEffect(() => {
    void reloadTree();
    selectableProtocols()
      .then(setProtocols)
      .catch((err) => setBanner(asDomainError(err).message));
    listSerialPorts()
      .then(setSerialPorts)
      .catch((err) => setBanner(asDomainError(err).message));
  }, [reloadTree]);

  function handleSelectProfile(id: ProfileId) {
    const profile = profiles.find((p) => p.id === id);
    if (profile) {
      setSelectedProfileId(id);
      setEditor({ kind: "edit", profile });
    }
  }

  function handleNewProfile(folderId: FolderId | null) {
    setSelectedProfileId(null);
    setEditor({ kind: "new", folderId });
  }

  async function handleProfileSaved(saved: ConnectionProfile) {
    await reloadTree();
    setSelectedProfileId(saved.id);
    setEditor({ kind: "edit", profile: saved });
  }

  // ----- Sessions / tabs (Requirements 9.1, 9.5, 9.7, 10.x, 7.5/7.6) -------

  /** Refresh one session's state from the backend (best-effort). */
  const refreshSessionState = useCallback(async (id: SessionId) => {
    try {
      const state = await sessionState(id);
      if (state) {
        setSessions((prev) =>
          prev.map((s) => (s.id === id ? { ...s, state } : s)),
        );
      }
    } catch {
      // A transient state read failure must not disturb the open tabs.
    }
  }, []);

  /**
   * Open a session for a profile and add it as the active tab (Req 9.1).
   * SSH/RDP currently reject with NOT_WIRED_CODE; surface that as a clear
   * "not yet available" banner without disturbing existing tabs.
   */
  async function handleOpenSession(id: ProfileId) {
    const profile = profiles.find((p) => p.id === id);
    if (!profile) return;
    setBanner(null);
    try {
      const sessionId = await openSession(id);
      // Seed the state, then refresh from the backend for accuracy.
      const next: OpenSession = {
        id: sessionId,
        title: profile.name,
        kind: profile.protocol,
        state: "connecting",
      };
      setSessions((prev) =>
        prev.some((s) => s.id === sessionId) ? prev : [...prev, next],
      );
      setActiveSessionId(sessionId);
      // Opening a session moves focus away from the editor to the tab view.
      setEditor({ kind: "empty" });
      void refreshSessionState(sessionId);
    } catch (err) {
      const domainErr = asDomainError(err);
      if (domainErr.code === NOT_WIRED_CODE) {
        setBanner(
          `${profile.protocol.toUpperCase()} sessions are not yet available — ` +
            "the backend transport is still being wired.",
        );
      } else {
        setBanner(domainErr.message);
      }
    }
  }

  /** Activate a tab, mirroring the choice to the backend (Req 9.5). */
  async function handleActivateTab(id: SessionId) {
    setActiveSessionId(id);
    try {
      await activateTab(id);
      void refreshSessionState(id);
    } catch (err) {
      setBanner(asDomainError(err).message);
    }
  }

  /** Remove a tab from the frontend list and fix up the active id. */
  function dropSession(id: SessionId) {
    setSessions((prev) => {
      const remaining = prev.filter((s) => s.id !== id);
      setActiveSessionId((current) => {
        if (current !== id) return current;
        // Activate a neighbouring tab if any remain.
        const fallback = remaining[remaining.length - 1]?.id ?? null;
        if (fallback) void activateTab(fallback).catch(() => {});
        return fallback;
      });
      return remaining;
    });
  }

  /**
   * Request to close a tab (Req 9.7). A tab hosting an active (connected)
   * session needs confirmation first; otherwise close straight away. We also
   * handle the backend's confirmation-required code defensively — the same
   * pattern the deletes use — in case our view of the state lags.
   */
  async function handleCloseTab(id: SessionId) {
    const session = sessions.find((s) => s.id === id);
    if (!session) return;
    if (session.state === "connected") {
      setPendingClose({ id, title: session.title });
      return;
    }
    try {
      await closeTab(id, false);
      dropSession(id);
    } catch (err) {
      const domainErr = asDomainError(err);
      if (domainErr.code === CONFIRMATION_REQUIRED_CODE) {
        // Our cached state lagged; fall back to confirming.
        setPendingClose({ id, title: session.title });
        return;
      }
      setBanner(domainErr.message);
    }
  }

  /** Confirm closing an active session's tab (Req 9.7). */
  async function confirmCloseTab() {
    if (!pendingClose) return;
    setCloseBusy(true);
    setBanner(null);
    try {
      await closeTab(pendingClose.id, true);
      dropSession(pendingClose.id);
      setPendingClose(null);
    } catch (err) {
      setBanner(asDomainError(err).message);
      setPendingClose(null);
    } finally {
      setCloseBusy(false);
    }
  }

  // ----- Folder creation (Requirements 3.1, 3.2) --------------------------
  function openCreateFolder(parentId: FolderId | null) {
    setFolderError(null);
    setNewFolderParent({ parentId });
  }

  async function confirmCreateFolder(name: string) {
    if (!newFolderParent) return;
    setFolderBusy(true);
    setFolderError(null);
    try {
      await createFolder(name, newFolderParent.parentId);
      setNewFolderParent(null);
      await reloadTree();
    } catch (err) {
      // Name validation (empty / too long / duplicate) surfaces as
      // `validation.name`; show it in the dialog (Requirement 3.2).
      setFolderError(asDomainError(err).message);
    } finally {
      setFolderBusy(false);
    }
  }

  // ----- Deletion with confirmation (Requirements 2.7, 3.6) ----------------
  function requestDeleteProfile(id: ProfileId) {
    const profile = profiles.find((p) => p.id === id);
    setPendingDelete({
      kind: "profile",
      id,
      name: profile?.name ?? "this profile",
    });
  }

  function requestDeleteFolder(id: FolderId) {
    const folder = folders.find((f) => f.id === id);
    setPendingDelete({
      kind: "folder",
      id,
      name: folder?.name ?? "this folder",
    });
  }

  async function confirmDelete() {
    if (!pendingDelete) return;
    setDeleteBusy(true);
    setBanner(null);
    try {
      // The user has confirmed in the UI, so we invoke with confirmed = true.
      if (pendingDelete.kind === "profile") {
        await deleteProfile(pendingDelete.id, true);
        if (selectedProfileId === pendingDelete.id) {
          setSelectedProfileId(null);
          setEditor({ kind: "empty" });
        }
      } else {
        await deleteFolder(pendingDelete.id, true);
      }
      setPendingDelete(null);
      await reloadTree();
    } catch (err) {
      const domainErr = asDomainError(err);
      // If the backend still reports confirmation-required, keep the dialog
      // open (should not happen since we pass confirmed=true, but handle it).
      if (domainErr.code !== CONFIRMATION_REQUIRED_CODE) {
        setPendingDelete(null);
      }
      setBanner(domainErr.message);
    } finally {
      setDeleteBusy(false);
    }
  }

  return (
    <div className="layout">
      <header className="app-header">
        <span className="app-brand">
          <img
            className="app-isotype"
            src={theme === "light" ? isotypeNavyUrl : isotypeWhiteUrl}
            alt=""
            aria-hidden="true"
          />
          <span className="app-title">Novrix Console</span>
        </span>
        <div className="app-header-actions">
          <button
            type="button"
            className="btn btn-small"
            aria-pressed={theme === "light"}
            title={
              theme === "dark"
                ? "Switch to light theme"
                : "Switch to dark theme"
            }
            onClick={() =>
              setTheme((t) => (t === "dark" ? "light" : "dark"))
            }
          >
            {theme === "dark" ? "Light theme" : "Dark theme"}
          </button>
          <button
            type="button"
            className="btn btn-small"
            onClick={() => setKeysOpen(true)}
          >
            SSH keys
          </button>
          <button
            type="button"
            className="btn btn-small"
            onClick={() => setSnippetsOpen(true)}
          >
            Snippets
          </button>
        </div>
      </header>

      {banner ? (
        <div className="banner" role="alert">
          <span>{banner}</span>
          <button
            type="button"
            className="banner-dismiss"
            aria-label="Dismiss"
            onClick={() => setBanner(null)}
          >
            ✕
          </button>
        </div>
      ) : null}

      <div className="body">
        <aside className="sidebar" style={{ position: "relative" }}>
          {treeLoading ? (
            <div className="busy-overlay">
              <span className="spinner" aria-hidden="true" /> Loading…
            </div>
          ) : null}
          <ProfileTree
            folders={folders}
            profiles={profiles}
            selectedProfileId={selectedProfileId}
            onSelectProfile={handleSelectProfile}
            onNewProfile={handleNewProfile}
            onCreateFolder={openCreateFolder}
            onDeleteProfile={requestDeleteProfile}
            onDeleteFolder={requestDeleteFolder}
            onOpenSession={handleOpenSession}
          />
        </aside>

        <main className="main">
          {editor.kind !== "empty" ? (
            <div className="editor-pane">
              <ProfileForm
                key={
                  editor.kind === "edit" ? editor.profile.id : "new-profile"
                }
                selectableProtocols={protocols}
                serialPorts={serialPorts}
                editing={editor.kind === "edit" ? editor.profile : null}
                targetFolderId={
                  editor.kind === "new" ? editor.folderId : null
                }
                onSaved={handleProfileSaved}
                onCancel={() => {
                  setEditor({ kind: "empty" });
                  setSelectedProfileId(null);
                }}
              />
            </div>
          ) : sessions.length > 0 ? (
            // Session tabs + terminal/RDP views (task 21.2). Every open session
            // stays mounted so a background session keeps buffering its own
            // output independently (Requirement 9.2); only the active one is
            // visible.
            <div className="session-area">
              <TabBar
                sessions={sessions}
                activeId={activeSessionId}
                onActivate={handleActivateTab}
                onClose={handleCloseTab}
              />
              <div className="session-stage">
                {sessions.map((session) => (
                  <SessionView
                    key={session.id}
                    session={session}
                    hidden={session.id !== activeSessionId}
                    onError={setBanner}
                  />
                ))}
              </div>
            </div>
          ) : (
            // Placeholder: no editor open and no sessions. Task 21.3's panels
            // attach inside SessionView's reserved action bar, not here.
            <div className="main-placeholder">
              <p>Select a profile to edit, or open one to start a session.</p>
              <p className="main-placeholder-note">
                Use the ▶ action on a profile to open it in a tab.
              </p>
            </div>
          )}
        </main>
      </div>

      {pendingDelete ? (
        <ConfirmDialog
          title={
            pendingDelete.kind === "profile"
              ? "Delete profile"
              : "Delete folder"
          }
          message={
            pendingDelete.kind === "profile"
              ? `Delete the profile “${pendingDelete.name}”? This cannot be undone.`
              : `Delete the folder “${pendingDelete.name}” and everything it contains (nested folders and profiles)? This cannot be undone.`
          }
          busy={deleteBusy}
          onConfirm={confirmDelete}
          onCancel={() => {
            if (!deleteBusy) setPendingDelete(null);
          }}
        />
      ) : null}

      {pendingClose ? (
        <ConfirmDialog
          title="Close session"
          message={`“${pendingClose.title}” is connected. Closing its tab will disconnect the session. Continue?`}
          confirmLabel="Close session"
          busy={closeBusy}
          onConfirm={confirmCloseTab}
          onCancel={() => {
            if (!closeBusy) setPendingClose(null);
          }}
        />
      ) : null}

      {hostKeyQueue.length > 0 ? (
        <HostKeyPromptDialog
          prompt={hostKeyQueue[0]}
          busy={hostKeyBusy}
          onAccept={() => void answerHostKey(hostKeyQueue[0], true)}
          onReject={() => void answerHostKey(hostKeyQueue[0], false)}
        />
      ) : null}

      {keysOpen ? <KeyManager onClose={() => setKeysOpen(false)} /> : null}

      {snippetsOpen ? (
        <SnippetsPanel
          activeSession={(() => {
            const s = sessions.find((x) => x.id === activeSessionId);
            return s ? { id: s.id, protocol: s.kind } : null;
          })()}
          onClose={() => setSnippetsOpen(false)}
        />
      ) : null}

      {newFolderParent ? (
        <PromptDialog
          title="New folder"
          label="Folder name"
          placeholder="e.g. Production servers"
          confirmLabel="Create folder"
          busy={folderBusy}
          error={folderError}
          onConfirm={confirmCreateFolder}
          onCancel={() => {
            if (!folderBusy) {
              setNewFolderParent(null);
              setFolderError(null);
            }
          }}
        />
      ) : null}
    </div>
  );
}

export default App;
