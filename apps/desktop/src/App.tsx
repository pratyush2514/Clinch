import { useEffect, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { Group, Panel, Separator } from "react-resizable-panels";
import { Command } from "cmdk";
import TaskWorkspace, { type Highlight } from "./TaskWorkspace";
import CommandBar from "./components/CommandBar";
import BrowserViewport from "./BrowserViewport";
import AuthPanel, { type AuthPanelState } from "./AuthPanel";

type SessionStatus = { state: "cookies_imported"; count: number }
  | { state: "manual_login"; reason: string | null };

type StorageStatus = { ready: boolean; cookieImportSupported: boolean; defaultBrowser: string; startupBuild: string };

function message(error: unknown): string {
  if (typeof error === "object" && error !== null && "code" in error) {
    if (error.code === "browser_unavailable") return "Could not open Chromium. Check CLINCH_CHROMIUM_PATH, then close and retry.";
    if (error.code === "busy") return "An operation is already in progress.";
    if (error.code === "storage_unavailable") return "Local storage is unavailable. Check app data permissions.";
    if (error.code === "session_required") return "Connect this portal and finish signing in in the managed browser before running tasks.";
    if (error.code === "workflow_failed") return "The workflow could not finish. Check its saved macro and the latest task checkpoint; no automatic retry was attempted.";
    if (error.code === "picker_unavailable") return "Element picking needs the visible managed Chromium window. Replays run headless — click Sync session to reopen it, then pick again.";
    if ("message" in error && typeof error.message === "string") return error.message;
  }
  return "The operation could not finish. Please retry.";
}

export default function App() {
  const [highlight, setHighlight] = useState<Highlight | null>(null);
  const [ready, setReady] = useState(false);
  const [supported, setSupported] = useState(false);
  const [browser, setBrowser] = useState("chrome");
  const [profile, setProfile] = useState("Default");
  const [portal, setPortal] = useState("");
  const [consent, setConsent] = useState(false);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState("Connect a portal to begin.");
  const [events, setEvents] = useState<string[]>([]);
  const [authPanel, setAuthPanel] = useState<AuthPanelState | null>(null);

  const [commandOpen, setCommandOpen] = useState(false);


  useEffect(() => {
    if (!isTauri()) { setStatus("Browser preview. Start the Tauri app to access local storage and session sync."); return; }
    invoke<StorageStatus>("initialize").then(result => {
      // First report of the boot lands at the top of Session Activity and
      // names the exact binary (stale builds stay unambiguous).
      report(result.startupBuild);
      setReady(result.ready); setSupported(result.cookieImportSupported);
      // The backend preselects the most reliable source browser per OS
      // (Brave on Windows — Chrome 127+ seals its key with App-Bound
      // encryption that third-party apps cannot unwrap).
      if (["chrome", "brave", "edge"].includes(result.defaultBrowser)) {
        setBrowser(result.defaultBrowser);
      }
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


  function report(text: string) {
    setStatus(text); setEvents(previous => [...previous.slice(-19), text]);
  }
  async function afterConnect(result: SessionStatus) {
    // Zero-touch reconciliation: a login/2FA challenge raises the embedded
    // in-app panel instead of opening an external OS browser window.
    try {
      const panel = await invoke<AuthPanelState | null>("auth_status");
      setAuthPanel(panel);
    } catch { /* panel state is best effort; status text below still applies */ }
    report(result.state === "cookies_imported"
      ? `Imported ${result.count} cookies. Verify the session in the Chromium window; sign in there if needed.`
      : result.reason === "app_bound_locked"
        ? "Chrome seals its cookies with App-Bound encryption that Clinch cannot unwrap. Switch the source browser to Brave for instant import, or sign in manually in the app-owned Chromium profile."
        : `Manual login is ready in the app-owned Chromium profile.${result.reason ? " Reason: " + result.reason.replaceAll("_", " ") + "." : ""}`);
  }
  async function connect(manual: boolean) {
    setBusy(true);
    try {
      const result = manual
        ? await invoke<SessionStatus>("manual_login", { portalUrl: portal })
        : await invoke<SessionStatus>("sync_session", { request: { browser, profile, portalUrl: portal, consent } });
      await afterConnect(result);
    } catch (error) { report(message(error)); }
    finally { setBusy(false); }
  }
  async function connectViaExtension() {
    setBusy(true);
    try {
      // No profile-consent checkbox needed: the click itself scopes this sync
      // to the portal above, and the extension only answers that domain.
      const result = await invoke<SessionStatus>("bridge_sync_session", { portalUrl: portal });
      await afterConnect(result);
    } catch (error) { report(message(error)); }
    finally { setBusy(false); }
  }
  async function openEmbeddedAuth() {
    setBusy(true);
    try {
      const panel = await invoke<AuthPanelState>("begin_embedded_auth", { portalUrl: portal });
      setAuthPanel(panel);
      report("Embedded sign-in opened in the app-owned profile. Complete login there, then continue.");
    } catch (error) { report(message(error)); }
    finally { setBusy(false); }
  }
  async function closeBrowser() {
    try { await invoke("close_browser"); report("Managed Chromium closed. The app-owned profile is retained."); }
    catch (error) { report(message(error)); }
  }

  return <main>
    <header><div><strong>Clinch<span className="dot">.</span></strong><span className="subtitle">Local action studio</span></div>
      <button onClick={() => setCommandOpen(true)}>Commands <kbd>⌘ K</kbd></button></header>
    <div className="stage"><span>LOCAL-FIRST · NATIVE CDP</span><span>{ready ? "SQLite ready · WAL" : "Desktop shell preview"}</span></div>
    <CommandBar ready={ready} busy={busy} portal={portal} report={report} errorMessage={message} />
    <Group orientation="horizontal" className="workspace">
      <Panel defaultSize="55%" minSize="35%">
        <section className="pane">
          <div className="eyebrow">01 / BROWSER SESSION</div>
          <h1>Your session.<br />Your machine.</h1>
          <p>Connect one portal using your local browser session, or sign in directly in Clinch’s own profile.</p>
          <BrowserViewport ready={ready} highlight={highlight} />
          <form onSubmit={event => { event.preventDefault(); void connect(false); }}>
            <label>Portal URL (optional override for runs)<input type="url" placeholder="https://github.com/account/billing/history" value={portal} onChange={event => { setPortal(event.target.value); setConsent(false); }} /></label>
            <div className="fields"><label>Source browser<select value={browser} onChange={event => { setBrowser(event.target.value); setConsent(false); }}><option value="chrome">Google Chrome</option><option value="brave">Brave</option><option value="edge">Microsoft Edge</option></select></label>
              <label>Profile folder<input value={profile} onChange={event => { setProfile(event.target.value); setConsent(false); }} placeholder="Default" /></label></div>
            <label className="consent"><input type="checkbox" checked={consent} onChange={event => setConsent(event.target.checked)} />
              <span>Allow Clinch to read this browser profile’s cookies for the portal above. macOS may ask for access to the browser’s Safe Storage key in Keychain. Cookies stay local and are never sent to an AI provider.</span></label>
            {!supported && <p className="notice">Cookie import is unavailable on this platform. Manual login is available in the desktop app.</p>}
            <div className="actions"><button className="primary" disabled={!ready || busy || !consent || !portal} type="submit">{busy ? "Connecting…" : "Sync session"}</button>
              <button type="button" disabled={!ready || busy || !portal} onClick={() => void connect(true)}>Sign in manually</button>
              <button type="button" disabled={!ready || busy || !portal} onClick={() => void connectViaExtension()}>Sync via extension</button>
              <button type="button" disabled={!ready || busy || !portal} onClick={() => void openEmbeddedAuth()}>Re-authenticate in app</button></div>
          </form>
          {authPanel && <AuthPanel panel={authPanel} onResolved={() => setAuthPanel(null)} report={report} errorMessage={message} />}

          <div className="browser-note"><span className="eyebrow">MANAGED CHROMIUM</span><p>The live preview mirrors the managed Chromium viewport. Use its separate window for manual interaction.</p>
            <button disabled={!ready || busy} onClick={() => void closeBrowser()}>Close managed browser</button></div>
        </section>
      </Panel>
      <Separator className="resize-handle" aria-label="Resize workspace panes" />
      <Panel minSize="25%">
        <section className="pane activity"><div className="eyebrow">02 / WORKSPACE</div><h2>Session activity</h2>
          <p role="status" aria-live="polite" className="status">{status}</p>
          <ol>{events.map((event, index) => <li key={index}><span>{String(index + 1).padStart(2, "0")}</span>{event}</li>)}</ol>
          <TaskWorkspace onHighlight={setHighlight} ready={ready} busy={busy} portal={portal} setBusy={setBusy} report={report} errorMessage={message} />
        </section>
      </Panel>
    </Group>
    <footer>LOCAL FIRST <span>Native CDP record / replay</span></footer>
    <Command.Dialog open={commandOpen} onOpenChange={setCommandOpen} label="Clinch commands">
      <Command.Input placeholder="Find a command…" /><Command.List><Command.Empty>No matching commands.</Command.Empty>

        <Command.Item onSelect={() => { setCommandOpen(false); document.querySelector<HTMLInputElement>('input[type="url"]')?.focus(); }}>Connect a portal</Command.Item>
      </Command.List>
    </Command.Dialog>
  </main>;
}
