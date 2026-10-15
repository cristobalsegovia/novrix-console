/**
 * KeyManager — SSH key management UI (task 21.3).
 *
 * A global modal (opened from the app header) that covers the Key_Manager
 * surface the backend exposes:
 *
 * - **Generate** a key pair (`generate_key`, Requirements 12.1, 12.2): pick a
 *   type (RSA / ECDSA / Ed25519) and, for the sized types, a size.
 * - **Import** an existing OpenSSH private key (`import_key`, Requirements 12.3,
 *   12.4, 12.5): read a file through a plain `<input type="file">` and pass its
 *   bytes as a `number[]`; an optional passphrase is stored securely by the
 *   core.
 * - **View / copy** a key's public component (`key_public_key`,
 *   Requirement 12.9) via the clipboard.
 * - **Delete** a key with confirmation (`delete_key`, Requirements 12.6, 12.7):
 *   the UI confirms first (ConfirmDialog), then invokes with `confirmed = true`.
 *
 * ## Frontend-owned key list
 *
 * The backend has no `list_keys` command, so this modal owns the list of
 * managed keys the same way `App` owns the open-sessions list: it seeds the
 * list from each generate / import result and trims it on delete. This means
 * the list reflects keys created in this app session; it is not a persistent
 * registry view (none is exposed by the backend yet).
 *
 * Branding is task 22; neutral token-driven styling only.
 */

import { useState } from "react";
import ConfirmDialog from "./ConfirmDialog";
import {
  asDomainError,
  deleteKey,
  generateKey,
  importKey,
  keyPublicKey,
  type KeyId,
  type KeyType,
  type ManagedKey,
} from "../ipc";

interface KeyManagerProps {
  onClose: () => void;
}

/** Default key size offered per sized key type (Ed25519 has a fixed size). */
const DEFAULT_SIZE: Record<KeyType, number> = {
  rsa: 3072,
  ecdsa: 256,
  ed25519: 256,
};

/** Size options offered per type (Ed25519's size is fixed, so no choice). */
const SIZE_OPTIONS: Record<KeyType, number[]> = {
  rsa: [2048, 3072, 4096],
  ecdsa: [256, 384, 521],
  ed25519: [256],
};

function KeyManager({ onClose }: KeyManagerProps) {
  const [keys, setKeys] = useState<ManagedKey[]>([]);

  const [keyType, setKeyType] = useState<KeyType>("ed25519");
  const [size, setSize] = useState<number>(DEFAULT_SIZE.ed25519);
  const [passphrase, setPassphrase] = useState("");

  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  // A pending delete awaiting confirmation (Requirement 12.6).
  const [pendingDelete, setPendingDelete] = useState<ManagedKey | null>(null);
  const [deleteBusy, setDeleteBusy] = useState(false);

  function onPickType(next: KeyType) {
    setKeyType(next);
    setSize(DEFAULT_SIZE[next]);
  }

  function upsertKey(key: ManagedKey) {
    setKeys((prev) =>
      prev.some((k) => k.id === key.id)
        ? prev.map((k) => (k.id === key.id ? key : k))
        : [...prev, key],
    );
  }

  async function handleGenerate() {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const key = await generateKey(keyType, size);
      upsertKey(key);
      setNotice(`Generated a new ${keyType.toUpperCase()} key.`);
    } catch (err) {
      setError(asDomainError(err).message);
    } finally {
      setBusy(false);
    }
  }

  async function handleImportFile(file: File) {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const buffer = await file.arrayBuffer();
      const bytes = Array.from(new Uint8Array(buffer));
      const key = await importKey(bytes, passphrase.trim() || null);
      upsertKey(key);
      setPassphrase("");
      setNotice(`Imported “${file.name}”.`);
    } catch (err) {
      setError(asDomainError(err).message);
    } finally {
      setBusy(false);
    }
  }

  async function handleCopyPublicKey(id: KeyId) {
    setError(null);
    setNotice(null);
    try {
      // Prefer the backend's authoritative public key; it also proves the key
      // still exists in the core (Requirement 12.9).
      const pub = await keyPublicKey(id);
      await navigator.clipboard.writeText(pub);
      setNotice("Public key copied to the clipboard.");
    } catch (err) {
      setError(asDomainError(err).message);
    }
  }

  async function confirmDelete() {
    if (!pendingDelete) return;
    setDeleteBusy(true);
    setError(null);
    setNotice(null);
    try {
      // UI already confirmed, so invoke with confirmed = true (Req 12.7).
      await deleteKey(pendingDelete.id, true);
      setKeys((prev) => prev.filter((k) => k.id !== pendingDelete.id));
      setPendingDelete(null);
      setNotice("Key deleted.");
    } catch (err) {
      setError(asDomainError(err).message);
      setPendingDelete(null);
    } finally {
      setDeleteBusy(false);
    }
  }

  const sizeOptions = SIZE_OPTIONS[keyType];
  const sizeFixed = sizeOptions.length <= 1;

  return (
    <div
      className="modal-overlay"
      role="presentation"
      onClick={() => {
        if (!busy && !deleteBusy) onClose();
      }}
    >
      <div
        className="modal modal-wide"
        role="dialog"
        aria-modal="true"
        aria-labelledby="keys-title"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="modal-header">
          <h2 id="keys-title" className="modal-title">
            SSH keys
          </h2>
          <button
            type="button"
            className="icon-btn"
            aria-label="Close"
            onClick={onClose}
            disabled={busy || deleteBusy}
          >
            ✕
          </button>
        </div>

        {error ? (
          <p className="form-error" role="alert">
            {error}
          </p>
        ) : null}
        {notice ? (
          <p className="form-notice" role="status">
            {notice}
          </p>
        ) : null}

        {/* Generate (Requirements 12.1, 12.2) */}
        <fieldset className="field-group">
          <legend>Generate a key</legend>
          <div className="field-row">
            <div className="field">
              <label htmlFor="key-type">Type</label>
              <select
                id="key-type"
                value={keyType}
                onChange={(e) => onPickType(e.target.value as KeyType)}
                disabled={busy}
              >
                <option value="ed25519">Ed25519</option>
                <option value="rsa">RSA</option>
                <option value="ecdsa">ECDSA</option>
              </select>
            </div>
            <div className="field">
              <label htmlFor="key-size">Size (bits)</label>
              <select
                id="key-size"
                value={size}
                onChange={(e) => setSize(Number(e.target.value))}
                disabled={busy || sizeFixed}
              >
                {sizeOptions.map((s) => (
                  <option key={s} value={s}>
                    {s}
                  </option>
                ))}
              </select>
            </div>
            <div className="field field-actions-inline">
              <button
                type="button"
                className="btn btn-primary"
                onClick={handleGenerate}
                disabled={busy}
              >
                Generate
              </button>
            </div>
          </div>
        </fieldset>

        {/* Import (Requirements 12.3, 12.4, 12.5) */}
        <fieldset className="field-group">
          <legend>Import a private key</legend>
          <div className="field">
            <label htmlFor="key-passphrase">Passphrase (optional)</label>
            <input
              id="key-passphrase"
              type="password"
              value={passphrase}
              placeholder="Only if the key is encrypted"
              onChange={(e) => setPassphrase(e.target.value)}
              disabled={busy}
            />
            <p className="field-hint">
              Stored securely in the OS keystore; never written to disk by the
              app.
            </p>
          </div>
          <div className="field">
            <label htmlFor="key-file">Private key file</label>
            <input
              id="key-file"
              type="file"
              disabled={busy}
              onChange={(e) => {
                const file = e.target.files?.[0];
                // Reset the input so re-selecting the same file re-fires.
                e.target.value = "";
                if (file) void handleImportFile(file);
              }}
            />
          </div>
        </fieldset>

        {/* Managed keys list + per-key actions (view/copy, delete) */}
        <div className="keys-list" aria-label="Managed keys">
          {keys.length === 0 ? (
            <p className="field-empty">
              No keys yet. Generate or import a key above.
            </p>
          ) : (
            <ul className="keys-items">
              {keys.map((key) => (
                <li key={key.id} className="keys-item">
                  <div className="keys-item-main">
                    <span className="keys-item-type">
                      {key.keyType.toUpperCase()}
                    </span>
                    <code className="keys-item-pub" title={key.publicKey}>
                      {key.publicKey}
                    </code>
                    {key.hasPassphrase ? (
                      <span className="keys-item-badge">passphrase</span>
                    ) : null}
                  </div>
                  <div className="keys-item-actions">
                    <button
                      type="button"
                      className="btn btn-small"
                      onClick={() => void handleCopyPublicKey(key.id)}
                    >
                      Copy public key
                    </button>
                    <button
                      type="button"
                      className="btn btn-small btn-danger"
                      onClick={() => setPendingDelete(key)}
                    >
                      Delete
                    </button>
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>

        <div className="modal-actions">
          <button
            type="button"
            className="btn"
            onClick={onClose}
            disabled={busy || deleteBusy}
          >
            Close
          </button>
        </div>
      </div>

      {pendingDelete ? (
        <ConfirmDialog
          title="Delete key"
          message={`Delete this ${pendingDelete.keyType.toUpperCase()} key${
            pendingDelete.hasPassphrase ? " and its stored passphrase" : ""
          }? This cannot be undone.`}
          busy={deleteBusy}
          onConfirm={confirmDelete}
          onCancel={() => {
            if (!deleteBusy) setPendingDelete(null);
          }}
        />
      ) : null}
    </div>
  );
}

export default KeyManager;
