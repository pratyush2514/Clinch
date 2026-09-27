# Clinch — Local Action Studio

Clinch is a Tauri v2 desktop app for local browser workflows. It connects to a managed Chromium process over CDP, imports browser sessions with consent, runs recorded macros and semantic playbooks, and displays progress and approval requests.

> Product direction (2026-09-27): [docs/CONSTITUTION.md](docs/CONSTITUTION.md)
> is now the product authority — Clinch turns a short intent into a finished
> action on a real website or a saved playbook, one engine with three faces
> (web flagship, phone wrapper, desktop pro/privacy). This README still
> describes the current working-tree code accurately.

The current working-tree code is the source of truth. [Build status](docs/STATUS.md) records the implemented scope and validation limits; [architecture](docs/ARCHITECTURE.md) and [technical contracts](docs/TRD.md) explain the execution paths. [Cloud transition](docs/cloud-transition.md) explains the product, the goals, and the local-to-cloud switch for new contributors (human or agent). Future ideas are isolated in [FUTURE_FEATURES.md](docs/FUTURE_FEATURES.md).

## Run locally

Use Rust stable with platform build tools, Node.js 24 (the CI version), and an installed Chrome/Chromium executable. Native cookie import has macOS and Windows implementations; platform support is not a real-portal compatibility guarantee.

```sh
npm ci
npm run tauri -- dev
```

Set `CLINCH_CHROMIUM_PATH` to select the managed browser executable. The source browser selected for cookie import is separate from this executable. Windows defaults the source selection to Brave; other platforms default to Chrome.

`npm run dev` starts the frontend alone. Native session and execution commands require Tauri. Installer bundling is currently disabled in `apps/desktop/src-tauri/tauri.conf.json`.

## Use the current UI

1. Enter the portal URL. For local-profile import, select Chrome, Brave, or Edge and a profile folder, then grant consent and choose **Sync session**. Alternatively choose **Sign in manually**, or use **Sync via extension** with the companion loaded.
2. Complete login/2FA in Clinch's managed Chromium window. The in-app sign-in panel reports status; it does not host the login page. Cookie import and URL-based session checks do not guarantee authenticated access.
3. Describe new work in the command bar. Review approval requests before execution. The natural-language path resolves a saved playbook, one semantic intent, or a plural batch; it is not a general multi-step autonomous planner.
4. Save completed command-bar runs as playbooks and replay them from the workflow list. Names use ASCII letters, digits, underscores, and hyphens. Optional descriptions are limited to 280 UTF-8 bytes.
5. Use **Run Task** in the command palette for an existing macro's workflow name. That form sends no planning selectors, so a new name without a macro fails validation. The Rust task API still supports script-planned first runs with selectors.
6. View task files through **Open File** or **Show in Folder**. The command resolves a completed task's saved file inside its download directory.

Cmd/Ctrl+K currently opens a single **Connect a portal** item; natural-language input is in the separate command bar. The element-picker backend remains available, but no picker component is mounted in the current UI.

## Browser and storage

Chromium is a separate process with an app-owned persistent `browser-profile`, not an embedded interactive webview. The UI has a polled JPEG mirror and an optional CDP screencast. **Spin up browser** acquires a background context; **Take Control** switches to a visible headed window on the current page. All background work — task macro replay, semantic playbook execution, direct opens, challenge escalation — runs **off-screen headed** (a real headed compositor positioned off-monitor and OS-hidden, no `--headless` flag): never a visible window. True headless mode was removed. A detected bot-mitigation challenge auto-escalates silently first, then offers a consent-gated session sync (Clinch keeps its own persistent copy of the login), and only then hands you Take Control; "Forget this site" revokes Clinch's copy at any time.

Application data contains `clinch.db` (SQLite WAL), `macros/<workflow>.json`, download directories, and the Chromium profile. Playbooks store their steps in SQLite; they do not require an attached macro file. Imported cookie values and Safe Storage keys are not written to the application database. Source cookie databases are temporarily copied for reading, and Chromium manages persistence of its own cookies and storage.

Planning and route lookup do not require a model. Optional `CLINCH_INTENT_PROVIDER` enables structured intent parsing; optional `CLINCH_REPAIR_PROVIDER` enables local selector repair in the task lane. See [provider setup](docs/PHASE_A_FINAL_INTEGRATION.md) for the separate contracts and limits.

## Verification

```sh
npm run build
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build -p clinch-desktop --locked
python -m unittest discover -s scripts -p 'test_*.py'
```

[The macOS CI workflow](.github/workflows/check.yml) runs the frontend build, Rust format/lint/test/build checks, and `cargo audit`. It does not run the Python adapter tests or ignored Chromium tests. See [STATUS.md](docs/STATUS.md) for browser test commands and what the latest documentation reconciliation actually verified.

There are no shipped ATS/job-application, price-monitoring, scheduler, team-sharing, or native-app automation features.
