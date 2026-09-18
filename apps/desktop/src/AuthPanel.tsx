import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";

type SessionStatus =
  | { state: "cookies_imported"; count: number }
  | { state: "manual_login"; reason: string | null };

export type AuthPanelState = { portal: string; reason: string };

export default function AuthPanel({ panel, onResolved, report, errorMessage }: {
  panel: AuthPanelState;
  onResolved: () => void;
  report: (text: string) => void;
  errorMessage: (error: unknown) => string;
}) {
  const [busy, setBusy] = useState(false);

  async function complete() {
    setBusy(true);
    try {
      const result = await invoke<SessionStatus>("complete_embedded_auth");
      report(result.state === "cookies_imported"
        ? `Re-authenticated ${panel.portal}. The panel closed automatically.`
        : "Signed in. The panel closed automatically — verify the portal, then run.");
      onResolved();
    } catch (error) {
      report(`${errorMessage(error)} Finish signing in in the managed Chromium window first.`);
    } finally {
      setBusy(false);
    }
  }

  async function cancel() {
    try { await invoke("cancel_embedded_auth"); } catch (error) { report(errorMessage(error)); }
    onResolved();
  }

  return (
    <div className="auth-panel" role="dialog" aria-label="Embedded sign-in panel">
      <div className="eyebrow">EMBEDDED SIGN-IN</div>
      <h3>Session expired</h3>
      <p>
        <code>{panel.portal}</code> needs a fresh sign-in ({panel.reason.replaceAll("_", " ")}).
        Sign in inside Clinch’s managed Chromium window — no external browser opens.
      </p>
      <div className="actions">
        <button className="primary" disabled={busy} onClick={() => void complete()}>
          {busy ? "Verifying…" : "I’ve signed in — Continue"}
        </button>
        <button disabled={busy} onClick={() => void cancel()}>Dismiss</button>
      </div>
    </div>
  );
}
