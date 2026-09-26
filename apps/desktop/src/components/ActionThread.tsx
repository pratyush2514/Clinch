import { useEffect, useRef } from "react";
import ActionCard from "./ActionCard";
import { activeEntry, type OutputChip, type ThreadEntry } from "../lib/thread";
import type { AgentCursor, ContextStatus } from "../lib/ipc";

/**
 * The whole workspace: one vertical thread of turns in the order they happened.
 *
 * Entries are only ever appended, so scroll position means chronology. The
 * newest turn is scrolled into view as it arrives, except while a gate is
 * pending — moving the page under a decision the user is reading would be
 * hostile.
 */
export default function ActionThread({
  entries,
  liveFrame,
  agentCursor,
  headless,
  windowMode,
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
  entries: ThreadEntry[];
  liveFrame: string | null;
  /** The agent's live pointer; rendered only on the running entry's card. */
  agentCursor: AgentCursor | null;
  headless: boolean;
  windowMode: ContextStatus["windowMode"];
  browserBusy: boolean;
  onDecide: (entryId: string, approved: boolean) => void;
  onRename: (entryId: string, saveName: string) => void;
  onSave: (entryId: string) => void;
  onFile: (entryId: string, chip: OutputChip, reveal: boolean) => void;
  onTakeControl: (url?: string) => void;
  onLendSession: (entryId: string, runId: string, sourceConnectionId?: number | null) => void;
  onSyncAuthSession: (entryId: string, runId: string, sourceConnectionId?: number | null) => void;
  onForgetSession: (entryId: string, host: string) => void;
  onRelease: () => void;
  onConnect: () => void;
  onSaveShortcut: (entryId: string) => void;
  onDismissShortcut: (entryId: string) => void;
}) {
  const end = useRef<HTMLDivElement>(null);
  const live = activeEntry(entries);
  const pending = live?.gate != null;

  useEffect(() => {
    if (pending) return;
    end.current?.scrollIntoView({ block: "end" });
  }, [entries.length, pending]);

  if (entries.length === 0) {
    return (
      <div className="thread thread-empty">
        <h1>
          Say what you want.
          <br />
          It runs here.
        </h1>
        <p>
          Describe a task in plain English. Clinch acquires its own background
          Chromium, streams the run inline, pauses for your approval before
          anything is submitted or downloaded, and offers to keep the path as a
          1-click playbook. Your session stays on this machine.
        </p>
        <p className="notice">
          Try “download all my invoices from github” or “open amazon”. Use ⌘K to
          connect a portal or replay something you already saved.
        </p>
      </div>
    );
  }

  return (
    <div className="thread">
      <ol className="turns">
        {entries.map(entry => (
          <li key={entry.id}>
            <ActionCard
              entry={entry}
              live={live?.id === entry.id}
              liveFrame={liveFrame}
              agentCursor={agentCursor}
              headless={headless}
              windowMode={windowMode}
              browserBusy={browserBusy}
              onDecide={approved => onDecide(entry.id, approved)}
              onRename={saveName => onRename(entry.id, saveName)}
              onSave={() => onSave(entry.id)}
              onFile={(chip, reveal) => onFile(entry.id, chip, reveal)}
              onTakeControl={onTakeControl}
              onLendSession={(runId, sourceConnectionId) => onLendSession(entry.id, runId, sourceConnectionId)}
              onSyncAuthSession={(runId, sourceConnectionId) => onSyncAuthSession(entry.id, runId, sourceConnectionId)}
              onForgetSession={(host) => onForgetSession(entry.id, host)}
              onRelease={onRelease}
              onConnect={onConnect}
              onSaveShortcut={() => onSaveShortcut(entry.id)}
              onDismissShortcut={() => onDismissShortcut(entry.id)}
            />
          </li>
        ))}
      </ol>
      <div ref={end} />
    </div>
  );
}
