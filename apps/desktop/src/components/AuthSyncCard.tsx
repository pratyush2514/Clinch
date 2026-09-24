import type { AuthUiState } from "../lib/thread";
import ForgetSiteButton from "./ForgetSiteButton";

/**
 * The signed-out handoff: the run completed, no human-verification gate was
 * found, but the settled page reads as logged out. For an automation tool
 * this is the whole job half-done — so the card offers to finish it.
 *
 * One tap is the consent event: Clinch pulls the site's cookies from the
 * daily browser through the Companion bridge and persists them into its own
 * browser profile — sync once, stay logged in. The transfer is one-way
 * (the daily browser is never written to), domain-scoped to this site, and
 * keeps the server's own cookie expiries. Signing out in the daily browser
 * afterwards does not sign Clinch out — this is an independent copy, which
 * is exactly why the card carries the "Forget this site" revocation.
 *
 * If the bridge isn't reachable, Take Control opens a headed window on the
 * same profile and logging in by hand persists just the same.
 */
export default function AuthSyncCard({
  url,
  lendId,
  busy,
  auth,
  onTakeControl,
  onSyncSession,
  onForgetSession,
}: {
  url: string;
  lendId: string | null;
  busy: boolean;
  auth: AuthUiState | null;
  onTakeControl: (url: string) => void;
  onSyncSession: (runId: string) => void;
  onForgetSession: (host: string) => void;
}) {
  let host = url;
  try {
    host = new URL(url).host;
  } catch {
    // Keep the raw value: display-only here; the backend resolves the real
    // page URL from its run registry, never from client input.
  }
  const working = busy || auth?.status === "busy";
  if (auth?.status === "synced") {
    return (
      <div className="auth-offer" aria-label="Session synced">
        <p>
          ✅ <strong>{host}</strong> is synced — Clinch is signed in as you.
        </p>
        <p className="notice">
          This is Clinch&apos;s own copy in its browser profile, kept with the
          site&apos;s own login lifetimes. Signing out in your daily browser
          does not sign Clinch out.
        </p>
        <ForgetSiteButton
          host={host}
          forgetting={auth.forgetting}
          onForget={onForgetSession}
        />
      </div>
    );
  }
  return (
    <div className="auth-offer" aria-label="Signed out">
      <p>
        🔑 <strong>{host}</strong> opened signed out.
      </p>
      <p className="notice">
        Sync once — Clinch keeps you logged in to {host} in its own browser
        profile. Signing out in your daily browser does not sign Clinch out.
      </p>
      <div className="actions">
        <button
          className="primary"
          type="button"
          disabled={working || !lendId}
          onClick={() => lendId && onSyncSession(lendId)}
        >
          {auth?.status === "busy" ? "Syncing…" : `Sync my ${host} session`}
        </button>
        <button type="button" disabled={working} onClick={() => onTakeControl(url)}>
          Take Control to log in
        </button>
      </div>
      {auth?.status === "failed" && auth.reason && (
        <p className="notice">{auth.reason}</p>
      )}
    </div>
  );
}
