# Implemented features

This inventory follows current code. It does not imply real-portal acceptance or production readiness. See [STATUS.md](STATUS.md) for verification limits.

> Constitution note (2026-09-27): [CONSTITUTION.md](CONSTITUTION.md) plans to
> converge the command bar / workflow builder / task workspace into one
> thread with playbooks as rows under it. Until that lands, this file
> describes the UI as built.

## Sessions and browser visibility

- Explicitly consented local-profile import for Chrome, Brave, and Edge, with macOS Keychain and Windows DPAPI implementations.
- Profile fallback, temporary cookie database/WAL copies, CBC/GCM decryption, scoped SSO cookie selection, best-effort localStorage hydration, and user-agent mirroring.
- Companion-extension session sync over loopback.
- Manual login in the managed Chromium window, with an in-app status/continue panel for reauthentication.
- **Bot-mitigation handoff ladder.** A detected challenge first auto-escalates silently (L1: the session restarts off-screen headed and polls ~30s for clearance), then surfaces a consent-gated session-sync card (L1.5), and only then offers Take Control (L2) — a headed managed window already on the challenged page. A cleared L1 escalation is stood down with no visible window left behind. Interactive gates automation cannot clear skip the L1 wait and go straight to the card.
- **Session lending.** A consent-gated one-way sync of a site's cookies from the Companion extension in the daily browser into Clinch's app-owned profile, offered from a challenge card or a signed-out landing. A tap is consent for exactly one attempt; the server's exact cookie expiry is preserved (persistent-by-default; true session cookies stay session-only); nothing is written back to the daily browser. The bridge survives MV3 suspension (socket in a watchdog-recreated offscreen document, bounded-backoff reconnect, correlated heartbeat nonces). A lending card polls bridge state while mounted: no companion means Sync stays disabled in an honest "waiting" state, one companion shows its identity, and two or more offer a source-browser picker. A confirmed login shows `synced (persisted)`; a later logged-out probe brings the offer back. Journals record host and cookie counts only.
- **"Forget this site".** Deletes the site's cookies from Clinch's profile via CDP and clears any remembered identity (profile URL) for the origin, returning the sync card to the signed-out offer; the daily browser is untouched.
- Polled JPEG viewport with target highlights and an optional live screencast; headless context acquisition, release, and headed takeover. **Open Preview** raises a near-fullscreen overlay dressed as a browser window (tab title, URL pill with secure icon, live/final badge): live streaming on a running entry, the frozen final frame on a settled one.

Import cannot promise zero re-login, zero 2FA, or anti-bot acceptance. A source browser's unsupported encryption falls back to manual login.

## Workflows

- Task workspace replays existing macro files by workflow name.
- Backend task API supports script-planned first-run file-download workflows.
- Workflow builder saves role/label semantic steps, previews a live match, and lists/runs saved playbooks.
- Command bar resolves saved playbooks or ad-hoc single/batch intents. Optional intent-provider parsing is available; default parsing is deterministic.
- Semantic matching uses labels and surrounding text, with support for contextual identifiers, ordinals, and plural targets. Execution clicks the grounded control; selecting a textbox role is not a general form-filling feature.
- Direct-open entry routing resolves `open X` without a curated table: a saved site shortcut, a structured site directory (Brave API when configured, keyless DuckDuckGo otherwise), or the fenced domain grounder — otherwise an honest miss that asks the user rather than scraping a search page. After a grounded landing, the Action Thread offers a consent-gated shortcut save; accepted shortcuts resolve with zero model calls on later runs. This is not unrestricted site discovery or a model-generated browsing plan.
- Plural execution caps candidates at 30 and uses batch plus per-click approvals.
- Completed command-bar runs can be saved by run ID, with an optional description. Playbooks persist in SQLite and replay from the workflow list.

## In-page goals (account home)

- **Generic account-home pursuit.** "open my profile on reddit", "open my account", and similar prompts map the identity noun (`profile`/`account`) to a generic goal class through a closed noun table — no site names, no URL templates, no selectors. The worker runs a bounded 3-click walk of the header identity chrome: it probes auth first (a signed-out page is never clicked), opens the identity control, reads the live page's revealed destination with an untruncated AX snapshot, and a Rust verifier confirms the landing is same-site, non-root, and carries the page-revealed username before anything counts as success. Retried controls are excluded by stable identity (role + name), not node ids; every click's effect is journaled with raw AX node counts.
- **Identity memory.** A verified profile URL is remembered per origin and goal class (www/case-normalized, https-only, credential-rejecting); repeat prompts navigate straight to the remembered page and re-verify the live landing, falling back to the worker on any disagreement. The page reveals the URL — it is never guessed from the prompt.
- **Miss recovery.** If the worker cannot verify a destination, the run FAILEDs naming what it tried, and the card offers "Take control" so you can click the avatar yourself in the still-on-portal window (existing command, no new IPC).
- **Origin www-folding.** `www.` folds both ways everywhere origin equality matters — drift check, Navigate validation, `check_origin`, auth signal, identity memory, verifier — with scheme and port strict and other subdomains untouched.

## Reliability and approvals

Task checkpoints preserve step state and mark unfinished work interrupted after restart. Missing or invalid targets stop execution. Task selector repair can call a configured adapter using stripped local structure; wait repair does not repeat the preceding action.

Clicks, non-secret fills, and submit actions in legacy execution require approval. Semantic intents also require approval. Navigation and typed download-links actions do not use that gate. Decisions are single-use and expire; there is no configurable action-risk policy or ATS submission workflow.

Downloads are correlated with CDP completion and checked locally. Task result cards expose Open File and Show in Folder. Run summaries, session outcomes, startup build identity, and grounding diagnostics are journaled locally.

## Current boundaries

Cmd/Ctrl+K contains only Connect a portal; the command bar is separate. The picker backend has no mounted frontend picker. Playbook import/export, delete/rename, scheduling, ATS forms, profile/resume storage, Kanban, expense tables, price monitoring, Notion/Slack export, and team features are not implemented.

[Future ideas](FUTURE_FEATURES.md) are proposals rather than shipped capabilities.
