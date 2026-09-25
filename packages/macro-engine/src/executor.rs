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

use browser_driver::{
    AuthState, AxElement, AxResyncCheck, BrowserError, Highlight, ManagedBrowser, Mark,
};
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
    /// Collection intent (`all invoices`): resolve and act on every
    /// eligible candidate instead of the top scorer, capped at
    /// [`MAX_BATCH_CLICKS`]. `#[serde(default)]` keeps older payloads
    /// single-shot.
    #[serde(default)]
    pub is_plural: bool,
    /// Navigation pre-condition: when present, execution moves the target
    /// to this URL (same portal — origin checks still apply) before
    /// snapshotting. `None` skips the extra round-trip entirely.
    /// `#[serde(default)]` keeps older payloads as-is.
    #[serde(default)]
    pub entry_url: Option<String>,
    /// Primary target noun stem (`invoice` for `download all my invoices`):
    /// batch candidates must mention it in visible text or surroundings.
    /// Modifiers alone (`all`) never qualify, so header links like
    /// `All issues` cannot join an invoice batch. `None` disables the gate
    /// (lone identifiers scope by container instead). `#[serde(default)]`
    /// keeps older payloads ungated.
    #[serde(default)]
    pub primary_target_noun: Option<String>,
}

/// Upper bound on the pass-through prompt: long prose must ground, never
/// fail validation on length.
const MAX_RAW_PROMPT_LEN: usize = 2000;

impl SemanticIntent {
    /// Shared bounds so schema validation and execution agree on what a
    /// runnable intent looks like. `pub` for the playbook schema only.
    ///
    /// Upper bound on entry URLs: portal paths, never data dumps.
    const MAX_ENTRY_URL_LEN: usize = 2048;

    /// # Errors
    /// Returns [`browser_driver::BrowserError::InvalidAction`] for empty or
    /// oversized roles, queries, and container scopes, an oversized raw
    /// prompt (empty raw prompts stay valid: older payloads carry none), or
    /// a malformed entry URL.
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
        if let Some(entry) = self.entry_url.as_ref() {
            Self::validate_entry_url(entry)?;
        }
        Ok((role, query))
    }

    /// Entry URLs must be absolute `http(s)` portal addresses with a host:
    /// same shape the playbook schema demands of origins.
    fn validate_entry_url(entry: &str) -> Result<(), browser_driver::BrowserError> {
        let valid = entry.len() <= Self::MAX_ENTRY_URL_LEN
            && url::Url::parse(entry).is_ok_and(|url| {
                matches!(url.scheme(), "https" | "http") && url.host_str().is_some()
            });
        if valid {
            Ok(())
        } else {
            Err(browser_driver::BrowserError::InvalidAction)
        }
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
    // `$4` match `$4.00` and vice versa via the surviving `4` token. Empty
    // tokens already returned above, so no emptiness re-check is needed.
    if token.chars().all(|c| c == '0') {
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
    /// Plural collection: every eligible candidate in document order,
    /// capped at [`MAX_BATCH_CLICKS`]. Produced only by [`resolve_batch`].
    BatchMatch(Vec<AxElement>),
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

/// Default cap on sequential batch clicks: runaway-loop protection for
/// plural intents. Bounds a bounded post-pass, never the snapshot itself.
pub const MAX_BATCH_CLICKS: usize = 30;
/// DOM settling pause between batch clicks, letting each action's mutations
/// land before the next control is acted on. Skipped after the final click.
const BATCH_SETTLE_MS: u64 = 100;

/// Landmark roles excluded from plural batches: page chrome (nav, header,
/// footer, sidebar equivalents) never holds the repeating data rows a batch
/// acts on. Mirrors the `LANDMARK_ROLES` set in `browser-driver::a11y`
/// (duplicated, not imported, so each side's set can evolve alone);
/// single-intent resolution ignores landmarks entirely, so nav links stay
/// resolvable on their own.
const BATCH_LANDMARK_EXCLUSIONS: &[&str] =
    &["navigation", "banner", "contentinfo", "complementary"];

/// Whether a candidate lives inside page chrome: batch data collection
/// skips it no matter its score, so one matching sidebar link can never
/// join — or lead — a table batch and navigate the run away mid-sequence.
fn is_page_chrome(element: &AxElement) -> bool {
    element
        .landmark
        .as_deref()
        .is_some_and(|role| BATCH_LANDMARK_EXCLUSIONS.contains(&role))
}

/// Whether a candidate mentions the intent's primary noun stem anywhere
/// human-visible: name, description, or surroundings, case-insensitively.
/// Substring matching keeps stemmed anchors (`invoice`) covering inflected
/// text (`Invoices`, `INV-001` carries no noun and relies on scope
/// instead). Single-intent resolution never consults the anchor — fuzzy
/// tolerance there is a feature, not a bug.
///
/// A tiny closed synonym table covers vocabulary sites use interchangeably
/// for the same surface (`profile`/`account` name the signed-in user's
/// page on most portals). General words only — never site procedures.
const NOUN_SYNONYMS: &[(&str, &[&str])] = &[("profile", &["account"]), ("account", &["profile"])];

fn mentions_noun(element: &AxElement, noun: &str) -> bool {
    let stem = noun.trim().to_lowercase();
    if stem.is_empty() {
        return true;
    }
    let expanded: Vec<&str> = std::iter::once(stem.as_str())
        .chain(
            NOUN_SYNONYMS
                .iter()
                .find(|(word, _)| *word == stem)
                .map(|(_, synonyms)| synonyms.iter().copied())
                .into_iter()
                .flatten(),
        )
        .collect();
    let name = normalize(&element.name);
    let description = normalize(&element.description);
    let container = joined_container(element);
    expanded
        .iter()
        .any(|word| name.contains(word) || description.contains(word) || container.contains(word))
}

/// Collect every eligible data candidate in document order for plural
/// intents, capped at [`MAX_BATCH_CLICKS`]. Ordinal selection does not
/// apply here — `is_plural` asked for all of them. Page-chrome controls
/// are excluded before scoring, and empty fields fail closed with the
/// standard diagnostic instead of an empty batch.
#[must_use]
pub fn resolve_batch(elements: &[AxElement], intent: &SemanticIntent) -> ResolveOutcome {
    let Ok((role, query)) = intent.validate() else {
        return ResolveOutcome::NoMatch(grounding_diagnostic(elements, intent));
    };
    let container = intent.container();
    let focus = container.as_deref().unwrap_or(&query);
    // Anchor nouns arrive pre-trimmed from the resolver; an empty anchor
    // means no gate, like a missing one.
    let anchor = intent
        .primary_target_noun
        .as_deref()
        .filter(|noun| !noun.trim().is_empty());
    let mut batch = Vec::new();
    for element in elements {
        if batch.len() >= MAX_BATCH_CLICKS {
            break;
        }
        if is_page_chrome(element) {
            continue;
        }
        if !role_admits(&role, &element.role) {
            continue;
        }
        if let Some(noun) = anchor
            && !mentions_noun(element, noun)
        {
            continue;
        }
        let (_, total) = total_score(&query, Some(focus), &intent.raw_prompt, element);
        if total < MIN_EXECUTION_SCORE {
            continue;
        }
        batch.push(element.clone());
    }
    if batch.is_empty() {
        ResolveOutcome::NoMatch(grounding_diagnostic(elements, intent))
    } else {
        ResolveOutcome::BatchMatch(batch)
    }
}

/// Upper bound on how far down a results page Stage 2 scans for a
/// followable link. Organic results sit near the top in document order;
/// scanning past this means the noun never really appeared.
const MAX_SEARCH_RESULT_SCAN: usize = 40;

/// Stage-2 navigation budget: how long a followed result may take to leave
/// the search page before the follow is reported as failed.
pub const FOLLOW_POLL_MS: u64 = 250;
pub const FOLLOW_TIMEOUT_MS: u64 = 10_000;

/// What Stage 2 did: the followed link's visible label plus the URL the
/// browser actually landed on. The landed URL is observed, never predicted,
/// so confinement re-anchors to where the click really went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FollowedResult {
    pub label: String,
    pub landed: url::Url,
}

/// Pick the link Stage 2 should follow on a search-results page.
///
/// Evidence-only — there is no engine-specific markup, selector, or result
/// container name anywhere in here. Among `link` candidates in snapshot
/// document order: skip page chrome (the engine's own `navigation` /
/// `banner` / `contentinfo` / `complementary` landmarks), skip unlabeled
/// links, and require the target noun in the link's visible text,
/// description, or surroundings. The first survivor is the top organic
/// result, because document order is the snapshot's ordering contract.
///
/// An empty noun yields `None` rather than "first link on the page": with
/// nothing to match against, clicking anything would be a guess.
#[must_use]
pub fn select_search_result<'a>(elements: &'a [AxElement], noun: &str) -> Option<&'a AxElement> {
    if noun.trim().is_empty() {
        return None;
    }
    elements
        .iter()
        .take(MAX_SEARCH_RESULT_SCAN)
        .find(|element| {
            element.role == "link"
                && !is_page_chrome(element)
                && !element.name.trim().is_empty()
                && mentions_noun(element, noun)
        })
}

/// Diagnostic for a Stage-2 follow that found no candidate: the noun plus
/// every link the page did offer, bounded like [`grounding_diagnostic`], so
/// a failed follow reads as evidence instead of a bare failure.
#[must_use]
pub fn follow_diagnostic(elements: &[AxElement], noun: &str) -> String {
    let mut rendered: Vec<String> = Vec::new();
    let mut links = 0_usize;
    for element in elements.iter().filter(|element| element.role == "link") {
        links += 1;
        if rendered.len() >= MAX_DIAGNOSTIC_CANDIDATES {
            continue;
        }
        let chrome = if is_page_chrome(element) {
            " [chrome]"
        } else {
            ""
        };
        let text: String = element.name.chars().take(MAX_DIAGNOSTIC_TEXT_LEN).collect();
        rendered.push(format!("'{text}'{chrome}"));
    }
    let hidden = links.saturating_sub(rendered.len());
    if hidden > 0 {
        rendered.push(format!("… and {hidden} more"));
    }
    format!(
        "Search follow found no result mentioning '{noun}'. Evaluated {links} links: [{}]",
        rendered.join(", ")
    )
}

/// Whether the live page has navigated away from `from`: origin or path
/// differs. Reuses [`url_drifted`], which also treats `blob:` / `data:`
/// targets as "not yet landed" — exactly right here, since a download
/// handoff is not a site landing and the poll should keep waiting.
fn navigated_away(from: &url::Url, now: &url::Url) -> bool {
    url_drifted(from, now)
}

/// Poll until the live URL leaves `from`, or the timeout elapses.
/// `None` means the click never navigated (an in-page result, or a dead
/// link), so the caller fails the follow honestly instead of re-anchoring
/// confinement to the search page it never left. Pure polling policy over
/// an injected URL reader, so the loop is provable without a browser.
async fn wait_for_navigation_with<F, Fut>(
    mut current: F,
    from: &url::Url,
    poll_interval: std::time::Duration,
    timeout: std::time::Duration,
) -> Option<url::Url>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<url::Url>>,
{
    let started = std::time::Instant::now();
    loop {
        if let Some(now) = current().await
            && navigated_away(from, &now)
        {
            return Some(now);
        }
        if started.elapsed() >= timeout {
            return None;
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// Stage 2 of search-and-follow: click the top result matching `noun` and
/// report where it landed.
///
/// Snapshots the settled search page, selects a candidate by visible
/// evidence, clicks it through the same badge-and-coordinate path every
/// other semantic click uses, then waits for the navigation to land. The
/// returned URL is observed from the live target, so the caller can
/// re-anchor portal confinement to the real destination instead of a
/// predicted one.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] when no link mentions the noun or the
/// click never navigated, and [`IntentError::Browser`] on CDP failure.
pub async fn follow_search_result(
    browser: &ManagedBrowser,
    search_origin: &url::Url,
    noun: &str,
) -> Result<FollowedResult, IntentError> {
    let (elements, _, _) = browser.ax_snapshot(search_origin).await;
    let candidate = select_search_result(&elements, noun)
        .ok_or_else(|| IntentError::NoMatch(follow_diagnostic(&elements, noun)))?;
    let label = candidate.name.clone();
    let from = browser
        .current_url()
        .await?
        .ok_or(browser_driver::BrowserError::WrongOrigin)?;
    click_element(browser, candidate).await?;
    let landed = wait_for_navigation_with(
        || async { browser.current_url().await.ok().flatten() },
        &from,
        std::time::Duration::from_millis(FOLLOW_POLL_MS),
        std::time::Duration::from_millis(FOLLOW_TIMEOUT_MS),
    )
    .await
    .ok_or_else(|| {
        IntentError::NoMatch(format!(
            "Search follow clicked '{label}' but the page never left the results."
        ))
    })?;
    Ok(FollowedResult { label, landed })
}

/// What pursuing an in-page follow-up did. The landed URL is observed from
/// the live page, never predicted — the same contract as [`FollowedResult`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageGoalOutcome {
    /// A click navigated somewhere new: the clicked label plus the URL.
    Navigated { label: String, landed: url::Url },
    /// The goal was already achieved on the current page; nothing to click.
    AlreadyThere { landed: url::Url },
    /// Verified goal landing: the label that got us there, the landed URL,
    /// and the username the live page revealed (if any — only the
    /// account-home worker fills this in). Only produced by a goal
    /// worker's verifier — a click alone never yields this.
    Verified {
        label: String,
        landed: url::Url,
        username: Option<String>,
    },
    /// The page is a signed-out guest landing: there is no identity
    /// chrome to pursue, so the worker stops before any click.
    SignedOut,
}

/// In-page follow-up budget: observe-act steps before the pursuit gives up
/// and reports [`IntentError::NoMatch`] with the evidence.
const PAGE_GOAL_MAX_STEPS: usize = 3;

/// Model-guided phase budget: the navigator gets fewer steps than the
/// deterministic phase because each one costs a model call.
const MODEL_GOAL_MAX_STEPS: usize = 5;

/// Actionable roles a follow-up can meaningfully click: links plus the
/// controls menus are made of. Wider than [`select_search_result`]'s
/// link-only contract on purpose — on a portal page the target often lives
/// behind a button.
const PAGE_GOAL_ROLES: &[&str] = &["link", "button", "menuitem", "menuitemlink"];

/// Pursue `noun` on the already-loaded portal page: the Muse-style
/// follow-up. No new browser, no entry-URL resolution, no navigation
/// before acting. Two phases:
///
/// 1. **Deterministic** (free, instant): click the first actionable control
///    mentioning `noun`; unfold one header menu when it isn't directly
///    visible. A URL change ends the pursuit as navigated.
/// 2. **Model-guided** (only when `navigator` is `Some`): the navigator
///    picks the next click from the live snapshot, up to
///    [`MODEL_GOAL_MAX_STEPS`] steps. Every picked element id is validated
///    against the snapshot before clicking.
///
/// [`IntentError::NoMatch`] with the rendered evidence when both phases
/// fail; [`IntentError::Browser`] on CDP failure.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] when the goal is not reached, and
/// [`IntentError::Browser`] on CDP failure.
pub async fn pursue_page_goal(
    browser: &ManagedBrowser,
    origin: &url::Url,
    noun: &str,
    navigator: Option<std::sync::Arc<dyn crate::navigator::PageNavigator>>,
) -> Result<PageGoalOutcome, IntentError> {
    let noun = noun.trim();
    if noun.is_empty() {
        return Err(IntentError::NoMatch("empty in-page goal".to_string()));
    }
    let deterministic_miss = match pursue_deterministic(browser, origin, noun).await {
        Ok(outcome) => return Ok(outcome),
        // Browser errors fail fast: retrying them through the model would
        // just burn model calls on a dead CDP session.
        Err(IntentError::Browser(error)) => return Err(IntentError::Browser(error)),
        Err(IntentError::NoMatch(diagnostic)) => diagnostic,
    };
    let Some(navigator) = navigator else {
        return Err(IntentError::NoMatch(deterministic_miss));
    };
    pursue_with_model(browser, origin, noun, navigator, deterministic_miss).await
}

/// Stable identity for a control the worker already tried. Backend node
/// ids churn on dynamic pages — React re-renders the header between
/// snapshots — so retry exclusion keys on what the user perceives
/// (role + name), never the node id. Without this the worker re-clicks
/// the same menu button once per snapshot and never advances to the next
/// candidate (caught on live Reddit: "Open user actions" clicked 3×).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickedControl {
    role: String,
    name: String,
}

impl ClickedControl {
    /// Stable key for `element`: role plus trimmed, lowercased name.
    #[must_use]
    pub fn of(element: &AxElement) -> Self {
        Self {
            role: element.role.clone(),
            name: element.name.trim().to_lowercase(),
        }
    }

    /// Whether `element` is the same logical control, regardless of the
    /// backend node id the current snapshot assigned it.
    #[must_use]
    pub fn matches(&self, element: &AxElement) -> bool {
        self.role == element.role && self.name == element.name.trim().to_lowercase()
    }
}

/// Whether `element` was already tried, by stable identity rather than
/// the snapshot-local backend node id.
fn already_clicked(clicked: &[ClickedControl], element: &AxElement) -> bool {
    clicked.iter().any(|tried| tried.matches(element))
}

/// Pursue an `account_home` goal ("my profile", "my account") on the
/// already-loaded portal page. Unlike the generic noun hunt, this worker
/// is scoped to the header identity chrome:
///
/// 1. **Guest short-circuit**: a signed-out page has no identity chrome,
///    so the worker stops before any click (`SignedOut`).
/// 2. **Deterministic chrome walk**: open the header identity control
///    (landmarked → account-worded → rightmost-in-strip), quiet-wait for
///    the revealed layer, then take the page-revealed profile destination
///    — a validated href when the page offers one, else a click with an
///    observed URL change. Every click must show an effect (new controls
///    or navigation) or the candidate is discarded.
/// 3. **Verifier**: a click alone never succeeds — the landed page must
///    be same-origin, non-root, and carry the revealed username when one
///    was read.
/// 4. **Model fallback** (only when `navigator` is `Some`): the model
///    picks the identity click, then the verifier still decides.
///
/// [`IntentError::NoMatch`] carries the tried-click log, not a control
/// dump, so the FAILED card shows what the worker actually attempted.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] when the goal is not reached or the
/// page is signed out (as `SignedOut`, not an error), and
/// [`IntentError::Browser`] on CDP failure.
pub async fn pursue_account_home(
    browser: &ManagedBrowser,
    origin: &url::Url,
    navigator: Option<std::sync::Arc<dyn crate::navigator::PageNavigator>>,
) -> Result<PageGoalOutcome, IntentError> {
    if browser.auth_state().await == AuthState::LoggedOut {
        return Ok(PageGoalOutcome::SignedOut);
    }
    let deterministic_miss = match pursue_identity_chrome(browser, origin).await {
        Ok(outcome) => return Ok(outcome),
        // Browser errors fail fast: retrying them through the model would
        // just burn model calls on a dead CDP session.
        Err(IntentError::Browser(error)) => return Err(IntentError::Browser(error)),
        Err(IntentError::NoMatch(diagnostic)) => diagnostic,
    };
    let Some(navigator) = navigator else {
        return Err(IntentError::NoMatch(deterministic_miss));
    };
    pursue_account_home_with_model(browser, origin, navigator, deterministic_miss).await
}

/// Identity-chrome click budget: opening the account menu plus one
/// revealed control is two clicks; a third covers a nested disclosure.
/// Bounded so a hostile header can't burn the run.
const IDENTITY_MAX_CLICKS: usize = 3;

/// Deterministic half of [`pursue_account_home`]: open the header identity
/// control, read what the live page reveals, navigate, verify.
async fn pursue_identity_chrome(
    browser: &ManagedBrowser,
    origin: &url::Url,
) -> Result<PageGoalOutcome, IntentError> {
    let mut clicked: Vec<ClickedControl> = Vec::new();
    let mut tried: Vec<String> = Vec::new();
    // Node ids from the previous iteration's snapshot. A "revealed"
    // destination must be genuinely new: the container-text rollup lets an
    // opened menu's "Profile" wording match every header button that was
    // already on the page, so without this the worker clicks the header
    // chrome itself as the profile destination and burns its click budget.
    let mut previously_seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    // Total click budget across both click kinds. The shared menu
    // primitive retries internally now (up to MENU_OPEN_MAX_TRIES per
    // call), so the loop counts every click it spends — menu-opening or
    // revealed-destination — against IDENTITY_MAX_CLICKS instead of
    // assuming one click per iteration. A menu Miss ends the worker: the
    // primitive already re-snapshotted and re-ranked between attempts, so
    // re-looping would only re-examine candidates it exhausted.
    let mut clicks_used: usize = 0;

    while clicks_used < IDENTITY_MAX_CLICKS {
        // Full snapshot, not the navigator slice: a revealed menu renders
        // at the end of the document (React portal), past the 300-element
        // head truncation — the capped view reported "no new controls"
        // for a menu that plainly opened. Before/after id sets share the
        // same full-list semantics so "genuinely new" stays correct.
        let (elements, check_before, _) = browser.ax_snapshot_untruncated(origin).await;
        let currently_seen: std::collections::HashSet<i64> =
            elements.iter().map(|el| el.backend_node_id).collect();

        // 1. A revealed profile destination ends the hunt — but only after
        // the worker opened something, and only when the candidate actually
        // appeared after that opening. A bare page's `u/someone` author
        // links are other users, never "my" profile.
        if let Some(target) = select_revealed_profile(&elements, &clicked, &previously_seen) {
            let label = target.name.clone();
            let node_id = target.backend_node_id;
            // Prefer the page-revealed href when the control is a link
            // that discloses one; validated in Rust, never trusted raw.
            if let Some(href) = browser.node_href(node_id).await
                && let Some(url) = validate_revealed_href(&href, origin)
            {
                tried.push(tried_label(target, "revealed href → navigate"));
                browser.navigate(&url).await?;
                let username = username_from_href(&url);
                let landed = browser.current_url().await.ok().flatten().unwrap_or(url);
                return verify_account_home(browser, origin, &label, &landed, username.as_deref())
                    .await;
            }
            // No usable href: click the revealed control and watch the URL.
            click_element(browser, target).await?;
            clicked.push(ClickedControl::of(target));
            clicks_used += 1;
            tried.push(tried_label(target, "clicked, watching URL"));
            if let Some(landed) = wait_for_url_change(browser).await {
                let username = username_from_menu_text(&label);
                return verify_account_home(browser, origin, &label, &landed, username.as_deref())
                    .await;
            }
            previously_seen = currently_seen;
            continue;
        }

        // 2. No revealed destination: spend the remaining budget on the
        // shared menu primitive in a single call — its internal
        // click → poll-for-evidence → re-rank loop supersedes the old
        // one-attempt-per-iteration shape, and a Miss means its
        // candidates are exhausted against fresh snapshots, so the
        // worker stops instead of re-looping.
        let baseline = MenuOpenBaseline {
            elements: &elements,
            check: &check_before,
        };
        let clicks_before = clicked.len();
        let opened = match open_identity_menu(
            browser,
            origin,
            baseline,
            &mut clicked,
            (IDENTITY_MAX_CLICKS - clicks_used).min(MENU_OPEN_MAX_TRIES),
        )
        .await?
        {
            OpenMenuOutcome::Opened { tried: line, .. } => {
                tried.push(line);
                true
            }
            OpenMenuOutcome::Miss { tried: lines } => {
                tried.extend(lines);
                false
            }
        };
        // The primitive records every attempt in `clicked`: the spend
        // counts whether or not a menu opened.
        clicks_used += clicked.len() - clicks_before;
        if !opened {
            break;
        }
        previously_seen = currently_seen;
    }
    Err(IntentError::NoMatch(identity_miss_diagnostic(&tried)))
}

/// Model-guided half of [`pursue_account_home`]: the navigator picks the
/// identity click from the live snapshot, but the verifier still decides
/// success — a model-picked click alone never yields `Verified`.
async fn pursue_account_home_with_model(
    browser: &ManagedBrowser,
    origin: &url::Url,
    navigator: std::sync::Arc<dyn crate::navigator::PageNavigator>,
    deterministic_miss: String,
) -> Result<PageGoalOutcome, IntentError> {
    // The deterministic tried-log rides along as prompt context: without
    // it the model re-proposes (or fails to recognize) controls the
    // deterministic phase already evaluated — e.g. concluding "no avatar
    // present" while staring at the "Open user actions" button it tried.
    let goal = format!(
        "account home: open the account/avatar menu in the page header, then the profile control, to reach my own profile page. Already tried without success: {deterministic_miss}"
    );
    match pursue_with_model(browser, origin, &goal, navigator, deterministic_miss).await {
        Ok(PageGoalOutcome::Navigated { label, landed }) => {
            verify_account_home(browser, origin, &label, &landed, None).await
        }
        Ok(other) => Ok(other),
        Err(error) => Err(error),
    }
}

/// What one shared menu-opening attempt did. Both in-page lanes route
/// their menu step through [`open_identity_menu`]; the tried-label lines
/// feed each lane's own miss journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenMenuOutcome {
    /// A candidate's click opened a menu/layer: the stable identity of
    /// the clicked control plus its tried-label line
    /// (`role 'name' → effect`) for the caller's journal.
    Opened {
        control: ClickedControl,
        tried: String,
    },
    /// No candidate opened a menu. `tried` holds one tried-label line per
    /// attempt, in attempt order; empty means no candidate existed at all.
    Miss { tried: Vec<String> },
}

/// Baseline snapshot for [`open_identity_menu`]'s click-effect check:
/// the element list plus the resync check carrying the raw AX node
/// count, from a single `ax_snapshot_untruncated` call.
#[derive(Debug)]
pub struct MenuOpenBaseline<'a> {
    pub elements: &'a [AxElement],
    pub check: &'a browser_driver::AxResyncCheck,
}

/// Async browser seam for the shared identity-menu primitive
/// ([`open_identity_menu`]): the CDP operations one menu-open attempt
/// needs. [`ManagedBrowser`] is the production implementation; tests
/// drive the click → poll → evidence loop against a scripted fake, so
/// the retry and timing behavior is proven with no Chromium.
pub trait MenuBrowser {
    /// Viewport CSS dimensions `(width, height)`; `None` fails the
    /// header-geometry candidate tiers closed.
    fn menu_viewport_size(&self) -> impl std::future::Future<Output = Option<(f64, f64)>> + Send;
    /// Viewport rectangle for a backend node id; errors (hidden or stale
    /// nodes) exclude the control from the geometry tiers.
    fn menu_node_rect(
        &self,
        backend_node_id: i64,
    ) -> impl std::future::Future<Output = Result<Highlight, BrowserError>> + Send;
    /// Full interactive-element list without head truncation, plus the
    /// resync check carrying the raw AX node count.
    fn menu_snapshot(
        &self,
        origin: &url::Url,
    ) -> impl std::future::Future<Output = (Vec<AxElement>, AxResyncCheck, u64)> + Send;
    /// Best-effort `aria-expanded` of the clicked control; `None` means
    /// unknown and is never evidence.
    fn menu_node_expanded(
        &self,
        backend_node_id: i64,
    ) -> impl std::future::Future<Output = Option<bool>> + Send;
    /// Click the control (rect resolve, mark, press).
    ///
    /// # Errors
    /// Returns [`IntentError::Browser`] on CDP failure.
    fn menu_click(
        &self,
        element: &AxElement,
    ) -> impl std::future::Future<Output = Result<(), IntentError>> + Send;
}

impl MenuBrowser for ManagedBrowser {
    async fn menu_viewport_size(&self) -> Option<(f64, f64)> {
        self.viewport_size().await
    }

    async fn menu_node_rect(&self, backend_node_id: i64) -> Result<Highlight, BrowserError> {
        self.node_rect(backend_node_id).await
    }

    async fn menu_snapshot(&self, origin: &url::Url) -> (Vec<AxElement>, AxResyncCheck, u64) {
        self.ax_snapshot_untruncated(origin).await
    }

    async fn menu_node_expanded(&self, backend_node_id: i64) -> Option<bool> {
        self.node_expanded(backend_node_id).await
    }

    async fn menu_click(&self, element: &AxElement) -> Result<(), IntentError> {
        click_element(self, element).await.map(|_| ())
    }
}

/// Per-invocation click cap for the shared menu primitive: one candidate
/// per try, each effect-verified. Callers pass their own remaining
/// budget (the identity lane passes its `IDENTITY_MAX_CLICKS` remainder);
/// the primitive never exceeds this cap, so a hostile header can't burn
/// the run.
const MENU_OPEN_MAX_TRIES: usize = 3;

/// Ordered menu-opening candidates, pure decision: (a) landmarked
/// banner/navigation buttons, then (b) account-worded buttons
/// ([`mentions_account_word`]), then (c) blank-named buttons inside the
/// header strip — the unlabeled-avatar case, structural (empty name +
/// header geometry), never a control-name string — then (d) the
/// remaining unclicked buttons inside the header strip, rightmost first.
/// Tier (d) folds in the rightmost-geometry fallback both lanes already
/// had (`select_identity_control`'s second tier and
/// [`select_rightmost_button`]), so sharing the primitive doesn't drop
/// the named-but-wordless header button case; blank-named avatars still
/// outrank arbitrary named buttons. `rects` carries the
/// boundary-measured `(backend_node_id, x, y)` of the unclicked buttons;
/// `strip_bottom` is the viewport-relative header cutoff (`None` when
/// the viewport won't read — geometry tiers stay empty, fail closed).
/// Already-clicked controls are excluded in every tier; each control
/// appears once, at its highest tier. CDP stays in the callers; this
/// stays unit-testable like [`pick_rightmost`].
#[must_use]
pub fn rank_menu_candidates<'a>(
    elements: &'a [AxElement],
    clicked: &[ClickedControl],
    rects: &[(i64, f64, f64)],
    strip_bottom: Option<f64>,
) -> Vec<&'a AxElement> {
    let in_strip: std::collections::HashMap<i64, (f64, f64)> = rects
        .iter()
        .filter(|(_, _, y)| strip_bottom.is_some_and(|bottom| *y <= bottom))
        .map(|(id, x, y)| (*id, (*x, *y)))
        .collect();
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut ranked: Vec<&'a AxElement> = Vec::new();
    let mut push = |element: &'a AxElement| {
        if seen.insert(element.backend_node_id) {
            ranked.push(element);
        }
    };
    // (a) landmarked header chrome.
    for element in elements.iter().filter(|element| {
        element.role == "button"
            && !already_clicked(clicked, element)
            && matches!(element.landmark.as_deref(), Some("banner" | "navigation"))
    }) {
        push(element);
    }
    // (b) account-worded buttons anywhere.
    for element in elements.iter().filter(|element| {
        element.role == "button"
            && !already_clicked(clicked, element)
            && mentions_account_word(element)
    }) {
        push(element);
    }
    // (c) + (d): in-strip buttons, blank-named first, each rightmost first.
    let mut strip_buttons: Vec<(&'a AxElement, f64)> = elements
        .iter()
        .filter(|element| {
            element.role == "button"
                && !already_clicked(clicked, element)
                && in_strip.contains_key(&element.backend_node_id)
        })
        .map(|element| (element, in_strip[&element.backend_node_id].0))
        .collect();
    strip_buttons.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut blank: Vec<(&'a AxElement, f64)> = Vec::new();
    let mut named: Vec<(&'a AxElement, f64)> = Vec::new();
    for entry in strip_buttons {
        if entry.0.name.trim().is_empty() {
            blank.push(entry);
        } else {
            named.push(entry);
        }
    }
    for (element, _) in blank.into_iter().chain(named) {
        push(element);
    }
    ranked
}

/// Header-geometry rects at the CDP boundary: `(backend_node_id, x, y)`
/// for every unclicked button, feeding [`rank_menu_candidates`] tiers
/// (c) and (d). `node_rect` rejects degenerate (hidden) boxes, so
/// invisible controls never qualify.
async fn header_button_rects<B: MenuBrowser>(
    browser: &B,
    elements: &[AxElement],
    clicked: &[ClickedControl],
) -> Vec<(i64, f64, f64)> {
    let mut rects = Vec::new();
    for element in elements
        .iter()
        .filter(|element| element.role == "button" && !already_clicked(clicked, element))
    {
        if let Ok(highlight) = browser.menu_node_rect(element.backend_node_id).await {
            rects.push((element.backend_node_id, highlight.x, highlight.y));
        }
    }
    rects
}

/// Click-effect verdict after a disclosure click, in the established
/// before/after diagnostic style: the menu counts as opened when the
/// actionable count grew or genuinely new nodes appeared. Node ids churn
/// on dynamic pages, so "new" is informational — retry exclusion keys on
/// stable identity, not ids. The raw AX node delta distinguishes "menu
/// rendered past the old head truncation" from "the click changed
/// nothing at all". Returns `(opened, effect)`.
fn menu_open_effect(
    before_ids: &std::collections::HashSet<i64>,
    actionable_before: usize,
    raw_before: usize,
    after: &[AxElement],
    raw_after: usize,
) -> (bool, String) {
    let actionable_after = count_actionable(after);
    let new_nodes: Vec<String> = after
        .iter()
        .filter(|element| !before_ids.contains(&element.backend_node_id))
        .take(6)
        .map(|element| format!("{} '{}'", element.role, element.name))
        .collect();
    let opened = actionable_after > actionable_before || !new_nodes.is_empty();
    let effect = if new_nodes.is_empty() {
        format!(
            "no new controls (actionable {actionable_before} → {actionable_after}; raw AX nodes {raw_before} → {raw_after})"
        )
    } else {
        format!(
            "+{} new [{}] (raw AX nodes {raw_before} → {raw_after})",
            new_nodes.len(),
            new_nodes.join(", ")
        )
    };
    (opened, effect)
}

/// Evidence-poll cadence after a disclosure click: re-snapshot every
/// 200ms and stop at the first tick that shows the menu opened, or give
/// up after 3s with no evidence. A live feed (Reddit's infinite scroll)
/// never settles, so `page_quiet` is not a signal here — the old
/// quiet-wait burned a fixed ~2s and snapshotted at an arbitrary moment,
/// which is exactly the race that flaked "open settings on reddit".
const MENU_OPEN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
const MENU_OPEN_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Roles that mark a revealed menu layer for the open-evidence check.
fn is_menu_role(role: &str) -> bool {
    matches!(role, "menu" | "menuitem" | "menuitemlink")
}

/// Per-tick opening evidence from a fresh snapshot, as the tried-journal
/// effect line. Any one of: (a) the [`menu_open_effect`] verdict — new
/// actionable controls vs the pre-click baseline (actionable count grew
/// or genuinely new backend nodes appeared); (c) menu-layer roles the
/// baseline lacked (a control that changes role keeps its backend node
/// id, so this catches what the new-node check misses). `None` when the
/// snapshot shows nothing. `aria-expanded` (b) needs a CDP read, so the
/// poll checks it only when this returns `None`.
fn menu_open_evidence(
    before_ids: &std::collections::HashSet<i64>,
    actionable_before: usize,
    raw_before: usize,
    menu_ids_before: &std::collections::HashSet<i64>,
    after: &[AxElement],
    raw_after: usize,
) -> Option<String> {
    let (opened, effect) =
        menu_open_effect(before_ids, actionable_before, raw_before, after, raw_after);
    if opened {
        return Some(effect);
    }
    let fresh_menu: Vec<String> = after
        .iter()
        .filter(|element| {
            is_menu_role(&element.role) && !menu_ids_before.contains(&element.backend_node_id)
        })
        .take(6)
        .map(|element| format!("{} '{}'", element.role, element.name))
        .collect();
    if fresh_menu.is_empty() {
        return None;
    }
    Some(format!(
        "menu layer revealed [{}] (actionable {actionable_before} → {}; raw AX nodes {raw_before} → {raw_after})",
        fresh_menu.join(", "),
        count_actionable(after),
    ))
}

/// Outcome of one disclosure click's evidence poll: whether a menu
/// opened, the tried-journal effect line, and the final fresh snapshot —
/// reused as the next attempt's re-rank baseline so a miss never pays
/// for a second fetch.
struct MenuOpenPoll {
    opened: bool,
    effect: String,
    after: Vec<AxElement>,
    raw_after: usize,
}

/// Poll-until-open after a disclosure click: snapshot, test the three
/// evidences, repeat every [`MENU_OPEN_POLL_INTERVAL`] until
/// [`MENU_OPEN_POLL_TIMEOUT`]. Returns at the first tick showing
/// evidence — a fast menu costs one snapshot, not the full bound — and
/// never past the bound. Evidence beats the clock: a tick that fires at
/// the deadline still counts as opened.
async fn poll_for_menu_open<B: MenuBrowser>(
    browser: &B,
    origin: &url::Url,
    candidate: &AxElement,
    before_ids: &std::collections::HashSet<i64>,
    actionable_before: usize,
    raw_before: usize,
    menu_ids_before: &std::collections::HashSet<i64>,
) -> MenuOpenPoll {
    let deadline = std::time::Instant::now() + MENU_OPEN_POLL_TIMEOUT;
    loop {
        let (after, check_after, _) = browser.menu_snapshot(origin).await;
        let raw_after = check_after.node_count;
        if let Some(effect) = menu_open_evidence(
            before_ids,
            actionable_before,
            raw_before,
            menu_ids_before,
            &after,
            raw_after,
        ) {
            return MenuOpenPoll {
                opened: true,
                effect,
                after,
                raw_after,
            };
        }
        // (b) aria-expanded on the clicked control: the only signal when
        // the menu renders with no AX subtree of its own. Checked after
        // the snapshot evidences so a fast menu costs no extra CDP call.
        if browser.menu_node_expanded(candidate.backend_node_id).await == Some(true) {
            let effect = format!(
                "aria-expanded=true on the clicked control (actionable {actionable_before} → {}; raw AX nodes {raw_before} → {raw_after})",
                count_actionable(&after),
            );
            return MenuOpenPoll {
                opened: true,
                effect,
                after,
                raw_after,
            };
        }
        if std::time::Instant::now() >= deadline {
            let (_, effect) =
                menu_open_effect(before_ids, actionable_before, raw_before, &after, raw_after);
            return MenuOpenPoll {
                opened: false,
                effect,
                after,
                raw_after,
            };
        }
        tokio::time::sleep(MENU_OPEN_POLL_INTERVAL).await;
    }
}

/// Shared menu-opening primitive for both in-page lanes. Each attempt:
/// rank [`rank_menu_candidates`] (landmarked → account-worded →
/// unlabeled-in-header → rightmost-in-header) against the live tree,
/// click the top unclicked candidate, then poll for opening evidence
/// ([`poll_for_menu_open`]) instead of a quiet-wait — a live feed never
/// settles, so the wait is evidence-driven and returns the moment the
/// menu shows. A miss re-ranks from the poll's final snapshot (the tree
/// moves under us; backend ids churn, so retry exclusion keys on the
/// stable [`ClickedControl`] identity) and tries the next candidate.
/// Bounded by `max_tries` and the [`MENU_OPEN_MAX_TRIES`] cap; stops
/// early when no unclicked candidate remains. Browser errors propagate;
/// every attempted click is recorded in `clicked` by stable identity so
/// neither lane re-tries — or toggles shut — the same control.
///
/// # Errors
/// Returns [`IntentError::Browser`] on CDP failure.
pub async fn open_identity_menu<B: MenuBrowser>(
    browser: &B,
    origin: &url::Url,
    baseline: MenuOpenBaseline<'_>,
    clicked: &mut Vec<ClickedControl>,
    max_tries: usize,
) -> Result<OpenMenuOutcome, IntentError> {
    // Attempt baselines: the first attempt ranks the caller's snapshot
    // (no extra fetch); later attempts re-rank from the previous
    // attempt's final poll snapshot.
    let mut current: Option<(Vec<AxElement>, usize)> = None;
    let mut tried = Vec::new();
    for _ in 0..max_tries.min(MENU_OPEN_MAX_TRIES) {
        let (elements, raw_before) = match current.as_ref() {
            Some((elements, raw)) => (elements.as_slice(), *raw),
            None => (baseline.elements, baseline.check.node_count),
        };
        let actionable_before = count_actionable(elements);
        let before_ids: std::collections::HashSet<i64> = elements
            .iter()
            .map(|element| element.backend_node_id)
            .collect();
        let menu_ids_before: std::collections::HashSet<i64> = elements
            .iter()
            .filter(|element| is_menu_role(&element.role))
            .map(|element| element.backend_node_id)
            .collect();

        // Geometry tiers at the CDP boundary; tiers (a) and (b) are pure.
        // An unreadable viewport fails the geometry tiers closed to empty —
        // tiers (a) and (b) are still tried.
        let strip_bottom = browser
            .menu_viewport_size()
            .await
            .map(|(_, height)| height * HEADER_STRIP_FRACTION);
        let rects = header_button_rects(browser, elements, clicked).await;
        let candidate = match rank_menu_candidates(elements, clicked, &rects, strip_bottom)
            .into_iter()
            .next()
        {
            Some(candidate) => candidate.clone(),
            // Every candidate already tried: re-clicking would only
            // toggle a menu shut.
            None => break,
        };

        let tried_key = ClickedControl::of(&candidate);
        browser.menu_click(&candidate).await?;
        clicked.push(tried_key.clone());

        let poll = poll_for_menu_open(
            browser,
            origin,
            &candidate,
            &before_ids,
            actionable_before,
            raw_before,
            &menu_ids_before,
        )
        .await;
        let line = tried_label(&candidate, &poll.effect);
        tried.push(line.clone());
        if poll.opened {
            return Ok(OpenMenuOutcome::Opened {
                control: tried_key,
                tried: line,
            });
        }
        current = Some((poll.after, poll.raw_after));
    }
    Ok(OpenMenuOutcome::Miss { tried })
}

/// A revealed profile destination: an actionable control mentioning
/// profile/account words or carrying a `u/`-style username, not yet
/// clicked, and genuinely new since the previous snapshot — the
/// container-text rollup shares an opened menu's wording with the header
/// buttons that were already there, so "new" is what makes it revealed.
/// Gated on `clicked` being non-empty (see [`pursue_identity_chrome`]) so
/// author links on a bare page never qualify.
#[must_use]
pub fn select_revealed_profile<'a, S: std::hash::BuildHasher>(
    elements: &'a [AxElement],
    clicked: &[ClickedControl],
    previously_seen: &std::collections::HashSet<i64, S>,
) -> Option<&'a AxElement> {
    if clicked.is_empty() {
        return None;
    }
    elements.iter().find(|element| {
        PAGE_GOAL_ROLES.contains(&element.role.as_str())
            && !already_clicked(clicked, element)
            && !previously_seen.contains(&element.backend_node_id)
            // `mentions_noun` expands "profile" to {"profile", "account"}
            // via NOUN_SYNONYMS, so one call covers both words.
            && (mentions_noun(element, "profile")
                || username_from_menu_text(&element.name).is_some())
    })
}

/// Settings vocabulary for the revealed-settings matcher. `NOUN_SYNONYMS`
/// carries no "setting" expansion, so the matcher uses this closed word
/// list instead. General words only — never site procedures. Substring
/// matching means "setting" also covers "settings" and "preference"
/// covers "preferences"; the longer forms are listed for readability.
const SETTINGS_WORDS: &[&str] = &["setting", "settings", "preference", "preferences"];

/// Whether `text` mentions a settings word, in [`normalize`]d form.
fn text_mentions_settings(text: &str) -> bool {
    let haystack = normalize(text);
    SETTINGS_WORDS.iter().any(|word| haystack.contains(word))
}

/// Whether the element's name, description, or container rollup mentions
/// a settings word — the name half of the revealed-settings matcher.
fn mentions_settings_words(element: &AxElement) -> bool {
    text_mentions_settings(&element.name)
        || text_mentions_settings(&element.description)
        || text_mentions_settings(&joined_container(element))
}

/// Whether a URL path segment names a settings destination, stemmed:
/// "settings" and "preferences" reduce to the roots the word list uses,
/// and compound segments ("user-settings", `account_preferences`) match
/// on their tokens. Generic path vocabulary — no site routes.
fn settings_path_token(segment: &str) -> bool {
    segment
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|token| {
            let stem = token.strip_suffix('s').unwrap_or(token);
            stem == "setting" || stem == "preference"
        })
}

/// Whether the URL's path carries a settings segment (stemmed match).
fn url_path_mentions_settings(url: &url::Url) -> bool {
    match url.path_segments() {
        Some(mut segments) => segments.any(settings_path_token),
        None => false,
    }
}

/// A revealed settings destination: an actionable control mentioning
/// settings/preferences words, not yet clicked, and genuinely new since
/// the previous snapshot — the same "revealed" gating as
/// [`select_revealed_profile`]: the container-text rollup shares an
/// opened menu's wording with header buttons that were already there,
/// so "new" is what makes it revealed. Gated on `clicked` being
/// non-empty so a bare page's "settings" footer link never qualifies —
/// the worker must have opened something first.
/// The href half of the matcher (a blank-named link to a settings path)
/// lives in the settings worker: [`AxElement`] carries no href, so a
/// pure selector cannot see it — the worker resolves `node_href` lazily
/// per genuinely-new candidate instead.
#[must_use]
// `&HashSet<i64>` (not a hasher-generic) is the prescribed contract for
// this selector, matching the call sites' concrete sets.
#[allow(clippy::implicit_hasher)]
pub fn select_revealed_settings<'a>(
    elements: &'a [AxElement],
    clicked: &[ClickedControl],
    previously_seen: &std::collections::HashSet<i64>,
) -> Option<&'a AxElement> {
    if clicked.is_empty() {
        return None;
    }
    elements.iter().find(|element| {
        PAGE_GOAL_ROLES.contains(&element.role.as_str())
            && !already_clicked(clicked, element)
            && !previously_seen.contains(&element.backend_node_id)
            && mentions_settings_words(element)
    })
}

/// Miss diagnostic for the settings worker: what it actually tried,
/// never a dump of the controls it evaluated.
#[must_use]
pub fn settings_miss_diagnostic(tried: &[String]) -> String {
    if tried.is_empty() {
        "settings: no settings control revealed from the account menu".to_owned()
    } else {
        format!(
            "settings: settings destination not reached. Tried: [{}]",
            tried.join("; ")
        )
    }
}

/// Async browser seam for the settings worker
/// ([`pursue_settings_chrome`]): the CDP operations the shared menu
/// primitive ([`MenuBrowser`]) does not cover — reading the live URL to
/// detect post-click navigation, page-revealed hrefs for the
/// blank-named-link case, and the document title for the
/// disclosure-without-navigation check. [`ManagedBrowser`] is the
/// production implementation; tests drive the worker against a scripted
/// fake, so the whole flow is proven with no Chromium.
pub trait SettingsBrowser: MenuBrowser {
    /// Current document URL; `None` fails the navigation check closed.
    fn settings_current_url(&self) -> impl std::future::Future<Output = Option<url::Url>> + Send;
    /// Best-effort `href` of the DOM node behind a backend node id;
    /// `None` on any failure or missing/empty href.
    fn settings_node_href(
        &self,
        backend_node_id: i64,
    ) -> impl std::future::Future<Output = Option<String>> + Send;
    /// Best-effort document title; `None` fails the title check closed.
    fn settings_page_title(&self) -> impl std::future::Future<Output = Option<String>> + Send;
}

impl SettingsBrowser for ManagedBrowser {
    async fn settings_current_url(&self) -> Option<url::Url> {
        self.current_url().await.ok().flatten()
    }

    async fn settings_node_href(&self, backend_node_id: i64) -> Option<String> {
        self.node_href(backend_node_id).await
    }

    async fn settings_page_title(&self) -> Option<String> {
        self.page_title().await
    }
}

/// Per-invocation click budget for the settings worker, mirroring
/// [`IDENTITY_MAX_CLICKS`]: the shared menu primitive retries internally,
/// so the loop counts every click — menu-opening or revealed-destination
/// — against one budget. A menu Miss ends the worker: the primitive
/// already re-snapshotted and re-ranked between attempts, so re-looping
/// would only re-examine candidates it exhausted.
const SETTINGS_MAX_CLICKS: usize = 3;

/// The href half of the revealed-settings matcher: a genuinely-new
/// actionable control whose page-revealed href resolves (via
/// [`validate_revealed_href`]) to a settings path. Same "revealed"
/// gating as [`select_revealed_settings`]; hrefs resolve lazily per
/// candidate — never for the whole snapshot.
async fn select_revealed_settings_href<'a, B: SettingsBrowser>(
    elements: &'a [AxElement],
    clicked: &[ClickedControl],
    previously_seen: &std::collections::HashSet<i64>,
    browser: &B,
    origin: &url::Url,
) -> Option<&'a AxElement> {
    if clicked.is_empty() {
        return None;
    }
    for element in elements {
        if !PAGE_GOAL_ROLES.contains(&element.role.as_str())
            || already_clicked(clicked, element)
            || previously_seen.contains(&element.backend_node_id)
        {
            continue;
        }
        let is_settings = browser
            .settings_node_href(element.backend_node_id)
            .await
            .as_deref()
            .and_then(|href| validate_revealed_href(href, origin))
            .is_some_and(|url| url_path_mentions_settings(&url));
        if is_settings {
            return Some(element);
        }
    }
    None
}

/// Post-click disclosure check: with no navigation, the click only
/// counts when the fresh page names settings in its title or a heading.
/// Generic words only — the same [`SETTINGS_WORDS`] vocabulary.
fn settings_surface_visible(elements: &[AxElement], title: Option<&str>) -> bool {
    if title.is_some_and(text_mentions_settings) {
        return true;
    }
    elements
        .iter()
        .any(|element| element.role == "heading" && mentions_settings_words(element))
}

/// Settings landing verifier: a navigation only counts when the landed
/// URL's path names a settings destination (stemmed segment match). A
/// click alone never yields success — an irrelevant landing is an honest
/// miss carrying the tried-lines journal.
fn verify_settings_landing(
    label: &str,
    landed: &url::Url,
    tried: &[String],
) -> Result<PageGoalOutcome, IntentError> {
    if url_path_mentions_settings(landed) {
        Ok(PageGoalOutcome::Navigated {
            label: label.to_owned(),
            landed: landed.clone(),
        })
    } else {
        Err(IntentError::NoMatch(settings_miss_diagnostic(tried)))
    }
}

/// First-class settings worker, generic over [`SettingsBrowser`] so the
/// flow is provable against a scripted fake; [`pursue_settings_chrome`]
/// fixes this to [`ManagedBrowser`] for the wiring worker. Modeled on
/// [`pursue_identity_chrome`].
///
/// Deterministic only — no model phase:
///
/// 1. Untruncated AX snapshot via [`MenuBrowser::menu_snapshot`] (for
///    [`ManagedBrowser`] this is `ax_snapshot_untruncated`: a revealed
///    menu renders at the end of the document, past the 300-element head
///    truncation).
/// 2. A revealed settings destination ends the hunt — name-worded first,
///    then the blank-named link whose page-revealed href points at a
///    settings path. The control is clicked and the URL watched: a
///    navigation to a settings path is [`PageGoalOutcome::Navigated`]; a
///    disclosure with no navigation is [`PageGoalOutcome::Verified`]
///    when the fresh page's title or headings name settings.
/// 3. Otherwise the shared [`open_identity_menu`] primitive spends the
///    remaining click budget opening the account menu in a single call —
///    its internal rank → click → poll loop is not reimplemented here,
///    and a Miss ends the worker instead of re-looping over candidates
///    it already exhausted.
///
/// An honest miss beats a guessed click: the worker only clicks controls
/// the revealed-gating selected, and [`IntentError::NoMatch`] carries the
/// tried-lines journal.
///
/// # Errors
///
/// Returns [`IntentError::NoMatch`] with the tried-lines journal when no
/// settings destination is reached, and [`IntentError::Browser`] on CDP
/// failure.
pub async fn pursue_settings_chrome_inner<B: SettingsBrowser>(
    browser: &B,
    origin: &url::Url,
) -> Result<PageGoalOutcome, IntentError> {
    let mut clicked: Vec<ClickedControl> = Vec::new();
    let mut tried: Vec<String> = Vec::new();
    // Node ids from the previous iteration's snapshot — same "revealed"
    // semantics as the identity worker: a destination must be genuinely
    // new since the menu opened, never chrome that was already there.
    let mut previously_seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut clicks_used: usize = 0;

    while clicks_used < SETTINGS_MAX_CLICKS {
        let (elements, check_before, _) = browser.menu_snapshot(origin).await;
        let currently_seen: std::collections::HashSet<i64> = elements
            .iter()
            .map(|element| element.backend_node_id)
            .collect();

        // 1. A revealed settings destination ends the hunt — but only
        // after the worker opened something, and only when the candidate
        // actually appeared after that opening.
        let target = match select_revealed_settings(&elements, &clicked, &previously_seen) {
            Some(target) => Some(target),
            None => {
                select_revealed_settings_href(
                    &elements,
                    &clicked,
                    &previously_seen,
                    browser,
                    origin,
                )
                .await
            }
        };
        if let Some(target) = target {
            let label = if target.name.trim().is_empty() {
                let description = target.description.trim();
                if description.is_empty() {
                    "(settings link)".to_owned()
                } else {
                    description.to_owned()
                }
            } else {
                target.name.clone()
            };
            browser.menu_click(target).await?;
            clicked.push(ClickedControl::of(target));
            clicks_used += 1;
            tried.push(tried_label(target, "clicked, watching URL"));
            if let Some(landed) = wait_for_url_change(browser).await {
                return verify_settings_landing(&label, &landed, &tried);
            }
            // No navigation: the control acted like a disclosure. The
            // settings surface must name itself in the fresh page's
            // title or headings, or the click is not evidence.
            let (fresh, _, _) = browser.menu_snapshot(origin).await;
            let title = browser.settings_page_title().await;
            if settings_surface_visible(&fresh, title.as_deref()) {
                let landed = browser
                    .settings_current_url()
                    .await
                    .unwrap_or_else(|| origin.clone());
                return Ok(PageGoalOutcome::Verified {
                    label,
                    landed,
                    username: None,
                });
            }
            tried.push(tried_label(target, "clicked, no settings evidence"));
            previously_seen = currently_seen;
            continue;
        }

        // 2. No revealed destination: spend the remaining budget on the
        // shared menu primitive in a single call — its internal
        // click → poll-for-evidence → re-rank loop supersedes the old
        // one-attempt-per-iteration shape, and a Miss means its
        // candidates are exhausted against fresh snapshots, so the
        // worker stops instead of re-looping.
        let baseline = MenuOpenBaseline {
            elements: &elements,
            check: &check_before,
        };
        let clicks_before = clicked.len();
        let opened = match open_identity_menu(
            browser,
            origin,
            baseline,
            &mut clicked,
            (SETTINGS_MAX_CLICKS - clicks_used).min(MENU_OPEN_MAX_TRIES),
        )
        .await?
        {
            OpenMenuOutcome::Opened { tried: line, .. } => {
                tried.push(line);
                true
            }
            OpenMenuOutcome::Miss { tried: lines } => {
                tried.extend(lines);
                false
            }
        };
        // The primitive records every attempt in `clicked`: the spend
        // counts whether or not a menu opened.
        clicks_used += clicked.len() - clicks_before;
        if !opened {
            break;
        }
        previously_seen = currently_seen;
    }
    Err(IntentError::NoMatch(settings_miss_diagnostic(&tried)))
}

/// First-class settings worker: pursue the settings destination through
/// the account menu — open it with the shared menu primitive, click the
/// revealed settings control, verify the landing generically. See
/// [`pursue_settings_chrome_inner`] for the flow.
///
/// # Errors
///
/// Returns [`IntentError::NoMatch`] with the tried-lines journal when no
/// settings destination is reached, and [`IntentError::Browser`] on CDP
/// failure.
pub async fn pursue_settings_chrome(
    browser: &ManagedBrowser,
    origin: &url::Url,
) -> Result<PageGoalOutcome, IntentError> {
    pursue_settings_chrome_inner(browser, origin).await
}

/// Count actionable controls in a snapshot: the click-effect check
/// compares this before and after a disclosure click.
fn count_actionable(elements: &[AxElement]) -> usize {
    elements
        .iter()
        .filter(|element| PAGE_GOAL_ROLES.contains(&element.role.as_str()))
        .count()
}

/// Validate a page-revealed href in Rust before navigating: absolute
/// `https`, same site as the portal (modulo a `www.` prefix), no
/// credentials, non-root path. Relative hrefs resolve against the
/// origin. A rejection is never an error — the caller falls back to
/// clicking the control itself.
#[must_use]
pub fn validate_revealed_href(href: &str, origin: &url::Url) -> Option<url::Url> {
    let href = href.trim();
    if href.is_empty() {
        return None;
    }
    // Reject non-navigable schemes before parsing: `javascript:`,
    // `data:`, `mailto:`, and bare fragments never become destinations.
    let url = if href.starts_with('/') || !href.contains(':') {
        origin.join(href).ok()?
    } else {
        url::Url::parse(href).ok()?
    };
    if url.scheme() != "https" {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = url.host_str()?.to_lowercase();
    let origin_host = origin.host_str()?.to_lowercase();
    if !same_site_host(&host, &origin_host) {
        return None;
    }
    if url.path() == "/" || url.path().is_empty() {
        return None;
    }
    Some(url)
}

/// Same-site host comparison: equal after lowercasing and stripping a
/// `www.` prefix. Conservative on purpose — `old.reddit.com` is not
/// `www.reddit.com`, and a mismatch fails the candidate closed, never
/// the run.
#[must_use]
pub fn same_site_host(host: &str, origin_host: &str) -> bool {
    fn normalize(h: &str) -> String {
        let lower = h.to_lowercase();
        lower.strip_prefix("www.").unwrap_or(&lower).to_owned()
    }
    normalize(host) == normalize(origin_host)
}

/// Username from a revealed profile URL: the last non-empty path
/// segment. Generic — whatever the site puts last is treated as the
/// username and must reappear in the verified landing URL.
#[must_use]
pub fn username_from_href(url: &url::Url) -> Option<String> {
    url.path_segments()?
        .rfind(|segment| !segment.is_empty())
        .map(str::to_owned)
}

/// Username from menu text: `u/name`, `U/name`, or `@name` handle shapes.
/// Single token only — a sentence that happens to start with `u/` is not
/// a username. Evidence only: a match strengthens verification (the
/// landing path must contain the handle); a miss falls back to the
/// weaker account-word checks, never to a guessed destination.
#[must_use]
pub fn username_from_menu_text(text: &str) -> Option<String> {
    let text = text.trim();
    for prefix in ["u/", "U/", "@"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            let rest = rest.trim();
            if !rest.is_empty() && !rest.chars().any(char::is_whitespace) {
                return Some(rest.to_owned());
            }
        }
    }
    None
}

/// Verify an account-home landing. The URL must be same-site and non-root
/// (a click that went nowhere verifies nothing); when the page revealed a
/// username, the landed path must carry it. Reads the live current URL —
/// the navigation target is evidence, not proof.
/// Closed general vocabulary for account-home URL paths: a landing
/// whose path names the account area is evidence even without a
/// username. General words, never site routes.
const ACCOUNT_PATH_WORDS: &[&str] = &["profile", "account", "settings", "user", "me"];

/// A URL path names the account area when one of its segments is an
/// account word (`/user/name/`, `/settings`).
#[must_use]
pub fn path_names_account(path: &str) -> bool {
    path.split('/')
        .any(|segment| ACCOUNT_PATH_WORDS.contains(&segment.to_lowercase().as_str()))
}

/// Profile/account evidence in a control label: the trigger that led to
/// the landing was itself profile-worded.
#[must_use]
pub fn label_names_profile(label: &str) -> bool {
    let lower = label.to_lowercase();
    lower.contains("profile") || lower.contains("account")
}

/// Pure account-home verification decision: given the live current URL,
/// the portal origin, the trigger label, and an optional page-revealed
/// username, decide whether the landing is the user's account home. The
/// browser read stays in [`verify_account_home`]; this is the decision
/// integration tests pin down.
///
/// # Errors
///
/// Returns the miss diagnostic when the landing is not verifiably the
/// user's account home (wrong site, root page, username mismatch, or no
/// account evidence without a username).
pub fn verify_account_landing(
    current: &url::Url,
    origin: &url::Url,
    label: &str,
    username: Option<&str>,
) -> Result<PageGoalOutcome, String> {
    let origin_host = origin.host_str().unwrap_or("").to_lowercase();
    let current_host = current.host_str().unwrap_or("").to_lowercase();
    let same_site = same_site_host(&current_host, &origin_host);
    let non_root = current.path() != "/" && !current.path().is_empty();
    let path = current.path().to_lowercase();
    let accepted = match username {
        // Strongest: the page-revealed username must reappear in the URL.
        Some(name) => same_site && non_root && path.contains(&name.to_lowercase()),
        // Without a username, same-site non-root alone is not enough:
        // the path must name the account area, or the trigger control
        // must have been profile/account-worded. Arbitrary navigation
        // fails the verification, never the run.
        None => {
            same_site
                && non_root
                && (path_names_account(current.path()) || label_names_profile(label))
        }
    };
    if accepted {
        Ok(PageGoalOutcome::Verified {
            label: label.to_owned(),
            landed: current.clone(),
            username: username.map(str::to_owned),
        })
    } else {
        Err(format!(
            "account-home landing failed verification: at {current} (label '{label}')"
        ))
    }
}

async fn verify_account_home(
    browser: &ManagedBrowser,
    origin: &url::Url,
    label: &str,
    landed: &url::Url,
    username: Option<&str>,
) -> Result<PageGoalOutcome, IntentError> {
    // The live page is the truth: re-read the URL after the navigation
    // and verify that, not the URL we asked for.
    let current = browser
        .current_url()
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| landed.clone());
    verify_account_landing(&current, origin, label, username).map_err(IntentError::NoMatch)
}

/// One tried-click log line: role, truncated name, observed effect.
#[must_use]
pub fn tried_label(element: &AxElement, effect: &str) -> String {
    let name: String = element.name.chars().take(40).collect();
    format!("{} '{name}' → {effect}", element.role)
}

/// Miss diagnostic for the account-home worker: what it actually tried,
/// never a dump of the controls it evaluated.
#[must_use]
pub fn identity_miss_diagnostic(tried: &[String]) -> String {
    if tried.is_empty() {
        "account-home: no identity control found in the header chrome".to_owned()
    } else {
        format!(
            "account-home: identity control not reached. Tried: [{}]",
            tried.join("; ")
        )
    }
}

/// Phase 1: deterministic observe-act loop. Evidence-only: every click
/// targets a control the live AX tree actually offered. Already-clicked
/// nodes are never re-clicked, so menus cannot be toggled shut by the loop
/// itself.
async fn pursue_deterministic(
    browser: &ManagedBrowser,
    origin: &url::Url,
    noun: &str,
) -> Result<PageGoalOutcome, IntentError> {
    let mut clicked: Vec<ClickedControl> = Vec::new();
    for _ in 0..PAGE_GOAL_MAX_STEPS {
        // Untruncated: a revealed menu renders at document end (React
        // portal), past the 300-element head the capped snapshot keeps —
        // "open the Settings on reddit" failed intermittently because the
        // menu item was invisible to this selector.
        let (elements, check_before, _) = browser.ax_snapshot_untruncated(origin).await;
        // 1. Direct hit: actionable control mentioning the noun.
        if let Some(target) = select_page_control(&elements, noun, &clicked) {
            let label = target.name.clone();
            let tried_key = ClickedControl::of(target);
            click_element(browser, target).await?;
            clicked.push(tried_key);
            if let Some(landed) = wait_for_url_change(browser).await {
                return Ok(PageGoalOutcome::Navigated { label, landed });
            }
            // No navigation: a menu or popover may have opened. Loop and
            // re-snapshot against the new tree.
            continue;
        }
        // 2. No direct hit: reveal more controls via the shared menu
        // primitive (landmarked → account-worded → unlabeled-in-header →
        // rightmost-in-header, each click effect-verified). The older
        // picks stay as fallbacks for candidates the primitive's try
        // budget didn't reach.
        let baseline = MenuOpenBaseline {
            elements: &elements,
            check: &check_before,
        };
        match open_identity_menu(browser, origin, baseline, &mut clicked, MENU_OPEN_MAX_TRIES)
            .await?
        {
            OpenMenuOutcome::Opened { .. } => continue,
            OpenMenuOutcome::Miss { .. } => {}
        }
        if let Some(menu) = select_menu_button(&elements, &clicked) {
            let tried_key = ClickedControl::of(menu);
            click_element(browser, menu).await?;
            clicked.push(tried_key);
            continue;
        }
        if let Some(rightmost) = select_rightmost_button(browser, &elements, &clicked).await {
            let tried_key = ClickedControl::of(rightmost);
            click_element(browser, rightmost).await?;
            clicked.push(tried_key);
            continue;
        }
        // 3. Nothing left to try.
        return Err(IntentError::NoMatch(page_goal_diagnostic(&elements, noun)));
    }
    Err(IntentError::NoMatch(format!(
        "In-page goal '{noun}' not reached after {PAGE_GOAL_MAX_STEPS} steps."
    )))
}

/// Phase 2: model-guided observe-act loop. The navigator sees the goal plus
/// the snapshot's actionable elements and picks one [`PageAction`]; the
/// pick is validated against the snapshot before anything clicks.
async fn pursue_with_model(
    browser: &ManagedBrowser,
    origin: &url::Url,
    goal: &str,
    navigator: std::sync::Arc<dyn crate::navigator::PageNavigator>,
    deterministic_miss: String,
) -> Result<PageGoalOutcome, IntentError> {
    use crate::navigator::PageAction;
    for _ in 0..MODEL_GOAL_MAX_STEPS {
        let (elements, _, _) = browser.ax_snapshot(origin).await;
        // The navigator is synchronous (one bounded HTTP call); the async
        // runtime never blocks on it. Everything the closure touches is
        // owned, so the future stays `'static`.
        //
        // Zone the same head slice the navigator renders, so `zones[i]`
        // describes rendered line `i`. Best-effort: unzoned on failure.
        let goal_owned = goal.to_owned();
        let owned_elements: Vec<AxElement> = elements
            .iter()
            .take(crate::MAX_NAVIGATOR_ELEMENTS)
            .cloned()
            .collect();
        let zones = position_zones(browser, &owned_elements).await;
        let owned_navigator = navigator.clone();
        let action = tokio::task::spawn_blocking(move || {
            owned_navigator.next_action_zoned(&goal_owned, &owned_elements, &zones)
        })
        .await
        .map_err(|_| {
            IntentError::NoMatch(format!("navigator task failed; {deterministic_miss}"))
        })?;
        match action {
            None => {
                return Err(IntentError::NoMatch(format!(
                    "navigator declined; {deterministic_miss}"
                )));
            }
            Some(PageAction::GiveUp { reason }) => {
                return Err(IntentError::NoMatch(format!(
                    "navigator gave up ({reason}); {deterministic_miss}"
                )));
            }
            Some(PageAction::Done) => {
                let landed = browser
                    .current_url()
                    .await?
                    .ok_or(browser_driver::BrowserError::WrongOrigin)?;
                return Ok(PageGoalOutcome::AlreadyThere { landed });
            }
            Some(PageAction::Click { target }) => {
                let element = elements
                    .iter()
                    .find(|element| element.backend_node_id == target)
                    .ok_or_else(|| {
                        IntentError::NoMatch(format!("navigator picked unknown element {target}"))
                    })?;
                let label = element.name.clone();
                click_element(browser, element).await?;
                if let Some(landed) = wait_for_url_change(browser).await {
                    return Ok(PageGoalOutcome::Navigated { label, landed });
                }
            }
        }
    }
    Err(IntentError::NoMatch(format!(
        "navigator exhausted {MODEL_GOAL_MAX_STEPS} steps; {deterministic_miss}"
    )))
}

/// Wait for the live page's URL to change from what it is now: shared by
/// both pursuit phases after every click.
async fn wait_for_url_change<B: SettingsBrowser>(browser: &B) -> Option<url::Url> {
    let from = browser.settings_current_url().await?;
    wait_for_navigation_with(
        || async { browser.settings_current_url().await },
        &from,
        std::time::Duration::from_millis(FOLLOW_POLL_MS),
        std::time::Duration::from_millis(FOLLOW_TIMEOUT_MS),
    )
    .await
}

/// First actionable control mentioning `noun`, in snapshot document order,
/// skipping nodes the loop already clicked. Page chrome is deliberately
/// NOT excluded here (unlike search results): on a portal page the header
/// is exactly where profile and settings live.
#[must_use]
pub fn select_page_control<'a>(
    elements: &'a [AxElement],
    noun: &str,
    clicked: &[ClickedControl],
) -> Option<&'a AxElement> {
    if noun.trim().is_empty() {
        return None;
    }
    elements.iter().find(|element| {
        PAGE_GOAL_ROLES.contains(&element.role.as_str())
            && !already_clicked(clicked, element)
            && mentions_noun(element, noun)
    })
}

/// One unopened menu/disclosure button in the page header, to reveal more
/// controls when the noun isn't directly visible. Three passes, cheapest
/// first: banner/navigation landmarks (avatars, user menus), then a closed
/// class of account-menu words for headers that carry no landmark (avatar
/// buttons named after the username, web-component headers), and — only in
/// the async [`select_rightmost_button`] — live geometry as a last resort.
/// Already-clicked nodes are skipped.
#[must_use]
pub fn select_menu_button<'a>(
    elements: &'a [AxElement],
    clicked: &[ClickedControl],
) -> Option<&'a AxElement> {
    elements
        .iter()
        .find(|element| {
            element.role == "button"
                && !already_clicked(clicked, element)
                && matches!(element.landmark.as_deref(), Some("banner" | "navigation"))
        })
        .or_else(|| {
            elements.iter().find(|element| {
                element.role == "button"
                    && !already_clicked(clicked, element)
                    && mentions_account_word(element)
            })
        })
}

/// Closed general vocabulary for account-menu disclosure buttons: `menu`,
/// `account`, `user`, `avatar`. Token-contains matching covers compounds
/// (`usermenu`, `u_someuser`) without matching across token boundaries.
/// General words, never site procedures.
const ACCOUNT_MENU_WORDS: &[&str] = &["menu", "account", "user", "avatar"];

fn mentions_account_word(element: &AxElement) -> bool {
    [&element.name, &element.description]
        .iter()
        .flat_map(|text| {
            normalize(text)
                .split(' ')
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .any(|token| ACCOUNT_MENU_WORDS.iter().any(|word| token.contains(word)))
}

/// Header strip as a fraction of viewport height: account controls live in
/// page headers, and headers live at the top. A button below the strip is
/// page content, never a header menu — fail closed, never click it.
const HEADER_STRIP_FRACTION: f64 = 0.25;

/// Pure ranking for the geometry fallback: largest `x` (rightmost) wins,
/// but only inside the header strip. Account controls cluster at the
/// right end of horizontal headers (chat, create, notifications, avatar
/// share one row), so rightmost beats topmost — the old topmost prior
/// picked an arbitrary header button on Reddit. Untestable CDP calls stay
/// outside in [`select_rightmost_button`]; the decision itself is
/// unit-tested.
#[must_use]
pub fn pick_rightmost(rects: &[(i64, f64, f64)], strip_bottom: f64) -> Option<i64> {
    rects
        .iter()
        .filter(|(_, _, y)| *y <= strip_bottom)
        .max_by(|(_, a, _), (_, b, _)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(id, _, _)| *id)
}

/// Last-resort header-menu pick for landmark-less, word-less headers
/// (avatar buttons named after the username, web-component headers with
/// no AX landmark). Measures every unclicked button via `DOM.getBoxModel`
/// and takes the rightmost one inside the header strip. Best-effort:
/// unreadable geometry or an unreadable viewport fails closed to `None`.
async fn select_rightmost_button<'a>(
    browser: &ManagedBrowser,
    elements: &'a [AxElement],
    clicked: &[ClickedControl],
) -> Option<&'a AxElement> {
    let (_, viewport_height) = browser.viewport_size().await?;
    let strip_bottom = viewport_height * HEADER_STRIP_FRACTION;
    let mut rects: Vec<(i64, f64, f64)> = Vec::new();
    for element in elements
        .iter()
        .filter(|element| element.role == "button" && !already_clicked(clicked, element))
    {
        // `node_rect` rejects degenerate (hidden) boxes, so invisible
        // controls never become candidates.
        if let Ok(highlight) = browser.node_rect(element.backend_node_id).await {
            rects.push((element.backend_node_id, highlight.x, highlight.y));
        }
    }
    let winner = pick_rightmost(&rects, strip_bottom)?;
    elements
        .iter()
        .find(|element| element.backend_node_id == winner)
}

/// Coarse position zones for the model phase: measure the head of the
/// snapshot (the same slice the navigator renders), then zone each point
/// against the measured bounding box. Best-effort and time-bounded —
///
/// geometry that won't resolve degrades to unzoned lines, never a stall.
async fn position_zones(
    browser: &ManagedBrowser,
    elements: &[AxElement],
) -> Vec<Option<crate::navigator::PositionZone>> {
    use crate::navigator::zone_for;
    const ZONE_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
    let head: Vec<i64> = elements
        .iter()
        .take(crate::navigator::MAX_NAVIGATOR_ELEMENTS)
        .map(|element| element.backend_node_id)
        .collect();
    let measured = tokio::time::timeout(ZONE_BUDGET, async {
        let mut points: Vec<(f64, f64)> = Vec::new();
        for id in &head {
            if let Ok(highlight) = browser.node_rect(*id).await {
                points.push((
                    highlight.x + highlight.width / 2.0,
                    highlight.y + highlight.height / 2.0,
                ));
            } else {
                points.push((f64::NAN, f64::NAN));
            }
        }
        points
    })
    .await
    .ok();
    let Some(points) = measured else {
        return vec![None; head.len()];
    };
    let finite: Vec<(f64, f64)> = points
        .iter()
        .copied()
        .filter(|(x, y)| x.is_finite() && y.is_finite())
        .collect();
    if finite.is_empty() {
        return vec![None; head.len()];
    }
    let (min_x, max_x) = finite
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), (x, _)| {
            (lo.min(*x), hi.max(*x))
        });
    let (min_y, max_y) = finite
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), (_, y)| {
            (lo.min(*y), hi.max(*y))
        });
    points
        .iter()
        .map(|(x, y)| {
            if x.is_finite() && y.is_finite() {
                Some(zone_for(*x, *y, (min_x, min_y, max_x, max_y)))
            } else {
                None
            }
        })
        .collect()
}

/// Diagnostic for a failed in-page pursuit: the noun plus every actionable
/// control the page offered, so the miss reads as evidence.
#[must_use]
pub fn page_goal_diagnostic(elements: &[AxElement], noun: &str) -> String {
    let mut rendered: Vec<String> = Vec::new();
    let mut controls = 0_usize;
    for element in elements
        .iter()
        .filter(|element| PAGE_GOAL_ROLES.contains(&element.role.as_str()))
    {
        controls += 1;
        if rendered.len() >= MAX_DIAGNOSTIC_CANDIDATES {
            continue;
        }
        let text: String = element.name.chars().take(MAX_DIAGNOSTIC_TEXT_LEN).collect();
        rendered.push(format!("'{}' [{}]", text, element.role));
    }
    let hidden = controls.saturating_sub(rendered.len());
    if hidden > 0 {
        rendered.push(format!("… and {hidden} more"));
    }
    format!(
        "In-page goal '{noun}' found no match. Evaluated {controls} controls: [{}]",
        rendered.join(", ")
    )
}

/// Pure route-mismatch decision for entry pre-conditions: literal URL
/// inequality, so any drift — path, query, or trailing-slash normalization
/// aside — navigates rather than grounding on the wrong page.
#[must_use]
pub fn entry_url_mismatched(current: &url::Url, entry: &url::Url) -> bool {
    current != entry
}

/// Enforce the navigation pre-condition before grounding: when the live page
/// differs from the intent's entry URL, navigate there first (`goto` awaits
/// page load), then refresh the driver target handle against the settled
/// top-level page session so post-swap CDP domains are re-armed before any
/// snapshot. An unreadable current URL also navigates — landing on known
/// state is the safe move. Same-portal moves keep origin checks green;
/// anything else fails closed on the subsequent check. Returns whether
/// navigation fired.
///
/// # Errors
/// Returns [`IntentError::Browser`] on current-URL or navigation failures.
pub async fn ensure_at_entry_url(
    browser: &ManagedBrowser,
    entry: &url::Url,
) -> Result<bool, IntentError> {
    let current = browser.current_url().await?;
    if current
        .as_ref()
        .is_some_and(|url| !entry_url_mismatched(url, entry))
    {
        return Ok(false);
    }
    browser.navigate(entry).await?;
    browser.refresh_target_session().await;
    Ok(true)
}

/// Move to the intent's entry URL when one is stated and mismatched.
/// Intents without an entry skip the extra round-trip entirely.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] for a malformed entry URL and
/// [`IntentError::Browser`] on navigation failures.
async fn ensure_entry(
    browser: &ManagedBrowser,
    intent: &SemanticIntent,
) -> Result<(), IntentError> {
    let Some(entry) = intent.entry_url.as_deref() else {
        return Ok(());
    };
    let entry = url::Url::parse(entry)
        .map_err(|_| IntentError::NoMatch(format!("invalid entry_url: '{entry}'")))?;
    ensure_at_entry_url(browser, &entry).await?;
    Ok(())
}

/// Content-settle cadence for post-navigation snapshots: poll the live page
/// every 250 ms, for at most 5000 ms, before the official ARIA snapshot.
/// Async tables (e.g. GitHub Payment History) render rows after `load`, so
/// snapshotting immediately yields zero candidates; explicit state polling
/// replaces fixed sleeps and blind re-navigation retries.
pub const SETTLE_POLL_MS: u64 = 250;
/// Upper bound on one settle wait. Expiry is not an error — the snapshot
/// still runs, so slow pages degrade to previous behavior instead of failing.
pub const SETTLE_TIMEOUT_MS: u64 = 5000;

/// Probe text for settle polling: the primary target noun when the intent
/// carries one, else the label query. Both are prompt-derived, so readiness
/// means "the content the user asked about rendered".
#[must_use]
pub fn settle_probe_text(intent: &SemanticIntent) -> &str {
    intent
        .primary_target_noun
        .as_deref()
        .filter(|noun| !noun.trim().is_empty())
        .unwrap_or(&intent.label_query)
}

/// Noun [`select_search_result`] should match on for one intent.
///
/// Stage 2 picks a *site*, while the batch gate picks *artifacts inside a
/// site* — two different words whenever the prompt named both. The prompt's
/// grammar decides which is available:
///
/// * `download all my invoices from github` parses a site complement, so
///   `site_context` is `github` and Stage 2 follows the GitHub result. The
///   artifact (`invoice`) stays on the intent, where [`resolve_batch`] gates
///   with it once the destination has loaded.
/// * `find amazon` carries no complement, so the direct object is
///   itself the destination and this falls back to
///   [`settle_probe_text`] — the intent's target noun. (Direct opens like
///   `open amazon for me` never reach search-and-follow; the ladder grounds
///   them or misses.)
///
/// Blank or missing site contexts fall back rather than failing: an empty
/// noun makes [`select_search_result`] return `None`, and losing a
/// followable result to whitespace would be a worse answer than the noun
/// the settle loop already trusts.
#[must_use]
pub fn search_follow_noun<'a>(
    intent: &'a SemanticIntent,
    site_context: Option<&'a str>,
) -> &'a str {
    site_context
        .map(str::trim)
        .filter(|site| !site.is_empty())
        .unwrap_or_else(|| settle_probe_text(intent))
}

/// Poll `snapshot` until its tree holds at least one valid target
/// candidate for `intent`, or `timeout` elapses. Readiness means
/// [`resolve_batch`] yields a [`ResolveOutcome::BatchMatch`] — a static
/// column header (`<th>Invoice</th>`) never satisfies it, only actionable
/// row candidates do, so body-text matches cannot end the wait early. The
/// first snapshot runs immediately, so already-settled pages pay one check
/// and no sleep. Returns `true` when candidates landed, `false` on
/// timeout. Snapshot errors count as not-ready (transient mid-navigation
/// states) and keep polling within the same bound. Pure polling policy
/// over an injected snapshot provider, so delayed-render fixtures can
/// prove the loop hermetically without a browser.
async fn wait_for_candidates_with<F, Fut>(
    mut snapshot: F,
    intent: &SemanticIntent,
    poll_interval: std::time::Duration,
    timeout: std::time::Duration,
) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<Vec<AxElement>>>,
{
    let started = std::time::Instant::now();
    loop {
        let settled = snapshot().await.is_some_and(|elements| {
            matches!(
                resolve_batch(&elements, intent),
                ResolveOutcome::BatchMatch(_)
            )
        });
        if settled {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// Wait for the intent's row candidates to render after navigation.
/// Fail-open by design: timeouts and transient snapshot errors both fall
/// through to the official snapshot, so unsettled pages behave exactly as
/// before instead of failing. Returns whether candidates were observed;
/// callers snapshot either way.
pub async fn wait_for_settled_candidates(
    browser: &ManagedBrowser,
    origin: &url::Url,
    intent: &SemanticIntent,
) -> bool {
    wait_for_candidates_with(
        || async { Some(browser.ax_snapshot(origin).await.0) },
        intent,
        std::time::Duration::from_millis(SETTLE_POLL_MS),
        std::time::Duration::from_millis(SETTLE_TIMEOUT_MS),
    )
    .await
}

/// Execute one intent: settle, snapshot, resolve, badge, click. The badge stays
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
    ensure_entry(browser, intent).await?;
    browser.check_anchored_origin(origin).await?;
    wait_for_settled_candidates(browser, origin, intent).await;
    let (elements, _, _) = browser.ax_snapshot(origin).await;
    let resolved = resolve_intent(&elements, intent)
        .ok_or_else(|| IntentError::NoMatch(grounding_diagnostic(&elements, intent)))?;
    click_element(browser, &resolved.element).await
}

/// Batch terminal states: every click landed, or the run stopped early at
/// the first page that drifted off the entry route. Partial progress is
/// data, not an error — the caller maps it to its own outcome shape.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecuteOutcome {
    Completed(Vec<IntentOutcome>),
    HaltedEarly {
        reason: &'static str,
        clicks_completed: usize,
        failed_candidate_index: usize,
        failed_candidate_label: String,
        diverged_url: String,
    },
}

/// Whether the live page left the batch route: origin or path differ.
/// Query strings and hash fragments never count as drift, and `blob:` /
/// `data:` targets (download handoffs) are exempt — the next click then
/// fails naturally on its own rect instead.
fn url_drifted(initial: &url::Url, now: &url::Url) -> bool {
    if matches!(now.scheme(), "blob" | "data") {
        return false;
    }
    initial.scheme() != now.scheme()
        || initial.host_str() != now.host_str()
        || initial.port_or_known_default() != now.port_or_known_default()
        || initial.path() != now.path()
}

/// Build the halt payload for a drifted batch: completed count, failing
/// index, the control's visible label, and the diverged URL (kept
/// in-memory for recovery UI — never logged, since URLs can carry tokens).
fn halted_early(
    clicks_completed: usize,
    index: usize,
    element: &AxElement,
    diverged: &url::Url,
) -> ExecuteOutcome {
    ExecuteOutcome::HaltedEarly {
        reason: "UrlDriftDetected",
        clicks_completed,
        failed_candidate_index: index,
        failed_candidate_label: element.name.clone(),
        diverged_url: diverged.as_str().to_owned(),
    }
}

/// Refusal carried by both halves of the batch path when an intent is not
/// plural: batch-clicking a single-target intent would act on controls the
/// user never asked for. Shared so previewing and executing fail closed
/// with the same words.
const NOT_PLURAL_REFUSAL: &str = "plural execution requires is_plural; refusing batch click";

/// Resolve every control a plural intent would act on, without touching
/// any of them: entry navigation, origin check, settle poll, snapshot,
/// collect. This is the read-only half of [`execute_batch`], split out so a
/// consent gate can name the exact candidates — count and labels — before
/// the first CDP click happens.
///
/// Navigation is the one side effect: an intent carrying an `entry_url`
/// still moves the target there, exactly as execution would, because
/// candidates that were never rendered cannot be previewed.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] when the intent is not plural, nothing
/// resolves, or the intent is malformed, and [`IntentError::Browser`] on
/// CDP failure.
pub async fn preview_batch(
    browser: &ManagedBrowser,
    origin: &url::Url,
    intent: &SemanticIntent,
) -> Result<Vec<AxElement>, IntentError> {
    if !intent.is_plural {
        return Err(IntentError::NoMatch(NOT_PLURAL_REFUSAL.to_owned()));
    }
    ensure_entry(browser, intent).await?;
    browser.check_anchored_origin(origin).await?;
    wait_for_settled_candidates(browser, origin, intent).await;
    let (elements, _, _) = browser.ax_snapshot(origin).await;
    match resolve_batch(&elements, intent) {
        ResolveOutcome::BatchMatch(batch) => Ok(batch),
        _ => Err(IntentError::NoMatch(grounding_diagnostic(
            &elements, intent,
        ))),
    }
}

/// Execute a plural intent: resolve once via [`preview_batch`], then badge
/// and click every collected candidate in document order with a settling
/// pause between actions. Refusing non-plural intents fail-closed.
///
/// Before every click after the first, the live URL is compared against
/// the route held at batch start: origin or path drift aborts the rest
/// immediately with [`ExecuteOutcome::HaltedEarly`] instead of acting on a
/// foreign page. Unreadable URLs fail closed the same way a CDP failure
/// does.
///
/// Callers that previewed first resolve twice by design: the second
/// resolution is the one that clicks, so a page that changed during the
/// approval wait is re-grounded rather than acted on through stale node
/// handles.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] when the intent is not plural, nothing
/// resolves, or the intent is malformed, and [`IntentError::Browser`] on
/// CDP failure (marks cleared, like the single path).
pub async fn execute_batch(
    browser: &ManagedBrowser,
    origin: &url::Url,
    intent: &SemanticIntent,
) -> Result<ExecuteOutcome, IntentError> {
    let batch = preview_batch(browser, origin, intent).await?;
    click_batch(browser, &batch).await
}

/// Badge and click a resolved batch in document order, halting on route
/// drift. Separated from resolution so the ordering contract — every click
/// preceded by a route check, a settle pause between clicks, none after the
/// last — lives in one place.
///
/// # Errors
/// Returns [`IntentError::Browser`] on CDP failure.
async fn click_batch(
    browser: &ManagedBrowser,
    batch: &[AxElement],
) -> Result<ExecuteOutcome, IntentError> {
    let initial = browser
        .current_url()
        .await?
        .ok_or(browser_driver::BrowserError::WrongOrigin)?;
    let mut outcomes = Vec::with_capacity(batch.len());
    for (index, element) in batch.iter().enumerate() {
        if index > 0 {
            let now = browser
                .current_url()
                .await?
                .ok_or(browser_driver::BrowserError::WrongOrigin)?;
            if url_drifted(&initial, &now) {
                return Ok(halted_early(outcomes.len(), index, element, &now));
            }
        }
        outcomes.push(click_element(browser, element).await?);
        if outcomes.len() < batch.len() {
            tokio::time::sleep(std::time::Duration::from_millis(BATCH_SETTLE_MS)).await;
        }
    }
    Ok(ExecuteOutcome::Completed(outcomes))
}

/// Badge one resolved element and click it: rect resolution, visible mark,
/// press. Shared by single and batch paths so both act identically; the
/// badge stays visible on success as evidence, failures clear it so no
/// stale overlay survives.
///
/// # Errors
/// Returns [`IntentError::Browser`] on CDP failure.
async fn click_element(
    browser: &ManagedBrowser,
    element: &AxElement,
) -> Result<IntentOutcome, IntentError> {
    let highlight = browser.node_rect(element.backend_node_id).await?;
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
            landmark: None,
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
            is_plural: false,
            entry_url: None,
            primary_target_noun: None,
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
            is_plural: false,
            entry_url: None,
            primary_target_noun: None,
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
            is_plural: false,
            entry_url: None,
            primary_target_noun: None,
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
            is_plural: false,
            entry_url: None,
            primary_target_noun: None,
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
            is_plural: false,
            entry_url: None,
            primary_target_noun: None,
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
            is_plural: false,
            entry_url: None,
            primary_target_noun: None,
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
            is_plural: false,
            entry_url: None,
            primary_target_noun: None,
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
            is_plural: false,
            entry_url: None,
            primary_target_noun: None,
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
    fn batch_execution_collects_and_returns_all_threshold_matches() {
        // Three identical controls, one plural intent: all three come back
        // in document order, capped rather than vetoed. A non-plural intent
        // refuses the batch path fail-closed.
        let rows = vec![
            AxElement {
                backend_node_id: 131,
                ..element("button", "Download")
            },
            AxElement {
                backend_node_id: 132,
                ..element("button", "Download")
            },
            AxElement {
                backend_node_id: 133,
                ..element("button", "Download")
            },
        ];
        let mut plural = intent("button", "download");
        plural.is_plural = true;
        let ResolveOutcome::BatchMatch(batch) = resolve_batch(&rows, &plural) else {
            panic!("plural intent batches every match")
        };
        assert_eq!(
            batch
                .iter()
                .map(|element| element.backend_node_id)
                .collect::<Vec<_>>(),
            vec![131, 132, 133]
        );
        assert!(batch.len() <= MAX_BATCH_CLICKS);
        let single = intent("button", "download");
        assert!(matches!(
            resolve_batch(&[], &single),
            ResolveOutcome::NoMatch(_)
        ));
    }

    #[test]
    fn entry_url_triggers_navigation_if_route_mismatched() -> Result<(), Box<dyn std::error::Error>>
    {
        // Pure route decision: identical URLs stay put, any drift navigates.
        // Live firing (`goto` + load wait) needs Chromium, so the async
        // helper stays compile-checked while this locks the contract.
        let here = url::Url::parse("https://portal.example.com/invoices")?;
        let same = url::Url::parse("https://portal.example.com/invoices")?;
        let away = url::Url::parse("https://portal.example.com/settings")?;
        assert!(!entry_url_mismatched(&here, &same));
        assert!(entry_url_mismatched(&here, &away));
        Ok(())
    }

    #[test]
    fn resolve_batch_ignores_sidebar_navigation_links() {
        // Three table-row controls plus three sidebar links with identical
        // labels: the batch must hold exactly the data rows. Sidebar links
        // carry container text too, so only the landmark — never emptiness —
        // may exclude them.
        let table = ["INV-001", "INV-002", "INV-003"];
        let mut elements = Vec::new();
        for (index, id) in table.iter().enumerate() {
            elements.push(AxElement {
                backend_node_id: i64::try_from(index + 1).unwrap_or(1),
                container_text: vec![(*id).into()],
                landmark: None,
                ..element("link", "Download")
            });
        }
        for (index, name) in ["Docs", "Billing", "Home"].iter().enumerate() {
            elements.push(AxElement {
                backend_node_id: i64::try_from(index + 11).unwrap_or(11),
                container_text: vec!["Primary".into(), (*name).into()],
                landmark: Some("navigation".into()),
                ..element("link", "Download")
            });
        }
        let mut plural = intent("link", "download");
        plural.is_plural = true;
        let ResolveOutcome::BatchMatch(batch) = resolve_batch(&elements, &plural) else {
            panic!("table rows batch together")
        };
        assert_eq!(
            batch
                .iter()
                .map(|element| element.backend_node_id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn drift_guard_allows_query_param_and_blob_changes() -> Result<(), Box<dyn std::error::Error>> {
        let entry = url::Url::parse("https://portal.example.com/invoices")?;
        // Query strings, hash fragments, and trailing-slash normalization
        // are not drift.
        for same in [
            "https://portal.example.com/invoices?sort=date",
            "https://portal.example.com/invoices#row-3",
            "https://portal.example.com/invoices?sort=date#row-3",
        ] {
            assert!(!url_drifted(&entry, &url::Url::parse(same)?), "{same}");
        }
        // Download handoffs never count as drift either.
        assert!(!url_drifted(
            &entry,
            &url::Url::parse("blob:https://portal.example.com/1")?
        ));
        // Origin and path changes do.
        for moved in [
            "https://portal.example.com/settings",
            "https://other.example.com/invoices",
            "http://portal.example.com/invoices",
        ] {
            assert!(url_drifted(&entry, &url::Url::parse(moved)?), "{moved}");
        }
        Ok(())
    }

    #[test]
    fn drift_guard_halts_and_reports_failed_candidate_details_on_path_change()
    -> Result<(), Box<dyn std::error::Error>> {
        // The halt payload names the failing control and the diverged page;
        // the live loop that builds it stays behind Chromium-gated try paths.
        let diverged = url::Url::parse("https://portal.example.com/settings")?;
        let outcome = halted_early(2, 2, &element("button", "Download"), &diverged);
        let ExecuteOutcome::HaltedEarly {
            reason,
            clicks_completed,
            failed_candidate_index,
            failed_candidate_label,
            diverged_url,
        } = outcome
        else {
            panic!("halt payload builds");
        };
        assert_eq!(reason, "UrlDriftDetected");
        assert_eq!(clicks_completed, 2);
        assert_eq!(failed_candidate_index, 2);
        assert_eq!(failed_candidate_label, "Download");
        assert_eq!(diverged_url, "https://portal.example.com/settings");
        Ok(())
    }

    #[test]
    fn resolve_batch_requires_primary_noun_and_filters_modifier_only_matches() {
        // Modifier-only matches (`All issues` via `all`) score on coverage
        // alone; the noun anchor (`invoice`) keeps them out while row
        // controls — named or merely surrounded — stay in.
        let elements = vec![
            element("link", "All issues"),
            element("link", "All pull requests"),
            element("link", "Download invoice 1"),
            AxElement {
                backend_node_id: 4,
                container_text: vec!["INV-002".into()],
                ..element("link", "Download invoice 2")
            },
        ];
        let mut intent = prose_intent("link", "invoices", None, "download all my invoices");
        intent.is_plural = true;
        intent.primary_target_noun = Some("invoice".into());
        let ResolveOutcome::BatchMatch(batch) = resolve_batch(&elements, &intent) else {
            panic!("invoice rows batch together")
        };
        // Exactly the two invoice controls; modifier-only matches stay out.
        assert_eq!(
            batch
                .iter()
                .map(|element| element.name.clone())
                .collect::<Vec<_>>(),
            vec![
                "Download invoice 1".to_owned(),
                "Download invoice 2".to_owned()
            ]
        );
        // Without the anchor the modifier matches would flood back in.
        intent.primary_target_noun = None;
        let ResolveOutcome::BatchMatch(batch) = resolve_batch(&elements, &intent) else {
            panic!("ungated batch collects")
        };
        assert_eq!(batch.len(), 4);
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

    /// Plural invoice intent mirroring the billing-history run: link role,
    /// `download` label, `invoice` noun anchor, batch collection on.
    fn invoice_batch_intent() -> SemanticIntent {
        let mut intent = prose_intent("link", "download", None, "download all my invoices");
        intent.is_plural = true;
        intent.primary_target_noun = Some("invoice".into());
        intent
    }

    /// Payment-history rows as the AX tree reports them once the async table
    /// renders: download links carrying invoice evidence in name or
    /// surroundings, plus one sidebar link the landmark gate must exclude.
    fn payment_history_rows() -> Vec<AxElement> {
        let mut rows = Vec::new();
        for (index, id) in ["INV-001", "INV-002", "INV-003"].iter().enumerate() {
            rows.push(AxElement {
                backend_node_id: i64::try_from(index + 1).unwrap_or(1),
                container_text: vec![(*id).into(), "Invoices".into()],
                landmark: None,
                ..element("link", "Download")
            });
        }
        rows.push(AxElement {
            backend_node_id: 11,
            container_text: vec!["Primary".into()],
            landmark: Some("navigation".into()),
            ..element("link", "Download")
        });
        rows
    }

    #[test]
    fn settle_cadence_is_explicit_state_polling() {
        // Regression guard on the contracted cadence: 250 ms polls, 5000 ms
        // ceiling. Production waits reuse these; hermetic tests below pass
        // scaled values to stay fast.
        assert_eq!(SETTLE_POLL_MS, 250);
        assert_eq!(SETTLE_TIMEOUT_MS, 5000);
        // Probe text is the prompt-derived noun, falling back to the label.
        let anchored = invoice_batch_intent();
        assert_eq!(settle_probe_text(&anchored), "invoice");
        let bare = intent("link", "download");
        assert_eq!(settle_probe_text(&bare), "download");
    }

    /// Static header-only tree: what the AX snapshot holds before the async
    /// table renders. The `Invoice` column header exists from page load, but
    /// as a non-interactive `columnheader` it can never satisfy
    /// [`resolve_batch`] — readiness needs row candidates, not header text.
    fn header_only_tree() -> Vec<AxElement> {
        vec![AxElement {
            backend_node_id: 99,
            role: "columnheader".into(),
            name: "Invoice".into(),
            description: String::new(),
            container_text: Vec::new(),
            landmark: None,
        }]
    }

    #[tokio::test]
    async fn settle_polling_waits_past_headers_for_row_candidates() {
        // The reported failure: `<th>Invoice</th>` exists on page load, so a
        // body-text check returns in <50 ms while the rows are still absent.
        // The loop must keep polling past header-only trees until row
        // candidate nodes land (header snapshots stand in for the first
        // ~10 ms of GitHub's ~500 ms async render; production cadence is
        // 250 ms polls / 5000 ms ceiling).
        use std::sync::{Arc, Mutex};
        let polls = Arc::new(Mutex::new(0_usize));
        let intent = invoice_batch_intent();
        // Headers alone never settle: no actionable candidate exists yet.
        assert!(matches!(
            resolve_batch(&header_only_tree(), &intent),
            ResolveOutcome::NoMatch(_)
        ));
        let seen = wait_for_candidates_with(
            {
                let polls = Arc::clone(&polls);
                move || {
                    let polls = Arc::clone(&polls);
                    async move {
                        let mut count = polls
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        *count += 1;
                        // Rows render after the second poll; earlier polls
                        // see the header-only tree.
                        Some(if *count > 2 {
                            payment_history_rows()
                        } else {
                            header_only_tree()
                        })
                    }
                }
            },
            &intent,
            std::time::Duration::from_millis(5),
            std::time::Duration::from_millis(500),
        )
        .await;
        assert!(seen, "polling observes the delayed rows");
        assert!(
            *polls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                > 1,
            "more than one poll ran before candidates landed"
        );
        let ResolveOutcome::BatchMatch(batch) = resolve_batch(&payment_history_rows(), &intent)
        else {
            panic!("settled rows batch together")
        };
        assert_eq!(
            batch
                .iter()
                .map(|element| element.backend_node_id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[tokio::test]
    async fn settle_timeout_fails_closed_when_rows_never_render() {
        // Headers forever, rows never: the loop must expire at its timeout
        // (scaled down here; production waits the full 5000 ms) and report
        // unready, and the field must fail closed with a diagnostic — never
        // an empty batch, never a blind navigation.
        let intent = invoice_batch_intent();
        let seen = wait_for_candidates_with(
            || async { Some(header_only_tree()) },
            &intent,
            std::time::Duration::from_millis(5),
            std::time::Duration::from_millis(50),
        )
        .await;
        assert!(!seen, "expiry reports unready");
        let empty: Vec<AxElement> = Vec::new();
        assert!(matches!(
            resolve_batch(&empty, &intent),
            ResolveOutcome::NoMatch(_)
        ));
        let diagnostic = grounding_diagnostic(&empty, &intent);
        assert!(
            diagnostic.contains("Evaluated 0 candidates"),
            "{diagnostic}"
        );
    }

    #[test]
    fn batch_scoring_matches_recorded_replay_for_download_files() {
        // Replay-target alignment: the fast replay path (`resolve_fast`,
        // what recorded `Download files` steps use) and the batch collector
        // share one scoring model, so the replay winner must sit inside the
        // batch set — never a control the batch would refuse.
        let intent = invoice_batch_intent();
        let rows = payment_history_rows();
        let (replayed, metrics) = resolve_fast(&rows, &intent);
        assert_eq!(metrics.cost_usd.to_bits(), 0.0f64.to_bits());
        let Some(winner) = replayed else {
            panic!("replay resolves a winner")
        };
        let ResolveOutcome::BatchMatch(batch) = resolve_batch(&rows, &intent) else {
            panic!("batch collects")
        };
        assert_eq!(batch.len(), 3);
        assert!(
            batch
                .iter()
                .any(|element| element.backend_node_id == winner.element.backend_node_id),
            "replay winner is a batch member"
        );
    }
}
