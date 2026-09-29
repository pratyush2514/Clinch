# CLAUDE.md — Clinch orientation

Read this first. It exists so you don't burn tokens discovering what's below by exploring. The constitution is product authority; **code is the authority on what is built**. This file adds the *why*: the decisions, the dead ends already debugged, and what's next.

## What Clinch is

Clinch turns a short intent into a finished action on a real website — and never sends, spends, or leaves the site you named without a tap. Local, consent-first browser operator for repeatable website work. Not a cloud assistant, not a Muse clone.

v1 intents: `OPEN <site>` · `GO <profile|settings|notifications|logout> on <site>` · `DO <playbook>` · `WATCH <playbook>` · `ASK_ME` when stuck / to send / to spend.

Product positioning (the wedge, the Muse comparison table, what Clinch will never be): `docs/PRD.md`. Product authority: `docs/CONSTITUTION.md`.

## Repo map

| Path | What it is | Key files |
|---|---|---|
| `apps/desktop/` | Tauri desktop app (pro/privacy face). Rust backend + React TS frontend | `src-tauri/src/service.rs` (AppService, all commands), `src-tauri/src/daemon_client.rs` (remote mode), `src/` (React UI), `src-tauri/tests/` |
| `packages/orchestration-engine/` | Intent parsing, routing ladder, runners, verifier | `src/intent_resolver.rs`, `src/runner.rs`, `src/route_proposer.rs`, `src/domain_grounder.rs`, `src/url_policy.rs` |
| `packages/browser-driver/` | Managed Chromium over CDP (off-screen headed, never visible) | `src/session.rs`, `src/actions.rs`, `src/a11y.rs`, `src/som.rs`, `src/picker.rs`, `src/test_utils/fake_cdp.rs` (shared fake CDP server for tests) |
| `packages/macro-engine/` | Legacy macro lane + semantic worker (account-home/logout walks) | `src/executor.rs`, `src/lib.rs` |
| `packages/clinch-daemon/` | Engine as standalone Linux binary over localhost WebSocket | `src/lib.rs` (all logic), `src/main.rs` (thin wrapper) |
| `packages/clinch-protocol/` | JSON WebSocket contract between client and daemon | `PROTOCOL.md` (read before touching either side), `src/lib.rs` |
| `packages/session-sync/` | Consent-gated cookie lending from daily browser → managed profile | `src/service.rs`, `src/crypto.rs`, `src/reader.rs` |
| `packages/credential-vault/` | OS-keychain credential storage (pilot for tests-in-`tests/` layout) | `src/lib.rs`, `tests/crate_root.rs` |
| `packages/playbook-store/` | SQLite: playbooks, shortcuts, run memory | `src/lib.rs`, `src/schema.rs` |
| `packages/extension-bridge/` | Chrome MV3 Companion extension (session lending source) | `background.js`, `offscreen.js` |
| `packages/llm-provider/` | Narrow model boundary (sanitized local structure only) | `src/lib.rs` |
| `packages/filesystem-tool/` | Download finalization (never trusts server filenames) | `src/lib.rs` |

## Runtime shape

```
thin client (Tauri desktop / future web)
  ↕ clinch-protocol JSON over WebSocket (daemon: 127.0.0.1:18790, loopback only)
clinch-daemon (Rust worker, own SQLite)
  ↕ localhost CDP
off-screen-headed Chromium (app-owned profile; daily browser untouched)
```

Browser launches lazily, one Chromium process reused across plain runs. `CLINCH_DAEMON_URL` switches the desktop app to remote mode; embedded mode is default.

## Contracts (do not violate; do not "simplify")

- **Verifier decides.** Only the deterministic verifier can declare `COMPLETED`. A click is not success. The model never grades its own work.
- **Consent gates everything.** No auto-save of shortcuts/playbooks. Batch runs require per-click approval. Session lending is one tap, one-way (daily → managed, never write-back), domain-scoped, exact expiry preserved.
- **Challenge ladder:** silent retry → consent-gated session lend → Take Control. Never click a captcha.
- **Routing ladder:** explicit domain → LLM adapter seam → saved shortcut → composite directory → fenced LLM grounder → honest miss. Google Search must never visibly open as fallback.
- **No hardcoded web knowledge.** No per-site/per-button selectors, labels, endpoints, procedures, or site-name routing lists. Explicit `gmail → mail.google.com`-style mappings are fine; name-similarity routing is not.
- **Signed-out is not success.** Guest/logged-out pages are detected, never acted on as authenticated.
- **Nondeterminism is quarantined:** models propose/ground/choose; deterministic Rust validates, executes, applies safety, verifies.
- `publish = false` workspace-wide: widening `pub` for test access is safe and expected.

## Key decisions (dated, with rationale)

- **2026-09-27 — Constitution adopted** (`docs/CONSTITUTION.md`): product sentence, five v1 intents, one engine three faces (web flagship / phone wrapper / desktop pro-privacy), trust layer. Amendments baked in: credential vault is a hard prerequisite for hosted sign-in; personal-cloud bridge kept as middle phase; our streaming protocol stays (noVNC is later scale, not a rewrite); cursor overlay stays (truthful overlay of real input, not fake).
- **2026-09-27 — Not a Muse clone.** Positioning decided: local consent-first operator for people who want control; the wedge is users who won't hand over inbox+cards and sites that block cloud agents. Recurring website admin (bills, portals, stop-before-submit forms) is the job. Job-application engines, deal radars, price monitors are distractions.
- **2026-09-27 — Cloud sequencing:** prove the local/Linux operator → personal-cloud bridge (own daemon + web/phone client) → hosted web face. Hosted is gated on: T1 acceptance bar, credential vault, real cost-per-session data. The hosted decision is a business decision (cost, abuse, support), decoupled from the lab work.
- **2026-09-27 — Web app Phase 1 is the next build** (agreed direction): TS protocol client mirroring `daemon_client.rs` + web shell + Flows test panel, against the local daemon. Phase 2 = personal cloud (TLS + token auth on the socket, HTTPS download relay, session persistence). Phase 3 = hosted (gated, see above). No user accounts / control plane / worker pools in Phase 1.
- **2026-09-27 — Dev environment: Windows + WSL2.** Engine/daemon work happens in WSL2 (the proven lab, the deployment target); native Windows kept only for Tauri desktop GUI builds. Native dual-boot Ubuntu rejected: WSL2 already proved itself and the daily browser profiles needed for session-lend testing live on Windows. Repo must live on the WSL2 filesystem (`~/...`), not `/mnt/c` (cargo is painfully slow on the Windows mount).
- **2026-09-27 — Tests live in `tests/`, never `src/`.** Workspace-wide migration completed and pushed. Pattern: `use <crate>::...`, shared helpers in `tests/common/`.
- **2026-09-27 — Cleanup:** deleted `entity_resolver.rs` + account-entity routing tier, true-headless execution (`WindowMode::Headless`, `--headless=new`), `LaunchOptions::replay()`. Macro replay is off-screen headed now. Legacy macro lane (`run_task`, selector plans) deliberately retained — converging it with playbooks needs explicit discussion first.
- **2026-09-26 — Daemon/protocol split:** engine runs as `clinch-daemon`, Tauri app is a thin client in remote mode. Frontend event shapes untouched.
- **Login saves the session, not the procedure.** Save-as-playbook is not offered on login/session flows (replaying a login click sequence is fragile and wrong); session persistence belongs to session-sync. Currently the save card appears only on command-bar semantic runs and batch runs.
- **Cookie-clear is fallback only.** For logout: bounded UI attempts first (avatar → verify → one re-grounded retry → max 3 model steps), cookie deletion only as the last resort, gated to `VerbKind::LogOut`.

## Problems already solved (do not re-debug)

- **Windows flakiness (corrupted frames, bot-like cursor):** environmental, not logic. Root causes found: a stale build still running (pulling main is not enough — needs full rebuild *including the frontend compile step*), and daily-browser interference. Resolution: moved the lab to WSL2. The decisive signal is `click_hit_test:` in the run journal — absent means old binary, present means a real new issue.
- **Chrome 154 DevTools probe timeout:** Chrome's DevTools HTTP server ignores `Connection: close`, so `read_to_end` burned the 3s timeout on healthy Chrome. Fixed: read headers, honor `Content-Length`, read exactly the body.
- **"Headless" was the wrong diagnosis:** the gap was input, not rendering. Clicks now dispatch hover → press → release via trusted CDP `Input.dispatchMouseEvent`, with a visible cursor overlay (SVG pointer + press ripple, presentational only). Cursor glides via mousemove waypoints for isolated jumps.
- **Logout misclicks:** the Figma-ad misclick was targeting wander, not click failure. Fixed with: candidates hard-filtered to the header strip (center-y ≤ 25% viewport), one re-grounded retry by stable role+name identity when the menu doesn't open, `click_hit_test` journal line after every click (`document.elementFromPoint` + mismatch flag).
- **Poisoned frames:** frame validation drops truncated/poisoned frames, keeps the last good one.
- **Groq model decommissioned:** `llama-3.1-8b-instant` died 2026-08-16; default is now `openai/gpt-oss-20b` (`CLINCH_GROQ_MODEL` overrides). Never use Enterprise-only `llama-3.3-70b-versatile`.
- **MV3 offscreen limits:** offscreen documents expose only `chrome.runtime` — the worker proxies storage access via message handlers (contract in the TRD). An unclosed `/*` comment once evaded `node --check`: the pre-push ritual includes a block-comment balance grep for JS files.
- **Brave API key env var:** correct spelling is `CLINCH_BRAVE_API_KEY` (not `BRAVE_SEARCH_API_KEY`). Keyless DuckDuckGo (`html.duckduckgo.com/html/?q=`) is the fallback; its scraping is ToS-gray, Brave API is the sanctioned upgrade (free-tier signup still pending).

## Open threads / next up

- **Decisive operator proof:** `log out from reddit` on the Linux lab — clean frames, menu opens, verifier passes. Not yet reported.
- **Flows/test panel** (proposed, not yet approved for build): one panel listing login, logout, save-as-playbook, replay, session lend — with run controls and explicit pass/fail. The user finds the current command-bar test loop inadequate; better UI comes before broad manual testing.
- **Shared identity-menu primitive** (adopted direction, not implemented): profile/settings/notifications/logout should all open the same identity menu then pick the noun — converge the logout-specific machinery (retry, header filter) into it.
- **Save-as-playbook coverage:** decide which successful actions offer Save, and where the affordance lives in the ordinary UX (consumer card vs dev panel vs both).
- **Playbook management gap:** delete/rename/export don't exist; completed-run save keys are in-memory (32 cap, lost on restart).
- **Session-sync ladder:** (1) stream the real login page, user types into remote browser → (2) credential vault with model-paused fill → (3) E2E extension→worker lending → (4) phone wrapper for OTP/approvals.
- **v1.5 intents** (only after the five are boring): `DRAFT` (write, never send), `FILL` (fill a form, stop before submit).

## State as of 2026-09-27

- **Proven:** WSL2 daemon lab completed a real login.
- **Known issues:** `service::tests::test_persist_ephemeral_run_to_playbook_and_replay` flakes ~20% (SQLite code 14, passes on retry — do not chase). Desktop test binaries can't link in containers missing `gdk-3`/`gdk_pixbuf-2.0` (env issue, not code). Take Control needs a display on the daemon host. Daemon's SQLite is fresh (no playbook migration from old Windows runs).
- **Acceptance bar (T1):** a stranger can open settings on a site they named and not see a search page congratulating them.

## Dev commands

```sh
cargo check --workspace --tests   # fast correctness gate
cargo test -p <crate>             # per-crate; run affected crates, not everything
cargo clippy -p <crate> --tests   # zero new warnings
cargo fmt --all -- --check
```

Windows native: `.\scripts\dev.ps1` (kill stale Chrome/Brave remote-debugging processes first). WSL2: plain `cargo` in the repo; daemon runbook at `docs/wsl2-daemon.md`. Set `CLINCH_CHROMIUM_PATH` to the Chromium binary. Grounder: `GROQ_API_KEY` (+ optional `CLINCH_GROUNDER_PROVIDER=groq`); without keys the grounder declines to an honest miss — that is correct behavior.

## Conventions & working norms

- **Tests live in `tests/`, never in `src/`.** See `credential-vault/tests/crate_root.rs` for the pattern.
- **New Rust tests belong under `/tests`.** Never add `#[cfg(test)]` modules to production files.
- **Discuss before building** product-facing changes; the user says "first discuss with me" and means it. Be real and logical, no sugarcoat. Prefer dynamic over hardcoded, always.
- **Verify before claiming.** Never say you checked/built/ran something without the tool result behind it. When your diagnosis breaks on contact with the code, say so plainly.
- **Push only when the user says so.** A bundled "build and push" is standing authorization; otherwise ask. The reusable helper is `~/workspace/push_clinch.py` (dry-run default).
- Long work needs progress updates; unexplained silence reads as stuck.
- The user's name is unknown. The assistant's name is Pratyush — never address the user as Pratyush.
- Never store credentials, API keys, or tokens in chat, memory, or files. (A Groq key was exposed in chat in Sept 2026 — rotation pending; do not reproduce or request it.)

## Where to read deeper (in order)

`docs/CONSTITUTION.md` → `docs/PRD.md` → `docs/ARCHITECTURE.md` → `docs/TRD.md` → `docs/STATUS.md` → `docs/cloud-transition.md` → `docs/wsl2-daemon.md` → `docs/FUTURE_FEATURES.md` (explicitly not built) → `docs/POC.md` (live-portal validation matrix).
