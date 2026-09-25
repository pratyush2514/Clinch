/**
 * The Action Thread's state model.
 *
 * One entry per submitted task, appended in order and never reordered, so the
 * thread reads chronologically: prompt, live viewport, approvals, outcome.
 * Everything here is pure — the reducer takes wire events already normalized
 * by the lane adapters in `useThread`, which is what lets the thread's
 * behavior be tested without a backend.
 *
 * Two execution lanes feed it. `playbook` covers natural-language dispatch and
 * saved replays (`dispatch_natural_command`, `execute_playbook`); `task`
 * covers recorded-macro replays (`run_task`). They differ only in how a gate
 * is answered and how files are opened, so both are normalized into the
 * shapes below rather than leaking into the views.
 */

import type { Action, CandidatePreview, PlaybookApproval, TaskGate } from "./ipc";

/** Which resolution tier answered, for the provenance pill. */
type Tier = "playbook" | "search";

type EntryStatus = "running" | "awaiting" | "completed" | "blocked" | "failed";

type Lane = "playbook" | "task";

/**
 * A pending Sentinel Gate, normalized across lanes. `runId` is the playbook
 * run id or the task id; the lane decides which command answers it.
 */
export type ThreadGate = {
  lane: Lane;
  runId: number;
  stepIndex: number;
  kind: string;
  summary: string;
  candidates: CandidatePreview[];
};

/**
 * A domain-grounder hit the backend offers to keep. The journal line only
 * exists after navigation to the grounded URL completed, so an offer here
 * is proof of a real landing — never a proposal that failed to land.
 */
export type ShortcutOffer = {
  site: string;
  url: string;
};

/** One artifact the run produced: a downloaded file or an extracted value. */
export type OutputChip = {
  kind: "file" | "data";
  label: string;
  detail: string | null;
  /** Position in the run's flattened file list; `null` for data chips. */
  fileIndex: number | null;
};

/**
 * How this run can be persisted. `run` uses the backend's completed-run
 * registry (the exact executed graph plus its origin, no client round-trip);
 * `steps` rebuilds from the echoed steps and therefore needs an origin.
 */
export type SaveTarget =
  | { via: "run"; runId: string; suggested: string }
  | { via: "steps"; steps: unknown[]; portalUrl: string; suggested: string };

export type EntryResult = {
  status: string;
  completedSteps: number;
  totalSteps: number;
  stoppedAt: number | null;
  chips: OutputChip[];
  save: SaveTarget | null;
};

type StepProgress = { index: number; phase: string };

export type ThreadEntry = {
  id: string;
  lane: Lane;
  prompt: string;
  startedAt: number;
  status: EntryStatus;
  /**
   * Backend identifier for the run behind this entry: the task id on the task
   * lane, the playbook run id otherwise. Needed to act on what a run produced
   * (opening a downloaded file) after its gate is gone.
   */
  handle: number | null;
  tier: Tier | null;
  /** Host the session re-anchored to, when a run moved the target. */
  anchor: string | null;
  steps: StepProgress[];
  gate: ThreadGate | null;
  /** Route and snapshot telemetry, one line each, in journal order. */
  notes: string[];
  /** Last screencast frame this entry saw, frozen once it settles. */
  frame: string | null;
  /**
   * Whether the backend's settle-time capture landed: `true` when the run
   * settled with a `finalFrame`, `false` when the card falls back to the
   * last live frame because the capture missed, `null` before the entry
   * settles. Drives the honest "last live frame" badge wording — `undefined`
   * (legacy data) keeps the old "final frame" label.
   */
  finalFrameCaptured: boolean | null;
  /**
   * Page URL when the run settled on a bot-mitigation interstitial
   * (human-verification gate) instead of the destination. The thread
   * offers headed takeover so the user solves the check once.
   */
  challenge: string | null;
  /** Page URL when the run settled on a clean signed-out guest landing.
   * The thread renders the auth-sync card; backend keeps the run in the
   * same lend registry the challenge card uses. Absent means the page
   * read as authenticated or unclassifiable. */
  authUrl: string | null;
  /** Backend run id for save-as-workflow on remembered runs. */
  runId: string | null;
  /** Session-lending registry key for this entry's challenge or auth-sync
   * card tap. Decoupled from `runId`: pure direct opens are deliberately
   * not remembered, but their card tap still carries a key. */
  lendId: string | null;
  /** L1.5 session-lend state for this entry's challenge card. */
  lend: LendUiState | null;
  /** Synced-session state for this entry's auth-sync card. */
  auth: AuthUiState | null;
  /** Settle-time page URL for the preview overlay's browser chrome. */
  finalUrl: string | null;
  /** Settle-time document title for the overlay's tab label. */
  pageTitle: string | null;
  elapsedMs: number | null;
  result: EntryResult | null;
  saveName: string;
  savedId: string | null;
  /** The backend's post-landing shortcut offer, parsed from journal lines. */
  shortcutOffer: ShortcutOffer | null;
  /** True once the offer above was accepted and persisted. */
  shortcutSaved: boolean;
  /** True once the offer above was declined; declining keeps nothing. */
  shortcutDismissed: boolean;
  error: { message: string; code: string | null } | null;
};

/** Session-lend UI state for a challenge card or an auth-sync card. */
export type LendUiState = {
  status: "busy" | "cleared" | "persistent";
  /** Static user-facing reason when the gate did not clear. */
  reason: string | null;
  /** Fresh settle-time frame captured after a cleared lend. */
  frame: string | null;
  /** "Forget this site" revocation: deleting the persisted cookies. */
  forgetting: "idle" | "busy" | "done" | "failed";
};

/** Auth-sync card state for a signed-out guest landing. */
export type AuthUiState = {
  status: "synced" | "busy" | "failed";
  /** Static user-facing reason when the sync did not work. */
  reason: string | null;
  /** Fresh frame captured after the session landed. */
  frame: string | null;
  /** "Forget this site" revocation: deleting the persisted cookies. */
  forgetting: "idle" | "busy" | "done" | "failed";
};

export type ThreadAction =
  | { type: "submit"; id: string; lane: Lane; prompt: string; at: number }
  | { type: "handle"; id: string; handle: number }
  | { type: "progress"; id: string; stepIndex: number; phase: string }
  | { type: "gate"; id: string; gate: ThreadGate | null }
  | { type: "notes"; id: string; lines: string[] }
  | { type: "provenance"; id: string; tier?: Tier; anchor?: string }
  | { type: "frame"; id: string; frame: string }
  | { type: "settled"; id: string; at: number; elapsedMs?: number; result: EntryResult; finalFrame?: string | null; lastLiveFrame?: string | null; challenge?: string | null; authUrl?: string | null; runId?: string | null; lendId?: string | null; finalUrl?: string | null; pageTitle?: string | null }
  | { type: "failed"; id: string; at: number; message: string; code: string | null }
  | { type: "lendState"; id: string; lend: LendUiState }
  | { type: "authState"; id: string; auth: AuthUiState | null }
  | { type: "rename"; id: string; saveName: string }
  | { type: "saved"; id: string; savedId: string }
  | { type: "shortcutSaved"; id: string }
  | { type: "shortcutDismissed"; id: string };

/** Terminal sequence statuses that are not a clean completion. */
const BLOCKED_STATUSES = new Set(["denied", "needs_repair"]);

function statusFor(result: EntryResult): EntryStatus {
  if (result.status === "completed") return "completed";
  return BLOCKED_STATUSES.has(result.status) ? "blocked" : "failed";
}

/** Replace one entry in place, preserving thread order. */
function patch(
  entries: ThreadEntry[],
  id: string,
  change: (entry: ThreadEntry) => ThreadEntry,
): ThreadEntry[] {
  return entries.map(entry => (entry.id === id ? change(entry) : entry));
}

export function threadReducer(entries: ThreadEntry[], action: ThreadAction): ThreadEntry[] {
  switch (action.type) {
    case "submit":
      return [
        ...entries,
        {
          id: action.id,
          lane: action.lane,
          prompt: action.prompt,
          startedAt: action.at,
          status: "running",
          handle: null,
          tier: null,
          anchor: null,
          steps: [],
          gate: null,
          notes: [],
          frame: null,
          // No settle-time capture yet: the card may not claim "final frame".
          finalFrameCaptured: null,
          challenge: null,
          authUrl: null,
          runId: null,
          lendId: null,
          lend: null,
          auth: null,
          /** Settle-time page URL/title for the preview overlay chrome. */
          finalUrl: null,
          pageTitle: null,
          elapsedMs: null,
          result: null,
          saveName: "",
          savedId: null,
          shortcutOffer: null,
          shortcutSaved: false,
          shortcutDismissed: false,
          error: null,
        },
      ];
    case "handle":
      return patch(entries, action.id, entry => ({ ...entry, handle: action.handle }));
    case "progress":
      // Phases arrive repeatedly per step (started, running, completed), so a
      // step is updated in place rather than appended twice.
      return patch(entries, action.id, entry => {
        const seen = entry.steps.some(step => step.index === action.stepIndex);
        return {
          ...entry,
          steps: seen
            ? entry.steps.map(step =>
                step.index === action.stepIndex ? { ...step, phase: action.phase } : step,
              )
            : [...entry.steps, { index: action.stepIndex, phase: action.phase }],
        };
      });
    case "gate":
      return patch(entries, action.id, entry => ({
        ...entry,
        gate: action.gate,
        status: action.gate ? "awaiting" : entry.status === "awaiting" ? "running" : entry.status,
      }));
    case "notes":
      return patch(entries, action.id, entry => {
        const notes = [...entry.notes, ...action.lines];
        return {
          ...entry,
          notes,
          // The card is derived from the journal, not stored separately:
          // the offer line only exists after a real landing. Parsed once —
          // a declined offer must not come back on a later notes action.
          shortcutOffer: entry.shortcutOffer ?? shortcutOfferFromLines(notes),
        };
      });
    case "provenance":
      return patch(entries, action.id, entry => ({
        ...entry,
        tier: action.tier ?? entry.tier,
        anchor: action.anchor ?? entry.anchor,
      }));
    case "frame":
      return patch(entries, action.id, entry =>
        // A settled entry keeps the frame it ended on: later runs must not
        // rewrite the evidence of an earlier one.
        entry.result || entry.error ? entry : { ...entry, frame: action.frame },
      );
    case "settled":
      return patch(entries, action.id, entry => ({
        ...entry,
        gate: null,
        status: statusFor(action.result),
        elapsedMs: action.elapsedMs ?? action.at - entry.startedAt,
        result: action.result,
        saveName: entry.saveName || action.result.save?.suggested || "",
        // The backend's settle-time capture is evidence of what the run
        // saw; without it the card keeps the last live frame the hook held
        // in its ref, which for direct opens can be the launch placeholder.
        frame: action.finalFrame ?? action.lastLiveFrame ?? entry.frame,
        // Whether the badge may honestly say "final frame": an explicit
        // false means the card is showing the last live frame instead.
        finalFrameCaptured: action.finalFrame != null,
        // A completed run that landed on a human-verification gate keeps
        // the challenge URL so the thread can offer headed takeover.
        challenge: action.challenge ?? null,
        // A completed run that landed on a clean signed-out guest landing
        // keeps the auth URL so the thread can render the auth-sync card.
        authUrl: action.authUrl ?? null,
        // Backend run id travels so remembered runs can be saved as
        // workflows; the lend id travels so the challenge and auth-sync
        // cards can lend a session. A fresh settle resets any previous
        // lend state.
        runId: action.runId ?? null,
        lendId: action.lendId ?? null,
        lend: null,
        auth: null,
        // Settle-time page description for the preview overlay's browser
        // chrome; absent when the target died mid-settle.
        finalUrl: action.finalUrl ?? null,
        pageTitle: action.pageTitle ?? null,
      }));
    case "lendState":
      return patch(entries, action.id, entry => ({
        ...entry,
        lend: action.lend,
        // A cleared lend brings a fresh frame of the landed page.
        frame: action.lend.frame ?? entry.frame,
      }));
    case "authState":
      return patch(entries, action.id, entry => ({
        ...entry,
        auth: action.auth,
        // A synced session brings a fresh frame of the landed page;
        // a forgotten session drops the frame so the next run re-reads.
        frame: action.auth?.frame ?? entry.frame,
      }));
    case "failed":
      return patch(entries, action.id, entry => ({
        ...entry,
        gate: null,
        status: "failed",
        elapsedMs: action.at - entry.startedAt,
        error: { message: action.message, code: action.code },
      }));
    case "rename":
      return patch(entries, action.id, entry => ({ ...entry, saveName: action.saveName }));
    case "saved":
      return patch(entries, action.id, entry => ({ ...entry, savedId: action.savedId }));
    case "shortcutSaved":
      return patch(entries, action.id, entry => ({ ...entry, shortcutSaved: true }));
    case "shortcutDismissed":
      // Declining leaves nothing behind: the card goes away and the journal
      // line stays as plain telemetry.
      return patch(entries, action.id, entry => ({ ...entry, shortcutDismissed: true }));
  }
}

/**
 * Which tier answered a dispatch. A stored playbook is the hot path (tier 1);
 * anything resolved at run time reached the destination by searching (tier 2).
 */
export function tierFor(kind: string): Tier {
  return kind === "saved" ? "playbook" : "search";
}

export const TIER_LABELS: Record<Tier, string> = {
  playbook: "Tier 1 · Playbook",
  search: "Tier 2 · Search & Follow",
};

/** Split a multi-line telemetry log into non-blank journal lines. */
export function telemetryLines(log: string | null | undefined): string[] {
  if (!log) return [];
  return log.split("\n").filter(line => line.trim().length > 0);
}

function hostOf(value: string): string | null {
  const trimmed = value.trim();
  if (!trimmed || trimmed === "none") return null;
  try {
    return new URL(trimmed).host || null;
  } catch {
    // `route_proposed` journals `host/path`, not an absolute URL.
    const [host] = trimmed.split("/");
    return host && host.includes(".") ? host : null;
  }
}

/**
 * The host a run ended up anchored to, read from its own journal lines.
 *
 * `portal_reanchored: <previous> → <current>` is authoritative because it
 * records where confinement actually moved. A `route_proposed:` line is the
 * fallback: it names the entry the run was sent to, which is the destination
 * whenever no re-anchor was needed.
 */
export function anchorFromLines(lines: string[]): string | null {
  for (const line of lines) {
    if (!line.startsWith("portal_reanchored:")) continue;
    const [, landed] = line.split("→");
    const host = landed ? hostOf(landed) : null;
    if (host) return host;
  }
  for (const line of lines) {
    if (!line.startsWith("route_proposed:")) continue;
    const host = hostOf(line.slice("route_proposed:".length).split("·")[0] ?? "");
    if (host) return host;
  }
  return null;
}

/**
 * The post-landing shortcut offer, read from the run's own journal lines.
 *
 * The backend journals `shortcut_offer: '<site>' → <url> · …` only after
 * navigation to a domain-grounded URL completed, so a parsed offer is
 * proof the card may be shown — a proposal that never landed produces no
 * such line.
 */
export function shortcutOfferFromLines(lines: string[]): ShortcutOffer | null {
  for (const line of lines) {
    if (!line.startsWith("shortcut_offer:")) continue;
    const body = line.slice("shortcut_offer:".length).split("·")[0]?.trim() ?? "";
    const [sitePart, url] = body.split("→").map(part => part.trim());
    const site = sitePart?.replace(/^'|'$/g, "");
    if (site && url) return { site, url };
  }
  return null;
}

/** A gate from the playbook lane, answered by `decide_playbook`. */
export function playbookGate(approval: PlaybookApproval): ThreadGate {
  return {
    lane: "playbook",
    runId: approval.runId,
    stepIndex: approval.stepIndex,
    kind: approval.kind,
    summary: approval.summary,
    candidates: approval.candidates,
  };
}

const ACTION_LABELS: Record<Action["type"], string> = {
  navigate: "Open page",
  submit: "Submit form",
  click: "Follow link",
  fill: "Apply filter",
  download_links: "Download files",
};

/** Plain-language description of a recorded action, for a task-lane gate. */
export function describeAction(action: Action): string {
  const target = "selector" in action ? action.selector : action.url;
  const label = `${ACTION_LABELS[action.type]} · ${target}`;
  return action.type === "fill" ? `${label} = ${action.value}` : label;
}

/** A gate from the task lane, answered by `task_decision`. */
export function taskGate(gate: TaskGate): ThreadGate {
  return {
    lane: "task",
    runId: gate.taskId,
    stepIndex: gate.stepIndex,
    kind: gate.action.type,
    summary: describeAction(gate.action),
    candidates: [],
  };
}

/** Downloaded files a finished task produced, flattened in step order. */
export function taskChips(files: { path: string; bytes: number }[]): OutputChip[] {
  return files.map((file, index) => ({
    kind: "file",
    label: file.path.split(/[\\/]/).pop() || file.path,
    detail: `${file.bytes} bytes · ${file.path}`,
    fileIndex: index,
  }));
}

/**
 * What a dispatch run has to show for itself. Dispatch outcomes carry no file
 * manifest, so the chips describe the executed graph: how far it got, and the
 * controls the approval covered.
 */
export function dispatchChips(
  completedSteps: number,
  totalSteps: number,
  approved: CandidatePreview[],
): OutputChip[] {
  const chips: OutputChip[] = [
    {
      kind: "data",
      label: `${completedSteps}/${totalSteps} steps`,
      detail: null,
      fileIndex: null,
    },
  ];
  for (const candidate of approved) {
    chips.push({
      kind: "data",
      label: candidate.label || "(unnamed)",
      detail: candidate.container,
      fileIndex: null,
    });
  }
  return chips;
}

/** The entry a live viewport belongs to: the one still running, if any. */
export function activeEntry(entries: ThreadEntry[]): ThreadEntry | null {
  for (let index = entries.length - 1; index >= 0; index -= 1) {
    const entry = entries[index];
    if (entry.status === "running" || entry.status === "awaiting") return entry;
  }
  return null;
}
