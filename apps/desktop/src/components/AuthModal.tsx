import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { message } from "../lib/errors";
import type { AuthPanelState, SessionStatus } from "../lib/ipc";

/**
 * Re-authentication, raised over the thread instead of parked in the layout.
 *
 * A login or 2FA challenge is an interruption, not a permanent feature of the
 * workspace, so it appears only when the backend reports one and disappears
 * when it is resolved. Sign-in happens inside the app-owned Chromium profile —
 * no external browser window opens, and no credential passes through here. The
 * panel knows only the portal address.
 */
export default function AuthModal({
  panel,
  onResolved,
  report,
}: {
  panel: AuthPanelState;
  onResolved: () => void;
  report: (text: string) => void;
}) {
  const modal = useRef<HTMLDialogElement>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    modal.current?.showModal();
  }, []);

  async function complete() {
    setBusy(true);
    try {
      const result = await invoke<SessionStatus>("complete_embedded_auth");
      report(
        result.state === "cookies_imported"
          ? `Re-authenticated ${panel.portal}. Run your task again.`
          : "Signed in. Verify the portal, then run your task again.",
      );
      onResolved();
    } catch (error) {
      report(`${message(error)} Finish signing in in the managed Chromium window first.`);
    } finally {
      setBusy(false);
    }
  }

  async function cancel() {
    try {
      await invoke("cancel_embedded_auth");
    } catch (error) {
      report(message(error));
    }
    onResolved();
  }

  return (
    <dialog
      ref={modal}
      className="auth-modal"
      aria-label="Embedded sign-in"
      // Dismissing without signing in leaves the session untouched, so Escape
      // is safe here and maps to the same path as the button.
      onCancel={event => {
        event.preventDefault();
        void cancel();
      }}
    >
      <div className="eyebrow">EMBEDDED SIGN-IN</div>
      <h3>This portal needs a fresh session</h3>
      <p>
        <code>{panel.portal}</code> raised a {panel.reason.replaceAll("_", " ")} challenge. Complete
        the sign-in in Clinch’s managed Chromium window, then continue — cookies stay on this
        machine and are never sent to an AI provider.
      </p>
      <div className="actions">
        <button className="primary" type="button" disabled={busy} onClick={() => void complete()}>
          {busy ? "Verifying…" : "I’ve signed in — Continue"}
        </button>
        <button type="button" disabled={busy} onClick={() => void cancel()}>
          Dismiss
        </button>
      </div>
    </dialog>
  );
}
