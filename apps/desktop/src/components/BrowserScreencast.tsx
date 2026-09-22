import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

type ContextStatus = { attached: boolean; headless: boolean };
type ScreencastFrame = { data: string; sessionId: number };

export default function BrowserScreencast({ ready, report, errorMessage }: {
  ready: boolean; report: (text: string) => void; errorMessage: (error: unknown) => string;
}) {
  const [status, setStatus] = useState<ContextStatus | null>(null);
  const [frame, setFrame] = useState<string | null>(null);
  const [previewOpen, setPreviewOpen] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!ready) return;
    invoke<ContextStatus>("browser_context_status")
      .then(result => { setStatus(result); })
      .catch(() => { setStatus({ attached: false, headless: true }); });
  }, [ready]);

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    listen<ScreencastFrame>("browser-screencast-frame", event => {
      setFrame(event.payload.data);
    }).then(fn => { unlisten = fn; }).catch(() => {});
    return () => { unlisten?.(); };
  }, []);

  async function acquire() {
    setBusy(true);
    try {
      const next = await invoke<ContextStatus>("acquire_browser_context");
      setStatus(next);
      report("Background browser ready — headless, app-owned profile, no OS window.");
    } catch (error) { report(errorMessage(error)); }
    finally { setBusy(false); }
  }

  async function release() {
    setBusy(true);
    try {
      await invoke("release_browser_context");
      setStatus({ attached: false, headless: true });
      setFrame(null);
      report("Background browser released — no Chrome process remains.");
    } catch (error) { report(errorMessage(error)); }
    finally { setBusy(false); }
  }

  async function takeControl() {
    setBusy(true);
    try {
      const next = await invoke<ContextStatus>("take_control");
      setStatus(next);
      report("Managed Chromium is now headful — interact with its window directly.");
    } catch (error) { report(errorMessage(error)); }
    finally { setBusy(false); }
  }

  return <section className="browser-context" aria-label="Background browser">
    <div className="eyebrow">BACKGROUND BROWSER</div>
    {status?.attached
      ? <p className="status">Browser Ready · {status.headless ? "headless background session" : "headful — direct control"}.</p>
      : <p className="status">Dormant — no Chrome process. Spin up the background browser to begin.</p>}
    <div className="actions">
      {!status?.attached && <button className="primary" disabled={!ready || busy} onClick={() => void acquire()}>Spin up browser</button>}
      {status?.attached && <button disabled={!ready || busy} onClick={() => setPreviewOpen(open => !open)}>{previewOpen ? "Hide preview" : "Open preview"}</button>}
      {status?.attached && status.headless && <button className="primary" disabled={!ready || busy} onClick={() => void takeControl()}>Take Control</button>}
      {status?.attached && <button disabled={!ready || busy} onClick={() => void release()}>Release</button>}
    </div>
    {previewOpen && frame && <div className="browser-frame"><img src={`data:image/jpeg;base64,${frame}`} alt="Live background browser preview" /></div>}
  </section>;
}
