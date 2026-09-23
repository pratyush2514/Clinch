import { useEffect } from "react";
import type { ThreadGate } from "../lib/thread";

/** How many queued controls the card itemizes before summarizing the rest. */
const VISIBLE_CANDIDATES = 10;

/**
 * The Sentinel Gate, inline in the thread rather than as a modal.
 *
 * Execution is parked in the backend until a decision arrives, so this card is
 * the whole story of the pause: what is about to happen, and — for a plural
 * batch — every control it will act on, itemized in document order. Three
 * identical "Download" labels are only distinguishable by their row evidence,
 * which is why the container fingerprint is shown rather than hidden.
 *
 * Escape rejects. The backend also denies on a five-minute timeout, so the
 * failure mode in every direction is "nothing happened".
 */
export default function SentinelGate({
  gate,
  onDecide,
}: {
  gate: ThreadGate;
  onDecide: (approved: boolean) => void;
}) {
  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        onDecide(false);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onDecide]);

  const hidden = Math.max(0, gate.candidates.length - VISIBLE_CANDIDATES);

  return (
    <div className="gate-card" role="alertdialog" aria-label="Sentinel Gate">
      <div className="eyebrow">
        🛑 SENTINEL GATE
        {gate.candidates.length > 0 ? ` · ${gate.candidates.length} CONTROLS FOUND` : ""}
      </div>
      <p>
        Step {gate.stepIndex + 1} · {gate.kind} · <code>{gate.summary}</code>. Execution is paused
        until you decide.
      </p>
      {gate.candidates.length > 0 && (
        <ol className="candidates" aria-label="Queued controls">
          {gate.candidates.slice(0, VISIBLE_CANDIDATES).map(candidate => (
            <li key={candidate.index}>
              <span>{String(candidate.index + 1).padStart(2, "0")}</span>
              <div>
                <strong>{candidate.label || "(unnamed)"}</strong>
                <small>
                  {candidate.role}
                  {candidate.container ? ` · ${candidate.container}` : ""}
                </small>
              </div>
              {candidate.isLandmark && <span className="badge">[Navigation Link]</span>}
            </li>
          ))}
        </ol>
      )}
      {hidden > 0 && <p className="notice">…and {hidden} more of the same shape.</p>}
      <div className="actions">
        <button type="button" onClick={() => onDecide(false)}>
          Reject
        </button>
        <button className="primary" type="button" onClick={() => onDecide(true)}>
          Approve &amp; Submit
        </button>
      </div>
    </div>
  );
}
