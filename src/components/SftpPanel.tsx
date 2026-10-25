import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  ArrowUp,
  Check,
  Download,
  File,
  Folder,
  FolderPlus,
  RefreshCw,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import type { SftpDirListing, SftpEntry } from "../types";

interface Props {
  sessionId?: string;
  sessionTitle: string;
  onClose: () => void;
  onNotice: (level: "info" | "danger", text: string) => void;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const kb = bytes / 1024;
  if (kb < 1024) return `${kb.toFixed(1)} KB`;
  const mb = kb / 1024;
  if (mb < 1024) return `${mb.toFixed(1)} MB`;
  return `${(mb / 1024).toFixed(2)} GB`;
}

export default function SftpPanel({
  sessionId,
  sessionTitle,
  onClose,
  onNotice,
}: Props) {
  const [currentPath, setCurrentPath] = useState<string>(".");
  const [pathInput, setPathInput] = useState<string>(".");
  const [entries, setEntries] = useState<SftpEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [dragOver, setDragOver] = useState(false);
  const [creatingFolder, setCreatingFolder] = useState(false);
  const [newFolderName, setNewFolderName] = useState("");
  const [armedDeletePath, setArmedDeletePath] = useState<string | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);

  const loadDir = useCallback(
    async (targetPath?: string) => {
      if (!sessionId) {
        setError("Conéctate a una sesión SSH activa para explorar archivos por SFTP.");
        setEntries([]);
        return;
      }
      setLoading(true);
      setError(null);
      setArmedDeletePath(null);
      try {
        const res = await invoke<SftpDirListing>("sftp_list_dir", {
          sessionId,
          path: targetPath ?? currentPath,
        });
        setCurrentPath(res.path);
        setPathInput(res.path);
        setEntries(res.entries);
      } catch (e) {
        setError(String(e));
      } finally {
        setLoading(false);
      }
    },
    [sessionId, currentPath],
  );

  useEffect(() => {
    setCurrentPath(".");
    setPathInput(".");
    setCreatingFolder(false);
    setArmedDeletePath(null);
    if (sessionId) {
      void loadDir(".");
    } else {
      setEntries([]);
      setError("Conéctate a una sesión SSH activa para usar SFTP.");
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  const goUp = () => {
    if (!currentPath || currentPath === "/") return;
    const parts = currentPath.replace(/\/+$/, "").split("/");
    parts.pop();
    const parent = parts.join("/") || "/";
    void loadDir(parent);
  };

  const handleDownload = async (entry: SftpEntry) => {
    if (!sessionId || entry.isDir) return;
    try {
      onNotice("info", `Descargando ${entry.name}…`);
      const savedPath = await invoke<string>("sftp_download", {
        sessionId,
        remotePath: entry.path,
      });
      onNotice("info", `Guardado en ${savedPath}`);
    } catch (e) {
      onNotice("danger", `Error al descargar: ${String(e)}`);
    }
  };

  const handleUploadFiles = async (files: FileList | null) => {
    if (!files || files.length === 0 || !sessionId) return;
    for (const file of Array.from(files)) {
      try {
        onNotice("info", `Subiendo ${file.name}…`);
        const buffer = await file.arrayBuffer();
        const bytes = Array.from(new Uint8Array(buffer));
        await invoke("sftp_upload_bytes", {
          sessionId,
          remoteDir: currentPath,
          fileName: file.name,
          data: bytes,
        });
        onNotice("info", `Archivo subido: ${file.name}`);
      } catch (e) {
        onNotice("danger", `Error al subir ${file.name}: ${String(e)}`);
      }
    }
    if (fileInputRef.current) fileInputRef.current.value = "";
    void loadDir(currentPath);
  };

  const submitNewFolder = async () => {
    if (!sessionId) return;
    const trimmed = newFolderName.trim();
    if (!trimmed) {
      setCreatingFolder(false);
      return;
    }
    const target =
      currentPath === "/"
        ? `/${trimmed}`
        : `${currentPath.replace(/\/+$/, "")}/${trimmed}`;
    try {
      await invoke("sftp_mkdir", { sessionId, path: target });
      setNewFolderName("");
      setCreatingFolder(false);
      void loadDir(currentPath);
    } catch (e) {
      onNotice("danger", `No se pudo crear carpeta: ${String(e)}`);
    }
  };

  const handleDelete = async (entry: SftpEntry) => {
    if (!sessionId) return;
    if (armedDeletePath !== entry.path) {
      setArmedDeletePath(entry.path);
      return;
    }
    try {
      await invoke("sftp_delete", {
        sessionId,
        path: entry.path,
        isDir: entry.isDir,
      });
      setArmedDeletePath(null);
      void loadDir(currentPath);
    } catch (e) {
      onNotice("danger", `Error al eliminar: ${String(e)}`);
    }
  };

  return (
    <aside
      className={`sftp-drawer ${dragOver ? "drag-over" : ""}`}
      onDragOver={(e) => {
        e.preventDefault();
        e.dataTransfer.dropEffect = "copy";
        if (!dragOver) setDragOver(true);
      }}
      onDragLeave={() => setDragOver(false)}
      onDrop={(e) => {
        e.preventDefault();
        setDragOver(false);
        void handleUploadFiles(e.dataTransfer.files);
      }}
    >
      <header className="sftp-head">
        <span className="sftp-title" title={sessionTitle}>
          Archivos · {sessionTitle}
        </span>
        <div className="sftp-head-actions">
          <button
            type="button"
            className="btn-icon"
            title="Carpeta superior"
            disabled={!sessionId || currentPath === "/"}
            onClick={goUp}
          >
            <ArrowUp size={14} />
          </button>
          <button
            type="button"
            className="btn-icon"
            title="Actualizar directorio"
            disabled={!sessionId || loading}
            onClick={() => void loadDir(currentPath)}
          >
            <RefreshCw size={13} className={loading ? "spin" : ""} />
          </button>
          <button
            type="button"
            className="btn-icon"
            title="Nueva carpeta remota"
            disabled={!sessionId}
            onClick={() => {
              setCreatingFolder((v) => !v);
              setNewFolderName("");
            }}
          >
            <FolderPlus size={14} />
          </button>
          <button
            type="button"
            className="btn-icon"
            title="Subir archivos al directorio actual"
            disabled={!sessionId}
            onClick={() => fileInputRef.current?.click()}
          >
            <Upload size={14} />
          </button>
          <button
            type="button"
            className="btn-icon"
            title="Cerrar panel SFTP (⌘E)"
            onClick={onClose}
          >
            <X size={14} />
          </button>
        </div>
        <input
          ref={fileInputRef}
          type="file"
          multiple
          style={{ display: "none" }}
          onChange={(e) => void handleUploadFiles(e.target.files)}
        />
      </header>

      <div className="sftp-path-bar">
        <input
          className="sftp-path-input"
          value={pathInput}
          disabled={!sessionId}
          onChange={(e) => setPathInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && pathInput.trim()) {
              void loadDir(pathInput.trim());
            }
          }}
          placeholder="/ruta/remota"
        />
      </div>

      {creatingFolder && (
        <div
          className="sftp-path-bar"
          style={{ background: "var(--surface)", gap: 6 }}
        >
          <input
            autoFocus
            className="sftp-path-input"
            value={newFolderName}
            onChange={(e) => setNewFolderName(e.target.value)}
            placeholder="Nombre de carpeta…"
            onKeyDown={(e) => {
              if (e.key === "Enter") void submitNewFolder();
              if (e.key === "Escape") setCreatingFolder(false);
            }}
          />
          <button
            type="button"
            className="btn-icon"
            title="Crear carpeta"
            onClick={() => void submitNewFolder()}
          >
            <Check size={13} />
          </button>
          <button
            type="button"
            className="btn-icon"
            title="Cancelar"
            onClick={() => setCreatingFolder(false)}
          >
            <X size={13} />
          </button>
        </div>
      )}

      {error && (
        <div
          style={{
            margin: "8px",
            padding: "8px 10px",
            borderRadius: 6,
            background: "rgba(255, 107, 94, 0.12)",
            border: "1px solid var(--danger)",
            color: "var(--danger)",
            fontSize: 12,
          }}
        >
          {error}
        </div>
      )}

      <div className="sftp-list">
        {!error && entries.length === 0 && !loading && (
          <div className="sftp-empty">
            Directorio vacío (arrastra archivos aquí para subirlos).
          </div>
        )}
        {entries.map((entry) => (
          <div key={entry.path} className="sftp-row">
            <button
              type="button"
              className="sftp-entry-main"
              onClick={() => {
                if (entry.isDir) void loadDir(entry.path);
              }}
              onDoubleClick={() => {
                if (!entry.isDir) void handleDownload(entry);
              }}
              title={
                entry.isDir
                  ? `Abrir carpeta ${entry.name}`
                  : `Doble clic para descargar ${entry.name}`
              }
            >
              <span className={`sftp-icon ${entry.isDir ? "dir" : "file"}`}>
                {entry.isDir ? <Folder size={13} /> : <File size={13} />}
              </span>
              <span className="sftp-name">{entry.name}</span>
            </button>
            <span className="sftp-size">
              {entry.isDir ? "DIR" : formatBytes(entry.size)}
            </span>
            <span className="sftp-row-actions">
              {!entry.isDir && (
                <button
                  type="button"
                  className="btn-icon"
                  title="Descargar en ~/Downloads"
                  onClick={() => void handleDownload(entry)}
                >
                  <Download size={12} />
                </button>
              )}
              <button
                type="button"
                className={`btn-icon danger ${
                  armedDeletePath === entry.path ? "armed" : ""
                }`}
                title={
                  armedDeletePath === entry.path
                    ? "Clic de nuevo para confirmar eliminación"
                    : "Eliminar"
                }
                onClick={() => void handleDelete(entry)}
              >
                <Trash2 size={12} />
              </button>
            </span>
          </div>
        ))}
      </div>
    </aside>
  );
}
