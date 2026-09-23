/**
 * The human-verification handoff: the run completed but landed on a
 * bot-mitigation interstitial (Cloudflare / Turnstile / reCAPTCHA) instead
 * of the destination. No launch flag defeats that gate — a headless,
 * CDP-driven Chromium is always distinguishable — so the check goes to the
 * one party the gate trusts: the user. Taking control opens a headed
 * window on the same profile, already on the challenged page; solving it
 * once keeps the clearance in the Clinch profile for later runs.
 */
export default function ChallengeCard({
  url,
  busy,
  onTakeControl,
}: {
  url: string;
  busy: boolean;
  onTakeControl: (url: string) => void;
}) {
  let host = url;
  try {
    host = new URL(url).host;
  } catch {
    // Keep the raw value: it is display-only here, and the backend
    // re-validates before any navigation.
  }
  return (
    <div className="challenge-offer" aria-label="Human verification required">
      <p>
        🛡️ <strong>{host}</strong> asked for a human check instead of the page.
      </p>
      <p className="notice">
        Bots can&apos;t click through this — but you can. Take control to solve it once
        in a real window; the clearance stays in Clinch&apos;s profile afterwards.
      </p>
      <div className="actions">
        <button
          className="primary"
          type="button"
          disabled={busy}
          onClick={() => onTakeControl(url)}
        >
          Take control
        </button>
      </div>
    </div>
  );
}
