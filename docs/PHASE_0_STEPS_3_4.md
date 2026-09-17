# Phase 0 Steps 3–4

## Execution contract

- `orchestration-engine` owns Task → Plan → Step state. A task starts planned, moves through running steps, and ends completed, failed, needs_repair, or interrupted. Completion is impossible with pending/failed steps. Each run has a SQLite-generated ID and monotonic checkpoint revision.
- The desktop service shares its lazy WAL pool with the engine and holds the existing single-operation semaphore throughout a run. Sync, manual navigation, closing the browser, and another run cannot race with execution. Mutexes only copy/update handles; no mutex guard spans browser I/O.
- Checkpoints are transactional: the current task snapshot and immutable history row commit together. Compare-and-swap revisions reject stale writers. Browser I/O runs outside SQL transactions. Recovery marks in-flight work interrupted instead of guessing whether an external action happened.
- `macro-engine` validates the complete version-1 file before replay, rejects unknown action types/fields/versions, bounds inputs and file size, and publishes completed recordings with an atomic replacement. Both recording and replay use the same native-CDP executor. Neither depends on `llm-provider`.
- First-run Invoice Harvester planning is script-based: navigate, optionally click the invoice-history link, then download matching invoices. Selectors and postconditions are recorded, rather than freezing invoice URLs from the first run. Each replay resolves the current links.
- DOM queries return only selector cardinality and bounding rectangles, never page text or input values. A missing, ambiguous, invalid, or invisible target produces a narrow repair flag. The wait-stage flag explicitly means the action already ran. Only read-only probes interrupted by navigation may retry within their original deadline; clicks/downloads are never retried automatically.
- Tauri `harvest_invoices` accepts a typed request and progress `Channel<TaskEvent>`. React receives durable state changes plus ephemeral highlight targets. `get_task` restores the last checkpoint after a view/app reload. Closing a view does not erase durable results.

## Local files

Under the existing application data directory:

```text
clinch.db                         shared SQLite WAL database
macros/<workflow>.json             versioned, validated completed recording
downloads/<task-id>/<cdp-guid>      browser-managed downloaded files
browser-profile/                  existing managed session
```

Workflow names are restricted to ASCII letters, digits, underscore, and hyphen. The desktop requires an existing connected session for the same portal origin. Session import consent and manual-login fallback remain unchanged. Importing cookies still does not establish successful portal authentication.

The macro writer never reads the cookie jar or credential vault. Macro inputs are non-secret filters; login remains manual. Download links must be same-origin; a completed download is correlated by CDP GUID and checked on disk. Each new run gets a separate directory. Files keep GUID names to avoid trusting server-supplied filesystem paths.

## Verification

Required commands:

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
npm run build
```

The opt-in integration test uses a real installed Chromium, a temporary profile, a synthetic session cookie, and a loopback billing server. It does not read a personal browser profile or contact an actual billing portal:

```powershell
$env:CLINCH_CHROMIUM_PATH = 'C:\Program Files\Google\Chrome\Application\chrome.exe'
cargo test -p orchestration-engine --test invoice_flow -- --ignored --nocapture
```

It asserts first-run recording, persisted step boundaries, actual nonempty PDF bytes, second-run replay with the macro unchanged, isolated output paths, event ordering, absence of the fixture cookie value from events/macros, and targeted target/wait repair failures without duplicate downloads. It measures replay wall-clock time and enforces a sub-second budget on this local fixture only. Unit tests cover illegal state transitions, crash recovery, stale writers, malformed/versioned macros, Unicode action round trips, input validation, and desktop session/concurrency guards. Tauri mock-runtime dispatch checks the new channel command's session requirement.

Final real-Chromium test on this Windows host passed with **840 ms** replay wall time (step times: 82, 284, 402 ms). It also verified native filter input, password-field rejection, and missing/ambiguous/invalid/invisible selector classifications. Replay succeeded with deliberately unusable new planning inputs, proving it loaded the recorded plan. The fixture handles idle Chromium connections concurrently and reads complete HTTP headers. An earlier timing run under concurrent compilation exceeded one second; host contention and network latency remain outside the deterministic action sequence's control.

## Remaining Phase A evidence

- The first-run provider is a script planner, not an LLM. Targeted repair is flagged, not automatically performed. Interrupted/needs-repair tasks cannot be resumed automatically.
- An installed Chromium integration test proves the native engine path on this Windows development host. It does not prove macOS Keychain behavior, 2FA, or authenticated operation on five real portals.
- Real portal/network/download latency can exceed one second. The local fixture timing is not a production latency guarantee.
- Browser preview inspection was blocked by the browser tool's unavailable admin-policy verification. Frontend compilation and IPC tests are not rendered UI evidence. The existing separate Chromium window remains; no embedded viewport is added here.
- No Phase B, scheduling, installer, or provider integration is included.
