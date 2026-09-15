# Architecture & Approach
## Autonomous Web Action Studio

---

## 1. High-Level Architecture

```
┌───────────────────────────────────────────────────────────────────┐
│              Tauri v2 (Rust) Desktop Shell — LOCKED                │
│      (native menus, OS Keychain access, filesystem, ~30-50MB RAM,  │
│       5-10MB installer — Electron rejected: 150MB+ bundle/RAM)     │
│                                                                     │
│  ┌───────────────────────────┐   ┌───────────────────────────────┐│
│  │  LEFT: Live Interactive    │   │  RIGHT: Spatial Canvas        ││
│  │  Webview                   │   │  - Scrubbable Action Canvas   ││
│  │  - Embedded, real Chromium │   │    (step timeline, replayable)││
│  │    instance (CDP control,  │   │  - Dynamic data tables         ││
│  │    NOT Tauri's default     │   │  - Playbook Cards              ││
│  │    WKWebView)               │   │  - Export: file/Notion/Slack  ││
│  │  - Agent cursor + DOM      │   └───────────────┬───────────────┘│
│  │    highlight overlays      │                    │                │
│  │  - 1-click manual takeover │                    │                │
│  │    bar (always visible)    │                    │                │
│  └──────────────┬──────────────┘                   │                │
│                 │                                    │              │
│         ┌───────▼────────────────────────────────────▼──────────┐  │
│         │            Orchestration Engine (local process)         │  │
│         │  - Task → Plan → Step loop (first run)                  │  │
│         │  - Self-healing CDP macro record / replay / heal        │  │
│         │  - Sentinel Gate rules evaluation                        │  │
│         │  - Checkpointing + event log (SQLite)                   │  │
│         └───────┬─────────────────────┬────────────┬─────────────┘  │
│                 │                     │             │                │
│      ┌──────────▼─────────┐  ┌────────▼──────┐ ┌────▼──────────┐    │
│      │ Session Sync Manager│  │ Filesystem /   │ │ Credential     │  │
│      │ - 1-Click Local     │  │ Local Tool     │ │ Vault (OS      │  │
│      │   Chrome/Brave      │  │ Layer (scoped) │ │ Keychain-      │  │
│      │   Cookie Import     │  └────────────────┘ │ backed)        │  │
│      │ - OS-Keychain-gated │                      └────────────────┘  │
│      │   decryption        │                                          │
│      │ - Fallback: manual  │                                          │
│      │   login in app-owned│                                          │
│      │   profile           │                                          │
│      └─────────────────────┘                                          │
│                                                                       │
│  ┌───────────────────────── Cmd+K Intent Bar ─────────────────────┐  │
│  └─────────────────────────────────────────────────────────────┘    │
└──────────────────────────────┬────────────────────────────────────┘
                                │  (minimal, scoped context — only on
                                │   first-run planning and selector
                                │   self-healing, never on macro replay)
                                ▼
                  ┌───────────────────────────┐
                  │   LLM Reasoning Provider    │
                  │  (BYO key: Anthropic/OpenAI │
                  │   /etc., provider-agnostic) │
                  └───────────────────────────┘
```

## 2. Design Principles (in priority order)

1. **Frictionless authenticated access, transparently obtained.** 1-Click Local Cookie Sync is the default session mechanism — it removes re-login and 2FA friction on Workday, Amazon, and other authenticated portals from minute one. The OS-level permission prompt this requires is paired with an in-app consent explanation beforehand, and a manual-login fallback always exists if sync fails.
2. **Visibility over autonomy.** Every action is inspectable before, during, and after execution via the mandated dual-pane layout — the primary differentiator vs. cloud-VM and terminal-daemon competitors.
3. **Pay the LLM once per workflow, not once per click.** The self-healing CDP macro loop is a structural requirement: record on first run, replay via native CDP sockets thereafter, heal only the specific broken selector.
4. **Reversibility.** Pause/edit/resume beats restart-from-scratch; the orchestration engine is checkpointable at every step boundary.
5. **Least privilege.** Filesystem and credential access are scoped per-Playbook/per-run; imported cookies never persist outside the app's own encrypted store.
6. **A safety floor that isn't configurable away.** For the Job Application Engine's submissions, the Sentinel Gate requirement is a hard product line, not a setting that can be disabled.

## 3. Core Components

### 3.1 Orchestration Engine
Per-`Run` state machine: on first run, requests a plan from the LLM and executes step-by-step, serializing each step into a deterministic CDP `Macro`. On repeat runs, replays the `Macro` directly via native CDP socket commands. On a broken selector, isolates just that step, sends minimal DOM context to the LLM for a targeted repair, updates the `Macro`, and resumes. Emits a structured event per step to both panes and appends to an immutable local event log. Checkpoints after every step.

### 3.2 Live Interactive Webview (Left Pane / Browser Driver Layer)
A real, embedded Chromium instance (not Tauri's native OS webview) controlled via CDP, running sandboxed and isolated from the user's default browser process. Renders agent cursor and DOM-highlight overlays during execution, and hosts the always-visible manual-takeover bar. Anti-detection pacing guardrails live here.

### 3.3 Session Sync Manager
Implements 1-Click Local Chrome/Brave Cookie Import: reads the local browser's cookie store, decrypts via the OS-Keychain-gated Safe Storage key, and injects the relevant cookies into the embedded Chromium instance's cookie jar via CDP. Falls back to a one-time manual login in an app-owned persistent profile whenever import fails (unsupported browser, decryption failure, missing domain) — this fallback path is a first-class part of the component, not an afterthought.

### 3.4 Filesystem / Local Tool Layer
Scoped file access per Playbook — explicit folder/file grants, surfaced to the user in plain language.

### 3.5 Credential Vault
OS Keychain-backed. The orchestration engine resolves a `CredentialRef` to inject a secret directly into a form field via the browser driver — the LLM never sees the raw value.

### 3.6 Spatial Canvas (Right Pane, Frontend)
Consumes the orchestration engine's event stream to render a scrubbable Action Canvas (replayable step timeline), dynamic data tables, and Playbook Cards. Supports export to local file, Notion, or Slack.

### 3.7 Sentinel Gate
Evaluates the rules engine against each state-changing step; renders a human-readable preview and blocks execution until confirmed. Hard-enforces the non-configurable submission-approval floor for the Job Application Engine. Logs every decision immutably.

### 3.8 Playbook Store
Versioned, human-readable (JSON) Playbook definitions, each referencing a recorded `Macro`. Git-diffable by design for future team-sharing.

## 4. Phased Build Approach

**Phase 0 — Spike:** Prove the full loop on Invoice Harvester (lower stakes) — Session Sync Manager (cookie import + fallback) + embedded Chromium + orchestration engine + macro record on first run + macro replay on second run + one Sentinel Gate. Validate against 5 real portals.

**Phase 1 — MVP:** Ship all three anchor Playbooks (Job Application Engine, Invoice Harvester, Deal Radar). Dual-pane UI, Cmd+K, Sentinel Gate (hard submission floor on Job Application Engine), 1-Click Cookie Sync with fallback, local storage.

**Phase 2 — Reliability & Reuse:** Harden self-healing accuracy, add scheduling, surface macro-reuse-rate and cookie-sync-success-rate metrics to users, build the golden-path regression suite.

**Phase 3 — Team Layer:** Shared Playbook libraries, roles, audit-log export, approval delegation.

**Phase 4 — Native App Reach (separate track):** macOS Accessibility API spike for non-browser app control.

## 5. Why This Architecture Fits the Positioning

- **1-Click Cookie Sync** solves the anti-bot/authenticated-site problem that structurally cripples cloud agents, with a real-browser session from the first run — the required consent screen and fallback path are what keep this a feature rather than a liability.
- **Self-healing CDP macros** make "$0 cost, sub-second repeat runs" an architectural property, not a marketing line.
- **Dual-pane + Spatial Canvas** turns output into an artifact (spreadsheet, Kanban board, export) instead of a chat transcript.
- **Tauri v2/Rust** keeps the product feeling like a lightweight workstation rather than a resource-hungry Electron app, matching the "professional tool" positioning.
- **Non-configurable Sentinel Gate floor** on the flagship use case is a hard-coded safety line because the downside is asymmetric and severe for the user this product is trying to help.
