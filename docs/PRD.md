# Clinch — Product Requirements

Owner: (unassigned) · Rewritten for readability 2026-09-27.
Product authority: [CONSTITUTION.md](CONSTITUTION.md). Code is the authority on what is actually built.

## What Clinch is

Clinch turns a short intent into a finished action on a real website — and it never sends, spends, or leaves the site you named without your tap.

You describe what you want in plain words — "open my profile on Reddit", "log me out of this site" — and Clinch drives a real browser the way a person would: moving the cursor, opening menus, clicking, filling fields. When it finishes, it shows you proof. When it can't, it says so honestly instead of pretending.

Chatbots explain. Clinch finishes. It is not a companion, a life OS, or a second chat app — it is the thing that does the boring step on a real site, with a visible handoff, for people who will not babysit it.

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

## How a run works

1. **Understand.** Clinch pulls out the slots — site, object, playbook — deterministically first. A model helps only when the site name is ambiguous, and it never invents URLs.
2. **Route.** A saved shortcut or playbook hits first (zero model calls). Otherwise the site name is grounded to a real domain through a fenced lookup. If it can't be grounded honestly, Clinch asks you instead of opening a search page.
3. **Act.** A managed browser — always off-screen, never a visible window — navigates and clicks with human-like input: hover, press, release, with a visible cursor so you can see where it is. Low-risk clicks flow; anything risky pauses for your tap.
4. **Verify.** This is what makes Clinch different: the AI never grades its own work. A deterministic verifier checks the live page — right site, right object, actually landed — and only it can declare success.
5. **Receipt.** You get a short, plain account of what happened, with the final view. Or an honest failure naming what was tried, with a Take Control button that hands you the browser exactly where it got stuck.

## Trust and safety

- **Never sends, spends, or leaves the site you named without a tap.** This is architecture, not a setting.
- **Bot checks and captchas:** Clinch tries silently first, then offers a consent-gated, one-time borrow of that site's login from your own daily browser, then hands you the controls. It never clicks a captcha itself.
- **Your logins stay yours.** Session lending is one-way (your browser → Clinch, never written back), per-site, and consent-gated. Clinch keeps its own copy, so signing out elsewhere doesn't sign Clinch out. "Forget this site" wipes Clinch's copy completely. Your daily browser is never touched.
- **Honest about being signed out.** If a page loads logged-out, Clinch says so and offers the one-tap sync — it never performs a "success" on a guest page.
- **Everything is journaled.** What it clicked, what it saw, what it decided. No silent actions.

## What works today

- Open any site by name: grounded to the real domain with no search page shown; an honest "which site?" when it can't tell.
- Account nouns on the live site — profile, account, settings, notifications, logout — opened through the site's own menu, verified against the live page, and remembered per site so repeats go straight there.
- Save any successful run as a named playbook and replay it; batch commands with per-click approvals.
- Site shortcuts: after a first successful visit, save `site → address` so the next visit resolves instantly with no model call.
- Challenge ladder: silent retry → consent-gated session lend → Take Control, with the session stood down cleanly afterward.
- Large Preview: a near-fullscreen view of what the run saw — live while running, frozen frame when settled.

## What it deliberately doesn't do (yet)

Per the constitution's build order: no scheduling yet (T3), no hosted web face yet (T2 — credential vault first), no sending messages or spending money without a tap (ever), no banking, government, or medical flows as early use cases, no job-application products, no price monitoring, no native-app automation, no team or household features yet (T6). If a requested feature doesn't make the product sentence truer, it's bloat.

## Measurement

Time-to-correct-page · % honest failures · % of Take Control handoffs completed · playbooks still working after a week. Not feature counts.

**Acceptance bar:** a stranger can open settings on a site they named and not see a search page congratulating them.

---

*Implementation detail lives in [ARCHITECTURE.md](ARCHITECTURE.md), [STATUS.md](STATUS.md), [FEATURES.md](FEATURES.md), and [TRD.md](TRD.md). Where this document and the code disagree about what is built, the code wins.*
