/**
 * The profile/folder hierarchy sidebar.
 *
 * Renders folders nested by `parentId` with their contained profiles
 * (Requirement 3.1 — folders shown in the hierarchy), lets the user select a
 * profile to edit, and exposes create-folder / new-profile / delete actions
 * that the parent `App` wires to the IPC commands.
 *
 * This component is presentational: it receives the already-loaded `folders`
 * and `profiles` and reports user intent through callbacks. Loading
 * (`list_folders` / `list_profiles`), mutation, and confirmation live in `App`.
 *
 * Branding/theme is task 22; neutral CSS-variable-driven styles only here.
 */

import { useMemo, useState } from "react";
import type { ConnectionProfile, Folder, FolderId, ProfileId } from "../ipc";

interface ProfileTreeProps {
  folders: Folder[];
  profiles: ConnectionProfile[];
  selectedProfileId: ProfileId | null;
  onSelectProfile: (id: ProfileId) => void;
  /** Create a new profile; `folderId` is the folder to place it under (or root). */
  onNewProfile: (folderId: FolderId | null) => void;
  /** Create a subfolder under `parentId` (null = root). */
  onCreateFolder: (parentId: FolderId | null) => void;
  onDeleteProfile: (id: ProfileId) => void;
  onDeleteFolder: (id: FolderId) => void;
  /** Open a session (new tab) for a profile (Requirement 9.1). */
  onOpenSession: (id: ProfileId) => void;
}

/** Index folders by parent id so the tree can be built top-down. */
function groupFoldersByParent(folders: Folder[]): Map<FolderId | null, Folder[]> {
  const byParent = new Map<FolderId | null, Folder[]>();
  for (const folder of folders) {
    const key = folder.parentId ?? null;
    const bucket = byParent.get(key);
    if (bucket) bucket.push(folder);
    else byParent.set(key, [folder]);
  }
  for (const bucket of byParent.values()) {
    bucket.sort((a, b) => a.name.localeCompare(b.name));
  }
  return byParent;
}

/** Index profiles by their owning folder id (null = root). */
function groupProfilesByFolder(
  profiles: ConnectionProfile[],
): Map<FolderId | null, ConnectionProfile[]> {
  const byFolder = new Map<FolderId | null, ConnectionProfile[]>();
  for (const profile of profiles) {
    const key = profile.folderId ?? null;
    const bucket = byFolder.get(key);
    if (bucket) bucket.push(profile);
    else byFolder.set(key, [profile]);
  }
  for (const bucket of byFolder.values()) {
    bucket.sort((a, b) => a.name.localeCompare(b.name));
  }
  return byFolder;
}

function ProfileTree(props: ProfileTreeProps) {
  const {
    folders,
    profiles,
    selectedProfileId,
    onSelectProfile,
    onNewProfile,
    onCreateFolder,
    onDeleteProfile,
    onDeleteFolder,
    onOpenSession,
  } = props;

  const foldersByParent = useMemo(
    () => groupFoldersByParent(folders),
    [folders],
  );
  const profilesByFolder = useMemo(
    () => groupProfilesByFolder(profiles),
    [profiles],
  );

  return (
    <nav className="tree" aria-label="Connections">
      <div className="tree-toolbar">
        <button
          type="button"
          className="btn btn-small"
          onClick={() => onCreateFolder(null)}
        >
          + Folder
        </button>
        <button
          type="button"
          className="btn btn-small btn-primary"
          onClick={() => onNewProfile(null)}
        >
          + Profile
        </button>
      </div>

      <ul className="tree-root" role="tree">
        <TreeLevel
          parentId={null}
          depth={0}
          foldersByParent={foldersByParent}
          profilesByFolder={profilesByFolder}
          selectedProfileId={selectedProfileId}
          onSelectProfile={onSelectProfile}
          onNewProfile={onNewProfile}
          onCreateFolder={onCreateFolder}
          onDeleteProfile={onDeleteProfile}
          onDeleteFolder={onDeleteFolder}
          onOpenSession={onOpenSession}
        />
      </ul>

      {folders.length === 0 && profiles.length === 0 ? (
        <p className="tree-empty">
          No connections yet. Create a folder or a profile to get started.
        </p>
      ) : null}
    </nav>
  );
}

interface TreeLevelProps {
  parentId: FolderId | null;
  depth: number;
  foldersByParent: Map<FolderId | null, Folder[]>;
  profilesByFolder: Map<FolderId | null, ConnectionProfile[]>;
  selectedProfileId: ProfileId | null;
  onSelectProfile: (id: ProfileId) => void;
  onNewProfile: (folderId: FolderId | null) => void;
  onCreateFolder: (parentId: FolderId | null) => void;
  onDeleteProfile: (id: ProfileId) => void;
  onDeleteFolder: (id: FolderId) => void;
  onOpenSession: (id: ProfileId) => void;
}

/** Recursively render one hierarchy level: subfolders first, then profiles. */
function TreeLevel(props: TreeLevelProps) {
  const {
    parentId,
    depth,
    foldersByParent,
    profilesByFolder,
    selectedProfileId,
    onSelectProfile,
    onNewProfile,
    onCreateFolder,
    onDeleteProfile,
    onDeleteFolder,
    onOpenSession,
  } = props;

  const childFolders = foldersByParent.get(parentId) ?? [];
  const childProfiles = profilesByFolder.get(parentId) ?? [];

  return (
    <>
      {childFolders.map((folder) => (
        <FolderNode
          key={folder.id}
          folder={folder}
          depth={depth}
          foldersByParent={foldersByParent}
          profilesByFolder={profilesByFolder}
          selectedProfileId={selectedProfileId}
          onSelectProfile={onSelectProfile}
          onNewProfile={onNewProfile}
          onCreateFolder={onCreateFolder}
          onDeleteProfile={onDeleteProfile}
          onDeleteFolder={onDeleteFolder}
          onOpenSession={onOpenSession}
        />
      ))}

      {childProfiles.map((profile) => (
        <li key={profile.id} className="tree-node" role="treeitem">
          <div
            className={
              "tree-row profile-row" +
              (profile.id === selectedProfileId ? " selected" : "")
            }
            style={{ paddingLeft: `${depth * 1.1 + 0.5}rem` }}
          >
            <button
              type="button"
              className="tree-label"
              onClick={() => onSelectProfile(profile.id)}
              title={`${profile.protocol} · ${profile.host}:${profile.port}`}
            >
              <span className="tree-icon" aria-hidden="true">
                🖥
              </span>
              <span className="tree-name">{profile.name}</span>
              <span className="tree-meta">{profile.protocol}</span>
            </button>
            <div className="tree-row-actions">
              <button
                type="button"
                className="tree-action"
                aria-label={`Open session for ${profile.name}`}
                title="Open session"
                onClick={() => onOpenSession(profile.id)}
              >
                ▶
              </button>
              <button
                type="button"
                className="tree-action"
                aria-label={`Delete profile ${profile.name}`}
                title="Delete profile"
                onClick={() => onDeleteProfile(profile.id)}
              >
                ✕
              </button>
            </div>
          </div>
        </li>
      ))}
    </>
  );
}

interface FolderNodeProps extends Omit<TreeLevelProps, "parentId"> {
  folder: Folder;
}

/** A single folder row with an expand toggle and its recursively rendered children. */
function FolderNode(props: FolderNodeProps) {
  const {
    folder,
    depth,
    foldersByParent,
    profilesByFolder,
    selectedProfileId,
    onSelectProfile,
    onNewProfile,
    onCreateFolder,
    onDeleteProfile,
    onDeleteFolder,
    onOpenSession,
  } = props;

  const [expanded, setExpanded] = useState(true);

  return (
    <li className="tree-node" role="treeitem" aria-expanded={expanded}>
      <div
        className="tree-row folder-row"
        style={{ paddingLeft: `${depth * 1.1 + 0.5}rem` }}
      >
        <button
          type="button"
          className="tree-label"
          onClick={() => setExpanded((v) => !v)}
        >
          <span className="tree-icon" aria-hidden="true">
            {expanded ? "▾" : "▸"}
          </span>
          <span className="tree-name">{folder.name}</span>
        </button>
        <div className="tree-row-actions">
          <button
            type="button"
            className="tree-action"
            aria-label={`New subfolder in ${folder.name}`}
            title="New subfolder"
            onClick={() => onCreateFolder(folder.id)}
          >
            ＋
          </button>
          <button
            type="button"
            className="tree-action"
            aria-label={`New profile in ${folder.name}`}
            title="New profile here"
            onClick={() => onNewProfile(folder.id)}
          >
            🖥
          </button>
          <button
            type="button"
            className="tree-action"
            aria-label={`Delete folder ${folder.name}`}
            title="Delete folder"
            onClick={() => onDeleteFolder(folder.id)}
          >
            ✕
          </button>
        </div>
      </div>

      {expanded ? (
        <ul className="tree-children" role="group">
          <TreeLevel
            parentId={folder.id}
            depth={depth + 1}
            foldersByParent={foldersByParent}
            profilesByFolder={profilesByFolder}
            selectedProfileId={selectedProfileId}
            onSelectProfile={onSelectProfile}
            onNewProfile={onNewProfile}
            onCreateFolder={onCreateFolder}
            onDeleteProfile={onDeleteProfile}
            onDeleteFolder={onDeleteFolder}
            onOpenSession={onOpenSession}
          />
        </ul>
      ) : null}
    </li>
  );
}

export default ProfileTree;
