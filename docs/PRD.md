# Clinch — Product Requirements

Owner: (unassigned) · Rewritten 2026-09-27.
Product authority: [CONSTITUTION.md](CONSTITUTION.md). Code is the authority on what is actually built.

## The product in one paragraph

Clinch does the boring step on a real website for you — and lets you watch it happen. You say "open my profile on Reddit" or "pull this month's bill," and Clinch drives a real browser the way a person would: moving the cursor, opening menus, clicking. Anything irreversible waits for your tap. When it finishes, it shows you proof. When it can't, it says so honestly instead of pretending.

Chatbots explain. Clinch finishes.

## Clinch next to Muse

Muse is a cloud life assistant that tries to run your errands: you talk, it books, shops, and keeps working from the vendor's cloud, and you hand over the keys. Clinch is a local, consent-first browser operator for repeatable website work: the browser is yours, a login is lent once with your consent, every irreversible action is a tap, and when the site fights back you get the window handed to you — not silence, not a lie about how it logged in.

| | Muse | Clinch |
|---|---|---|
| Where it runs | Vendor cloud | Your machine today; your own worker next |
| Interface | Chat / app | Command bar, saved playbooks, live preview |
| Job | Run errands end-to-end, 24/7 | Repeat actions on sites you already use |
| Trust model | Hand over the keys, monitoring watches | Approve the click; forget the site anytime |
| When the site fights back | Often blocked or stalled | Designed handoff: silent retry → session lend → Take Control |
| Form filling, email, phone | Core pitch | Explicitly not built yet |
| Household / schedule | Product promise | Backlog |

The wedge is the whole strategy, and it is durable: people who will not give a vendor their inbox and cards, and sites that already block cloud agents. Muse wins on distribution and "just talk to it." Clinch wins on ownership, visibility, and failure honesty. We do not try to become Muse on the desktop — we are the trusted hands Muse cannot be.

In one line: **it never sends or spends until you tap yes, and you can see the page.**

## The job: recurring website admin

Clinch's daily loop is narrow on purpose: the portals people already live in — insurance, the school parent site, the landlord portal, hospital billing, the airline account. Cloud agents get blocked there or are too scary to trust there. Clinch's product is the boring admin made repeatable:

- "Every month, open this bill site and pull the PDF" — then stop, or hand it to you.
- "Open the camp forms and stop before submit."
- "Open my profile / settings / notifications on this site" — through the site's own menu, verified against the live page.

Saved playbooks are the life-easier part: do it once with your eyes on it, name it, replay it next month. Not a general agent. Not a job-application engine, not a deal radar — those are distractions.

## Who it's for

One engine, three faces — not one UI pretending a retiree and a Rust engineer want the same thing:

- **Web (flagship):** non-technical and aging users who won't install anything. A browser tab with a prompt, a live view, Approve, and Take Control.
- **Phone wrapper:** the same people away from their desk. Approvals and "needs you" come first.
- **Desktop (pro / privacy):** technical users and small businesses. Local daemon, optional extension session-lend, journals if they want them.

Later: households — a second seat sees shared tasks and approvals, never the other adult's login cookies.

## The v1 vocabulary: five intents

Every request reduces to one of five intents. Anything else is a saved playbook built from these, or a clarifying question.

- `OPEN <site>` — "open Reddit for me"
- `GO <profile | settings | notifications | logout> on <site>` — "open my profile on Reddit"
- `DO <named playbook>` — replay something that worked before
- `WATCH <named playbook>` — run it on a schedule
- `ASK_ME` — Clinch stops and asks when it's stuck, or before anything is sent or spent

Natural language is welcome; the system still reduces it to these five. v1.5 adds `DRAFT` (write, never send) and `FILL` (fill a form, stop before submit) — only after the five are boring.

## How a run feels

1. **Understand.** Clinch pulls out the slots — site, object, playbook — deterministically first. A model helps only when the site name is ambiguous, and it never invents URLs. No model is called unless you configured one.
2. **Route.** A saved shortcut or playbook hits first. Otherwise the site name is grounded to a real domain through a fenced lookup. If it can't be grounded honestly, Clinch asks you instead of opening a search page.
3. **Act.** A managed browser — always off-screen, never a visible window — navigates and clicks with human-like input: hover, press, release, with a visible cursor so you can see where it is. Low-risk clicks flow; anything risky pauses for your tap.
4. **Verify.** This is what makes Clinch different: the AI never grades its own work. A deterministic verifier checks the live page — right site, right object, actually landed — and only it can declare success. A click is not success.
5. **Receipt.** You get a short, plain account of what happened, with the final view. Or an honest failure naming what was tried, with a Take Control button that hands you the browser exactly where it got stuck.

## Trust is the product

- **Never sends, spends, or leaves the site you named without a tap.** This is architecture, not a setting.
- **Bot checks and captchas:** Clinch tries silently first, then offers a consent-gated, one-time borrow of that site's login from your own daily browser, then hands you the controls. It never clicks a captcha itself.
- **Your logins stay yours.** Session lending is one-way (your browser → Clinch, never written back), per-site, and consent-gated. Clinch keeps its own copy, so signing out elsewhere doesn't sign Clinch out. "Forget this site" wipes Clinch's copy completely. Your daily browser is never touched.
- **Honest about being signed out.** If a page loads logged-out, Clinch says so and offers the one-tap sync — it never performs a "success" on a guest page.
- **Everything is journaled.** What it clicked, what it saw, what it decided. No silent actions.

## What's built today

- Open any site by name: grounded to the real domain with no search page shown; an honest "which site?" when it can't tell.
- Account nouns on the live site — profile, account, settings, notifications, logout — opened through the site's own menu, verified against the live page, and remembered per site so repeats go straight there.
- Save a successful command-bar run as a named playbook and replay it; batch commands with per-click approvals. (Save is consent-gated and currently offered on the command-bar and batch paths — not on login/session flows, where what you'd want kept is the session, not the click sequence.)
- Site shortcuts: after a first successful visit, save `site → address` so the next visit resolves instantly with no model call.
- Challenge ladder: silent retry → consent-gated session lend → Take Control, with the session stood down cleanly afterward.
- Large Preview: a near-fullscreen view of what the run saw — live while running, frozen frame when settled.

## What's next, in product order

Ship a narrow daily loop before more engine:

1. **One recurring portal workflow** a real person reuses weekly — download a statement, open an account page, check a reservation. The loop has to survive contact with a real Tuesday.
2. **A playbook list people can actually manage** — name, delete, rename, export, replay without assistance. Delete/rename/export don't exist yet; the reuse loop that makes this a daily habit is still thin.
3. **Reliability on five messy live sites** (the POC.md matrix), including signed-out and challenge paths. A stranger opens settings on a named site and never sees a search page congratulating them.
4. **Household renewals** — "stop before submit" as the default shape for anything with consequences.
5. **Only later:** email drafts, scheduling, a messaging surface, phone.

## What it will never be

- A Muse clone. Different job, different trust model, different brand.
- A silent agent. If it can't show you the page and name what it did, it doesn't ship.
- A cloud that holds your inbox. Local-first is the brand; the day the browser isn't yours, the wedge is gone.
- A form-filler that submits. `FILL` stops before submit; sending is always a tap.

## Measurement

Time-to-correct-page · % honest failures · % of Take Control handoffs completed · playbooks still working after a week. Not feature counts.

**Acceptance bar:** a stranger can open settings on a site they named and not see a search page congratulating them.

---

*Implementation detail lives in [ARCHITECTURE.md](ARCHITECTURE.md), [STATUS.md](STATUS.md), [FEATURES.md](FEATURES.md), and [TRD.md](TRD.md). Where this document and the code disagree about what is built, the code wins.*
