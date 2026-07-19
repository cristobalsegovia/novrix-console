import { useMemo, useState } from "react";
import {
  ChevronDown,
  ChevronRight,
  Copy,
  CornerUpRight,
  FolderPlus,
  Monitor,
  Pencil,
  Plus,
  ScrollText,
  Search,
  Terminal,
  Trash2,
} from "lucide-react";
import type { GroupNode, SavedSession } from "../types";
import { dragWindow } from "../drag";
import Logo from "./Logo";

interface Actions {
  onConnect: (s: SavedSession) => void;
  onEdit: (s: SavedSession) => void;
  onNewInGroup: (groupId: string) => void;
  onNewSubgroup: (parentId: string) => void;
  onCreateGroup: (name: string, parentId?: string) => void;
  onRenameGroup: (id: string, name: string) => void;
  onDeleteGroup: (id: string) => void;
  onDeleteSession: (id: string) => void;
  onDuplicateSession: (id: string) => void;
  onMoveGroup: (id: string, parentId: string | null) => void;
  onMoveSession: (id: string, groupId: string) => void;
  onRevealLogs: () => void;
}

interface Props extends Actions {
  tree: GroupNode[];
}

/** What is currently being dragged. */
type DragItem =
  | { kind: "group"; id: string }
  | { kind: "session"; id: string };

interface DragState {
  item: DragItem | null;
  /** id of the group currently hovered as a drop target, or "__root__". */
  over: string | null;
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
  armedDelete,
  armOrFire,
  drag,
  setDrag,
  allGroups,
  moveMenu,
  setMoveMenu,
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
  armedDelete: string | null;
  armOrFire: (key: string, fire: () => void) => void;
  drag: DragState;
  setDrag: React.Dispatch<React.SetStateAction<DragState>>;
  allGroups: { id: string; path: string }[];
  moveMenu: string | null;
  setMoveMenu: (key: string | null) => void;
  actions: Actions;
}) {
  const closed = !filtering && collapsed.has(group.id);
  const hasKids = group.children.length > 0 || group.sessions.length > 0;
  const submitRename = (id: string) => {
    const name = editDraft.trim();
    if (name) actions.onRenameGroup(id, name);
    setEditingGroup(null);
  };

  // Is this group a valid drop target for whatever's being dragged?
  const dragging = drag.item;
  const isDropTarget =
    dragging !== null &&
    !(dragging.kind === "group" && dragging.id === group.id);

  const handleDrop = (e: React.DragEvent) => {
    if (!isDropTarget) return;
    e.preventDefault();
    // The innermost group handles the drop; stop it bubbling to ancestors.
    e.stopPropagation();
    // Read from dataTransfer first (survives WebView quirks), fall back to state.
    let item = drag.item;
    const raw = e.dataTransfer.getData("application/x-switchboard-item");
    if (raw) {
      try {
        item = JSON.parse(raw) as DragItem;
      } catch {
        /* keep state fallback */
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
      className="tree-group"
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
        // Only clear when the pointer actually leaves this subtree, not when
        // it crosses into a child element.
        if (!e.currentTarget.contains(e.relatedTarget as Node)) {
          setDrag((d) => (d.over === group.id ? { ...d, over: null } : d));
        }
      }}
      onDrop={handleDrop}
    >
      <div
        className={`tree-group-row ${drag.over === group.id ? "drop-into" : ""} ${
          dragging?.kind === "group" && dragging.id === group.id ? "dragging" : ""
        }`}
        style={{ paddingLeft: 8 + depth * 14 }}
        draggable={editingGroup !== group.id}
        onDragStart={(e) => {
          e.stopPropagation();
          const item: DragItem = { kind: "group", id: group.id };
          e.dataTransfer.effectAllowed = "move";
          e.dataTransfer.setData("application/x-switchboard-item", JSON.stringify(item));
          e.dataTransfer.setData("text/plain", group.name);
          setDrag({ item, over: null });
        }}
        onDragEnd={() => setDrag({ item: null, over: null })}
      >
        <button
          className="tree-caret"
          onClick={() => hasKids && toggleGroup(group.id)}
          style={{ visibility: hasKids ? "visible" : "hidden" }}
        >
          {closed ? <ChevronRight size={13} /> : <ChevronDown size={13} />}
        </button>
        {editingGroup === group.id ? (
          <input
            className="rename-input"
            autoFocus
            value={editDraft}
            onChange={(e) => setEditDraft(e.target.value)}
            onBlur={() => submitRename(group.id)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submitRename(group.id);
              if (e.key === "Escape") setEditingGroup(null);
            }}
          />
        ) : (
          <span
            className="tree-group-name"
            onDoubleClick={() => {
              setEditingGroup(group.id);
              setEditDraft(group.name);
            }}
          >
            {group.name}
          </span>
        )}
        <span className="tree-count">
          {group.sessions.length + group.children.length}
        </span>
        <span className="row-actions">
          <button
            className="btn-icon"
            title="New session here"
            onClick={() => actions.onNewInGroup(group.id)}
          >
            <Plus size={13} />
          </button>
          <button
            className="btn-icon"
            title="New subgroup"
            onClick={() => actions.onNewSubgroup(group.id)}
          >
            <FolderPlus size={12} />
          </button>
          <button
            className="btn-icon"
            title="Rename group"
            onClick={() => {
              setEditingGroup(group.id);
              setEditDraft(group.name);
            }}
          >
            <Pencil size={12} />
          </button>
          <span className="move-wrap">
            <button
              className="btn-icon"
              title="Move to…"
              onClick={() =>
                setMoveMenu(moveMenu === `g:${group.id}` ? null : `g:${group.id}`)
              }
            >
              <CornerUpRight size={12} />
            </button>
            {moveMenu === `g:${group.id}` && (
              <div className="move-menu" onMouseLeave={() => setMoveMenu(null)}>
                <button
                  className="move-item"
                  onClick={() => {
                    actions.onMoveGroup(group.id, null);
                    setMoveMenu(null);
                  }}
                >
                  Top level
                </button>
                {allGroups
                  .filter((g) => !descendantIds(group).has(g.id))
                  .map((g) => (
                    <button
                      key={g.id}
                      className="move-item"
                      onClick={() => {
                        actions.onMoveGroup(group.id, g.id);
                        setMoveMenu(null);
                      }}
                    >
                      {g.path}
                    </button>
                  ))}
              </div>
            )}
          </span>
          <button
            className={`btn-icon danger ${armedDelete === `g:${group.id}` ? "armed" : ""}`}
            title="Delete group and everything inside"
            onClick={() => armOrFire(`g:${group.id}`, () => actions.onDeleteGroup(group.id))}
          >
            <Trash2 size={12} />
          </button>
        </span>
      </div>

      {!closed && (
        <>
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
              armedDelete={armedDelete}
              armOrFire={armOrFire}
              drag={drag}
              setDrag={setDrag}
              allGroups={allGroups}
              moveMenu={moveMenu}
              setMoveMenu={setMoveMenu}
              actions={actions}
            />
          ))}
          {group.sessions.map((s) => (
            <div
              key={s.id}
              className={`tree-session ${
                dragging?.kind === "session" && dragging.id === s.id ? "dragging" : ""
              }`}
              style={{ paddingLeft: 24 + depth * 14 }}
              draggable
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
              onDoubleClick={() => actions.onConnect(s)}
              title={`${s.protocol.toUpperCase()} · ${s.username}@${s.host}:${s.port} — double-click to connect, drag to move`}
            >
              <span className="tree-proto" aria-hidden="true">
                {s.protocol === "rdp" ? <Monitor size={13} /> : <Terminal size={13} />}
              </span>
              <span className="tree-session-name">{s.name}</span>
              <span className="row-actions">
                <button className="btn-icon" title="Edit" onClick={() => actions.onEdit(s)}>
                  <Pencil size={12} />
                </button>
                <button
                  className="btn-icon"
                  title="Duplicate session"
                  onClick={() => actions.onDuplicateSession(s.id)}
                >
                  <Copy size={12} />
                </button>
                <span className="move-wrap">
                  <button
                    className="btn-icon"
                    title="Move to…"
                    onClick={() =>
                      setMoveMenu(moveMenu === `s:${s.id}` ? null : `s:${s.id}`)
                    }
                  >
                    <CornerUpRight size={12} />
                  </button>
                  {moveMenu === `s:${s.id}` && (
                    <div className="move-menu" onMouseLeave={() => setMoveMenu(null)}>
                      {allGroups
                        .filter((g) => g.id !== s.groupId)
                        .map((g) => (
                          <button
                            key={g.id}
                            className="move-item"
                            onClick={() => {
                              actions.onMoveSession(s.id, g.id);
                              setMoveMenu(null);
                            }}
                          >
                            {g.path}
                          </button>
                        ))}
                    </div>
                  )}
                </span>
                <button
                  className={`btn-icon danger ${armedDelete === `s:${s.id}` ? "armed" : ""}`}
                  title="Delete session"
                  onClick={() => armOrFire(`s:${s.id}`, () => actions.onDeleteSession(s.id))}
                >
                  <Trash2 size={12} />
                </button>
              </span>
            </div>
          ))}
        </>
      )}
    </div>
  );
}

/** Flatten the tree to (id, "Parent › Child") pairs for the "Move to" menu. */
function flattenGroups(
  nodes: GroupNode[],
  prefix = "",
): { id: string; path: string }[] {
  return nodes.flatMap((g) => {
    const path = prefix ? `${prefix} › ${g.name}` : g.name;
    return [{ id: g.id, path }, ...flattenGroups(g.children, path)];
  });
}

/** Collect a group's own id plus all descendant ids (invalid move targets). */
function descendantIds(group: GroupNode): Set<string> {
  const ids = new Set<string>([group.id]);
  const walk = (n: GroupNode) => {
    for (const c of n.children) {
      ids.add(c.id);
      walk(c);
    }
  };
  walk(group);
  return ids;
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
      if (children.length > 0 || sessions.length > 0 || g.name.toLowerCase().includes(q)) {
        return { ...g, children, sessions };
      }
      return null;
    })
    .filter((g): g is GroupNode => g !== null);
}

export default function Sidebar({ tree, ...actions }: Props) {
  const [filter, setFilter] = useState("");
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [editingGroup, setEditingGroup] = useState<string | null>(null);
  const [editDraft, setEditDraft] = useState("");
  const [newGroupOpen, setNewGroupOpen] = useState(false);
  const [newGroupName, setNewGroupName] = useState("");
  const [armedDelete, setArmedDelete] = useState<string | null>(null);
  const [drag, setDrag] = useState<DragState>({ item: null, over: null });
  const [moveMenu, setMoveMenu] = useState<string | null>(null);

  const q = filter.trim().toLowerCase();
  const visible = useMemo(() => (q ? filterTree(tree, q) : tree), [tree, q]);
  const allGroups = useMemo(() => flattenGroups(tree), [tree]);

  const toggleGroup = (id: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const armOrFire = (key: string, fire: () => void) => {
    if (armedDelete === key) {
      setArmedDelete(null);
      fire();
    } else {
      setArmedDelete(key);
      window.setTimeout(() => setArmedDelete((k) => (k === key ? null : k)), 2500);
    }
  };

  const submitNewGroup = () => {
    const name = newGroupName.trim();
    if (name) actions.onCreateGroup(name);
    setNewGroupName("");
    setNewGroupOpen(false);
  };

  // Drop onto the root zone: promote a group to top level.
  const rootActive = drag.item?.kind === "group";
  const handleRootDrop = (e: React.DragEvent) => {
    e.preventDefault();
    let item = drag.item;
    const raw = e.dataTransfer.getData("application/x-switchboard-item");
    if (raw) {
      try {
        item = JSON.parse(raw) as DragItem;
      } catch {
        /* keep state fallback */
      }
    }
    setDrag({ item: null, over: null });
    if (item?.kind === "group") actions.onMoveGroup(item.id, null);
  };

  return (
    <aside className="sidebar">
      <div className="sidebar-drag" data-tauri-drag-region onMouseDown={dragWindow} />
      <div className="sidebar-brand" onMouseDown={dragWindow}>
        <span className="brand-lockup">
          <Logo size={17} />
          <span className="brand-name">Switchboard</span>
        </span>
        <button
          className="btn-icon"
          title="New top-level group"
          onClick={() => setNewGroupOpen((v) => !v)}
        >
          <FolderPlus size={15} />
        </button>
      </div>
      <div className="sidebar-head">
        <span className="plate">Sessions</span>
      </div>

      <div className="sidebar-search">
        <Search size={13} />
        <input
          placeholder="Filter by name, host, user"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
      </div>

      {newGroupOpen && (
        <div className="inline-new-group">
          <input
            autoFocus
            placeholder="Group name (client, site…)"
            value={newGroupName}
            onChange={(e) => setNewGroupName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submitNewGroup();
              if (e.key === "Escape") setNewGroupOpen(false);
            }}
          />
          <button className="btn accent sm" onClick={submitNewGroup}>
            Add
          </button>
        </div>
      )}

      <div className="tree">
        {visible.length === 0 && (
          <p className="tree-empty">
            {tree.length === 0
              ? "No saved sessions yet. Create a group, then save hosts into it. Groups can hold subgroups too."
              : "Nothing matches the filter."}
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
            armedDelete={armedDelete}
            armOrFire={armOrFire}
            drag={drag}
            setDrag={setDrag}
            allGroups={allGroups}
            moveMenu={moveMenu}
            setMoveMenu={setMoveMenu}
            actions={actions}
          />
        ))}
        {/* keep-alive comment */}

        {/* Root drop zone: appears while dragging a group, to un-nest it. */}
        {rootActive && (
          <div
            className={`root-drop ${drag.over === "__root__" ? "active" : ""}`}
            onDragEnter={(e) => e.preventDefault()}
            onDragOver={(e) => {
              e.preventDefault();
              e.dataTransfer.dropEffect = "move";
              if (drag.over !== "__root__") setDrag((d) => ({ ...d, over: "__root__" }));
            }}
            onDragLeave={() =>
              setDrag((d) => (d.over === "__root__" ? { ...d, over: null } : d))
            }
            onDrop={handleRootDrop}
          >
            Drop here to move to top level
          </div>
        )}
      </div>

      <footer className="sidebar-foot">
        <button className="btn ghost sm" onClick={actions.onRevealLogs}>
          <ScrollText size={13} /> Session logs
        </button>
      </footer>
    </aside>
  );
}
