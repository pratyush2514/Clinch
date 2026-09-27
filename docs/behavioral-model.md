# Personal behavioral model — act like *this* user, not *a* user

The generic human-like spec (`docs/human-like-operation.md`) makes the cloud browser act human in general. This doc specifies the next level: the Companion extension observes the real user's own input behavior, fits a compact behavioral model **locally**, and the cloud worker samples from it — so the agent doesn't just move like a human, it moves like *that* human.

This inverts behavioral biometrics (banks verify identity from typing/mouse patterns; we synthesize the verified pattern). The model is per-user and non-transferable: even if someone copies the technique, they can't copy your user's fitted model.

## 1. Privacy architecture (non-negotiable, read first)

- **Raw event streams never leave the user's machine.** Not for training, not for debugging, not ever.
- Fitting happens **inside the extension**. Only distribution parameters (means, variances, histogram bins — a few KB of non-reversible aggregates) are uploaded, and only with explicit opt-in consent.
- The user can view a human-readable summary of their model ("your typing speed, your mouse style") and delete it at any time. Deletion reverts the worker to the generic spec.
- Model stored encrypted per-user, versioned, included in Forget-Site semantics (deleting site data deletes the model slice if we ever go per-site).

This is the line that makes the feature a trust asset instead of a scandal. There is no "temporary" raw-upload phase.

## 2. Telemetry capture

A new content script (`telemetry.js`) injected into http/https pages. It observes **only** input event metadata — never key values, never field contents, never URLs beyond the registrable domain (for future per-site models):

| Stream | Events captured | Derived features |
|---|---|---|
| Mouse | `mousemove` (t, x, y), `mousedown`/`mouseup` (t, x, y, button) | Path curvature (Bezier control-point offsets), speed profile, easing exponent, overshoot frequency + magnitude, micro-jitter amplitude, hover-to-press delay, press dwell |
| Keyboard | `keydown`/`keyup` (t, code only — **never `key`**) | Inter-key intervals by bigram class (letter-letter, letter-space, etc.), key hold durations, word-boundary pauses, thinking-pause rate |
| Wheel | `wheel` (t, deltaY, deltaMode) | Ticks per gesture, inter-tick timing, deltaY decay curve, corrective-scroll rate |
| Focus/click | `click` target role (from `document.activeElement` tag only) | Inter-action gaps by preceding action type |

Sampling guards: cap at ~50 events/sec, drop streams during password fields entirely (`input[type=password]` → pause capture), never capture inside iframes from other origins. Estimated raw volume is small; the content script keeps a rolling buffer and aggregates continuously so raw events live only seconds in memory.

No new manifest permissions are needed: `<all_urls>` + `scripting` already granted. The content script is code-only.

## 3. Model format (`behavioral-model v1`, JSON, a few KB)

```jsonc
{
  "version": 1,
  "typing": {
    "inter_key_ms": { "by_bigram_class": { "ll": [mu, sigma], "lspace": [mu, sigma] /* log-normal */ } },
    "hold_ms": [mu, sigma],
    "word_pause_ms": [mu, sigma],
    "think_pause_prob": 0.05, "think_pause_ms": [mu, sigma]
  },
  "mouse": {
    "bezier_offset_frac": [mu, sigma],   // control-point offset as fraction of distance
    "easing_exponent": [mu, sigma],
    "speed_px_per_ms": [mu, sigma],
    "overshoot_prob": 0.15, "overshoot_px": [mu, sigma],
    "jitter_px": 1.5
  },
  "scroll": {
    "ticks_per_gesture": [mu, sigma],
    "inter_tick_ms": [mu, sigma],
    "decay": 0.7,
    "corrective_prob": 0.2
  },
  "click": { "hover_ms": [mu, sigma], "dwell_ms": [mu, sigma], "settle_ms": [mu, sigma] },
  "pacing": { "inter_action_ms": [mu, sigma] }
}
```

Distributions are log-normal where human timing is concerned (reaction times are never Gaussian). v1 is a single global model; per-context (field type, device class) is a later version — the schema versions cleanly.

## 4. Fitting (online, in-extension)

- Maintain running Welford mean/variance per parameter over a rolling window (last ~30 days or ~50k keystrokes, whichever is smaller). Exponential decay (half-life ~14 days) handles drift — new mouse, tired evenings, trackpad-vs-mouse switches.
- **Activation thresholds** (model stays dormant until): typing ≥ 2,000 keystrokes; mouse ≥ 500 moves; scroll ≥ 200 gestures. Below threshold the worker uses the generic spec — the cold-start fallback is automatic per modality, so a user can have personalized typing with generic mouse.
- Sanity clamps: if fitted params fall outside human-plausible bounds (broken sensor, bot-like paste bursts), discard the window and keep the previous model. Paste events (`ctrl+v`, bulk inserts) are excluded from typing stats entirely.

## 5. Worker sampling

- The cloud worker fetches the user's model at session start (with the session). Every jitter draw in `docs/human-like-operation.md` §3 samples the user's fitted distribution instead of the generic range.
- Same determinism rule: per-run seeded RNG, seed journaled; tests inject a fixed seed. Positions exact, timing sampled.
- If the model is absent, stale (>90 days), or deleted → generic spec. The worker never blocks on the model.

## 6. Validation plan (before production)

1. **Stability (local-only phase).** Extension records and fits locally; nothing uploaded. Question: does a user's fitted distribution stay stable week-over-week? Measure KL divergence between weekly fits. Ship criterion: stable for 3+ weeks across ≥80% of pilot users.
2. **Distinguishability (offline eval).** Train a classifier on real-user streams; test it against (a) generic-spec bot, (b) model-sampled bot, (c) held-out real user. Ship criterion: (b) is classified "human" significantly more often than (a), approaching (c).
3. **Production (flagged).** Roll out behind a per-user flag; monitor block/challenge rates vs. generic-spec cohort. The honest metric is challenge rate, not vibes.

## 7. Honest limits

- Doesn't fix IP reputation. Behavioral layer only — complements the session/IP strategy, doesn't replace it.
- Doesn't help new users (cold start) — the generic spec remains the default and must stay good.
- Adversarial ML keeps improving on the detection side too. This buys distance, not immunity.
- Per-context and per-device models are v2; v1's global model is deliberately crude.

## 8. Sequencing

1. Ship generic human-like spec first (it's the fallback and cold-start default regardless).
2. Extension telemetry, local-only, stability validation (§6.1).
3. Local fitting + parameters upload behind consent.
4. Worker sampling behind flag, challenge-rate A/B (§6.3).

## 9. Reading order for implementers

This doc → `docs/human-like-operation.md` (the sampling target) → `packages/extension-bridge/background.js` (where the model will live) → `packages/extension-bridge/manifest.json` (permissions — no changes needed).
