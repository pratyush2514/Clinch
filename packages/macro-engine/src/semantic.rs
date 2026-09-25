//! Semantic menu-item matching via the classifier.dev zero-shot API.
//!
//! [`SemanticMatcher`] answers the one question the action engine's
//! deterministic worker cannot answer with its closed vocabulary: *does
//! this revealed menu item's accessible name mean the same thing as the
//! verb's action?* A later integration step calls it in two places —
//! scoring revealed items when vocabulary matching is inconclusive, and
//! disambiguating the account/chrome opener among candidates.
//!
//! # Privacy
//! Only the candidate's accessible name (trimmed, truncated to
//! [`MAX_NAME_CHARS`] characters) and the two labels
//! (`"<verb_hint> action"`, `"unrelated control"`) leave the machine. No
//! URLs, no page text, no cookies, no site names, no user data. The
//! operator's own API key is not needed: classifier.dev serves
//! unauthenticated callers under a per-IP quota.
//!
//! # Failure posture
//! The classifier is a best-effort hint, never on the critical path. Any
//! failure — network, timeout, non-2xx status, rate limit, malformed or
//! unscored response — declines silently to [`None`]. The blocking HTTP
//! call runs inside [`tokio::task::spawn_blocking`] with a bounded
//! [`CLASSIFY_TIMEOUT`], so the classifier can never stall a run.
//!
//! # Discipline
//! [`SemanticMatcher::score`] handles exactly one candidate name per call
//! and caches every answer (including declines) for the run; repeat names
//! never re-hit the network. Ambiguity and volume discipline are the
//! caller's job — this module never sweeps.
//!
//! # API contract (verified 2026-09-25 against
//! <https://classifier.dev/openapi.json>)
//! `POST https://classifier.dev/v1/classify`, no key, no account. Request:
//! `{"input": "<text>", "labels": ["<verb> action", "unrelated control"],
//! "tier": "fast"}`. 200 response: `{"results": [{"label": "<winning
//! label>", "confidence": 0.0..=1.0, "scores": {"<label>": <score>, ...}}]}`
//! — one result per input, in order; `scores` sums to 1 for single-label
//! calls. (The service's GET forms answer plain text with no scores, which
//! is why this module uses the JSON POST.) Rate limits are per IP: 3,000
//! classifications/min and 20,000/day on the default fast tier. The
//! per-run cache plus one-candidate-per-call discipline keeps usage far
//! below quota; a 429 declines to [`None`] like any other failure.

use std::{collections::HashMap, time::Duration};

/// Score at or above which the caller treats a candidate as the verb's
/// action. The classifier's scores are calibrated probabilities (0..=1);
/// 0.75 keeps the hint conservative — vocabulary matching stays the
/// primary signal.
pub const SEMANTIC_ACCEPT: f32 = 0.75;

/// Upper bound on one classification call: the classifier is a hint and
/// must never stall a run.
const CLASSIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// Candidate names are truncated to this many characters before leaving
/// the machine; control names are short, and no run should ship
/// unbounded text to a third party.
const MAX_NAME_CHARS: usize = 120;

/// Production endpoint. Tests point the matcher at a loopback mock via
/// [`SemanticMatcher::with_base_url`] — or, for worker-level tests that
/// cannot inject a matcher, via the `CLINCH_CLASSIFIER_BASE_URL`
/// environment override read by [`SemanticMatcher::new`].
const CLASSIFY_BASE_URL: &str = "https://classifier.dev";

/// Environment override for the classifier base URL, read by
/// [`SemanticMatcher::new`]. Production never sets it; integration tests
/// point it at a loopback mock so the worker's semantic paths stay
/// hermetic. Empty values fall back to the production endpoint.
const CLASSIFY_BASE_URL_ENV: &str = "CLINCH_CLASSIFIER_BASE_URL";

/// The second label every call competes against — the closed, generic
/// negative that keeps the score honest.
const NEGATIVE_LABEL: &str = "unrelated control";

/// Bounded, fail-soft semantic matcher over one verb hint.
///
/// `verb_hint` is generic words like `"log out"` — never a site name. The
/// matcher is per-run: the cache is dropped with it, and nothing is
/// shared across runs.
pub struct SemanticMatcher {
    verb_hint: String,
    action_label: String,
    base_url: String,
    agent: ureq::Agent,
    /// Normalized candidate name -> last score, declines included. Repeat
    /// names never re-hit the network.
    cache: HashMap<String, Option<f32>>,
}

impl SemanticMatcher {
    /// Build for `verb_hint` against the production endpoint, or against
    /// the `CLINCH_CLASSIFIER_BASE_URL` override when it names a
    /// non-empty URL (integration tests use this to point the worker's
    /// semantic paths at a loopback mock).
    #[must_use]
    pub fn new(verb_hint: &str) -> Self {
        let base_url = std::env::var(CLASSIFY_BASE_URL_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| CLASSIFY_BASE_URL.to_owned());
        Self::with_base_url(verb_hint, &base_url)
    }

    /// Build for `verb_hint` against an explicit base URL. The test suite
    /// points this at a loopback mock; production code always uses
    /// [`SemanticMatcher::new`].
    #[must_use]
    pub fn with_base_url(verb_hint: &str, base_url: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(CLASSIFY_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        let verb_hint = verb_hint.trim().to_owned();
        let action_label = format!("{verb_hint} action");
        Self {
            verb_hint,
            action_label,
            base_url: base_url.trim_end_matches('/').to_owned(),
            agent,
            cache: HashMap::new(),
        }
    }

    /// Score one candidate name against the verb's action label.
    ///
    /// Returns the classifier's score for `"<verb_hint> action"`
    /// (0.0..=1.0), or [`None`] when the call is inconclusive or
    /// unavailable: blank input, blank verb hint, network failure, timeout,
    /// non-2xx status (including rate limits), or a response with no usable
    /// score. The blocking HTTP call runs on `spawn_blocking`, so the async
    /// runtime never waits on it; a cancelled blocking task declines
    /// silently like any other transport failure. Never panics.
    pub async fn score(&mut self, candidate_name: &str) -> Option<f32> {
        let key = normalize_name(candidate_name);
        if key.is_empty() || self.verb_hint.is_empty() {
            return None;
        }
        if let Some(cached) = self.cache.get(&key) {
            return *cached;
        }
        // Only the accessible name (truncated) and the two labels cross the
        // wire. Everything the blocking closure touches is owned, so the
        // future stays `'static`.
        let text: String = candidate_name.trim().chars().take(MAX_NAME_CHARS).collect();
        let agent = self.agent.clone();
        let url = format!("{}/v1/classify", self.base_url);
        let action_label = self.action_label.clone();
        let body = serde_json::json!({
            "input": text,
            "labels": [&action_label, NEGATIVE_LABEL],
            "tier": "fast",
        });
        let scored = tokio::task::spawn_blocking(move || classify_once(&agent, &url, body))
            .await
            .ok()
            .flatten()
            .and_then(|payload| extract_score(&payload, &action_label));
        self.cache.insert(key, scored);
        scored
    }
}

/// Cache key: trimmed, lowercased, whitespace-collapsed, so cosmetic
/// differences in the accessible name never cost a second call.
fn normalize_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// One bounded classification call, returning the raw response payload.
/// `None` on any transport or status failure — silently.
fn classify_once(
    agent: &ureq::Agent,
    url: &str,
    body: serde_json::Value,
) -> Option<serde_json::Value> {
    let response = agent.post(url).send_json(body).ok()?;
    if !matches!(response.status().as_u16(), 200..=299) {
        return None;
    }
    response.into_body().read_json().ok()
}

/// Read the action label's score out of one classify response: prefer the
/// per-label `scores` map, fall back to `confidence` when the winner is the
/// action label itself. Anything else is inconclusive — the caller, not
/// this module, decides the threshold.
fn extract_score(payload: &serde_json::Value, action_label: &str) -> Option<f32> {
    let result = payload.get("results")?.as_array()?.first()?;
    if let Some(score) = result
        .get("scores")
        .and_then(|scores| scores.get(action_label))
        .and_then(serde_json::Value::as_f64)
        .filter(|score| score.is_finite())
    {
        #[allow(clippy::cast_possible_truncation)]
        return Some(score as f32);
    }
    let winner = result.get("label")?.as_str()?;
    if winner == action_label {
        let confidence = result
            .get("confidence")?
            .as_f64()
            .filter(|confidence| confidence.is_finite())?;
        #[allow(clippy::cast_possible_truncation)]
        return Some(confidence as f32);
    }
    None
}
