# Technical Requirements Document (TRD)
## Autonomous Web Action Studio

**Status:** Draft v3 (Tauri locked, cookie sync locked, dual-pane mandated, self-healing macro engine specified)
**Companion to:** PRD.md, ARCHITECTURE.md

---

## 1. Platform & Packaging

- **Target OS (v1):** macOS (Apple Silicon primary, Intel best-effort). Windows is a v2 target.
- **App shell — LOCKED: Tauri v2 (Rust).** No longer an open decision. Rationale: ~5–10MB installer bundle vs. Electron's 150MB+, ~30–50MB RAM footprint vs. Electron's typical 150MB+, native OS Keychain/Keytar integration, sub-second IPC between the Rust core and the frontend. Electron is rejected for this product specifically because bundle size and memory footprint directly undercut the "lightweight workstation" positioning.
- **Automation surface:** Tauri's default webview (WKWebView on macOS) does not expose Chrome DevTools Protocol. The left-pane live execution view hosts a **separately embedded, real Chromium instance** controlled via CDP — Tauri provides the shell and native OS bindings around it, not the automation surface itself.
- **Distribution:** Signed, notarized `.dmg` with auto-update; Mac App Store deferred (sandboxing constraints complicate automation).

## 2. Functional Requirements

### 2.1 Session & Authentication: 1-Click Local Cookie Sync (LOCKED)
- On first connecting a site, or on launch, the app reads the user's local Chrome/Brave cookie store and imports the relevant session cookies into the embedded Chromium instance via CDP's `Network.setCookie`, so authenticated portals (Workday, Amazon, enterprise SSO-backed sites) work immediately without a fresh login or 2FA challenge.
- **Implementation requirements:**
  - Locate and read the local browser's `Cookies` SQLite database.
  - Decrypt values using the OS-Keychain-gated Safe Storage key (macOS: Keychain access, which surfaces a native OS permission prompt — this prompt IS the user's point of informed consent at the OS level, and must be paired with an in-app explanation before it appears, not sprung on the user unexplained).
  - Inject decrypted cookies into the embedded Chromium profile's cookie jar via CDP; never persist raw imported cookie values outside the app's own encrypted local storage.
  - **Mandatory fallback:** if decryption fails (browser version incompatibility, unsupported browser, no local cookie found for the target domain), fall back to a one-time manual login inside the app's own persistent embedded profile — this path must always exist and be tested, since cookie-store formats change across browser releases.
- This is the default mechanism for all three v1 anchor Playbooks (Job Application Engine, Invoice Harvester, Deal Radar).

### 2.2 Task Execution Engine
- Accepts a natural-language task (via Cmd+K) or a triggered Playbook Card.
- First run: LLM produces a plan, executes each step against the embedded Chromium instance, and **records the step sequence as a macro** (selector paths, action types, inputs) as it executes.
- Repeat runs: replay the recorded macro directly via native CDP socket commands — no LLM call, millisecond-scale step latency, $0 marginal API cost.
- Self-healing: if a recorded selector fails to resolve, pause that step, send only the broken step's local context to the LLM for a targeted repair, update the macro file, and continue.
- Supports pause / resume / edit-step-and-resume / abort at any point, backed by checkpointed state.

### 2.3 Self-Healing CDP Macro Architecture (detailed spec)
- **First Run:** LLM plans the DOM interaction sequence; each planned action is executed and simultaneously serialized into a deterministic CDP script (selector, action type, input value, wait conditions).
- **Subsequent Runs:** the orchestration engine replays the serialized CDP script directly against the browser's DevTools socket — no LLM involvement, no network round-trip to a model provider, sub-second-to-millisecond execution per step.
- **Self-Healing Loop:** a replay step whose selector no longer resolves triggers a narrow-scope LLM call containing only the current DOM state around the expected element; the LLM returns a corrected selector, which is written back into the macro file so the fix persists for future runs.
- Macro files are versioned (JSON), with a `last_healed_at` timestamp and a healing-event history for transparency.

### 2.4 Dual-Pane UI (mandated)
- **Left pane — Live Interactive Webview:** renders the embedded Chromium instance's actual viewport in real time; overlays a visible agent cursor and DOM-highlight indicators on the element currently being acted on; includes an always-visible, single-click "take manual control" bar.
- **Right pane — Spatial Canvas:** a scrubbable Action Canvas (step timeline, replayable forward/back), dynamic data tables generated from extracted content, and Playbook Cards. Supports export to local file, Notion, or Slack.
- **Cmd+K Intent Bar:** global shortcut for plain-English task entry, available regardless of which pane has focus.

### 2.5 Sentinel Gate (Approval System)
- Rules engine: `{action_type: [submit_form, send_message, make_payment, delete_file], requires_approval: bool, preview_required: bool, risk_tier: enum}`.
- **Non-configurable override for v1:** the Job Application Engine requires a Sentinel Gate approval before every submission, regardless of policy configuration.
- Preview renders human-readable state before blocking on confirmation. All decisions logged immutably.

### 2.6 Anti-Detection Guardrails
- Randomized micro-delays (200–800ms) between CDP actions to avoid obviously robotic timing.
- No headless-mode artifacts (`navigator.webdriver` flags, synthetic viewport signatures).
- Reduces detection risk; does not guarantee it — communicated honestly in-product, not marketed as a bypass.

### 2.7 Credential Vault
- OS Keychain-backed, local-only storage for credentials the user explicitly enters into the app.
- Injected directly into form fields by the automation layer; the LLM never receives raw secret values.

### 2.8 Local Data & Privacy
- Runs, Playbooks, macros, screenshots, imported-cookie metadata, and the Master Profile/resume vault stored locally by default (SQLite + filesystem blob store).
- Minimum-necessary context sent to the LLM provider (page text/structure relevant to the current step or a broken-selector repair) — never raw credentials or cookie values.
- Visible, per-run log of exactly what data (if any) left the device.

## 3. Non-Functional Requirements

- **Reliability:** a failed or ambiguous step fails visibly on the Canvas — never silently continues (critical for the Job Application Engine).
- **Performance:** macro-replay steps execute in well under 1 second; first-run steps are bounded by model latency.
- **Security:** the embedded Chromium instance runs sandboxed; filesystem access scoped per-Playbook; imported cookie data never persisted outside the app's own encrypted store.
- **Auditability:** every action, Sentinel Gate decision, cookie-sync event, and data-egress event is logged with timestamp and immutable ID.
- **Macro & Playbook portability:** stored as plain, versioned JSON — git-diffable, exportable, importable.

## 4. Trust & Legal Considerations

- Under U.S. case law (*Van Buren v. United States*, *hiQ Labs v. LinkedIn*), using a user's own valid credentials/session to access their own accounts via automation is generally understood not to constitute CFAA "unauthorized access," and a bare ToS violation is civil, not criminal. This does not eliminate the operational risk that a target platform can suspend an account it flags as automated — mitigated by pacing, mandatory Sentinel Gates, and the fallback design above, not by legal precedent alone.
- The cookie-sync mechanism reads only the local user's own browser data via an OS-permissioned path (Keychain-gated decryption) — this must be communicated transparently in-app (consent screen before the OS prompt appears) both because it's the right thing to do and because undisclosed credential-adjacent data access is the kind of pattern security software and platform reviewers scrutinize.
- Local-only storage of credentials, resumes, and profile data reduces GDPR/CCPA exposure.
- None of the above is legal advice; a real legal review is warranted before launch, especially for the Job Application Engine given ATS-specific terms of use.

## 5. Data Model (high-level entities)

- `Task` — a single user request; has one or more `Run`s.
- `Run` — one execution attempt of a Task or Playbook; contains ordered `Step`s.
- `Step` — atomic action with type, target selector, inputs, outputs, screenshot, status.
- `Macro` — the recorded, replayable CDP step sequence for a Playbook, versioned, with `last_healed_at` and a healing-event history.
- `Playbook` — versioned, named, parameterized template; references a `Macro` once one exists.
- `SentinelGateEvent` — linked to a `Step`; records the policy rule applied, preview shown, decision.
- `CredentialRef` — pointer into OS Keychain, never the raw secret.
- `BrowserProfile` — the app-owned embedded Chromium profile that receives synced cookies from the local Chrome/Brave installation, with manual-login fallback state.
- `MasterProfile` (Job Application Engine) — personal details, work authorization, portfolio links, resume/cover-letter files, answer bank for custom questions.

## 6. Testing & Quality Requirements

- Golden-path regression suite of real ATS/portal task recordings, re-run on every macro-engine or model change.
- Explicit test coverage for the cookie-sync fallback path (simulate decryption failure, unsupported browser version, missing domain) — this path must never hard-fail.
- Explicit test coverage for Sentinel Gate bypass attempts and the non-configurable submit-approval floor on the Job Application Engine.
- Chaos testing: network drop mid-run, page-structure change mid-macro-replay, credential vault lock — must fail visibly, never silently corrupt state.

## 7. Open Technical Decisions

1. Exact embedded-Chromium approach: bundled Chromium build vs. CEF vs. a Playwright-managed instance rendered into the Tauri window.
2. Selector-recording strategy for macros (CSS selector vs. accessibility-tree path vs. hybrid) — affects self-healing accuracy.
3. Cross-browser cookie-store support beyond Chrome/Brave (Safari, Firefox) — different encryption schemes, deferred prioritization.
4. Scoping mechanism for filesystem/Playbook permissions (per-folder grants vs. per-run sandbox).
