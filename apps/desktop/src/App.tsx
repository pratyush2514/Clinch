import { useEffect, useRef, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { Group, Panel, Separator } from "react-resizable-panels";
import { Command } from "cmdk";

type SessionStatus = { state: "cookies_imported"; count: number }
  | { state: "manual_login"; reason: string | null };
type Approval = { id: number; title: string; description: string };
type StorageStatus = { ready: boolean; cookieImportSupported: boolean };

function message(error: unknown): string {
  if (typeof error === "object" && error !== null && "code" in error) {
    if (error.code === "browser_unavailable") return "Could not open Chromium. Check CLINCH_CHROMIUM_PATH, then close and retry.";
    if (error.code === "busy") return "An operation is already in progress.";
    if (error.code === "storage_unavailable") return "Local storage is unavailable. Check app data permissions.";
    if ("message" in error && typeof error.message === "string") return error.message;
  }
  return "The operation could not finish. Please retry.";
}

export default function App() {
  const [ready, setReady] = useState(false);
  const [supported, setSupported] = useState(false);
  const [browser, setBrowser] = useState("chrome");
  const [profile, setProfile] = useState("Default");
  const [portal, setPortal] = useState("");
  const [consent, setConsent] = useState(false);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState("Connect a portal to begin.");
  const [events, setEvents] = useState<string[]>([]);
  const [approval, setApproval] = useState<Approval | null>(null);
  const [commandOpen, setCommandOpen] = useState(false);
  const dialog = useRef<HTMLDialogElement>(null);

  useEffect(() => {
    if (!isTauri()) { setStatus("Browser preview. Start the Tauri app to access local storage and session sync."); return; }
    invoke<StorageStatus>("initialize").then(result => {
      setReady(result.ready); setSupported(result.cookieImportSupported);
    }).catch(error => setStatus(message(error)));
  }, []);
  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault(); setCommandOpen(open => !open);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, []);
  useEffect(() => { if (approval) dialog.current?.showModal(); }, [approval]);

  function report(text: string) {
    setStatus(text); setEvents(previous => [...previous.slice(-19), text]);
  }
  async function connect(manual: boolean) {
    setBusy(true);
    try {
      const result = manual
        ? await invoke<SessionStatus>("manual_login", { portalUrl: portal })
        : await invoke<SessionStatus>("sync_session", { request: { browser, profile, portalUrl: portal, consent } });
      report(result.state === "cookies_imported"
        ? `Imported ${result.count} cookies. Verify the session in the Chromium window; sign in there if needed.`
        : `Manual login is ready in the app-owned Chromium profile.${result.reason ? " Reason: " + result.reason.replaceAll("_", " ") + "." : ""}`);
    } catch (error) { report(message(error)); }
    finally { setBusy(false); }
  }
  async function preview() {
    setCommandOpen(false);
    try { setApproval(await invoke<Approval>("preview_approval")); }
    catch (error) { report(message(error)); }
  }
  async function decide(approved: boolean) {
    if (!approval) return;
    try {
      const accepted = await invoke<boolean>("resolve_approval", { id: approval.id, approved });
      report(accepted ? "Dummy approval confirmed by Rust. No action was executed." : "Dummy approval rejected.");
      dialog.current?.close(); setApproval(null);
    } catch (error) { report(message(error)); }
  }
  async function closeBrowser() {
    try { await invoke("close_browser"); report("Managed Chromium closed. The app-owned profile is retained."); }
    catch (error) { report(message(error)); }
  }

  return <main>
    <header><div><strong>Clinch<span className="dot">.</span></strong><span className="subtitle">Local action studio</span></div>
      <button onClick={() => setCommandOpen(true)}>Commands <kbd>⌘ K</kbd></button></header>
    <div className="stage"><span>PHASE 0 / WEEK 1</span><span>{ready ? "SQLite ready · WAL" : "Desktop shell preview"}</span></div>
    <Group orientation="horizontal" className="workspace">
      <Panel defaultSize="55%" minSize="35%">
        <section className="pane">
          <div className="eyebrow">01 / BROWSER SESSION</div>
          <h1>Your session.<br />Your machine.</h1>
          <p>Connect one billing portal using your local browser session, or sign in directly in Clinch’s own profile.</p>
          <form onSubmit={event => { event.preventDefault(); void connect(false); }}>
            <label>Portal URL<input type="url" required placeholder="https://billing.example.com" value={portal} onChange={event => { setPortal(event.target.value); setConsent(false); }} /></label>
            <div className="fields"><label>Source browser<select value={browser} onChange={event => { setBrowser(event.target.value); setConsent(false); }}><option value="chrome">Google Chrome</option><option value="brave">Brave</option></select></label>
              <label>Profile folder<input value={profile} onChange={event => { setProfile(event.target.value); setConsent(false); }} placeholder="Default" /></label></div>
            <label className="consent"><input type="checkbox" checked={consent} onChange={event => setConsent(event.target.checked)} />
              <span>Allow Clinch to read this browser profile’s cookies for the portal above. macOS may ask for access to the browser’s Safe Storage key in Keychain. Cookies stay local and are never sent to an AI provider.</span></label>
            {!supported && <p className="notice">Cookie import requires macOS. Manual login is available in the desktop app.</p>}
            <div className="actions"><button className="primary" disabled={!ready || busy || !consent || !portal} type="submit">{busy ? "Connecting…" : "Sync session"}</button>
              <button type="button" disabled={!ready || busy || !portal} onClick={() => void connect(true)}>Sign in manually</button></div>
          </form>
          <div className="browser-note"><span className="eyebrow">MANAGED CHROMIUM</span><p>Week 1 opens an interactive Chromium window with an isolated, persistent profile. Inline viewport rendering is not part of this scaffold.</p>
            <button disabled={!ready || busy} onClick={() => void closeBrowser()}>Close managed browser</button></div>
        </section>
      </Panel>
      <Separator className="resize-handle" aria-label="Resize workspace panes" />
      <Panel minSize="25%">
        <section className="pane activity"><div className="eyebrow">02 / WORKSPACE</div><h2>Session activity</h2>
          <p role="status" aria-live="polite" className="status">{status}</p>
          <ol>{events.map((event, index) => <li key={index}><span>{String(index + 1).padStart(2, "0")}</span>{event}</li>)}</ol>
          <div className="gate-card"><div className="eyebrow">SENTINEL GATE</div><h3>A human decision.</h3><p>Test the approval round trip with a harmless preview. Invoice downloads are not gated in Phase A.</p><button disabled={!ready || !!approval} onClick={() => void preview()}>Preview dummy approval</button></div>
        </section>
      </Panel>
    </Group>
    <footer>LOCAL FIRST <span>Session sync spike · No workflow execution yet</span></footer>
    <dialog ref={dialog} onCancel={event => { event.preventDefault(); void decide(false); }} aria-labelledby="approval-title">
      <div className="eyebrow">APPROVAL REQUIRED / TEST ONLY</div><h2 id="approval-title">{approval?.title}</h2><p>{approval?.description}</p>
      <div className="actions"><button onClick={() => void decide(false)}>Reject</button><button className="primary" onClick={() => void decide(true)}>Approve dummy action</button></div>
    </dialog>
    <Command.Dialog open={commandOpen} onOpenChange={setCommandOpen} label="Clinch commands">
      <Command.Input placeholder="Find a command…" /><Command.List><Command.Empty>No matching commands.</Command.Empty>
        <Command.Item disabled={!ready} onSelect={() => void preview()}>Preview dummy approval</Command.Item>
        <Command.Item onSelect={() => { setCommandOpen(false); document.querySelector<HTMLInputElement>('input[type="url"]')?.focus(); }}>Connect a portal</Command.Item>
      </Command.List>
    </Command.Dialog>
  </main>;
}
