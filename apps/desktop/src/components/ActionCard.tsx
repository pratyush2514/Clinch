import ChallengeCard from "./ChallengeCard";
import AuthSyncCard from "./AuthSyncCard";
import OutcomeCard from "./OutcomeCard";
import ScreencastCard from "./ScreencastCard";
import SentinelGate from "./SentinelGate";
import ShortcutCard from "./ShortcutCard";
import { TIER_LABELS, type OutputChip, type ThreadEntry } from "../lib/thread";
import { needsSession, isPursuitMiss } from "../lib/errors";

const STATUS_WORDS: Record<ThreadEntry["status"], string> = {
  running: "running",
  awaiting: "awaiting approval",
  completed: "completed",
  blocked: "blocked",
  failed: "failed",
};

/**
 * One turn of the thread, top to bottom in the order it happened: what was
 * asked, where it went, what it looked like, what it paused on, what it
 * produced.
 */
export default function ActionCard({
  entry,
  live,
  liveFrame,
  headless,
  browserBusy,
  onDecide,
  onRename,
  onSave,
  onFile,
  onTakeControl,
  onLendSession,
  onSyncAuthSession,
  onForgetSession,
  onRelease,
  onConnect,
  onSaveShortcut,
  onDismissShortcut,
}: {
  entry: ThreadEntry;
  /** Whether this is the entry the managed browser is currently working for. */
  live: boolean;
  liveFrame: string | null;
  headless: boolean;
  browserBusy: boolean;
  onDecide: (approved: boolean) => void;
  onRename: (saveName: string) => void;
  onSave: () => void;
  onFile: (chip: OutputChip, reveal: boolean) => void;
  onTakeControl: (url?: string) => void;
  onLendSession: (runId: string, sourceConnectionId?: number | null) => void;
  onSyncAuthSession: (runId: string, sourceConnectionId?: number | null) => void;
  onForgetSession: (host: string) => void;
  onRelease: () => void;
  onConnect: () => void;
  onSaveShortcut: () => void;
  onDismissShortcut: () => void;
}) {
  const frame = live ? liveFrame ?? entry.frame : entry.frame;
  const showCast = live || entry.frame !== null;

  return (
    <article className={`action-card action-${entry.status}`} aria-label={entry.prompt}>
      <p className="prompt">
        <span className="prompt-who">You</span>
        {entry.prompt}
      </p>
      <div className="pills">
        <span className={`pill pill-${entry.status}`}>{STATUS_WORDS[entry.status]}</span>
        {entry.tier && <span className="pill pill-tier">{TIER_LABELS[entry.tier]}</span>}
        {entry.anchor && <span className="pill pill-anchor">🟢 Re-anchored to {entry.anchor}</span>}
      </div>
      {showCast && (
        <ScreencastCard
          frame={frame}
          live={live}
          headless={headless}
          busy={browserBusy}
          finalUrl={entry.finalUrl}
          pageTitle={entry.pageTitle}
          anchorHost={entry.anchor}
          onTakeControl={onTakeControl}
          onRelease={onRelease}
        />
      )}
      {entry.steps.length > 0 && (
        <ol className="steps" aria-label="Step progress">
          {entry.steps.map(step => (
            <li key={step.index}>
              <span>{String(step.index + 1).padStart(2, "0")}</span>
              <div>
                <strong>Step {step.index + 1}</strong>
                <small>{step.phase.replaceAll("_", " ")}</small>
              </div>
            </li>
          ))}
        </ol>
      )}
      {entry.gate && <SentinelGate gate={entry.gate} onDecide={onDecide} />}
      {entry.challenge && (
        <ChallengeCard
          url={entry.challenge}
          lendId={entry.lendId}
          busy={browserBusy}
          lend={entry.lend}
          onTakeControl={url => onTakeControl(url)}
          onLendSession={onLendSession}
          onForgetSession={onForgetSession}
        />
      )}
      {!entry.challenge && entry.authUrl && (
        <AuthSyncCard
          url={entry.authUrl}
          lendId={entry.lendId}
          busy={browserBusy}
          auth={entry.auth}
          onTakeControl={url => onTakeControl(url)}
          onSyncSession={onSyncAuthSession}
          onForgetSession={onForgetSession}
        />
      )}
      {entry.shortcutOffer && !entry.shortcutDismissed && (
        entry.shortcutSaved ? (
          <p className="target">
            🔖 Saved shortcut <strong>{entry.shortcutOffer.site}</strong> →{" "}
            <code>{entry.shortcutOffer.url}</code>. “open {entry.shortcutOffer.site}” now
            jumps straight there.
          </p>
        ) : (
          <ShortcutCard
            offer={entry.shortcutOffer}
            onSave={onSaveShortcut}
            onDismiss={onDismissShortcut}
          />
        )
      )}
      {entry.notes.length > 0 && (
        <details className="telemetry">
          <summary>Route &amp; snapshot telemetry ({entry.notes.length})</summary>
          <ol>
            {entry.notes.map((note, index) => (
              <li key={`${index}-${note}`}>
                <span>{String(index + 1).padStart(2, "0")}</span>
                <code>{note}</code>
              </li>
            ))}
          </ol>
        </details>
      )}
      {entry.error && (
        <div className="status" role="status">
          <p>{entry.error.message}</p>
          {/* Every lane needs a connected session, so this failure has exactly
              one fix — offer it here instead of leaving it to be found. */}
          {needsSession(entry.error.code) && (
            <div className="actions">
              <button className="primary" type="button" onClick={onConnect}>
                Connect a portal
              </button>
            </div>
          )}
          {/* Account-home pursuit miss: the worker journaled what it tried
              and the browser is still on the portal — hand the window over
              so the user can click the avatar themselves. */}
          {isPursuitMiss(entry.error.code, entry.error.message) && (
            <div className="actions">
              <button
                className="secondary"
                type="button"
                disabled={browserBusy}
                onClick={() => onTakeControl()}
              >
                Take control
              </button>
            </div>
          )}
        </div>
      )}
      <OutcomeCard entry={entry} onRename={onRename} onSave={onSave} onFile={onFile} />
    </article>
  );
}
