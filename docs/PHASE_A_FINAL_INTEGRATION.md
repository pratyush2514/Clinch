# Phase A final integration

## Local repair setup

Install Python 3 and Ollama, and explicitly choose an already installed local model
(`ollama list`). Start Ollama locally (`ollama serve` if it is not already running).
No model is downloaded or selected by Clinch. Configure the same PowerShell session
that launches Tauri:

```powershell
Set-Location C:\Users\Pratyush\Downloads\Clinch
$env:CLINCH_REPAIR_PROVIDER = (Get-Command python).Source
$env:CLINCH_REPAIR_PROVIDER_SCRIPT = (Resolve-Path scripts/local_repair_adapter.py).Path
$env:CLINCH_OLLAMA_MODEL = '<installed-local-model-name>'
$env:CLINCH_CHROMIUM_PATH = 'C:\Program Files\Google\Chrome\Application\chrome.exe'
npm ci
npm run tauri dev
```

`npm run tauri -- dev` is the equivalent explicit npm argument-forwarding form.
On Unix, an executable adapter script with a Python shebang can itself be
`CLINCH_REPAIR_PROVIDER`; leave `CLINCH_REPAIR_PROVIDER_SCRIPT` unset in that case.
The optional script variable adds exactly one argument to the trusted executable,
without a shell. Existing executable-only adapters keep working when it is unset.

The adapter reads `selector`, stripped `html`, and CSS-pixel `bounds` from stdin.
It uses Ollama's [generate API](https://docs.ollama.com/api/generate) at
`http://127.0.0.1:11434/api/generate`, requests non-streaming JSON, and emits only
`{"selector":"..."}` on stdout. It disables proxies and redirects, uses a 25-second
network timeout within the Rust provider's 30-second total deadline, and fails
closed on malformed or empty output. Warm the chosen model before testing repair
if cold loading exceeds that deadline. Only the already stripped structural snippet
is provider context; JPEG previews are local UI data. Rust remains responsible for
checking selector validity, target type, uniqueness where required, and region scope.

## Manual Invoice Harvester walkthrough

1. Enter the real HTTPS billing-page URL in the left pane. Choose **Sign in manually**
   and complete login/2FA in the separate managed Chromium window. On macOS, consented
   session import is also available; verify authentication in Chromium afterward.
2. Confirm the left pane mirrors Chromium. Resize the divider and browser window;
   the JPEG and target outline should scale together. Interact with the actual
   Chromium window to scroll or sign in; the preview does not forward input.
3. Choose a new workflow name. Supply an optional same-origin invoice-history link
   selector and the required invoice-download link selector. Prefer stable, anchored
   selectors. The current planner supports same-origin links, not arbitrary portal
   widgets, cross-origin downloads, or automatic login.
4. Click **Run Invoice Harvester**. A history-link click pauses at Sentinel Gate.
   Check the run, step, action, and selector. **Review Details** must leave execution
   blocked; **Reject** (or Escape) must stop the action; **Approve & Submit** permits
   only that action. Downloads themselves do not open the gate.
5. Confirm completed steps and nonempty local files using the paths shown in the UI.
   Run the same workflow again to exercise saved-macro replay. Existing workflows
   ignore replacement planning selectors. Human approval time and real network latency
   are included in elapsed time; the historical 432 ms fixture is not a portal SLA.
6. To exercise repair, use a controlled portal whose target class has changed under
   a surviving unique ancestor. The model must produce a valid scoped selector or the
   task stops at `needs_repair`. Review saved healing history; never alter a production
   portal to force this test. Interrupted runs are not automatically resumed.

The invoice planner currently creates navigate/click/download actions. A saved,
validated macro containing `{"type":"submit","selector":"#form-id"}` exercises the
same channel's submit gate, but only test this against a controlled form with an
understood effect. The built-in healing fixture already covers approved/rejected
submission without using a real account. The React gate displays the typed action
details and keeps subsequent approvals intact if an earlier IPC response arrives late.

## Verification commands

```powershell
python -m unittest discover -s scripts -p 'test_*.py'
cargo test --workspace
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
npm run build
```

Adapter tests mock Ollama; they establish request/response handling, not model
accuracy. Live model and authenticated portal acceptance require the manual flow above.

Final integration checks on this Windows host: 27 workspace tests passed with three
opt-in browser tests ignored; all five Python adapter tests, frontend production
build, Clippy with warnings denied, formatting, and diff whitespace checks passed.
Rendered browser QA was blocked by unavailable admin-policy verification in the
browser tool. The live Ollama model and authenticated portal flow were not run.

## Download and replay behavior

Extensionless downloads are inspected with `infer` using at most their first 512 bytes.
Detected MIME types supply their standard extension (for example PDF, PNG, ZIP, or XLSX),
and files are published as `<cdp-guid>.<extension>`
immediately on each CDP write-completion event, before the next download starts or paths are checkpointed.
The filesystem post-processor uses this completion signal rather than polling incomplete files.
Same-origin `blob:` links and same-origin links that generate blob downloads are supported;
cross-origin and opaque blobs remain rejected. Unknown formats keep their original names; existing
extensions are preserved and destination collisions fail without overwriting files.
CSV has no magic signature and stays unchanged. XLSX detection depends on identifying
ZIP entries fitting in the 512-byte window; otherwise the detector may report ZIP.
Completed task cards offer **Open File** and **Show in Folder** for each saved file.
The native commands resolve the file from the saved task and restrict it to that run's
download directory. Previously downloaded bare GUID files are not migrated.

Saved workflows automatically use `LaunchOptions { headless: true }`; the engine also rejects
replay with a headed browser before creating a task or executing an action. The desktop
restarts its managed profile when changing modes, preserving cookies in memory across
the restart. Manual sign-in/session connection switches back to a visible browser.
Expired sessions still require manual sign-in; tab sessionStorage is not transferred.
Sentinel approvals remain required for interactive macro actions in headless mode.
