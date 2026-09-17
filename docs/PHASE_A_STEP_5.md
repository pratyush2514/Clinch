# Phase A Step 5

Selector repair is integrated into task execution. Replay stays model-free when selectors resolve. A target or wait selector failure makes at most one local provider request per stage, bounded to 30 seconds. A repaired wait resumes only the wait; it never replays the preceding action. Rejected, ambiguous, out-of-region, or unresolved candidates leave the macro unchanged and produce `needs_repair`.

## Local provider setup

Set `CLINCH_REPAIR_PROVIDER` to a trusted local model-adapter executable before starting Clinch. The adapter reads one JSON document from stdin and writes exactly `{"selector":"..."}` to stdout, then exits successfully. Input fields are `selector`, `html`, and `bounds` (CSS-pixel x/y/width/height). Connect this adapter to your chosen local model; no model or API key is bundled or silently selected. No adapter means repair fails closed with `needs_repair`.

The HTML is a bounded structural clone of a unique local ancestor and its adjacent parent. Text, field values, URLs, script/style contents, and non-structural attributes are removed. Only id/class/type/role attributes remain. A legacy selector with no surviving unique local ancestor cannot safely be localized and remains `needs_repair`; the entire page is never substituted. Prefer anchored selectors such as `#billing .invoice` when recording.

Validated repairs use the existing same-directory temporary file, sync, and atomic replacement. Macro schema version remains 1 with backward-compatible `lastHealedAt` and `healingHistory` fields. First-run plans are still published only after every step completes.

## Sentinel Gate and viewport

React receives task/highlight/approval events through the existing Tauri Channel. Clicks, non-secret typing, and the new typed `submit` form action require a local decision tied to the exact task and step. Review Details keeps execution blocked. Approve & Submit grants one action; Reject, stale/duplicate decisions, and the five-minute deadline cannot authorize execution. Decisions are inserted into SQLite before executing an approved action. The old low-level executor rejects submit actions; the healing executor routes them through consent. Submission targets must be unique, same-origin forms.

The left pane polls local CDP JPEG viewport frames, with overlays scaled using CSS viewport dimensions. These images stay local and are never provider context. Manual interaction still uses the separate managed Chromium window; this is a mirrored viewport, not native window embedding or remote-input forwarding.

## Validation

Required checks: `cargo test --workspace`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo fmt --all --check`, and `npm run build`.

The opt-in `macro-engine` integration test runs an isolated local Chromium fixture:

```powershell
$env:CLINCH_CHROMIUM_PATH = 'C:\Program Files\Google\Chrome\Application\chrome.exe'
cargo test -p macro-engine --test healing -- --ignored --nocapture
```

It checks sanitized mock-provider context, invalid-candidate preservation, atomic publication, successful action execution, healing history, wait-only repair without repeated input, rejected consent, and actual viewport capture. No personal profile or real billing portal is used. The orchestration unit test verifies blocking, rejection, matching identity, single-use decisions, and the audit rows.

Browser-tool visual inspection is currently blocked by unavailable admin-policy verification. Rust/CDP integration and frontend compilation do not establish rendered modal/overlay correctness or real-model accuracy.

Final verification on this Windows host: 27 workspace tests passed (3 opt-in tests excluded from the default run); both the macro healing and invoice regression opt-in tests passed separately. Invoice replay measured 432 ms on the local fixture with simulated immediate consent. Clippy with `-D warnings`, formatting, frontend build, and `git diff --check` passed. The submission fixture also confirms the ungated executor rejects submit and that only approved execution triggers the form handler.
