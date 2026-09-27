# Clinch — Local to Cloud Transition Guide

Audience: an AI coding agent (or a new engineer) dropped into this repo with
no prior context. Product authority is [CONSTITUTION.md](CONSTITUTION.md);
code is the authority on what is actually built. This doc explains where the
product is going (cloud) and what that changes in the code.

## What Clinch is (one paragraph)

Clinch turns a short intent into a finished action on a real website or a
saved playbook — and it never sends, spends, or leaves the site you named
without a tap. v1 ships five intents: `OPEN <site>`, `GO <profile |
settings | notifications | logout> on <site>`, `DO <named playbook>`,
`WATCH <named playbook>`, `ASK_ME` when stuck / to send / to spend. The
differentiation is supervised, verified execution on real websites: honest
verification (the verifier alone decides `COMPLETED`), visible navigation
(a cursor you can watch), last-click handoff, recurring verified playbooks,
readable logs, risk-sensitive approvals. Muse explains; Clinch finishes.

## Current state: local-first, cloud-shaped

The engine already runs as a standalone Linux binary (`clinch-daemon`)
speaking a JSON protocol (`clinch-protocol`) over a WebSocket. The desktop
app is a thin client in remote mode (`CLINCH_DAEMON_URL`). This split is
the foundation of the cloud move — it is not legacy code.

The critical architectural fact: **CDP never leaves the worker.** The
chain is always

```
client (tab/app) ←WebSocket, clinch-protocol→ clinch-daemon ←CDP, localhost→ Chromium
```

The client receives screencast frames, cursor events, and progress events;
it sends the 38 protocol commands. It never speaks CDP. The web face is
the same thin client the desktop app already is — minus Tauri.

## The deployment ladder (in order, no skipping)

1. **Local worker (now).** Daemon + client on the user's machine (or WSL2).
   Loopback is the trust boundary: no auth on the socket, companion
   extension talks to `127.0.0.1:9223`.
2. **Personal cloud (next).** The user's *own* daemon on their *own* box or
   VPS, phone/web client over the network. Validates the thin-client shape,
   the protocol over a real network, and session persistence — without
   multi-tenancy and without the vault hard requirement (it's their
   machine). This is the cheap cloud validation.
3. **Hosted cloud (later, on demand-pull).** Multi-tenant: a control plane
   spawns one worker (container/VM) per active session, each with its own
   Chromium. Gated on: the T1 acceptance bar (a stranger opens settings on
   a named site), the credential vault (mandatory *before* hosted sign-in —
   without it, hosted sign-in is a trust hole), and real session-cost data
   from rung 2.

Do not build rung 3 before rung 2 has real usage. One Chromium per active
session is real RAM per user; the unit economics are unproven.

## What changes per rung

### Session sync (the hard problem)

Local today: the companion extension reads cookies from the daily browser
and lends them one-way into Clinch's app-owned profile (domain-scoped,
exact server expiry, never write-back). On cloud the extension cannot reach
the worker's localhost, so the ladder becomes:

1. **"Sign in there"** (v1): worker opens the real login page, streams it;
   the user types credentials into the streamed view. Clinch never sees or
   stores the password — keystrokes forward to the site's page.
2. **Vault-mediated sign-in**: user opts credentials into the vault; the
   worker logs in itself with *model-paused filling* (the model is frozen
   during the fill so the password never enters model context).
3. **Cloud lend**: the companion posts the site's cookies to the user's
   worker over an E2E-encrypted pipe (worker per-session public key; the
   relay cannot read the payload). One tap, one site, one expiry.
4. **Phone OTP handoff**: the phone wrapper approves 2FA codes.

Semantics never change: one-way, domain-scoped, expiry preserved,
"Forget this site" wipes the worker's copy, every lend journaled.

### Hosted-worker fencing (keep the code, disable on hosted)

- `session-sync` / `credential-vault` local profile readers: meaningless
  and dangerous on a hosted worker. Desktop-only; the worker must not
  expose these commands.
- Companion bridge listener (`127.0.0.1:9223`): local-only; hosted workers
  must not bind it.
- Keyless DuckDuckGo directory rung: desktop/local only (ToS-gray,
  datacenter IPs get rate-limited). Hosted rung is the Brave API or an
  honest miss.
- `downloaded_file_path` needs an HTTPS download relay on web (the
  `\\wsl$\` translation trick dies outside local).
- `take_control` on a headless Linux worker: v1 is click-through-the-stream
  (synthetic input forwarding); noVNC is the later scale option.
- Auth: per-session authenticated WebSockets, TLS, daemon on the control
  plane's internal network. Cookies still never cross the socket — only
  outcome metadata (already true; keep it).
- `acquire/release_browser_context` is the "don't stream frames when
  nobody's watching" switch — frames are the bandwidth bill.

### What does NOT change

Verifier honesty, AX-not-DOM to models, deterministic Rust validation /
execution / safety, challenge ladder (L1 silent → L1.5 consent-gated lend
→ L2 Take Control), routing ladder (shortcut → directory → fenced grounder
→ honest miss), consent-gated saves, kill switch, readable log,
last-click handoff, Forget/Disconnect behavior. Cloud changes where the
browser runs, not how intent, cost controls, policy, or verification work.

## Code map for agents

| Crate / dir | Role | Cloud relevance |
|---|---|---|
| `packages/browser-driver` | CDP, AX snapshots, hit-testing, cursor events, off-screen headed launch | Unchanged; runs on the worker |
| `packages/orchestration-engine` | Intent parsing, ladders, verifier, task/playbook execution | Unchanged; runs on the worker |
| `packages/playbook-store` | SQLite playbook persistence | Worker-local; needs encrypted session store for hosted |
| `packages/clinch-protocol` | JSON wire contract (`PROTOCOL.md`) | **The** web-face contract — extend, don't replace |
| `packages/clinch-daemon` | Standalone WS server, 38-command dispatch | Becomes the per-session worker |
| `packages/session-sync`, `packages/credential-vault` | Local profile readers | Desktop-only; fence on hosted |
| `packages/extension-bridge` | MV3 companion (plain JS) | Local lend only; cloud lend is a new E2E pipe |
| `apps/desktop` | Tauri thin client (remote mode) | Reference client; web face mirrors it |

Removed 2026-09-27 (do not reintroduce): `entity_resolver.rs`
(unwired connected-account tier), `WindowMode::Headless` /
`LaunchOptions::replay()` (true `--headless=new`; everything is off-screen
headed now), stale `WorkflowForm`/`TaskWorkspace` names (components never
existed under those names in this tree).

## Open convergences (need a human decision — do not start unasked)

- **Macro lane vs playbook lane**: `run_task`/`HealingReplay` (legacy
  selector macros) vs `execute_playbook`/`run_steps`. Constitution says one
  thread. Converging deletes working behavior — needs explicit approval.
- **Two save verbs**: `save_playbook` vs `save_run_as_workflow`.
- **Shared chrome primitive**: profile/settings/notifications/logout should
  converge on "open identity menu → observe → choose → verify".

## The one test that matters

`log out from reddit` on the Linux lab, then read the `click_hit_test:`
journal lines: clean frames + menu opens = the lab works and the operator
is proven (T1). That gate — not enthusiasm — is what unlocks the hosted
build.
