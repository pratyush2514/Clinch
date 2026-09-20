#![deny(unsafe_code)]
//! Semantic intent execution over live AX snapshots with Set-of-Marks clicks.
//!
//! Playbooks describe *what* they want (`{role, label}`) instead of *where it
//! is* (selectors). At runtime every role-matching candidate is scored by a
//! weighted soft model — label similarity plus container-token overlap — and
//! the winner is acted on through its backend node id (rect resolution plus
//! coordinate click). No selector is ever constructed, stored, or repaired
//! here, and no candidate is ever vetoed for partial container text: weak
//! evidence lowers a score, never disqualifies.

use browser_driver::{AxElement, Highlight, ManagedBrowser, Mark};
use serde::{Deserialize, Serialize};

/// What a Playbook step wants, in words. Role and label are required: a bare
/// role with several live matches is ambiguous and resolves to nothing. The
/// optional container query boosts candidates whose surroundings mention the
/// target (`0LWQXDWW`); it is a soft signal, never a veto — partial overlap
/// still outscores no overlap, and only a below-threshold total fails with
/// `NoMatch`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticIntent {
    pub role: String,
    pub label_query: String,
    /// Added after `role`/`label_query`: `#[serde(default)]` keeps saved
    /// playbooks and older UI payloads parsing with no container scope.
    #[serde(default)]
    pub container_query: Option<String>,
    /// The raw conversational prompt, passed through verbatim (truncated to
    /// [`MAX_RAW_PROMPT_LEN`]) so the live DOM labels act as the dictionary:
    /// sub-token coverage needs the user's own words, not the stripped
    /// `label_query`. `#[serde(default)]` keeps older payloads parsing with
    /// an empty prompt, which simply contributes no coverage signal.
    #[serde(default)]
    pub raw_prompt: String,
    /// Zero-based position among eligible candidates (`second`/`2nd` → 1).
    /// Selects in snapshot document order — the deterministic proxy for
    /// visual top-to-bottom until geometry plumbing exists. Out of range
    /// fails closed. `#[serde(default)]` keeps older payloads scopeless.
    #[serde(default)]
    pub ordinal_index: Option<usize>,
    /// Bare `last` (without time words like `week`) selects the final
    /// eligible candidate. Explicit `ordinal_index` wins if both are set.
    #[serde(default)]
    pub is_last: bool,
}

/// Upper bound on the pass-through prompt: long prose must ground, never
/// fail validation on length.
const MAX_RAW_PROMPT_LEN: usize = 2000;

impl SemanticIntent {
    /// Shared bounds so schema validation and execution agree on what a
    /// runnable intent looks like. `pub` for the playbook schema only.
    ///
    /// # Errors
    /// Returns [`browser_driver::BrowserError::InvalidAction`] for empty or
    /// oversized roles, queries, and container scopes, or an oversized raw
    /// prompt (empty raw prompts stay valid: older payloads carry none).
    pub fn validate(&self) -> Result<(String, String), browser_driver::BrowserError> {
        let role = normalize(&self.role);
        let query = normalize(&self.label_query);
        if role.is_empty() || query.is_empty() || role.len() > 64 || query.len() > 512 {
            return Err(browser_driver::BrowserError::InvalidAction);
        }
        if let Some(container) = self.container_query.as_ref() {
            let scoped = normalize(container);
            if scoped.is_empty() || scoped.len() > 512 {
                return Err(browser_driver::BrowserError::InvalidAction);
            }
        }
        if self.raw_prompt.len() > MAX_RAW_PROMPT_LEN {
            return Err(browser_driver::BrowserError::InvalidAction);
        }
        Ok((role, query))
    }

    /// Normalized container scope, if any. Empty-after-trim and oversized
    /// values are rejected by [`Self::validate`]; this only normalizes.
    fn container(&self) -> Option<String> {
        self.container_query
            .as_ref()
            .map(|container| normalize(container))
            .filter(|container| !container.is_empty() && container.len() <= 512)
    }
}

/// A resolved intent: the winning element plus its match strength
/// (3 exact, 2 prefix, 1 containment) for planners and previews.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedIntent {
    pub element: AxElement,
    pub score: u8,
}

/// Clickable admission pool: intent roles that act through pointer controls
/// (`button`, `link`, `menuitem`) additionally admit `tab` elements, since
/// intents never infer a `tab` role yet tabbed navigation is a click action.
/// One-directional only — `button` still never matches a `link` intent, and
/// `textbox` stays unreachable from every click intent (input protection).
fn role_admits(intent_role: &str, element_role: &str) -> bool {
    element_role == intent_role
        || (element_role == "tab" && matches!(intent_role, "button" | "link" | "menuitem"))
}

/// What an executed intent acted on: the badge shown and the rect clicked.
#[derive(Clone, Debug, PartialEq)]
pub struct IntentOutcome {
    pub mark: Mark,
    pub highlight: Highlight,
}

#[derive(Debug, thiserror::Error)]
pub enum IntentError {
    /// Grounding failure carrying the full step-log diagnostic: target
    /// queries plus every evaluated candidate and its score.
    #[error("{0}")]
    NoMatch(String),
    #[error("CDP execution failed")]
    Browser(#[from] browser_driver::BrowserError),
}

/// Lowercase, whitespace-collapsed comparison form. Case-insensitive via
/// `to_lowercase` and whitespace-normalized via `split_whitespace`, so
/// `0LWQXDWW` matches `0lwqxdww` and `  June   12 ` matches `June 12`.
/// No language-specific patterns.
fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Tokens that never identify a row on their own: ordinals and positional
/// words (`second`), relative-time words (`last week`), generic list words,
/// action verbs, month names handled alongside, and cents-only `00` groups.
/// Filtering them keeps free-form container scopes (`"second invoice"`,
/// `"receipt for last week"`) matching instead of vetoing on words the DOM
/// never contains.
const CONTAINER_NOISE: &[&str] = &[
    "first",
    "second",
    "third",
    "fourth",
    "fifth",
    "sixth",
    "seventh",
    "eighth",
    "ninth",
    "tenth",
    "1st",
    "2nd",
    "3rd",
    "4th",
    "5th",
    "6th",
    "7th",
    "8th",
    "9th",
    "10th",
    "next",
    "last",
    "previous",
    "prior",
    "list",
    "item",
    "row",
    "week",
    "month",
    "year",
    "day",
    "today",
    "yesterday",
    "tomorrow",
    "ago",
    "from",
    "into",
    "onto",
    "over",
    "under",
    "between",
    "among",
    "across",
    "download",
    "downloads",
    "downloading",
    "get",
    "fetch",
    "open",
    "show",
    "click",
    "fill",
    "type",
    "enter",
    "press",
    "submit",
    "tap",
    "select",
    "choose",
    "my",
    "the",
    "a",
    "an",
    "please",
    "now",
    "latest",
    "new",
    "here",
    "this",
    "that",
    "me",
    "for",
    "to",
    "on",
    "and",
    "or",
    "of",
    "in",
    "is",
    "it",
    "id",
    "with",
    "named",
    "called",
];

fn is_noise_token(token: &str) -> bool {
    if token.is_empty() {
        return true;
    }
    if CONTAINER_NOISE.contains(&token) {
        return true;
    }
    // Cents-only groups (`00` from `$4.00`) never identify alone: they let
    // `$4` match `$4.00` and vice versa via the surviving `4` token.
    if !token.is_empty() && token.chars().all(|c| c == '0') {
        return true;
    }
    false
}

/// Singular variant for plural-tolerant matching (`invoices` matches
/// `invoice`). Only strips one trailing `s` from longer tokens, never `ss`.
fn singular_variant(token: &str) -> Option<String> {
    if token.len() > 3 && token.ends_with('s') && !token.ends_with("ss") {
        Some(token[..token.len() - 1].to_owned())
    } else {
        None
    }
}

/// Sub-token set coverage of `label` inside `prompt`: both sides split into
/// lowercase alphanumeric tokens (no dictionaries, no stopwords, no length
/// checks — pure set coverage, immune to prompt-length disparities), then
/// the fraction of label tokens present in the prompt set. `0.0` when the
/// label carries no tokens. Lets a verbose prompt
/// (`"can you please … click on the submit application button for me"`)
/// ground a short control (`"Submit Application"`) with no verb stripping:
/// the live DOM labels act as the dictionary.
#[must_use]
pub fn calculate_subtoken_coverage(prompt: &str, label: &str) -> f64 {
    fn tokens(text: &str) -> Vec<String> {
        text.to_lowercase()
            .split(|character: char| !character.is_alphanumeric())
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect()
    }
    let label_tokens = tokens(label);
    if label_tokens.is_empty() {
        return 0.0;
    }
    let prompt_tokens = tokens(prompt);
    let matched = label_tokens
        .iter()
        .filter(|token| prompt_tokens.contains(token))
        .count();
    // Token counts here are UI-string lengths, far below float precision limits.
    #[allow(clippy::cast_precision_loss)]
    let coverage = matched as f64 / label_tokens.len() as f64;
    coverage
}

/// Significant alphanumeric tokens of a normalized scope, noise-filtered.
/// Single-character tokens stay: amounts like `4` (`$4`) are significant.
fn significant_tokens(normalized: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for part in normalized.split(|c: char| !c.is_alphanumeric()) {
        if part.is_empty() || is_noise_token(part) {
            continue;
        }
        if !tokens.contains(&part.to_owned()) {
            tokens.push(part.to_owned());
        }
    }
    tokens
}

/// Score weights for the soft grounding model. The label carries the action
/// (`Download`); the container carries the target (`0LWQXDWW`). Container
/// evidence weighs 10x so that a control inside a matching container (11.0)
/// comfortably beats scopeless or unmatched candidates (1.0) instead of
/// falling back to document-order tie-breaks — without vetoing anyone.
const LABEL_WEIGHT: f64 = 1.0;
const CONTAINER_WEIGHT: f64 = 10.0;
/// Minimum total score for execution. Containment-strength labels (1.0)
/// pass; label misses with no container backing (0.0) fail with a rich
/// `NoMatch` diagnostic instead of a wrong click.
const MIN_EXECUTION_SCORE: f64 = 1.0;
/// Cap on candidates rendered into one failure diagnostic so step logs stay
/// readable on crowded pages.
const MAX_DIAGNOSTIC_CANDIDATES: usize = 12;
/// Per-candidate text budget inside one failure diagnostic.
const MAX_DIAGNOSTIC_TEXT_LEN: usize = 120;

/// Normalized token-overlap fraction (0.0–1.0) between a container scope and
/// one element's surroundings, case-insensitive and plural-tolerant. Exact
/// normalized substrings score 1.0 (covers `0lwqxdww` vs `0LWQXDWW`);
/// otherwise the fraction of significant scope tokens present anywhere in
/// the joined container text, so `$4 declined June 12` scores fully across
/// items like `$4.00` / `Declined` / `June 12` in any order or case, and
/// partially when the tree missed a cell. Month names stay significant (so
/// `June 12` needs both parts); ordinals, relative-time, and action verbs
/// are noise. Empty and noise-only scopes score 0.0.
fn container_overlap(scope: &str, element: &AxElement) -> f64 {
    let target = scope.trim().to_lowercase();
    if target.is_empty() {
        return 0.0;
    }
    let joined = joined_container(element);
    if joined.is_empty() {
        return 0.0;
    }
    if joined.contains(&target) {
        return 1.0;
    }
    let tokens = significant_tokens(&target);
    if tokens.is_empty() {
        return 0.0;
    }
    let hits = tokens
        .iter()
        .filter(|token| {
            joined.contains(*token)
                || singular_variant(token).is_some_and(|singular| joined.contains(&singular))
        })
        .count();
    // Counts here are container lengths, far below float precision limits.
    #[allow(clippy::cast_precision_loss)]
    let fraction = hits as f64 / tokens.len() as f64;
    fraction
}

/// One element's surroundings as a single normalized comparison string.
fn joined_container(element: &AxElement) -> String {
    element
        .container_text
        .iter()
        .map(|context| normalize(context))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Weight of the prose-coverage signal: a control whose whole label is
/// covered by the user's words grounds even when the stripped `label_query`
/// missed it (long prose, paraphrase). Large enough to clear the threshold
/// on its own, so coverage rescues rather than merely reranks.
const COVERAGE_WEIGHT: f64 = 100.0;

/// Weighted total for one candidate:
/// `label * 1.0 + overlap * 10.0 + coverage(raw_prompt, name) * 100.0`.
/// Returns the label strength (kept as the planner-facing `score`) and the
/// float total used for ranking. An empty raw prompt contributes no coverage,
/// which keeps older payloads (stored before the field existed) scoring
/// exactly as before.
fn total_score(
    query: &str,
    scope: Option<&str>,
    raw_prompt: &str,
    element: &AxElement,
) -> (u8, f64) {
    let label = label_score(query, element);
    let overlap = scope.map_or(0.0, |scope| container_overlap(scope, element));
    let coverage = calculate_subtoken_coverage(raw_prompt, &element.name);
    let total =
        f64::from(label) * LABEL_WEIGHT + overlap * CONTAINER_WEIGHT + coverage * COVERAGE_WEIGHT;
    (label, total)
}

fn label_score(query: &str, element: &AxElement) -> u8 {
    let name = normalize(&element.name);
    if name == query {
        return 3;
    }
    if name.starts_with(query) {
        return 2;
    }
    if name.contains(query) || normalize(&element.description).contains(query) {
        return 1;
    }
    // Container evidence alone still grounds: duplicate bare controls
    // ("Download", "Download") resolve through their surroundings.
    if element
        .container_text
        .iter()
        .any(|context| normalize(context).contains(query))
    {
        return 1;
    }
    0
}

/// Resolve `intent` against a live snapshot with weighted soft scoring.
///
/// Priority tiers, enforced structurally rather than by multipliers:
///
/// * **Tier 1 — interactive-control admission.** Only elements whose role
///   equals the intent role are scored at all, so click/navigation intents
///   (`button`, `link`, `menuitem`) can only ever act on clickable
///   controls — plus `tab` elements, admitted one-way into every clickable
///   pool since intents never infer `tab` yet tabbed navigation is a click
///   action (see [`role_admits`]). There is no cross-role multiplier
///   because cross-role candidates never enter the race.
/// * **Tier 2 — container disambiguation.** Overlap boosts totals only
///   through the `+ overlap * 10.0` term: it separates identically labeled
///   controls and lifts scoped matches, never vetoes.
/// * **Tier 3 — input protection.** `textbox` controls are unreachable from
///   click/navigation intents by the Tier 1 gate — even with a perfect
///   label and full container overlap — so search bars and inputs can
///   never steal clicks.
///
/// Role admission follows [`role_admits`] (exact match, plus one-way `tab`
/// admission into clickable pools); every other candidate stays alive and
/// is ranked by `label * 1.0 + container_overlap * 10.0 + coverage * 100.0`.
/// The overlap scope is the
/// container query when one is stated, else the label itself — so container
/// evidence breaks ties even for scopeless intents. The highest total at or
/// above `MIN_EXECUTION_SCORE` wins; exact-total ties keep document order.
/// An explicit ordinal (`ordinal_index`/`is_last`) instead selects the nth
/// eligible candidate in snapshot document order — the deterministic proxy
/// for visual top-to-bottom — and fails closed when out of range.
/// Partial container evidence only lowers a total — nothing is vetoed — and
/// a field of weak candidates resolves to nothing instead of a wrong
/// control.
#[must_use]
pub fn resolve_intent(elements: &[AxElement], intent: &SemanticIntent) -> Option<ResolvedIntent> {
    let (role, query) = intent.validate().ok()?;
    let container = intent.container();
    // Overlap focus follows the scope when one is stated, else the label —
    // identical focus to the previous tie-break for scopeless intents, so
    // container evidence still breaks ties between duplicate bare controls.
    let focus = container.as_deref().unwrap_or(&query);
    // Eligible set in document order: admitted roles at or above threshold.
    // Collected up front so ordinal selection and best-total share one
    // definition of "candidate".
    let mut eligible: Vec<(ResolvedIntent, f64)> = Vec::new();
    for element in elements {
        if !role_admits(&role, &element.role) {
            continue;
        }
        let (label, total) = total_score(&query, Some(focus), &intent.raw_prompt, element);
        if total < MIN_EXECUTION_SCORE {
            continue;
        }
        eligible.push((
            ResolvedIntent {
                element: element.clone(),
                score: label,
            },
            total,
        ));
    }
    if intent.is_last {
        return eligible.pop().map(|(resolved, _)| resolved);
    }
    if let Some(index) = intent.ordinal_index {
        return eligible.get(index).map(|(resolved, _)| resolved.clone());
    }
    let mut best: Option<(ResolvedIntent, f64)> = None;
    for (resolved, total) in eligible {
        let stronger = best
            .as_ref()
            .is_none_or(|(_, best_total)| total > *best_total);
        if stronger {
            best = Some((resolved, total));
        }
    }
    best.map(|(resolved, _)| resolved)
}

/// Step-log diagnostic for a failed grounding: the target queries plus every
/// role-matching candidate's visible text and weighted total, so a failure
/// reads as evidence instead of `failed · 0/1 steps`. Bounded per candidate
/// and in candidate count for crowded pages.
#[must_use]
pub fn grounding_diagnostic(elements: &[AxElement], intent: &SemanticIntent) -> String {
    let scope = intent.container_query.as_deref().unwrap_or("none");
    let head = format!("Grounding failed. Target container_query: '{scope}'. ");
    let Ok((role, query)) = intent.validate() else {
        return format!(
            "{head}Evaluated 0 candidates: [intent is malformed: label_query: '{}']",
            intent.label_query
        );
    };
    let container = intent.container();
    let focus = container.as_deref().unwrap_or(&query);
    let mut rendered: Vec<String> = Vec::new();
    let mut evaluated: usize = 0;
    for element in elements {
        if !role_admits(&role, &element.role) {
            continue;
        }
        evaluated += 1;
        if rendered.len() >= MAX_DIAGNOSTIC_CANDIDATES {
            continue;
        }
        let (_, total) = total_score(&query, Some(focus), &intent.raw_prompt, element);
        let mut text = if element.name.is_empty() {
            element.description.clone()
        } else {
            element.name.clone()
        };
        let context = joined_container(element);
        if !context.is_empty() {
            text.push_str(" [in: ");
            text.push_str(&context);
            text.push(']');
        }
        let text: String = text.chars().take(MAX_DIAGNOSTIC_TEXT_LEN).collect();
        rendered.push(format!(
            "Candidate {} text: '{text}' (total {total:.2})",
            evaluated - 1,
        ));
    }
    let hidden = evaluated.saturating_sub(rendered.len());
    if hidden > 0 {
        rendered.push(format!("… and {hidden} more"));
    }
    format!(
        "{head}Evaluated {evaluated} candidates: [{}]",
        rendered.join(", ")
    )
}

/// Cost of one replay resolution in USD: always zero. The replay path makes
/// no model calls — scoring is pure Rust over the snapshot — so saved
/// playbooks replay free as well as fast.
pub const RESOLVE_COST_USD: f64 = 0.0;

/// Timed replay metrics: wall-clock duration plus the (zero) model cost.
/// Measured around [`resolve_intent`], which matches the stored signature
/// directly without re-tokenizing any prompt through the resolver.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FastReplayMetrics {
    pub duration_ms: u64,
    pub cost_usd: f64,
}

/// Fast-path replay: resolve the stored intent against the snapshot and
/// report how long the pure-Rust match took. Sub-millisecond in practice;
/// the resolver never runs here, so this is the $0 path saved playbooks
/// take on every step.
#[must_use]
pub fn resolve_fast(
    elements: &[AxElement],
    intent: &SemanticIntent,
) -> (Option<ResolvedIntent>, FastReplayMetrics) {
    let started = std::time::Instant::now();
    let resolved = resolve_intent(elements, intent);
    let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    (
        resolved,
        FastReplayMetrics {
            duration_ms,
            cost_usd: RESOLVE_COST_USD,
        },
    )
}

/// Human-readable old and new signatures plus the winning total, returned
/// (never applied) when the winner differs from the stored intent. The
/// caller — human approval, never automation — decides whether
/// `playbook-store` adopts the proposal.
#[derive(Clone, Debug, PartialEq)]
pub struct DriftDetail {
    pub old_signature: String,
    pub new_signature: String,
    pub total: f64,
}

/// Replay outcome with self-healing visibility: a crisp hit, a resolved but
/// drifted winner carrying its adoption proposal, or a fail-closed
/// diagnostic. Drift never mutates storage; it only describes.
#[derive(Clone, Debug, PartialEq)]
pub enum ResolveOutcome {
    Match(ResolvedIntent),
    Drift {
        resolved: ResolvedIntent,
        detail: DriftDetail,
    },
    NoMatch(String),
}

/// Render one side of a drift comparison: normalized label plus normalized
/// scope (`-` when scopeless), so casing and padding never read as drift.
fn render_signature(label: &str, container: &str) -> String {
    format!("{}|{}", normalize(label), normalize(container))
}

/// Resolve with drift visibility. Crisp evidence — an exact normalized
/// label hit, or a present scope fully overlapped — returns [`Match`]. A
/// resolved winner that only matches fuzzily returns [`Drift`] with the
/// stored and observed signatures for approval. Below-threshold fields
/// return [`NoMatch`] with the standard diagnostic.
#[must_use]
pub fn resolve_with_drift(elements: &[AxElement], intent: &SemanticIntent) -> ResolveOutcome {
    let Some(resolved) = resolve_intent(elements, intent) else {
        return ResolveOutcome::NoMatch(grounding_diagnostic(elements, intent));
    };
    let scope = intent.container();
    let name_hit = normalize(&resolved.element.name) == normalize(&intent.label_query);
    let scope_hit = scope.as_deref().is_some_and(|scope| {
        !scope.is_empty() && container_overlap(scope, &resolved.element) >= 1.0
    });
    if name_hit || scope_hit {
        return ResolveOutcome::Match(resolved);
    }
    let (_, total) = total_score(
        &normalize(&intent.label_query),
        scope.as_deref(),
        &intent.raw_prompt,
        &resolved.element,
    );
    let detail = DriftDetail {
        old_signature: render_signature(
            &intent.label_query,
            intent.container_query.as_deref().unwrap_or("-"),
        ),
        new_signature: render_signature(
            &resolved.element.name,
            &joined_container(&resolved.element),
        ),
        total,
    };
    ResolveOutcome::Drift { resolved, detail }
}

/// Execute one intent: snapshot, resolve, badge, click. The badge stays
/// visible on success as evidence of what was acted on; failures clear it so
/// no stale overlay survives.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] with a [`grounding_diagnostic`] payload
/// when nothing reaches the execution threshold (or the intent is malformed),
/// and [`IntentError::Browser`] on CDP failure.
pub async fn execute_intent(
    browser: &ManagedBrowser,
    origin: &url::Url,
    intent: &SemanticIntent,
) -> Result<IntentOutcome, IntentError> {
    browser.check_origin(origin).await?;
    let elements = browser.ax_snapshot(origin).await?;
    let resolved = resolve_intent(&elements, intent)
        .ok_or_else(|| IntentError::NoMatch(grounding_diagnostic(&elements, intent)))?;
    let highlight = browser.node_rect(resolved.element.backend_node_id).await?;
    let mark = Mark {
        index: 0,
        x: highlight.x,
        y: highlight.y,
        width: highlight.width,
        height: highlight.height,
    };
    browser.show_marks(std::slice::from_ref(&mark)).await?;
    if let Err(error) = browser.click_mark(&mark).await {
        let _ = browser.clear_marks().await;
        return Err(error.into());
    }
    Ok(IntentOutcome { mark, highlight })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(role: &str, name: &str) -> AxElement {
        AxElement {
            backend_node_id: 1,
            role: role.into(),
            name: name.into(),
            description: String::new(),
            container_text: Vec::new(),
        }
    }

    fn intent(role: &str, label: &str) -> SemanticIntent {
        SemanticIntent {
            role: role.into(),
            label_query: label.into(),
            container_query: None,
            // Legacy shapes predate the pass-through prompt: empty contributes
            // no coverage signal, keeping their totals exactly as before.
            raw_prompt: String::new(),
            ordinal_index: None,
            is_last: false,
        }
    }

    fn scoped_intent(role: &str, label: &str, container: &str) -> SemanticIntent {
        SemanticIntent {
            role: role.into(),
            label_query: label.into(),
            container_query: Some(container.into()),
            raw_prompt: String::new(),
            ordinal_index: None,
            is_last: false,
        }
    }

    /// Intent carrying the user's verbatim words for sub-token coverage.
    fn prose_intent(role: &str, label: &str, container: Option<&str>, raw: &str) -> SemanticIntent {
        SemanticIntent {
            role: role.into(),
            label_query: label.into(),
            container_query: container.map(str::to_owned),
            raw_prompt: raw.into(),
            ordinal_index: None,
            is_last: false,
        }
    }

    /// Intent carrying an explicit position among eligible candidates.
    fn ordinal_intent(role: &str, label: &str, index: Option<usize>, last: bool) -> SemanticIntent {
        SemanticIntent {
            role: role.into(),
            label_query: label.into(),
            container_query: None,
            raw_prompt: String::new(),
            ordinal_index: index,
            is_last: last,
        }
    }

    #[test]
    fn scoring_prefers_exact_over_prefix_over_containment() {
        let elements = vec![
            element("button", "Submit application form"),
            element("button", "Submitter"),
            element("button", "Submit"),
        ];
        let Some(resolved) = resolve_intent(&elements, &intent("button", "Submit")) else {
            panic!("exact match resolves")
        };
        assert_eq!(resolved.score, 3);
        assert_eq!(resolved.element.name, "Submit");
    }

    #[test]
    fn role_mismatch_and_empty_query_never_match() {
        let elements = vec![element("button", "Sign in")];
        assert!(resolve_intent(&elements, &intent("link", "Sign in")).is_none());
        assert!(resolve_intent(&elements, &intent("button", "")).is_none());
        assert!(resolve_intent(&elements, &intent("", "Sign in")).is_none());
        assert!(resolve_intent(&[], &intent("button", "Sign in")).is_none());
    }

    #[test]
    fn description_matches_when_name_misses() {
        let mut recovery = element("textbox", "q");
        recovery.description = "Search reports".into();
        let Some(resolved) = resolve_intent(&[recovery], &intent("textbox", "reports")) else {
            panic!("description match resolves")
        };
        assert_eq!(resolved.score, 1);
    }

    #[test]
    fn ties_keep_document_order() {
        let first = AxElement {
            backend_node_id: 11,
            ..element("link", "Pricing")
        };
        let second = AxElement {
            backend_node_id: 22,
            ..element("link", "Pricing")
        };
        let Some(resolved) = resolve_intent(&[first, second], &intent("link", "pricing")) else {
            panic!("tie resolves")
        };
        assert_eq!(resolved.element.backend_node_id, 11);
    }

    #[test]
    fn container_evidence_grounds_otherwise_bare_controls() {
        let plain = element("button", "Download");
        let contextual = AxElement {
            backend_node_id: 7,
            container_text: vec!["Statements".into(), "Statement #42".into()],
            ..element("button", "Download")
        };
        // Name alone cannot match: the container carries the query.
        let Some(resolved) =
            resolve_intent(&[plain, contextual.clone()], &intent("button", "statement"))
        else {
            panic!("container match resolves")
        };
        assert_eq!(resolved.score, 1);
        assert_eq!(resolved.element.backend_node_id, 7);
        // Equal name scores: container hits win over document order.
        let plain_file = AxElement {
            backend_node_id: 11,
            ..element("button", "Download statements file")
        };
        let contextual_file = AxElement {
            backend_node_id: 22,
            container_text: vec!["Statements".into()],
            ..element("button", "Download statements archive")
        };
        let Some(resolved) = resolve_intent(
            &[plain_file, contextual_file],
            &intent("button", "statements"),
        ) else {
            panic!("container tie-break resolves")
        };
        assert_eq!(resolved.element.backend_node_id, 22);
    }

    #[test]
    fn container_scope_boosts_in_scope_controls_without_veto() {
        let in_scope = AxElement {
            backend_node_id: 11,
            container_text: vec!["0LWQXDWW".into()],
            ..element("link", "Download report")
        };
        let out_of_scope = AxElement {
            backend_node_id: 22,
            container_text: vec!["Other".into()],
            ..element("link", "Download report")
        };
        // Same labels: the scoped control wins on container overlap.
        let Some(resolved) = resolve_intent(
            &[out_of_scope.clone(), in_scope.clone()],
            &scoped_intent("link", "download", "0LWQXDWW"),
        ) else {
            panic!("scoped intent resolves")
        };
        assert_eq!(resolved.element.backend_node_id, 11);
        // No veto: a lone out-of-scope control still resolves best-effort
        // instead of failing closed.
        let Some(resolved) = resolve_intent(
            &[out_of_scope],
            &scoped_intent("link", "download", "0LWQXDWW"),
        ) else {
            panic!("partial evidence never vetoes")
        };
        assert_eq!(resolved.element.backend_node_id, 22);
        // Scopeless behavior is unchanged by the new field.
        assert!(resolve_intent(&[in_scope], &intent("link", "download")).is_some());
    }

    #[test]
    fn partial_container_overlap_still_grounds() {
        // The anti-veto core: no candidate contains the full scope, yet the
        // partially matching row outscores the unrelated one and resolves.
        let partial = AxElement {
            backend_node_id: 61,
            container_text: vec!["0LWQXDWW".into()],
            ..element("button", "Download")
        };
        let unrelated = AxElement {
            backend_node_id: 62,
            container_text: vec!["Welcome tour".into()],
            ..element("button", "Download")
        };
        let scope = "0lwqxdww visa 2919";
        let Some(resolved) = resolve_intent(
            &[unrelated.clone(), partial.clone()],
            &scoped_intent("button", "download", scope),
        ) else {
            panic!("partial overlap resolves")
        };
        assert_eq!(resolved.element.backend_node_id, 61);
        // And the partial row resolves on its own: weak evidence lowers the
        // total (2.0 + 10.0 / 3) but never disqualifies.
        assert!(resolve_intent(&[partial], &scoped_intent("button", "download", scope)).is_some());
    }

    #[test]
    fn below_threshold_resolves_to_nothing_with_diagnostic() {
        // Label miss plus no container backing: total 0.0 stays home.
        let elements = vec![element("button", "Download")];
        let intent = intent("button", "Sign in");
        assert!(resolve_intent(&elements, &intent).is_none());
        let diagnostic = grounding_diagnostic(&elements, &intent);
        assert!(diagnostic.contains("Grounding failed."));
        assert!(diagnostic.contains("Target container_query: 'none'"));
        assert!(diagnostic.contains("Evaluated 1 candidates"));
        assert!(diagnostic.contains("Candidate 0 text: 'Download'"));
        // Nothing role-matching: zero candidates, still a readable line.
        let empty = grounding_diagnostic(&[], &intent);
        assert!(empty.contains("Evaluated 0 candidates"));
        // Malformed intents diagnose instead of panicking.
        let broken = SemanticIntent {
            role: String::new(),
            label_query: String::new(),
            container_query: None,
            raw_prompt: String::new(),
            ordinal_index: None,
            is_last: false,
        };
        assert!(grounding_diagnostic(&elements, &broken).contains("malformed"));
    }

    #[test]
    fn container_bounds_reject_empty_and_oversized_scopes() {
        let elements = vec![element("button", "Pay")];
        let empty = SemanticIntent {
            role: "button".into(),
            label_query: "Pay".into(),
            container_query: Some("   ".into()),
            raw_prompt: String::new(),
            ordinal_index: None,
            is_last: false,
        };
        assert!(empty.validate().is_err());
        assert!(resolve_intent(&elements, &empty).is_none());
        let huge = SemanticIntent {
            role: "button".into(),
            label_query: "Pay".into(),
            container_query: Some("x".repeat(513)),
            raw_prompt: String::new(),
            ordinal_index: None,
            is_last: false,
        };
        assert!(huge.validate().is_err());
        assert!(resolve_intent(&elements, &huge).is_none());
        let huge_raw = SemanticIntent {
            role: "button".into(),
            label_query: "Pay".into(),
            container_query: None,
            raw_prompt: "x".repeat(2001),
            ordinal_index: None,
            is_last: false,
        };
        assert!(huge_raw.validate().is_err());
        assert!(resolve_intent(&elements, &huge_raw).is_none());
    }

    #[test]
    fn saved_payloads_without_the_key_still_parse() -> Result<(), Box<dyn std::error::Error>> {
        // Backward compatibility: envelopes written before `containerQuery`
        // existed deserialize with no scope.
        let parsed: SemanticIntent =
            serde_json::from_str(r#"{"role":"button","labelQuery":"Pay"}"#)?;
        assert_eq!(parsed.container_query, None);
        assert!(parsed.validate().is_ok());
        let rendered = serde_json::to_string(&scoped_intent("link", "download", "0LWQXDWW"))?;
        assert!(rendered.contains("\"containerQuery\":\"0LWQXDWW\""));
        let revived: SemanticIntent = serde_json::from_str(&rendered)?;
        assert_eq!(revived.container_query.as_deref(), Some("0LWQXDWW"));
        assert!(
            serde_json::from_str::<SemanticIntent>(
                r#"{"role":"button","labelQuery":"Pay","containerQuery":7}"#
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn intent_shape_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_string(&intent("button", "Sign in"))?;
        let parsed: SemanticIntent = serde_json::from_str(&json)?;
        assert_eq!(parsed.role, "button");
        assert!(serde_json::from_str::<SemanticIntent>(r#"{"role":"button"}"#).is_err());
        Ok(())
    }

    #[test]
    fn container_matching_is_case_insensitive_and_whitespace_normalized() {
        // Uppercase DOM text matches a lowercased user prompt with full
        // token overlap, whitespace and case normalized.
        let uppercase = AxElement {
            backend_node_id: 11,
            container_text: vec!["0LWQXDWW".into()],
            ..element("link", "Download report")
        };
        let other = AxElement {
            backend_node_id: 22,
            container_text: vec!["Other".into()],
            ..element("link", "Download report")
        };
        let Some(resolved) = resolve_intent(
            &[other, uppercase],
            &scoped_intent("link", "download", "0lwqxdww"),
        ) else {
            panic!("lowercase scope matches uppercase container");
        };
        assert_eq!(resolved.element.backend_node_id, 11);
        // Whitespace-collapsed scopes match padded container text.
        let padded = AxElement {
            backend_node_id: 31,
            container_text: vec!["  June   12  ".into()],
            ..element("link", "Download")
        };
        assert!(container_overlap("June 12", &padded) > 0.99);
        assert!(container_overlap("  june   12 ", &padded) > 0.99);
    }

    #[test]
    fn textbox_never_steals_click_intents_despite_perfect_overlap() {
        // Input protection is structural: a search box named exactly
        // "Download" sitting inside the scoped row would total 13.0, yet a
        // click intent (`button`) must still act on the plainer button.
        let search_box = AxElement {
            backend_node_id: 81,
            description: String::new(),
            container_text: vec!["0LWQXDWW".into()],
            ..element("textbox", "Download")
        };
        let plain_button = AxElement {
            backend_node_id: 82,
            container_text: Vec::new(),
            ..element("button", "Download")
        };
        let Some(resolved) = resolve_intent(
            &[search_box, plain_button],
            &scoped_intent("button", "download", "0lwqxdww"),
        ) else {
            panic!("click intent resolves to a button")
        };
        assert_eq!(resolved.element.backend_node_id, 82);
        assert_eq!(resolved.element.role, "button");
    }

    #[test]
    fn tab_roles_admitted_and_grounded_for_click_intents() {
        // Tabs carry no inferred intent role, yet `open the Analytics` (role
        // `link`) must ground on the Analytics tab — admitted one-way into
        // the clickable pool and scored normally from there.
        let analytics = AxElement {
            backend_node_id: 91,
            container_text: vec!["Views".into()],
            ..element("tab", "Analytics")
        };
        let deployments = AxElement {
            backend_node_id: 92,
            container_text: vec!["Views".into()],
            ..element("tab", "Deployments")
        };
        for (label, winner) in [("analytics", 91), ("deployments", 92)] {
            let Some(resolved) = resolve_intent(
                &[analytics.clone(), deployments.clone()],
                &intent("link", label),
            ) else {
                panic!("tab grounds for click intent: {label}")
            };
            assert_eq!(resolved.element.backend_node_id, winner);
            assert_eq!(resolved.element.role, "tab");
            assert_eq!(resolved.score, 3);
        }
        // Button intents admit tabs too; fill intents never do, and the
        // reverse direction stays closed (tabs match nothing for textboxes).
        assert!(
            resolve_intent(
                std::slice::from_ref(&analytics),
                &intent("button", "analytics")
            )
            .is_some()
        );
        assert!(resolve_intent(&[analytics], &intent("textbox", "analytics")).is_none());
        // One-directional means legacy distinctions hold: buttons still
        // never match link intents.
        assert!(
            resolve_intent(&[element("button", "Pricing")], &intent("link", "pricing")).is_none()
        );
    }

    #[test]
    fn lowercase_id_queries_win_decisively_over_unmatched_rows() {
        // The acceptance spelling: a lowercased `1tuewzua` scope against an
        // uppercase container resolves to that row — the 10x container boost
        // puts it far above the scopeless-strength alternatives.
        let target = AxElement {
            backend_node_id: 71,
            container_text: vec!["1TUEWZUA".into(), "Visa ending in 2919".into()],
            ..element("button", "Download")
        };
        let decoy = AxElement {
            backend_node_id: 72,
            container_text: vec!["0FLI32JA".into()],
            ..element("button", "Download")
        };
        let Some(resolved) = resolve_intent(
            &[decoy, target],
            &scoped_intent("button", "download", "1tuewzua"),
        ) else {
            panic!("lowercase ID scope resolves")
        };
        assert_eq!(resolved.element.backend_node_id, 71);
    }

    #[test]
    fn container_matching_is_fuzzy_across_split_attributes() {
        // One row carries amount, status, and date in separate items; the
        // multi-attribute scope matches them in any order or case.
        let row = AxElement {
            backend_node_id: 41,
            container_text: vec!["$4.00".into(), "Declined".into(), "June 12".into()],
            ..element("button", "Download")
        };
        let other = AxElement {
            backend_node_id: 42,
            container_text: vec!["$9.00".into(), "Paid".into(), "June 13".into()],
            ..element("button", "Download")
        };
        for scope in [
            "$4 declined June 12",
            "declined $4 june 12",
            "  $4   DECLINED june 12 ",
        ] {
            let Some(resolved) = resolve_intent(
                &[other.clone(), row.clone()],
                &scoped_intent("button", "download", scope),
            ) else {
                panic!("fuzzy scope resolves: {scope}");
            };
            assert_eq!(resolved.element.backend_node_id, 41, "{scope}");
        }
        // Soft scoring never drops on partial attributes: the wrong-amount
        // row still resolves best-effort (3/4 overlap), while the exact row
        // outranks it head-to-head.
        assert!(
            resolve_intent(
                std::slice::from_ref(&row),
                &scoped_intent("button", "download", "$9 declined June 12")
            )
            .is_some()
        );
        let Some(resolved) = resolve_intent(
            &[row.clone(), other.clone()],
            &scoped_intent("button", "download", "$4 declined June 12"),
        ) else {
            panic!("exact scope outranks partial")
        };
        assert_eq!(resolved.element.backend_node_id, 41);
        // Plurals match singulars and noise words never veto.
        let invoices = AxElement {
            backend_node_id: 51,
            container_text: vec!["Invoice 0LWQXDWW".into()],
            ..element("button", "Download")
        };
        assert!(container_overlap("invoices", &invoices) > 0.99);
        assert!(container_overlap("second invoice", &invoices) > 0.99);
        assert!(
            container_overlap(
                "receipt for last week",
                &AxElement {
                    backend_node_id: 52,
                    container_text: vec!["Receipt".into()],
                    ..element("button", "Download")
                }
            ) > 0.99
        );
    }

    #[test]
    fn subtoken_coverage_counts_label_tokens_in_prompt() {
        // Pure set coverage, no dictionaries: verbose prose covers short
        // labels, unrelated pairs score nothing, empties stay zero.
        assert!(
            calculate_subtoken_coverage(
                "can you please go ahead and click on the submit application button for me",
                "Submit Application",
            ) > 0.99
        );
        assert!(calculate_subtoken_coverage("open the ANALYTICS page", "analytics") > 0.99);
        assert!(calculate_subtoken_coverage("open dashboard", "Analytics") < 0.01);
        assert!(calculate_subtoken_coverage("anything at all", "") < 0.01);
        assert!(calculate_subtoken_coverage("", "Download") < 0.01);
    }

    #[test]
    fn long_prose_prompt_grounds_to_short_button_label() {
        // The stripped label (`button`) misses entirely, yet full label
        // coverage (100.0) grounds the verbose prompt with no verb lists.
        let raw = "can you please go ahead and click on the submit application button for me";
        let target = element("button", "Submit Application");
        let intent = prose_intent("button", "button", None, raw);
        let Some(resolved) = resolve_intent(std::slice::from_ref(&target), &intent) else {
            panic!("long prose grounds to the short label")
        };
        assert_eq!(resolved.element.name, "Submit Application");
        let diagnostic = grounding_diagnostic(std::slice::from_ref(&target), &intent);
        assert!(diagnostic.contains("(total 100.00)"), "{diagnostic}");
    }

    #[test]
    fn search_input_container_text_never_steals_link_click() {
        // The search box's container matches, but the Tier 1 gate admits no
        // textbox into a link intent — so it loses alone (no match) and
        // head-to-head, regardless of overlap scores.
        let raw = "open the analytics page";
        let intent = prose_intent("link", "analytics", None, raw);
        let link = element("link", "Analytics");
        let search_box = AxElement {
            backend_node_id: 102,
            container_text: vec!["Analytics dashboard".into()],
            ..element("textbox", "Search")
        };
        assert!(resolve_intent(std::slice::from_ref(&search_box), &intent).is_none());
        let Some(resolved) = resolve_intent(&[search_box, link], &intent) else {
            panic!("link wins over the matching-container input")
        };
        assert_eq!(resolved.element.role, "link");
        assert_eq!(resolved.element.name, "Analytics");
    }

    #[test]
    fn identical_buttons_disambiguated_by_container_context() {
        // Same names, same coverage: the row whose surroundings mention the
        // target wins by exactly the overlap boost (+10.0).
        let raw = "download invoice 0lwqxdww";
        let intent = prose_intent("button", "download", Some("0lwqxdww"), raw);
        let in_scope = AxElement {
            backend_node_id: 111,
            container_text: vec!["Invoice 0LWQXDWW".into()],
            ..element("button", "Download")
        };
        let out_of_scope = AxElement {
            backend_node_id: 112,
            container_text: vec!["Other".into()],
            ..element("button", "Download")
        };
        let elements = [out_of_scope, in_scope];
        let Some(resolved) = resolve_intent(&elements, &intent) else {
            panic!("scoped row wins")
        };
        assert_eq!(resolved.element.backend_node_id, 111);
        let diagnostic = grounding_diagnostic(&elements, &intent);
        assert!(diagnostic.contains("(total 113.00)"), "{diagnostic}");
        assert!(diagnostic.contains("(total 103.00)"), "{diagnostic}");
    }

    #[test]
    fn ordinal_queries_select_nth_visual_candidate() {
        // Three identical controls in document order (the fixture's visual
        // order — true viewport sorting needs geometry plumbing): `2nd`
        // takes index 1, `last` takes the final one, out-of-range fails
        // closed instead of falling back to Candidate 0.
        let rows = vec![
            AxElement {
                backend_node_id: 121,
                ..element("button", "Invoice")
            },
            AxElement {
                backend_node_id: 122,
                ..element("button", "Invoice")
            },
            AxElement {
                backend_node_id: 123,
                ..element("button", "Invoice")
            },
        ];
        let Some(second) =
            resolve_intent(&rows, &ordinal_intent("button", "invoice", Some(1), false))
        else {
            panic!("ordinal 1 selects the second row")
        };
        assert_eq!(second.element.backend_node_id, 122);
        let Some(last) = resolve_intent(&rows, &ordinal_intent("button", "invoice", None, true))
        else {
            panic!("is_last selects the final row")
        };
        assert_eq!(last.element.backend_node_id, 123);
        assert!(
            resolve_intent(&rows, &ordinal_intent("button", "invoice", Some(7), false)).is_none()
        );
        // Without ordinals the top scorer still wins (unchanged default).
        let Some(top) = resolve_intent(&rows, &intent("button", "invoice")) else {
            panic!("scopeless default resolves")
        };
        assert_eq!(top.element.backend_node_id, 121);
    }

    #[test]
    fn fast_path_replay_bypasses_resolver_in_sub_100ms() {
        // The stored intent is matched directly — this test builds it by
        // hand, so no resolver runs anywhere on this path — in microseconds,
        // with zero model cost by construction.
        let elements = vec![element("button", "Pay now"), element("button", "Cancel")];
        let intent = intent("button", "Pay now");
        let (resolved, metrics) = resolve_fast(&elements, &intent);
        let Some(resolved) = resolved else {
            panic!("stored signature matches directly")
        };
        assert_eq!(resolved.element.name, "Pay now");
        assert!(metrics.duration_ms < 100, "{}", metrics.duration_ms);
        // Bit-exact zero: the cost is assigned, never computed.
        assert_eq!(metrics.cost_usd.to_bits(), RESOLVE_COST_USD.to_bits());
        assert_eq!(RESOLVE_COST_USD.to_bits(), 0.0f64.to_bits());
    }

    #[test]
    fn unmatched_intent_fails_closed_without_candidate_zero() {
        // Neither control shares a token with the prompt: both are evaluated
        // (the log names Candidate 0 and Candidate 1) and neither is acted
        // on — no document-order fallback click.
        let raw = "launch the quantum hyperdrive";
        let intent = prose_intent("button", "hyperdrive", None, raw);
        let elements = vec![
            element("button", "Download"),
            AxElement {
                backend_node_id: 2,
                ..element("button", "Settings")
            },
        ];
        assert!(resolve_intent(&elements, &intent).is_none());
        let diagnostic = grounding_diagnostic(&elements, &intent);
        assert!(
            diagnostic.contains("Evaluated 2 candidates"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("Candidate 0 text:"), "{diagnostic}");
        assert!(diagnostic.contains("Candidate 1 text:"), "{diagnostic}");
    }
}
