import type { OutputChip, ThreadEntry } from "../lib/thread";

/** Playbook names are ASCII identifiers, matching the schema's own bound. */
const NAME_PATTERN = "[A-Za-z0-9_-]+";

function statusWords(status: string): string {
  return status.replaceAll("_", " ");
}

/**
 * What the run produced and what to do with it.
 *
 * Saving is the learning loop and it only happens on this click: a run that
 * resolved by searching is slow once, and keeping it records the phrasing so
 * the same request replays straight from storage next time. Declining leaves
 * nothing behind — which is why nothing is persisted automatically.
 *
 * Only completed ad-hoc runs offer a save; stored replays are already saved and
 * unfinished runs have nothing proven to keep.
 */
export default function OutcomeCard({
  entry,
  onRename,
  onSave,
  onFile,
}: {
  entry: ThreadEntry;
  onRename: (saveName: string) => void;
  onSave: () => void;
  onFile: (chip: OutputChip, reveal: boolean) => void;
}) {
  const result = entry.result;
  if (!result) return null;
  const done = result.status === "completed";
  const files = result.chips.filter(chip => chip.kind === "file");
  const data = result.chips.filter(chip => chip.kind === "data");

  return (
    <div className="outcome" aria-label="Run outcome">
      <div className="outcome-head">
        <strong>
          {done ? "✅ COMPLETED" : `⚠ ${statusWords(result.status).toUpperCase()}`}
          {entry.elapsedMs === null ? "" : ` in ${entry.elapsedMs}ms`}
        </strong>
        <span>
          {result.completedSteps}/{result.totalSteps} steps
          {result.stoppedAt === null ? "" : ` · stopped at step ${result.stoppedAt + 1}`}
        </span>
      </div>
      {data.length > 0 && (
        <div className="chips" aria-label="Extracted data">
          {data.map((chip, index) => (
            <span className="chip" key={`${chip.label}-${index}`} title={chip.detail ?? undefined}>
              {chip.label}
            </span>
          ))}
        </div>
      )}
      {files.map(chip => (
        <div className="download-file" key={chip.fileIndex ?? chip.label}>
          <p>
            📄 {chip.label}
            <br />
            <code>{chip.detail}</code>
          </p>
          {done && (
            <div className="actions">
              <button type="button" onClick={() => onFile(chip, false)}>
                Open File
              </button>
              <button type="button" onClick={() => onFile(chip, true)}>
                Show in Folder
              </button>
            </div>
          )}
        </div>
      ))}
      {result.save &&
        (entry.savedId ? (
          <p className="target">
            Saved as a 1-click playbook (id <code>{entry.savedId}</code>). Saying “{entry.prompt}”
            again replays it instantly from SQLite.
          </p>
        ) : (
          <div className="save-row">
            <p className="notice">
              This ran by searching. Save it and “{entry.prompt}” replays instantly next time.
            </p>
            <div className="actions">
              <input
                aria-label="Playbook name"
                required
                pattern={NAME_PATTERN}
                maxLength={64}
                placeholder={result.save.suggested}
                value={entry.saveName}
                onChange={event => onRename(event.target.value)}
              />
              <button
                className="primary"
                type="button"
                disabled={!entry.saveName.trim()}
                onClick={onSave}
              >
                Save as Playbook
              </button>
            </div>
          </div>
        ))}
    </div>
  );
}
