# Clinch product scope

Owner: Pratyush. Reconciled 2026-09-24 against current code.

## Product purpose

Clinch provides a local desktop workspace for repeatable browser actions, visible execution, explicit approvals, and reusable workflows. The implementation is a generic browser workflow tool; it does not ship the previously proposed Job Application Engine, Invoice Harvester product, or Deal Radar.

## Current user journeys

1. Connect a portal through consented source-profile import, companion-extension sync, or manual login in managed Chromium.
2. Describe an action in the command bar or build semantic role/label steps. Inspect the matched controls and approve execution.
3. Save a completed command-bar run as a named playbook, optionally with a description, and replay it from the saved list.
4. Replay an existing selector macro by workflow name in the task workspace. Review step state, repair failures, and downloaded-file results.
5. Observe browser frames and switch a headless context to a visible managed window for manual interaction.
6. Open a site directly from the command bar (`open amazon for me`): the resolver grounds the site name to a domain without showing search results. After a successful first landing, optionally save it as a site shortcut so later prompts open it with no model call.
7. Land on a bot-mitigation challenge (`open claude for me` hitting Turnstile): the run auto-escalates silently first (off-screen headed, ~30s), then offers a consent-gated session sync from the Companion extension in your daily browser, then hands you a headed managed window to solve it once (Take Control). Only the human fallback is ever visible.
8. Land signed out on a site you normally use (`open reddit for me` while logged out in Clinch): instead of a bare "completed", the card offers a one-tap sync of your login from the daily browser. Sync once and Clinch keeps its own persistent copy in its profile — signing out in the daily browser does not sign Clinch out. "Forget this site" revokes Clinch's copy at any time.
9. Enlarge any settled run's final frame with Open Preview: a near-fullscreen browser-chrome view (tab title, URL pill, live/final badge) for reading what the run saw.

The task form no longer collects first-run selectors. Backend script planning remains available, but a new workflow name in that form is not enough to record a macro.

## Implemented product contracts

- Session import requires explicit consent; manual login remains available when import cannot proceed.
- Session lending (challenge card or auth-sync card) is one-way, domain-scoped, and consent-gated: a tap syncs the site's cookies from the daily browser's Companion extension into Clinch's app-owned profile, preserving the server's exact expiry; the daily browser profile is never attached, copied, or written back. "Forget this site" deletes Clinch's copy of the site's cookies.
- Browser execution happens locally in a separate managed Chromium process. The preview is an image surface, not an interactive embedded browser. Background work runs off-screen headed with no visible window; the human fallback (Take Control) is the only path that puts a window on screen.
- Saved macro replay avoids planning calls when selectors resolve. Optional intent parsing and task selector repair may invoke configured adapters. Direct-open routing may invoke the fenced domain grounder (Groq/Ollama, env-configured); it returns only a bare domain, validated in Rust before navigation, and declines cleanly when unconfigured or offline.
- Legacy click/fill/submit actions and semantic intents require approvals. There is no universal risk-classification engine.
- Tasks persist checkpoints and stop on uncertainty; interrupted tasks are not automatically resumed.
- Playbooks persist their step definitions in SQLite. Completed-run save keys are temporary session state, capped at 32 entries.
- Native task file actions resolve stored paths within the completed run's download directory.

## Measurement and acceptance

The backend exposes `get_poc_metrics`: run summaries by status, completed-task macro-replay percentage, and counts of session outcomes. These are implementation metrics, not proof of successful portal authentication, time saved, or voluntary reuse. There is no dedicated frontend metrics dashboard.

The real-world validation plan is [POC.md](POC.md). Its goals remain unmeasured unless supported by a separate recorded pilot. Old fixture timings, dependency audit counts, installer-size estimates, and competitor comparisons are not product guarantees.

## Scope not implemented

Scheduling, prebuilt domain playbooks, arbitrary ATS form filling, a credential/profile/resume vault, price monitoring, extracted expense sheets, a scrubbable action replay UI, Notion/Slack exports, team roles/sharing, native-app automation, and mobile approvals remain future work.

See [FEATURES.md](FEATURES.md) for existing features and [FUTURE_FEATURES.md](FUTURE_FEATURES.md) for retained proposals. Code takes precedence over every document.
