# Development setup and repository map

This file describes the existing project, replacing the obsolete initialization checklist.

## Stack and startup

- Rust 2024 workspace, Tauri v2, Tokio, SQLx/SQLite, chromiumoxide.
- React 19, TypeScript, Vite 8, Tailwind 4, react-resizable-panels, and cmdk.
- Node.js 24 is used by CI. Rust stable and native platform build tools are required.
- Install dependencies with `npm ci`; start the desktop with `npm run tauri -- dev`.
- `npm run dev` runs only Vite; `npm run check` type-checks; `npm run build` type-checks and builds the frontend.
- Set `CLINCH_CHROMIUM_PATH` when the default installed Chromium path is unsuitable.
- macOS and Windows cookie-import paths exist. No browser binary is bundled; Tauri bundling is disabled.

## Repository map

```text
apps/desktop/src/            React shell, session UI, task workspace, workflow builder, command bar
apps/desktop/src-tauri/      Tauri commands, AppService, auth state, loopback bridge
packages/browser-driver/    Chromium lifecycle, CDP actions, AX/SoM, picker, previews
packages/orchestration-engine/ task checkpoints, gates, runner, intent and route resolution
packages/macro-engine/      macro files, selector repair, semantic grounding and batches
packages/playbook-store/    shared SQLite schema and playbook/run persistence
packages/session-sync/      validated extraction, profile paths, crypto, localStorage, UA
packages/credential-vault/  existing OS-protected browser-key access
packages/filesystem-tool/   download extension finalization
packages/llm-provider/      selector-repair adapter process
packages/extension-bridge/  MV3 JavaScript extension, excluded from Cargo
scripts/                   Python Ollama repair adapter and its unit tests
.github/workflows/check.yml macOS build, Rust checks, and dependency audit
docs/                      current contracts, status, validation, and future proposals
```

There is no `scripts/golden-path-tests` directory. Browser integration fixtures live under the Rust packages' test directories and in gated unit-test modules.

## Documentation map

- [README](../README.md): running the app and current user flows.
- [STATUS](STATUS.md): implemented scope, verification commands, and remaining gaps.
- [ARCHITECTURE](ARCHITECTURE.md): package responsibilities, execution paths, state boundaries.
- [TRD](TRD.md): concrete contracts, storage, limits, and provider interfaces.
- [FEATURES](FEATURES.md) and [PRD](PRD.md): current capabilities and product scope.
- [POC](POC.md): real-world acceptance plan, not completed pilot evidence.
- [FUTURE_FEATURES](FUTURE_FEATURES.md): unimplemented proposals.
- [Task execution](PHASE_0_STEPS_3_4.md), [repair and approvals](PHASE_A_STEP_5.md), and [integration setup](PHASE_A_FINAL_INTEGRATION.md): retained filenames with updated technical content.

The earlier phase/week schedule is obsolete. Existing code determines what is implemented; no phase label establishes completion of real-portal or user-pilot validation.
