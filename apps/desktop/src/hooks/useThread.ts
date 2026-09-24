/**
 * The Action Thread's controller: it drives every execution lane and folds
 * what comes back into thread entries.
 *
 * All three lanes stream progress over a `tauri::ipc::Channel`, so a closed
 * view never stops a run — the terminal return value still lands, and the
 * entry settles from it. Lane differences (how a gate is answered, how a file
 * is opened, what can be saved) are resolved here so the views stay lane-blind.
 */

import { useCallback, useMemo, useReducer, useRef } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import {
  PLAYBOOKS_CHANGED,
  type DispatchOutcome,
  type LendOutcome,
  type PlaybookEvent,
  type PlaybookSummary,
  type SequenceOutcome,
  type Task,
  type TaskEvent,
  type CandidatePreview,
} from "../lib/ipc";
import { errorCode, message } from "../lib/errors";
import {
  activeEntry,
  anchorFromLines,
  dispatchChips,
  playbookGate,
  taskChips,
  taskGate,
  telemetryLines,
  threadReducer,
  tierFor,
  type EntryResult,
  type OutputChip,
  type SaveTarget,
  type ThreadEntry,
} from "../lib/thread";

/** Remembers the last task so a relaunch can restore what it produced. */
const LAST_TASK_KEY = "clinch-last-task";

export type ThreadApi = {
  entries: ThreadEntry[];
  /** True while any entry is still running or waiting on a decision. */
  running: boolean;
  submit: (prompt: string) => Promise<void>;
  replayPlaybook: (playbook: PlaybookSummary) => Promise<void>;
  replayMacro: (workflow: string, portalUrl: string) => Promise<void>;
  decide: (entryId: string, approved: boolean) => Promise<void>;
  save: (entryId: string) => Promise<void>;
  rename: (entryId: string, saveName: string) => void;
  /** Accept the post-landing shortcut offer: persist `site → url`. */
  saveShortcut: (entryId: string) => Promise<void>;
  /** Decline the post-landing shortcut offer: keep nothing. */
  dismissShortcut: (entryId: string) => void;
  openFile: (entryId: string, chip: OutputChip, reveal: boolean) => Promise<void>;
  /**
   * Session lending: the challenge-card consent tap. Pulls the challenged
   * site's cookies from the companion extension into the managed browser
   * and re-probes the gate. One attempt per run.
   */
  lendSession: (entryId: string, runId: string, sourceConnectionId?: number | null) => Promise<void>;
  /**
   * Auth sync: the auth-sync-card consent tap. Same lend path, but the
   * session lands on the auth-sync card with its persisted badge; the
   * cookies persist to Clinch's app profile.
   */
  syncAuthSession: (entryId: string, runId: string, sourceConnectionId?: number | null) => Promise<void>;
  /**
   * Forget a synced session: delete the site's cookies from Clinch's own
   * profile. The daily browser is untouched.
   */
  forgetSiteSession: (entryId: string, host: string) => Promise<void>;
  /** Attribute the newest frame to whichever entry is currently live. */
  noteFrame: (frame: string) => void;
};

/**
 * Whether and how a finished dispatch can become a playbook.
 *
 * Only a completed ad-hoc run is worth learning: a saved replay is already
 * stored, and a run that did not finish has nothing proven to keep. The
 * registry key is preferred because it carries the exact executed graph plus
 * its origin server-side; rebuilding from echoed steps needs a portal, so a
 * portal-less run simply offers no save.
 */
function saveTargetFor(outcome: DispatchOutcome, portalUrl: string): SaveTarget | null {
  if (outcome.kind !== "ephemeral" || outcome.result.status !== "completed") return null;
  if (outcome.runId) return { via: "run", runId: outcome.runId, suggested: outcome.name };
  if (!portalUrl) return null;
  return {
    via: "steps",
    steps: outcome.steps,
    portalUrl,
    suggested: `${outcome.name}-playbook`,
  };
}

function sequenceResult(
  outcome: SequenceOutcome,
  approved: CandidatePreview[],
  save: SaveTarget | null,
): EntryResult {
  return {
    status: outcome.status,
    completedSteps: outcome.completedSteps,
    totalSteps: outcome.totalSteps,
    stoppedAt: outcome.stoppedAt,
    chips: dispatchChips(outcome.completedSteps, outcome.totalSteps, approved),
    save,
  };
}

export function useThread(
  ensureBrowser: () => Promise<void>,
  portalUrl: string,
  report: (text: string) => void,
): ThreadApi {
  const [entries, dispatch] = useReducer(threadReducer, [] as ThreadEntry[]);
  const counter = useRef(0);
  // The reducer owns entry state, but callbacks must read the current entries
  // without being rebuilt for every progress event.
  const latest = useRef<ThreadEntry[]>(entries);
  latest.current = entries;

  const nextId = useCallback(() => {
    counter.current += 1;
    return `entry-${counter.current}`;
  }, []);

  const fail = useCallback((id: string, error: unknown) => {
    dispatch({
      type: "failed",
      id,
      at: Date.now(),
      message: message(error),
      code: errorCode(error),
    });
  }, []);

  const noteFrame = useCallback((frame: string) => {
    const live = activeEntry(latest.current);
    if (live) dispatch({ type: "frame", id: live.id, frame });
  }, []);

  /**
   * Wire one playbook-lane channel into an entry. Returns the channel plus a
   * reader for the candidates the gate actually showed, so the outcome card can
   * still name what was acted on after the gate is gone.
   */
  const playbookChannel = useCallback((id: string) => {
    const progress = new Channel<PlaybookEvent>();
    let approved: CandidatePreview[] = [];
    progress.onmessage = event => {
      dispatch({ type: "handle", id, handle: event.runId });
      dispatch({ type: "progress", id, stepIndex: event.stepIndex, phase: event.phase });
      if (event.approval) approved = event.approval.candidates;
      dispatch({ type: "gate", id, gate: event.approval ? playbookGate(event.approval) : null });
    };
    return { progress, approved: () => approved };
  }, []);

  const submit = useCallback(
    async (prompt: string) => {
      const text = prompt.trim();
      if (!text) return;
      const id = nextId();
      dispatch({ type: "submit", id, lane: "playbook", prompt: text, at: Date.now() });
      try {
        // Zero friction: the thread acquires the background browser itself
        // rather than asking the user to spin one up first.
        await ensureBrowser();
        const { progress, approved } = playbookChannel(id);
        const outcome = await invoke<DispatchOutcome>("dispatch_natural_command", {
          prompt: text,
          progress,
        });
        // Route telemetry first, then snapshot telemetry, matching journal
        // order — the resolved entry should read before the outcome.
        const lines = [
          ...telemetryLines(outcome.routeLog),
          ...telemetryLines(outcome.telemetryLog),
        ];
        dispatch({ type: "notes", id, lines });
        const anchor = anchorFromLines(lines);
        dispatch({
          type: "provenance",
          id,
          tier: tierFor(outcome.kind),
          ...(anchor ? { anchor } : {}),
        });
        dispatch({
          type: "settled",
          id,
          at: Date.now(),
          result: sequenceResult(outcome.result, approved(), saveTargetFor(outcome, portalUrl)),
          // Settle-time capture from the backend wins over the last live
          // screencast frame: the stream is armed pre-navigation and can
          // freeze on the launch placeholder for direct opens.
          finalFrame: outcome.finalFrame ?? null,
          // Human-verification gate instead of the destination: the card
          // offers headed takeover so the user solves it once.
          challenge: outcome.challenge ?? null,
          // Clean signed-out guest landing: the thread renders the
          // auth-sync card.
          authUrl: outcome.authUrl ?? null,
          // Backend run id so remembered runs can be saved as workflows.
          runId: outcome.runId ?? null,
          // Session-lending registry key for the challenge and auth-sync
          // card taps; decoupled from runId on purpose.
          lendId: outcome.lendId ?? null,
          // Settle-time page description for the preview overlay chrome.
          finalUrl: outcome.finalUrl ?? null,
          pageTitle: outcome.pageTitle ?? null,
        });
      } catch (error) {
        fail(id, error);
      }
    },
    [ensureBrowser, fail, nextId, playbookChannel, portalUrl],
  );

  const replayPlaybook = useCallback(
    async (playbook: PlaybookSummary) => {
      const id = nextId();
      dispatch({
        type: "submit",
        id,
        lane: "playbook",
        prompt: `Replay ${playbook.name}`,
        at: Date.now(),
      });
      try {
        await ensureBrowser();
        const { progress, approved } = playbookChannel(id);
        // A stored playbook carries its own origin, so replay needs no portal.
        const outcome = await invoke<SequenceOutcome>("execute_playbook", {
          id: playbook.id,
          progress,
        });
        let anchor: string | null = null;
        try {
          anchor = new URL(playbook.portalUrl).host;
        } catch {
          /* A stored origin that no longer parses simply shows no anchor. */
        }
        dispatch({
          type: "provenance",
          id,
          tier: "playbook",
          ...(anchor ? { anchor } : {}),
        });
        dispatch({
          type: "settled",
          id,
          at: Date.now(),
          // Already stored: there is nothing left to learn from this run.
          result: sequenceResult(outcome, approved(), null),
        });
      } catch (error) {
        fail(id, error);
      }
    },
    [ensureBrowser, fail, nextId, playbookChannel],
  );

  const replayMacro = useCallback(
    async (workflow: string, portal: string) => {
      const id = nextId();
      dispatch({ type: "submit", id, lane: "task", prompt: `Replay macro ${workflow}`, at: Date.now() });
      try {
        const progress = new Channel<TaskEvent>();
        progress.onmessage = event => {
          dispatch({ type: "handle", id, handle: event.task.id });
          localStorage.setItem(LAST_TASK_KEY, String(event.task.id));
          for (const [index, step] of event.task.plan.steps.entries()) {
            dispatch({ type: "progress", id, stepIndex: index, phase: step.state });
          }
          dispatch({ type: "gate", id, gate: event.approval ? taskGate(event.approval) : null });
        };
        // Saved macros replay by workflow name alone; no selectors are entered.
        const task = await invoke<Task>("run_task", {
          request: { workflow, portalUrl: portal, linkSelector: null, downloadSelector: "" },
          progress,
        });
        const files = task.plan.steps.flatMap(step => step.output.files);
        dispatch({
          type: "settled",
          id,
          at: Date.now(),
          // The backend timed this run; its number beats a client stopwatch.
          elapsedMs: task.elapsedMs,
          result: {
            status: task.state,
            completedSteps: task.plan.steps.filter(step => step.state === "completed").length,
            totalSteps: task.plan.steps.length,
            stoppedAt: task.repair?.stepIndex ?? null,
            chips: taskChips(files),
            save:
              task.state === "completed"
                ? {
                    via: "steps",
                    steps: task.plan.recording.steps.map(step => ({
                      kind: "legacy_selector",
                      action: step.action,
                      wait: step.wait,
                    })),
                    portalUrl: portal,
                    suggested: `${workflow}-playbook`,
                  }
                : null,
          },
        });
      } catch (error) {
        fail(id, error);
      }
    },
    [fail, nextId],
  );

  const decide = useCallback(async (entryId: string, approved: boolean) => {
    const entry = latest.current.find(candidate => candidate.id === entryId);
    const gate = entry?.gate;
    if (!gate) return;
    // Clear optimistically: a decided gate must not stay clickable, and a
    // stale-approval rejection below would otherwise leave it on screen.
    dispatch({ type: "gate", id: entryId, gate: null });
    try {
      if (gate.lane === "playbook") {
        await invoke("decide_playbook", {
          runId: gate.runId,
          index: gate.stepIndex,
          approved,
        });
      } else {
        await invoke("task_decision", { id: gate.runId, index: gate.stepIndex, approved });
      }
    } catch (error) {
      report(message(error));
    }
  }, [report]);

  const save = useCallback(
    async (entryId: string) => {
      const entry = latest.current.find(candidate => candidate.id === entryId);
      const target = entry?.result?.save;
      const name = entry?.saveName.trim();
      if (!entry || !target || !name) return;
      try {
        const savedId =
          target.via === "run"
            ? await invoke<string>("save_run_as_workflow", {
                runId: target.runId,
                name,
                description: null,
              })
            : await invoke<string>("save_playbook", {
                name,
                portalUrl: target.portalUrl,
                steps: target.steps,
              });
        dispatch({ type: "saved", id: entryId, savedId });
        report(`Saved playbook ${name} (id ${savedId}) — “${entry.prompt}” now replays instantly.`);
        // The saved list lives in the command palette: notify it rather than
        // threading state across the shell.
        window.dispatchEvent(new CustomEvent(PLAYBOOKS_CHANGED));
      } catch (error) {
        report(message(error));
      }
    },
    [report],
  );

  const rename = useCallback((entryId: string, saveName: string) => {
    dispatch({ type: "rename", id: entryId, saveName });
  }, []);

  /**
   * Accept the post-landing shortcut offer: persist `site → url` through the
   * same command the shortcut manager uses, so the next "open {site}"
   * answers from the shortcut rung with zero tokens. Consent-gated — the
   * backend never writes this on its own.
   */
  const saveShortcut = useCallback(
    async (entryId: string) => {
      const entry = latest.current.find(candidate => candidate.id === entryId);
      const offer = entry?.shortcutOffer;
      if (!entry || !offer || entry.shortcutSaved || entry.shortcutDismissed) return;
      try {
        await invoke("save_site_shortcut", { name: offer.site, url: offer.url });
        dispatch({ type: "shortcutSaved", id: entryId });
        report(
          `Saved shortcut ${offer.site} → ${offer.url} — “open ${offer.site}” now jumps straight there.`,
        );
      } catch (error) {
        report(message(error));
      }
    },
    [report],
  );

  const dismissShortcut = useCallback((entryId: string) => {
    dispatch({ type: "shortcutDismissed", id: entryId });
  }, []);

  const lendSession = useCallback(
    async (entryId: string, runId: string, sourceConnectionId?: number | null) => {
      dispatch({
        type: "lendState",
        id: entryId,
        lend: { status: "busy", reason: null, frame: null, forgetting: "idle" },
      });
      try {
        const outcome = await invoke<LendOutcome>("lend_session", {
          request: { lendId: runId, sourceConnectionId: sourceConnectionId ?? null },
        });
        dispatch({
          type: "lendState",
          id: entryId,
          lend: {
            status: outcome.cleared ? "cleared" : "persistent",
            reason: outcome.reason,
            frame: outcome.finalFrame,
            forgetting: "idle",
          },
        });
        report(
          outcome.cleared
            ? `Session synced (${outcome.cookiesLent} cookies) — the page should now load as you.`
            : (outcome.reason ?? "The human check is still there after syncing."),
        );
      } catch (error) {
        dispatch({
          type: "lendState",
          id: entryId,
          lend: { status: "persistent", reason: message(error), frame: null, forgetting: "idle" },
        });
      }
    },
    [report],
  );

  /** Auth-sync card: same consent-gated lend, but the synced state lands on
   * the auth-sync card with its persisted badge and "Forget this site"
   * control. `host` is derived from the card's backend-supplied URL. */
  const syncAuthSession = useCallback(
    async (entryId: string, runId: string, sourceConnectionId?: number | null) => {
      dispatch({
        type: "authState",
        id: entryId,
        auth: { status: "busy", reason: null, frame: null, forgetting: "idle" },
      });
      try {
        const outcome = await invoke<LendOutcome>("lend_session", {
          request: { lendId: runId, sourceConnectionId: sourceConnectionId ?? null },
        });
        dispatch({
          type: "authState",
          id: entryId,
          auth: outcome.cleared
            ? { status: "synced", reason: null, frame: outcome.finalFrame, forgetting: "idle" }
            : { status: "failed", reason: outcome.reason, frame: null, forgetting: "idle" },
        });
        report(
          outcome.cleared
            ? `Session synced (${outcome.cookiesLent} cookies) — you're logged in inside Clinch's own browser profile.`
            : (outcome.reason ?? "The sync didn't take."),
        );
      } catch (error) {
        dispatch({
          type: "authState",
          id: entryId,
          auth: { status: "failed", reason: message(error), frame: null, forgetting: "idle" },
        });
      }
    },
    [report],
  );

  /** Revoke a synced session: delete the site's cookies from Clinch's own
   * profile. The daily browser is untouched. On the auth-sync card the
   * offer returns (the live probe, not the badge, decides next); on a
   * cleared challenge card a confirmation line shows instead. */
  const forgetSiteSession = useCallback(
    async (entryId: string, host: string) => {
      const entry = latest.current.find(candidate => candidate.id === entryId);
      const auth = entry?.auth ?? null;
      const lend = !auth ? (entry?.lend ?? null) : null;
      if (!auth && !lend) return;
      if (auth) {
        dispatch({
          type: "authState",
          id: entryId,
          auth: { ...auth, forgetting: "busy" },
        });
      } else if (lend) {
        dispatch({
          type: "lendState",
          id: entryId,
          lend: { ...lend, forgetting: "busy" },
        });
      }
      try {
        const cleared = await invoke<number>("forget_site_session", { host });
        report(
          cleared > 0
            ? `Forgot ${host} — ${cleared} cookies cleared from Clinch's browser.`
            : `Forgot ${host} — nothing left to clear.`,
        );
        const current = latest.current.find(candidate => candidate.id === entryId);
        if (auth) {
          // The persisted copy is gone; the auth card drops back to the
          // signed-out offer.
          dispatch({ type: "authState", id: entryId, auth: null });
        } else if (current?.lend) {
          dispatch({
            type: "lendState",
            id: entryId,
            lend: { ...current.lend, forgetting: "done" },
          });
        }
      } catch (error) {
        const current = latest.current.find(candidate => candidate.id === entryId);
        if (auth && current?.auth) {
          dispatch({
            type: "authState",
            id: entryId,
            auth: { ...current.auth, forgetting: "failed" },
          });
        } else if (lend && current?.lend) {
          dispatch({
            type: "lendState",
            id: entryId,
            lend: { ...current.lend, forgetting: "failed" },
          });
        }
        report(message(error));
      }
    },
    [report],
  );

  const openFile = useCallback(
    async (entryId: string, chip: OutputChip, reveal: boolean) => {
      const entry = latest.current.find(candidate => candidate.id === entryId);
      if (!entry || entry.handle === null || chip.fileIndex === null) return;
      try {
        await invoke("downloaded_file_action", {
          id: entry.handle,
          index: chip.fileIndex,
          reveal,
        });
      } catch (error) {
        report(message(error));
      }
    },
    [report],
  );

  const running = useMemo(
    () => entries.some(entry => entry.status === "running" || entry.status === "awaiting"),
    [entries],
  );

  return {
    entries,
    running,
    submit,
    replayPlaybook,
    replayMacro,
    decide,
    save,
    rename,
    saveShortcut,
    dismissShortcut,
    openFile,
    lendSession,
    syncAuthSession,
    forgetSiteSession,
    noteFrame,
  };
}
