//! Verb spec table: verb → {match vocabulary, verifier kind}.
//!
//! The closed noun table that routes an artifact noun to its verb spec —
//! the vocabulary that reveals the verb's destination in the page's
//! identity chrome and the verifier that proves the action landed. Verbs
//! are few, sites are millions: per-verb strategies, nothing per-site.
//! This replaces the closed `GoalClass` → worker mapping with data; a new
//! verb is a new row in the macro-engine's table, not a new worker.
//!
//! The spec types themselves ([`VerbSpec`], [`VerbKind`], [`VerifierKind`])
//! live in `macro-engine` next to the worker that consumes them (the
//! dependency runs orchestration → macro, so the leaf crate owns the
//! contract); this module re-exports them and adds the noun routing.

pub use macro_engine::{VerbKind, VerbSpec, VerifierKind, verb_specs};

/// Closed noun → verb spec mapping. Identity and settings nouns qualify;
/// log-out phrases qualify; every other artifact noun keeps the generic
/// noun-hunt path, so a new verb is an explicit product decision, not an
/// emergent match. Both the funnel's canonical nouns ("settings") and the
/// follow-up detector's stemmed nouns ("setting") map, since both reach
/// this table. Matching is case-insensitive; multi-word vocabulary
/// ("log out") matches the whole noun.
#[must_use]
pub fn spec_for_noun(noun: &str) -> Option<&'static VerbSpec> {
    macro_engine::verb_specs()
        .iter()
        .find(|spec| spec.matches_noun(noun))
}

/// The site context of a verb-led action's remainder: one leading
/// preposition stripped ("from reddit" → "reddit"), `None` when nothing
/// site-like remains — a bare verb, or an aside-only remainder ("log out
/// for me" → "me", a pronoun, not a site). Generic English
/// prepositions/pronouns only — no site list. Pure, so the dispatcher and
/// the tests share it.
#[must_use]
pub fn verb_site_context(remainder: &str) -> Option<String> {
    const PREPOSITIONS: &[&str] = &["from", "of", "on", "in", "at", "for"];
    const PRONOUNS: &[&str] = &["me", "you", "us", "myself", "yourself"];
    // A trailing politeness marker ("log out please") names no site.
    // Case-insensitive: the detector lowercases its remainder, but this
    // stays robust as a public helper. "please" alone leaves nothing.
    let lowered = remainder.trim().to_lowercase();
    let text = strip_trailing_please(&lowered);
    if text.is_empty() {
        return None;
    }
    let mut words = text.split_whitespace();
    let stripped = match words.next() {
        Some(first) if PREPOSITIONS.contains(&first) => words.collect::<Vec<_>>().join(" "),
        _ => text.to_owned(),
    };
    if stripped.is_empty() || PRONOUNS.contains(&stripped.as_str()) {
        None
    } else {
        Some(stripped)
    }
}

/// Strip repeated trailing "please" politeness markers (`s` is already
/// lowercased by the caller). A borrow-safe loop: each iteration shortens
/// the slice, so no clone-into-borrowed-text is needed.
fn strip_trailing_please(s: &str) -> &str {
    let mut rest = s;
    loop {
        match rest.strip_suffix("please") {
            Some(before) => rest = before.trim_end(),
            None => return rest,
        }
    }
}

/// A verb-led prompt: the prompt starts with a verb phrase from a spec's
/// closed vocabulary, and the rest names the site context. Pure — the
/// caller grounds the site text through the normal ladder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerbLedAction {
    /// The matched verb spec (vocabulary + verifier).
    pub spec: &'static VerbSpec,
    /// The prompt's remainder after the verb phrase, trimmed ("from
    /// reddit" in "log out from reddit"; empty for a bare "log out").
    pub site_text: String,
}

/// Verb-led action detection: the prompt must START with a verb phrase
/// from a spec's closed vocabulary ("log out", "log off", "sign out") —
/// the phrase is matched with a word boundary, so "logout" (one word) or
/// "log outerwear" never match. The site context is the verbatim
/// remainder; the dispatcher grounds it through the normal site ladder,
/// so no site list lives here.
///
/// Case-insensitive; internal whitespace is collapsed before matching so
/// "log   out" still matches. `None` for anything not verb-led — the
/// funnel and the normal ladder own those prompts.
#[must_use]
pub fn detect_verb_led_action(prompt: &str) -> Option<VerbLedAction> {
    let normalized: String = prompt
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    // Log-out only: this is the verb-led preemption the bug report
    // motivated ("log out me from the reddit" misrouted through the
    // funnel). Other verbs keep their noun-led dispatch — the vocabulary
    // still comes from the canonical log-out spec row, not a separate
    // phrase list.
    let spec = macro_engine::VerbSpec::for_kind(macro_engine::VerbKind::LogOut);
    // Longest phrase first so multi-word vocabulary wins over any
    // shorter prefix a future row might share.
    let mut phrases: Vec<&&str> = spec.vocabulary.iter().collect();
    phrases.sort_by_key(|phrase| std::cmp::Reverse(phrase.len()));
    for phrase in phrases {
        if normalized.len() < phrase.len() {
            continue;
        }
        let (head, rest) = normalized.split_at(phrase.len());
        if head != *phrase {
            continue;
        }
        // Word boundary: the phrase must end the prompt or be followed
        // by a non-word character — "log outerwear" is not "log out".
        if rest
            .chars()
            .next()
            .is_some_and(|next| next.is_alphanumeric() || next == '_')
        {
            continue;
        }
        return Some(VerbLedAction {
            spec,
            site_text: rest.trim().to_owned(),
        });
    }
    None
}
