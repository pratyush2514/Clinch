/**
 * The thread's contract: a run reads top to bottom in the order it happened,
 * and turns stack in the order they were asked.
 *
 * Entries are built by folding real wire events through `threadReducer` rather
 * than hand-written, so these assertions cover the reducer and the views
 * together — a card that renders correctly from a state the backend can never
 * produce would not be worth much.
 */

import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import ActionThread from "./ActionThread";
import { threadReducer, type ThreadAction, type ThreadEntry } from "../lib/thread";
import type { PlaybookApproval } from "../lib/ipc";

const FRAME = "/9j/4AAQSkZJRg==";

function fold(actions: ThreadAction[]): ThreadEntry[] {
  return actions.reduce(threadReducer, [] as ThreadEntry[]);
}

const BATCH_APPROVAL: PlaybookApproval = {
  runId: 7,
  stepIndex: 0,
  kind: "intent",
  summary: "Batch click: 3 link controls for download all my invoices from github",
  candidates: [
    { index: 0, label: "Download", role: "link", isLandmark: false, container: "Invoices INV-001" },
    { index: 1, label: "Download", role: "link", isLandmark: false, container: "Invoices INV-002" },
    { index: 2, label: "Download", role: "link", isLandmark: false, container: "Invoices INV-003" },
  ],
};

/** Everything up to the pending gate: the state a paused batch is really in. */
function awaitingApproval(): ThreadEntry[] {
  return fold([
    { type: "submit", id: "e1", lane: "playbook", prompt: "download all my invoices from github", at: 1_000 },
    { type: "handle", id: "e1", handle: 7 },
    { type: "progress", id: "e1", stepIndex: 0, phase: "started" },
    { type: "provenance", id: "e1", tier: "search", anchor: "github.com" },
    { type: "frame", id: "e1", frame: FRAME },
    {
      type: "gate",
      id: "e1",
      gate: {
        lane: "playbook",
        runId: BATCH_APPROVAL.runId,
        stepIndex: BATCH_APPROVAL.stepIndex,
        kind: BATCH_APPROVAL.kind,
        summary: BATCH_APPROVAL.summary,
        candidates: BATCH_APPROVAL.candidates,
      },
    },
  ]);
}

/** The same run, approved and finished, with something to keep. */
function completed(): ThreadEntry[] {
  return fold([
    ...toActions(awaitingApproval()),
    { type: "progress", id: "e1", stepIndex: 0, phase: "completed" },
    {
      type: "settled",
      id: "e1",
      at: 1_840,
      result: {
        status: "completed",
        completedSteps: 1,
        totalSteps: 1,
        stoppedAt: null,
        chips: [
          { kind: "data", label: "1/1 steps", detail: null, fileIndex: null },
          { kind: "file", label: "invoice_aug2026.pdf", detail: "20 bytes · /tmp/invoice_aug2026.pdf", fileIndex: 0 },
        ],
        save: { via: "run", runId: "run-7", suggested: "download-invoices" },
      },
    },
  ]);
}

/**
 * Replays an already-folded thread as the actions that produced it. Keeps the
 * two fixtures above sharing one history instead of drifting apart.
 */
function toActions(entries: ThreadEntry[]): ThreadAction[] {
  return entries.flatMap<ThreadAction>(entry => [
    { type: "submit", id: entry.id, lane: entry.lane, prompt: entry.prompt, at: entry.startedAt },
    ...(entry.handle === null
      ? []
      : [{ type: "handle" as const, id: entry.id, handle: entry.handle }]),
    ...entry.steps.map(step => ({
      type: "progress" as const,
      id: entry.id,
      stepIndex: step.index,
      phase: step.phase,
    })),
    ...(entry.tier || entry.anchor
      ? [
          {
            type: "provenance" as const,
            id: entry.id,
            ...(entry.tier ? { tier: entry.tier } : {}),
            ...(entry.anchor ? { anchor: entry.anchor } : {}),
          },
        ]
      : []),
    ...(entry.frame ? [{ type: "frame" as const, id: entry.id, frame: entry.frame }] : []),
    ...(entry.gate ? [{ type: "gate" as const, id: entry.id, gate: entry.gate }] : []),
  ]);
}

function view(entries: ThreadEntry[], overrides: Partial<Parameters<typeof ActionThread>[0]> = {}) {
  return render(
    <ActionThread
      entries={entries}
      liveFrame={null}
      agentCursor={null}
      headless
      windowMode="headless"
      browserBusy={false}
      onDecide={vi.fn()}
      onRename={vi.fn()}
      onSave={vi.fn()}
      onFile={vi.fn()}
      onTakeControl={vi.fn()}
      onLendSession={vi.fn()}
      onSyncAuthSession={vi.fn()}
      onForgetSession={vi.fn()}
      onRelease={vi.fn()}
      onConnect={vi.fn()}
      onSaveShortcut={vi.fn()}
      onDismissShortcut={vi.fn()}
      {...overrides}
    />,
  );
}

/** The card's direct children, in document order, by leading class name. */
function sections(card: HTMLElement): string[] {
  return Array.from(card.children).map(child => child.className.split(" ")[0]);
}

function card(index = 0): HTMLElement {
  return screen.getAllByRole("article")[index];
}

describe("ActionThread", () => {
  it("stacks the prompt, screencast and inline approval in that order while a run is paused", () => {
    view(awaitingApproval());

    expect(sections(card())).toEqual(["prompt", "pills", "cast", "steps", "gate-card"]);

    // The prompt is shown verbatim, attributed to the user.
    expect(screen.getByText("download all my invoices from github")).toBeDefined();
    expect(screen.getByText("You")).toBeDefined();
    // Provenance: which tier answered, and where the session re-anchored to.
    expect(screen.getByText("Tier 2 · Search & Follow")).toBeDefined();
    expect(screen.getByText("🟢 Re-anchored to github.com")).toBeDefined();
    // The live viewport renders the streamed JPEG frame.
    const viewport = screen.getByAltText("Managed Chromium viewport") as HTMLImageElement;
    expect(viewport.src).toBe(`data:image/jpeg;base64,${FRAME}`);
    // The gate names the batch and itemizes every control it will click.
    const gate = screen.getByRole("alertdialog", { name: "Sentinel Gate" });
    expect(gate.textContent).toContain("3 CONTROLS FOUND");
    expect(gate.textContent).toContain(BATCH_APPROVAL.summary);
    expect(screen.getAllByText("Download")).toHaveLength(3);
    expect(screen.getByText("link · Invoices INV-003")).toBeDefined();
    expect(screen.getByRole("button", { name: "Approve & Submit" })).toBeDefined();
    expect(screen.getByRole("button", { name: "Reject" })).toBeDefined();
    // Nothing has been produced yet, so no completion card exists.
    expect(screen.queryByLabelText("Run outcome")).toBeNull();
  });

  it("replaces the approval with a completion card carrying timing, artifacts and a save", () => {
    view(completed());

    expect(sections(card())).toEqual(["prompt", "pills", "cast", "steps", "outcome"]);

    // A decided gate is gone: it must not stay clickable after the fact.
    expect(screen.queryByRole("alertdialog")).toBeNull();
    const outcome = screen.getByLabelText("Run outcome");
    expect(outcome.textContent).toContain("✅ COMPLETED");
    // Wall-clock timing, derived from the entry's own start and settle times.
    expect(outcome.textContent).toContain("in 840ms");
    expect(outcome.textContent).toContain("1/1 steps");
    expect(screen.getByText("📄 invoice_aug2026.pdf")).toBeDefined();
    expect(screen.getByRole("button", { name: "Open File" })).toBeDefined();
    expect(screen.getByRole("button", { name: "Show in Folder" })).toBeDefined();
    // One click persists the proven path for Tier 1 replay.
    expect(screen.getByRole("button", { name: "Save as Playbook" })).toBeDefined();
    // The frozen frame stays as evidence of what the run saw.
    expect(screen.getByAltText("Managed Chromium viewport")).toBeDefined();
  });

  it("keeps turns in submission order and gives viewport controls only to the live turn", () => {
    const entries = fold([
      ...toActions(completed()),
      { type: "submit", id: "e2", lane: "playbook", prompt: "open amazon", at: 2_000 },
      { type: "frame", id: "e2", frame: FRAME },
    ]);
    view(entries);

    const prompts = screen.getAllByRole("article").map(article => article.getAttribute("aria-label"));
    expect(prompts).toEqual(["download all my invoices from github", "open amazon"]);
    // Only the running turn may release or take over the browser; offering it
    // on a finished turn would act on a run that is no longer there.
    expect(screen.getAllByRole("button", { name: "Release Browser" })).toHaveLength(1);
    expect(screen.getAllByRole("button", { name: "Take Control" })).toHaveLength(1);
    // Enlarging a frame acts on nothing, so every turn that has one offers it.
    expect(screen.getAllByRole("button", { name: "Open Preview" })).toHaveLength(2);
    expect(screen.getByText("final frame")).toBeDefined();
    expect(screen.getByText("live · headless background session")).toBeDefined();
  });

  it("offers the one fix a session failure has, instead of only naming it", () => {
    const onConnect = vi.fn();
    const entries = fold([
      { type: "submit", id: "e1", lane: "playbook", prompt: "download my invoices", at: 1_000 },
      {
        type: "failed",
        id: "e1",
        at: 1_200,
        message: "Connect this portal and finish signing in in the managed browser before running tasks.",
        code: "session_required",
      },
    ]);
    view(entries, { onConnect });

    screen.getByRole("button", { name: "Connect a portal" }).click();
    expect(onConnect).toHaveBeenCalledTimes(1);
  });

  it("invites a first task when the thread is empty", () => {
    view([]);
    expect(screen.queryByRole("article")).toBeNull();
    expect(screen.getByText(/download all my invoices from github/)).toBeDefined();
  });

  it("renders the Save as Shortcut card only after the post-landing offer line", () => {
    const landed = fold([
      { type: "submit", id: "e1", lane: "playbook", prompt: "open amazon for me", at: 1_000 },
      {
        type: "notes",
        id: "e1",
        lines: [
          "route_proposed:www.amazon.in/ · source: DomainGrounded",
          "shortcut_offer: 'amazon' → https://www.amazon.in · save to skip grounding next time",
        ],
      },
    ]);
    view(landed);
    const card = screen.getByLabelText("Save shortcut offer");
    expect(card.textContent).toContain("amazon");
    expect(card.textContent).toContain("https://www.amazon.in");
    expect(screen.getByRole("button", { name: "Save shortcut" })).toBeDefined();
  });

  it("shows no shortcut card when the journal never offered one", () => {
    const entries = fold([
      { type: "submit", id: "e1", lane: "playbook", prompt: "open amazon for me", at: 1_000 },
      {
        type: "notes",
        id: "e1",
        lines: ["route_proposed:www.amazon.in/ · source: DomainGrounded"],
      },
    ]);
    view(entries);
    expect(screen.queryByLabelText("Save shortcut offer")).toBeNull();
    expect(screen.queryByRole("button", { name: "Save shortcut" })).toBeNull();
  });

  it("accepting the card saves the shortcut for that entry", () => {
    const onSaveShortcut = vi.fn();
    const entries = fold([
      { type: "submit", id: "e1", lane: "playbook", prompt: "open amazon for me", at: 1_000 },
      {
        type: "notes",
        id: "e1",
        lines: ["shortcut_offer: 'amazon' → https://www.amazon.in · save to skip grounding next time"],
      },
    ]);
    view(entries, { onSaveShortcut });
    screen.getByRole("button", { name: "Save shortcut" }).click();
    expect(onSaveShortcut).toHaveBeenCalledWith("e1");
  });

  it("declining the card dismisses it for that entry", () => {
    const onDismissShortcut = vi.fn();
    const entries = fold([
      { type: "submit", id: "e1", lane: "playbook", prompt: "open amazon for me", at: 1_000 },
      {
        type: "notes",
        id: "e1",
        lines: ["shortcut_offer: 'amazon' → https://www.amazon.in · save to skip grounding next time"],
      },
    ]);
    view(entries, { onDismissShortcut });
    screen.getByRole("button", { name: "Not now" }).click();
    expect(onDismissShortcut).toHaveBeenCalledWith("e1");
  });

  it("hands the browser over on an account-home pursuit miss", () => {
    const onTakeControl = vi.fn();
    const entries = fold([
      { type: "submit", id: "e1", lane: "playbook", prompt: "open my profile on reddit", at: 1_000 },
      {
        type: "failed",
        id: "e1",
        at: 1_200,
        message:
          "account-home: tried avatar button (menu opened); tried profile link (no new controls); no verified destination",
        code: "workflow_failed",
      },
    ]);
    view(entries, { onTakeControl });

    // The worker journaled what it tried; the browser is still on the
    // portal, so the card offers the window instead of a dead end.
    const button = screen.getByRole("button", { name: "Take control" });
    button.click();
    expect(onTakeControl).toHaveBeenCalledTimes(1);
    expect(onTakeControl).toHaveBeenCalledWith();
  });

  it("does not offer Take control on other workflow failures", () => {
    const entries = fold([
      { type: "submit", id: "e1", lane: "playbook", prompt: "download my invoices", at: 1_000 },
      {
        type: "failed",
        id: "e1",
        at: 1_200,
        message: "The run failed before anything could be tried.",
        code: "workflow_failed",
      },
    ]);
    view(entries);

    // The miss affordance is gated on the worker's own journal contract;
    // unrelated failures must not grow a Take control button.
    expect(screen.queryByRole("button", { name: "Take control" })).toBeNull();
  });
});
