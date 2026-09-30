# Clinch implementation status

Reconciled on 2026-09-30 against the tree at `0e7d569`. Code is the source of truth. This file summarizes that code; it does not supersede it.

> Product direction moved on 2026-09-27: [CONSTITUTION.md](CONSTITUTION.md)
> is the authority on where Clinch is going. This file stays an accurate
> record of what is built.

## Implemented

- Tauri v2/React desktop shell with resizable panes, session controls, command bar, workflow builder, task workspace, and approvals.
- Installed Chromium managed over CDP with an app-owned profile, launched lazily when a task needs it. The background session runs **off-screen headed** (a real compositor positioned off-monitor and OS-hidden, no `--headless` flag) so bot-mitigation probes see a headed browser while the user sees no window; macro replay runs true headless; interactive work (manual login, Take Control, session sync) runs visibly headed. The viewport is mirrored as JPEG into React; a preview is not native browser embedding. `navigator.webdriver` is masked and `AutomationControlled` disabled, but a CDP-driven Chromium remains distinguishable to Cloudflare by design — a human check is routed to the human.
- **Large preview overlay:** "Open Preview" on the screencast card raises a near-fullscreen native `<dialog>` dressed as a browser window — tab label from the settle-time document title, a URL pill from the settle-time page URL with a secure/non-secure icon, and a live/final badge. Running entries keep streaming frames (a large live view); settled entries show the frozen final frame. Escape and backdrop-click close it.
- **Bot-mitigation handoff ladder (L1 → L1.5 → L2).** A run that lands on a detected challenge (Cloudflare / Turnstile / reCAPTCHA human-verification gate, detected by title, challenge-platform URL, and visible text — checked before and outranking any auth probe) flows through:
  1. **L1 auto-escalation** — the attached session restarts as off-screen headed on the same app-owned profile, re-navigates to the challenged URL, and polls ~30s for clearance (a live page on the challenged host is required, so a dead target never reads as cleared). Outcomes journal as `challenge_auto_escalated: <host> · cleared | persistent | failed (<static label>)`. A cleared escalation is stood down gracefully (CDP close flushes clearance cookies to the profile on disk), so the next acquisition launches lazily with no phantom window; a persistent challenge keeps the headed session alive for L2.
  2. **L1.5 session lending** — a challenge card offers a consent-gated one-way sync of the site's cookies from the Clinch Companion extension in the user's daily browser into Clinch's app-owned profile. Interactive gates (e.g. the cross-origin reCAPTCHA checkbox iframe) skip the L1 wait and go straight to the card, since automation cannot clear them.
  3. **L2 Take Control** — the human fallback: a headed managed window already on the challenged page, so the user solves the check once and the profile keeps the clearance naturally.
- **Persistent session lending + auth sync.** A tap on the sync card is consent: exactly one lend attempt per run, desktop-initiated single-use bridge request, server-side domain filter on the settled URL, one-way only (daily browser → Clinch, never write-back). The card polls bridge state while mounted and keeps Sync disabled in an honest "waiting" state until a companion attaches; with two or more companions connected it offers a source-browser picker. Injection **preserves the server's exact cookie expiry** — Chromium persists lent cookies in the app profile's cookie database ("sync once, stay logged in"); cookies without an expiry stay true session cookies, and no lifetime is invented. Post-sync the page re-navigates and re-probes origin-aware: a challenge run checks the gate, a guest landing checks for an authenticated page. `synced (persisted)` marks a confirmed login; a stale or rejected session reads as logged out and the card simply reappears (self-healing). A "Forget this site" revocation deletes the site's cookies from Clinch's profile via CDP (the daily browser is never touched) and returns the card to the signed-out offer. Journals record host and cookie counts only — never cookie values.
- **Auth-state probe at settle time.** When no challenge is detected, `auth_state()` classifies the landed page from URL, title, and up to 4,000 characters of visible text, failing open to `Unknown`. Authenticated copy (e.g. `log out`/`sign out`) wins over guest markers, login paths (`/login`, `/signin`, `/signup`, `/register`) count as logged out, and generic guest text requires at least two distinct markers. A clean signed-out guest landing surfaces an **auth-sync card** with the same consent-gated session-lending flow; the consent copy is explicit that Clinch keeps an independent copy of the login in its own profile and that signing out in the daily browser does not sign Clinch out. Anything authenticated or unclassifiable keeps the previous silent-success behavior.
- Consented Chrome/Brave/Edge profile import with macOS Keychain/Windows DPAPI paths, App-Bound fallback, temporary cookie/WAL copies, CBC/GCM decryption, scoped SSO cookies, best-effort localStorage hydration, and user-agent mirroring. Imported secrets are not stored in `clinch.db`.
- **Companion bridge.** The MV3 companion extension talks to the desktop over a loopback WebSocket on `127.0.0.1:9223`. The socket lives in an offscreen document — the MV3 service worker suspends when idle and cannot hold a long-lived connection — recreated by a watchdog alarm; reconnect uses bounded exponential backoff and heartbeat PING/PONG frames carry correlated nonces. Each companion identifies itself in HELLO with browser brand and an installation ID minted at boot; the desktop tracks identity per connection (each accepted socket handled concurrently, oldest evicted past the connection cap) and exposes one entry per connection through `bridge_status`. The extension reads cookies domain-scoped (`chrome.cookies.getAll({domain})` per scope root, exact requested domain first, authoritative post-read scope filter kept) instead of the whole cookie jar. Its options/status page shows socket state, browser label, installation ID, last sync, and a real loopback ping test. Manual login and in-app reauthentication status remain. Bridge errors surface as user-facing reasons: no extension connected, extension timed out, or no cookies for the portal.
- Task state/checkpoint persistence, script-planned backend first runs, versioned macro recording/replay, bounded task selector repair, and interrupted-task recovery.
- Semantic AX/Set-of-Marks execution, context-sensitive matching, ordinals, and plural command batches capped at 30.
- SQLite playbook save/list/run, descriptions, completed command-bar run saving, run-summary journaling, and session/grounding diagnostics.
- Direct-open entry routing with no curated route table: explicit domain in the prompt → account directory (unwired) → LLM intent adapter (unwired) → direct-open grounding ladder **(saved site shortcut → structured site directory → fenced domain grounder)** → honest miss, with a grounded search fallback only for non-direct opens. The directory rung is composite: Brave Search API when `CLINCH_BRAVE_API_KEY` is set, keyless DuckDuckGo HTML as the zero-config fallback, ranked results parsed in memory — the browser never sees a search page. The domain grounder resolves a site name to a bare domain through Groq (`CLINCH_GROUNDER_PROVIDER=groq`, key from `GROQ_API_KEY`, zeroized on drop) or local Ollama (`CLINCH_GROUNDER_PROVIDER=ollama`); unset or offline configuration declines to the next rung. Only the site slot and region hint leave the machine, and the call is time-bounded (15s).
- Post-landing consent-gated shortcut card: after a grounded navigation succeeds, the Action Thread offers to save `site → URL`. Saving persists a SQLite site shortcut that later runs resolve with zero model calls. Nothing is written without the explicit save; declining writes nothing.
- Backend metrics for run status, task replay share, and session outcomes.
- **In-page account-home goal.** Identity artifact nouns ("my profile", "my account") map to a generic `GoalClass::AccountHome` — a closed noun table, never site names, URL templates, or selectors. A follow-up on the live portal takes an already-on-origin fast path (the routing ladder is skipped and journaled); a cold prompt lands the grounded site first. `dispatch_in_page_goal` journals `in_page_goal_class: account_home`, then memory, then the chrome worker, then the verifier. Identity memory (`identity_memory` table, origin + goal class upsert) recalls a page-verified profile URL: www/case-normalized, https-only, credential-rejecting, same-site validated against the live landing (`in_page_goal_memory: hit/miss/stale` journal lines). The bounded 3-click chrome worker probes auth first — a signed-out page short-circuits and clicks nothing — opens the header identity control with an untruncated AX snapshot (a revealed menu renders past the 300-element head cap), and the Rust verifier demands same-site, non-root, and the revealed username when one was read (`in_page_goal_memory: write` on success). Stable click identity (normalized role + name, surviving backend-node-id churn) excludes retried controls in both pursue lanes; click-effect diagnostics (raw AX node counts, actionable before→after, newly revealed controls) and the deterministic tried-log ride along to the optional model fallback, which never verifies by itself. A miss yields FAILED with a "Take control" button on the card (gated on the `account-home:` journal prefix), handing the still-on-portal window to the user via the existing take-control command. "Forget this site" clears the remembered identity row alongside the profile's cookies; the daily browser is never touched.
- **Origin www-folding.** `same_site_origin` treats `www.` as an alias in either direction (folded, not ignored), with scheme and port still strict and other subdomains (e.g. `mail.`) NOT folded. It governs the portal drift check, Navigate validation, `check_origin`, and `detect_auth_signal`; the identity memory and account-home verifier fold the same way. Previously, a grounded bare-domain route against a live www page blinded the worker, the auth probe, and the model fallback at once.
- **In-page verb worker and the log-out ladder.** The four in-page verbs (`profile`/account-home, `settings`, `notifications`, `logout`) share one `VerbSpec`-parameterized worker (`pursue_chrome_action_with_vision`, called by `pursue_verb_goal`); the verb's own verifier decides completion. For log out the attempt is a fixed chain, each hand-off journaled once as `logout_ui_bounded: ...`: avatar click -> menu-open verify -> one re-grounded retry of the same opener -> **text lane** -> **visual fallback** -> one short model pass -> deterministic cookie-clear backstop (`VerbKind::LogOut` only; the verifier still decides). The **text lane** (`text_lane_step`, deterministic, no model) runs after an opener click when the AX pick has no direct match, which is the case for role-less menu rows: read-only page JS reports visible text nodes (open shadow roots included), Rust matches the verb's own vocabulary by *equality* inside the opener-centred menu region, an occlusion guard demands the deepest element under the point be that text (a neighbouring row or the whole menu list is refused), then the trusted CDP click and the signed-out verifier. Journal lines: `text_lane: click_at (x, y) "<word>"`, or `text_lane: missed (no text match | ambiguous | occluded: hit "..." | click budget exhausted | verification failed)`. A miss without a click falls through to vision; a click that fails verification re-snapshots and retires vision for the run. The **visual fallback** (vision model, opt-in via `CLINCH_VISION_MODEL`) crops a 720px square around the opener, remaps the model's crop-space point through Rust validation, and persists the exact model-bound JPEG (quality 90) to the OS temp dir (`visual_fallback: crop_saved <path>`; save failures journal `crop_save_failed` and never fail the run). Successful runs journal their attempt trail (including `text_lane: click_at`) via `tried_lines`.
- **Daemon logging.** The per-command `[clinch-daemon] cmd=... id=...` stderr line is opt-in (`CLINCH_LOG_COMMANDS=1`); the desktop polls `bridge_status`, so the always-on line flooded the terminal.
- macOS CI in [.github/workflows/check.yml](../.github/workflows/check.yml).

## Important implementation distinctions

- **Task form:** replays existing macros by name; it sends empty planning selectors. Creating a new selector recording requires a backend API/fixture caller.
- **Command bar:** resolves a saved playbook, single intent, or plural batch. Command decomposition and dynamic-variable helpers exist but are not called by desktop dispatch.
- **Models:** saved matching and default parsing are deterministic. Optional intent-provider parsing and task selector-repair providers can invoke models. The production route proposer now wires the fenced domain grounder (`LlmDomainGrounder::from_env()`, Groq or Ollama) with a declining stub fallback when unconfigured or offline; the account directory and the Tier-2 LLM intent adapter remain unwired.
- **Intent provider limit:** unlike selector repair, this subprocess path has no enforced timeout and captures output before checking its size.
- **Browser mode:** everything the service launches for background work — task macro replay included — is **off-screen headed**, never `--headless=new` (the true-headless mode and `LaunchOptions::replay()` were removed 2026-09-27). Interactive lanes (manual login, Take Control, session sync) run visibly headed. The contract is "never a visible window", not "never an OS window". A preview is not native browser embedding.
- **Approvals:** click/fill/submit legacy actions and semantic intents gate execution; typed navigate/download_links actions do not. Task decisions use sentinel_decisions, playbook decisions use session_events text.
- **Storage:** tasks have durable step checkpoints; playbook runs have summary journaling. Some telemetry/journal writes are best effort. This is not an immutable comprehensive audit log.
- **Saving:** playbooks persist, but completed-run save keys are held in memory with a 32-entry cap. Macro runs save through `save_run_as_workflow`; playbook runs through `save_playbook` — two save verbs, convergence pending (see CONSTITUTION.md).
- **SSO:** known identity-provider hops have a dedicated challenge classification; they remain unconnected until the portal-origin check passes.
- **UI:** Cmd/Ctrl+K has only Connect a portal; the natural-language command bar is separate. Picker commands exist without a mounted picker component. No metrics dashboard is mounted.

## Verification for this documentation reconciliation

Executed on this Linux VM on 2026-09-25 (code-verified, not live-run by this pass):

- Code paths above were traced to source: `apps/desktop/src-tauri/src/service.rs` (`dispatch_in_page_goal`, `dispatch_account_home_goal`, `recall_account_home`, `navigate_remembered_identity`, `forget_site_session`), `packages/orchestration-engine/src/goal_class.rs`, `packages/macro-engine/src/executor.rs` (`pursue_account_home`, `pursue_identity_chrome`, `ClickedControl`, `same_site_host`, `verify_account_landing`), `packages/browser-driver/src/lib.rs` (`same_site_origin`), `packages/browser-driver/src/a11y.rs` (`ax_snapshot_untruncated`, `interactive_elements_all`), `packages/playbook-store/src/lib.rs` (`identity_memory` CRUD), `apps/desktop/src/components/ActionCard.tsx` + `apps/desktop/src/lib/errors.ts` (`isPursuitMiss` gate on the Take-control miss button).

On 2026-09-30 (native Windows) `cargo test -p macro-engine` passed in full (including 23 new tests in `tests/text_lane.rs`, with the 17 crop and 11 visual-fallback tests unchanged), `cargo check --workspace --tests --locked` and `cargo fmt --check` were clean, and clippy on macro-engine showed no new warnings; the two page-JS expressions were syntax-checked with `node --check`. The JS itself has only run in the live observation above.

The 2026-09-24 Linux verification pass (303 tests / clippy / fmt / tsc / build / 38 vitest, one transient hit of the known pre-existing SQLite flake `test_persist_ephemeral_run_to_playbook_and_replay`, green on retry) remains the most recent full-suite run executed by this reconciliation. The account-home implementation was separately reported (2026-09-25, Linux sandbox) as `cargo test --workspace` 403 passed / 0 failed (one flake hit, green on retry), clippy and fmt clean, plus `tsc --noEmit` and `vitest` 40/40 — reported by the implementation loop, not re-executed here. The earlier verification pass (2026-09-22) was executed locally on Windows and is kept for its Windows-specific coverage. No pass ran real Chromium fixtures against live portals, live model calls, personal-profile import, rendered UI testing, authenticated portal flows, cargo audit, installer builds, or macOS runtime checks.

## Live-proven on native Windows, 2026-09-25

- "open my profile on reddit" COMPLETED twice on build `10016872`: first run ~9s landing on the real profile (u/MangoTree-1233), second run ~4.9s — consistent with an identity-memory hit, but the write→hit is inferred from timing, not journal-confirmed.

## Live-observed on native Windows, 2026-09-30

- "log out from reddit" on build `0e7d569` (text lane): preview screenshots show the cursor on the avatar, the menu open, then the click landing on the **Log Out** row itself (previously it landed on Display Mode and the session ended only through the cookie-clear backstop). This is a preview observation. **Not yet journal-confirmed:** the run journal still needs to show `text_lane: click_at ... "log out"`, a `click_hit_test:` on the Log Out row, `in_page_goal_done: signed out via 'text pick: log out'`, and no `session cookies cleared` / `visual_fallback:` lines. The Linux/WSL2 daemon run is also still outstanding.

## Implemented, not yet live-proven

- Signed-out zero-click short-circuit, the Take-control button on an account-home miss, Forget-site clearing the remembered identity row, multi-run identity-memory recall, and the generic noun-pursuit regression path have tests but no live Windows runs yet.

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
