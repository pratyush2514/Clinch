# Clinch implementation status

Reconciled on 2026-09-22 against the current working tree, including uncommitted implementation changes. Code is the source of truth. This file summarizes that code; it does not supersede it.

## Implemented

- Tauri v2/React desktop shell with resizable panes, session controls, command bar, workflow builder, task workspace, and approvals.
- Installed Chromium managed over CDP, with an app-owned profile, headed/headless modes, cookie-preserving restarts, JPEG mirror, and acquired screencast.
- Consented Chrome/Brave/Edge profile import with macOS Keychain/Windows DPAPI paths, App-Bound fallback, temporary cookie/WAL copies, CBC/GCM decryption, scoped SSO cookies, best-effort localStorage hydration, and user-agent mirroring.
- MV3 companion extension and loopback session bridge; manual login and in-app reauthentication status.
- Task state/checkpoint persistence, script-planned backend first runs, versioned macro recording/replay, bounded task selector repair, and interrupted-task recovery.
- Semantic AX/Set-of-Marks execution, context-sensitive matching, ordinals, and plural command batches capped at 30.
- SQLite playbook save/list/run, descriptions, completed command-bar run saving, run-summary journaling, and session/grounding diagnostics.
- Backend metrics for run status, task replay share, and session outcomes.
- macOS CI in [.github/workflows/check.yml](../.github/workflows/check.yml).

## Important implementation distinctions

- **Task form:** replays existing macros by name; it sends empty planning selectors. Creating a new selector recording requires a backend API/fixture caller.
- **Command bar:** resolves a saved playbook, single intent, or plural batch. Command decomposition and dynamic-variable helpers exist but are not called by desktop dispatch.
- **Models:** saved matching and default parsing are deterministic. Optional intent-provider parsing and task selector-repair providers can invoke models. The production route proposer has neither an account directory nor an LLM adapter wired.
- **Intent provider limit:** unlike selector repair, this subprocess path has no enforced timeout and captures output before checking its size.
- **Browser mode:** task macro replay is headless; current semantic/playbook execution and session setup use headed mode. A preview is not native browser embedding.
- **Approvals:** click/fill/submit legacy actions and semantic intents gate execution; typed navigate/download_links actions do not. Task decisions use sentinel_decisions, playbook decisions use session_events text.
- **Storage:** tasks have durable step checkpoints; playbook runs have summary journaling. Some telemetry/journal writes are best effort. This is not an immutable comprehensive audit log.
- **Saving:** playbooks persist, but completed-run save keys are held in memory with a 32-entry cap. TaskWorkspace uses its own legacy-step save path.
- **SSO:** known identity-provider hops have a dedicated challenge classification; they remain unconnected until the portal-origin check passes.
- **UI:** Cmd/Ctrl+K has only Connect a portal; the natural-language command bar is separate. Picker commands exist without a mounted picker component. No metrics dashboard is mounted.

## Verification for this documentation reconciliation

Executed locally on Windows:

- `cargo test --workspace --locked`: passed; opt-in real-browser tests remained ignored.
- `npm run build`: passed TypeScript checking and the Vite production build.
- `python -m unittest discover -s scripts -p 'test_*.py'`: five tests passed; Ollama is mocked.
- `cargo fmt --all --check`: passed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: passed.
- Local Markdown links and fenced code blocks: checked across all 12 product documentation files; 62 local links resolve.
- `git diff --check`: passed.

Historical test counts, fixture timings, audit-warning totals, and earlier browser-tool failures have been removed rather than presented as current evidence.

This pass did not run real Chromium fixtures, live model calls, personal-profile import, rendered UI testing, authenticated portal flows, cargo audit, installer builds, or macOS runtime checks.

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
