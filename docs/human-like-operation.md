# Human-like operation — staying unblocked

How Clinch avoids bot detection, and the spec for making its browser input indistinguishable from a human's. All approaches here are legal: we win by *being* the user, not by deceiving anyone. What we never do is listed at the bottom as red lines.

## 1. Why we're hard to block (structural)

Cloud agents (Muse Secure VM, Grok Bot workers) get blocked because they present two machine-detectable signals: **datacenter IPs** and **ephemeral fresh browsers**. Our architecture avoids both by default, and they cannot copy us without abandoning the cloud model:

- **Residential IP.** The engine runs on the user's machine (or personal VPS) — the site sees a home connection, not a cloud VM range. Strongest setup: daemon on the home machine; personal VPS is a half-step down; both beat shared cloud pools by a mile.
- **One persistent profile.** A single app-owned Chromium profile accumulates cookies, history, and localStorage over time. We never spin up a fresh instance per run. Returning-user signals beat first-visit scrutiny every time.
- **Off-screen headed, never headless.** Headless Chrome leaks detectable signals (`navigator.webdriver`, missing plugins, odd rendering). Headed Chromium over CDP is a real browser.
- **Trusted CDP input.** `Input.dispatchMouseEvent` / `dispatchKeyEvent` produce browser-trusted events, not synthetic JS events. Sites that listen for `isTrusted` see real input.
- **Session lending as the default login path.** Fresh login is the highest-risk moment for bot detection. Borrowing the already-authenticated session from the daily browser skips it entirely — the site already trusts that session.

## 2. Threat model

| Layer | What sites check | Our posture |
|---|---|---|
| IP reputation | Datacenter vs residential ranges | Won by architecture (§1) |
| TLS / HTTP fingerprint | JA3, header order | Real Chromium — nothing to do |
| Browser fingerprint | `webdriver` flag, plugins, canvas | Headed + persistent profile; **never randomize** — consistency is the signal, randomization looks guiltier |
| Input authenticity | `isTrusted`, event sequences | Trusted CDP events; full key/mouse event streams (§3) |
| Behavioral analysis | Timing regularity, inhuman speed, perfect geometry | The spec below — this is the remaining work |
| Rate / volume | Actions per minute, session velocity | Human-scale pacing (§3.5); playbooks are chores, not scrapers |

The infrastructure war is won by default. The behavioral war is winnable polish. No layer requires anything deceptive.

## 3. Human-like input spec

Golden rule: **humans are inconsistent.** Any constant interval, any perfectly straight line, any uniform speed is a bot signature. Every parameter below is a range, never a fixed value.

### 3.1 Cursor movement

Current state: `cursor.rs::travel_waypoints` glides in 40px steps, max 12, at a constant 18ms interval. The geometry is fine; the **constant timing must go**.

- Path: cubic Bezier from current position to target. Offset both control points perpendicular to the travel direction by 10–30% of the distance (random sign). Long moves get a slightly S-shaped path; short moves stay near-straight.
- Easing: ease-out with deceleration into the target — humans move fast then home in. Sample the curve at variable time steps, not variable distances: waypoints dense near the target, sparse at the start.
- Timing: total travel time scales with distance (~1ms per 2px is inhumanly fast; aim ~0.5–1.0 ms/px with ±30% jitter). Never a fixed per-waypoint interval.
- Overshoot: on ~15% of moves, overshoot the target by 3–8px and correct back with 1–2 small moves. Humans do this constantly; bots never do.
- Micro-jitter: add ±1.5px noise to intermediate waypoints (never to the final target — clicks must land).

### 3.2 Clicks

Current state: hover → press → release via trusted CDP events with a fixed 100ms press dwell. Keep the sequence; unfix the timing.

- Hover pause before press: 80–200ms (humans hesitate; the pause also lets hover states render, which some menus need).
- Press dwell: 70–130ms instead of fixed 100ms.
- After release: 100–300ms settle before the next action (lets the page react; also reads as human patience).

### 3.3 Typing

Current state: `actions.rs` uses `Input.insertText` — the entire string appears at once with **zero key events**. This is the single biggest bot signal in the codebase. Fix priority: highest.

- Replace bulk insert with per-character `Input.dispatchKeyEvent` (`keyDown` → `keyUp`) for visible form fields. Bulk insert may stay only for hidden/programmatic fills where no human would type.
- Cadence (per-key delay, ms): base 90–160ms within words; 180–450ms at word boundaries (space); occasional 600–1200ms "thinking" pause mid-field (~5% of gaps). Real typing is bursty — fast runs inside common bigrams, pauses between thoughts. Uniform 100ms is a metronome; metronomes are bots.
- Key hold: keyDown→keyUp 30–80ms per key (humans don't tap instantaneously).
- Shift/caps for uppercase: dispatch the real modifier sequence, don't just send the uppercase char.
- Special keys (Tab, Enter, Backspace): slightly longer pre-delay (150–300ms) — humans aim for them.

### 3.4 Scroll

- Wheel events via `Input.dispatchMouseWheel`, not JS `scrollTo` (untrusted).
- Variable velocity: start fast, ease out; 2–5 wheel ticks per gesture with 40–120ms between ticks and decaying deltaY.
- Reading pauses: after scrolling to content, pause 800–2000ms before acting — humans read; bots act instantly.
- Small corrective scrolls: humans overshoot and scroll back slightly (~20% of scrolls).

### 3.5 Pacing between actions

- Discrete actions (click → type → click): 400–1200ms gaps. Never chain at machine speed.
- Before "thinking" steps (re-grounding, choosing a candidate): 600–1500ms. The model is deciding; the pause is honest and human.
- Playbook replays: keep the recorded *sequence*, not the recorded *speed*. Humanize timing on every replay.

### 3.6 Determinism note

Timing jitter must not break tests. Use a small per-run seeded RNG for all jitter; journal the seed with the run. Tests inject a fixed seed (or zero jitter) via the fake CDP server. Positions stay exact; only timing varies.

## 4. Session & identity hygiene

- **Consistency over cleverness.** User-agent, viewport (1920x1080), timezone, locale: set once from the user's real environment, never change. A user whose fingerprint changes every run is more suspicious than any automation.
- **Profile warm-up.** A brand-new profile is the most scrutinized. The managed profile should accumulate ordinary state; avoid factory-resetting it.
- **Login via lending, not credentials.** Prefer the session-lending ladder over fresh logins everywhere. Fewer logins = fewer high-risk moments.
- **Cookies:** never clear except the LogOut fallback and explicit Forget Site. Clearing cookies destroys the trust the profile earned.

## 5. Red lines (never)

- No CAPTCHA-solving services (2captcha-style APIs). Interactive challenges go to Take Control — the ladder already handles this.
- No stealth-fingerprint-spoofing plugins. They start an arms race we lose, and getting caught cheating would be fatal for a trust product.
- No residential proxy networks. Legally murky, expensive, and contradicts the privacy story.
- No fingerprint randomization. See §4: consistency is the defense.
- No credential stuffing patterns: never retry logins rapidly, never rotate accounts.

## 6. Implementation status (2026-09-27)

Built: trusted CDP click sequence (hover→press→release + dwell), waypoint glide geometry, `click_hit_test` journaling, persistent off-screen-headed profile, session lending ladder, Take Control fallback.

Next (in priority order): per-key typing with cadence (§3.3) → variable-timing Bezier cursor (§3.1) → click timing jitter (§3.2) → wheel scrolling with easing (§3.4) → inter-action pacing (§3.5) → seeded-RNG determinism (§3.6).

## 7. Reading order for implementers

This doc → `packages/browser-driver/src/cursor.rs` (glide geometry) → `packages/browser-driver/src/actions.rs` (`InsertText` call site) → `docs/TRD.md` (challenge ladder, Take Control contract).
