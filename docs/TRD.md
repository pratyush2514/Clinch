# Clinch technical contracts

Reconciled against working-tree code on 2026-09-22. Implementation references below take precedence over this summary.

## Desktop and browser

[Command registration](../apps/desktop/src-tauri/src/lib.rs) defines the IPC surface. [AppService](../apps/desktop/src-tauri/src/service.rs) wires commands to browser, storage, task, and playbook operations. Task/playbook progress uses typed Tauri Channels; screencast frames use events.

[ManagedBrowser](../packages/browser-driver/src/lib.rs) launches an installed Chromium with an app-owned profile and loopback CDP. `LaunchOptions::interactive()` is headed and `replay()` is headless. Task replay refuses a headed browser. Playbook `run_steps` and semantic dispatch currently use headed mode. Mode changes restart the browser and transfer cookies in memory; tab sessionStorage is not transferred.

The UI uses Tauri's native webview for React and JPEG images for the browser mirror. Neither native Chromium embedding nor input forwarding through the image is implemented. Bundling is disabled in the Tauri configuration.

## Sessions and local data

[SyncRequest validation](../packages/session-sync/src/profile.rs) requires consent, an HTTPS portal without credentials, and a supported profile name. The task command additionally disallows URL queries/fragments. Source browsers are Chrome, Brave, and Edge, with OS-specific profile paths and Default → Profile 1 fallback.

[The reader](../packages/session-sync/src/reader.rs) stages the source Cookies database and best-effort WAL sibling in temporary storage before reading. Selection includes the host, related domain/subdomain rules, and curated SSO secondaries. It filters expired cookies and rejects unsupported schema/encryption/partitioning. Temporary files are removed on normal cleanup; this is not an encrypted application vault.

[Crypto](../packages/session-sync/src/crypto.rs) supports legacy AES-128-CBC and AES-256-GCM paths, including schema host binding. [Credential access](../packages/credential-vault/src/lib.rs) reads macOS Safe Storage keys or Windows DPAPI-protected Local State keys. App-Bound key detection can produce an explicit fallback. Keys and decrypted values use zeroizing buffers.

The service bounds key access to 120 seconds and profile reads to 15 seconds. A running native permission call may outlive the application's wait. Extraction and injection failures map to fallback reasons; browser/storage failures can still return errors.

Best-effort localStorage extraction seeds missing keys, and source user-agent lookup can mirror browser identity. Imported secrets are not stored in `clinch.db`; Chromium owns persistence within its profile. Prompts and grounding diagnostics may appear in local session logs or saved intents, so the database is not restricted to counts-only metadata.

[Auth classification](../packages/browser-driver/src/session.rs) checks URL origin and login path segments, distinguishing known SSO from unknown foreign origins. It does not inspect authenticated account state. Manual-login readiness requires the user to finish signing in. The bridge auth probe rejects explicit NoCookies replies but otherwise permits continuation, including when no extension is connected.

## Extension bridge

The [MV3 manifest](../packages/extension-bridge/manifest.json) requests cookies, storage, activeTab, scripting, alarms, and all-URL host permissions. The desktop [WebSocket server](../apps/desktop/src-tauri/src/ws_server.rs) binds loopback port 9223 at startup, with lazy startup as a backstop.

Desktop-initiated SYNC_SESSION requests have single-use identities, expiration, and server-side domain filtering. The extension returns scoped cookies and a user agent. Cookie contents are passed to CDP, not to model adapters or application journal rows.

## Task and macro contracts

[TaskRequest](../packages/orchestration-engine/src/lib.rs) has `workflow`, `portalUrl`, optional `linkSelector`, and `downloadSelector`. Names are 1–64 ASCII letters/digits/underscores/hyphens. The script planner builds navigate, optional link click, and download-links actions. Existing macros supply the plan on replay.

[Action](../packages/browser-driver/src/actions.rs) supports navigate, click, non-secret fill, submit, and download_links. These are constrained operations, not arbitrary script execution. Typed downloads accept supported same-origin links/blobs, cap a download-links step at 25 links, correlate completion by CDP GUID, and require a nonempty file.

Task states are planned, running, needs_repair, completed, failed, and interrupted. Step states are pending, running, completed, needs_repair, failed, and interrupted. Snapshot/checkpoint writes use revision checks. Recovery marks unfinished tasks interrupted; there is no general pause/edit/resume/abort API.

[Macro](../packages/macro-engine/src/lib.rs) files use version 1, reject unknown fields/actions/versions, and are bounded to 1 MiB. Optional lastHealedAt/healingHistory metadata records validated selector updates. Successful first-run recordings publish atomically after all steps complete.

## Repair and provider contracts

The task lane uses HealingReplay with LocalProvider. One repair attempt per failed target/wait stage has a 30-second bound. Context is stripped local structure plus selector and bounds; an invalid or unlocalized repair stops with needs_repair. A failed wait is repaired without repeating its action.

`CLINCH_REPAIR_PROVIDER` selects a trusted executable; `CLINCH_REPAIR_PROVIDER_SCRIPT` adds one argument without a shell. Input is `{"selector":"…","html":"…","bounds":[0,0,100,100]}`; output is `{"selector":"…"}`, with output/selector size validation. The supplied Python adapter uses loopback Ollama. It does not implement intent parsing.

Ephemeral command parsing separately supports `CLINCH_INTENT_PROVIDER` and `CLINCH_INTENT_PROVIDER_SCRIPT`. Input is `{"prompt":"…"}`; output contains `label_query` and optional nullable `container_query`. Unset configuration or returned invalid output uses deterministic parsing. The current implementation synchronously waits for process completion, has no enforced timeout, and checks its 4096-byte output limit after capture. Do not describe this path as bounded or guaranteed responsive.

No provider executable is sandboxed by these contracts. There is no built-in cloud provider or per-run egress dashboard.

## Playbooks and semantic execution

[Playbook schema](../packages/playbook-store/src/schema.rs) version 1 contains name, origin, steps, and optional description. Limits are 100 steps, 64 ASCII name bytes, and 280 UTF-8 description bytes. Steps are legacy_selector (action and wait) or semantic (intent). The database stores the envelope in steps_json and upserts by name; legacy descriptions are migrated additively.

[SemanticIntent](../packages/macro-engine/src/executor.rs) includes role, labelQuery, containerQuery, rawPrompt, ordinalIndex, isLast, isPlural, entryUrl, and primaryTargetNoun. Grounding uses role admission, label/context scoring, document-order ordinal selection, and live node geometry. It is a pointer-action executor, not a general text-entry planner.

Saved playbooks and ephemeral commands use [the runner](../packages/orchestration-engine/src/runner.rs). Legacy selector failures return repair needs without invoking task HealingReplay. Semantic drift handling and signature-history helpers do not constitute a user-facing rollback feature.

The plural command lane resolves at most 30 candidates, approves the batch and each click, and halts on failures. Snapshot document order is the ordering contract. Entry routes in production come from the curated route table; optional account/model route interfaces are not configured.

## Approval and persistence boundaries

Legacy click/fill/submit and semantic intent execution require explicit decisions. Typed navigate/download_links actions do not. Decisions identify the run/step, are single-use, and have five-minute deadlines. The generic low-level executor refuses submit; approved form submission uses a dedicated path.

Task approvals write sentinel_decisions before execution. Playbook decisions use session_events text. The preview_approval/resolve_approval demo is separate from execution gates. No configurable risk tiers or ATS-specific submission feature exists.

Database tables include session_events, playbooks, runs, signature_history, entry_urls, tasks, task_checkpoints, and sentinel_decisions. Tasks have durable step checkpoints; playbook runs have summary journaling, not equivalent restart/resume semantics. Some journal writes are best effort.

Completed command-bar save keys are held in memory (32-entry cap) and consumed by save_run_as_workflow. Playbook persistence is durable; the key registry is not. TaskWorkspace saves legacy steps through save_playbook instead.

See [STATUS.md](STATUS.md) for validation and [POC.md](POC.md) for outstanding acceptance work.
