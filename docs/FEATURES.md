# Implemented features

This inventory follows current code. It does not imply real-portal acceptance or production readiness. See [STATUS.md](STATUS.md) for verification limits.

## Sessions and browser visibility

- Explicitly consented local-profile import for Chrome, Brave, and Edge, with macOS Keychain and Windows DPAPI implementations.
- Profile fallback, temporary cookie database/WAL copies, CBC/GCM decryption, scoped SSO cookie selection, best-effort localStorage hydration, and user-agent mirroring.
- Companion-extension session sync over loopback.
- Manual login in the managed Chromium window, with an in-app status/continue panel for reauthentication.
- Polled JPEG viewport with target highlights and an optional live screencast; headless context acquisition, release, and headed takeover.

Import cannot promise zero re-login, zero 2FA, or anti-bot acceptance. A source browser's unsupported encryption falls back to manual login.

## Workflows

- Task workspace replays existing macro files by workflow name.
- Backend task API supports script-planned first-run file-download workflows.
- Workflow builder saves role/label semantic steps, previews a live match, and lists/runs saved playbooks.
- Command bar resolves saved playbooks or ad-hoc single/batch intents. Optional intent-provider parsing is available; default parsing is deterministic.
- Semantic matching uses labels and surrounding text, with support for contextual identifiers, ordinals, and plural targets. Execution clicks the grounded control; selecting a textbox role is not a general form-filling feature.
- Curated entry routes support navigation before grounding. This is not unrestricted site discovery or a model-generated browsing plan.
- Plural execution caps candidates at 30 and uses batch plus per-click approvals.
- Completed command-bar runs can be saved by run ID, with an optional description. Playbooks persist in SQLite and replay from the workflow list.

## Reliability and approvals

Task checkpoints preserve step state and mark unfinished work interrupted after restart. Missing or invalid targets stop execution. Task selector repair can call a configured adapter using stripped local structure; wait repair does not repeat the preceding action.

Clicks, non-secret fills, and submit actions in legacy execution require approval. Semantic intents also require approval. Navigation and typed download-links actions do not use that gate. Decisions are single-use and expire; there is no configurable action-risk policy or ATS submission workflow.

Downloads are correlated with CDP completion and checked locally. Task result cards expose Open File and Show in Folder. Run summaries, session outcomes, startup build identity, and grounding diagnostics are journaled locally.

## Current boundaries

Cmd/Ctrl+K contains only Connect a portal; the command bar is separate. The picker backend has no mounted frontend picker. Playbook import/export, delete/rename, scheduling, ATS forms, profile/resume storage, Kanban, expense tables, price monitoring, Notion/Slack export, and team features are not implemented.

[Future ideas](FUTURE_FEATURES.md) are proposals rather than shipped capabilities.
