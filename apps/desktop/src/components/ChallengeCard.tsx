import type { LendUiState } from "../lib/thread";

/**
 * The human-verification handoff: the run completed but landed on a
 * bot-mitigation gate (Cloudflare / Turnstile / reCAPTCHA) instead of the
 * destination. No launch flag defeats that gate — a headless, CDP-driven
 * Chromium is always distinguishable — so the check goes to the one party
 * the gate trusts: the user.
 *
 * Two rungs: L1.5 session lending first — one tap pulls this site's cookies
 * from the Clinch Companion extension into the managed browser, session-only
 * (never written to disk), and re-probes the gate. The tap is the consent
 * event; nothing is synced without it, and the flow is one-way (the daily
 * browser is never written to). L2 Take Control opens a headed window on
 * the same profile for solving the check by hand; solving it once keeps
 * the clearance in Clinch's profile for later runs.
 */
export default function ChallengeCard({
  url,
  runId,
  busy,
  lend,
  onTakeControl,
  onLendSession,
}: {
  url: string;
  runId: string | null;
  busy: boolean;
  lend: LendUiState | null;
  onTakeControl: (url: string) => void;
  onLendSession: (runId: string) => void;
}) {
  let host = url;
  try {
    host = new URL(url).host;
  } catch {
    // Keep the raw value: it is display-only here, and the backend
    // re-validates before any navigation.
  }
  const working = busy || lend?.status === "busy";
  if (lend?.status === "cleared") {
    return (
      <div className="challenge-offer" aria-label="Session synced">
        <p>
          ✅ <strong>{host}</strong> loaded with your synced session.
        </p>
        <p className="notice">
          The lent cookies live only in the managed browser&apos;s memory — nothing was
          written to disk or back to your daily browser.
        </p>
      </div>
    );
  }
  return (
    <div className="challenge-offer" aria-label="Human verification required">
      <p>
        🛡️ <strong>{host}</strong> asked for a human check instead of the page.
      </p>
      <p className="notice">
        Bots can&apos;t click through this — but your own session can. Sync it once from
        your daily browser, or take control to solve it by hand.
      </p>
      <div className="actions">
        <button
          className="primary"
          type="button"
          disabled={working || !runId}
          onClick={() => runId && onLendSession(runId)}
        >
          {lend?.status === "busy" ? "Syncing…" : `Sync my ${host} session`}
        </button>
        <button type="button" disabled={working} onClick={() => onTakeControl(url)}>
          Take control
        </button>
      </div>
      {lend?.status === "persistent" && lend.reason && (
        <p className="notice">{lend.reason}</p>
      )}
    </div>
  );
}
