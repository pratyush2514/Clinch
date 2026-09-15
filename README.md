# Clinch — Phase 0, Week 1

Implements **Steps 1 and 2 only** from `Phase_0_Prompt.md`: workspace initialization and consent-gated session sync. No invoice automation, macro execution, ATS forms, scheduling, or LLM calls.

## Run

Prerequisites: Rust stable with the platform C/C++ build tools, Node 24, and an installed Chrome/Chromium executable. The product targets macOS; Windows builds support development and manual-login fallback, **not Windows cookie decryption**.

```sh
npm ci
npm run tauri -- dev
```

`CLINCH_CHROMIUM_PATH` optionally selects the browser executable. On macOS the default is `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`. The browser runs as a managed, headed child with an app-owned persistent profile, `--remote-debugging-port=0`, and loopback CDP. No sandbox-disabling switches are used. This spike opens a separate interactive Chromium window; it does not claim inline embedding in the left pane.

The desktop app initializes `clinch.db` in its platform app-data directory through one lazy `sqlx` pool, with WAL mode and foreign keys enabled. Its sibling `browser-profile` directory holds Chromium's own persistent profile. The database records outcome metadata only, never cookie values or Safe Storage keys. Chrome controls encryption of its own profile. No separate plaintext cookie dump is written.

## Workspace and dependency structure

```text
Cargo.toml
apps/desktop/                 React + TypeScript + Vite + Tailwind
  src/                        Resizable shell, consent/fallback UI, dummy gate
  src-tauri/                  Thin Tauri v2 commands and service wiring
packages/
  orchestration-engine/       Reserved boundary; no execution yet
  browser-driver/             Managed child + chromiumoxide CDP bridge
  macro-engine/               Reserved boundary
  session-sync/               Consent, profile reader, decryption, fallback
  filesystem-tool/            Reserved boundary
  credential-vault/           keyring-core + macOS Keychain adapter
  playbook-store/             SQLite WAL initialization; schemas deferred
  llm-provider/               Reserved boundary; no provider requests
```

Dependency direction:

```text
desktop -> session-sync -> credential-vault
desktop -> browser-driver -> session-sync (cookie contract)
desktop -> playbook-store -> sqlx/SQLite
browser-driver -> chromiumoxide -> native CDP WebSocket
```

The other four packages deliberately have no dependencies or placeholder APIs. UI uses `react-resizable-panels` and `cmdk`; the command menu currently exposes only scaffold actions.

## Session-sync logic

1. `profile.rs`: validates explicit consent, HTTPS portal URL without embedded credentials, and `Default` / `Profile N` names. Paths are derived under the selected browser's macOS Application Support directory, not accepted as arbitrary IPC paths.
2. `credential-vault`: reads the existing `Chrome Safe Storage` / `Chrome` or `Brave Safe Storage` / `Brave` entry through a local `keyring-core` store. Keychain calls run on a blocking worker; no entries are created or changed. Keys are kept in zeroizing buffers.
3. `reader.rs`: opens `Network/Cookies` or `Cookies` read-only with `sqlx`; a transaction includes committed WAL content. Selects only exact host cookies and applicable parent-domain cookies, filters expired cookies, and preserves cookie security flags, paths, expiry, and SameSite.
4. `crypto.rs`: supports macOS `v10` AES-128-CBC with PBKDF2-HMAC-SHA1 (1003 iterations), PKCS#7 validation, and schema-24 SHA-256 host binding. Schema 23 is also supported. Unknown schemas/encryption and partitioned cookies fail closed to manual login.
5. `service.rs`: maps absent cookies, unsupported platforms/formats, Keychain failures, decryption failures, and timeouts to explicit fallback reasons. Native Keychain prompts have a 120-second application wait limit; a timed-out OS prompt may still need dismissal because an already-running native call cannot be cancelled safely.
6. `browser-driver`: translates each prepared cookie to `Network.setCookie`; host-only cookies use URL with domain omitted, and session cookies omit expiry. Each response and timeout is checked. Partial injection is never reported as success; manual login remains available.
7. Desktop opens the portal in its own Chromium profile. **Imported cookies do not prove authenticated access.** The user verifies the portal and can sign in manually, including when a site requires fresh 2FA. Manual login reads neither source cookies nor Keychain.

The dummy Sentinel Gate previews a harmless backend-issued request and accepts a single explicit approve/reject response. It executes no external action. Phase A bill downloads are not approval-gated.

## Verification commands

```sh
npm run build
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build -p clinch-desktop --locked
cargo audit
```

An opt-in synthetic CDP integration test launches an installed browser with a temporary profile and does not access personal profiles or navigate to a real portal:

```sh
# Set CLINCH_CHROMIUM_PATH to your installed browser first.
cargo test -p browser-driver real_cdp_cookie_injection -- --ignored
```

Tests cover local SQLite fixtures, live WAL reads, scoped domains, expiration, unsupported formats, wrong keys, host-binding checks, property-based Unicode crypto round trips and malformed input, fallback mapping, cookie-to-CDP conversion, WAL initialization, and single-use dummy approval state. CI runs on macOS so the platform-specific adapter is compiled.

The Windows workspace run passed all 19 tests, including the opt-in real Chromium cookie injection test and Tauri mock-runtime command dispatch for approval and consent. The Windows build script supplies Common Controls v6 to both app and test executables without duplicate manifests. These IPC tests do not establish rendered UI behavior.

The Windows-hosted check of `credential-vault` for `aarch64-apple-darwin` passed; this is compile evidence, not live Keychain validation. Browser preview QA was blocked by the browser tool's unavailable admin-policy check.

`cargo audit` completed with seven upstream warnings: unmaintained `proc-macro-error` and five `unic-*` packages, plus `RUSTSEC-2024-0429` in `glib` through Tauri's Linux GTK dependency tree. No advisory was suppressed. Linux is not a target of this spike; review these upstream dependencies before expanding platform support or shipping.

## Validation still required on macOS

- Actual Keychain consent/denial and Chrome/Brave decryption on installed browser versions.
- Portal acceptance, manual 2FA login, and persistent authenticated sessions on five real portals.
- Signed/notarized distribution and a bundled Chromium strategy; no installer is produced in this step.
- Phase A H1–H4 metrics and H5's two-month time tracking; scaffolding tests do not establish those gates.

Implementation references: [Chromium macOS v10 encryption](https://raw.githubusercontent.com/chromium/chromium/130.0.6723.58/components/os_crypt/sync/os_crypt_mac.mm), [Chromium cookie schema and host binding](https://raw.githubusercontent.com/chromium/chromium/main/net/extras/sqlite/sqlite_persistent_cookie_store.cc), and [keyring-core Entry API](https://docs.rs/keyring-core/1.0.0/keyring_core/struct.Entry.html). Unsupported future formats use fallback rather than assumptions about compatibility.
