import { useEffect, useMemo, useState } from "react";
import { ChevronDown, ChevronRight, Plus, Search } from "lucide-react";
import type { GroupNode, Identity, SavedSession } from "../types";
import { dragWindow } from "../drag";
import { NovrixLockup } from "./Logo";

interface Actions {
  onConnect: (s: SavedSession) => void;
  onConnectInSplit: (s: SavedSession) => void;
  onEdit: (s: SavedSession) => void;
  onNewInGroup: (groupId: string) => void;
  onNewSubgroup: (parentId: string) => void;
  onCreateGroup: (name: string, parentId?: string) => void;
  onRenameGroup: (id: string, name: string) => void;
  onSetGroupIdentity: (id: string, identityId: string | null) => void;
  onDeleteGroup: (id: string) => void;
  onDeleteSession: (id: string) => void;
  onDuplicateSession: (id: string) => void;
  onMoveGroup: (id: string, parentId: string | null) => void;
  onMoveSession: (id: string, groupId: string) => void;
  onRevealLogs: () => void;
  onOpenIdentities: () => void;
  onOpenPalette: () => void;
  onImportSshConfig: () => void;
}

interface Props extends Actions {
  tree: GroupNode[];
  identities: Identity[];
  liveCount?: number;
}

type DragItem =
  | { kind: "group"; id: string }
  | { kind: "session"; id: string };

interface DragState {
  item: DragItem | null;
  over: string | null;
}

interface ContextMenuState {
  x: number;
  y: number;
  target:
    | { kind: "session"; session: SavedSession }
    | { kind: "group"; group: GroupNode };
}

function countTree(nodes: GroupNode[]): { groups: number; sessions: number } {
  let groups = 0;
  let sessions = 0;
  for (const g of nodes) {
    groups += 1;
    sessions += g.sessions.length;
    const sub = countTree(g.children);
    groups += sub.groups;
    sessions += sub.sessions;
  }
  return { groups, sessions };
}

function GroupBranch({
  group,
  depth,
  filtering,
  collapsed,
  toggleGroup,
  editingGroup,
  editDraft,
  setEditingGroup,
  setEditDraft,
  drag,
  setDrag,
  onContextMenu,
  actions,
}: {
  group: GroupNode;
  depth: number;
  filtering: boolean;
  collapsed: Set<string>;
  toggleGroup: (id: string) => void;
  editingGroup: string | null;
  editDraft: string;
  setEditingGroup: (id: string | null) => void;
  setEditDraft: (v: string) => void;
  drag: DragState;
  setDrag: React.Dispatch<React.SetStateAction<DragState>>;
  onContextMenu: (
    e: React.MouseEvent,
    target: ContextMenuState["target"],
  ) => void;
  actions: Actions;
}) {
  const closed = !filtering && collapsed.has(group.id);
  const hasKids = group.children.length > 0 || group.sessions.length > 0;
  const submitRename = (id: string) => {
    const name = editDraft.trim();
    if (name) actions.onRenameGroup(id, name);
    setEditingGroup(null);
  };

  const dragging = drag.item;
  const isDropTarget =
    dragging !== null &&
    !(dragging.kind === "group" && dragging.id === group.id);

  const handleDrop = (e: React.DragEvent) => {
    if (!isDropTarget) return;
    e.preventDefault();
    e.stopPropagation();
    let item = drag.item;
    const raw = e.dataTransfer.getData("application/x-switchboard-item");
    if (raw) {
      try {
        item = JSON.parse(raw) as DragItem;
      } catch {
        /* keep fallback */
      }
    }
    setDrag({ item: null, over: null });
    if (!item) return;
    if (item.kind === "group") {
      if (item.id !== group.id) actions.onMoveGroup(item.id, group.id);
    } else {
      actions.onMoveSession(item.id, group.id);
    }
  };

  return (
    <div
      className={`group ${drag.over === group.id ? "drop-target" : ""}`}
      onDragEnter={(e) => {
        if (!isDropTarget) return;
        e.preventDefault();
      }}
      onDragOver={(e) => {
        if (!isDropTarget) return;
        e.preventDefault();
        e.dataTransfer.dropEffect = "move";
        if (drag.over !== group.id) setDrag((d) => ({ ...d, over: group.id }));
      }}
      onDragLeave={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node)) {
          setDrag((d) => (d.over === group.id ? { ...d, over: null } : d));
        }
      }}
      onDrop={handleDrop}
    >
      <div
        className={`group-head ${drag.over === group.id ? "drop-target" : ""}`}
        style={{ paddingLeft: 6 + depth * 14 }}
        draggable={editingGroup !== group.id}
        onClick={() => hasKids && toggleGroup(group.id)}
        onContextMenu={(e) => onContextMenu(e, { kind: "group", group })}
        onDragStart={(e) => {
          e.stopPropagation();
          const item: DragItem = { kind: "group", id: group.id };
          e.dataTransfer.effectAllowed = "move";
          e.dataTransfer.setData(
            "application/x-switchboard-item",
            JSON.stringify(item),
          );
          e.dataTransfer.setData("text/plain", group.name);
          setDrag({ item, over: null });
        }}
        onDragEnd={() => setDrag({ item: null, over: null })}
      >
        <span
          style={{
            display: "inline-flex",
            color: "var(--ink-muted)",
            visibility: hasKids ? "visible" : "hidden",
          }}
        >
          {closed ? (
            <ChevronRight size={12} strokeWidth={1.5} />
          ) : (
            <ChevronDown size={12} strokeWidth={1.5} />
          )}
        </span>

        {editingGroup === group.id ? (
          <input
            className="group-rename"
            autoFocus
            value={editDraft}
            onClick={(e) => e.stopPropagation()}
            onChange={(e) => setEditDraft(e.target.value)}
            onBlur={() => submitRename(group.id)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submitRename(group.id);
              if (e.key === "Escape") setEditingGroup(null);
            }}
          />
        ) : (
          <span
            className="group-name"
            onDoubleClick={(e) => {
              e.stopPropagation();
              setEditingGroup(group.id);
              setEditDraft(group.name);
            }}
          >
            {group.name}
          </span>
        )}

        <span className="group-count">{group.sessions.length}</span>
      </div>

      {!closed && (
        <div className="group-body">
          {group.children.map((child) => (
            <GroupBranch
              key={child.id}
              group={child}
              depth={depth + 1}
              filtering={filtering}
              collapsed={collapsed}
              toggleGroup={toggleGroup}
              editingGroup={editingGroup}
              editDraft={editDraft}
              setEditingGroup={setEditingGroup}
              setEditDraft={setEditDraft}
              drag={drag}
              setDrag={setDrag}
              onContextMenu={onContextMenu}
              actions={actions}
            />
          ))}
          {group.sessions.map((s) => (
            <div
              key={s.id}
              className="host-row"
              style={{
                paddingLeft: 20 + depth * 14,
                minHeight: 30,
                height: 30,
                paddingTop: 0,
                paddingBottom: 0,
              }}
              draggable
              onContextMenu={(e) =>
                onContextMenu(e, { kind: "session", session: s })
              }
              onDragStart={(e) => {
                e.stopPropagation();
                const item: DragItem = { kind: "session", id: s.id };
                e.dataTransfer.effectAllowed = "move";
                e.dataTransfer.setData(
                  "application/x-switchboard-item",
                  JSON.stringify(item),
                );
                e.dataTransfer.setData("text/plain", s.name);
                setDrag({ item, over: null });
              }}
              onDragEnd={() => setDrag({ item: null, over: null })}
              onClick={() => actions.onConnect(s)}
              title={`${s.username}@${s.host}:${s.port}`}
            >
              <span
                className={`host-status-dot ${s.protocol}`}
                aria-hidden="true"
              />
              <span
                className="host-name"
                style={{ fontSize: 13.5, fontWeight: 400 }}
              >
                {s.name}
              </span>
              <span className="host-ip-label">
                {s.protocol === "serial"
                  ? (s.serialPort || s.host).replace("/dev/", "")
                  : s.host}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function filterTree(nodes: GroupNode[], q: string): GroupNode[] {
  return nodes
    .map((g) => {
      const children = filterTree(g.children, q);
      const sessions = g.sessions.filter(
        (s) =>
          s.name.toLowerCase().includes(q) ||
          s.host.toLowerCase().includes(q) ||
          s.username.toLowerCase().includes(q),
      );
      if (
        children.length > 0 ||
        sessions.length > 0 ||
        g.name.toLowerCase().includes(q)
      ) {
        return { ...g, children, sessions };
      }
      return null;
    })
    .filter((g): g is GroupNode => g !== null);
}

export default function Sidebar({
  tree,
  identities,
  liveCount = 0,
  ...actions
}: Props) {
  const [filter, setFilter] = useState("");
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [editingGroup, setEditingGroup] = useState<string | null>(null);
  const [editDraft, setEditDraft] = useState("");
  const [newGroupOpen, setNewGroupOpen] = useState(false);
  const [newGroupParent, setNewGroupParent] = useState<{
    id: string;
    name: string;
  } | null>(null);
  const [newGroupName, setNewGroupName] = useState("");
  const [drag, setDrag] = useState<DragState>({ item: null, over: null });
  const [ctxMenu, setCtxMenu] = useState<ContextMenuState | null>(null);

  const q = filter.trim().toLowerCase();
  const visible = useMemo(() => (q ? filterTree(tree, q) : tree), [tree, q]);
  const totals = useMemo(() => countTree(tree), [tree]);

  useEffect(() => {
    if (!ctxMenu) return;
    const close = () => setCtxMenu(null);
    window.addEventListener("mousedown", close);
    return () => window.removeEventListener("mousedown", close);
  }, [ctxMenu]);

  const toggleGroup = (id: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const submitNewGroup = () => {
    const name = newGroupName.trim();
    if (name) {
      actions.onCreateGroup(name, newGroupParent?.id);
    }
    setNewGroupName("");
    setNewGroupParent(null);
    setNewGroupOpen(false);
  };

  const rootActive = drag.item?.kind === "group";
  const handleRootDrop = (e: React.DragEvent) => {
    e.preventDefault();
    let item = drag.item;
    const raw = e.dataTransfer.getData("application/x-switchboard-item");
    if (raw) {
      try {
        item = JSON.parse(raw) as DragItem;
      } catch {
        /* keep fallback */
      }
    }
    setDrag({ item: null, over: null });
    if (item?.kind === "group") actions.onMoveGroup(item.id, null);
  };

  const openContextMenu = (
    e: React.MouseEvent,
    target: ContextMenuState["target"],
  ) => {
    e.preventDefault();
    e.stopPropagation();
    setCtxMenu({ x: e.clientX, y: e.clientY, target });
  };

  return (
    <aside className="sidebar" aria-label="Explorador de infraestructura">
      <div
        className="sidebar-header-graphite"
        data-tauri-drag-region
        onMouseDown={dragWindow}
      >
        <NovrixLockup />
      </div>

      <div className="sidebar-section-head">
        <span className="sidebar-section-title">Infraestructura</span>
        <button
          type="button"
          className="btn-icon"
          aria-label="Nuevo grupo"
          title="Nuevo grupo"
          onClick={() => {
            setNewGroupParent(null);
            setNewGroupOpen((v) => !v);
          }}
        >
          <Plus size={14} strokeWidth={1.5} />
        </button>
      </div>

      <div className="sidebar-search">
        <Search size={13} strokeWidth={1.5} />
        <input
          placeholder="Filtrar"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
        <button
          type="button"
          className="palette-trigger-kbd"
          title="Paleta de comandos (⌘K)"
          onClick={actions.onOpenPalette}
        >
          ⌘K
        </button>
      </div>

      {newGroupOpen && (
        <div className="inline-new-group">
          <input
            autoFocus
            placeholder={
              newGroupParent
                ? `Subgrupo en ${newGroupParent.name}`
                : "Nombre del grupo"
            }
            value={newGroupName}
            onChange={(e) => setNewGroupName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submitNewGroup();
              if (e.key === "Escape") {
                setNewGroupParent(null);
                setNewGroupOpen(false);
              }
            }}
          />
          <button
            type="button"
            className="btn accent sm"
            onClick={submitNewGroup}
          >
            Crear
          </button>
        </div>
      )}

      <div className="tree" role="tree" aria-label="Nodos y sesiones">
        {visible.length === 0 && (
          <p className="tree-empty">
            {tree.length === 0
              ? "Sin sesiones guardadas."
              : "Sin coincidencias."}
          </p>
        )}
        {visible.map((group) => (
          <GroupBranch
            key={group.id}
            group={group}
            depth={0}
            filtering={q !== ""}
            collapsed={collapsed}
            toggleGroup={toggleGroup}
            editingGroup={editingGroup}
            editDraft={editDraft}
            setEditingGroup={setEditingGroup}
            setEditDraft={setEditDraft}
            drag={drag}
            setDrag={setDrag}
            onContextMenu={openContextMenu}
            actions={actions}
          />
        ))}

        {rootActive && (
          <div
            className={`root-drop ${drag.over === "__root__" ? "active" : ""}`}
            onDragEnter={(e) => e.preventDefault()}
            onDragOver={(e) => {
              e.preventDefault();
              e.dataTransfer.dropEffect = "move";
              if (drag.over !== "__root__")
                setDrag((d) => ({ ...d, over: "__root__" }));
            }}
            onDragLeave={() =>
              setDrag((d) => (d.over === "__root__" ? { ...d, over: null } : d))
            }
            onDrop={handleRootDrop}
          >
            Mover al nivel raíz
          </div>
        )}
      </div>

      {ctxMenu && (
        <div
          className="context-menu"
          style={{ top: ctxMenu.y, left: ctxMenu.x }}
          onMouseDown={(e) => e.stopPropagation()}
        >
          {ctxMenu.target.kind === "session" ? (
            (() => {
              const targetSession = ctxMenu.target.session;
              return (
                <>
                  <button
                    type="button"
                    className="context-item"
                    onClick={() => {
                      actions.onConnect(targetSession);
                      setCtxMenu(null);
                    }}
                  >
                    Conectar
                  </button>
                  {targetSession.protocol !== "rdp" && (
                    <button
                      type="button"
                      className="context-item"
                      onClick={() => {
                        actions.onConnectInSplit(targetSession);
                        setCtxMenu(null);
                      }}
                    >
                      Abrir en panel dividido
                    </button>
                  )}
                  <div className="context-sep" />
                  <button
                    type="button"
                    className="context-item"
                    onClick={() => {
                      actions.onEdit(targetSession);
                      setCtxMenu(null);
                    }}
                  >
                    Editar conexión…
                  </button>
                  <button
                    type="button"
                    className="context-item"
                    onClick={() => {
                      actions.onDuplicateSession(targetSession.id);
                      setCtxMenu(null);
                    }}
                  >
                    Clonar
                  </button>
                  <div className="context-sep" />
                  <button
                    type="button"
                    className="context-item danger"
                    onClick={() => {
                      actions.onDeleteSession(targetSession.id);
                      setCtxMenu(null);
                    }}
                  >
                    Eliminar sesión
                  </button>
                </>
              );
            })()
          ) : (
            (() => {
              const targetGroup = ctxMenu.target.group;
              return (
                <>
                  <button
                    type="button"
                    className="context-item"
                    onClick={() => {
                      actions.onNewInGroup(targetGroup.id);
                      setCtxMenu(null);
                    }}
                  >
                    Nueva conexión en {targetGroup.name}…
                  </button>
                  <button
                    type="button"
                    className="context-item"
                    onClick={() => {
                      setNewGroupParent({
                        id: targetGroup.id,
                        name: targetGroup.name,
                      });
                      setNewGroupName("");
                      setNewGroupOpen(true);
                      setCtxMenu(null);
                    }}
                  >
                    Nuevo subgrupo…
                  </button>
                  <button
                    type="button"
                    className="context-item"
                    onClick={() => {
                      setEditingGroup(targetGroup.id);
                      setEditDraft(targetGroup.name);
                      setCtxMenu(null);
                    }}
                  >
                    Renombrar
                  </button>
                  {identities.length > 0 && (
                    <>
                      <div className="context-sep" />
                      {identities.map((ident) => (
                        <button
                          type="button"
                          key={ident.id}
                          className="context-item"
                          onClick={() => {
                            actions.onSetGroupIdentity(
                              targetGroup.id,
                              targetGroup.defaultIdentityId === ident.id
                                ? null
                                : ident.id,
                            );
                            setCtxMenu(null);
                          }}
                        >
                          <span>Identidad: {ident.name}</span>
                          {targetGroup.defaultIdentityId === ident.id && (
                            <span className="menu-right-meta">Activa</span>
                          )}
                        </button>
                      ))}
                    </>
                  )}
                  {targetGroup.parentId && (
                    <button
                      type="button"
                      className="context-item"
                      onClick={() => {
                        actions.onMoveGroup(targetGroup.id, null);
                        setCtxMenu(null);
                      }}
                    >
                      Mover al nivel raíz
                    </button>
                  )}
                  <div className="context-sep" />
                  <button
                    type="button"
                    className="context-item danger"
                    onClick={() => {
                      actions.onDeleteGroup(targetGroup.id);
                      setCtxMenu(null);
                    }}
                  >
                    Eliminar grupo
                  </button>
                </>
              );
            })()
          )}
        </div>
      )}

      <footer className="sidebar-foot">
        <button
          type="button"
          className="sidebar-foot-btn"
          onClick={actions.onOpenIdentities}
        >
          <span>Identidades</span>
          <span className="ident-count-pill">{identities.length}</span>
        </button>
        <button
          type="button"
          className="sidebar-foot-btn"
          onClick={actions.onImportSshConfig}
        >
          <span>Importar sesiones</span>
        </button>
        <div className="sidebar-foot-summary">
          {totals.groups} {totals.groups === 1 ? "grupo" : "grupos"} ·{" "}
          {totals.sessions} {totals.sessions === 1 ? "sesión" : "sesiones"} ·{" "}
          {liveCount} en ejecución
        </div>
      </footer>
    </aside>
  );
}
