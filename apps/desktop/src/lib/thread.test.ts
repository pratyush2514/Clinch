/**
 * The thread model's own rules, independent of any view: how repeated progress
 * events collapse, which entry a frame belongs to, and what the provenance pill
 * can honestly claim from a run's journal lines.
 */

import { describe, expect, it } from "vitest";
import {
  activeEntry,
  anchorFromLines,
  describeAction,
  dispatchChips,
  shortcutOfferFromLines,
  taskChips,
  telemetryLines,
  threadReducer,
  tierFor,
  type ThreadAction,
  type ThreadEntry,
} from "./thread";

function fold(actions: ThreadAction[]): ThreadEntry[] {
  return actions.reduce(threadReducer, [] as ThreadEntry[]);
}

const SUBMIT: ThreadAction = {
  type: "submit",
  id: "e1",
  lane: "playbook",
  prompt: "download all my invoices",
  at: 1_000,
};

describe("threadReducer", () => {
  it("collapses repeated phases for one step instead of appending duplicates", () => {
    const [entry] = fold([
      SUBMIT,
      { type: "progress", id: "e1", stepIndex: 0, phase: "started" },
      { type: "progress", id: "e1", stepIndex: 0, phase: "running" },
      { type: "progress", id: "e1", stepIndex: 1, phase: "started" },
      { type: "progress", id: "e1", stepIndex: 0, phase: "completed" },
    ]);
    expect(entry.steps).toEqual([
      { index: 0, phase: "completed" },
      { index: 1, phase: "started" },
    ]);
  });

  it("moves in and out of awaiting as a gate opens and closes", () => {
    const gate = {
      lane: "playbook" as const,
      runId: 1,
      stepIndex: 0,
      kind: "intent",
      summary: "Batch click: 2 link controls",
      candidates: [],
    };
    const opened = fold([SUBMIT, { type: "gate", id: "e1", gate }]);
    expect(opened[0].status).toBe("awaiting");
    const closed = threadReducer(opened, { type: "gate", id: "e1", gate: null });
    expect(closed[0].status).toBe("running");
    expect(closed[0].gate).toBeNull();
  });

  it("freezes a settled entry's frame so a later run cannot rewrite its evidence", () => {
    const settled = fold([
      SUBMIT,
      { type: "frame", id: "e1", frame: "first" },
      {
        type: "settled",
        id: "e1",
        at: 1_500,
        result: {
          status: "completed",
          completedSteps: 1,
          totalSteps: 1,
          stoppedAt: null,
          chips: [],
          save: null,
        },
      },
      { type: "frame", id: "e1", frame: "second" },
    ]);
    expect(settled[0].frame).toBe("first");
    expect(settled[0].elapsedMs).toBe(500);
    expect(settled[0].status).toBe("completed");
  });

  it("prefers the backend's settle-time capture over the last live frame", () => {
    // Direct opens can freeze the live screencast on the launch
    // placeholder; the one-shot capture taken at settle is the evidence.
    const settled = fold([
      SUBMIT,
      { type: "frame", id: "e1", frame: "placeholder" },
      {
        type: "settled",
        id: "e1",
        at: 1_500,
        result: {
          status: "completed",
          completedSteps: 0,
          totalSteps: 0,
          stoppedAt: null,
          chips: [],
          save: null,
        },
        finalFrame: "amazon-at-settle",
      },
    ]);
    expect(settled[0].frame).toBe("amazon-at-settle");
  });

  it("keeps the last live frame when the backend reports no capture", () => {
    const settled = fold([
      SUBMIT,
      { type: "frame", id: "e1", frame: "live" },
      {
        type: "settled",
        id: "e1",
        at: 1_500,
        result: {
          status: "completed",
          completedSteps: 0,
          totalSteps: 0,
          stoppedAt: null,
          chips: [],
          save: null,
        },
        finalFrame: null,
      },
    ]);
    expect(settled[0].frame).toBe("live");
  });

  it("prefers a backend-reported duration over the client stopwatch", () => {
    const [entry] = fold([
      SUBMIT,
      {
        type: "settled",
        id: "e1",
        at: 9_999,
        elapsedMs: 840,
        result: {
          status: "completed",
          completedSteps: 2,
          totalSteps: 2,
          stoppedAt: null,
          chips: [],
          save: null,
        },
      },
    ]);
    expect(entry.elapsedMs).toBe(840);
  });

  it("separates a denial from a failure, because one is a decision", () => {
    const outcome = (status: string) => ({
      status,
      completedSteps: 0,
      totalSteps: 1,
      stoppedAt: 0,
      chips: [],
      save: null,
    });
    const denied = fold([SUBMIT, { type: "settled", id: "e1", at: 1_100, result: outcome("denied") }]);
    expect(denied[0].status).toBe("blocked");
    const failed = fold([SUBMIT, { type: "settled", id: "e1", at: 1_100, result: outcome("failed") }]);
    expect(failed[0].status).toBe("failed");
    const repair = fold([
      SUBMIT,
      { type: "settled", id: "e1", at: 1_100, result: outcome("needs_repair") },
    ]);
    expect(repair[0].status).toBe("blocked");
  });

  it("pre-fills the save name from the backend slug and lets it be edited", () => {
    const entries = fold([
      SUBMIT,
      {
        type: "settled",
        id: "e1",
        at: 1_100,
        result: {
          status: "completed",
          completedSteps: 1,
          totalSteps: 1,
          stoppedAt: null,
          chips: [],
          save: { via: "run", runId: "run-1", suggested: "download-invoices" },
        },
      },
    ]);
    expect(entries[0].saveName).toBe("download-invoices");
    const renamed = threadReducer(entries, { type: "rename", id: "e1", saveName: "invoices" });
    expect(renamed[0].saveName).toBe("invoices");
    const saved = threadReducer(renamed, { type: "saved", id: "e1", savedId: "4" });
    expect(saved[0].savedId).toBe("4");
  });

  it("appends turns without reordering and tracks the live one", () => {
    const entries = fold([
      SUBMIT,
      {
        type: "settled",
        id: "e1",
        at: 1_100,
        result: {
          status: "completed",
          completedSteps: 1,
          totalSteps: 1,
          stoppedAt: null,
          chips: [],
          save: null,
        },
      },
      { type: "submit", id: "e2", lane: "task", prompt: "Replay macro reports", at: 2_000 },
    ]);
    expect(entries.map(entry => entry.id)).toEqual(["e1", "e2"]);
    expect(activeEntry(entries)?.id).toBe("e2");
    const done = threadReducer(entries, {
      type: "failed",
      id: "e2",
      at: 2_100,
      message: "nope",
      code: null,
    });
    expect(activeEntry(done)).toBeNull();
  });
});

describe("provenance", () => {
  it("reads the tier from which lane answered", () => {
    expect(tierFor("saved")).toBe("playbook");
    expect(tierFor("ephemeral")).toBe("search");
  });

  it("prefers the re-anchor line, because it records where confinement moved", () => {
    const host = anchorFromLines([
      "route_proposed:www.google.com/search · source: SearchFallback",
      "portal_reanchored: https://www.google.com/ → https://github.com/account/billing/history",
      "ax_snapshot_telemetry: total_nodes=142, target_noun_matches=3 (noun='invoice')",
    ]);
    expect(host).toBe("github.com");
  });

  it("falls back to the proposed entry, and to nothing when neither is present", () => {
    expect(anchorFromLines(["route_proposed:github.com/account/billing · source: SavedPlaybook"])).toBe(
      "github.com",
    );
    // A first-ever anchor journals `none` on the left, which is not a host.
    expect(anchorFromLines(["portal_reanchored: none → https://amazon.in/"])).toBe("amazon.in");
    expect(anchorFromLines(["ax_resync_counter: before_discard=0 after_drain=0"])).toBeNull();
    expect(anchorFromLines([])).toBeNull();
  });

  it("drops blank telemetry lines rather than rendering empty rows", () => {
    expect(telemetryLines("one\n\n  \ntwo")).toEqual(["one", "two"]);
    expect(telemetryLines(null)).toEqual([]);
    expect(telemetryLines(undefined)).toEqual([]);
  });
});

describe("output chips", () => {
  it("names each approved control so a batch is legible after the gate closes", () => {
    const chips = dispatchChips(1, 1, [
      { index: 0, label: "Download", role: "link", isLandmark: false, container: "INV-001" },
      { index: 1, label: "", role: "link", isLandmark: false, container: null },
    ]);
    expect(chips.map(chip => chip.label)).toEqual(["1/1 steps", "Download", "(unnamed)"]);
    expect(chips.every(chip => chip.kind === "data")).toBe(true);
  });

  it("shows a downloaded file by name and keeps its index for opening it", () => {
    const [chip] = taskChips([{ path: "C:\\runs\\7\\invoice_aug2026.pdf", bytes: 2048 }]);
    expect(chip).toEqual({
      kind: "file",
      label: "invoice_aug2026.pdf",
      detail: "2048 bytes · C:\\runs\\7\\invoice_aug2026.pdf",
      fileIndex: 0,
    });
  });
});

describe("describeAction", () => {
  it("describes a recorded action in words, including the value a fill applies", () => {
    expect(describeAction({ type: "navigate", url: "https://example.com/" })).toBe(
      "Open page · https://example.com/",
    );
    expect(describeAction({ type: "submit", selector: "form#pay" })).toBe("Submit form · form#pay");
    expect(describeAction({ type: "fill", selector: "#q", value: "june" })).toBe(
      "Apply filter · #q = june",
    );
    expect(describeAction({ type: "download_links", selector: "a.report" })).toBe(
      "Download files · a.report",
    );
  });
});

const OFFER_LINE =
  "shortcut_offer: 'amazon' → https://www.amazon.in · save to skip grounding next time";

describe("shortcutOfferFromLines", () => {
  it("parses the backend's post-landing offer line into site and url", () => {
    expect(shortcutOfferFromLines([OFFER_LINE])).toEqual({
      site: "amazon",
      url: "https://www.amazon.in",
    });
  });

  it("ignores every other journal line", () => {
    expect(
      shortcutOfferFromLines([
        "route_proposed:www.amazon.in/ · source: DomainGrounded",
        "portal_reanchored: google.com → www.amazon.in",
      ]),
    ).toBeNull();
  });

  it("returns null for a malformed offer line instead of a half offer", () => {
    expect(shortcutOfferFromLines(["shortcut_offer: amazon"])).toBeNull();
    expect(shortcutOfferFromLines(["shortcut_offer:"])).toBeNull();
  });
});

describe("shortcut offer state", () => {
  it("offers nothing when the journal has no offer line — the card only exists after a real landing", () => {
    const [entry] = fold([
      SUBMIT,
      {
        type: "notes",
        id: "e1",
        lines: ["route_proposed:www.amazon.in/ · source: DomainGrounded"],
      },
    ]);
    expect(entry.shortcutOffer).toBeNull();
    expect(entry.shortcutSaved).toBe(false);
  });

  it("derives the offer from the journal line once it arrives", () => {
    const [entry] = fold([SUBMIT, { type: "notes", id: "e1", lines: [OFFER_LINE] }]);
    expect(entry.shortcutOffer).toEqual({ site: "amazon", url: "https://www.amazon.in" });
  });

  it("marks the offer saved on acceptance", () => {
    const [entry] = fold([
      SUBMIT,
      { type: "notes", id: "e1", lines: [OFFER_LINE] },
      { type: "shortcutSaved", id: "e1" },
    ]);
    expect(entry.shortcutSaved).toBe(true);
    expect(entry.shortcutOffer).toEqual({ site: "amazon", url: "https://www.amazon.in" });
  });

  it("a declined offer stays gone even if more notes arrive", () => {
    const entries = fold([
      SUBMIT,
      { type: "notes", id: "e1", lines: [OFFER_LINE] },
      { type: "shortcutDismissed", id: "e1" },
    ]);
    expect(entries[0].shortcutDismissed).toBe(true);
    const [entry] = threadReducer(entries, {
      type: "notes",
      id: "e1",
      lines: ["ax_snapshot: 42 nodes"],
    });
    expect(entry.shortcutDismissed).toBe(true);
    expect(entry.shortcutOffer).toEqual({ site: "amazon", url: "https://www.amazon.in" });
  });
});
