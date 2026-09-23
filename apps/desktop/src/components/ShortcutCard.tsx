import type { ShortcutOffer } from "../lib/thread";

/**
 * The consent-gated learning loop for direct opens: after a domain-grounded
 * route actually landed, the thread offers to keep `site → url` as a named
 * shortcut. Accepting persists it via `save_site_shortcut`, so the next
 * "open {site}" answers from the shortcut rung with zero tokens.
 * Declining keeps nothing — the journal line stays as plain telemetry.
 */
export default function ShortcutCard({
  offer,
  onSave,
  onDismiss,
}: {
  offer: ShortcutOffer;
  onSave: () => void;
  onDismiss: () => void;
}) {
  return (
    <div className="shortcut-offer" aria-label="Save shortcut offer">
      <p>
        🔖 Save shortcut: <strong>{offer.site}</strong> → <code>{offer.url}</code>?
      </p>
      <p className="notice">
        Next time “open {offer.site}” jumps straight there — no grounding, no tokens.
      </p>
      <div className="actions">
        <button className="primary" type="button" onClick={onSave}>
          Save shortcut
        </button>
        <button type="button" onClick={onDismiss}>
          Not now
        </button>
      </div>
    </div>
  );
}
