# Proof of Concept (POC) Plan
## Autonomous Web Action Studio

**Purpose of this document:** define the smallest, cheapest test that tells you honestly whether this product works — technically and as something people actually want — before you commit to the full MVP build in PROJECT_INIT.md.

---

## 1. What This POC Is Actually Testing (explicit hypotheses)

Every one of these must be treated as an open question, not an assumption:

1. **H1 — Session mechanics work in the real world.** 1-Click Local Cookie Sync can actually authenticate the embedded Chromium instance against real portals, and the manual-login fallback catches the cases where it can't.
2. **H2 — The macro engine delivers on its promise.** A first-run, LLM-planned task can be serialized into a replayable CDP macro, and repeat runs actually execute faster/cheaper without a fresh LLM call.
3. **H3 — Self-healing is real, not aspirational.** When a site's DOM changes, the targeted-repair loop can fix a broken selector without a full re-plan.
4. **H4 — The dual-pane view is legible to a non-technical person.** Someone who didn't build this can watch a run and correctly describe what it's doing and whether they'd trust it.
5. **H5 — This saves real time on a real task.** Not a synthetic demo — an actual person's actual recurring chore, measured before/after.
6. **H6 — The flagship use case (Job Application Engine) is safe enough to pilot.** The Sentinel Gate approval floor genuinely prevents any accidental submission, and a small real pilot doesn't produce any account warnings or suspensions.
7. **H7 — People want to keep using it.** Not "cool demo" reactions — actual voluntary reuse within the pilot window.

If any of H1–H4 fail, the architecture needs rework before anything else matters. H5–H7 are the actual product bet.

## 2. Structure: Two Gated Phases, Not One Big Test

Running the flagship, highest-stakes use case (Job Application Engine) against real ATS accounts before you've proven the underlying mechanism works is the wrong order of operations — a bug in cookie sync or the macro engine shouldn't be discovered on someone's real job search. Phase A isolates and de-risks the mechanism; Phase B is the actual product bet, and it only runs if Phase A clears its bar.

---

## 3. Phase A — Technical Feasibility Spike

**Anchor Playbook: Invoice/Bill Harvester** (chosen deliberately — read-only, no state-changing actions, no Sentinel Gate complexity needed, no account-ban risk, cleanly isolates H1–H4).

### 3.1 Scope — Build Only This
- Tauri v2 (Rust) shell, macOS only, no installer polish, no auto-update.
- One embedded, CDP-controllable Chromium instance.
- Session Sync Manager: cookie import from local Chrome/Brave, with the manual-login fallback — this is the single highest-risk component, build and test it first, in isolation, before anything else.
- Orchestration engine: first-run LLM plan + macro record; repeat-run macro replay; targeted self-heal on a broken selector.
- Minimal dual-pane UI: left pane renders the live Chromium viewport (no cursor/DOM-highlight polish yet); right pane shows a plain step list and the downloaded-file/summary output. No Playbook Cards, no Cmd+K, no scheduling.
- Basic local storage (SQLite) for run history — enough to answer the metrics below, not the full production data model.

### 3.2 Explicitly Cut From Phase A
- Sentinel Gate (not needed — no state-changing actions in this Playbook).
- Job Application Engine, Deal Radar (Phase B and beyond).
- Team features, native app reach, Windows, scheduling, exports to Notion/Slack (local file is enough).
- Any UI polish beyond "a non-technical person can follow what's happening."

### 3.3 Test Portals (concrete, not hypothetical)
Pick 5 real portals across different auth patterns to stress-test cookie sync realistically: e.g., one major utility company, one telecom carrier, one insurance provider, one SaaS billing portal (e.g., a subscription you actually pay for), one bank or credit-card portal with stronger 2FA. Deliberately include at least one portal known for aggressive session/2FA behavior — that's where you'll actually learn something.

### 3.4 Pass Bar (Go/No-Go for Phase B)
- **Cookie sync**: succeeds on at least 4 of 5 test portals without a fresh login; on the portal(s) where it fails, the manual-login fallback works cleanly with zero hard failures or crashes.
- **Macro replay**: at least 90% of repeat runs on a given portal execute via the replayed macro (no LLM call) rather than a fresh plan.
- **Self-healing**: deliberately break one selector (e.g., after a portal's minor UI update, or by editing the macro file) and confirm the targeted-repair loop fixes it without a full re-plan.
- **Legibility**: 3 non-technical people (not you, not the builder) watch a live run cold, with no explanation beforehand, and can correctly describe what it's doing and whether they'd trust it unsupervised.
- **Real time saved**: run this on your own actual bills for 2 consecutive months and measure wall-clock time vs. doing it manually the way you did before.

**If cookie sync fails on a majority of real-world portals even with the fallback**, that's a serious signal — it means the core convenience promise degrades to "just log in manually" most of the time, and the architecture needs rework before Phase B, not after.

---

## 4. Phase B — Product & Trust Validation (only if Phase A passes)

**Anchor Playbook: Job Application Engine.**

### 4.1 Scope — Add Only This On Top of Phase A's Engine
- Master Profile + resume/project vault (minimal version — enough fields to fill a real ATS form, not the full v1 feature set).
- ATS detection for 2–3 platforms only (recommend Workday and Greenhouse — high real-world coverage, well-documented form structures).
- **Sentinel Gate, fully implemented and non-bypassable**: every single submission requires an explicit human approval with a real preview of the filled form. This is not optional for Phase B — it's the entire safety mechanism the pilot depends on.
- A minimal Kanban view (Applied / Interview / Offer) — just enough to track pilot outcomes, not the polished v1 version.

### 4.2 Pilot Design
- **5–10 real job seekers**, ideally people who are actually job-hunting right now (not friends doing you a favor with no stakes) — pick people for whom this solves a real, current pain.
- Before the pilot: a short, honest briefing on what the tool does, that it's experimental, and the (small but real) risk that a target platform could flag automated activity on their account — informed consent matters here, these are people's real accounts and real job searches.
- Each participant uses it for real applications over 1–2 weeks, with every submission going through the Sentinel Gate.
- Instrument every run: time from start to submission-ready, number of Sentinel Gate approvals vs. edits/rejections, any error or unexpected behavior.

### 4.3 Pass Bar (Go/No-Go for full MVP build)
- **Zero account warnings or suspensions** across all pilot participants during the test window — this is a hard gate, not a soft metric.
- **Sentinel Gate integrity**: 100% of submissions went through an explicit human approval — verified by log, not by self-report.
- **Real time saved**: majority of participants report meaningfully less time per application (target: 50%+ reduction) with output quality they're comfortable submitting.
- **Voluntary reuse**: majority of participants use it again for a second application without being prompted.
- **Qualitative pull**: ask directly — "would you be disappointed if this went away?" and "would you pay for this?" — a small pilot's honest answer here matters more than a synthetic survey score.

**If any participant's account gets flagged or suspended**, stop the pilot, treat it as a critical finding, and revisit the pacing/rate-limit/Sentinel Gate design before running another pilot — do not proceed to MVP on the current design.

---

## 5. Timeline (lean, sequential)

| Week | Focus |
|---|---|
| 1 | Session Sync Manager spike in isolation — cookie import + fallback, tested against all 5 Phase A portals before building anything else |
| 2 | Orchestration engine + macro record/replay + minimal dual-pane, wired to the Invoice Harvester flow |
| 3 | Self-healing loop; run the 3-person legibility test; start the 2-month real-bill time-tracking |
| 4 | Phase A go/no-go review against the pass bar in Section 3.4 |
| 5–6 | (If go) Build Job Application Engine on top: Master Profile, ATS detection for 2 platforms, full Sentinel Gate |
| 7–8 | Recruit and run the Phase B pilot (5–10 real job seekers), instrumented throughout |
| 9 | Phase B go/no-go review against the pass bar in Section 4.3 — decision point for the full MVP build |

## 6. What You Need in Place Before Starting

- Instrumentation from day one: every run, every cookie-sync attempt (success/fallback), every macro replay vs. fresh-plan event, every Sentinel Gate decision — logged locally so the pass-bar metrics are measured, not guessed.
- A short internal note on the target ATS platforms' terms of use before Phase B — not a full legal review, but enough awareness to brief pilot participants honestly about the real risk they're taking on.
- A kill switch: a simple way to immediately disable the Job Application Engine for all pilot users if something goes wrong mid-pilot.

## 7. Decision Framework After Phase B

- **Both phases pass their bars** → proceed to the full MVP build per PROJECT_INIT.md, now with real data instead of assumptions about macro-replay rates, cookie-sync reliability, and time saved.
- **Phase A passes, Phase B shows time savings but any account-risk signal** → do not proceed to Job Application Engine at MVP scope; consider leading the public launch with Invoice Harvester and Deal Radar only, revisiting Job Application Engine once pacing/detection mitigations are stronger.
- **Phase A fails on cookie sync specifically** → this is a architecture-level finding, not a bug to patch — revisit the session model (Session Sync Manager design in ARCHITECTURE.md, Section 3.3) before spending more time downstream.
- **Users don't reuse voluntarily even though the mechanics work** → this is a product-desirability finding, not a technical one — worth pausing before building the Team Layer or additional Playbooks on a habit that isn't forming.
