# Clinch architecture

Reconciled against the working-tree implementation on 2026-09-24. Code is authoritative; this document describes existing paths, not a target architecture.

## Runtime and ownership

The React/TypeScript frontend runs in Tauri's webview. Rust commands in [lib.rs](../apps/desktop/src-tauri/src/lib.rs) delegate to [AppService](../apps/desktop/src-tauri/src/service.rs), which owns the database pool, managed browser handle, connected portal, pending approvals, and completed-run registry.

Chromium is an independently launched process controlled through chromiumoxide/CDP. Its viewport is mirrored into React; input is performed in the managed window or through typed backend actions. The app does not embed Chromium's native window or forward pointer/keyboard input through the preview.

The operation semaphore serializes task execution, playbook execution, sync, and manual navigation where acquired. Background-context acquire/release/take-control methods do not acquire this semaphore; do not assume every browser lifecycle action is protected by the run lock.

## Packages

- `browser-driver`: process launch/restart, CDP cookies and navigation, constrained actions, accessibility snapshots, Set-of-Marks clicks, picker bindings, JPEG previews, and screencast frames.
- `session-sync`: consent/request validation, platform profile paths, temporary cookie database reads, decryption, localStorage extraction, and source user-agent lookup.
- `credential-vault`: read-only macOS Safe Storage access and Windows DPAPI key access, including App-Bound detection.
- `macro-engine`: validated version-1 macro JSON, atomic recording publication, bounded selector repair, semantic grounding, and plural execution.
- `orchestration-engine`: durable task state/checkpoints, task approvals, playbook step dispatch, command resolution, and route lookup.
- `playbook-store`: shared SQLite initialization, validated playbook persistence, run summaries, signature history, and entry-URL storage helpers.
- `filesystem-tool`: download extension finalization from bounded file inspection.
- `llm-provider`: selector-repair process contract. Intent parsing has a separate provider implementation in the orchestration engine.
- `extension-bridge`: plain JavaScript MV3 companion; excluded from Cargo workspace membership.

Dependency direction is desktop → orchestration → macro → browser driver; orchestration and desktop also use playbook-store. Playbook-store uses macro/browser types, browser-driver uses session-sync's cookie contract, and session-sync uses credential-vault. Download finalization uses filesystem-tool.

## Execution paths

### Task macros

`TaskWorkspace → run_task → Engine::run_task → HealingReplay`.

The current form replays an existing macro by workflow name. The backend's `TaskRequest::plan` can construct navigate, optional link click, and download-links steps for a first run when selectors are supplied by an API caller. Both paths checkpoint before and after each step. Completed first runs publish a versioned macro atomically.

Task replay requires a headless browser. Startup recovery marks unfinished task state interrupted; it does not repeat uncertain actions or provide a resume API. Selector repair is confined to a failed target or wait stage, with no repetition of an action whose wait failed.

### Playbooks and natural-language commands

`WorkflowForm → save_playbook/list_playbooks/execute_playbook → run_steps`.

Playbooks contain legacy selector steps or semantic intents. Legacy playbook steps use `replay_step`, not the task lane's `HealingReplay`; selector failures stop for repair. Semantic intents ground role, label, and optional contextual fields against live accessibility data, then use node geometry for clicks.

`CommandBar → dispatch_natural_command → resolve_command` chooses a saved playbook, one ephemeral semantic intent, or a plural batch. Saved-name/host matching is deterministic. Ephemeral parsing can invoke a configured intent provider, with a deterministic fallback for returned failures or invalid output.

Cold-path entry routing uses a tiered resolver, not a curated route table (deleted): explicit domain → account directory (unwired) → LLM intent adapter (unwired) → direct-open grounding ladder (saved site shortcut → structured site directory → fenced domain grounder) → honest miss or grounded search fallback. The directory rung is composite: Brave Search API when `CLINCH_BRAVE_API_KEY` is set, keyless DuckDuckGo HTML as the zero-config fallback — backend HTTP in memory, so the browser never sees a search page. The production grounder is `LlmDomainGrounder::from_env()` — Groq cloud via `GROQ_API_KEY`, or local Ollama via `CLINCH_GROUNDER_PROVIDER` — declining to a stub when unconfigured or offline. It returns only a bare domain from the site slot plus a region hint; the domain is validated in Rust (https, valid TLD, no credentials, no raw IP) before anything navigates, and a malformed response degrades to the next rung, never to a guessed `www.{noun}.com`. An entry URL on the first semantic step supports pre-navigation and re-anchoring; the separate `entry_urls` table is not read on this dispatch path. After a grounded navigation succeeds, the service journals a consent-gated shortcut offer; an accepted save persists a site shortcut that later runs resolve through the shortcut rung with no model call.

Plural dispatch snapshots candidates, asks for batch approval, and checks approval before each click. It caps execution at 30 candidates and stops on drift/failure. Candidate ordering follows snapshot document order, not a geometric visual sort.

The exported decomposition and dynamic-variable extraction helpers are tested library code; the desktop does not call them. A compound natural-language prompt is not automatically a sequence of saved steps.

## Persistence and transport

Task snapshots and append-only checkpoint rows commit together with revision checks; browser I/O stays outside those transactions. Task approval records use `sentinel_decisions`. Playbook decisions and diagnostics use `session_events` text rows.

The `playbooks` table contains name, portal URL, serialized step envelope, description, and timestamps. Save upserts by name. `runs` journals playbook/ephemeral run summaries; tasks have their own snapshot/checkpoint storage. Some run/telemetry writes are best effort, so these are not a complete immutable audit trail.

Completed command-bar runs can be saved using a session-memory registry capped at 32 entries. Saved playbooks and run rows survive restart; registry keys do not. Signature-history and entry-URL helpers exist without a user-facing management/rollback surface.

Task and playbook progress use Tauri Channels. The JPEG mirror polls `browser_viewport`; acquired screencasts use `browser-screencast-frame` events. Closing a frontend view does not itself cancel an executing task.

## Session and provider boundaries

Session import is explicit and supports Chrome, Brave, and Edge. The extension listens through a desktop loopback server at `127.0.0.1:9223`; requests are initiated by the desktop, correlated, expired, and domain-filtered. Source profiles and the managed profile are separate.

Manual login uses the managed Chromium window. The URL classifier distinguishes same-origin landings, login path markers, known SSO challenges, and unknown origin mismatches. Known SSO is still pending auth until returning to the portal. This classifier is a heuristic, not proof of account access.

**Settle-time challenge/auth branching.** After a direct-open run settles, `settle_ephemeral_outcome` captures the final frame and then branches three ways:

1. **Challenge detected** (title, challenge-platform URL, visible text; checked first and outranks everything else) → the L1 → L1.5 → L2 handoff ladder. L1 auto-escalation restarts the attached session as off-screen headed on the same app-owned profile and polls ~30s for clearance; a cleared escalation is shut down gracefully (CDP close flushes clearance cookies to the profile) and the browser detaches, so the next acquisition launches lazily. L2 Take Control hands the user a headed managed window already on the challenged page.
2. **No challenge, clean signed-out guest landing** → an auth-sync card offering L1.5 session lending as the persistence mechanism. The lend is origin-tagged (`SessionLendOrigin::{Challenge, GuestLanding}`) so the post-lend re-probe is origin-aware: challenge runs check the gate, guest landings check for authenticated state. One registry, one `lend_session` Tauri command, one attempt per run; a repeat tap replays the stored outcome. Post-sync the card can show `synced (persisted)` or return to the offer on failure.
3. **Authenticated or unclassifiable** → silent success, as before.

The auth probe (`auth_state()`, in `browser-driver/src/session.rs`) reads URL, title, and up to 4,000 characters of visible `innerText`, and fails open to `Unknown` on any probe failure. Cookie presence alone never proves authentication. Saved replays intentionally skip settle-time detection (last live frame only) — replaying someone's stored run must not reinterpret a page.

**Session lending and revocation.** A sync card tap is consent for one attempt: `request_bridge_session` long-polls the companion extension for the settled host's cookies, `inject` writes them to the managed profile preserving the server's exact expiry (Chromium owns persistence in its cookie database; cookies without an expiry stay true session cookies), the page re-navigates, and the origin-aware probe verifies. Failure reasons map to static user-facing labels (no extension, extension timeout, no cookies, injection/navigation failure). `forget_site_session` deletes the host's cookies from the app profile via CDP and returns the card to the signed-out offer. Journals carry host and cookie counts only — never cookie values. The daily browser profile is never attached, mounted, or copied; nothing is written back to it.

No cloud provider is configured by default. The domain grounder activates only through explicit environment configuration (`CLINCH_GROUNDER_PROVIDER`); unset means the declining stub and the ladder degrades to shortcuts, directory, or the honest miss. Optional adapters are trusted executables, not sandboxed model runtimes. See [TRD.md](TRD.md) for privacy, timeout, and action limits, [STATUS.md](STATUS.md) for validation, and [FUTURE_FEATURES.md](FUTURE_FEATURES.md) for unimplemented ideas.
