#![deny(unsafe_code)]
//! Deterministic compound-prompt splitting for multi-action commands.
//!
//! A compound prompt chains two or more verb-led actions with a
//! coordinating separator: `"log out me from the reddit and re-open the
//! reddit"`. [`split_compound`] splits on the separators and requires every
//! segment to be verb-led against a small GENERIC verb list — no site
//! names, no artifact nouns, no parsing heuristics. Anything that is not a
//! clean multi-action chain returns `None`, so the caller keeps
//! single-prompt behavior for everything else.

/// Coordinating separators, longest first so `"and then"` wins over
/// `"and"`. Matched case-insensitively on whole-word boundaries (the
/// boundary check supplies the surrounding spaces, so `"stand"` never
/// matches `"and"`).
const SEPARATORS: [&str; 3] = ["and then", "then", "and"];

/// Generic verb phrases that may lead a compound segment. Action verbs
/// only — no site names, no artifact nouns. Longest first so `"log me
/// out"` matches before its prefix `"log out"`.
const COMPOUND_VERBS: [&str; 15] = [
    "log me out",
    "sign me out",
    "log out",
    "sign out",
    "take me to",
    "bring me to",
    "go to",
    "navigate to",
    "re-open",
    "reopen",
    "open",
    "close",
    "visit",
    "launch",
    "show",
];

/// A word character for boundary purposes: anything alphanumeric glues to
/// its neighbors, so `"wand"` never matches `" and "`.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// Whether `phrase` occurs in `lowered` at `start` on whole-word
/// boundaries: the characters immediately before and after the match must
/// not be alphanumeric.
fn phrase_at(lowered: &[char], start: usize, phrase: &str) -> bool {
    let needle: Vec<char> = phrase.chars().collect();
    if start + needle.len() > lowered.len() {
        return false;
    }
    if lowered[start..start + needle.len()] != needle[..] {
        return false;
    }
    if start > 0 && is_word_char(lowered[start - 1]) {
        return false;
    }
    if start + needle.len() < lowered.len() && is_word_char(lowered[start + needle.len()]) {
        return false;
    }
    true
}

/// Whether the trimmed segment starts with one of [`COMPOUND_VERBS`] on a
/// whole-word boundary. Multi-word verbs match the segment's leading words
/// (`"log out me from the reddit"` and `"take me to the settings"` are
/// verb-led); a verb that is merely a prefix of a longer word
/// (`"opener"`) is not. Case-insensitive.
fn is_verb_led(segment: &str) -> bool {
    let lowered: Vec<char> = segment
        .chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect();
    COMPOUND_VERBS
        .iter()
        .any(|verb| phrase_at(&lowered, 0, verb))
}

/// Trim a char slice and push it when non-empty.
fn push_trimmed(segments: &mut Vec<String>, chars: &[char]) {
    let text: String = chars.iter().collect();
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_owned());
    }
}

/// Split a compound prompt into its verb-led action segments.
///
/// Splits on `" and then "` / `" then "` / `" and "` (case-insensitive,
/// whole-word, longest match first), trims the pieces, and drops empties.
/// Returns the segments in order (case preserved) when at least two remain
/// AND every one is verb-led against [`COMPOUND_VERBS`]; `None` otherwise,
/// so the caller keeps single-prompt behavior for non-compound prompts
/// (`"open reddit for me"` → `None`) and for prompts whose segments are
/// not actions (`"peanut butter and jelly"` → `None`).
///
/// Pure and total: no site names, no nouns, no network, no model.
#[must_use]
pub fn split_compound(prompt: &str) -> Option<Vec<String>> {
    // Char vectors with a 1:1 index mapping, so separator spans found on
    // the lowered copy slice the case-preserving original exactly.
    let original: Vec<char> = prompt.chars().collect();
    let lowered: Vec<char> = original
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();

    // Scan left to right; at each position the longest separator wins.
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    let mut index = 0;
    while index < lowered.len() {
        let hit = SEPARATORS
            .iter()
            .find(|sep| phrase_at(&lowered, index, sep))
            .map(|sep| sep.chars().count());
        if let Some(len) = hit {
            cuts.push((index, index + len));
            index += len;
        } else {
            index += 1;
        }
    }

    let mut segments = Vec::new();
    let mut prev = 0;
    for (start, end) in cuts {
        push_trimmed(&mut segments, &original[prev..start]);
        prev = end;
    }
    push_trimmed(&mut segments, &original[prev..]);

    if segments.len() < 2 {
        return None;
    }
    if !segments.iter().all(|segment| is_verb_led(segment)) {
        return None;
    }
    Some(segments)
}
