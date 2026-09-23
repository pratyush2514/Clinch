import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { message } from "../lib/errors";
import type { SessionStatus } from "../lib/ipc";

const SOURCE_BROWSERS = [
  { value: "chrome", label: "Google Chrome" },
  { value: "brave", label: "Brave" },
  { value: "edge", label: "Microsoft Edge" },
];

/** Why a cookie import fell back, in words the user can act on. */
function fallbackCopy(result: SessionStatus): string {
  if (result.state === "cookies_imported") {
    return `Imported ${result.count} cookies. Verify the session in the Chromium window; sign in there if needed.`;
  }
  if (result.reason === "app_bound_locked") {
    return "Chrome seals its cookies with App-Bound encryption that Clinch cannot unwrap. Switch the source browser to Brave for instant import, or sign in manually in the app-owned Chromium profile.";
  }
  const reason = result.reason ? ` Reason: ${result.reason.replaceAll("_", " ")}.` : "";
  return `Manual login is ready in the app-owned Chromium profile.${reason}`;
}

/**
 * Connecting a portal, on demand.
 *
 * These inputs used to occupy half the window permanently even though they are
 * touched once per portal. They live in a dialog now, reached from the command
 * palette or from the one failure that needs them, so the thread keeps the
 * whole surface.
 *
 * The consent checkbox is deliberately not remembered: reading a browser
 * profile's cookies is a per-portal decision, and it resets whenever the portal
 * or source changes.
 */
export default function SessionDialog({
  ready,
  supported,
  defaultBrowser,
  portal,
  setPortal,
  onClose,
  onConnected,
  report,
}: {
  ready: boolean;
  supported: boolean;
  defaultBrowser: string;
  portal: string;
  setPortal: (portal: string) => void;
  onClose: () => void;
  onConnected: () => void;
  report: (text: string) => void;
}) {
  const modal = useRef<HTMLDialogElement>(null);
  const [browser, setBrowser] = useState(defaultBrowser);
  const [profile, setProfile] = useState("Default");
  const [consent, setConsent] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    modal.current?.showModal();
  }, []);

  async function connect(connector: () => Promise<SessionStatus>) {
    setBusy(true);
    try {
      report(fallbackCopy(await connector()));
      onConnected();
      onClose();
    } catch (error) {
      report(message(error));
    } finally {
      setBusy(false);
    }
  }

  const blocked = !ready || busy || !portal;

  return (
    <dialog
      ref={modal}
      className="session-dialog"
      aria-label="Connect a portal"
      onCancel={event => {
        event.preventDefault();
        onClose();
      }}
    >
      <div className="eyebrow">PORTAL SESSION</div>
      <h3>Connect one portal</h3>
      <p>
        Reuse the session from your own browser, or sign in directly in Clinch’s profile. Cookies
        stay local and are never sent to an AI provider.
      </p>
      <form
        onSubmit={event => {
          event.preventDefault();
          void connect(() =>
            invoke<SessionStatus>("sync_session", {
              request: { browser, profile, portalUrl: portal, consent },
            }),
          );
        }}
      >
        <label>
          Portal URL
          <input
            type="url"
            required
            placeholder="https://github.com/"
            value={portal}
            onChange={event => {
              setPortal(event.target.value);
              setConsent(false);
            }}
          />
        </label>
        <div className="fields">
          <label>
            Source browser
            <select
              value={browser}
              onChange={event => {
                setBrowser(event.target.value);
                setConsent(false);
              }}
            >
              {SOURCE_BROWSERS.map(option => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </select>
          </label>
          <label>
            Profile folder
            <input
              value={profile}
              placeholder="Default"
              onChange={event => {
                setProfile(event.target.value);
                setConsent(false);
              }}
            />
          </label>
        </div>
        <label className="consent">
          <input
            type="checkbox"
            checked={consent}
            onChange={event => setConsent(event.target.checked)}
          />
          <span>
            Allow Clinch to read this browser profile’s cookies for the portal above. macOS may ask
            for access to the browser’s Safe Storage key in Keychain.
          </span>
        </label>
        {!supported && (
          <p className="notice">
            Cookie import is unavailable on this platform. Manual login still works below.
          </p>
        )}
        <div className="actions">
          <button className="primary" type="submit" disabled={blocked || !consent}>
            {busy ? "Connecting…" : "Sync session"}
          </button>
          <button
            type="button"
            disabled={blocked}
            onClick={() =>
              void connect(() => invoke<SessionStatus>("manual_login", { portalUrl: portal }))
            }
          >
            Sign in manually
          </button>
          <button
            type="button"
            disabled={blocked}
            onClick={() =>
              // The click itself scopes this sync to the portal above, and the
              // companion extension only answers that domain — no profile
              // consent is involved.
              void connect(() =>
                invoke<SessionStatus>("bridge_sync_session", { portalUrl: portal }),
              )
            }
          >
            Sync via extension
          </button>
          <button type="button" disabled={busy} onClick={onClose}>
            Cancel
          </button>
        </div>
      </form>
    </dialog>
  );
}
