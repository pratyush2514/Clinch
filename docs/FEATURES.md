# Best Features & Approach
## Autonomous Web Action Studio

Every feature below is filtered against one test: **does this save real time or build real trust, or is it decoration?**

---

## Tier 1 — Core Differentiators (build these first, they ARE the product)

### 1. Dual-Pane Execution: Live Webview + Spatial Canvas
Left pane shows the actual, real-time execution (clicks, form fills, page navigation) in an embedded Chromium instance. Right pane compiles the output into spreadsheets, comparison cards, a step/action log, and (for Job Auto-Apply) a Kanban board — with export to Notion, Slack, or local file. This replaces "wall of chat text" output with artifacts people actually use, and replaces "invisible cloud VM" execution with something people can watch and trust.

### 2. Self-Healing CDP Macros
The first run of any task is LLM-planned and gets recorded as a deterministic macro (selectors + actions). Every subsequent run replays that macro directly — sub-second, no LLM call, near-zero marginal cost. If a site's layout changes and a selector breaks, only that broken step gets sent to the LLM for a targeted repair, and the macro updates itself. This is the direct, structural answer to competitors that re-plan every click on every run — both a speed and a cost moat.

### 3. Sentinel Gate
Before any state-changing action (submit, pay, delete, send), execution pauses and shows a concrete, human-readable preview — the actual filled form, the actual amount and recipient — and waits for a one-click decision. Configurable per action type and per Playbook, with risk tiering. For the Job Auto-Apply flow specifically, this is a **hard, non-configurable floor**: every submission requires a human click, full stop, because a banned job-search account is a severe, asymmetric cost to the exact user this product is trying to help.

### 4. 1-Click Local Cookie Sync
On launch, the embedded browser imports the user's active session cookies from their local Chrome/Brave installation, so authenticated portals (Workday, Amazon, enterprise SSO sites) work immediately — no fresh login, no 2FA prompt, no cold-session anti-bot flag. Paired with a plain-language consent screen (naming exactly what's read) and a mandatory manual-login fallback whenever import fails, so the convenience never becomes a silent failure mode.

### 5. Cmd+K Intent Bar + Playbook Cards
Two entry points into the same execution engine: type a plain-English task, or click a pre-built Playbook Card ("Collect Monthly Utility Invoices," "Apply to This Job"). No blank prompt box as the only way in — this is what makes the product usable by non-technical people on day one.

### 6. Playbooks as Versioned, Shareable Artifacts
Any successful run saves as a named, parameterized `.playbook` file (with its recorded macro attached), git-diffable and human-readable — not an opaque prompt blob. Rerun with new inputs, schedule it, or (v2) share it with a team. This is the habit-forming engine: the product becomes indispensable the moment someone reruns a Playbook instead of redoing the task by hand.

## Flagship Launch Playbooks (three anchors, shipped out-of-the-box)

### Job Application Engine
One-time setup: Master Profile (personal details, work authorization), a Project/Portfolio Vault (links, case studies), a resume/cover-letter vault, and an answer bank for common custom questions. On trigger, the engine detects the ATS (Workday, Greenhouse, Lever, Taleo, iCIMS), logs into the user's already-authenticated portal session, populates the multi-page form, uploads the right resume variant, and drafts tailored answers to open-ended questions from the local profile vault. Tracks every submission on a Kanban board (Applied → Interview → Offer). **Every single submission requires a Sentinel Gate approval in v1** — no full-auto-submit, regardless of policy settings, given real account-suspension risk on ATS platforms.

### Invoice/Bill Harvester (lower stakes, proves the core loop)
Logs into utility, telecom, insurance, and SaaS portals monthly, downloads PDF statements to a local folder, and compiles an expense summary on the Spatial Canvas.

### Competitive Pricing & Deal Radar
Monitors price drops across 5+ e-commerce or travel portals simultaneously on a schedule, surfacing threshold-crossing drops as cards on the Spatial Canvas.

## Tier 2 — Strong Follow-ons (v2, once core loop is proven)

### 6. Correction-Rate & Macro-Reuse Trust Meters
Surface, per Playbook, how often a run needed a manual correction and what % of runs hit the fast macro-replay path vs. a fresh LLM plan — turns reliability into a visible, satisfying signal instead of an invisible backend metric.

### 7. Shared Team Playbook Library
Playbooks become shareable, reviewable (git-diff-style), and assignable within a team, with role-based approval delegation and a central audit log — the wedge into team/enterprise revenue.

### 8. Scoped Permission Grants, Visualized
Show exactly which folders, sites, and credential types a Playbook can touch, and let the user narrow it before saving.

### 9. Run Replay & Diff
Full step-by-step replay of any past run, including before/after page or file state — for debugging trust issues and onboarding teammates to a shared Playbook.

### 10. Deep Research / Live Signal Radar (adjacent persona, not v1)
As the user views a company, product, or news page in the left pane, the right canvas streams related discussion (X, Reddit, HN) and price/history context — a research-workspace mode for founders/analysts, built once the core execution loop is proven.

## Tier 3 — Later, Higher-Risk

### 11. Native App Reach (macOS Accessibility API)
Extend beyond the browser to control native Mac apps. Genuinely differentiated scope — but technically fragile (permissions, notarization, reliability of arbitrary UI automation); its own R&D track, not a launch dependency.

### 12. Playbook Marketplace
Once private sharing is proven inside teams, open a marketplace for publishing/discovering Playbooks.

## Explicitly Rejected (gimmicks and risky shortcuts to avoid)

- **Persona/avatar customization for the agent** — doesn't save time or build trust; skip it.
- **Engagement notifications / streaks / gamification** — against the "addictive through real value" goal; do not build.
- **Chat-first interface as the primary surface** — every competitor already owns this; dilutes the dual-pane differentiation.
- **Racing agentic-browsing benchmark leaderboards as marketing** — not a fight a small team wins.
- **Silent, undisclosed cookie import with no consent screen or fallback** — the sync mechanism itself is in scope (see Tier 1, #4), but skipping the consent explanation or the manual-login fallback is not an acceptable shortcut.
- **Full-auto-submit on the Job Application Engine, at any policy setting** — the one place this product must never offer a "just do it without asking" toggle, because the downside (account suspension) is too asymmetric for the user it's meant to help.
