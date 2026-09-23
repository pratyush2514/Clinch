import { useCallback, useEffect, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import ActionThread from "./components/ActionThread";
import AuthModal from "./components/AuthModal";
import CommandBar from "./components/CommandBar";
import CommandPalette from "./components/CommandPalette";
import SessionDialog from "./components/SessionDialog";
import { usePlaybooks } from "./hooks/usePlaybooks";
import { useScreencast } from "./hooks/useScreencast";
import { useSiteShortcuts } from "./hooks/useSiteShortcuts";
import { useThread } from "./hooks/useThread";
import { message } from "./lib/errors";
import type { AuthPanelState, StorageStatus } from "./lib/ipc";

const SOURCE_BROWSERS = ["chrome", "brave", "edge"];

/**
 * The desktop shell: a header, one vertical Action Thread, and one prompt bar.
 *
 * Everything that is not a task — portal sessions, saved replays, browser
 * control, re-authentication — is on demand, in the ⌘K palette or a dialog it
 * opens. Nothing here occupies the window waiting to be used, which is what
 * lets a run start from typing one sentence.
 */
export default function App() {
  const [ready, setReady] = useState(false);
  const [supported, setSupported] = useState(false);
  const [defaultBrowser, setDefaultBrowser] = useState("chrome");
  // Lifted only because four session commands and the legacy save fallback need
  // an origin; no view renders it as a field in the main layout.
  const [portal, setPortal] = useState("");
  const [status, setStatus] = useState("Describe a task below to begin.");
  const [authPanel, setAuthPanel] = useState<AuthPanelState | null>(null);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [sessionOpen, setSessionOpen] = useState(false);

  const report = useCallback((text: string) => setStatus(text), []);
  const screencast = useScreencast(ready, report);
  const playbooks = usePlaybooks(ready);
  const { shortcuts, saveShortcut, deleteShortcut } = useSiteShortcuts(ready);
  const thread = useThread(screencast.ensure, portal, report);

  useEffect(() => {
    if (!isTauri()) {
      setStatus(
        "Browser preview. Start the Tauri app to access local storage and session sync.",
      );
      return;
    }
    invoke<StorageStatus>("initialize")
      .then(result => {
        // Naming the exact binary keeps a stale build unambiguous.
        setStatus(result.startupBuild);
        setReady(result.ready);
        setSupported(result.cookieImportSupported);
        // The backend preselects the most reliable source browser per OS
        // (Brave on Windows — Chrome 127+ seals its key with App-Bound
        // encryption that third-party apps cannot unwrap).
        if (SOURCE_BROWSERS.includes(result.defaultBrowser)) {
          setDefaultBrowser(result.defaultBrowser);
        }
      })
      .catch(error => setStatus(message(error)));
  }, []);

  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPaletteOpen(open => !open);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, []);

  // Frames belong to whichever entry is running, so a finished turn keeps the
  // viewport it ended on instead of mirroring a later run.
  useEffect(() => {
    if (screencast.frame) thread.noteFrame(screencast.frame);
  }, [screencast.frame, thread.noteFrame]);

  /** Re-read the embedded-auth panel; a challenge raises the modal. */
  const refreshAuth = useCallback(async () => {
    try {
      setAuthPanel(await invoke<AuthPanelState | null>("auth_status"));
    } catch {
      /* Panel state is best effort; the status line still carries the outcome. */
    }
  }, []);

  const reauthenticate = useCallback(async () => {
    try {
      setAuthPanel(await invoke<AuthPanelState>("begin_embedded_auth", { portalUrl: portal }));
      report("Embedded sign-in opened in the app-owned profile. Complete login there, then run.");
    } catch (error) {
      report(message(error));
    }
  }, [portal, report]);

  const closeBrowser = useCallback(async () => {
    try {
      await invoke("close_browser");
      report("Managed Chromium closed and the session forgotten. The profile is retained.");
    } catch (error) {
      report(message(error));
    }
  }, [report]);

  return (
    <main className="shell">
      <header>
        <div>
          <strong>
            Clinch<span className="dot">.</span>
          </strong>
          <span className="subtitle">Local action studio</span>
        </div>
        <button type="button" onClick={() => setPaletteOpen(true)}>
          Commands <kbd>⌘ K</kbd>
        </button>
      </header>
      <div className="stage">
        <span>LOCAL-FIRST · NATIVE CDP</span>
        <span>{ready ? "SQLite ready · WAL" : "Desktop shell preview"}</span>
      </div>
      <ActionThread
        entries={thread.entries}
        liveFrame={screencast.frame}
        headless={screencast.status.headless}
        browserBusy={screencast.busy}
        onDecide={(id, approved) => void thread.decide(id, approved)}
        onRename={thread.rename}
        onSave={id => void thread.save(id)}
        onFile={(id, chip, reveal) => void thread.openFile(id, chip, reveal)}
        onTakeControl={() => void screencast.takeControl()}
        onRelease={() => void screencast.release()}
        onConnect={() => setSessionOpen(true)}
      />
      <footer>
        <p className="status" role="status" aria-live="polite">
          {status}
        </p>
        <CommandBar
          ready={ready}
          running={thread.running}
          onSubmit={prompt => void thread.submit(prompt)}
        />
      </footer>
      {sessionOpen && (
        <SessionDialog
          ready={ready}
          supported={supported}
          defaultBrowser={defaultBrowser}
          portal={portal}
          setPortal={setPortal}
          onClose={() => setSessionOpen(false)}
          onConnected={() => void refreshAuth()}
          report={report}
        />
      )}
      {authPanel && (
        <AuthModal panel={authPanel} onResolved={() => setAuthPanel(null)} report={report} />
      )}
      <CommandPalette
        open={paletteOpen}
        onOpenChange={setPaletteOpen}
        ready={ready}
        portal={portal}
        attached={screencast.status.attached}
        headless={screencast.status.headless}
        playbooks={playbooks}
        shortcuts={shortcuts}
        onSaveShortcut={(name, url) => saveShortcut(name, url).then(() => {})}
        onDeleteShortcut={name => deleteShortcut(name).then(() => {})}
        onConnect={() => setSessionOpen(true)}
        onReauthenticate={() => void reauthenticate()}
        onReplayPlaybook={playbook => void thread.replayPlaybook(playbook)}
        onReplayMacro={workflow => void thread.replayMacro(workflow, portal)}
        onTakeControl={() => void screencast.takeControl()}
        onRelease={() => void screencast.release()}
        onCloseBrowser={() => void closeBrowser()}
      />
    </main>
  );
}
