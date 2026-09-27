# Clinch Constitution

Adopted 2026-09-27. This is the product authority: any doc, plan, or feature
that contradicts it is wrong. Code remains the authority on *what is built*;
this document is the authority on *what we are building toward*.
Amendments to the original draft are logged at the bottom.

## 1. Product sentence

**Clinch turns a short intent into a finished action on a real website or a
saved playbook — and it never sends, spends, or leaves the site you named
without a tap.**

Not a companion, not a life OS, not a second Muse, not a local ChatGPT with
Chrome glued on. If a feature does not make that sentence truer, it is bloat.

## 2. Why this can exist after Muse / Grok / open agents

Those products optimize for conversation + tools + a general loop. They are
good when the user is willing to talk, wait, and forgive. Still empty:

- "Do this portal again next Friday" — recurring admin nobody owns.
- "Show me the tab; I tap the last thing" — visible handoff for non-tech,
  aging, or blocked users.
- "Both of us must say yes" — households, not single users.
- "Delete means delete; this runs on my box" — privacy-sensitive and SMB.
- Honest `FAILED` instead of a search page called success.

We do not beat general chatbots at general browsing. We beat silence, lies,
and one-person-only design on recurring admin.

## 3. The only intents that ship in v1

Five intents. Everything else is a playbook built from these, or a question.

```text
OPEN     <site>
GO       <profile | settings | notifications | logout> on <site>
DO       <named playbook>
WATCH    <named playbook>          // schedule
ASK_ME   when stuck / to send / to spend
```

Natural language is allowed; the system still reduces to those five.
"I want you to open X and I'll log in first" = `OPEN x` + aside
`user_will_login`. Not a new verb.

v1.5 (only after the five are dull): `DRAFT` (email/message, never send)
and `FILL` (form, stop before submit).

Not v1: grocery-from-Reels, phone calls, virtual cards, Instagram
connectors, avatars, goals graphs, job-application products.

## 4. How value feels

A good Clinch run is short. The user never chooses a mode — they state an
intent; Clinch chooses open vs replay vs ask. One thread; saved playbooks
are rows under the thread, not a second app. Command Bar vs Workflow Form
vs Task Workspace as separate products is dead.

## 5. Faces (one engine)

| Face | Who | What they open |
|---|---|---|
| Web (flagship consumer) | Non-tech, aging, "I won't install" | Browser tab: prompt + live view + Approve + Take Control |
| Phone wrapper | Same people, away from desk | Same session; approvals and "needs you" first |
| Desktop (pro / privacy) | Tech, SMBs | Local daemon + optional extension lend |

Chat (WhatsApp etc.) is a pipe into the same API after web works — not the
architecture. Household later: a second seat sees tasks and approvals, never
the other adult's session cookies.

## 6. Deployment: hybrid

```text
Web / phone → control plane (cloud: users, permissions, schedules, Disconnect)
                ├─ hosted worker (Linux container: Chromium + daemon)
                └─ local worker (their PC or their VPS; same binary)
```

- **Hosted worker** is the consumer default. The user signs in *there* —
  which requires the credential vault first (see §9): the model never sees
  the secret, vault fills while the model is paused. Disconnect wipes the
  profile. No vault, no hosted sign-in.
- **Local worker** is the pro/privacy default: same binary on their machine
  or a $10 VPS Compose file. Extension session-lend stays local-only;
  cloud lend is a separate, consented upload — never the default.
- **Personal cloud** (user-operated daemon + phone client) is the bridge
  between the two: tasks run while laptops are closed without us becoming
  a hosting company first.
- **Never in the model:** cookies, passwords, raw DOM, card numbers.
- **Never only-local** if we want parents (their laptop will be closed).
  **Never only-cloud** if we want the trust edge (self-host is the SKU the
  giants will not price at $6).
- First hosted browsers: rent (Browserbase-class) or our Docker on one box.
  Do not build a fleet. Schedules that must run while the laptop sleeps
  need *a* worker that is up — theirs or ours.

## 7. Engineering rules

One pipeline, no third navigator:

```text
text → normalize (strip asides)
     → slots { site, object, playbook, aside } — deterministic first;
        model only if site missing; model never emits URLs
     → policy — guest / signed-in / challenge; Look | Draft | Tap yes |
        Recurring-this-playbook
     → hand — memory/playbook hit? replay : browser worker
        (chrome-first for account objects) : skill if user connected
        that origin+object
     → observe — AX + frame; elementFromPoint after clicks
     → verify — landed host = site slot; search engine ≠ success;
        object page matches goal
     → memory — write only after verify
     → narrate — 3–8 human lines
```

Closed actions: `navigate_origin`, `click(mark)`, `open_href(mark)`,
`type(mark)`, `wait`, `stop`, `ask_human`. Settled rules (already true in
this codebase, non-negotiable): the verifier alone decides `COMPLETED`;
a click never succeeds by itself; cookie count never proves signed-in —
captions follow the frame; memory writes only after verification.

**Chrome primitive is shared.** Profile, settings, notifications, logout all
open the same identity menu, then pick the noun. Converge per-noun
machinery (e.g. logout-specific retry/filter) into this one primitive.

**Cursor overlay stays.** It is a truthful overlay of real CDP-dispatched
input, not a fake — and visible human-like navigation is the product bar.

## 8. Session and identity

- Consumers: Connect-site = sign in inside the worker's Chrome once
  (vault-mediated on hosted). Caption follows the frame.
- Logout: click Log Out in the menu, or journal `logout: cookies` if we
  wipe. Never pretend those are the same.
- Challenge: interactive gates always go to Take Control. Never CDP-click
  a captcha.
- Passwords never in the prompt.

## 9. Trust layer (this instead of more verbs)

Four levels, shown in words: 1. Look only · 2. Draft only · 3. Do it after
I tap yes · 4. Do **this named playbook** on a schedule. Every risky card
shows site, account hint (never the secret), exact text, amount if any.
Global kill: Pause all. Readable activity log; status reads
`step 3 of 6 — waiting on menu`. Blocked site is a proud path: "form is
filled; you submit." Instructions inside a page or email can never raise
the permission level. Household: shared task, separate sessions;
dual-approve is a control-plane flag.

## 10. Build order

- **T0 — Constitution.** This file. Verbs, COMPLETED rules, object words
  never hit the site ladder.
- **T1 — Operator is dull.** Hit-test after clicks, 2-try menu, caption =
  frame, settle never green on Google. Still local or Docker on our box.
- **T2 — One hosted face.** Vault first (§6, §9), then daemon in Docker;
  web: login, prompt, live frame, Take Control, Disconnect. Invite 10.
  No WhatsApp.
- **T3 — Recurring value.** Save playbook from a verified run; run it on
  a clock; library of files; "stuck at step 3."
- **T4 — Trust product.** Permission levels in UI; kill; readable log;
  last-click handoff card.
- **T5 — Audience.** One job: renewals/bills **or** school forms **or**
  vendor invoices. Not all.
- **T6 — Household + self-host SKU.** Second approver; Compose file.

Do not start T5 before T1 is green.

## 11. What we keep from current Clinch

Rust worker next to Chrome · AX snapshots, never raw DOM to the model ·
Rust URL validation · Take Control / pause · identity memory keyed by
`origin + goal_class` · Forget/Disconnect deletes rows + profile ·
playbooks as verified recipes · challenge ladder (L1 → L1.5 → L2) ·
consent-gated session lending · our streaming protocol (noVNC is a later
scale choice, not a rewrite).

Frozen: three front doors; directory scrape that can navigate off-slot;
per-site hostname tables; Windows as the only browser host for production.

## 12. How we measure

Time-to-correct-page · % honest fails · % Take Control finishes ·
playbooks that survive a week. Not feature counts.

**Acceptance bar:** a stranger can open settings on a site they named and
not see Google congratulating them.

## 13. Competitive stance

Muse / Grok-class: "I'll handle it in chat." Open harnesses: "Here's a
loop and tools; you glue it." **Clinch:** "I'll open the right page,
remember it, do it again on Tuesday, and stop for you." We will not win a
model bake-off. We win if a user says: *it did the admin that keeps coming
back, and it never sent that email without me.*

---

## Amendment log (vs the original draft, 2026-09-27 review)

1. **Vault before hosted sign-in.** The draft had users signing into our
   cloud Chrome with the vault mentioned but unscheduled. The vault
   (model-paused fill) is now an explicit prerequisite of T2 — without
   it, hosted sign-in is a trust hole, not a feature.
2. **Personal-cloud bridge kept.** The draft jumped local → hosted. The
   user-operated daemon + phone client stays as the middle phase: it is
   how tasks run with closed laptops without us becoming a hosting
   company before demand exists.
3. **Our protocol stays.** The draft suggested noVNC / a hosted-browser
   vendor "instead of writing a protocol". The protocol is written,
   tested, and working — noVNC is a later scale option, not a rewrite.
4. **Cursor overlay stays.** The draft listed it under "throw away". It is
   a truthful overlay of real input and an explicit user product-bar
   requirement — not a fake.
5. **"Already true" items recorded as settled**, not as work: verifier
   honesty, AX-not-DOM, Rust URL validation, caption-from-frame,
   Take Control-first on interactive challenges, Linux worker OS,
   daemon split, playbooks-as-verified-recipes.
