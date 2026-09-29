import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { message } from "../lib/errors";

type Shortcut = { name: string; url: string };

/** User-edited destination knowledge; a fresh database has no entries. */
export default function ShortcutDialog({ onClose }: { onClose: () => void }) {
  const modal = useRef<HTMLDialogElement>(null);
  const [shortcuts, setShortcuts] = useState<Shortcut[]>([]);
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    modal.current?.showModal();
    invoke<Shortcut[]>("list_shortcuts").then(setShortcuts).catch(e => setError(message(e)));
  }, []);

  async function save() {
    setBusy(true);
    setError("");
    try {
      await invoke("save_shortcut", { name, url });
      setShortcuts(await invoke<Shortcut[]>("list_shortcuts"));
      setName("");
      setUrl("");
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(false);
    }
  }

  async function remove(name: string) {
    setBusy(true);
    setError("");
    try {
      await invoke("delete_shortcut", { name });
      setShortcuts(await invoke<Shortcut[]>("list_shortcuts"));
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <dialog ref={modal} className="session-dialog" aria-label="Site shortcuts" onCancel={e => { e.preventDefault(); onClose(); }}>
      <div className="eyebrow">DIRECT OPEN</div>
      <h3>Site shortcuts</h3>
      <p>Save a destination for a site name. “Open name” will go there directly.</p>
      <form onSubmit={e => { e.preventDefault(); void save(); }}>
        <div className="fields">
          <label>Site name<input required maxLength={64} pattern="[A-Za-z0-9-]+" value={name} onChange={e => setName(e.target.value)} placeholder="amazon" /></label>
          <label>HTTPS URL<input required type="url" value={url} onChange={e => setUrl(e.target.value)} placeholder="https://www.amazon.in/" /></label>
        </div>
        <div className="actions">
          <button className="primary" type="submit" disabled={busy}>Save shortcut</button>
          <button type="button" onClick={onClose}>Done</button>
        </div>
      </form>
      {error && <p role="alert">{error}</p>}
      <ul className="shortcut-list">
        {shortcuts.map(item => <li key={item.name}><span><strong>{item.name}</strong><small>{item.url}</small></span><button type="button" disabled={busy} onClick={() => void remove(item.name)}>Remove</button></li>)}
      </ul>
    </dialog>
  );
}
