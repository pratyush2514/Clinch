# Product Requirements Document (PRD)
## Autonomous Web Action Studio — Clinch (working name)
**Status:** Draft v3 (Tauri locked, cookie sync locked, dual-pane mandated, 3 anchor Playbooks)
**Owner:** Pratyush
**Founder:** Pratyush
**Date:** September 2026

---

## 1. Problem Statement

People and teams spend hours a week on repeatable, multi-step digital work that lives across the browser, files, and local apps: filling out ATS job applications, harvesting monthly invoices from utility portals, tracking prices across e-commerce/travel sites. Existing answers force a bad trade-off:

- **Cloud agents (Meta Muse, Manus, Grok Bot)** run inside datacenter VMs — they get IP-blocked, CAPTCHA'd, or 2FA-locked out of exactly the authenticated, anti-bot-protected sites (Workday, LinkedIn, Amazon) where this work happens, and they re-plan every click via a fresh LLM call.
- **Local CLI daemons (OpenClaw)** run on the user's machine (good for auth) but execution is invisible — buried in terminal logs — with no visual approval step before a state-changing action fires.
- **AI browsers (Aside)** watch live page renders but ask you to replace your daily browser, and output stays trapped in a single-session view rather than becoming a reusable asset.

Nobody has combined **frictionless authenticated access (real local session, zero re-login)**, **visible, steerable execution**, and **reusable output** in one desktop tool.

## 2. Target Users & Anchor Playbooks (v1 launch set — three, not vague personas)

1. **Job seekers** — Automated Job Application Engine: auto-filling multi-page ATS forms (Workday, Greenhouse, Lever, Taleo, iCIMS) from a local Master Profile, project index, and resume vault. High pain (30–45 min/application), high emotional stakes, high virality.
2. **Anyone managing recurring bills** — Monthly Invoice & Bill Harvester: logging into utility, telecom, and SaaS portals to download PDF statements into local folders and compile an expense sheet. Lower stakes, proves the core loop cleanly.
3. **Shoppers/travelers/deal-hunters** — Competitive Pricing & Deal Radar: monitoring price drops across 5+ e-commerce or travel portals simultaneously, alerting on threshold hits.

**Secondary (v2+):** Growth/RevOps prosumers, small teams wanting shared Playbooks.
**Tertiary (v3+):** Enterprise ops teams needing compliance, audit trail, role-based approval.

## 3. Product Thesis

A **dedicated desktop application** (Tauri v2/Rust — locked, not Electron) that:
1. Executes multi-step web workflows **locally**, using the user's **already-authenticated browser session via 1-click cookie sync** — so authenticated, anti-bot-protected sites work from minute one, with no fresh login or 2FA friction.
2. Presents a **mandatory dual-pane viewport**: a live interactive webview (left) showing real execution with agent cursor/DOM highlights and a 1-click manual-takeover bar, and a Spatial Canvas (right) with a scrubbable action timeline, dynamic data tables, and Playbook cards.
3. Records successful runs as **self-healing CDP macros** — deterministic replay at near-zero cost, LLM invoked only to repair a broken selector.
4. Gates every state-changing action behind a **Sentinel Gate** — non-negotiable, per-submission approval on the Job Application Engine specifically, given account-suspension risk.

## 4. UI Architecture: Dual-Pane Viewport (mandated, not optional)

- **Left Viewport — Live Interactive Webview:** real-time DOM runner rendering the agent's actual clicks and field fills, cursor and DOM-highlight overlays so the user can follow along, and a always-visible 1-click manual takeover bar to seize control mid-run.
- **Right Canvas — Spatial Workspace:** a scrubbable Action Canvas (step-by-step timeline, replayable), dynamic data tables generated from extracted content, and Playbook Cards for triggering saved workflows. Supports export to local file, Notion, or Slack.
- **Cmd+K Intent Bar:** global shortcut for plain-English task entry, available regardless of which pane has focus.

This is a structural requirement, not a cosmetic layout choice — it's what makes the product legible as a workstation rather than a bot in a box.

## 5. Session & Authentication: 1-Click Local Cookie Sync

- On launch (or on first connecting a new site), the embedded Chromium instance imports the user's active session cookies from their local Chrome/Brave installation, so Workday, Amazon, and enterprise portal logins work immediately — no manual re-login, no fresh 2FA challenge, no cold-session anti-bot flag.
- **Required safeguards (non-optional engineering requirements, not nice-to-haves):**
  - An explicit, plain-language consent screen on first use, naming exactly what is read (local browser cookie store) and why.
  - The OS-level Keychain permission prompt this requires (macOS gates Chrome's Safe Storage key behind Keychain access) is surfaced to the user as part of that consent moment, not hidden.
  - **Graceful fallback:** if cookie decryption fails (browser version change, unsupported browser) or a needed portal isn't found in existing cookies, fall back to a one-time manual login inside the app's own embedded profile — never a hard failure.
- This mechanism is the v1 default across all three anchor Playbooks.

## 6. Core User Journeys (v1)

1. **Job Application Engine**: one-time Master Profile + resume/project vault setup → feed a job link or trigger via Cmd+K → left pane fills the ATS form using the synced session → Sentinel Gate shows a full preview before every single submission (no exceptions) → track results on a Kanban board (Applied → Interview → Offer) on the right canvas.
2. **Invoice Harvester**: a Playbook logs into N utility/SaaS portals (synced session), downloads PDFs, compiles an expense spreadsheet on the Spatial Canvas.
3. **Deal Radar**: a scheduled Playbook checks prices across 5+ sites and surfaces threshold-crossing drops as cards on the right canvas.
4. **Save and reuse**: any successful run saves as a named Playbook with its recorded CDP macro; reruns replay the macro directly rather than re-planning.
5. **Recover from a broken step**: a run hits a changed page layout → pauses, shows what broke → self-heals the selector or lets the user correct it inline and resume.

## 7. v1 Scope (MVP)

**In scope:**
- Tauri v2 (Rust) desktop shell, macOS first, hosting an embedded, CDP-controllable Chromium instance.
- 1-Click Local Chrome/Brave Cookie Sync as the default session mechanism, with consent screen and manual-login fallback.
- Mandated dual-pane viewport (live webview + Spatial Canvas) and Cmd+K Intent Bar.
- Self-healing CDP macro engine: record on first run, replay on repeat runs, targeted LLM repair on broken selectors.
- Sentinel Gate: configurable per action type; **mandatory, non-bypassable approval before every submission** on the Job Application Engine specifically.
- Local credential vault (OS Keychain-backed) for any credentials the user explicitly stores in-app.
- Local-first storage (SQLite, WAL mode) for runs, Playbooks, macros, and the application-tracking log.
- All three anchor Playbooks shipped out-of-the-box: Job Application Engine, Invoice Harvester, Deal Radar.

**Explicitly out of scope for v1:**
- Native macOS Accessibility API automation of non-browser apps (v2).
- Team sharing, roles, SSO (v2/v3).
- Windows build (post-Mac validation), mobile companion.
- Full-auto-submit on the Job Application Engine — every submission requires a human click in v1, regardless of policy configuration.

## 8. Success Metrics

- **Time-to-first-successful-run**: under 10 minutes from install, cookie sync should make this fast by design.
- **Macro reuse rate**: % of runs hitting the fast self-healing-macro path vs. requiring a fresh LLM plan.
- **Playbook reuse rate**: % of users rerunning a saved Playbook within 7 days — the real "addiction" signal.
- **Sentinel Gate approval rate & time-to-decision**: high approval rate + fast decisions signal a trustworthy preview.
- **Account-health signal (self-reported)**: for the Job Application Engine, track any user-reported portal warnings/lockouts as an early-warning metric.
- **Cookie-sync success rate & fallback rate**: % of sessions that sync cleanly vs. fall back to manual login — a direct signal of the mechanism's real-world reliability across browser versions.

## 9. Non-Goals

- Not a general-purpose chatbot — every interaction ends in an executed action or a saved Playbook.
- Not competing on browsing-benchmark leaderboards as marketing.
- No engagement mechanics disconnected from real time saved.
- **Never silently mass-submit job applications or any state-changing action without a per-instance human decision in v1.**

## 10. Risks

- **Platform account-ban risk**: using valid credentials via automation is generally not CFAA "unauthorized access" (*Van Buren*, *hiQ v. LinkedIn*), and ToS violations are civil, not criminal — but that doesn't stop Workday/LinkedIn from suspending an account flagged as automated. Mitigated by pacing, mandatory Sentinel Gates, and conservative rate limits, not eliminated by legal precedent.
- **Cookie-sync fragility**: browser security updates can change cookie-store encryption; mitigated by the required manual-login fallback, not by assuming permanence.
- **Macro fragility**: self-healing reduces but doesn't eliminate classic RPA brittleness on layout changes.
- **Reputational risk of the flagship use case**: mass semi-automated job applications are already a friction point for recruiters; answer quality and sane rate limits matter as much as speed.
