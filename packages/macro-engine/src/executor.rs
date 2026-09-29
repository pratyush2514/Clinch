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

pub use browser_driver::{
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

/// What an executed intent acted on: the badge shown and the rect clicked,
/// plus the click's `click_hit_test:` journal line (what the page itself had
/// under the click point).
#[derive(Clone, Debug, PartialEq)]
pub struct IntentOutcome {
    pub mark: Mark,
    pub highlight: Highlight,
    pub hit_line: String,
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
pub fn container_overlap(scope: &str, element: &AxElement) -> f64 {
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

/// Stage-2 navigation budget: how long a followed result may take to leave
/// the search page before the follow is reported as failed.
pub const FOLLOW_POLL_MS: u64 = 250;
pub const FOLLOW_TIMEOUT_MS: u64 = 10_000;

/// What pursuing an in-page follow-up did. The landed URL is observed from
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

/// What pursuing an in-page follow-up did. The landed URL is observed from
/// the live page, never predicted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageGoalOutcome {
    /// A click navigated somewhere new: the clicked label plus the URL.
    ///
    /// `hit_lines` are the `click_hit_test:` journal lines of the clicks
    /// that got here, one per click, in click order.
    Navigated {
        label: String,
        landed: url::Url,
        hit_lines: Vec<String>,
    },
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
        /// The `click_hit_test:` lines of the clicks before verification
        /// (empty when no click happened, e.g. a cookie-clear or href
        /// navigation).
        hit_lines: Vec<String>,
    },
    /// The page is a signed-out guest landing: there is no identity
    /// chrome to pursue, so the worker stops before any click.
    SignedOut,
}

impl PageGoalOutcome {
    /// Attach the `click_hit_test:` lines found in a tried-journal to a
    /// click-produced outcome. Other variants carry no clicks and pass
    /// through untouched.
    #[must_use]
    fn with_hit_lines(mut self, tried: &[String]) -> Self {
        if let Self::Navigated { hit_lines, .. } | Self::Verified { hit_lines, .. } = &mut self {
            *hit_lines = tried
                .iter()
                .filter(|line| line.starts_with("click_hit_test:"))
                .cloned()
                .collect();
        }
        self
    }
}

/// In-page follow-up budget: observe-act steps before the pursuit gives up
/// and reports [`IntentError::NoMatch`] with the evidence.
const PAGE_GOAL_MAX_STEPS: usize = 3;

/// Model-guided phase budget: the navigator gets fewer steps than the
/// deterministic phase because each one costs a model call. Eight bounds a
/// real multi-turn pursuit (open menu → scan → click → verify) while a
/// hostile page still cannot burn the run.
const MODEL_GOAL_MAX_STEPS: usize = 8;

/// Model budget of the bounded identity-menu policy
/// ([`IdentityMenuPolicy::BOUNDED`]): the UI attempt is a bounded chain
/// (gear-1 click → menu-open verify → one opener retry → one short model
/// pass → the verb's deterministic backstop), so gear 2 gets a short pass
/// instead of the full [`MODEL_GOAL_MAX_STEPS`] hunt.
const BOUNDED_MODEL_MAX_STEPS: usize = 3;

/// Actionable roles a follow-up can meaningfully click: links plus the
/// controls menus are made of. Wider than a link-only contract on
/// purpose — on a portal page the target often lives behind a button.
const PAGE_GOAL_ROLES: &[&str] = &["link", "button", "menuitem", "menuitemlink"];

/// Pursue `noun` on the already-loaded portal page: the Muse-style
/// follow-up. No new browser, no entry-URL resolution, no navigation
/// before acting. Two gears:
///
/// 1. **Deterministic** (free, instant): click the first actionable control
///    mentioning `noun`; unfold one header menu when it isn't directly
///    visible. A URL change ends the pursuit as navigated.
/// 2. **Generalist loop** (only when `navigator` is `Some`): the navigator
///    proposes one [`PageAction`] per turn from the live snapshot, up to
///    [`MODEL_GOAL_MAX_STEPS`] steps, and deterministic Rust executes
///    each pick after validating it against the untruncated snapshot. The
///    model never declares completion — [`PageAction::Done`] is a decline.
///    With no verb spec there is no verifier, so a navigation still ends
///    the loop as navigated.
///
/// [`IntentError::NoMatch`] with the rendered evidence when both gears
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
    escalation: Option<ModelEscalation>,
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
    pursue_with_model(
        browser,
        origin,
        noun,
        navigator,
        None,
        deterministic_miss,
        escalation,
    )
    .await
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

/// One action engine, three gears, for a verb goal: gear 1 (the
/// deterministic chrome worker [`pursue_chrome_action`]) → gear 2 (the
/// generalist model loop with the verb's vocabulary as a prompt hint and
/// the verb's verifier as the wall) → honest miss.
///
/// Playbook replay stays the dispatcher's first gear (the memory fast
/// paths), so this starts at gear 1. The model phase runs only when
/// `navigator` is `Some`; when the model phase's tail fails verification
/// and `escalation` is `Some`, exactly one additional bounded model pass
/// runs under the escalation navigator before the honest miss. Gear 1 can
/// return its deterministic success shapes unverified (e.g. the settings
/// `Navigated` landing) — the
/// dispatcher must run the verb's verifier before treating them as
/// COMPLETED, and maps a failed verifier to a workflow failure (hence
/// FAILED/Take Control), never a completion. Gear 2's success shapes
/// are verifier-decided: [`PageGoalOutcome::Verified`], or the log-out
/// `AlreadyThere` on a pre-signed-out probe (the dispatcher re-reads the
/// live auth state before accepting that one too).
///
/// [`IntentError::NoMatch`] with the tried-click journal when both gears
/// miss — or, for the `LogOut` verb, after the deterministic cookie-clear
/// backstop below has also missed; [`IntentError::Browser`] on CDP failure.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] when the goal is not reached or not
/// verified, and [`IntentError::Browser`] on CDP failure.
pub async fn pursue_verb_goal<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    spec: &VerbSpec,
    navigator: Option<std::sync::Arc<dyn crate::navigator::PageNavigator>>,
    escalation: Option<ModelEscalation>,
) -> Result<PageGoalOutcome, IntentError> {
    let deterministic_miss = match pursue_chrome_action(browser, origin, spec).await {
        Ok(outcome) => return Ok(outcome),
        // Browser errors fail fast: retrying them through the model would
        // just burn model calls on a dead CDP session.
        Err(IntentError::Browser(error)) => return Err(IntentError::Browser(error)),
        Err(IntentError::NoMatch(diagnostic)) => diagnostic,
    };
    // The UI attempt's miss journal, from gear 1 alone or both gears: it
    // rides along so the model phase — and the LogOut cookie fallback —
    // keep the full tried-click story.
    let (ui_miss, model_ran) = match navigator {
        Some(navigator) => {
            // The deterministic tried-log rides along as prompt context: without
            // it the model re-proposes (or fails to recognize) controls the
            // deterministic phase already evaluated — e.g. concluding "no avatar
            // present" while staring at the "Open user actions" button it tried.
            let goal = format!(
                "{}. Already tried without success: {deterministic_miss}",
                verb_goal_text(spec)
            );
            match pursue_with_model(
                browser,
                origin,
                &goal,
                navigator,
                Some(spec),
                deterministic_miss,
                escalation,
            )
            .await
            {
                Ok(outcome) => return Ok(outcome),
                Err(IntentError::Browser(error)) => return Err(IntentError::Browser(error)),
                Err(IntentError::NoMatch(diagnostic)) => (diagnostic, true),
            }
        }
        None => (deterministic_miss, false),
    };
    // `LogOut`-only deterministic backstop: both UI gears failed to reach a
    // verified signed-out state. Clearing the managed profile's session
    // cookies ends the session without depending on the page's menu
    // cooperating; the verifier still decides COMPLETED.
    if spec.kind == VerbKind::LogOut {
        // Final handoff of the bounded `LogOut` UI chain
        // (gear1_click → menu_verify → opener_retry → model_pass →
        // fallback). Journaled only when the model pass actually ran —
        // with `navigator: None` there is no model_pass stage to hand off
        // from.
        let ui_miss = if model_ran {
            format!(
                "{ui_miss}; {}_ui_bounded: model_pass -> fallback",
                spec.menu_policy().journal_tag
            )
        } else {
            ui_miss
        };
        return logout_cookie_fallback(browser, origin, &ui_miss).await;
    }
    Err(IntentError::NoMatch(ui_miss))
}

/// Registrable-host approximation for the `LogOut` cookie fallback:
/// `www.example.com` → `example.com` — the last two dot-separated labels
/// after stripping a `www.` prefix; hosts with fewer than two labels keep
/// the whole host.
///
/// Deliberately NOT a public-suffix list (the same documented
/// approximation the routing layer uses for plausibility checks):
/// `example.co.uk` labels as `co.uk`. Acceptable here because the label
/// only scopes `clear_host_cookies`' dot-boundary suffix match, the host
/// always comes from the live page URL (never user input), and the full
/// host would miss the registrable-domain cookies that actually hold the
/// session.
fn registrable_host(host: &str) -> &str {
    let host = host.strip_prefix("www.").unwrap_or(host);
    let mut parts = host.rsplitn(3, '.');
    match (parts.next(), parts.next()) {
        (Some(tld), Some(sld)) => {
            let start = host.len() - sld.len() - 1 - tld.len();
            &host[start..]
        }
        _ => host,
    }
}

/// `LogOut`-only deterministic backstop, run after both UI gears missed:
/// delete the managed profile's session cookies for the live page's
/// registrable domain (derived from the page URL — never a hardcoded
/// domain list), re-load the page so the auth probe reads post-clear
/// state, and let the verifier decide.
///
/// Ordering: UI → verify (both gears) → clear → verify → COMPLETED or
/// the honest miss. The verification wall does not move: COMPLETED
/// requires the auth probe to read signed out after the clear, exactly
/// as it does after a UI click. The daily browser is never touched.
///
/// [`IntentError::NoMatch`] carries the UI tried-journal plus the
/// distinct fallback line (`log_out: UI path missed; cleared N session
/// cookies for <domain>; verifier: <signed-out|still-unknown>`);
/// [`IntentError::Browser`] on CDP failure (fail fast, like the gears).
async fn logout_cookie_fallback<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    ui_miss: &str,
) -> Result<PageGoalOutcome, IntentError> {
    let page_url = browser.settings_current_url().await;
    let host = page_url
        .as_ref()
        .unwrap_or(origin)
        .host_str()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let registrable = registrable_host(&host);
    if registrable.is_empty() || registrable.contains(['/', ':', '@', '?', '#', ' ']) {
        return Err(IntentError::NoMatch(format!(
            "{ui_miss}; log_out: UI path missed; cookie-clear fallback skipped (unusable page host)"
        )));
    }
    let cleared = browser.chrome_clear_host_cookies(registrable).await?;
    // The pre-clear DOM still renders the signed-in page until reloaded;
    // re-navigate so the auth probe reads post-clear state.
    let reload_url = page_url.unwrap_or_else(|| origin.clone());
    browser.chrome_navigate(&reload_url).await?;
    let signed_out = verify_verb(browser, origin, &LOG_OUT_SPEC, None, None).await;
    let state = if signed_out {
        "signed-out"
    } else {
        "still-unknown"
    };
    let line = format!(
        "log_out: UI path missed; cleared {cleared} session cookies for {registrable}; verifier: {state}"
    );
    if signed_out {
        let landed = browser
            .settings_current_url()
            .await
            .unwrap_or_else(|| origin.clone());
        return Ok(PageGoalOutcome::Verified {
            label: "session cookies cleared".to_string(),
            landed,
            username: None,
            hit_lines: Vec::new(),
        });
    }
    Err(IntentError::NoMatch(format!("{ui_miss}; {line}")))
}

/// Goal sentence for the generalist loop, per verb: what the destination
/// is, in generic words. No site names, no selectors, no procedures — the
/// verb's closed vocabulary rides along separately as the prompt hint.
fn verb_goal_text(spec: &VerbSpec) -> &'static str {
    match spec.kind {
        VerbKind::AccountHome => {
            "account home: open the account/avatar menu in the page header, then the profile control, to reach my own profile page"
        }
        VerbKind::Settings => {
            "settings: open the account menu in the page header, then the settings control, to reach the settings page"
        }
        VerbKind::LogOut => {
            "log out: open the account menu in the page header, then the log-out control, to sign out"
        }
        VerbKind::Notifications => {
            "notifications: open the notifications control in the page header (or the account menu) to reach the notifications page or panel"
        }
    }
}

/// Pursue an `account_home` goal ("my profile", "my account") on the
/// already-loaded portal page through the action engine: gear 1
/// (deterministic chrome walk via [`pursue_chrome_action`] with the
/// account-home spec — the guest short-circuit lives inside it, so a
/// signed-out landing still returns `SignedOut` before the model phase),
/// then gear 2 (the generalist loop), then the honest miss. The verifier
/// decides success — a click alone never succeeds.
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
    escalation: Option<ModelEscalation>,
) -> Result<PageGoalOutcome, IntentError> {
    pursue_verb_goal(
        browser,
        origin,
        VerbSpec::for_kind(VerbKind::AccountHome),
        navigator,
        escalation,
    )
    .await
}

/// The verbs the chrome worker can pursue. Verbs are few, sites are
/// millions: each row of the table below pairs a verb with the vocabulary
/// that reveals its destination in the page's identity chrome and the
/// verifier that proves the action landed. Generic words only — no site
/// names, no selectors, no URLs, no procedures. This table replaces the
/// closed goal-class → worker mapping: a new verb is a new row, not a new
/// worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerbKind {
    /// "profile", "account": the signed-in user's own page on the current
    /// origin. Pursued via the header identity chrome, verified by
    /// page-revealed identity evidence, remembered per origin for repeats.
    AccountHome,
    /// "settings", "preferences": the origin's settings surface. Pursued
    /// through the same header identity chrome, remembered per origin only
    /// from a verified landing, and never from a miss.
    Settings,
    /// "log out", "log off", "sign out": end the session on the current
    /// origin. Verified by the live page reading signed out afterwards.
    LogOut,
    /// "notifications": the origin's notifications surface — a page or a
    /// revealed panel. Pursued through the same identity chrome, verified
    /// by [`verify_notifications_surface`].
    Notifications,
}

impl VerbKind {
    /// Stable key used for identity-memory rows and journal lines.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            VerbKind::AccountHome => "account_home",
            VerbKind::Settings => "settings",
            VerbKind::LogOut => "log_out",
            VerbKind::Notifications => "notifications",
        }
    }
}

/// How a chrome action's landing is verified. A click alone never
/// succeeds — the verifier decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifierKind {
    /// The landed URL's path names the destination: stemmed generic tokens
    /// (the settings row uses "setting"/"preference").
    UrlPathTokens(&'static [&'static str]),
    /// Page-revealed identity evidence: a username reappearing in the landed
    /// URL, an account-worded path, or a profile-worded trigger label.
    IdentityEvidence,
    /// The live page reads signed out after the action.
    AuthSignedOut,
    /// A notifications surface is observably present on the same site: see
    /// [`verify_notifications_surface`].
    NotificationSurface,
}

/// One row of the verb table: the verb, the closed vocabulary that names
/// its destination in revealed chrome, and the verifier that proves the
/// action landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerbSpec {
    pub kind: VerbKind,
    pub vocabulary: &'static [&'static str],
    pub verifier: VerifierKind,
}

impl VerbSpec {
    /// Whether the artifact noun names this verb: exact match against the
    /// closed vocabulary, case-insensitive. Nouns only — never site names.
    #[must_use]
    pub fn matches_noun(&self, noun: &str) -> bool {
        let needle = noun.trim().to_lowercase();
        self.vocabulary.iter().any(|word| *word == needle)
    }

    /// The table's spec for `kind`. Total: every [`VerbKind`] names its
    /// own row below, so the match is exhaustive by construction.
    #[must_use]
    pub fn for_kind(kind: VerbKind) -> &'static VerbSpec {
        match kind {
            VerbKind::AccountHome => &ACCOUNT_HOME_SPEC,
            VerbKind::Settings => &SETTINGS_SPEC,
            VerbKind::LogOut => &LOG_OUT_SPEC,
            VerbKind::Notifications => &NOTIFICATIONS_SPEC,
        }
    }

    /// The identity-menu policy this verb runs under.
    #[must_use]
    pub fn menu_policy(&self) -> IdentityMenuPolicy {
        match self.kind {
            VerbKind::LogOut => IdentityMenuPolicy::BOUNDED,
            VerbKind::AccountHome | VerbKind::Settings | VerbKind::Notifications => {
                IdentityMenuPolicy::STANDARD
            }
        }
    }
}

/// The knobs of the one shared identity-menu path (open the identity
/// menu, pick a noun). Every verb runs the same primitive; a policy only
/// parameterizes the extras around it, so a verb's behavior is data, not a
/// private code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityMenuPolicy {
    /// One re-grounded retry of the same opener (by stable role+name
    /// identity) when the menu primitive missed; also journals the
    /// bounded-chain handoffs.
    pub opener_retry: bool,
    /// Offer the model only header-strip candidates (see
    /// [`header_strip_candidates`]).
    pub header_strip_filter: bool,
    /// Acting budget of one model pass.
    pub model_max_steps: usize,
    /// Whether a failed main model pass may escalate to the stronger model.
    pub model_escalation: bool,
    /// Prefix of the bounded-chain journal lines (`{tag}_ui_bounded: …`,
    /// `{tag} opener retry: …`, `{tag} model filter …`).
    pub journal_tag: &'static str,
}

impl IdentityMenuPolicy {
    /// Default: no opener retry, no header filter, the full model budget,
    /// escalation allowed.
    pub const STANDARD: Self = Self {
        opener_retry: false,
        header_strip_filter: false,
        model_max_steps: MODEL_GOAL_MAX_STEPS,
        model_escalation: true,
        journal_tag: "",
    };
    /// Bounded chain (log out): one opener retry, header-strip candidates
    /// only, one short model pass, no escalation.
    pub const BOUNDED: Self = Self {
        opener_retry: true,
        header_strip_filter: true,
        model_max_steps: BOUNDED_MODEL_MAX_STEPS,
        model_escalation: false,
        journal_tag: "logout",
    };
}

/// Stemmed path tokens for the settings verifier: "settings" and
/// "preferences" reduce to these roots, and compound segments
/// ("user-settings", `account_preferences`) match on their tokens.
/// Generic path vocabulary — no site routes.
const SETTINGS_PATH_TOKENS: &[&str] = &["setting", "preference"];

/// The verb table: one row per [`VerbKind`], each naming the closed
/// vocabulary that reveals the verb's destination in identity chrome and
/// the verifier that proves the action landed. Row order is irrelevant:
/// [`VerbSpec::for_kind`] names each row directly and the orchestration
/// layer's noun table scans the slice.
static ACCOUNT_HOME_SPEC: VerbSpec = VerbSpec {
    kind: VerbKind::AccountHome,
    vocabulary: &["profile", "account"],
    verifier: VerifierKind::IdentityEvidence,
};
static SETTINGS_SPEC: VerbSpec = VerbSpec {
    kind: VerbKind::Settings,
    vocabulary: &["setting", "settings", "preference", "preferences"],
    verifier: VerifierKind::UrlPathTokens(SETTINGS_PATH_TOKENS),
};
static LOG_OUT_SPEC: VerbSpec = VerbSpec {
    kind: VerbKind::LogOut,
    vocabulary: &["log out", "log off", "sign out"],
    verifier: VerifierKind::AuthSignedOut,
};
static NOTIFICATIONS_SPEC: VerbSpec = VerbSpec {
    kind: VerbKind::Notifications,
    vocabulary: &["notification", "notifications"],
    verifier: VerifierKind::NotificationSurface,
};
static VERB_SPECS: &[VerbSpec] = &[
    ACCOUNT_HOME_SPEC,
    SETTINGS_SPEC,
    LOG_OUT_SPEC,
    NOTIFICATIONS_SPEC,
];

/// Every verb the chrome worker can pursue.
#[must_use]
pub fn verb_specs() -> &'static [VerbSpec] {
    VERB_SPECS
}

/// Async browser seam for the spec-parameterized chrome worker
/// ([`pursue_chrome_action`]): the menu primitive's seam
/// ([`MenuBrowser`]) plus the live URL, node href, and page title reads
/// ([`SettingsBrowser`]), plus the auth-state probe and the validated-href
/// navigation the identity lane needs. [`ManagedBrowser`] is the
/// production implementation; tests drive the worker against a scripted
/// fake, so the whole flow is proven with no Chromium.
pub trait ChromeActionBrowser: SettingsBrowser {
    /// Live auth-state probe (title + URL + visible text — the same
    /// [`ManagedBrowser::auth_state`] the settle probe uses, not the
    /// URL-path-only `detect_auth_signal`: a signed-out landing page is
    /// not always a login URL). The account-home lane short-circuits on
    /// `LoggedOut` before any click; the log-out verifier requires
    /// `LoggedOut` after the click.
    fn chrome_auth_state(&self) -> impl std::future::Future<Output = AuthState> + Send;
    /// Navigate to a page-revealed, Rust-validated URL: the identity lane
    /// prefers navigating a validated href over clicking the control.
    ///
    /// # Errors
    /// Returns [`IntentError::Browser`] on CDP failure.
    fn chrome_navigate(
        &self,
        url: &url::Url,
    ) -> impl std::future::Future<Output = Result<(), IntentError>> + Send;
    /// Delete the managed profile's cookies for `host` — the same
    /// revocation the "Forget this site" control uses (registrable domain
    /// with dot-boundary subdomain matching, httpOnly included). The
    /// `LogOut` verb's deterministic backstop: ends the session without
    /// depending on the page's menu cooperating. Managed profile only;
    /// the daily browser is untouched. Returns the number of cookies
    /// deleted.
    ///
    /// The default is inert (`Ok(0)`): scripted fakes that do not model
    /// cookie state keep it; production overrides it with the real CDP
    /// call.
    ///
    /// # Errors
    /// Returns [`IntentError::Browser`] on CDP failure.
    fn chrome_clear_host_cookies(
        &self,
        _host: &str,
    ) -> impl std::future::Future<Output = Result<usize, IntentError>> + Send {
        std::future::ready(Ok(0))
    }
}

impl ChromeActionBrowser for ManagedBrowser {
    async fn chrome_auth_state(&self) -> AuthState {
        self.auth_state().await
    }

    async fn chrome_navigate(&self, url: &url::Url) -> Result<(), IntentError> {
        self.navigate(url).await.map_err(IntentError::Browser)
    }

    async fn chrome_clear_host_cookies(&self, host: &str) -> Result<usize, IntentError> {
        self.clear_host_cookies(host)
            .await
            .map_err(IntentError::Browser)
    }
}

/// Click budget for the chrome worker: opening the account menu plus one
/// revealed-destination click, with one spare for a nested disclosure.
/// Bounded so a hostile header can't burn the run.
const CHROME_ACTION_MAX_CLICKS: usize = 3;

/// A revealed destination for this worker iteration: the vocabulary
/// matcher first, then — for path-token verbs — the href half (a
/// blank-named link to a matching path). [`AxElement`] carries no href,
/// so hrefs resolve lazily per genuinely-new candidate, never for the
/// whole snapshot.
async fn select_revealed_target<'a, B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    elements: &'a [AxElement],
    clicked: &[ClickedControl],
    previously_seen: &std::collections::HashSet<i64>,
    spec: &VerbSpec,
) -> Option<&'a AxElement> {
    if let Some(target) = select_revealed_action(elements, clicked, previously_seen, spec) {
        return Some(target);
    }
    match spec.verifier {
        VerifierKind::UrlPathTokens(tokens) => {
            select_revealed_action_href(elements, clicked, previously_seen, browser, origin, tokens)
                .await
        }
        VerifierKind::IdentityEvidence
        | VerifierKind::AuthSignedOut
        | VerifierKind::NotificationSurface => None,
    }
}

/// Spend the remaining click budget on the shared menu primitive in a
/// single call: its internal click → poll-for-evidence → re-rank loop
/// supersedes the old one-attempt-per-iteration shape, and a Miss means
/// its candidates are exhausted against fresh snapshots, so the worker
/// stops instead of re-looping. `preferred` is the ambiguity gate's
/// chosen opener, clicked on the primitive's first attempt instead of
/// the ranking's favorite. Returns whether a menu opened.
///
/// # Errors
/// Returns [`IntentError::Browser`] on CDP failure.
// Eight parameters: the primitive's own inputs plus the gate's chosen
// opener; bundling would only hide the data flow.
#[allow(clippy::too_many_arguments)]
async fn open_menu_within_budget<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    elements: &[AxElement],
    check: &browser_driver::AxResyncCheck,
    clicked: &mut Vec<ClickedControl>,
    tried: &mut Vec<String>,
    clicks_used: &mut usize,
    preferred: Option<&AxElement>,
) -> Result<bool, IntentError> {
    let baseline = MenuOpenBaseline { elements, check };
    let clicks_before = clicked.len();
    let opened = match open_identity_menu(
        browser,
        origin,
        baseline,
        clicked,
        (CHROME_ACTION_MAX_CLICKS - *clicks_used).min(MENU_OPEN_MAX_TRIES),
        preferred,
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
    // The primitive records every attempt in `clicked`: the spend counts
    // whether or not a menu opened.
    *clicks_used += clicked.len() - clicks_before;
    Ok(opened)
}

/// Single opener retry for the chrome worker's step 2 (policies with
/// [`IdentityMenuPolicy::opener_retry`]): the menu primitive missed with the gate's chosen opener. Re-snapshot,
/// re-ground the SAME opener by stable identity
/// ([`ClickedControl::matches`] — backend node ids churn, so the retry
/// keys on role+name, never the id), click it once through the menu seam,
/// and re-check with the existing [`menu_open_effect`] verdict against
/// the fresh pre-click baseline. The retry spends at most one extra click
/// beyond [`CHROME_ACTION_MAX_CLICKS`]; the caller's flag fires it at most
/// once per run, and the loop's budget check still bounds whatever
/// follows. Every outcome is journaled in `tried` — including a vanished
/// opener, which spends no click. Returns whether a menu opened; the
/// caller keeps its exact break/continue shape.
///
/// # Errors
/// Returns [`IntentError::Browser`] on CDP failure.
async fn identity_opener_retry<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    tag: &str,
    opener: &AxElement,
    clicked: &mut Vec<ClickedControl>,
    tried: &mut Vec<String>,
    clicks_used: &mut usize,
) -> Result<bool, IntentError> {
    let (elements, check, _) = browser.menu_snapshot(origin).await;
    let key = ClickedControl::of(opener);
    let Some(candidate) = elements.iter().find(|element| key.matches(element)) else {
        let name: String = opener.name.chars().take(40).collect();
        tried.push(format!(
            "{tag} opener retry: {} '{name}' not in fresh snapshot, no click spent",
            opener.role
        ));
        return Ok(false);
    };
    // Fresh pre-click baseline: the effect check compares the post-click
    // controls against THIS snapshot, not the stale pre-primitive one.
    let before_ids: std::collections::HashSet<i64> = elements
        .iter()
        .map(|element| element.backend_node_id)
        .collect();
    let actionable_before = count_actionable(&elements);
    let raw_before = check.node_count;
    browser.menu_click_reported(candidate, tried).await?;
    clicked.push(ClickedControl::of(candidate));
    *clicks_used += 1;
    let (after, check_after, _) = browser.menu_snapshot(origin).await;
    let (menu_opened, effect) = menu_open_effect(
        &before_ids,
        actionable_before,
        raw_before,
        &after,
        check_after.node_count,
    );
    tried.push(format!(
        "{tag} opener retry: {}",
        tried_label(candidate, &effect)
    ));
    Ok(menu_opened)
}

/// Identity lane: navigate a page-revealed, Rust-validated href when the
/// revealed control discloses one; otherwise click it and watch the URL.
/// Either way the identity verifier decides. Returns `Some` on a decided
/// outcome, `None` when the click produced no evidence and the hunt
/// continues.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] when the landing carries no identity
/// evidence, and [`IntentError::Browser`] on CDP failure.
async fn act_on_identity_target<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    target: &AxElement,
    label: &str,
    clicked: &mut Vec<ClickedControl>,
    tried: &mut Vec<String>,
    clicks_used: &mut usize,
) -> Result<Option<PageGoalOutcome>, IntentError> {
    // Prefer the page-revealed href when the control is a link that
    // discloses one; validated in Rust, never trusted raw.
    if let Some(href) = browser.settings_node_href(target.backend_node_id).await
        && let Some(url) = validate_revealed_href(&href, origin)
    {
        tried.push(tried_label(target, "revealed href → navigate"));
        browser.chrome_navigate(&url).await?;
        let username = username_from_href(&url);
        let landed = browser.settings_current_url().await.unwrap_or(url);
        return verify_account_home(browser, origin, label, &landed, username.as_deref())
            .await
            .map(|outcome| Some(outcome.with_hit_lines(tried)));
    }
    // No usable href: click the revealed control and watch the URL.
    browser.menu_click_reported(target, tried).await?;
    clicked.push(ClickedControl::of(target));
    *clicks_used += 1;
    tried.push(tried_label(target, "clicked, watching URL"));
    if let Some(landed) = wait_for_url_change(browser).await {
        let username = username_from_menu_text(label);
        return verify_account_home(browser, origin, label, &landed, username.as_deref())
            .await
            .map(|outcome| Some(outcome.with_hit_lines(tried)));
    }
    Ok(None)
}

/// Path-token lane (settings): click the revealed control and watch the
/// URL. A token-named landing decides; a disclosure surface must name
/// itself in the fresh page's title or headings, or the click is not
/// evidence. Returns `Some` on a decided outcome, `None` when the click
/// produced no evidence and the hunt continues.
///
/// # Errors
/// Returns [`IntentError::Browser`] on CDP failure.
async fn act_on_path_tokens_target<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    target: &AxElement,
    spec: &VerbSpec,
    clicked: &mut Vec<ClickedControl>,
    tried: &mut Vec<String>,
    clicks_used: &mut usize,
) -> Result<Option<PageGoalOutcome>, IntentError> {
    let VerifierKind::UrlPathTokens(tokens) = spec.verifier else {
        // `pursue_chrome_action` only calls this helper for the path-token
        // verifier; any other verifier here is a caller bug, reported as an
        // honest miss rather than a panic.
        return Err(IntentError::NoMatch(chrome_action_miss_diagnostic(
            spec, tried,
        )));
    };
    let label = revealed_label(target, spec);
    browser.menu_click_reported(target, tried).await?;
    clicked.push(ClickedControl::of(target));
    *clicks_used += 1;
    tried.push(tried_label(target, "clicked, watching URL"));
    if let Some(landed) = wait_for_url_change(browser).await {
        return verify_path_tokens_landing(&label, &landed, tokens, spec, tried).map(Some);
    }
    // No navigation: the control acted like a disclosure. The surface must
    // name itself in the fresh page's title or headings, or the click is
    // not evidence.
    let (fresh, _, _) = browser.menu_snapshot(origin).await;
    let title = browser.settings_page_title().await;
    if action_surface_visible(&fresh, title.as_deref(), spec.vocabulary) {
        let landed = browser
            .settings_current_url()
            .await
            .unwrap_or_else(|| origin.clone());
        return Ok(Some(
            PageGoalOutcome::Verified {
                label,
                landed,
                username: None,
                hit_lines: Vec::new(),
            }
            .with_hit_lines(tried),
        ));
    }
    tried.push(tried_label(target, "clicked, no evidence"));
    Ok(None)
}

/// Signed-out lane (log out): click the revealed control, then the auth
/// probe decides — the URL may or may not change. Returns `Some` on a
/// decided outcome, `None` when the page still reads signed in and the
/// hunt continues.
///
/// # Errors
/// Returns [`IntentError::Browser`] on CDP failure.
async fn act_on_signed_out_target<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    target: &AxElement,
    label: &str,
    clicked: &mut Vec<ClickedControl>,
    tried: &mut Vec<String>,
    clicks_used: &mut usize,
) -> Result<Option<PageGoalOutcome>, IntentError> {
    browser.menu_click_reported(target, tried).await?;
    clicked.push(ClickedControl::of(target));
    *clicks_used += 1;
    tried.push(tried_label(target, "clicked, watching URL"));
    // Logging out usually navigates; the URL may also stay put, so the
    // wait is best-effort and the auth probe decides either way.
    let _ = wait_for_url_change(browser).await;
    let landed = browser
        .settings_current_url()
        .await
        .unwrap_or_else(|| origin.clone());
    if browser.chrome_auth_state().await == AuthState::LoggedOut {
        return Ok(Some(
            PageGoalOutcome::Verified {
                label: label.to_owned(),
                landed,
                username: None,
                hit_lines: Vec::new(),
            }
            .with_hit_lines(tried),
        ));
    }
    tried.push(tried_label(target, "clicked, still signed in"));
    Ok(None)
}

/// Notifications lane: click the revealed control, then the surface
/// verifier decides — the click may navigate to a page or reveal a
/// panel/overlay in place. Returns `Some` on a decided outcome, `None`
/// when no notifications surface is observable and the hunt continues.
///
/// # Errors
/// Returns [`IntentError::Browser`] on CDP failure.
async fn act_on_notifications_target<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    target: &AxElement,
    spec: &VerbSpec,
    clicked: &mut Vec<ClickedControl>,
    tried: &mut Vec<String>,
    clicks_used: &mut usize,
) -> Result<Option<PageGoalOutcome>, IntentError> {
    let label = revealed_label(target, spec);
    browser.menu_click_reported(target, tried).await?;
    clicked.push(ClickedControl::of(target));
    *clicks_used += 1;
    tried.push(tried_label(target, "clicked, watching URL"));
    let _ = wait_for_url_change(browser).await;
    if verify_verb(browser, origin, spec, Some(&label), None).await {
        let landed = browser
            .settings_current_url()
            .await
            .unwrap_or_else(|| origin.clone());
        return Ok(Some(
            PageGoalOutcome::Verified {
                label,
                landed,
                username: None,
                hit_lines: Vec::new(),
            }
            .with_hit_lines(tried),
        ));
    }
    tried.push(tried_label(target, "clicked, no notifications surface"));
    Ok(None)
}

/// Run the spec's act lane on a revealed (or already-open-menu) target:
/// the single dispatch behind the main loop's revealed step, the
/// already-open-menu step (1a), and the semantic fallback — one
/// implementation, three call sites. Returns `Some` on a decided outcome,
/// `None` when the click produced no evidence and the hunt continues.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] on a path-token caller bug (never for
/// the other verifiers), and [`IntentError::Browser`] on CDP failure.
async fn act_on_verb_target<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    target: &AxElement,
    spec: &VerbSpec,
    clicked: &mut Vec<ClickedControl>,
    tried: &mut Vec<String>,
    clicks_used: &mut usize,
) -> Result<Option<PageGoalOutcome>, IntentError> {
    let label = revealed_label(target, spec);
    match spec.verifier {
        VerifierKind::IdentityEvidence => {
            act_on_identity_target(browser, origin, target, &label, clicked, tried, clicks_used)
                .await
        }
        VerifierKind::UrlPathTokens(_) => {
            act_on_path_tokens_target(browser, origin, target, spec, clicked, tried, clicks_used)
                .await
        }
        VerifierKind::AuthSignedOut => {
            act_on_signed_out_target(browser, origin, target, &label, clicked, tried, clicks_used)
                .await
        }
        VerifierKind::NotificationSurface => {
            act_on_notifications_target(browser, origin, target, spec, clicked, tried, clicks_used)
                .await
        }
    }
}

/// Generic verb hint for the semantic matcher: the verb's plain words
/// (`"log out"`, `"settings"`, `"account home"`) — never site names.
/// [`crate::semantic::SemanticMatcher`] appends `" action"` itself.
fn semantic_verb_hint(spec: &VerbSpec) -> String {
    spec.kind.as_str().replace('_', " ")
}

/// Outcome of [`already_open_menu_step`]: whether the step ran, and
/// whether the act lane decided the run.
enum AlreadyOpenMenuOutcome {
    /// Preconditions didn't hold; the hunt continues inline.
    Skipped,
    /// The step ran without deciding; the caller records the snapshot
    /// ids and continues the hunt.
    Ran,
    /// The act lane decided the run.
    Decided(PageGoalOutcome),
}

/// Worker step 1a: already-open menu. The worker hasn't clicked anything
/// yet but the snapshot already shows a menu layer — the page opened the
/// menu itself. Click the verb's destination directly through the spec's
/// act lane; clicking an opener here would toggle the open menu shut.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] on a path-token caller bug (never for
/// the other verifiers), and [`IntentError::Browser`] on CDP failure.
async fn already_open_menu_step<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    elements: &[AxElement],
    spec: &VerbSpec,
    clicked: &mut Vec<ClickedControl>,
    tried: &mut Vec<String>,
    clicks_used: &mut usize,
) -> Result<AlreadyOpenMenuOutcome, IntentError> {
    if !clicked.is_empty() {
        return Ok(AlreadyOpenMenuOutcome::Skipped);
    }
    let Some(target) = select_already_open_menu_target(elements, spec) else {
        return Ok(AlreadyOpenMenuOutcome::Skipped);
    };
    let name: String = target.name.chars().take(40).collect();
    tried.push(format!(
        "menu already open: clicked '{name}' directly (no opener)"
    ));
    if let Some(outcome) =
        act_on_verb_target(browser, origin, target, spec, clicked, tried, clicks_used).await?
    {
        return Ok(AlreadyOpenMenuOutcome::Decided(outcome));
    }
    // The page's own menu may still be open: dismiss now (best-effort
    // no-op when it already closed) so its light-dismiss can't swallow
    // a later opener click.
    browser.menu_dismiss().await;
    tried.push("dismissed possibly-open menu after direct click".to_owned());
    Ok(AlreadyOpenMenuOutcome::Ran)
}

/// Worker step 1b: opener ambiguity gate, before any opener click. The
/// deterministic lane only spends a click when exactly one strong opener
/// exists — an account-worded header button, or a blank-named in-strip
/// button (the avatar case). On 2+ the semantic opener matcher gets one
/// tiebreak attempt.
///
/// Returns the opener the deterministic lane must click — the single
/// strong opener, or the semantic winner. The caller clicks exactly the
/// returned opener (the menu primitive's `preferred` candidate): the
/// primitive's independent ranking must not substitute a different
/// candidate, so identifying the winner is not enough — the winner is
/// what gets clicked.
///
/// Returns `Err(IntentError::NoMatch)` naming the deferral when zero or
/// 2+ (untiebroken) strong openers exist — the worker clicked nothing
/// and the model phase takes over with the tried journal intact.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] when no unambiguous opener exists,
/// and [`IntentError::Browser`] on CDP failure.
async fn opener_gate_pick<'a, B: MenuBrowser>(
    browser: &B,
    elements: &'a [AxElement],
    clicked: &[ClickedControl],
    opener_matcher: &mut crate::semantic::SemanticMatcher,
    tried: &mut Vec<String>,
    spec: &VerbSpec,
) -> Result<&'a AxElement, IntentError> {
    let strip_bottom = header_strip_bottom(browser).await;
    let rects = header_button_rects(browser, elements, clicked).await;
    let strong = strong_openers(elements, clicked, &rects, strip_bottom);
    if let [only] = strong.as_slice() {
        return Ok(*only);
    }
    if strong.len() >= 2
        && let Some((winner, score)) = semantic_opener_winner(opener_matcher, &strong).await
    {
        let name: String = winner.name.chars().take(40).collect();
        tried.push(format!(
            "semantic opener '{name}' (score {score:.2}) disambiguated among {} candidates",
            strong.len()
        ));
        return Ok(winner);
    }
    Err(IntentError::NoMatch(format!(
        "no unambiguous account-menu opener ({} strong candidates); deferring to model phase; {}",
        strong.len(),
        chrome_action_miss_diagnostic(spec, tried)
    )))
}

/// Worker step 1: revealed destination. A revealed destination ends the
/// hunt — but only after the worker opened something (`clicked` is
/// non-empty), and only when the candidate is genuinely new since the
/// previous snapshot: the container-text rollup lets an opened menu's
/// wording match every header button that was already on the page, so
/// without the newness gate the worker clicks the header chrome itself
/// as the destination and burns its click budget. When the closed
/// vocabulary matcher finds nothing, the semantic fallback scores
/// genuinely-new menu-layer candidates' accessible names against the
/// verb (capped, menu-layer only, thresholded) — a blank-named link to
/// a matching path still counts, which the name-driven vocabulary lane
/// can never see.
///
/// Returns the candidate to click, or `None` when nothing revealed.
///
/// # Errors
/// Returns [`IntentError::Browser`] on CDP failure.
// The eight parameters are the two callees' inputs concatenated
// (`select_revealed_target` needs browser+origin for lazy href
// resolution; the semantic fallback needs the matcher and the journal);
// bundling them would only hide the data flow.
#[allow(clippy::too_many_arguments)]
async fn select_revealed_destination<'a, B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    elements: &'a [AxElement],
    clicked: &[ClickedControl],
    previously_seen: &std::collections::HashSet<i64>,
    verb_matcher: &mut crate::semantic::SemanticMatcher,
    tried: &mut Vec<String>,
    spec: &VerbSpec,
) -> Option<&'a AxElement> {
    if let Some(target) =
        select_revealed_target(browser, origin, elements, clicked, previously_seen, spec).await
    {
        return Some(target);
    }
    // Vocabulary found nothing: one semantic attempt over the
    // genuinely-new menu-layer candidates, before the opener click.
    semantic_revealed_target(verb_matcher, elements, clicked, previously_seen, tried).await
}

/// Worker step 0: per-verb pre-state strategies, before any click.
/// Account-home short-circuits on a signed-out page (`SignedOut` — a
/// guest landing has no identity chrome to pursue); log-out completes
/// immediately when the page already reads signed out (`AlreadyThere`).
///
/// Returns `Ok(Some(outcome))` when the verb short-circuits without
/// clicking, `Ok(None)` when the hunt proceeds.
///
/// # Errors
/// Returns [`IntentError::Browser`] on CDP failure.
async fn chrome_action_prestate<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    spec: &VerbSpec,
) -> Result<Option<PageGoalOutcome>, IntentError> {
    match spec.kind {
        VerbKind::AccountHome => {
            // Guest short-circuit: a signed-out page has no identity chrome,
            // so the worker stops before any click.
            if browser.chrome_auth_state().await == AuthState::LoggedOut {
                return Ok(Some(PageGoalOutcome::SignedOut));
            }
        }
        VerbKind::LogOut => {
            // Already signed out: the goal is achieved; nothing to click.
            if browser.chrome_auth_state().await == AuthState::LoggedOut {
                let landed = browser
                    .settings_current_url()
                    .await
                    .unwrap_or_else(|| origin.clone());
                return Ok(Some(PageGoalOutcome::AlreadyThere { landed }));
            }
        }
        VerbKind::Settings | VerbKind::Notifications => {}
    }
    Ok(None)
}

/// Pursue a verb-spec action through the page's identity chrome: the single
/// parameterized worker behind every in-page verb, collapsing the old
/// per-goal workers. Deterministic — no model phase:
///
/// 1. **Per-verb pre-state, no clicks yet**: account-home short-circuits on
///    a signed-out page (`SignedOut` — a guest landing has no identity
///    chrome to pursue); log-out completes immediately when the page
///    already reads signed out (`AlreadyThere`).
/// 2. **Untruncated AX snapshot** via [`MenuBrowser::menu_snapshot`] (a
///    revealed menu renders at the end of the document, past the
///    300-element head truncation).
/// 3. **Already-open menu**: when the worker hasn't clicked anything yet
///    but the snapshot already shows a menu layer, the page opened the
///    menu itself — the verb's destination is clicked directly through
///    the spec's act lane, never via an opener (which would toggle the
///    menu shut).
/// 4. **A revealed destination ends the hunt**: an actionable control
///    matching the spec's vocabulary that appeared *after* the worker
///    opened something — `clicked` is non-empty and the candidate is
///    genuinely new since the previous snapshot, never pre-existing
///    chrome. When the vocabulary matcher finds nothing, the semantic
///    fallback scores genuinely-new menu-layer candidates' accessible
///    names against the verb (capped, menu-layer only). The control is
///    clicked (the identity lane navigates a page-revealed,
///    Rust-validated href directly instead), then the spec's verifier
///    decides: [`PageGoalOutcome::Navigated`] for a path-token landing,
///    [`PageGoalOutcome::Verified`] for identity evidence, a named
///    disclosure surface, or a signed-out page.
/// 5. **Opener ambiguity gate**: the deterministic lane spends an opener
///    click only when exactly one strong opener exists (account-worded,
///    or blank-named in the header strip — the avatar case); on 2+ the
///    semantic opener matcher gets one tiebreak attempt. Otherwise the
///    worker clicks nothing and defers to the model phase with the tried
///    journal intact.
/// 6. **Otherwise the shared [`open_identity_menu`] primitive** spends the
///    remaining click budget opening the account menu — its internal
///    rank → click → poll loop is not reimplemented here, and a Miss ends
///    the worker instead of re-looping over candidates it exhausted — with
///    one exception: a policy with [`IdentityMenuPolicy::opener_retry`]
///    (log out) gets exactly one re-grounded retry of the same opener (same stable identity, never a new candidate)
///    before the Miss, covering actuation misses where the click never
///    landed.
///
/// An honest miss beats a guessed click: the worker only clicks controls
/// the revealed-gating selected, and [`IntentError::NoMatch`] carries the
/// tried-lines journal.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] with the tried-lines journal when the
/// action is not verified, and [`IntentError::Browser`] on CDP failure.
// The worker's six documented steps read as one function: splitting the
// loop body would scatter the step ordering the doc comment narrates.
#[allow(clippy::too_many_lines)]
pub async fn pursue_chrome_action<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    spec: &VerbSpec,
) -> Result<PageGoalOutcome, IntentError> {
    // Per-verb pre-state strategies, before any click.
    if let Some(outcome) = chrome_action_prestate(browser, origin, spec).await? {
        return Ok(outcome);
    }

    let mut clicked: Vec<ClickedControl> = Vec::new();
    let mut tried: Vec<String> = Vec::new();
    // Node ids from the previous iteration's snapshot. A "revealed"
    // destination must be genuinely new: the container-text rollup lets an
    // opened menu's wording match every header button that was already on
    // the page, so without this the worker clicks the header chrome itself
    // as the destination and burns its click budget.
    let mut previously_seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    // Total click budget across both click kinds. The shared menu primitive
    // retries internally, so the loop counts every click it spends —
    // menu-opening or revealed-destination — against
    // CHROME_ACTION_MAX_CLICKS. A menu Miss ends the worker: the primitive
    // already re-snapshotted and re-ranked between attempts, so re-looping
    // would only re-examine candidates it exhausted.
    let mut clicks_used: usize = 0;
    // Whether the previous menu-opening attempt reported a menu open: the
    // revealed-destination search found nothing in it, so it is the wrong
    // menu (or a false positive) and must be dismissed before the next
    // attempt clicks — otherwise its light-dismiss swallows that click.
    let mut menu_maybe_open = false;
    // The verb's identity-menu policy parameterizes the shared path.
    let policy = spec.menu_policy();
    // Single opener retry (step 2, policy-gated): the menu primitive can
    // spend its whole budget on an opener whose click never lands
    // (recorded, but no menu evidence on a live page). Exactly one
    // re-grounded retry of the SAME opener per run; policies without it
    // keep the plain miss path.
    let mut opener_retried = false;
    // Bounded-chain marker: with the retry policy the UI attempt is a fixed
    // chain — gear-1 click → menu-open verify → one opener retry → one
    // short model pass → backstop — and each handoff is journaled once per
    // run so the bound is visible in the tried log.
    let mut bounded_g1_journaled = false;
    // Per-run semantic matchers, created once: the verb's generic words
    // score revealed items when the closed vocabulary is inconclusive,
    // and "account menu" disambiguates the opener when several strong
    // candidates compete. Cached per run, names only — volume discipline
    // is enforced at the call sites (menu-layer candidates, ambiguity
    // only). The classifier is a hint: any failure declines silently and
    // the deterministic flow continues unchanged.
    let mut verb_matcher = crate::semantic::SemanticMatcher::new(&semantic_verb_hint(spec));
    let mut opener_matcher = crate::semantic::SemanticMatcher::new("account menu");

    while clicks_used < CHROME_ACTION_MAX_CLICKS {
        // Full snapshot, not the navigator slice: a revealed menu renders at
        // the end of the document (React portal), past the 300-element head
        // cap. Before/after id sets share the same full-list semantics so
        // "genuinely new" stays correct.
        let (elements, check_before, _) = browser.menu_snapshot(origin).await;
        let currently_seen: std::collections::HashSet<i64> =
            elements.iter().map(|el| el.backend_node_id).collect();

        // 0. Already-open menu (1a): the page opened the menu itself —
        // click the destination directly, never an opener.
        match already_open_menu_step(
            browser,
            origin,
            &elements,
            spec,
            &mut clicked,
            &mut tried,
            &mut clicks_used,
        )
        .await?
        {
            AlreadyOpenMenuOutcome::Decided(outcome) => return Ok(outcome),
            AlreadyOpenMenuOutcome::Ran => {
                previously_seen = currently_seen;
                continue;
            }
            AlreadyOpenMenuOutcome::Skipped => {}
        }

        // 1. A revealed destination ends the hunt — but only after the worker
        // opened something, and only when the candidate actually appeared
        // after that opening (vocabulary first, one semantic attempt over
        // genuinely-new menu-layer candidates second).
        if let Some(target) = select_revealed_destination(
            browser,
            origin,
            &elements,
            &clicked,
            &previously_seen,
            &mut verb_matcher,
            &mut tried,
            spec,
        )
        .await
        {
            if let Some(outcome) = act_on_verb_target(
                browser,
                origin,
                target,
                spec,
                &mut clicked,
                &mut tried,
                &mut clicks_used,
            )
            .await?
            {
                return Ok(outcome);
            }
            previously_seen = currently_seen;
            continue;
        }

        // 2. No revealed destination: spend the remaining budget on the
        // shared menu primitive in a single call. A wrong menu left open
        // by the previous attempt light-dismisses on the next click —
        // swallowing it instead of letting it reach the next candidate —
        // so Escape first (best-effort no-op when nothing is open).
        if std::mem::replace(&mut menu_maybe_open, false) {
            browser.menu_dismiss().await;
            tried.push("dismissed possibly-open menu before next attempt".to_owned());
        }
        // 1b. Opener ambiguity gate, before any opener click: the
        // deterministic lane only spends a click when exactly one strong
        // opener exists. Zero or 2+ (untiebroken) defers to the model
        // phase with the tried journal intact, which `pursue_verb_goal`
        // feeds to gear 2 as "Already tried without success". The gate
        // returns the opener to click — the primitive's first attempt
        // clicks exactly it, never the ranking's independent favorite.
        let chosen_opener = opener_gate_pick(
            browser,
            &elements,
            &clicked,
            &mut opener_matcher,
            &mut tried,
            spec,
        )
        .await?;
        let opened = open_menu_within_budget(
            browser,
            origin,
            &elements,
            &check_before,
            &mut clicked,
            &mut tried,
            &mut clicks_used,
            Some(chosen_opener),
        )
        .await?;
        // Policy-gated single retry: when the primitive missed, re-ground
        // the SAME opener by stable identity and spend one more click on
        // it. An actuation miss (click recorded, menu never opened) reads
        // identically to a wrong-opener miss, and the retry distinguishes
        // them. At most one per run; a second miss breaks exactly as
        // before, and policies without it never enter this branch. The
        // bounded chain's handoffs are journaled (`{tag}_ui_bounded`) so
        // the fail-fast path is visible in the tried log.
        let bounded = policy.opener_retry;
        let tag = policy.journal_tag;
        if bounded && !bounded_g1_journaled {
            bounded_g1_journaled = true;
            tried.push(format!("{tag}_ui_bounded: gear1_click -> menu_verify"));
        }
        let opened = if !opened && bounded && !opener_retried {
            opener_retried = true;
            tried.push(format!("{tag}_ui_bounded: menu_verify -> opener_retry"));
            identity_opener_retry(
                browser,
                origin,
                tag,
                chosen_opener,
                &mut clicked,
                &mut tried,
                &mut clicks_used,
            )
            .await?
        } else {
            opened
        };
        if !opened {
            if bounded {
                tried.push(format!("{tag}_ui_bounded: opener_retry -> model_pass"));
            }
            break;
        }
        menu_maybe_open = true;
        previously_seen = currently_seen;
    }
    Err(IntentError::NoMatch(chrome_action_miss_diagnostic(
        spec, &tried,
    )))
}

/// A revealed verb destination: an actionable control matching the spec's
/// vocabulary (or, for the identity lane, a page-revealed `u/name`-style
/// handle), not yet clicked, and genuinely new since the previous snapshot
/// — the container-text rollup shares an opened menu's wording with the
/// header buttons that were already there, so "new" is what makes it
/// revealed. Gated on `clicked` being non-empty (see
/// [`pursue_chrome_action`]) so a bare page's footer links never qualify:
/// the worker must have opened something first.
#[must_use]
pub fn select_revealed_action<'a, S: std::hash::BuildHasher>(
    elements: &'a [AxElement],
    clicked: &[ClickedControl],
    previously_seen: &std::collections::HashSet<i64, S>,
    spec: &VerbSpec,
) -> Option<&'a AxElement> {
    if clicked.is_empty() {
        return None;
    }
    let identity_lane = matches!(spec.verifier, VerifierKind::IdentityEvidence);
    elements.iter().find(|element| {
        PAGE_GOAL_ROLES.contains(&element.role.as_str())
            && !already_clicked(clicked, element)
            && !previously_seen.contains(&element.backend_node_id)
            && (mentions_vocabulary(element, spec.vocabulary)
                || (identity_lane && username_from_menu_text(&element.name).is_some()))
    })
}

/// The href half of the revealed-item matcher for the
/// [`VerifierKind::UrlPathTokens`] lane: a genuinely-new actionable control
/// whose page-revealed href resolves (via [`validate_revealed_href`]) to a
/// URL whose path carries one of `tokens` — the blank-named-link case.
/// Same "revealed" gating as [`select_revealed_action`]; hrefs resolve
/// lazily per candidate, never for the whole snapshot.
async fn select_revealed_action_href<'a, B: ChromeActionBrowser, S: std::hash::BuildHasher>(
    elements: &'a [AxElement],
    clicked: &[ClickedControl],
    previously_seen: &std::collections::HashSet<i64, S>,
    browser: &B,
    origin: &url::Url,
    tokens: &[&str],
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
        let is_target = browser
            .settings_node_href(element.backend_node_id)
            .await
            .as_deref()
            .and_then(|href| validate_revealed_href(href, origin))
            .is_some_and(|url| url_path_has_tokens(&url, tokens));
        if is_target {
            return Some(element);
        }
    }
    None
}

/// Already-open menu target (worker step 0 / 1a): when the worker hasn't
/// clicked anything yet but the snapshot already shows a menu layer
/// (menu/menuitem/menuitemlink roles, or a dialog role), the page opened
/// the menu itself — scan those menu-layer elements for the verb's
/// vocabulary match. Requires actual menu-layer evidence, so a bare
/// page's footer links never qualify (the existing revealed gate's
/// intent); the caller clicks the match directly instead of an opener,
/// which would toggle the open menu shut. Pure and unit-tested.
///
/// Dialog reconciliation: a `dialog` role counts as menu-layer evidence
/// for the gate, but the direct target must itself be an actionable
/// menu-layer control — the AX tree is flat (no parent pointers), so
/// "controls inside the dialog" can't be identified structurally, and
/// clicking a non-menu button merely because a dialog exists would be a
/// guessed click. A `dialog` element itself is never the target (it is
/// not actionable).
#[must_use]
pub fn select_already_open_menu_target<'a>(
    elements: &'a [AxElement],
    spec: &VerbSpec,
) -> Option<&'a AxElement> {
    if !elements
        .iter()
        .any(|element| is_menu_role(&element.role) || element.role == "dialog")
    {
        return None;
    }
    elements.iter().find(|element| {
        (is_menu_role(&element.role) || element.role == "dialog")
            && PAGE_GOAL_ROLES.contains(&element.role.as_str())
            && mentions_vocabulary(element, spec.vocabulary)
    })
}

/// How many menu-layer candidates the semantic revealed fallback scores
/// per turn: menu trees are short, and the classifier is a hint — six
/// names per turn keeps the run far below quota even on pathological
/// pages.
const SEMANTIC_REVEALED_CAP: usize = 6;

/// Semantic fallback for revealed items: when the vocabulary matcher
/// found nothing but the worker did open something (`clicked` non-empty —
/// the same gate as [`select_revealed_action`]), score the accessible
/// names of genuinely-new actionable menu-layer candidates against the
/// verb, capped at [`SEMANTIC_REVEALED_CAP`], and take the first at or
/// above [`crate::semantic::SEMANTIC_ACCEPT`]. The pick is journaled as
/// `semantic match '<name>' (score X)`. The classifier is a hint — any
/// failure declines to `None` and the worker falls through to the opener
/// hunt unchanged. Volume discipline: menu-layer only, ambiguity only,
/// cached per run by the matcher, names only.
pub async fn semantic_revealed_target<'a, S: std::hash::BuildHasher>(
    matcher: &mut crate::semantic::SemanticMatcher,
    elements: &'a [AxElement],
    clicked: &[ClickedControl],
    previously_seen: &std::collections::HashSet<i64, S>,
    tried: &mut Vec<String>,
) -> Option<&'a AxElement> {
    if clicked.is_empty() {
        return None;
    }
    let mut scored = 0;
    for element in elements {
        if scored >= SEMANTIC_REVEALED_CAP {
            break;
        }
        if !is_menu_role(&element.role)
            || !PAGE_GOAL_ROLES.contains(&element.role.as_str())
            || already_clicked(clicked, element)
            || previously_seen.contains(&element.backend_node_id)
        {
            continue;
        }
        scored += 1;
        if let Some(score) = matcher.score(&element.name).await
            && score >= crate::semantic::SEMANTIC_ACCEPT
        {
            let name: String = element.name.chars().take(40).collect();
            tried.push(format!("semantic match '{name}' (score {score:.2})"));
            return Some(element);
        }
    }
    None
}

/// Whether the element's name, description, or container rollup mentions a
/// word of the verb's vocabulary, in [`normalize`]d form. Substring
/// matching, so "setting" covers "settings"; multi-word vocabulary
/// ("log out") matches the whitespace-collapsed text.
fn mentions_vocabulary(element: &AxElement, vocabulary: &[&str]) -> bool {
    let name = normalize(&element.name);
    let description = normalize(&element.description);
    let container = joined_container(element);
    vocabulary
        .iter()
        .any(|word| name.contains(word) || description.contains(word) || container.contains(word))
}

/// Whether `text` mentions a vocabulary word, in [`normalize`]d form.
fn text_mentions_vocabulary(text: &str, vocabulary: &[&str]) -> bool {
    let haystack = normalize(text);
    vocabulary.iter().any(|word| haystack.contains(word))
}

/// Whether a URL path segment names a token destination, stemmed: "settings"
/// reduces to the root the token list uses, and compound segments
/// ("user-settings", `account_preferences`) match on their tokens. Generic
/// path vocabulary — no site routes.
fn path_token_matches(segment: &str, tokens: &[&str]) -> bool {
    segment
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|token| {
            let stem = token.strip_suffix('s').unwrap_or(token);
            tokens.contains(&stem)
        })
}

/// Whether the URL's path carries a token segment (stemmed match).
fn url_path_has_tokens(url: &url::Url, tokens: &[&str]) -> bool {
    match url.path_segments() {
        Some(mut segments) => segments.any(|segment| path_token_matches(segment, tokens)),
        None => false,
    }
}

/// Post-click disclosure check for the path-token lane: with no navigation,
/// the click only counts when the fresh page names the verb in its title or
/// a heading. Generic words only — the spec's own vocabulary.
fn action_surface_visible(
    elements: &[AxElement],
    title: Option<&str>,
    vocabulary: &[&str],
) -> bool {
    if title.is_some_and(|title| text_mentions_vocabulary(title, vocabulary)) {
        return true;
    }
    elements
        .iter()
        .any(|element| element.role == "heading" && mentions_vocabulary(element, vocabulary))
}

/// The label a revealed control is journaled under: its name, else its
/// description, else a generic fallback.
fn revealed_label(target: &AxElement, spec: &VerbSpec) -> String {
    let name = target.name.trim();
    if !name.is_empty() {
        return name.to_owned();
    }
    let description = target.description.trim();
    if !description.is_empty() {
        return description.to_owned();
    }
    format!("({} link)", spec.kind.as_str())
}

/// Path-token landing verifier: a navigation only counts when the landed
/// URL's path names a token destination (stemmed segment match). A click
/// alone never yields success — an irrelevant landing is an honest miss
/// carrying the tried-lines journal.
fn verify_path_tokens_landing(
    label: &str,
    landed: &url::Url,
    tokens: &[&str],
    spec: &VerbSpec,
    tried: &[String],
) -> Result<PageGoalOutcome, IntentError> {
    if url_path_has_tokens(landed, tokens) {
        Ok(PageGoalOutcome::Navigated {
            label: label.to_owned(),
            landed: landed.clone(),
            hit_lines: Vec::new(),
        }
        .with_hit_lines(tried))
    } else {
        Err(IntentError::NoMatch(chrome_action_miss_diagnostic(
            spec, tried,
        )))
    }
}

/// Miss diagnostic for the chrome worker: what it actually tried, never a
/// dump of the controls it evaluated.
#[must_use]
pub fn chrome_action_miss_diagnostic(spec: &VerbSpec, tried: &[String]) -> String {
    let target = match spec.kind {
        VerbKind::AccountHome => "identity",
        VerbKind::Settings => "settings",
        VerbKind::LogOut => "log out",
        VerbKind::Notifications => "notifications",
    };
    let key = spec.kind.as_str();
    if tried.is_empty() {
        format!("{key}: no {target} control revealed from the account menu")
    } else {
        format!(
            "{key}: {target} destination not reached. Tried: [{}]",
            tried.join("; ")
        )
    }
}

/// Deterministic verification wall for a verb spec: the ONLY decider of
/// goal completion across the action engine's gears. Never clicks, never
/// navigates — it reads the live page after the acting loop (gear 1 or
/// gear 2) and reports whether the page proves the verb's destination was
/// reached. A click alone never succeeds; this is what does.
///
/// `label` is the last action's label hint (clicked control name, or the
/// goal text when nothing was clicked) for the identity lane; `username`
/// is a page-revealed username when the acting loop read one — from a
/// validated href or a menu label, never derived from the landed URL
/// itself (that would make the identity check circular).
///
/// Per [`VerifierKind`]:
/// * `UrlPathTokens`: the live URL's path carries the tokens, or the fresh
///   page names the destination in its title/headings (the
///   disclosure-without-navigation case) — the same two checks the chrome
///   worker's path-token lane applies.
/// * `IdentityEvidence`: the shared [`verify_account_landing`] core —
///   same-site, non-root, and the username reappearing or the path/label
///   naming the account area.
/// * `AuthSignedOut`: the live auth probe reads signed out.
///
/// A failed browser read fails the verification closed (`false`): the
/// caller reports the honest miss.
pub async fn verify_verb<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    spec: &VerbSpec,
    label: Option<&str>,
    username: Option<&str>,
) -> bool {
    match spec.verifier {
        VerifierKind::UrlPathTokens(tokens) => {
            let Some(current) = browser.settings_current_url().await else {
                return false;
            };
            if url_path_has_tokens(&current, tokens) {
                return true;
            }
            // No navigation: the disclosure surface must name itself in
            // the fresh page's title or headings, or the action is not
            // evidence — same bar as the chrome worker.
            let (fresh, _, _) = browser.menu_snapshot(origin).await;
            let title = browser.settings_page_title().await;
            action_surface_visible(&fresh, title.as_deref(), spec.vocabulary)
        }
        VerifierKind::IdentityEvidence => {
            let Some(current) = browser.settings_current_url().await else {
                return false;
            };
            verify_account_landing(&current, origin, label.unwrap_or(""), username).is_ok()
        }
        VerifierKind::AuthSignedOut => browser.chrome_auth_state().await == AuthState::LoggedOut,
        VerifierKind::NotificationSurface => {
            let Some(current) = browser.settings_current_url().await else {
                return false;
            };
            let (fresh, _, _) = browser.menu_snapshot(origin).await;
            let title = browser.settings_page_title().await;
            verify_notifications_surface(&current, origin, title.as_deref(), &fresh)
        }
    }
}

/// Roles whose accessible name may carry the notifications vocabulary:
/// headings, landmarks and dialogs. AX only — never DOM text, container
/// rollups or descriptions.
const NOTIFICATION_NAME_ROLES: &[&str] = &[
    "heading",
    "dialog",
    "alertdialog",
    "region",
    "complementary",
    "navigation",
    "main",
];

/// Roles that make a revealed panel/overlay observable in place.
const OVERLAY_ROLES: &[&str] = &["dialog", "alertdialog", "menu", "complementary", "region"];

/// Strict notifications verifier, pure (no browser). All three must hold:
///
/// 1. **Same site**: `current` is on `origin`'s site.
/// 2. **A surface is observably present**: a non-root landing path, OR a
///    revealed panel/overlay (an overlay-role element in the AX snapshot).
/// 3. **Notification vocabulary** in the page title or in the accessible
///    name of an AX heading/landmark/dialog element.
///
/// Anything else is `false` — the honest miss.
#[must_use]
pub fn verify_notifications_surface(
    current: &url::Url,
    origin: &url::Url,
    title: Option<&str>,
    elements: &[AxElement],
) -> bool {
    let vocabulary = NOTIFICATIONS_SPEC.vocabulary;
    if !same_site_host(
        current.host_str().unwrap_or(""),
        origin.host_str().unwrap_or(""),
    ) {
        return false;
    }
    let non_root = !matches!(current.path(), "" | "/");
    let overlay = elements
        .iter()
        .any(|element| OVERLAY_ROLES.contains(&element.role.as_str()));
    if !non_root && !overlay {
        return false;
    }
    title.is_some_and(|title| text_mentions_vocabulary(title, vocabulary))
        || elements.iter().any(|element| {
            NOTIFICATION_NAME_ROLES.contains(&element.role.as_str())
                && text_mentions_vocabulary(&element.name, vocabulary)
        })
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
    /// Click the control like [`MenuBrowser::menu_click`], additionally
    /// journaling the click hit-test line (expected role+name vs what the
    /// click point resolved to) into `tried`. The default is the plain
    /// click with no journal — scripted fakes keep their shape; only the
    /// production [`ManagedBrowser`] impl overrides this.
    ///
    /// # Errors
    /// Returns [`IntentError::Browser`] on CDP failure.
    fn menu_click_reported(
        &self,
        element: &AxElement,
        tried: &mut Vec<String>,
    ) -> impl std::future::Future<Output = Result<(), IntentError>> + Send {
        let _ = tried;
        self.menu_click(element)
    }
    /// Dismiss any open popup layer (Escape keypress). A wrong menu left
    /// open by an earlier attempt light-dismisses on the next click —
    /// swallowing it instead of letting it reach the next candidate — so
    /// the worker calls this before spending another menu-opening click.
    /// Best-effort: implementations must not fail when nothing is open.
    fn menu_dismiss(&self) -> impl std::future::Future<Output = ()> + Send;
    /// Best-effort viewport screenshot as base64 JPEG with NO data-URI
    /// prefix; `None` when capture fails. Feeds the model loop's visual
    /// turn; a failed capture degrades to the text-only turn and must
    /// never fail the run.
    fn menu_screenshot(&self) -> impl std::future::Future<Output = Option<String>> + Send;
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
        click_element_reported(self, element).await.map(|_| ())
    }

    async fn menu_click_reported(
        &self,
        element: &AxElement,
        tried: &mut Vec<String>,
    ) -> Result<(), IntentError> {
        let outcome = click_element_reported(self, element).await?;
        tried.push(outcome.hit_line);
        Ok(())
    }

    async fn menu_dismiss(&self) {
        // Best-effort Escape: a popup light-dismisses; with nothing open
        // the keypress is a harmless no-op. Failures never fail the run.
        let _ = self.press_escape().await;
    }

    async fn menu_screenshot(&self) -> Option<String> {
        // Best-effort: the visual turn degrades to text-only when capture
        // fails, and the failure must never fail the run.
        self.viewport().await.map(|viewport| viewport.data).ok()
    }
}

/// Per-invocation click cap for the shared menu primitive: one candidate
/// per try, each effect-verified. Callers pass their own remaining
/// budget (the chrome worker passes its `CHROME_ACTION_MAX_CLICKS`
/// remainder); the primitive never exceeds this cap, so a hostile header
/// can't burn the run.
const MENU_OPEN_MAX_TRIES: usize = 3;

/// Ordered menu-opening candidates, pure decision. Two passes: the header
/// strip first, then below-strip content — account chrome lives in page
/// headers, and a feed-content button carrying a navigation landmark must
/// not outrank the header's account button (caught live on Reddit: the
/// worker clicked a post's "Open user actions" menu first, and the open
/// popup's light-dismiss then swallowed the avatar-menu click, so "log
/// out" never executed). Within each pass the legacy sub-order holds:
/// (a) landmarked banner/navigation buttons, then (b) account-worded
/// buttons ([`mentions_account_word`]), then — strip pass only —
/// (c) blank-named buttons inside the header strip — the unlabeled-avatar
/// case, structural (empty name + header geometry), never a control-name
/// string — then (d) the remaining unclicked buttons inside the header
/// strip, rightmost first. Tier (d) folds in the rightmost-geometry
/// fallback both lanes already had (`select_identity_control`'s second
/// tier and [`select_rightmost_button`]), so sharing the primitive doesn't
/// drop the named-but-wordless header button case; blank-named avatars
/// still outrank arbitrary named buttons. `rects` carries the
/// boundary-measured `(backend_node_id, x, y)` of the unclicked buttons;
/// `strip_bottom` is the viewport-relative header cutoff (`None` when
/// the viewport won't read — the strip pass stays empty and the
/// below-strip pass ranks in the legacy order, fail closed).
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
    // Header strip first, then page content: the account-menu trigger is
    // header chrome, never feed content.
    for strip_pass in [true, false] {
        let in_pass =
            |element: &AxElement| in_strip.contains_key(&element.backend_node_id) == strip_pass;
        // (a) landmarked header chrome.
        for element in elements.iter().filter(|element| {
            element.role == "button"
                && !already_clicked(clicked, element)
                && matches!(element.landmark.as_deref(), Some("banner" | "navigation"))
                && in_pass(element)
        }) {
            push(element);
        }
        // (b) account-worded buttons.
        for element in elements.iter().filter(|element| {
            element.role == "button"
                && !already_clicked(clicked, element)
                && mentions_account_word(element)
                && in_pass(element)
        }) {
            push(element);
        }
        if !strip_pass {
            continue;
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
    }
    ranked
}

/// Strong opener candidates for the worker's ambiguity gate (1b): the
/// same geometry inputs [`rank_menu_candidates`] takes, restricted to the
/// header strip, keeping only unclicked buttons that are "strong" — an
/// account-worded button ([`mentions_account_word`]) or a blank-named
/// in-strip button (the avatar case). Exactly one strong opener means the
/// deterministic lane may click; zero or 2+ defers to the model phase.
/// Pure and unit-tested.
#[must_use]
pub fn strong_openers<'a>(
    elements: &'a [AxElement],
    clicked: &[ClickedControl],
    rects: &[(i64, f64, f64)],
    strip_bottom: Option<f64>,
) -> Vec<&'a AxElement> {
    let in_strip: std::collections::HashSet<i64> = rects
        .iter()
        .filter(|(_, _, y)| strip_bottom.is_some_and(|bottom| *y <= bottom))
        .map(|(id, _, _)| *id)
        .collect();
    elements
        .iter()
        .filter(|element| {
            element.role == "button"
                && !already_clicked(clicked, element)
                && in_strip.contains(&element.backend_node_id)
                && (mentions_account_word(element) || element.name.trim().is_empty())
        })
        .collect()
}

/// Semantic opener disambiguation for the 1b gate's 2+ case: score the
/// ambiguous strong openers' accessible names against the "account menu"
/// hint and return the single winner at or above
/// [`crate::semantic::SEMANTIC_ACCEPT`], with its score for the journal.
/// Two or more above threshold is still ambiguous — `None` — as is none
/// above threshold. Blank names decline silently (no network), cached per
/// run by the matcher, names only.
pub async fn semantic_opener_winner<'a>(
    matcher: &mut crate::semantic::SemanticMatcher,
    openers: &[&'a AxElement],
) -> Option<(&'a AxElement, f32)> {
    let mut winner: Option<(&'a AxElement, f32)> = None;
    for opener in openers {
        let Some(score) = matcher.score(&opener.name).await else {
            continue;
        };
        if score < crate::semantic::SEMANTIC_ACCEPT {
            continue;
        }
        if winner.is_some() {
            // Two above threshold: still ambiguous.
            return None;
        }
        winner = Some((opener, score));
    }
    winner
}

/// Header-strip cutoff for opener geometry: the viewport-relative header
/// boundary [`open_identity_menu`], the chrome worker's opener ambiguity
/// gate, and the `LogOut` model candidate filter derive — one definition,
/// no drift. `None` when the viewport won't read: the strip pass stays
/// empty and ranking fails closed, exactly like the primitive's inline
/// version did.
async fn header_strip_bottom<B: MenuBrowser>(browser: &B) -> Option<f64> {
    browser
        .menu_viewport_size()
        .await
        .map(|(_, height)| height * HEADER_STRIP_FRACTION)
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
/// `preferred`, when `Some`, is clicked on the first attempt instead of
/// the ranking's top candidate: a caller that already resolved which
/// opener to click (the chrome worker's ambiguity gate) gets exactly
/// that candidate, never the ranking's independent favorite. Later
/// attempts rank normally — the preferred control is in `clicked` by
/// then, so it can't be re-clicked (toggling the menu shut). `None`
/// preserves the pure rank → click behavior.
///
/// # Errors
/// Returns [`IntentError::Browser`] on CDP failure.
pub async fn open_identity_menu<B: MenuBrowser>(
    browser: &B,
    origin: &url::Url,
    baseline: MenuOpenBaseline<'_>,
    clicked: &mut Vec<ClickedControl>,
    max_tries: usize,
    preferred: Option<&AxElement>,
) -> Result<OpenMenuOutcome, IntentError> {
    // Attempt baselines: the first attempt ranks the caller's snapshot
    // (no extra fetch); later attempts re-rank from the previous
    // attempt's final poll snapshot.
    let mut current: Option<(Vec<AxElement>, usize)> = None;
    let mut preferred = preferred;
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
        let strip_bottom = header_strip_bottom(browser).await;
        let rects = header_button_rects(browser, elements, clicked).await;
        // A caller-resolved opener goes first, exactly once: the
        // primitive's independent ranking must not substitute a
        // different candidate for the caller's choice. Later attempts
        // rank normally — the preferred control is in `clicked` by then,
        // so it can't be re-clicked (toggling the menu shut).
        let candidate = match preferred.take().filter(|p| !already_clicked(clicked, p)) {
            Some(winner) => winner.clone(),
            None => {
                match rank_menu_candidates(elements, clicked, &rects, strip_bottom)
                    .into_iter()
                    .next()
                {
                    Some(candidate) => candidate.clone(),
                    // Every candidate already tried: re-clicking would only
                    // toggle a menu shut.
                    None => break,
                }
            }
        };

        let tried_key = ClickedControl::of(&candidate);
        browser.menu_click_reported(&candidate, &mut tried).await?;
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

/// Async browser seam for the spec-parameterized chrome worker
/// ([`pursue_chrome_action`]): the CDP operations the shared menu
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
            hit_lines: Vec::new(),
        })
    } else {
        Err(format!(
            "account-home landing failed verification: at {current} (label '{label}')"
        ))
    }
}

async fn verify_account_home<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    label: &str,
    landed: &url::Url,
    username: Option<&str>,
) -> Result<PageGoalOutcome, IntentError> {
    // The live page is the truth: re-read the URL after the navigation
    // and verify that, not the URL we asked for.
    let current = browser
        .settings_current_url()
        .await
        .unwrap_or_else(|| landed.clone());
    verify_account_landing(&current, origin, label, username).map_err(IntentError::NoMatch)
}

/// One tried-click log line: role, truncated name, observed effect.
#[must_use]
pub fn tried_label(element: &AxElement, effect: &str) -> String {
    let name: String = element.name.chars().take(40).collect();
    format!("{} '{name}' → {effect}", element.role)
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
    // Every click's `click_hit_test:` line, kept for the miss diagnostic.
    let mut tried: Vec<String> = Vec::new();
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
            tried.push(click_element_reported(browser, target).await?.hit_line);
            clicked.push(tried_key);
            if let Some(landed) = wait_for_url_change(browser).await {
                return Ok(PageGoalOutcome::Navigated {
                    label,
                    landed,
                    hit_lines: Vec::new(),
                }
                .with_hit_lines(&tried));
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
        match open_identity_menu(
            browser,
            origin,
            baseline,
            &mut clicked,
            MENU_OPEN_MAX_TRIES,
            None,
        )
        .await?
        {
            OpenMenuOutcome::Opened { tried: line, .. } => {
                tried.push(line);
                continue;
            }
            OpenMenuOutcome::Miss { tried: lines } => tried.extend(lines),
        }
        if let Some(menu) = select_menu_button(&elements, &clicked) {
            let tried_key = ClickedControl::of(menu);
            tried.push(click_element_reported(browser, menu).await?.hit_line);
            clicked.push(tried_key);
            continue;
        }
        if let Some(rightmost) = select_rightmost_button(browser, &elements, &clicked).await {
            let tried_key = ClickedControl::of(rightmost);
            tried.push(click_element_reported(browser, rightmost).await?.hit_line);
            clicked.push(tried_key);
            continue;
        }
        // 3. Nothing left to try.
        return Err(IntentError::NoMatch(with_tried(
            page_goal_diagnostic(&elements, noun),
            &tried,
        )));
    }
    Err(IntentError::NoMatch(with_tried(
        format!("In-page goal '{noun}' not reached after {PAGE_GOAL_MAX_STEPS} steps."),
        &tried,
    )))
}

/// `diagnostic` with the tried-click lines appended, when there are any.
fn with_tried(diagnostic: String, tried: &[String]) -> String {
    if tried.is_empty() {
        diagnostic
    } else {
        format!("{diagnostic}; tried [{}]", tried.join("; "))
    }
}

/// Escalation for gear 2's model loop: when the main pass's tail fails
/// verification and this is `Some`, exactly one additional bounded model
/// pass runs under `navigator` before the honest miss. A verified main
/// tail never escalates; the escalation pass itself never escalates
/// further. Built by the desktop service from
/// [`orchestration_engine::LlmPageNavigator::escalation_from_env`]; unset
/// keeps today's behavior.
#[derive(Clone)]
pub struct ModelEscalation {
    /// The escalation navigator: a stronger model behind the same
    /// [`crate::navigator::PageNavigator`] fence.
    pub navigator: std::sync::Arc<dyn crate::navigator::PageNavigator>,
    /// Model label for the journal line (`escalated to <model>`).
    pub model_name: String,
}

/// Mutable state accumulated across one [`pursue_with_model`] run's model
/// passes: the click log, the tried journal, the last clicked label, and
/// the page-revealed username candidate. Shared between the main pass
/// and the escalation pass so the journal is one continuous record.
#[derive(Default)]
struct ModelLoopState {
    clicked: Vec<ClickedControl>,
    tried: Vec<String>,
    last_label: Option<String>,
    revealed_username: Option<String>,
}

/// Gear 2: the generalist agent loop — observe, propose, act.
///
/// Per turn:
/// 1. **Observe**: untruncated AX snapshot via [`MenuBrowser::menu_snapshot`]
///    (a revealed menu renders at document end, past the head truncation
///    the old phase-2 loop used — the pick is validated against the same
///    full list the loop observes), plus a best-effort viewport screenshot
///    for the visual turn (a failed capture degrades to the text-only
///    turn, never fails the run).
/// 2. **Propose**: the navigator picks exactly ONE [`PageAction`] from the
///    rendered head slice ([`MAX_NAVIGATOR_ELEMENTS`] lines, zones
///    included) via [`crate::navigator::PageNavigator::next_action_visual`].
///    The optional `spec` adds the verb's closed vocabulary as a prompt
///    hint — selection and execution stay deterministic.
/// 3. **Act**: deterministic Rust validates the picked id against the live
///    snapshot and clicks it. Unknown ids decline, never guess; an
///    already-clicked re-pick declines instead of toggling a menu shut.
///
/// Bounded budget: `max_steps` steps (see [`model_loop_pass`]), then the
/// loop stops.
/// The model NEVER declares completion: [`PageAction::Done`] is a decline —
/// the loop stops acting and falls through to verification. Completion is
/// decided ONLY by the verifier: with `spec` present [`verify_verb`] runs
/// after every click (an early verified landing ends the loop instead of
/// burning the remaining budget) and once more at the tail, so a decline
/// on an already-correct page still completes — the verifier decided, not
/// the model. Without a spec (the generic noun hunt) no verifier exists,
/// so a navigation ends the loop as `Navigated`, like the old phase-2
/// contract.
///
/// Escalation: when the main pass's tail fails verification and
/// `escalation` is `Some`, one additional bounded pass runs under the
/// escalation navigator (journaled as `escalated to <model>`), then the
/// run falls through to the honest miss carrying the journal. Exactly one
/// escalation per run; a verified main tail never escalates.
///
/// [`IntentError::NoMatch`] with the loop's tried-click journal when the
/// goal is not verified; [`IntentError::Browser`] on CDP failure.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] when the goal is not reached or not
/// verified, and [`IntentError::Browser`] on CDP failure.
pub async fn pursue_with_model<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    goal: &str,
    navigator: std::sync::Arc<dyn crate::navigator::PageNavigator>,
    spec: Option<&VerbSpec>,
    deterministic_miss: String,
    escalation: Option<ModelEscalation>,
) -> Result<PageGoalOutcome, IntentError> {
    let mut state = ModelLoopState::default();
    // The verb's policy sets the budget and escalation: the bounded chain
    // (log out) runs one short model pass, no escalation — its
    // deterministic backstop makes an extra pass pure model-call burn.
    let policy = spec.map_or(IdentityMenuPolicy::STANDARD, VerbSpec::menu_policy);
    let max_steps = policy.model_max_steps;
    let escalation = if policy.model_escalation {
        escalation
    } else {
        None
    };
    let main_miss = match model_loop_pass(
        browser,
        origin,
        goal,
        navigator,
        spec,
        max_steps,
        &deterministic_miss,
        &mut state,
    )
    .await
    {
        ok @ Ok(_) => return ok,
        // Browser errors fail fast: escalating on a dead CDP session
        // would just burn model calls.
        Err(browser @ IntentError::Browser(_)) => return Err(browser),
        Err(miss @ IntentError::NoMatch(_)) => miss,
    };
    let Some(escalation) = escalation else {
        return Err(main_miss);
    };
    state
        .tried
        .push(format!("escalated to {}", escalation.model_name));
    // The escalation pass shares the accumulated state, so the journal
    // stays one continuous record; the tail renders it into the honest
    // miss when verification fails again.
    model_loop_pass(
        browser,
        origin,
        goal,
        escalation.navigator,
        spec,
        max_steps,
        &deterministic_miss,
        &mut state,
    )
    .await
}

/// Click lane for [`model_loop_pass`]: grounds the navigator's pick
/// against the full snapshot via [`model_loop_click`]. Returns `Some`
/// when the click decided the run, `None` to keep looping.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] on an ungroundable pick, and
/// [`IntentError::Browser`] on CDP failure.
// Eight inputs: the click's own; the state's fields travel as one borrow
// so the call site stays readable — the `too_many_arguments` shape this
// crate already uses for its click helpers.
#[allow(clippy::too_many_arguments)]
async fn model_loop_click_arm<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    goal: &str,
    spec: Option<&VerbSpec>,
    elements: &[AxElement],
    target: i64,
    deterministic_miss: &str,
    state: &mut ModelLoopState,
) -> Result<Option<PageGoalOutcome>, IntentError> {
    model_loop_click(
        browser,
        origin,
        goal,
        spec,
        elements,
        target,
        deterministic_miss,
        &mut state.clicked,
        &mut state.tried,
        &mut state.last_label,
        &mut state.revealed_username,
    )
    .await
}

/// Exit lane for [`model_loop_pass`]: every way out of the acting loop —
/// a navigator decline, a give-up, a `Done` claim, or an exhausted step
/// budget — runs the verification tail with the journal intact, so the
/// verifier, never the model, decides completion.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] when the goal is not reached or not
/// verified, and [`IntentError::Browser`] on CDP failure.
async fn model_loop_exit<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    spec: Option<&VerbSpec>,
    goal: &str,
    deterministic_miss: &str,
    state: &ModelLoopState,
    reason: &str,
) -> Result<PageGoalOutcome, IntentError> {
    model_loop_tail(
        browser,
        origin,
        spec,
        goal,
        deterministic_miss,
        &state.tried,
        state.last_label.as_deref(),
        state.revealed_username.as_deref(),
        reason,
    )
    .await
}

/// One bounded model pass: the observe → propose → act loop shared by
/// gear 2's main pass and the escalation pass, so the click/verify logic
/// is not duplicated. `state` accumulates across passes; the pass ends in
/// [`model_loop_tail`], whose [`IntentError::NoMatch`] carries the
/// journal. Exactly one pass escalates — this helper never escalates
/// itself. `max_steps` is the acting budget: [`MODEL_GOAL_MAX_STEPS`]
/// from the verb's [`IdentityMenuPolicy`].
///
/// # Errors
/// Returns [`IntentError::NoMatch`] when the goal is not reached or not
/// verified, and [`IntentError::Browser`] on CDP failure.
// Eight parameters: the pass's own inputs plus the step budget; the
// `too_many_arguments` shape this crate already uses for its loop
// helpers.
#[allow(clippy::too_many_arguments)]
async fn model_loop_pass<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    goal: &str,
    navigator: std::sync::Arc<dyn crate::navigator::PageNavigator>,
    spec: Option<&VerbSpec>,
    max_steps: usize,
    deterministic_miss: &str,
    state: &mut ModelLoopState,
) -> Result<PageGoalOutcome, IntentError> {
    use crate::navigator::PageAction;
    // Neutral acting state: the previous phase may have left a wrong menu
    // open, whose light-dismiss would swallow the pass's first click.
    // Best-effort no-op when nothing is open.
    browser.menu_dismiss().await;
    let policy = spec.map_or(IdentityMenuPolicy::STANDARD, VerbSpec::menu_policy);
    // The header filter's fail-safe line is journaled once per pass.
    let mut filter_fallback_journaled = false;

    for _ in 0..max_steps {
        // Untruncated: the pick is validated against the same full list
        // the loop observed — a control past the head truncation is
        // clickable when the model names it.
        let (elements, _, _) = browser.menu_snapshot(origin).await;
        let prompt_goal = model_goal_text(goal, spec);
        // The navigator renders the head slice; zones describe exactly
        // that slice, best-effort — unzoned on failure.
        let mut head: Vec<AxElement> = elements
            .iter()
            .take(crate::MAX_NAVIGATOR_ELEMENTS)
            .cloned()
            .collect();
        // One measurement round feeds both the zones and the header
        // filter — never a second CDP pass.
        let points = model_zone_points(browser, &head).await;
        let mut zones = zones_from_points(&points);
        // Policy-gated guard: offer the model only header-strip
        // candidates (see [`header_strip_model_filter`]); the live miss
        // clicked a feed ad's options button from this very turn.
        if policy.header_strip_filter
            && let Some(strip_bottom) = header_strip_bottom(browser).await
        {
            (head, zones) = header_strip_model_filter(
                head,
                zones,
                &points,
                strip_bottom,
                policy.journal_tag,
                &mut state.tried,
                &mut filter_fallback_journaled,
            );
        }
        // Phase 2: the visual turn — a best-effort viewport screenshot
        // accompanies the element list. The capture must never fail the
        // run: `None` degrades to the text-only turn.
        let screenshot = browser.menu_screenshot().await;
        // The navigator is synchronous (one bounded HTTP call); the async
        // runtime never blocks on it. Everything the closure touches is
        // owned, so the future stays `'static`.
        let owned_navigator = navigator.clone();
        let action = tokio::task::spawn_blocking(move || {
            owned_navigator.next_action_visual(&prompt_goal, &head, &zones, screenshot.as_deref())
        })
        .await
        .map_err(|_| {
            IntentError::NoMatch(format!("navigator task failed; {deterministic_miss}"))
        })?;
        // A decline stops the acting loop; the verification tail — never
        // the model — decides completion.
        let decline_reason: Option<String> = match &action {
            None => Some("navigator declined".to_owned()),
            Some(PageAction::GiveUp { reason }) => Some(format!("navigator gave up ({reason})")),
            // Done is a decline, not a completion claim: the model never
            // declares the goal achieved. The tail still runs the
            // verifier, so a correct page completes on evidence.
            Some(PageAction::Done) => Some("navigator done (treated as decline)".to_owned()),
            Some(PageAction::Click { .. }) => None,
        };
        if let Some(reason) = decline_reason {
            return model_loop_exit(
                browser,
                origin,
                spec,
                goal,
                deterministic_miss,
                state,
                &reason,
            )
            .await;
        }
        if let Some(PageAction::Click { target }) = action
            && let Some(outcome) = model_loop_click_arm(
                browser,
                origin,
                goal,
                spec,
                &elements,
                target,
                deterministic_miss,
                state,
            )
            .await?
        {
            return Ok(outcome);
        }
    }
    model_loop_exit(
        browser,
        origin,
        spec,
        goal,
        deterministic_miss,
        state,
        &format!("navigator exhausted {max_steps} steps"),
    )
    .await
}

/// One model-picked click inside [`pursue_with_model`]: validate the
/// picked id against the live snapshot, gather identity-lane evidence,
/// click, watch for navigation, run the per-click verifier.
///
/// Returns `Some` when the loop ends here — a decline (unknown id or a
/// re-picked control), a verified landing, or the generic lane's
/// navigation — and `None` to keep observing.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] for an unknown element id and
/// [`IntentError::Browser`] on CDP failure.
#[allow(clippy::too_many_arguments)]
async fn model_loop_click<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    goal: &str,
    spec: Option<&VerbSpec>,
    elements: &[AxElement],
    target: i64,
    deterministic_miss: &str,
    clicked: &mut Vec<ClickedControl>,
    tried: &mut Vec<String>,
    last_label: &mut Option<String>,
    revealed_username: &mut Option<String>,
) -> Result<Option<PageGoalOutcome>, IntentError> {
    let element = elements
        .iter()
        .find(|element| element.backend_node_id == target)
        .ok_or_else(|| {
            IntentError::NoMatch(format!(
                "navigator picked unknown element {target}; {deterministic_miss}"
            ))
        })?;
    if already_clicked(clicked, element) {
        return model_loop_tail(
            browser,
            origin,
            spec,
            goal,
            deterministic_miss,
            tried,
            last_label.as_deref(),
            revealed_username.as_deref(),
            &format!(
                "navigator re-picked an already-clicked control ({}); {deterministic_miss}",
                tried_label(element, "already clicked"),
            ),
        )
        .await
        .map(Some);
    }
    let label = element.name.clone();
    // Identity-lane evidence, read before the click.
    if revealed_username.is_none()
        && matches!(
            spec.map(|spec| spec.verifier),
            Some(VerifierKind::IdentityEvidence)
        )
        && let Some(href) = browser.settings_node_href(element.backend_node_id).await
        && let Some(url) = validate_revealed_href(&href, origin)
        && let Some(name) = username_from_href(&url)
    {
        *revealed_username = Some(name);
    }
    if revealed_username.is_none()
        && matches!(
            spec.map(|spec| spec.verifier),
            Some(VerifierKind::IdentityEvidence)
        )
    {
        *revealed_username = username_from_menu_text(&label);
    }
    browser.menu_click_reported(element, tried).await?;
    clicked.push(ClickedControl::of(element));
    *last_label = Some(label.clone());
    let navigated = wait_for_url_change(browser).await;
    if let Some(spec) = spec {
        // The verifier decides after every click: an early
        // verified landing ends the loop instead of burning
        // the remaining budget on clicks that could navigate
        // away again.
        if verify_verb(
            browser,
            origin,
            spec,
            Some(&label),
            revealed_username.as_deref(),
        )
        .await
        {
            let landed = browser
                .settings_current_url()
                .await
                .unwrap_or_else(|| origin.clone());
            return Ok(Some(
                PageGoalOutcome::Verified {
                    label,
                    landed,
                    username: revealed_username.clone(),
                    hit_lines: Vec::new(),
                }
                .with_hit_lines(tried),
            ));
        }
        tried.push(tried_label(
            element,
            if navigated.is_some() {
                "clicked, navigated, verifier failed"
            } else {
                "clicked, no navigation, verifier failed"
            },
        ));
    } else if let Some(landed) = navigated {
        // Generic noun hunt: no verifier exists, so a
        // navigation ends the loop as before.
        return Ok(Some(
            PageGoalOutcome::Navigated {
                label,
                landed,
                hit_lines: Vec::new(),
            }
            .with_hit_lines(tried),
        ));
    } else {
        tried.push(tried_label(element, "clicked, no navigation"));
    }
    Ok(None)
}

/// Goal text for one model turn: the caller's goal plus, when a verb spec
/// is present, the verb's closed vocabulary as a hint. The vocabulary is
/// generic words from the spec table — never site names, selectors, or
/// procedures — and the model still only proposes actions; selection and
/// execution stay deterministic.
fn model_goal_text(goal: &str, spec: Option<&VerbSpec>) -> String {
    match spec {
        None => goal.to_owned(),
        Some(spec) => format!(
            "{goal} [verb hint: {}; destination words: {}; prefer header/account chrome (top of the page) over feed content controls]",
            spec.kind.as_str(),
            spec.vocabulary.join(", ")
        ),
    }
}

/// Measured viewport centers for the model phase over the menu seam:
/// one `(x, y)` center per element, in order; `(NaN, NaN)` when the rect
/// won't resolve. Measured through [`MenuBrowser::menu_node_rect`].
/// Best-effort and time-bounded — geometry that won't resolve degrades to
/// unmeasured points, never a stall. Split out so the header-strip
/// filter reuses the same measurement round instead of paying a second
/// CDP pass.
async fn model_zone_points<B: MenuBrowser>(browser: &B, elements: &[AxElement]) -> Vec<(f64, f64)> {
    const ZONE_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
    let ids: Vec<i64> = elements
        .iter()
        .map(|element| element.backend_node_id)
        .collect();
    tokio::time::timeout(ZONE_BUDGET, async {
        let mut points: Vec<(f64, f64)> = Vec::new();
        for id in &ids {
            if let Ok(highlight) = browser.menu_node_rect(*id).await {
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
    .unwrap_or_else(|_| vec![(f64::NAN, f64::NAN); ids.len()])
}

/// Coarse position zones from measured viewport centers: the pure half of
/// the model phase's geometry. Distribution-relative zoning — each
/// rendered control's center zoned against the bounding box of the
/// measured control set, so the model can pick an unnamed avatar button
/// by its header position instead of guessing. An empty finite set (or a
/// fully unmeasured round) degrades to unzoned lines, never a stall.
#[must_use]
fn zones_from_points(points: &[(f64, f64)]) -> Vec<Option<crate::navigator::PositionZone>> {
    use crate::navigator::zone_for;
    let finite: Vec<(f64, f64)> = points
        .iter()
        .copied()
        .filter(|(x, y)| x.is_finite() && y.is_finite())
        .collect();
    if finite.is_empty() {
        return vec![None; points.len()];
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

/// Header-strip model-phase candidate filter (policy-gated), pure: keep the head element ids
/// whose rect center-y sits inside the header strip (at or above
/// `strip_bottom`). The account menu renders near the header, so
/// feed/ad/main-content controls below the strip are never identity-menu
/// candidates — this is the guard that keeps a generalist model from
/// clicking a feed ad's options button when it should be opening the
/// account menu. Unmeasurable centers (NaN) are excluded: a control with
/// no geometry can't be placed in the header. Returns the kept ids; an
/// empty result means the caller falls back to the unfiltered list —
/// never blind the model.
#[must_use]
pub fn header_strip_candidates(ids: &[i64], y_centers: &[f64], strip_bottom: f64) -> Vec<i64> {
    ids.iter()
        .zip(y_centers.iter())
        .filter(|(_, y)| y.is_finite() && **y <= strip_bottom)
        .map(|(id, _)| *id)
        .collect()
}

/// Header-strip guard application for [`model_loop_pass`]: narrow the
/// navigator's head slice (and its 1:1 zones) to header-strip candidates
/// via [`header_strip_candidates`], so the generalist can't wander into
/// feed/ad controls — the live miss clicked a feed ad's options button
/// from this very turn. An emptied list keeps the full head and journals
/// one line per pass (`fallback_journaled`) instead of blinding the
/// model; the caller fails closed to the unfiltered head when the
/// viewport won't read. Returns the (possibly filtered) head and zones,
/// still aligned.
fn header_strip_model_filter(
    head: Vec<AxElement>,
    zones: Vec<Option<crate::navigator::PositionZone>>,
    points: &[(f64, f64)],
    strip_bottom: f64,
    tag: &str,
    tried: &mut Vec<String>,
    fallback_journaled: &mut bool,
) -> (Vec<AxElement>, Vec<Option<crate::navigator::PositionZone>>) {
    let ids: Vec<i64> = head.iter().map(|element| element.backend_node_id).collect();
    let y_centers: Vec<f64> = points.iter().map(|(_, y)| *y).collect();
    let kept: std::collections::HashSet<i64> =
        header_strip_candidates(&ids, &y_centers, strip_bottom)
            .into_iter()
            .collect();
    if kept.is_empty() {
        if !*fallback_journaled {
            *fallback_journaled = true;
            tried.push(format!(
                "{tag} model filter found no header-strip candidates; showing the full list"
            ));
        }
        return (head, zones);
    }
    let mut filtered_head = Vec::with_capacity(kept.len());
    let mut filtered_zones = Vec::with_capacity(kept.len());
    for (element, zone) in head.into_iter().zip(zones) {
        if kept.contains(&element.backend_node_id) {
            filtered_head.push(element);
            filtered_zones.push(zone);
        }
    }
    (filtered_head, filtered_zones)
}

/// Verification tail of the generalist loop: runs after the budget is
/// exhausted or the navigator declines (including [`PageAction::Done`]).
/// With `spec` present the verb's verifier decides — a decline on an
/// already-correct page still completes as `Verified`, because the
/// verifier decided, not the model. Without a spec (the generic noun
/// hunt) no verifier exists, so a decline is the honest miss.
#[allow(clippy::too_many_arguments)]
async fn model_loop_tail<B: ChromeActionBrowser>(
    browser: &B,
    origin: &url::Url,
    spec: Option<&VerbSpec>,
    goal: &str,
    deterministic_miss: &str,
    tried: &[String],
    last_label: Option<&str>,
    revealed_username: Option<&str>,
    note: &str,
) -> Result<PageGoalOutcome, IntentError> {
    let journal = if tried.is_empty() {
        format!("{note}; {deterministic_miss}")
    } else {
        format!("{note}; tried [{}]; {deterministic_miss}", tried.join("; "))
    };
    let Some(spec) = spec else {
        return Err(IntentError::NoMatch(journal));
    };
    if verify_verb(
        browser,
        origin,
        spec,
        last_label.or(Some(goal)),
        revealed_username,
    )
    .await
    {
        let landed = browser
            .settings_current_url()
            .await
            .unwrap_or_else(|| origin.clone());
        return Ok(PageGoalOutcome::Verified {
            label: last_label.unwrap_or(goal).to_owned(),
            landed,
            username: revealed_username.map(str::to_owned),
            hit_lines: Vec::new(),
        }
        .with_hit_lines(tried));
    }
    Err(IntentError::NoMatch(journal))
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
pub async fn wait_for_candidates_with<F, Fut>(
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
    click_element_reported(browser, &resolved.element).await
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
pub fn url_drifted(initial: &url::Url, now: &url::Url) -> bool {
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
pub fn halted_early(
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
        outcomes.push(click_element_reported(browser, element).await?);
        if outcomes.len() < batch.len() {
            tokio::time::sleep(std::time::Duration::from_millis(BATCH_SETTLE_MS)).await;
        }
    }
    Ok(ExecuteOutcome::Completed(outcomes))
}

/// Badge one resolved element and click it through the reported click:
/// rect resolution, visible mark, press, plus the click hit-test journal
/// line (the expected role+name vs what the click point resolved to —
/// role+name, never backend node ids). The badge stays visible on
/// success as evidence, failures clear it so no stale overlay survives.
///
/// # Errors
/// Returns [`IntentError::Browser`] on CDP failure.
async fn click_element_reported(
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
    // The reported click: the same hover → press → release as
    // `click_mark`, plus what the click point actually resolved to
    // (page-side `elementFromPoint`) for the journal. The probe is pure
    // diagnostic — it never fails the click.
    let hit = match browser.click_mark_reported(&mark).await {
        Ok(hit) => hit,
        Err(error) => {
            let _ = browser.clear_marks().await;
            return Err(error.into());
        }
    };
    let hit_line = hit.journal_line(&element.role, &element.name);
    Ok(IntentOutcome {
        mark,
        highlight,
        hit_line,
    })
}
