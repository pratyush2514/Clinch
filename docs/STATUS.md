# Clinch implementation status

Reconciled on 2026-09-24 against the current working tree. Code is the source of truth. This file summarizes that code; it does not supersede it.

## Implemented

- Tauri v2/React desktop shell with resizable panes, session controls, command bar, workflow builder, task workspace, and approvals.
- Installed Chromium managed over CDP with an app-owned profile, launched lazily when a task needs it. The background session runs **off-screen headed** (a real compositor positioned off-monitor and OS-hidden, no `--headless` flag) so bot-mitigation probes see a headed browser while the user sees no window; macro replay runs true headless; interactive work (manual login, Take Control, session sync) runs visibly headed. The viewport is mirrored as JPEG into React; a preview is not native browser embedding. `navigator.webdriver` is masked and `AutomationControlled` disabled, but a CDP-driven Chromium remains distinguishable to Cloudflare by design — a human check is routed to the human.
- **Large preview overlay:** "Open Preview" on the screencast card raises a near-fullscreen native `<dialog>` dressed as a browser window — tab label from the settle-time document title, a URL pill from the settle-time page URL with a secure/non-secure icon, and a live/final badge. Running entries keep streaming frames (a large live view); settled entries show the frozen final frame. Escape and backdrop-click close it.
- **Bot-mitigation handoff ladder (L1 → L1.5 → L2).** A run that lands on a detected challenge (Cloudflare / Turnstile / reCAPTCHA human-verification gate, detected by title, challenge-platform URL, and visible text — checked before and outranking any auth probe) flows through:
  1. **L1 auto-escalation** — the attached session restarts as off-screen headed on the same app-owned profile, re-navigates to the challenged URL, and polls ~30s for clearance (a live page on the challenged host is required, so a dead target never reads as cleared). Outcomes journal as `challenge_auto_escalated: <host> · cleared | persistent | failed (<static label>)`. A cleared escalation is stood down gracefully (CDP close flushes clearance cookies to the profile on disk), so the next acquisition launches lazily with no phantom window; a persistent challenge keeps the headed session alive for L2.
  2. **L1.5 session lending** — a challenge card offers a consent-gated one-way sync of the site's cookies from the Clinch Companion extension in the user's daily browser into Clinch's app-owned profile. Interactive gates (e.g. the cross-origin reCAPTCHA checkbox iframe) skip the L1 wait and go straight to the card, since automation cannot clear them.
  3. **L2 Take Control** — the human fallback: a headed managed window already on the challenged page, so the user solves the check once and the profile keeps the clearance naturally.
- **Persistent session lending + auth sync.** A tap on the sync card is consent: exactly one lend attempt per run, desktop-initiated single-use bridge request, server-side domain filter on the settled URL, one-way only (daily browser → Clinch, never write-back). Injection **preserves the server's exact cookie expiry** — Chromium persists lent cookies in the app profile's cookie database ("sync once, stay logged in"); cookies without an expiry stay true session cookies, and no lifetime is invented. Post-sync the page re-navigates and re-probes origin-aware: a challenge run checks the gate, a guest landing checks for an authenticated page. `synced (persisted)` marks a confirmed login; a stale or rejected session reads as logged out and the card simply reappears (self-healing). A "Forget this site" revocation deletes the site's cookies from Clinch's profile via CDP (the daily browser is never touched) and returns the card to the signed-out offer. Journals record host and cookie counts only — never cookie values.
- **Auth-state probe at settle time.** When no challenge is detected, `auth_state()` classifies the landed page from URL, title, and up to 4,000 characters of visible text, failing open to `Unknown`. Authenticated copy (e.g. `log out`/`sign out`) wins over guest markers, login paths (`/login`, `/signin`, `/signup`, `/register`) count as logged out, and generic guest text requires at least two distinct markers. A clean signed-out guest landing surfaces an **auth-sync card** with the same consent-gated session-lending flow; the consent copy is explicit that Clinch keeps an independent copy of the login in its own profile and that signing out in the daily browser does not sign Clinch out. Anything authenticated or unclassifiable keeps the previous silent-success behavior.
- Consented Chrome/Brave/Edge profile import with macOS Keychain/Windows DPAPI paths, App-Bound fallback, temporary cookie/WAL copies, CBC/GCM decryption, scoped SSO cookies, best-effort localStorage hydration, and user-agent mirroring. Imported secrets are not stored in `clinch.db`.
- MV3 companion extension and loopback session bridge (desktop WebSocket server on `127.0.0.1:9223`); manual login and in-app reauthentication status. Bridge errors surface as user-facing reasons: no extension connected, extension timed out, or no cookies for the portal.
- Task state/checkpoint persistence, script-planned backend first runs, versioned macro recording/replay, bounded task selector repair, and interrupted-task recovery.
- Semantic AX/Set-of-Marks execution, context-sensitive matching, ordinals, and plural command batches capped at 30.
- SQLite playbook save/list/run, descriptions, completed command-bar run saving, run-summary journaling, and session/grounding diagnostics.
- Direct-open entry routing with no curated route table: explicit domain in the prompt → account directory (unwired) → LLM intent adapter (unwired) → direct-open grounding ladder **(saved site shortcut → structured site directory → fenced domain grounder)** → honest miss, with a grounded search fallback only for non-direct opens. The directory rung is composite: Brave Search API when `CLINCH_BRAVE_API_KEY` is set, keyless DuckDuckGo HTML as the zero-config fallback, ranked results parsed in memory — the browser never sees a search page. The domain grounder resolves a site name to a bare domain through Groq (`CLINCH_GROUNDER_PROVIDER=groq`, key from `GROQ_API_KEY`, zeroized on drop) or local Ollama (`CLINCH_GROUNDER_PROVIDER=ollama`); unset or offline configuration declines to the next rung. Only the site slot and region hint leave the machine, and the call is time-bounded (15s).
- Post-landing consent-gated shortcut card: after a grounded navigation succeeds, the Action Thread offers to save `site → URL`. Saving persists a SQLite site shortcut that later runs resolve with zero model calls. Nothing is written without the explicit save; declining writes nothing.
- Backend metrics for run status, task replay share, and session outcomes.
- macOS CI in [.github/workflows/check.yml](../.github/workflows/check.yml).

## Important implementation distinctions

- **Task form:** replays existing macros by name; it sends empty planning selectors. Creating a new selector recording requires a backend API/fixture caller.
- **Command bar:** resolves a saved playbook, single intent, or plural batch. Command decomposition and dynamic-variable helpers exist but are not called by desktop dispatch.
- **Models:** saved matching and default parsing are deterministic. Optional intent-provider parsing and task selector-repair providers can invoke models. The production route proposer now wires the fenced domain grounder (`LlmDomainGrounder::from_env()`, Groq or Ollama) with a declining stub fallback when unconfigured or offline; the account directory and the Tier-2 LLM intent adapter remain unwired.
- **Intent provider limit:** unlike selector repair, this subprocess path has no enforced timeout and captures output before checking its size.
- **Browser mode:** task macro replay runs true headless (`LaunchOptions::replay()`); interactive lanes (manual login, Take Control, session sync) run visibly headed. Everything the desktop service launches for background work (semantic dispatch, direct opens, challenge escalation) is **off-screen headed**, never `--headless=new` — the contract is "never a visible window", not "never an OS window". A preview is not native browser embedding.
- **Approvals:** click/fill/submit legacy actions and semantic intents gate execution; typed navigate/download_links actions do not. Task decisions use sentinel_decisions, playbook decisions use session_events text.
- **Storage:** tasks have durable step checkpoints; playbook runs have summary journaling. Some telemetry/journal writes are best effort. This is not an immutable comprehensive audit log.
- **Saving:** playbooks persist, but completed-run save keys are held in memory with a 32-entry cap. TaskWorkspace uses its own legacy-step save path.
- **SSO:** known identity-provider hops have a dedicated challenge classification; they remain unconnected until the portal-origin check passes.
- **UI:** Cmd/Ctrl+K has only Connect a portal; the natural-language command bar is separate. Picker commands exist without a mounted picker component. No metrics dashboard is mounted.

## Verification for this documentation reconciliation

Executed on this Linux VM on 2026-09-24:

- `cargo test --workspace`: 303 passed / 0 failed (one transient hit of the known pre-existing SQLite flake `test_persist_ephemeral_run_to_playbook_and_replay`, green on retry; it fails ~20% of runs with `SqliteError code 14` and is unrelated to recent features).
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo fmt --all --check`: clean.
- `npx tsc --noEmit`: clean.
- `npm run build`: clean.
- `vitest`: 38 passed / 0 failed.
- Local Markdown links and fenced code blocks: checked across the product documentation files; all local links resolve.

The earlier verification pass (2026-09-22) was executed locally on Windows and is kept for its Windows-specific coverage; the 2026-09-24 pass above ran on Linux. Neither pass ran real Chromium fixtures against live portals, live model calls, personal-profile import, rendered UI testing, authenticated portal flows, cargo audit, installer builds, or macOS runtime checks.

Historical test counts, fixture timings, audit-warning totals, and earlier browser-tool failures have been removed rather than presented as current evidence.

## Reproduce checks

```powershell
npm run build
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build -p clinch-desktop --locked
python -m unittest discover -s scripts -p 'test_*.py'
```

CI runs on macos-latest and also installs/runs cargo-audit. It does not run the Python tests or ignored browser fixtures. CI configuration is not evidence that a particular remote run passed.

Opt-in fixtures require an installed Chromium and temporary test profiles:

```powershell
$env:CLINCH_CHROMIUM_PATH = 'C:\Program Files\Google\Chrome\Application\chrome.exe'
cargo test -p browser-driver real_cdp_cookie_injection -- --ignored
cargo test -p browser-driver --test dynamic_discovery -- --ignored
cargo test -p macro-engine --test healing -- --ignored
cargo test -p macro-engine --test dynamic_intent -- --ignored
cargo test -p orchestration-engine --test workflow_run -- --ignored
cargo test -p orchestration-engine --test semantic_runner -- --ignored
```

These synthetic tests are not substitutes for real portal/account validation.

## Remaining validation and product gaps

- Five-portal import/fallback/replay matrix, live OS-key access across supported browsers, and real-model selector-repair accuracy.
- Rendered approvals, preview scaling, takeover, session transitions, and frontend workflows against live portals.
- Real user time savings, voluntary reuse, correction rates, and account-health evidence.
- Intent-provider timeout/output handling and browser-context lifecycle concurrency need engineering review; this reconciliation changes documentation only.
- No general task resume/edit/abort UI, playbook delete/rename/import/export UI, or exposed signature rollback.
- No scheduler, ATS/job application product, resume/profile vault, price monitor, extracted expense sheet, team layer, remote approvals, or native-app automation.
- No signed/notarized distribution, enabled installer bundle, or bundled browser.

See [TRD.md](TRD.md) for contracts, [POC.md](POC.md) for acceptance goals, and [FUTURE_FEATURES.md](FUTURE_FEATURES.md) for proposals.
