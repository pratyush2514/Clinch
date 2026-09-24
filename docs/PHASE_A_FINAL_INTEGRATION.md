# Local integration and provider setup

The filename is retained for existing links. This guide follows current code and UI; it does not certify completion of a phase or pilot.

## Optional selector-repair adapter

Install Python 3 and Ollama separately, start the local Ollama service, and choose an already installed model. Clinch does not download or choose a model.

From the repository root in the PowerShell session that will launch Tauri:

```powershell
$env:CLINCH_REPAIR_PROVIDER = (Get-Command python).Source
$env:CLINCH_REPAIR_PROVIDER_SCRIPT = (Resolve-Path scripts/local_repair_adapter.py).Path
$env:CLINCH_OLLAMA_MODEL = '<installed-local-model-name>'
$env:CLINCH_CHROMIUM_PATH = 'C:\Program Files\Google\Chrome\Application\chrome.exe'
npm ci
npm run tauri -- dev
```

On Unix, an executable adapter script may itself be CLINCH_REPAIR_PROVIDER; leave CLINCH_REPAIR_PROVIDER_SCRIPT unset then. The executable and optional script path are passed without a shell.

The supplied adapter sends stripped selector context to `http://127.0.0.1:11434/api/generate`, disables proxies/redirects, requests non-streaming JSON, and uses a 25-second network timeout inside the Rust provider's 30-second bound. It emits only `{"selector":"…"}`. Rust validates the returned selector and region before use. A cold model may exceed the deadline.

## Optional intent provider is a different contract

The command resolver supports CLINCH_INTENT_PROVIDER and optional CLINCH_INTENT_PROVIDER_SCRIPT. A custom executable receives `{"prompt":"…"}` and returns `{"label_query":"…","container_query":null}` (or a nonempty container string).

The bundled repair script does not implement this protocol. Without an intent provider, parsing uses the deterministic fallback. A configured provider currently runs synchronously without an enforced timeout; its output size is checked only after capture. See [TRD.md](TRD.md).

## Current manual walkthrough

1. Launch the Tauri app, enter an HTTPS portal URL, and sync with explicit consent or choose Sign in manually.
2. Finish sign-in/2FA in the managed Chromium window. If the auth panel is shown, use its Continue action after returning to the portal.
3. Use the command bar for new actions, or add role/label steps in the workflow builder and preview a match before saving. The current Run Task form accepts only a saved macro's workflow name; it cannot record a new selector plan.
4. Inspect and approve the concrete execution requests. Reject must prevent that action. Navigation and typed download-links do not open task action gates; semantic clicks are gated.
5. Save a completed command-bar run and replay it from the saved list. A completed-run key is session-scoped; durable playbooks survive restart.
6. For existing task macros, inspect downloaded files with Open File/Show in Folder. Test selector repair only against a controlled fixture or known test portal.

To load the optional companion, use the source browser's extension developer mode and load `packages/extension-bridge` as an unpacked extension. Keep Clinch running for its loopback listener, then choose Sync via extension. This does not use the local-profile decryption path. The extension's options page is a read-only diagnostics surface — socket state, browser label, installation ID, last sync, and a loopback ping test — for checking a silent bridge without touching any cookies.

## Download and replay boundaries

Extensionless files are inspected with infer using at most 512 bytes and finalized on CDP completion. Known formats gain an extension; existing extensions remain. Unknown formats, including CSV without a signature, may keep GUID-only names. Destination collisions fail without overwrite; older files are not migrated.

Task macro replay switches to headless mode. Session setup, manual interaction, and current semantic playbook execution use the browser service's launch modes: background work runs off-screen headed (no visible window, no `--headless` flag), interactive work runs visibly headed. Browser restarts preserve cookies in memory, not tab sessionStorage. The image preview does not forward user input.

## Checks

```powershell
python -m unittest discover -s scripts -p 'test_*.py'
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
npm run build
```

Python tests mock Ollama. Default Rust tests skip opt-in Chromium fixtures. Neither establishes live model accuracy, authenticated portal acceptance, or rendered UI behavior; see [STATUS.md](STATUS.md).
