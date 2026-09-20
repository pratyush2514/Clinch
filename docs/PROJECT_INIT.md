# Project Initialization
## Autonomous Web Action Studio

---

## 1. Locked Stack Decisions (no longer open questions)

- **App shell: Tauri v2 (Rust).** Electron rejected — 5–10MB installer and 30–50MB RAM footprint vs. Electron's 150MB+ on both counts; native OS Keychain/Keytar integration; sub-second IPC.
- **Automation surface:** a separately embedded, real Chromium instance (not Tauri's default WKWebView) controlled via CDP, rendered into the left pane.
- **Session mechanism:** 1-Click Local Chrome/Brave Cookie Sync as the default, with mandatory manual-login fallback.
- **UI:** mandated dual-pane viewport (live webview + Spatial Canvas) plus a global Cmd+K Intent Bar.
- **Execution model:** self-healing CDP macro engine (LLM plans once, replays via native CDP thereafter, heals only broken selectors).

## 2. Repository Structure

```
autonomous-studio/
├── apps/
│   └── desktop/                 # Tauri v2 (Rust) shell + frontend UI (dual-pane, Cmd+K, Playbook cards, Sentinel Gate modals)
├── packages/
│   ├── orchestration-engine/    # Task→Plan→Step loop, checkpointing, Sentinel Gate rules evaluation
│   ├── browser-driver/          # Embedded Chromium control via CDP, agent cursor/DOM-highlight overlays
│   ├── macro-engine/            # CDP macro record / replay / self-heal
│   ├── session-sync/            # 1-Click Chrome/Brave cookie import, OS-Keychain-gated decryption, manual-login fallback
│   ├── filesystem-tool/         # Scoped local file access layer
│   ├── credential-vault/        # OS Keychain integration, credential injection (no raw secrets to LLM)
│   ├── playbook-store/          # Playbook schema, versioning, import/export (JSON)
│   └── llm-provider/            # Provider-agnostic reasoning interface (Anthropic/OpenAI/etc.)
├── docs/
│   ├── PRD.md
│   ├── TRD.md
│   ├── ARCHITECTURE.md
│   └── FEATURES.md
└── scripts/
    └── golden-path-tests/       # Recorded real-world task regressions across all 3 anchor Playbooks
```

## 3. Milestone Plan

| Phase | Goal | Exit Criteria | Status (see `STATUS.md`) |
|---|---|---|---|
| 0. Spike | Prove the core loop on Invoice Harvester | Session sync (+ fallback) working; macro records on run 1, replays on run 2, on 5 real portals | Engine done; 5-portal validation open |
| 1. MVP | Ship all 3 anchor Playbooks | Job Application Engine, Invoice Harvester, Deal Radar all functional; dual-pane UI, Cmd+K, Sentinel Gate hard floor on Job Application Engine | Partial: Invoice flow only; Job/Deal unstarted |
| B1. Session hardening | Zero-touch sync on real machines | Edge/AES-GCM/DPAPI, shadow-copy reads, hydration, wildcard SSO, UA mirror, Brave default, App-Bound diagnostics | Implemented, unit + fixture proven |
| B2. Dynamic discovery | Selectors optional | AX tree, Set-of-Marks, semantic executor, picker fix | Implemented, live-Chromium proven |
| B3. Playbooks end-to-end | Save, run, command | Schema + v1 migration, persistence, runner, approvals, builder, command bar | Implemented, approval-gated |
| 2. Reliability | Harden self-healing, add scheduling | Macro-reuse-rate and cookie-sync-success-rate metrics live; golden-path suite passing on every engine/model update | Not started |
| 3. Team Layer | Shared Playbooks, roles, audit export | First team of 3+ using shared Playbooks with approval delegation | Not started |
| 4. Native Reach | Accessibility API spike | Feasibility validated on 2–3 target native apps before committing to full build | Not started |

## 4. Immediate Next Steps (Week 1)

1. Scaffold the Tauri v2 (Rust) app shell with native menu bar, Keychain access, and filesystem permission plumbing.
2. Stand up the embedded Chromium instance (evaluate bundled Chromium vs. CEF vs. Playwright-managed) and confirm CDP control from the Rust core.
3. Build the `session-sync` package: local cookie-store reader, OS-Keychain-gated decryption, CDP cookie injection, and the manual-login fallback path — test against real Chrome/Brave installations early, since this is the highest-fragility component.
4. Build the orchestration engine skeleton: `Task → Plan → Step` loop with checkpoint persistence (SQLite).
5. Build the minimal dual-pane UI: left pane rendering the live Chromium viewport with cursor/DOM-highlight overlays, right pane rendering a basic step list (no editing yet).
6. Implement one hardcoded Sentinel Gate (e.g., "form submission") end-to-end, including the preview UI.
7. Pick Invoice Harvester as the Phase 0 pilot task and validate against 5 real portals, not synthetic demos.

## 5. Definition of Done for MVP (Phase 1)

- All three anchor Playbooks (Job Application Engine, Invoice Harvester, Deal Radar) work end-to-end for at least 3 real portals/sites each.
- 1-Click Cookie Sync succeeds on a majority of real-world Chrome/Brave setups tested, with the manual-login fallback verified working on the remainder (never a hard failure).
- A completed run can be saved as a Playbook, rerun with new inputs, and scheduled; repeat runs measurably hit the macro-replay path (not a fresh LLM plan) the majority of the time.
- Every Job Application Engine submission requires a Sentinel Gate approval — verified by explicit test, not just by default configuration.
- Credentials and imported cookie values are never visible in any log, event, or LLM context — verified by explicit test.
- A visible, accurate data-egress log exists for every run.
- Pilot group of 5–10 target-persona users has completed at least one real (non-demo) task per anchor Playbook.

## 6. Success Gate Before Building Phase 3 (Team Layer)

Do not build team/sharing features until Playbook reuse rate and macro-replay rate (PRD.md, Section 8) show individual users are already rerunning saved Playbooks organically and hitting the cheap-replay path. Building collaboration on top of a habit that doesn't exist yet is the most common way this category of product fails.
