# Clinch technical contracts

Reconciled against working-tree code on 2026-09-25. Implementation references below take precedence over this summary.

## Desktop and browser

[Command registration](../apps/desktop/src-tauri/src/lib.rs) defines the IPC surface. [AppService](../apps/desktop/src-tauri/src/service.rs) wires commands to browser, storage, task, and playbook operations. Task/playbook progress uses typed Tauri Channels; screencast frames use events.

[ManagedBrowser](../packages/browser-driver/src/lib.rs) launches an installed Chromium with an app-owned profile and loopback CDP. `LaunchOptions::interactive()` is visibly headed (manual login, Take Control, user-named session sync); `LaunchOptions::replay()` is true headless for task macro replay, which refuses a headed browser. Everything the desktop service runs in the background — playbook `run_steps`, semantic dispatch, direct opens, challenge escalation — uses off-screen headed (`LaunchOptions::offscreen_headed()`): a real headed compositor positioned off-monitor and OS-hidden, never a visible window. Mode changes restart the browser and transfer cookies in memory; tab sessionStorage is not transferred.

The UI uses Tauri's native webview for React and JPEG images for the browser mirror. Neither native Chromium embedding nor input forwarding through the image is implemented. Bundling is disabled in the Tauri configuration.

## Sessions and local data

[SyncRequest validation](../packages/session-sync/src/profile.rs) requires consent, an HTTPS portal without credentials, and a supported profile name. The task command additionally disallows URL queries/fragments. Source browsers are Chrome, Brave, and Edge, with OS-specific profile paths and Default → Profile 1 fallback.

[The reader](../packages/session-sync/src/reader.rs) stages the source Cookies database and best-effort WAL sibling in temporary storage before reading. Selection includes the host, related domain/subdomain rules, and curated SSO secondaries. It filters expired cookies and rejects unsupported schema/encryption/partitioning. Temporary files are removed on normal cleanup; this is not an encrypted application vault.

[Crypto](../packages/session-sync/src/crypto.rs) supports legacy AES-128-CBC and AES-256-GCM paths, including schema host binding. [Credential access](../packages/credential-vault/src/lib.rs) reads macOS Safe Storage keys or Windows DPAPI-protected Local State keys. App-Bound key detection can produce an explicit fallback. Keys and decrypted values use zeroizing buffers.

The service bounds key access to 120 seconds and profile reads to 15 seconds. A running native permission call may outlive the application's wait. Extraction and injection failures map to fallback reasons; browser/storage failures can still return errors.

Best-effort localStorage extraction seeds missing keys, and source user-agent lookup can mirror browser identity. Imported secrets are not stored in `clinch.db`; Chromium owns persistence within its profile. Prompts and grounding diagnostics may appear in local session logs or saved intents, so the database is not restricted to counts-only metadata.

[Auth classification](../packages/browser-driver/src/session.rs) checks URL origin and login path segments, distinguishing known SSO from unknown foreign origins; known SSO is still pending auth until returning to the portal. The URL classifier is a heuristic, not proof of account access. Separately, the settle-time [auth-state probe](../packages/browser-driver/src/session.rs) (`auth_state()`) DOES inspect the landed page for authenticated state: it reads the URL, title, and up to 4,000 characters of visible text, fails open to `Unknown` on any probe failure, always runs after (and is outranked by) challenge detection, requires at least two distinct guest markers for a logged-out verdict, and lets authenticated copy (e.g. `log out`/`sign out`) win over guest markers. Cookie presence alone never proves authentication.

## Session-lending and revocation contracts

Session lending is the consent-gated, one-way (daily browser → Clinch managed profile, never write-back) transfer of a site's cookies from the Clinch Companion extension. Contract terms, all enforced in `AppService`/`BridgeServer`:

- **Consent and scope:** a sync-card tap is consent for exactly one lend attempt per run; repeat taps replay the stored `LendOutcome`. Desktop-initiated SYNC_SESSION requests carry single-use identities, expiration, and a server-side domain filter matched against the run's settled URL. Cookie values are passed to CDP, never to model adapters or application journal rows.
- **Persistence:** injection preserves the server's exact cookie expiry (persistent-by-default); Chromium owns persistence in the app profile's cookie database. Cookies without an expiry remain true session cookies. No artificial lifetime is invented.
- **Origin-aware verification:** `SessionLendOrigin::{Challenge, GuestLanding}` tags the lend. A challenge lend re-probes the gate after re-navigation (cleared → challenge done, else `persistent`); a guest-landing lend re-probes auth state (`synced (persisted)` on `AuthState::Authenticated`, else `not synced`). A later run whose probe sees a logged-out landing brings the sync offer back — self-healing.
- **Identity:** the managed browser mirrors the source user-agent before injection; a mismatched UA aborts the lend.
- **Failure semantics:** every exit journals exactly one `session_lent:`/`session_lend_failed:` line with host and cookie counts only. Zero cookies from the companion keeps the card with a "no usable cookies" reason (it does not prove the daily browser is logged out). Failure reasons map to static user-facing labels: extension not connected, extension timed out, no cookies for the portal, injection failure, re-navigation failure.
- **Revocation:** `forget_site_session` deletes the host's cookies from Clinch's app-owned profile through CDP, journaling host and cookie count, and returns the card to the signed-out offer. It also deletes every remembered identity row for that origin (`identity_forgotten: <host> · <n> remembered profile(s) cleared`), matching www and bare forms, so a signed-out origin is never navigated from a stale profile. It never touches the daily browser.

The manual `bridge_sync` path (user-named portal sync through the interactive lane) keeps its own contract: request first (a missing companion fails fast without opening a window), visible headed browser because the landing may need the user to finish a login or challenge, UA mirroring before injection.

## Extension bridge

The [MV3 manifest](../packages/extension-bridge/manifest.json) requests cookies, storage, activeTab, scripting, alarms, offscreen, and all-URL host permissions. The desktop [WebSocket server](../apps/desktop/src-tauri/src/ws_server.rs) binds loopback port 9223 at startup, with lazy startup as a backstop.

The bridge socket lives in an offscreen document, not the MV3 service worker: the worker suspends when idle and cannot hold a long-lived connection. Offscreen documents expose only `chrome.runtime` — `chrome.storage` is undefined there even with the `storage` permission — so the worker proxies every storage need of the offscreen document (`OFFSCREEN_SOCKET_STATE` diagnostics writes, `OFFSCREEN_GET_INSTALL_ID`) through message handlers; the offscreen document never touches storage directly. A watchdog alarm in the service worker recreates the offscreen document; reconnect uses bounded exponential backoff and heartbeat PING/PONG frames carry correlated nonces. Each companion identifies itself in HELLO with browser brand and an installation ID minted at extension boot. The server tracks identity per connection, serves every accepted socket in its own task, evicts the oldest connection past the connection cap, and exposes one entry per connection through `bridge_status` — one entry per live browser, never a stale reconnect.

Desktop-initiated SYNC_SESSION requests have single-use identities, expiration, and server-side domain filtering. `lend_session` accepts an optional `sourceConnectionId`: a valid selected connection receives the request exclusively; a stale ID falls back to broadcast. The extension reads cookies domain-scoped — `chrome.cookies.getAll({domain})` per scope root, exact requested domain first — and the existing authoritative post-read scope filter is kept. The extension returns scoped cookies and a user agent. Cookie contents are passed to CDP, not to model adapters or application journal rows. The extension options page is a read-only diagnostics surface — socket state, browser label, installation ID, last sync, and a loopback ping test — and never sends cookies anywhere.

## Task and macro contracts

[TaskRequest](../packages/orchestration-engine/src/lib.rs) has `workflow`, `portalUrl`, optional `linkSelector`, and `downloadSelector`. Names are 1–64 ASCII letters/digits/underscores/hyphens. The script planner builds navigate, optional link click, and download-links actions. Existing macros supply the plan on replay.

[Action](../packages/browser-driver/src/actions.rs) supports navigate, click, non-secret fill, submit, and download_links. These are constrained operations, not arbitrary script execution. Typed downloads accept supported same-origin links/blobs, cap a download-links step at 25 links, correlate completion by CDP GUID, and require a nonempty file.

Task states are planned, running, needs_repair, completed, failed, and interrupted. Step states are pending, running, completed, needs_repair, failed, and interrupted. Snapshot/checkpoint writes use revision checks. Recovery marks unfinished tasks interrupted; there is no general pause/edit/resume/abort API.

[Macro](../packages/macro-engine/src/lib.rs) files use version 1, reject unknown fields/actions/versions, and are bounded to 1 MiB. Optional lastHealedAt/healingHistory metadata records validated selector updates. Successful first-run recordings publish atomically after all steps complete.

## Repair and provider contracts

The task lane uses HealingReplay with LocalProvider. One repair attempt per failed target/wait stage has a 30-second bound. Context is stripped local structure plus selector and bounds; an invalid or unlocalized repair stops with needs_repair. A failed wait is repaired without repeating its action.

`CLINCH_REPAIR_PROVIDER` selects a trusted executable; `CLINCH_REPAIR_PROVIDER_SCRIPT` adds one argument without a shell. Input is `{"selector":"…","html":"…","bounds":[0,0,100,100]}`; output is `{"selector":"…"}`, with output/selector size validation. The supplied Python adapter uses loopback Ollama. It does not implement intent parsing.

Ephemeral command parsing separately supports `CLINCH_INTENT_PROVIDER` and `CLINCH_INTENT_PROVIDER_SCRIPT`. Input is `{"prompt":"…"}`; output contains `label_query` and optional nullable `container_query`. Unset configuration or returned invalid output uses deterministic parsing. The current implementation synchronously waits for process completion, has no enforced timeout, and checks its 4096-byte output limit after capture. Do not describe this path as bounded or guaranteed responsive.

No provider executable is sandboxed by these contracts. The domain grounder is a built-in provider path rather than a subprocess adapter: `CLINCH_GROUNDER_PROVIDER=groq` uses Groq's chat API with the key from `GROQ_API_KEY` (zeroized on drop), `=ollama` talks to the local daemon; only the site slot and region hint leave the machine, and the call is time-bounded. There is no per-run egress dashboard.

## Origin comparison contract

`same_site_origin` (`browser-driver/src/lib.rs`) is the single origin-equality rule for the portal drift check (`is_anchored_drift`), `Action::Navigate` validation, `ManagedBrowser::check_origin`, and `detect_auth_signal`. A leading `www.` folds in either direction — `https://www.reddit.com` and `https://reddit.com` are the same site — while scheme and port compare strictly and every other subdomain stays distinct (`mail.example.com` ≠ `example.com`). Case is folded. The identity-memory keys and the account-home host comparisons fold the same way (a host-level variant, `same_site_host`, for the verifier and recall validation), so a grounded bare-domain route against a live www page no longer blinds the worker, the auth probe, and the model fallback at once.

## Account-home identity contracts

The account-home lane (`packages/macro-engine/src/executor.rs`, `goal_class.rs`; dispatch in `service.rs`) is the generic way identity artifact nouns reach the signed-in user's page on the current origin. It contains no site names, no URL templates, no selectors, and no usernames.

- **Goal class:** `goal_class_for` is a closed mapping over artifact nouns: `profile` and `account` map to `GoalClass::AccountHome` (key `account_home`); every other noun keeps the generic noun-hunt path. `dispatch_in_page_goal` journals `in_page_goal_class: account_home` on the branch.
- **Identity memory:** `identity_memory(origin, goal_class, username, href, source)` is an observed fact — a profile URL the live page revealed — upserted automatically on verifier success and announced in the journal (`in_page_goal_memory: write <origin> <class_key>`). Keys are www-stripped and lowercased (`normalize_identity_origin`); goal class and source are normalized the same way. Recall journals `in_page_goal_memory: hit <href>` / `miss` / `stale → rediscover`, and a stale row is deleted. A recalled href is navigation-only data: it must be an absolute `https` URL with a non-root path, must carry no embedded credentials, must be same-site with the portal host, and — when a username was recorded — the username segment must survive both the recall and the live landing, otherwise the row is treated as stale and the live worker runs. Remembered facts are never sent to model adapters; they only steer navigation.
- **Pursuit:** `pursue_account_home` first probes auth — a `LoggedOut` page returns `SignedOut` with zero clicks (`in_page_goal_signed_out`), and the settle-time auth probe renders the normal lend card. The deterministic chrome worker is bounded to 3 clicks (`IDENTITY_MAX_CLICKS`): open the header identity control, quiet-wait for the revealed layer, take the page-revealed profile destination (Rust-validated href when the page offers one, else a click with an observed URL change). Revealed candidates must be genuinely new since the previous snapshot and require at least one opened control, so author links on a bare page never qualify. Retry exclusion uses `ClickedControl` — normalized role + trimmed lowercase name — in both the deterministic and model lanes; backend node ids churn on re-renders and are never the identity. Click effects journal as `<role> '<name>' → +N new […] (raw AX nodes N → M)` or `→ no new controls (actionable N → M; raw AX nodes N → M)`; the model lane keeps the truncated AX slice while the worker's re-snapshot uses `interactive_elements_all` / `ax_snapshot_untruncated`. The deterministic tried-log is appended to the model prompt when a navigator is armed.
- **Verifier:** `verify_account_landing` re-reads the live URL after navigation. With a page-revealed username it requires same-site, non-root, and the username segment in the path; without one it requires same-site, non-root, and either an account-naming path or a profile/account-worded trigger control. A click alone never verifies; an unverifiable landing fails the run, never the page.
- **Miss recovery:** the miss diagnostic names only what the worker actually tried (never a control dump). The FAILED card shows "Take control" gated on `isPursuitMiss` — the error code `workflow_failed` plus the `account-home:` journal prefix — calling the existing `take_control` command with no new IPC.

## Playbooks and semantic execution

[Playbook schema](../packages/playbook-store/src/schema.rs) version 1 contains name, origin, steps, and optional description. Limits are 100 steps, 64 ASCII name bytes, and 280 UTF-8 description bytes. Steps are legacy_selector (action and wait) or semantic (intent). The database stores the envelope in steps_json and upserts by name; legacy descriptions are migrated additively.

[SemanticIntent](../packages/macro-engine/src/executor.rs) includes role, labelQuery, containerQuery, rawPrompt, ordinalIndex, isLast, isPlural, entryUrl, and primaryTargetNoun. Grounding uses role admission, label/context scoring, document-order ordinal selection, and live node geometry. It is a pointer-action executor, not a general text-entry planner.

Saved playbooks and ephemeral commands use [the runner](../packages/orchestration-engine/src/runner.rs). Legacy selector failures return repair needs without invoking task HealingReplay. Semantic drift handling and signature-history helpers do not constitute a user-facing rollback feature.

The plural command lane resolves at most 30 candidates, approves the batch and each click, and halts on failures. Snapshot document order is the ordering contract. Entry routes in production come from the tiered resolver (explicit domain → account directory → LLM intent adapter → grounding ladder → search fallback). The account directory and Tier-2 LLM adapter remain unconfigured; the ladder's domain grounder is env-configured (Groq/Ollama) with a declining stub fallback, and the shortcut rung reads the user's own SQLite site shortcuts.

## Approval and persistence boundaries

Legacy click/fill/submit and semantic intent execution require explicit decisions. Typed navigate/download_links actions do not. Decisions identify the run/step, are single-use, and have five-minute deadlines. The generic low-level executor refuses submit; approved form submission uses a dedicated path.

Task approvals write sentinel_decisions before execution. Playbook decisions use session_events text. The preview_approval/resolve_approval demo is separate from execution gates. No configurable risk tiers or ATS-specific submission feature exists.

Database tables include session_events, playbooks, runs, signature_history, entry_urls, identity_memory, tasks, task_checkpoints, and sentinel_decisions. Tasks have durable step checkpoints; playbook runs have summary journaling, not equivalent restart/resume semantics. Some journal writes are best effort.

Completed command-bar save keys are held in memory (32-entry cap) and consumed by save_run_as_workflow. Playbook persistence is durable; the key registry is not. TaskWorkspace saves legacy steps through save_playbook instead.

See [STATUS.md](STATUS.md) for validation and [POC.md](POC.md) for outstanding acceptance work.
