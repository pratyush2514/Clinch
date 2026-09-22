# Task execution and macro replay

This retained phase filename now describes current behavior. Historical test counts and timings have been removed; use [STATUS.md](STATUS.md) for verification.

## Current execution contract

[The orchestration engine](../packages/orchestration-engine/src/lib.rs) owns Task → Plan → Step state, transactional checkpoint revisions, recovery, and task approval decisions. Browser I/O occurs outside checkpoint transactions. Unfinished tasks recover as interrupted rather than automatically replaying uncertain effects.

The command is `run_task` with `TaskRequest` and `Channel<TaskEvent>`; `get_task` loads durable state. A connected portal of the same origin is required. The current task form supplies no selectors and therefore only replays an existing workflow. The backend planner can still record a first run when valid link/download selectors are supplied.

The first-run plan is script-based: navigate, optionally follow a same-origin link, and download matching links. Successful completion publishes `macros/<workflow>.json`. Existing macros are validated and reused; replay requires headless mode. Click/fill/submit gates and optional bounded selector repair apply through HealingReplay.

## Files and failures

Task downloads are scoped to `downloads/<task-id>/`, correlated to CDP completion, and checked for nonempty content. Extensionless files may gain a detected extension; server-provided filenames are not trusted as output paths. Supported same-origin blob downloads are included.

Missing, ambiguous, invalid, or invisible selectors trigger target/wait repair handling. A wait failure means the preceding action already ran; repair does not repeat it. Failed or interrupted runs may leave partial files. No automatic resume command exists.

The database stores current task snapshots and checkpoint history. The UI displays progress, local files, approvals, and repair/failure state; it does not provide an editable or scrubbable replay timeline.

## Focused browser fixture

```powershell
$env:CLINCH_CHROMIUM_PATH = 'C:\Program Files\Google\Chrome\Application\chrome.exe'
cargo test -p orchestration-engine --test workflow_run -- --ignored --nocapture
```

This test uses a temporary browser profile and synthetic local file portal. It covers recording/replay, checkpoints, downloads, and repair behavior. Its timing assertion is fixture-specific, not a real-portal latency promise. It does not validate personal-profile import, 2FA, or a five-portal pilot.

See [repair and approvals](PHASE_A_STEP_5.md) and [integration setup](PHASE_A_FINAL_INTEGRATION.md).
