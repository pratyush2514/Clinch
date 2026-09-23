#![deny(unsafe_code)]
//! Natural-language command routing to saved Playbooks.
//!
//! Saved-workflow routing stays deterministic keyword matching — never a model
//! call — so replay stays instant and free. Ephemeral intents use an AI
//! structured-extraction pass (local model / Ollama via a provider adapter)
//! with an instant deterministic fallback, so free-form prompts like
//! `"download invoice for ID 0lwqxdww"` or `"download the $4 declined invoice
//! from June 12"` resolve to `{label_query, container_query}` without
//! stalling saved replays. Anything ambiguous resolves to `None` and the UI
//! says so instead of guessing.

use macro_engine::SemanticIntent;
use playbook_store::PlaybookSummary;

/// Closed English stopword set for command matching. Verbs stay: they drive
/// ephemeral role inference below. Cue words (`for`, `id`, `with`, …) stay
/// out: they introduce identifiers, which `extract_identifier` handles.
const STOPWORDS: &[&str] = &[
    "my", "the", "a", "an", "please", "kindly", "now", "latest", "new", "here", "this", "that",
    "me", "for", "to", "on", "and", "or", "of", "in", "is", "it", "id", "with", "named", "called",
];

/// URL tokens that must never match (`https`, TLDs): they appear in every
/// portal and carry no intent.
const URL_NOISE: &[&str] = &["https", "http", "www", "com", "org", "net", "io", "dev"];

/// Lowercase alphanumeric tokens of length two or more.
pub(crate) fn tokens(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|part| part.len() > 1)
        .map(str::to_ascii_lowercase)
        .collect()
}

pub(crate) fn content_tokens(text: &str) -> Vec<String> {
    tokens(text)
        .into_iter()
        .filter(|token| !STOPWORDS.contains(&token.as_str()))
        .collect()
}

/// Every role [`role_for`] can emit, and therefore the complete action
/// vocabulary a [`SemanticIntent`] may carry.
///
/// Single source of truth on purpose: the intent-parser seam validates
/// model-supplied actions against this same list, so a parser can never
/// introduce a role the deterministic path would not have produced. Adding
/// a role means extending [`role_for`], and both readers move together.
pub(crate) const INTENT_ROLES: &[&str] = &["textbox", "button", "combobox", "link"];

fn role_for(keywords: &[String]) -> &'static str {
    let has = |words: &[&str]| keywords.iter().any(|token| words.contains(&token.as_str()));
    if has(&["fill", "type", "enter"]) {
        "textbox"
    } else if has(&["click", "press", "submit", "tap"]) {
        "button"
    } else if has(&["select", "choose"]) {
        "combobox"
    } else {
        "link"
    }
}

/// Characters that may appear inside one identifier run: letters, digits,
/// and the separators of common ID, date, and amount shapes.
fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric()
        || matches!(character, '-' | '/' | '.' | ',' | '$' | '%' | '#' | ':')
}

/// Extract the first identifier-like run from raw text: a mixed letter-digit
/// run (`0LWQXDWW`, `INV-2024-001`) or a structured all-numeric run
/// (`2026-09-15`, `1,234.56`). Shapes, never site lists: detection is pure
/// character structure, so new portals need no new rules. Runs shorter than
/// four alphanumeric characters are weak evidence (counts, single years)
/// and are skipped. Returns the canonical form with edge sigils trimmed.
#[must_use]
pub fn extract_identifier(prompt: &str) -> Option<String> {
    for run in prompt.split(|character: char| !is_identifier_char(character)) {
        let alnum_count = run.chars().filter(|c| c.is_alphanumeric()).count();
        if alnum_count < 4 {
            continue;
        }
        let has_letter = run.chars().any(char::is_alphabetic);
        let has_digit = run.chars().any(char::is_numeric);
        let groups: Vec<&str> = run
            .split(|c: char| !c.is_alphanumeric())
            .filter(|part| !part.is_empty())
            .collect();
        let structured =
            groups.len() >= 2 && groups.iter().all(|part| part.chars().all(char::is_numeric));
        if (has_letter && has_digit) || structured {
            let canonical = run.trim_matches(|c| matches!(c, '$' | '#' | '%'));
            let canonical = strip_id_label(canonical);
            if canonical.chars().filter(|c| c.is_alphanumeric()).count() >= 4 {
                return Some(canonical.to_owned());
            }
        }
    }
    None
}

/// Strip a leading `ID` metadata label from one identifier run (`ID:0LWQXDWW`,
/// `id-1TUEWZUA`) so the container query carries the pure target token.
/// Space-separated labels (`ID 0LWQXDWW`) never reach here — runs split on
/// spaces and a bare `ID` carries no digit. A `for` prefix is deliberately
/// left alone: separator-joined `for-…` forms are natural prose (`for-sale`),
/// while space-separated `for` already splits into its own digit-less run.
fn strip_id_label(run: &str) -> &str {
    let Some(prefix) = run.get(..2) else {
        return run;
    };
    if !prefix.eq_ignore_ascii_case("id") {
        return run;
    }
    let bytes = run.as_bytes();
    if bytes.len() <= 2 || !matches!(bytes[2], b'-' | b':' | b'#' | b'/' | b'.' | b',') {
        return run;
    }
    // Byte-wise: only ASCII separators are skipped, so boundaries hold.
    let mut start = 2;
    while start < bytes.len()
        && matches!(
            bytes[start],
            b'-' | b':' | b'#' | b'/' | b'.' | b',' | b'$' | b'%' | b'_'
        )
    {
        start += 1;
    }
    run.get(start..).unwrap_or(run)
}

/// Rank table over the ordinal vocabulary already present in
/// `NON_IDENTIFYING` — no new word lists. Words (`first`) and numerals
/// (`1st`) share ranks; only the bare `last` is positional-by-time and
/// handled separately below.
const ORDINAL_RANKS: &[(&str, usize)] = &[
    ("first", 0),
    ("1st", 0),
    ("second", 1),
    ("2nd", 1),
    ("third", 2),
    ("3rd", 2),
    ("fourth", 3),
    ("4th", 3),
    ("fifth", 4),
    ("5th", 4),
    ("sixth", 5),
    ("6th", 5),
    ("seventh", 6),
    ("7th", 6),
    ("eighth", 7),
    ("8th", 7),
    ("ninth", 8),
    ("9th", 8),
    ("tenth", 9),
    ("10th", 9),
];

/// Time-unit words (a subset of `NON_IDENTIFYING`) that flip a bare `last`
/// from positional to temporal: `last invoice` selects the final row, while
/// `receipt for last week` carries no position at all.
const TIME_QUALIFIERS: &[&str] = &[
    "week",
    "month",
    "year",
    "day",
    "today",
    "yesterday",
    "tomorrow",
    "ago",
];

/// Positional selection buried in a prompt: an explicit rank
/// (`second`/`2nd` → 1) plus whether a bare, non-temporal `last` selects the
/// final candidate. Ordinal words never reach labels or scopes (filtered by
/// `NON_IDENTIFYING`), so this is purely additive — prompts without ordinals
/// resolve exactly as before.
fn parse_ordinal(prompt: &str) -> (Option<usize>, bool) {
    let words: Vec<String> = prompt
        .split(|character: char| !character.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut index = None;
    for word in &words {
        if let Some((_, rank)) = ORDINAL_RANKS.iter().find(|(token, _)| token == word) {
            index = Some(*rank);
            break;
        }
    }
    let has_last = words.iter().any(|word| word == "last");
    let has_time = words
        .iter()
        .any(|word| TIME_QUALIFIERS.contains(&word.as_str()));
    let is_last = index.is_none() && has_last && !has_time;
    (index, is_last)
}

/// Dynamic value class for template extraction. Detection uses only the
/// existing character-shape scanners plus quoted spans — no model, no new
/// word lists beyond a tiny currency-code set documented below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VariableKind {
    Id,
    Amount,
    Date,
    Entity,
}

/// One inferred template variable: its placeholder name, class, default
/// value (the observed span), inferred field type, and a UI label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedVariable {
    pub name: String,
    pub kind: VariableKind,
    pub default_value: String,
    pub field_type: &'static str,
    pub ui_label: String,
}

/// Prompt with dynamic spans replaced by placeholders, plus the variable
/// schema in first-appearance order (deterministic, unlike a hash map).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariableExtraction {
    pub template: String,
    pub variables: Vec<ExtractedVariable>,
}

/// ISO-style currency codes for bare `120 EUR` amounts (`$`-amounts ride
/// the existing amount scanner). Major settlement currencies only.
const CURRENCY_CODES: &[&str] = &["eur", "usd", "gbp", "inr"];

/// Upper bound on extracted variables per call: prompts are short, and the
/// cap keeps pathological inputs bounded.
const MAX_VARIABLES: usize = 8;

/// Structured numeric runs that read as dates (`2026-09-20`, `20/09/2026`)
/// rather than amounts: two-plus all-numeric groups joined by `-` or `/`.
fn is_calendar_span(run: &str) -> bool {
    let groups: Vec<&str> = run
        .split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect();
    groups.len() >= 2
        && groups.iter().all(|part| part.chars().all(char::is_numeric))
        && run.contains(['-', '/'])
}

/// Find every non-overlapping span matching `find_first` left to right.
/// `find_first` returns the first span in its input (like the existing
/// shape scanners); positions advance past each hit via substring search,
/// which is boundary-safe because matches are always `&str` slices.
fn collect_spans(text: &str, find_first: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let mut spans = Vec::new();
    let mut rest = text;
    while spans.len() < MAX_VARIABLES {
        let Some(span) = find_first(rest) else {
            break;
        };
        let Some(offset) = rest.find(span.as_str()) else {
            break;
        };
        spans.push(span.clone());
        rest = rest.get(offset + span.len()..).unwrap_or("");
    }
    spans
}

/// Bare `120 EUR`-style amounts: ASCII digits (with `,`/`.`) plus one space
/// plus a currency code, case-insensitive.
fn find_code_amount(text: &str) -> Option<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    for pair in words.windows(2) {
        let (amount, code) = (pair[0], pair[1]);
        let code = code.trim_matches(|c: char| !c.is_alphanumeric());
        if !CURRENCY_CODES.contains(&code.to_ascii_lowercase().as_str()) {
            continue;
        }
        let digits = amount.trim_matches(|c: char| !c.is_alphanumeric());
        if digits.is_empty()
            || !digits
                .chars()
                .all(|c| c.is_ascii_digit() || c == ',' || c == '.')
            || !digits.chars().any(char::is_numeric)
        {
            continue;
        }
        let span = format!("{digits} {code}");
        if text.contains(&span) {
            return Some(span);
        }
        let span = format!("{amount} {code}");
        if text.contains(&span) {
            return Some(span);
        }
    }
    None
}

/// Double-quoted entities (`"subheader.lol"`), shortest pairs first via
/// left-to-right scan. Empty quotes and multi-line spans never qualify.
fn find_quoted(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'"' {
            index += 1;
            continue;
        }
        let mut end = index + 1;
        while end < bytes.len() && bytes[end] != b'"' && bytes[end] != b'\n' {
            end += 1;
        }
        if end < bytes.len() && bytes[end] == b'"' && end > index + 1 {
            if let Some(span) = text.get(index..=end)
                && span.len() <= 66
            {
                return Some(span.to_owned());
            }
            index = end + 1;
        } else {
            index += 1;
        }
    }
    None
}

/// Extract invisible dynamic variables from the uttered prompt first, then
/// grounded surroundings: template slots cover prompt spans, while the
/// schema additionally records matching spans observed in the label and
/// container (untemplated, usable as prefill evidence). Reuses the existing
/// shape scanners, so no new pattern vocabulary beyond currency codes.
#[must_use]
pub fn extract_dynamic_variables(
    prompt: &str,
    grounded_label: &str,
    container_text: &str,
) -> VariableExtraction {
    // (span, kind) in scan order: amounts before ids so `$45.00` wins over
    // its bare `45.00` run; calendar runs classify as dates, the rest as ids.
    let mut found: Vec<(String, VariableKind)> = Vec::new();
    let mut push = |span: String, kind: VariableKind| {
        if found.len() < MAX_VARIABLES
            && !found.iter().any(|(seen, _)| seen == &span)
            && !found
                .iter()
                .any(|(seen, _)| span.len() < seen.len() && seen.contains(span.as_str()))
        {
            found.push((span, kind));
        }
    };
    for span in collect_spans(prompt, extract_amount) {
        push(span, VariableKind::Amount);
    }
    for span in collect_spans(prompt, find_code_amount) {
        push(span, VariableKind::Amount);
    }
    for span in collect_spans(prompt, extract_identifier) {
        if is_calendar_span(&span) {
            push(span, VariableKind::Date);
        } else {
            push(span, VariableKind::Id);
        }
    }
    if let Some(span) = extract_month_day(prompt) {
        push(span, VariableKind::Date);
    }
    for span in collect_spans(prompt, find_quoted) {
        push(span, VariableKind::Entity);
    }
    // Grounded surroundings: only spans already shaped above, merged without
    // duplicating the template slots.
    for text in [grounded_label, container_text] {
        for span in collect_spans(text, extract_identifier) {
            if prompt.contains(span.as_str()) {
                continue;
            }
            if is_calendar_span(&span) {
                push(span, VariableKind::Date);
            } else {
                push(span, VariableKind::Id);
            }
        }
    }
    let variables = name_spans(prompt, &found);
    // Longest spans first so `$45.00` templates before a bare `45.00` could;
    // spans already swallowed check out via containment and are skipped.
    let mut template = prompt.to_owned();
    let mut ordered: Vec<(String, String)> = found
        .iter()
        .zip(variables.iter())
        .map(|((span, _), variable)| (span.clone(), variable.name.clone()))
        .collect();
    ordered.sort_by_key(|slot| std::cmp::Reverse(slot.0.len()));
    for (span, name) in ordered {
        if template.contains(span.as_str()) {
            template = template.replace(span.as_str(), &format!("{{{{{name}}}}}"));
        }
    }
    VariableExtraction {
        template,
        variables,
    }
}

/// Name collected spans with placeholder, type, and UI metadata. The first
/// ID becomes `invoice_id` when the prompt mentions invoices, else `id`;
/// repeats take numeric suffixes. Pure naming — no scanning here.
fn name_spans(prompt: &str, found: &[(String, VariableKind)]) -> Vec<ExtractedVariable> {
    let invoiced = prompt.to_ascii_lowercase().contains("invoice");
    let mut counters = [0_usize; 4];
    let mut variables = Vec::new();
    for (span, kind) in found {
        let slot = slot_for(*kind, &mut counters, invoiced);
        variables.push(ExtractedVariable {
            name: slot.0,
            kind: *kind,
            default_value: span.clone(),
            field_type: slot.2,
            ui_label: slot.1,
        });
    }
    variables
}

/// Placeholder, UI label, and field type for the next span of one kind.
/// Counter advances here so naming stays in one place.
fn slot_for(
    kind: VariableKind,
    counters: &mut [usize; 4],
    invoiced: bool,
) -> (String, String, &'static str) {
    let cell = match kind {
        VariableKind::Id => &mut counters[0],
        VariableKind::Amount => &mut counters[1],
        VariableKind::Date => &mut counters[2],
        VariableKind::Entity => &mut counters[3],
    };
    *cell += 1;
    let count = *cell;
    let (base, label, field_type) = match kind {
        VariableKind::Id if invoiced => ("invoice_id", "Invoice ID", "text"),
        VariableKind::Id => ("id", "ID", "text"),
        VariableKind::Amount => ("amount", "Amount", "number"),
        VariableKind::Date => ("date", "Date", "date"),
        VariableKind::Entity => ("entity_name", "Entity name", "text"),
    };
    if count <= 1 {
        (base.to_owned(), label.to_owned(), field_type)
    } else {
        (
            format!("{base}_{count}"),
            format!("{label} {count}"),
            field_type,
        )
    }
}

/// Structured intent extracted from a free-form prompt: the primary action
/// label plus key target identifiers, dates, amounts, or text snippets
/// normalized for container matching. Produced by the AI pass when a provider
/// is configured, otherwise by the deterministic fallback below.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedIntent {
    pub label_query: String,
    pub container_query: Option<String>,
}

/// Executable honoring the stdin/stdout JSON contract for AI intent parsing
/// (`{"prompt": "..."}` in, `{"label_query": "...", "container_query":
/// "..."|null}` out). Mirrors `CLINCH_REPAIR_PROVIDER`; unset means no
/// model call and an instant deterministic fallback. Any Ollama wrapper
/// speaking this contract works (see `scripts/local_repair_adapter.py` for
/// the loopback/no-redirect pattern to copy).
const INTENT_PROVIDER_ENV: &str = "CLINCH_INTENT_PROVIDER";
const INTENT_PROVIDER_SCRIPT_ENV: &str = "CLINCH_INTENT_PROVIDER_SCRIPT";
/// Upper bounds for the provider round-trip: prompts are truncated, outputs
/// are capped, and the child is killed on timeout so routing never stalls.
const MAX_PROMPT_CHARS: usize = 2000;
const MAX_PROVIDER_BYTES: usize = 4096;

/// Month names for date extraction (full and common abbreviations).
const MONTHS: &[&str] = &[
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "jan",
    "feb",
    "mar",
    "apr",
    "jun",
    "jul",
    "aug",
    "sep",
    "sept",
    "oct",
    "nov",
    "dec",
];

/// Invoice/payment lifecycle words that identify a row when present in DOM
/// text. Kept small and generic: portals reuse these English terms.
const STATUS_WORDS: &[&str] = &[
    "declined",
    "approved",
    "paid",
    "unpaid",
    "pending",
    "overdue",
    "failed",
    "refunded",
    "cancelled",
    "canceled",
    "draft",
    "sent",
    "due",
];

/// Collection words marking plural intent (`all invoices`). Like ordinals,
/// they describe *which* matches, not *what* to click, so they stay out of
/// labels too (`"download all"` labels `download`). Detection reads the same
/// list via [`PLURAL_MARKERS`]; nothing here is new vocabulary.
pub(crate) const PLURAL_MARKERS: &[&str] = &["all", "every", "each"];

/// Whether the prompt asks for every matching control rather than one.
/// Exact token match only (`overall` never counts as `all`).
fn parse_plural(prompt: &str) -> bool {
    tokens(prompt)
        .iter()
        .any(|token| PLURAL_MARKERS.contains(&token.as_str()))
}

/// Tokens that must never become a label or veto a container: ordinals and
/// positional words (`second`), relative-time words (`last week`), generic
/// list words, prepositions, month names, and pure numbers. They describe
/// *which* match, not *what* to click, so filtering them keeps free-form
/// prompts (`"second invoice"`, `"receipt for last week"`) matching instead
/// of failing closed on words the DOM never contains.
pub(crate) const NON_IDENTIFYING: &[&str] = &[
    "all",
    "every",
    "each",
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
];

fn is_month(token: &str) -> bool {
    MONTHS.contains(&token)
}

fn is_non_identifying(token: &str) -> bool {
    NON_IDENTIFYING.contains(&token) || is_month(token) || token.chars().all(|c| c.is_ascii_digit())
}

/// First `$`-prefixed amount in raw text (`$4`, `$4.00`, `$1,234.56`).
/// Small amounts matter here: `tokens()` drops single digits, so `$4` would
/// otherwise vanish from the container scope.
fn extract_amount(prompt: &str) -> Option<String> {
    let bytes = prompt.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'$' {
            let mut end = index + 1;
            while end < bytes.len()
                && (bytes[end].is_ascii_digit() || matches!(bytes[end], b',' | b'.'))
            {
                end += 1;
            }
            if end > index + 1 {
                let mut candidate = prompt
                    .get(index..end)
                    .unwrap_or("")
                    .trim_end_matches(['.', ',']);
                if candidate.len() > 1 {
                    // Keep edge sigils off without panicking on boundaries.
                    while candidate.starts_with('$')
                        && candidate.len() > 1
                        && candidate[1..].starts_with(['$', '#', '%'])
                    {
                        candidate = candidate.get(1..).unwrap_or(candidate);
                    }
                    if !candidate.is_empty() {
                        return Some(candidate.to_owned());
                    }
                }
            }
            index = end.max(index + 1);
        } else {
            index += 1;
        }
    }
    None
}

/// First month-plus-day span (`June 12`, `jun 12th`) as typed. Day suffixes
/// (`12th`) and trailing commas are trimmed; the match is case-insensitive
/// so `june 12` finds `June 12` downstream via case-insensitive matching.
fn extract_month_day(prompt: &str) -> Option<String> {
    let words: Vec<&str> = prompt.split_whitespace().collect();
    for (position, word) in words.iter().enumerate() {
        let cleaned = word
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_ascii_lowercase();
        if !is_month(&cleaned) {
            continue;
        }
        if let Some(next) = words.get(position + 1) {
            let mut day = next.trim_matches(|c: char| !c.is_alphanumeric());
            for suffix in ["st", "nd", "rd", "th"] {
                if let Some(stripped) = day.strip_suffix(suffix)
                    && stripped.chars().any(|c| c.is_ascii_digit())
                {
                    day = stripped;
                    break;
                }
            }
            let day = day.trim_matches(|c: char| !c.is_numeric());
            if !day.is_empty() && day.len() <= 2 && day.chars().all(|c| c.is_ascii_digit()) {
                let month = word.trim_matches(|c: char| !c.is_alphanumeric());
                return Some(format!("{month} {day}"));
            }
        }
        let month = word.trim_matches(|c: char| !c.is_alphanumeric());
        if !month.is_empty() {
            return Some(month.to_owned());
        }
    }
    None
}

/// Status words present in the prompt, in prompt order, deduplicated.
fn extract_statuses(prompt: &str) -> Vec<String> {
    let mut found = Vec::new();
    for part in prompt.split(|c: char| !c.is_alphanumeric()) {
        if part.is_empty() {
            continue;
        }
        let lower = part.to_ascii_lowercase();
        if STATUS_WORDS.contains(&lower.as_str()) && !found.contains(&lower) {
            found.push(lower);
        }
    }
    found
}

/// Label candidate filtering: drop positional/temporal/month/numeric noise
/// so `"receipt for last week"` labels `receipt` (not `week`) and
/// `"$4 declined invoice from June 12"` labels `invoice` (not `12`).
fn label_keywords(prompt: &str) -> Vec<String> {
    content_tokens(prompt)
        .into_iter()
        .filter(|token| !is_non_identifying(token.as_str()))
        .collect()
}

/// Singular stem for anchor matching: one trailing `s` off longer tokens,
/// never `ss`. Mirrors the executor's plural tolerance; over-stripping is
/// harmless downstream because matching is substring-based (`analytic`
/// still sits inside `Analytics`), while under-stripping would miss
/// (`invoices` never contains `invoice`).
fn singular_stem(token: &str) -> String {
    if token.len() > 3 && token.ends_with('s') && !token.ends_with("ss") {
        token[..token.len() - 1].to_owned()
    } else {
        token.to_owned()
    }
}

/// Prepositional cues that introduce a destination complement
/// (`… from github`, `… on github`, `… at github`). Closed English grammar
/// vocabulary, never a site list: the complement itself is whatever word
/// the user typed, and nothing here checks it against known portals.
const PREPOSITION_CUES: &[&str] = &["from", "on", "at"];

/// Verbs the parser reads as the clause's verb slot rather than a noun.
/// Their only job is telling a real prepositional complement
/// (`invoices from github`) from a phrasal-verb particle (`click on pay`):
/// with no noun before the cue there is no artifact, so the prompt parses
/// as a direct action instead. Closed English vocabulary — no site names,
/// no portal spellings, nothing that grows per portal.
const ACTION_VERBS: &[&str] = &[
    "download", "get", "fetch", "grab", "pull", "export", "open", "show", "view", "find", "check",
    "click", "press", "tap", "submit", "fill", "type", "enter", "select", "choose", "toggle",
    "turn", "run", "go", "navigate", "visit", "launch",
];

/// Words that open a subordinate clause, which means the prompt is not a
/// plain imperative. `"pull up what I owe on aws"` reads structurally like
/// `"<verb> … <clause> on <site>"`, and the clause body (`owe`) is a verb
/// phrase, not the artifact the user wants — so the fast path reports low
/// confidence instead of anchoring on a misparse.
///
/// Structural, not topical: these are closed-class English function words,
/// so the list never grows with portals, artifacts, or phrasings.
const CLAUSE_MARKERS: &[&str] = &[
    "what", "whatever", "which", "who", "whom", "whose", "how", "why", "where", "when", "whether",
    "that", "if",
];

/// Host words of the connected portal, which name plumbing rather than
/// intent and so never fill a noun slot. Empty without a connection.
fn origin_tokens(connected_origin: Option<&url::Url>) -> Vec<String> {
    connected_origin.map_or_else(Vec::new, |origin| {
        content_tokens(origin.as_str())
            .into_iter()
            .filter(|token| !URL_NOISE.contains(&token.as_str()))
            .collect()
    })
}

/// How much the deterministic fast path trusts its own parse.
///
/// This gates the intent-parser seam: [`Confidence::High`] runs immediately
/// at zero token cost, [`Confidence::Low`] defers to a structured parser
/// (and, when none answers, to raw search). The bias is deliberately
/// conservative — `Low` costs a bounded parser call, while a wrong `High`
/// silently drives the browser at the wrong target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Confidence {
    /// A plain imperative whose slots are all accounted for: a recognized
    /// verb heads the clause, no subordinate clause muddies the object, and
    /// the matched pattern filled every slot it needs.
    High,
    /// Anything else — no verb to anchor on, a subordinate clause, or a
    /// pattern that came up short. The default, so an empty parse is never
    /// mistaken for a confident one.
    #[default]
    Low,
}

impl Confidence {
    /// Whether this parse may skip the intent-parser seam entirely.
    #[must_use]
    pub fn is_high(self) -> bool {
        matches!(self, Self::High)
    }
}

/// Grammar slots parsed out of one ad-hoc prompt.
///
/// Two shapes, told apart by sentence structure alone — no portal
/// vocabulary is consulted anywhere:
///
/// * **Prepositional complement** — `download all my invoices from github`
///   fills `artifact_noun: invoice` (the direct object, which downstream
///   batch matching anchors on) and `site_context: github` (the
///   prepositional complement naming the domain).
/// * **Direct action** — `open amazon for me` has no complement, so the
///   direct object *is* the destination: `target_noun: amazon`,
///   `site_context: None`.
///
/// At most one of `artifact_noun` / `target_noun` is ever populated, which
/// keeps [`Self::primary_noun`] unambiguous.
///
/// `confidence` reports whether the parse is trustworthy enough to act on
/// without a structured parser. Low-confidence slots are still returned
/// rather than discarded: when no parser answers they remain the best
/// available evidence, which is what keeps the offline path working.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedGrammar {
    pub artifact_noun: Option<String>,
    pub site_context: Option<String>,
    pub target_noun: Option<String>,
    pub confidence: Confidence,
}

impl ParsedGrammar {
    /// The noun downstream matching anchors on: the artifact when the prompt
    /// carried a prepositional complement, else the direct object. Both are
    /// stemmed, so a singular anchor covers inflected page text.
    #[must_use]
    pub fn primary_noun(&self) -> Option<&str> {
        self.artifact_noun
            .as_deref()
            .or(self.target_noun.as_deref())
    }

    /// Adopt fenced parser slots as grammar slots.
    ///
    /// Stemming happens here rather than in the parser so model output and
    /// deterministic output normalize identically — a parser answering
    /// `bills` anchors on `bill` exactly as `download bills` would. The
    /// slots arrive already sanitized ([`crate::ParsedSlots::sanitized`]);
    /// this only maps and stems them.
    ///
    /// Confidence is [`Confidence::High`] because the seam has already been
    /// consulted: the field gates *whether to call a parser*, and there is
    /// no second parser behind this one.
    #[must_use]
    pub fn from_slots(slots: &crate::intent_parser::ParsedSlots) -> Self {
        Self {
            artifact_noun: slots.artifact_noun.as_deref().map(singular_stem),
            site_context: slots.site_context.clone(),
            target_noun: None,
            confidence: Confidence::High,
        }
    }
}

/// Parse the prompt's grammar slots without any site whitelist.
///
/// Cues are scanned right to left so the complement nearest the end wins —
/// destinations trail in English (`invoices from github`, never
/// `github from invoices`). A cue only forms a prepositional reading when
/// both slots are really there: the following token must be a content word
/// (months, ordinals, digits, and bare stopwords are structure, not a
/// destination), and some noun must precede it. Otherwise the prompt falls
/// through to the direct-action reading, whose object is the last content
/// word.
///
/// Portal host words are excluded from noun slots (plumbing, not intent),
/// the same host/intent split the saved-playbook path already uses; the
/// site slot keeps them, since naming your current portal is a legitimate
/// destination.
///
/// # Confidence
///
/// A parse earns [`Confidence::High`] only when the prompt reads as a plain
/// imperative *and* the matched pattern filled every slot it needs:
///
/// * a recognized verb heads the clause, so there is something to act on;
/// * no [`CLAUSE_MARKERS`] word opens a subordinate clause, which is what
///   separates `"download invoices from github"` from
///   `"pull up what I owe on aws"` — the latter matches a preposition cue
///   just as cleanly, yet its artifact slot lands on the clause verb
///   (`owe`) rather than a real artifact;
/// * the pattern is complete: prepositional needs both artifact and site,
///   direct action needs its object.
///
/// Everything else is [`Confidence::Low`], which costs a bounded parser
/// call rather than a wrong click.
#[must_use]
pub fn parse_grammar(prompt: &str, connected_origin: Option<&url::Url>) -> ParsedGrammar {
    let host_tokens = origin_tokens(connected_origin);
    let words = tokens(prompt);
    // Same content filter the label stream uses: stopwords out, and
    // positional/temporal/month/numeric structure words out.
    let is_content = |token: &str| !STOPWORDS.contains(&token) && !is_non_identifying(token);
    let is_noun = |token: &String| is_content(token.as_str()) && !host_tokens.contains(token);
    // Structural preconditions, shared by both patterns below. A verb head
    // proves the prompt commands something; a clause marker proves it does
    // so in more grammar than this parser models.
    let verb_led = words
        .iter()
        .any(|token| ACTION_VERBS.contains(&token.as_str()));
    let subordinated = words
        .iter()
        .any(|token| CLAUSE_MARKERS.contains(&token.as_str()));
    let plain_imperative = verb_led && !subordinated;
    let confidence = |complete: bool| {
        if plain_imperative && complete {
            Confidence::High
        } else {
            Confidence::Low
        }
    };
    for (position, word) in words.iter().enumerate().rev() {
        if !PREPOSITION_CUES.contains(&word.as_str()) {
            continue;
        }
        let Some(site) = words
            .get(position + 1)
            .filter(|token| is_content(token.as_str()))
        else {
            continue;
        };
        let Some(artifact) = words
            .get(..position)
            .unwrap_or_default()
            .iter()
            .rev()
            .find(|token| is_noun(token) && !ACTION_VERBS.contains(&token.as_str()))
        else {
            continue;
        };
        return ParsedGrammar {
            artifact_noun: Some(singular_stem(artifact)),
            site_context: Some(site.clone()),
            target_noun: None,
            // Both slots are filled by construction here.
            confidence: confidence(true),
        };
    }
    let target_noun = words
        .iter()
        .rev()
        .find(|token| is_noun(token))
        .map(|token| singular_stem(token));
    ParsedGrammar {
        artifact_noun: None,
        site_context: None,
        confidence: confidence(target_noun.is_some()),
        target_noun,
    }
}

/// Primary target noun for the ephemeral intent: the grammar's artifact
/// noun when the prompt named a destination (`download all my invoices from
/// github` → `invoice`, never the `github` trailer), else its direct object
/// (`open amazon for me` → `amazon`). Collection modifiers and positional
/// words never survive the content filter, so the noun is always the
/// content word — never `all`. `None` when nothing content-bearing remains
/// (a lone identifier scopes by container instead and needs no anchor).
fn extract_primary_noun(prompt: &str, connected_origin: Option<&url::Url>) -> Option<String> {
    parse_grammar(prompt, connected_origin)
        .primary_noun()
        .map(str::to_owned)
}

/// A closed app-local command: Tier 0 of the dispatch cascade, checked
/// before saved playbooks, grammar, and every networked tier.
///
/// Closed means closed: the set below names app behavior, never sites, so
/// it cannot drift into world knowledge and never needs a search call.
/// Matching is a frozen normalizer plus exact-phrase equality — never the
/// full stopword filter, which would eat content words like `new` in
/// `new blank page` and collapse near-misses into the command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppCommand {
    /// Ensure the managed browser is attached (launching it if needed) and
    /// showing a blank tab. No navigation, no search, no network.
    OpenBlankBrowser,
}

/// Politeness filler the Tier-0 normalizer strips from prompt ends before
/// the closed match. Frozen vocabulary: only these phrases, applied
/// repeatedly, so filler can pad a command but never smuggle a
/// non-command in.
const POLITE_AFFIXES: &[&str] = &[
    "please",
    "kindly",
    "for me",
    "for us",
    "thanks",
    "thank you",
];

/// Closed command phrases, after normalization. Every phrasing names the
/// browser or a blank page/tab explicitly; a prompt with any other content
/// word (`open browser settings`, `open amazon`) is not a member.
const BLANK_BROWSER_PHRASES: &[&str] = &[
    "spin up browser",
    "spin up the browser",
    "spin up a browser",
    "open browser",
    "open the browser",
    "launch browser",
    "launch the browser",
    "start browser",
    "start the browser",
    "show browser",
    "show the browser",
    "show me the browser",
    "open a blank tab",
    "open blank tab",
    "new blank tab",
    "new blank page",
];

/// Normalize for Tier 0: lowercase, collapse whitespace, strip the frozen
/// politeness affixes from both ends until none remain.
fn normalize_app_command(prompt: &str) -> String {
    // Punctuation at token edges is orthography, not content: "spin up
    // browser, thanks" normalizes the same as "spin up browser thanks".
    // Internal punctuation (`amazon.in`, `don't`) is untouched.
    let mut text: String = prompt
        .to_ascii_lowercase()
        .split_whitespace()
        .map(|token| token.trim_matches(|c: char| c.is_ascii_punctuation()))
        .collect::<Vec<_>>()
        .join(" ");
    loop {
        let mut stripped = false;
        for affix in POLITE_AFFIXES {
            if text == *affix {
                text.clear();
                stripped = true;
            } else if let Some(rest) = text.strip_prefix(&format!("{affix} ")) {
                text = rest.to_owned();
                stripped = true;
            } else if let Some(rest) = text.strip_suffix(&format!(" {affix}")) {
                text = rest.to_owned();
                stripped = true;
            }
        }
        if !stripped {
            break;
        }
    }
    text
}

/// Resolve a Tier-0 app command from the prompt, if it names one.
///
/// The match is closed-world: the normalized prompt must equal a phrase in
/// [`BLANK_BROWSER_PHRASES`], so `\"open browser settings\"` (extra word
/// `settings`) and `\"open amazon\"` (no lifecycle vocabulary) fall through
/// instead of being swallowed. `None` is the common case — most prompts
/// are not app commands.
#[must_use]
pub fn resolve_app_command(prompt: &str) -> Option<AppCommand> {
    if BLANK_BROWSER_PHRASES.contains(&normalize_app_command(prompt).as_str()) {
        Some(AppCommand::OpenBlankBrowser)
    } else {
        None
    }
}

/// Verbs that phrase a direct site open. Closed vocabulary: everything
/// else (`download`, `find`, `check`) keeps the search-grounded path, so a
/// retrieval verb can never silently become a navigation.
const OPEN_VERBS: &[&str] = &["open", "go", "navigate", "launch", "visit"];

/// Whether the grammar describes a direct site open: high confidence, one
/// target noun, no artifact (no prepositional complement), an open-class
/// verb heading the prompt, and no coordinators.
///
/// The coordinator check is what keeps `"open amazon and flipkart"` out of
/// this path: a multi-target prompt must re-resolve through the
/// batch-consent lane, never silently open one of its targets. Raw tokens
/// (stopwords kept) are checked so the `and`/`or` the content filter drops
/// still vetoes.
#[must_use]
pub fn is_direct_open(prompt: &str, grammar: &ParsedGrammar) -> bool {
    if !grammar.confidence.is_high() {
        return false;
    }
    if grammar.artifact_noun.is_some() || grammar.target_noun.is_none() {
        return false;
    }
    if tokens(prompt)
        .iter()
        .any(|token| matches!(token.as_str(), "and" | "or"))
    {
        return false;
    }
    let content = content_tokens(prompt);
    content
        .first()
        .is_some_and(|verb| OPEN_VERBS.contains(&verb.as_str()))
}

/// Deterministic structured fallback: instant, offline, no model call.
/// Collects the identifier (any case: `0lwqxdww` matches `0LWQXDWW`
/// downstream), `$`-amounts, month/day dates, and status words into one
/// space-joined container scope; the label is the last identifying keyword
/// with the identifier as a last resort. Returns `None` label only when the
/// prompt carries nothing routable.
fn deterministic_parse(prompt: &str) -> Option<ParsedIntent> {
    let identifier = extract_identifier(prompt);
    let amount = extract_amount(prompt);
    let month_day = extract_month_day(prompt);
    let statuses = extract_statuses(prompt);
    let cleaned = identifier.as_ref().map_or_else(
        || prompt.to_owned(),
        |id| prompt.replacen(id.as_str(), "", 1),
    );
    let keywords = content_tokens(&cleaned);
    if keywords.is_empty() && identifier.is_none() {
        return None;
    }
    let filtered = label_keywords(&cleaned);
    let label = filtered
        .last()
        .or_else(|| keywords.last())
        .cloned()
        .or_else(|| identifier.clone())?;
    let mut parts: Vec<String> = Vec::new();
    if let Some(id) = identifier.clone() {
        parts.push(id);
    }
    if let Some(value) = amount
        && !parts.iter().any(|part| part.contains(value.as_str()))
    {
        parts.push(value);
    }
    for status in statuses {
        if !parts.iter().any(|part| part.eq_ignore_ascii_case(&status)) {
            parts.push(status);
        }
    }
    if let Some(date) = month_day
        && !parts.iter().any(|part| part.eq_ignore_ascii_case(&date))
    {
        parts.push(date);
    }
    let container_query = if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    };
    Some(ParsedIntent {
        label_query: label,
        container_query,
    })
}

/// Best-effort AI structured pass over stdin/stdout JSON. Returns `None` on
/// any failure (unset env, spawn error, timeout, oversized or invalid JSON)
/// so callers always fall back to [`deterministic_parse`] instantly.
fn try_ai_structured_parse(prompt: &str) -> Option<ParsedIntent> {
    let executable = std::env::var_os(INTENT_PROVIDER_ENV)?;
    if executable.is_empty() {
        return None;
    }
    let truncated: String = prompt.chars().take(MAX_PROMPT_CHARS).collect();
    if truncated.trim().is_empty() {
        return None;
    }
    let request = serde_json::json!({ "prompt": truncated });
    let bytes = serde_json::to_vec(&request).ok()?;
    let mut command = std::process::Command::new(executable);
    if let Some(script) = std::env::var_os(INTENT_PROVIDER_SCRIPT_ENV)
        && !script.is_empty()
    {
        command.arg(script);
    }
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().ok()?;
    let mut stdin = child.stdin.take();
    let outcome = std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            use std::io::Write as _;
            if let Some(mut handle) = stdin.take() {
                let _ = handle.write_all(&bytes);
            }
        });
        let output = child.wait_with_output();
        let _ = writer.join();
        output.ok()
    });
    let output = outcome?;
    if !output.status.success() || output.stdout.len() > MAX_PROVIDER_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let label = value.get("label_query")?.as_str()?;
    let label = label.trim();
    if label.is_empty() || label.len() > 512 {
        return None;
    }
    let container = match value.get("container_query") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() || trimmed.len() > 512 {
                return None;
            }
            Some(trimmed.to_owned())
        }
        Some(_) => return None,
    };
    Some(ParsedIntent {
        label_query: label.to_owned(),
        container_query: container,
    })
}

/// AI structured extraction with instant deterministic fallback. When
/// `CLINCH_INTENT_PROVIDER` is set, the provider speaks first; any failure
/// (or unset env) falls back to [`deterministic_parse`] with no latency, so
/// saved-playbook replay performance never depends on a model.
#[must_use]
pub fn parse_intent_structured(prompt: &str) -> Option<ParsedIntent> {
    if let Some(parsed) = try_ai_structured_parse(prompt) {
        return Some(parsed);
    }
    deterministic_parse(prompt)
}

/// What a free-form command resolved to: a stored workflow id, or a
/// single-step intent for the connected portal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandMatch {
    Saved { id: String },
    Ephemeral { intent: SemanticIntent },
}

/// Route `prompt` against saved workflows and the connected portal.
/// Best token overlap wins (ties keep list order); a bare portal with no
/// overlap falls through to an ephemeral intent; anything else is `None`.
///
/// A prompt carrying a specific identifier never replays a static saved
/// flow: recordings have no parameter slots, so replay would silently run
/// the wrong target set. Identified prompts always take the dynamic path.
///
/// Split a composite prompt on multi-action connectors (`and`, `then`,
/// `, then`, case-insensitive) into ordered non-empty segments. Matching is
/// longest-separator-first so `", then "` never leaves a stray comma, and
/// byte ranges come from an ASCII-lowercased mirror (length-preserving), so
/// slicing the original is always boundary-safe.
fn split_conjunctions(prompt: &str) -> Vec<&str> {
    // Longest separator first: `", then "` must win over `" then "` so no
    // stray comma survives on the left segment.
    const SEPARATORS: &[&[char]] = &[
        &[',', ' ', 't', 'h', 'e', 'n', ' '],
        &[' ', 't', 'h', 'e', 'n', ' '],
        &[' ', 'a', 'n', 'd', ' '],
    ];
    let bytes: Vec<(usize, char)> = prompt.char_indices().collect();
    let lower: Vec<char> = bytes
        .iter()
        .map(|(_, character)| character.to_ascii_lowercase())
        .collect();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < lower.len() {
        let mut matched = 0;
        for separator in SEPARATORS {
            if lower[index..].starts_with(separator) {
                matched = separator.len();
                break;
            }
        }
        if matched > 0 {
            push_segment(prompt, &bytes, start, index, &mut segments);
            index += matched;
            start = index;
        } else {
            index += 1;
        }
    }
    push_segment(prompt, &bytes, start, index, &mut segments);
    segments
}

/// Push `chars[start..end]` trimmed, skipping empties (leading/trailing or
/// doubled connectors contribute nothing).
fn push_segment<'a>(
    prompt: &'a str,
    bytes: &[(usize, char)],
    start: usize,
    end: usize,
    segments: &mut Vec<&'a str>,
) {
    if start >= end {
        return;
    }
    let from = bytes.get(start).map_or(prompt.len(), |(offset, _)| *offset);
    let to = bytes.get(end).map_or(prompt.len(), |(offset, _)| *offset);
    let segment = prompt.get(from..to).unwrap_or("").trim();
    if !segment.is_empty() {
        segments.push(segment);
    }
}

/// Decompose a composite prompt into an ordered step sequence, resolving
/// each segment through [`resolve_command`]. Single-action prompts yield a
/// single-element vec, so this is a strict generalization of the
/// single-step path — which stays untouched (dispatch still consumes one
/// [`CommandMatch`); sequencing execution across these steps is future
/// runner work, not a behavior change here.
#[must_use]
pub fn decompose_command(
    prompt: &str,
    connected_origin: Option<&url::Url>,
    saved: &[PlaybookSummary],
) -> Vec<CommandMatch> {
    split_conjunctions(prompt)
        .into_iter()
        .filter_map(|segment| resolve_command(segment, connected_origin, saved))
        .collect()
}

/// `connected_origin` is presence-checked only — callers supply the portal
/// for session scoping and execution.
///
/// An exact [`prompt_key`] match dominates candidate *ranking*, but it does
/// not bypass the safety guards below it: a plural or scope-carrying prompt
/// still takes the dynamic path even when its own saved workflow is sitting
/// right there. The reason is execution shape, not matching — saved replay
/// resolves one control per step, so replaying a learned `"download all …"`
/// would click once and silently under-deliver. Those prompts re-resolve
/// each run instead, which is slower but correct.
#[must_use]
pub fn resolve_command(
    prompt: &str,
    connected_origin: Option<&url::Url>,
    saved: &[PlaybookSummary],
) -> Option<CommandMatch> {
    let keywords = content_tokens(prompt);
    if keywords.is_empty() {
        return None;
    }
    // The learning loop's index: a workflow saved from this exact phrasing
    // is not a guess, so it outranks every token-overlap candidate. This is
    // what turns a once-ambiguous prompt into a deterministic tier-1 hit —
    // the phrasing was taught, not inferred.
    let key = prompt_key(prompt);
    let mut best: Option<(&PlaybookSummary, usize)> = None;
    for playbook in saved {
        if let Some(key) = key.as_deref()
            && playbook.prompt_key.as_deref() == Some(key)
        {
            best = Some((playbook, usize::MAX));
            break;
        }
        // A token in the workflow's own name outweighs one in its URL: names
        // carry intent, hosts carry plumbing.
        let name_tokens = content_tokens(&playbook.name.replace(['-', '_'], " "));
        let host_tokens: Vec<String> = content_tokens(&playbook.portal_url)
            .into_iter()
            .filter(|token| !URL_NOISE.contains(&token.as_str()))
            .collect();
        let score = 2 * keywords
            .iter()
            .filter(|keyword| name_tokens.contains(keyword))
            .count()
            + keywords
                .iter()
                .filter(|keyword| host_tokens.contains(keyword))
                .count();
        if score > 0 && best.is_none_or(|(_, top)| score > top) {
            best = Some((playbook, score));
        }
    }
    // Scoped prompts never replay a static saved flow: recordings carry no
    // parameter slots, so replay would silently run the wrong target set.
    // The scope check uses the cheap deterministic parse only (no model call)
    // so saved replays stay instant; the AI provider runs solely on the
    // ephemeral path below. Scopes include IDs (`0LWQXDWW`), amounts (`$4`),
    // dates (`June 12`), and status words (`declined`) — any of which makes
    // the prompt target-specific. A prompt holding nothing but a scope still
    // runs: the scope doubles as a last-resort label.
    let quick = deterministic_parse(prompt);
    // Plural prompts never replay a static saved flow either: a one-click
    // recording cannot honor "all/every/each", and replay would silently act
    // once instead of batching. Like identifiers, plurality forces the
    // dynamic path even when a name matches — still no model call, so saved
    // replays stay instant.
    let plural = parse_plural(prompt);
    // A specific scope with a static saved flow always takes the dynamic path
    // even when a name matches.
    if !plural
        && quick
            .as_ref()
            .is_none_or(|parsed| parsed.container_query.is_none())
        && let Some((playbook, _)) = best
    {
        return Some(CommandMatch::Saved {
            id: playbook.id.clone(),
        });
    }
    connected_origin?;
    // AI structured pass with deterministic fallback. Provider failures fall
    // back instantly; saved replays above never waited on a model.
    let parsed = parse_intent_structured(prompt)?;
    // The provider may find a scope the cheap pass missed: re-check saved so
    // a newly-scoped prompt cannot slip into a static replay. Plurality was
    // already decided above and holds here too.
    if !plural
        && parsed.container_query.is_none()
        && let Some((playbook, _)) = best
    {
        return Some(CommandMatch::Saved {
            id: playbook.id.clone(),
        });
    }
    let identifier = extract_identifier(prompt);
    let cleaned = identifier.as_ref().map_or_else(
        || prompt.to_owned(),
        |id| prompt.replacen(id.as_str(), "", 1),
    );
    let role_keywords = content_tokens(&cleaned);
    // Pass-through: the raw conversational prompt travels verbatim (bounded)
    // so sub-token coverage grounds against the live DOM labels as the
    // dictionary. Inference above stays — saved-playbook token routing, role
    // inference, previews, and ephemeral names all need the stripped
    // label/scope forms, and the coverage path itself uses zero dictionaries.
    // Ordinals ride along the same way (deterministic for both AI and
    // fallback paths): position words never pollute labels or scopes.
    let raw_prompt: String = prompt.chars().take(2000).collect();
    let (ordinal_index, is_last) = parse_ordinal(prompt);
    let primary_target_noun = extract_primary_noun(&cleaned, connected_origin);
    Some(CommandMatch::Ephemeral {
        intent: SemanticIntent {
            role: role_for(&role_keywords).to_owned(),
            label_query: parsed.label_query,
            container_query: parsed.container_query,
            raw_prompt,
            ordinal_index,
            is_last,
            is_plural: parse_plural(prompt),
            entry_url: None,
            primary_target_noun,
        },
    })
}

/// Normalized matching key for one prompt — the learning loop's index.
///
/// Lowercased and whitespace-collapsed, so `"Download  GitHub Invoices "`
/// and `"download github invoices"` are one key and a repeated phrasing
/// resolves deterministically instead of racing token overlap. Deliberately
/// *not* stemmed or stopword-filtered: this identifies an exact phrasing the
/// user already saved, and loosening it would let one saved workflow capture
/// prompts the user never taught it.
///
/// `None` for blank prompts (nothing to key) and for prompts past
/// [`playbook_store::schema::MAX_PROMPT_KEY_LEN`], which keep matching by
/// token overlap rather than being truncated into a colliding key.
#[must_use]
pub fn prompt_key(prompt: &str) -> Option<String> {
    let key = prompt
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if key.is_empty() || key.len() > playbook_store::schema::MAX_PROMPT_KEY_LEN {
        return None;
    }
    Some(key)
}

/// Filesystem-safe ephemeral workflow name from the prompt. Always passes
/// `Playbook::new` validation by construction.
#[must_use]
pub fn ephemeral_name(prompt: &str) -> String {
    let slug: String = content_tokens(prompt)
        .into_iter()
        .take(3)
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = slug.chars().take(32).collect();
    if slug.is_empty() {
        "ad-hoc".to_owned()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(id: &str, name: &str, portal_url: &str) -> PlaybookSummary {
        PlaybookSummary {
            id: id.into(),
            name: name.into(),
            portal_url: portal_url.into(),
            step_count: 1,
            updated_at: String::new(),
            description: None,
            prompt_key: None,
        }
    }

    /// The same summary plus a learned prompt key.
    fn learned(id: &str, name: &str, portal_url: &str, key: &str) -> PlaybookSummary {
        PlaybookSummary {
            prompt_key: Some(key.into()),
            ..saved(id, name, portal_url)
        }
    }

    fn portal() -> Result<url::Url, url::ParseError> {
        url::Url::parse("https://github.com/")
    }

    #[test]
    fn name_overlap_beats_host_only_match() {
        let saved = vec![
            saved("1", "reports", "https://github.com/"),
            saved("2", "github-reports", "https://github.com/"),
        ];
        assert_eq!(
            resolve_command("download my latest github report", None, &saved),
            Some(CommandMatch::Saved { id: "2".into() })
        );
    }

    #[test]
    fn host_tokens_route_portal_commands() {
        let saved = vec![saved("1", "reports", "https://github.com/")];
        assert_eq!(
            resolve_command("show github usage", None, &saved),
            Some(CommandMatch::Saved { id: "1".into() })
        );
    }

    #[test]
    fn noise_never_matches() -> Result<(), url::ParseError> {
        let portal = portal()?;
        let saved = vec![saved("1", "reports", "https://portal.example.com/")];
        // Bare noise or pure stopwords: nothing to route on.
        assert_eq!(resolve_command("the", None, &saved), None);
        assert_eq!(resolve_command("", Some(&portal), &saved), None);
        // "com" alone must not match every portal on the internet.
        assert_eq!(
            resolve_command("com", Some(&portal), &saved),
            Some(CommandMatch::Ephemeral {
                intent: SemanticIntent {
                    role: "link".into(),
                    label_query: "com".into(),
                    container_query: None,
                    raw_prompt: "com".into(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: false,
                    entry_url: None,
                    primary_target_noun: Some("com".into()),
                },
            })
        );
        Ok(())
    }

    #[test]
    fn identifiers_skip_static_replays_for_dynamic_resolution() -> Result<(), url::ParseError> {
        let portal = portal()?;
        let saved = vec![saved("1", "github-reports", "https://github.com/")];
        // Token overlap alone would replay — but the ID forces the dynamic
        // path, since the recording cannot honor a specific identifier.
        assert_eq!(
            resolve_command("download github report 0LWQXDWW", Some(&portal), &saved),
            Some(CommandMatch::Ephemeral {
                intent: SemanticIntent {
                    role: "link".into(),
                    label_query: "report".into(),
                    container_query: Some("0LWQXDWW".into()),
                    raw_prompt: "download github report 0LWQXDWW".into(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: false,
                    entry_url: None,
                    primary_target_noun: Some("report".into()),
                },
            })
        );
        // The same prompt without an identifier still replays the recording.
        assert_eq!(
            resolve_command("download github report", Some(&portal), &saved),
            Some(CommandMatch::Saved { id: "1".into() })
        );
        Ok(())
    }

    #[test]
    fn ephemeral_intents_infer_role_and_label() -> Result<(), url::ParseError> {
        let portal = portal()?;
        let cases = [
            ("fill expense report", "textbox", "report", "report"),
            ("click pay now", "button", "pay", "pay"),
            ("open dashboard", "link", "dashboard", "dashboard"),
        ];
        for (prompt, role, label, noun) in cases {
            assert_eq!(
                resolve_command(prompt, Some(&portal), &[]),
                Some(CommandMatch::Ephemeral {
                    intent: SemanticIntent {
                        role: role.into(),
                        label_query: label.into(),
                        container_query: None,
                        raw_prompt: prompt.into(),
                        ordinal_index: None,
                        is_last: false,
                        is_plural: false,
                        entry_url: None,
                        primary_target_noun: Some(noun.into()),
                    },
                }),
                "{prompt}"
            );
        }
        // No connected portal, no saved match: honestly nothing.
        assert_eq!(resolve_command("open dashboard", None, &[]), None);
        Ok(())
    }

    #[test]
    fn extract_identifier_reads_shapes_not_sites() {
        // Mixed letter-digit runs of length four or more.
        assert_eq!(
            extract_identifier("download report for ID 0LWQXDWW"),
            Some("0LWQXDWW".into())
        );
        assert_eq!(
            extract_identifier("open 1TUEWZUA now"),
            Some("1TUEWZUA".into())
        );
        assert_eq!(
            extract_identifier("pay INV-2024-001 today"),
            Some("INV-2024-001".into())
        );
        // Structured all-numeric runs: dates and amounts.
        assert_eq!(
            extract_identifier("statements from 2026-09-15"),
            Some("2026-09-15".into())
        );
        assert_eq!(
            extract_identifier("total $1,234.56 due"),
            Some("1,234.56".into())
        );
        // Weak evidence stays out: plain words, short numbers, bare years,
        // hostnames, and single characters.
        for prompt in [
            "download the monthly report",
            "toggle dark mode",
            "pay 42 now",
            "archive 2026 filings",
            "open github.com",
            "a",
        ] {
            assert_eq!(extract_identifier(prompt), None, "{prompt}");
        }
    }

    #[test]
    fn extract_identifier_strips_id_labels() -> Result<(), url::ParseError> {
        // Separator-joined labels yield pure target tokens.
        assert_eq!(
            extract_identifier("download invoice for ID:0LWQXDWW"),
            Some("0LWQXDWW".into())
        );
        assert_eq!(
            extract_identifier("open id-1TUEWZUA now"),
            Some("1TUEWZUA".into())
        );
        // Space-separated labels already split into runs; hyphenated codes
        // and hyphenated prose keep working untouched.
        assert_eq!(
            extract_identifier("pay INV-2024-001 today"),
            Some("INV-2024-001".into())
        );
        // Weak remainders keep scanning instead of returning fragments.
        assert_eq!(extract_identifier("tag ID-42 here"), None);
        // End to end: the scoped container carries the pure token.
        let portal = portal()?;
        assert_eq!(
            resolve_command("download invoice for ID:0LWQXDWW", Some(&portal), &[]),
            Some(CommandMatch::Ephemeral {
                intent: SemanticIntent {
                    role: "link".into(),
                    label_query: "invoice".into(),
                    container_query: Some("0LWQXDWW".into()),
                    raw_prompt: "download invoice for ID:0LWQXDWW".into(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: false,
                    entry_url: None,
                    primary_target_noun: Some("invoice".into()),
                },
            })
        );
        Ok(())
    }

    #[test]
    fn variable_extractor_detects_ids_and_amounts_cleanly() {
        // Grounded scenario: the ID appears in both prompt and container but
        // templates exactly once; the schema carries type and UI metadata.
        let extraction = extract_dynamic_variables(
            "download invoice for 1TUEWZUA",
            "Download invoice",
            "Invoice 1TUEWZUA",
        );
        assert_eq!(extraction.template, "download invoice for {{invoice_id}}");
        assert_eq!(extraction.variables.len(), 1);
        let variable = &extraction.variables[0];
        assert_eq!(variable.name, "invoice_id");
        assert_eq!(variable.kind, VariableKind::Id);
        assert_eq!(variable.default_value, "1TUEWZUA");
        assert_eq!(variable.field_type, "text");
        assert_eq!(variable.ui_label, "Invoice ID");
        // Amounts, dates, and quoted entities classify distinctly.
        let extraction =
            extract_dynamic_variables("pay $45.00 on Sep 17 for \"subheader.lol\"", "", "");
        assert_eq!(
            extraction.template,
            "pay {{amount}} on {{date}} for {{entity_name}}"
        );
        let kinds: Vec<VariableKind> = extraction
            .variables
            .iter()
            .map(|variable| variable.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                VariableKind::Amount,
                VariableKind::Date,
                VariableKind::Entity
            ]
        );
        // Plain prompts stay literal with an empty schema.
        let extraction = extract_dynamic_variables("toggle dark mode", "", "");
        assert_eq!(extraction.template, "toggle dark mode");
        assert!(extraction.variables.is_empty());
    }

    #[test]
    fn identifiers_scope_ephemeral_intents() -> Result<(), url::ParseError> {
        let portal = portal()?;
        // The ID leaves the label stream and becomes the container scope.
        assert_eq!(
            resolve_command("download report for ID 0LWQXDWW", Some(&portal), &[]),
            Some(CommandMatch::Ephemeral {
                intent: SemanticIntent {
                    role: "link".into(),
                    label_query: "report".into(),
                    container_query: Some("0LWQXDWW".into()),
                    raw_prompt: "download report for ID 0LWQXDWW".into(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: false,
                    entry_url: None,
                    primary_target_noun: Some("report".into()),
                },
            })
        );
        // A lone identifier still runs: it doubles as a last-resort label.
        assert_eq!(
            resolve_command("0LWQXDWW", Some(&portal), &[]),
            Some(CommandMatch::Ephemeral {
                intent: SemanticIntent {
                    role: "link".into(),
                    label_query: "0LWQXDWW".into(),
                    container_query: Some("0LWQXDWW".into()),
                    raw_prompt: "0LWQXDWW".into(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: false,
                    entry_url: None,
                    primary_target_noun: None,
                },
            })
        );
        // Plain prompts keep working with no scope attached.
        assert_eq!(
            resolve_command("toggle dark mode", Some(&portal), &[]),
            Some(CommandMatch::Ephemeral {
                intent: SemanticIntent {
                    role: "link".into(),
                    label_query: "mode".into(),
                    container_query: None,
                    raw_prompt: "toggle dark mode".into(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: false,
                    entry_url: None,
                    primary_target_noun: Some("mode".into()),
                },
            })
        );
        Ok(())
    }

    #[test]
    fn ephemeral_names_always_validate() -> Result<(), Box<dyn std::error::Error>> {
        let portal = portal()?;
        for prompt in ["Pay my !!! reports ???", "", "a"] {
            let name = ephemeral_name(prompt);
            let playbook = playbook_store::Playbook::new(
                name,
                portal.clone(),
                vec![playbook_store::Step::Semantic {
                    intent: SemanticIntent {
                        role: "link".into(),
                        label_query: "x".into(),
                        container_query: None,
                        raw_prompt: String::new(),
                        ordinal_index: None,
                        is_last: false,
                        is_plural: false,
                        entry_url: None,
                        primary_target_noun: None,
                    },
                }],
            )?;
            assert!(!playbook.name.is_empty());
        }
        assert_eq!(
            ephemeral_name("Pay my monthly reports"),
            "pay-monthly-reports"
        );
        Ok(())
    }

    #[test]
    fn structured_pass_handles_any_case_and_format() -> Result<(), url::ParseError> {
        let portal = portal()?;
        // Lowercase ID: preserved as typed, matching stays case-insensitive
        // downstream in the executor.
        let Some(parsed) = parse_intent_structured("download invoice for me for ID 0lwqxdww")
        else {
            panic!("lowercase ID parses");
        };
        assert_eq!(parsed.container_query.as_deref(), Some("0lwqxdww"));
        assert!(!parsed.label_query.is_empty());
        // Attributes plus date collapse into one container scope.
        let Some(parsed) = parse_intent_structured("download the $4 declined invoice from June 12")
        else {
            panic!("attributes parse");
        };
        let Some(scope) = parsed.container_query else {
            panic!("scope present");
        };
        assert!(scope.contains("$4"), "{scope}");
        assert!(scope.to_ascii_lowercase().contains("declined"), "{scope}");
        assert!(scope.to_ascii_lowercase().contains("june"), "{scope}");
        assert_eq!(parsed.label_query, "invoice");
        // Relative position and relative time never become labels or vetoes.
        assert_eq!(
            parse_intent_structured("download the second invoice in the list")
                .map(|parsed| parsed.label_query),
            Some("invoice".into())
        );
        assert_eq!(
            parse_intent_structured("get the receipt for last week")
                .map(|parsed| parsed.label_query),
            Some("receipt".into())
        );
        // End-to-end: lowercase prompt still scopes the ephemeral intent.
        assert_eq!(
            resolve_command("download invoice for ID 0lwqxdww", Some(&portal), &[]),
            Some(CommandMatch::Ephemeral {
                intent: SemanticIntent {
                    role: "link".into(),
                    label_query: "invoice".into(),
                    container_query: Some("0lwqxdww".into()),
                    raw_prompt: "download invoice for ID 0lwqxdww".into(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: false,
                    entry_url: None,
                    primary_target_noun: Some("invoice".into()),
                },
            })
        );
        Ok(())
    }

    #[test]
    fn navigation_verbs_isolate_target_labels() -> Result<(), url::ParseError> {
        // Verb isolation lives in the deterministic fallback (there is no
        // LLM system prompt in-tree: the provider speaks the JSON contract
        // and the fallback must already separate verbs from targets, since
        // it runs whenever the provider is unset). Navigation verbs never
        // leak into the label; the role carries the click/navigate action
        // (`link` here — `SemanticIntent` has no separate action field, and
        // labels normalize to lowercase for case-insensitive matching).
        let portal = portal()?;
        for (prompt, label, noun) in [
            ("open the Analytics", "analytics", "analytic"),
            ("open the Deployments", "deployments", "deployment"),
        ] {
            assert_eq!(
                resolve_command(prompt, Some(&portal), &[]),
                Some(CommandMatch::Ephemeral {
                    intent: SemanticIntent {
                        role: "link".into(),
                        label_query: label.into(),
                        container_query: None,
                        raw_prompt: prompt.into(),
                        ordinal_index: None,
                        is_last: false,
                        is_plural: false,
                        entry_url: None,
                        primary_target_noun: Some(noun.into()),
                    },
                }),
                "{prompt}"
            );
        }
        Ok(())
    }

    #[test]
    fn ordinals_parse_to_positions_without_polluting_labels() -> Result<(), url::ParseError> {
        // Rank words reuse the existing ordinal vocabulary; a bare `last`
        // beside time words stays temporal (no position).
        assert_eq!(
            parse_ordinal("download the second invoice"),
            (Some(1), false)
        );
        assert_eq!(parse_ordinal("click the 2nd button"), (Some(1), false));
        assert_eq!(parse_ordinal("open the first report"), (Some(0), false));
        assert_eq!(parse_ordinal("open the last invoice"), (None, true));
        assert_eq!(
            parse_ordinal("get the receipt for last week"),
            (None, false)
        );
        assert_eq!(parse_ordinal("toggle dark mode"), (None, false));
        // End to end: position rides the ephemeral intent, label stays clean.
        let portal = portal()?;
        assert_eq!(
            resolve_command("download the second invoice", Some(&portal), &[]),
            Some(CommandMatch::Ephemeral {
                intent: SemanticIntent {
                    role: "link".into(),
                    label_query: "invoice".into(),
                    container_query: None,
                    raw_prompt: "download the second invoice".into(),
                    ordinal_index: Some(1),
                    is_last: false,
                    is_plural: false,
                    entry_url: None,
                    primary_target_noun: Some("invoice".into()),
                },
            })
        );
        Ok(())
    }

    #[test]
    fn composite_prompt_decomposes_into_sequential_steps() -> Result<(), url::ParseError> {
        let portal = portal()?;
        // Two connectors, two ordered ephemeral intents; each segment keeps
        // its own label, raw prompt, and position data.
        let steps = decompose_command("open settings and turn off dark mode", Some(&portal), &[]);
        assert_eq!(
            steps,
            vec![
                CommandMatch::Ephemeral {
                    intent: SemanticIntent {
                        role: "link".into(),
                        label_query: "settings".into(),
                        container_query: None,
                        raw_prompt: "open settings".into(),
                        ordinal_index: None,
                        is_last: false,
                        is_plural: false,
                        entry_url: None,
                        primary_target_noun: Some("setting".into()),
                    },
                },
                CommandMatch::Ephemeral {
                    intent: SemanticIntent {
                        role: "link".into(),
                        label_query: "mode".into(),
                        container_query: None,
                        raw_prompt: "turn off dark mode".into(),
                        ordinal_index: None,
                        is_last: false,
                        is_plural: false,
                        entry_url: None,
                        primary_target_noun: Some("mode".into()),
                    },
                },
            ]
        );
        // Comma-then splits without leaving punctuation on either side, and
        // single-action prompts stay single-element.
        let steps = decompose_command("open settings, then turn off dark mode", Some(&portal), &[]);
        assert_eq!(steps.len(), 2);
        // Case-insensitive connectors split identically; only the pass-through
        // raw text keeps its original casing.
        let loud = decompose_command("OPEN SETTINGS AND TURN OFF DARK MODE", Some(&portal), &[]);
        let shape = |steps: &[CommandMatch]| -> Vec<String> {
            steps
                .iter()
                .map(|step| match step {
                    CommandMatch::Saved { id } => format!("saved:{id}"),
                    CommandMatch::Ephemeral { intent } => {
                        format!("{}:{}", intent.role, intent.label_query)
                    }
                })
                .collect()
        };
        assert_eq!(shape(&steps), shape(&loud));
        assert_eq!(
            decompose_command("toggle dark mode", Some(&portal), &[]).len(),
            1
        );
        assert!(decompose_command("", Some(&portal), &[]).is_empty());
        Ok(())
    }

    #[test]
    fn plural_intent_extracted_on_all_keyword() -> Result<(), url::ParseError> {
        // Collection markers ride the intent without polluting the label:
        // `all`/`every`/`each` set the flag, everything else stays singular.
        assert!(parse_plural("download all invoices"));
        assert!(parse_plural("get every receipt"));
        assert!(parse_plural("open each statement"));
        assert!(!parse_plural("download invoice"));
        assert!(!parse_plural("toggle dark mode"));
        assert!(!parse_plural("overall summary"));
        let portal = portal()?;
        assert_eq!(
            resolve_command("download all invoices", Some(&portal), &[]),
            Some(CommandMatch::Ephemeral {
                intent: SemanticIntent {
                    role: "link".into(),
                    label_query: "invoices".into(),
                    container_query: None,
                    raw_prompt: "download all invoices".into(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: true,
                    entry_url: None,
                    primary_target_noun: Some("invoice".into()),
                },
            })
        );
        Ok(())
    }

    #[test]
    fn plural_prompts_skip_single_step_saved_replays() -> Result<(), url::ParseError> {
        let portal = portal()?;
        let saved = vec![saved("1", "download-invoices", "https://github.com/")];
        // Token overlap alone would replay — but plurality forces the dynamic
        // batch path, since a one-click recording cannot honor "all".
        let matched = resolve_command(
            "download all my invoices from github",
            Some(&portal),
            &saved,
        );
        let Some(CommandMatch::Ephemeral { intent }) = matched else {
            panic!("plural prompt bypasses the saved replay");
        };
        assert!(intent.is_plural);
        // The singular twin still replays the recording untouched.
        assert_eq!(
            resolve_command("download invoice from github", Some(&portal), &saved),
            Some(CommandMatch::Saved { id: "1".into() })
        );
        Ok(())
    }

    #[test]
    fn test_grammar_parsing_without_portal_whitelist() -> Result<(), url::ParseError> {
        // Prepositional complement: the noun before the cue is the artifact
        // the batch acts on, the token after it names the domain.
        let parsed = parse_grammar("download all my invoices from github", None);
        assert_eq!(parsed.artifact_noun.as_deref(), Some("invoice"));
        assert_eq!(parsed.site_context.as_deref(), Some("github"));
        assert_eq!(parsed.target_noun, None);
        let parsed = parse_grammar("download reports from linear", None);
        assert_eq!(parsed.artifact_noun.as_deref(), Some("report"));
        assert_eq!(parsed.site_context.as_deref(), Some("linear"));
        assert_eq!(parsed.target_noun, None);
        // Direct action: no complement, so the direct object *is* the target.
        let parsed = parse_grammar("open amazon for me", None);
        assert_eq!(parsed.target_noun.as_deref(), Some("amazon"));
        assert_eq!(parsed.site_context, None);
        assert_eq!(parsed.artifact_noun, None);
        // Structure decides, not vocabulary: a site nobody enumerated parses
        // exactly like `github`, and `on`/`at` read the same as `from`.
        for (prompt, artifact, site) in [
            ("download invoices from acmecorp", "invoice", "acmecorp"),
            ("grab my statements on zzyzxbank", "statement", "zzyzxbank"),
            ("export receipts at quuxvendor", "receipt", "quuxvendor"),
        ] {
            let parsed = parse_grammar(prompt, None);
            assert_eq!(parsed.artifact_noun.as_deref(), Some(artifact), "{prompt}");
            assert_eq!(parsed.site_context.as_deref(), Some(site), "{prompt}");
        }
        // A cue with no noun before it is a phrasal-verb particle, not a
        // complement: `click on pay now` still targets the control.
        let parsed = parse_grammar("click on pay now", None);
        assert_eq!(parsed.site_context, None);
        assert_eq!(parsed.target_noun.as_deref(), Some("pay"));
        // A cue followed by structure rather than a place is not a complement
        // either: months, ordinals, and digits never name a destination.
        let parsed = parse_grammar("download the declined invoice from June 12", None);
        assert_eq!(parsed.site_context, None);
        assert_eq!(parsed.target_noun.as_deref(), Some("invoice"));
        // Parsing is connection-independent: the same prompt yields the same
        // slots whether parked on the named portal, on another one, or on
        // nothing at all.
        let portal = portal()?;
        let google = url::Url::parse("https://google.com")?;
        for origin in [None, Some(&portal), Some(&google)] {
            let parsed = parse_grammar("download all my invoices from github", origin);
            assert_eq!(parsed.artifact_noun.as_deref(), Some("invoice"));
            assert_eq!(parsed.site_context.as_deref(), Some("github"));
        }
        // Nothing content-bearing leaves every slot empty.
        assert_eq!(parse_grammar("", None), ParsedGrammar::default());
        assert_eq!(parse_grammar("the", None), ParsedGrammar::default());
        Ok(())
    }

    #[test]
    fn confidence_gates_on_structure_not_slot_count() -> Result<(), url::ParseError> {
        // Plain imperatives with complete slots are trusted outright.
        for prompt in [
            "download invoices from github",
            "download all my invoices from github",
            "grab my statements on zzyzxbank",
            "export receipts at quuxvendor",
            // Direct action: the object is the destination, and that is a
            // complete parse for its pattern.
            "open amazon for me",
            "click pay now",
            "toggle dark mode",
            "fill expense report",
        ] {
            assert_eq!(
                parse_grammar(prompt, None).confidence,
                Confidence::High,
                "{prompt} is a plain imperative"
            );
        }
        // A subordinate clause means more grammar than this parser models.
        // Note both slots *are* populated here — slot count alone would have
        // called this confident, and the artifact would have been `owe`.
        let parsed = parse_grammar("pull up what I owe on aws", None);
        assert_eq!(parsed.confidence, Confidence::Low);
        assert_eq!(parsed.artifact_noun.as_deref(), Some("owe"));
        assert_eq!(parsed.site_context.as_deref(), Some("aws"));
        for prompt in [
            "pull up what I owe on aws",
            "show me which invoices are overdue",
            "find whatever bills are on github",
            "get the thing that I paid for",
        ] {
            assert_eq!(
                parse_grammar(prompt, None).confidence,
                Confidence::Low,
                "{prompt} carries a subordinate clause"
            );
        }
        // No verb to anchor on: a bare noun phrase is not a command.
        for prompt in ["com", "statements", "my invoices", "the github thing"] {
            assert_eq!(
                parse_grammar(prompt, None).confidence,
                Confidence::Low,
                "{prompt} has no verb head"
            );
        }
        // Nothing content-bearing is never confident, and the default agrees.
        assert_eq!(parse_grammar("", None).confidence, Confidence::Low);
        assert_eq!(Confidence::default(), Confidence::Low);
        assert!(Confidence::High.is_high());
        assert!(!Confidence::Low.is_high());
        // Confidence is a property of the prompt, not of the live session.
        let portal = portal()?;
        assert_eq!(
            parse_grammar("download invoices from github", Some(&portal)).confidence,
            Confidence::High
        );
        Ok(())
    }

    #[test]
    fn role_inference_only_emits_the_shared_action_vocabulary() {
        // The parser seam validates model output against `INTENT_ROLES`, so
        // that list must stay exactly what `role_for` can produce — otherwise
        // the two vocabularies drift and a valid parse gets rejected (or an
        // invalid one accepted).
        for prompt in [
            "fill expense report",
            "type my address",
            "enter the code",
            "click pay now",
            "press submit",
            "submit the form",
            "tap continue",
            "select a plan",
            "choose the date",
            "download invoices from github",
            "open amazon",
            "",
        ] {
            let role = role_for(&content_tokens(prompt));
            assert!(INTENT_ROLES.contains(&role), "{prompt} inferred {role}");
        }
    }

    #[test]
    fn prompt_keys_normalize_phrasing_and_refuse_unusable_keys() {
        // Casing and spacing collapse, so one phrasing is one key.
        assert_eq!(
            prompt_key("  Pull up   what I owe ON aws ").as_deref(),
            Some("pull up what i owe on aws")
        );
        assert_eq!(
            prompt_key("download github invoices").as_deref(),
            Some("download github invoices")
        );
        // Deliberately not stemmed or stopword-filtered: a key identifies the
        // exact phrasing the user saved, so distinct prompts stay distinct.
        assert_ne!(
            prompt_key("download my invoices"),
            prompt_key("download invoices")
        );
        // Unusable keys are refused rather than stored: blanks would match
        // every blank prompt, and truncating an oversized prompt would let
        // two different commands collide on one workflow.
        assert_eq!(prompt_key(""), None);
        assert_eq!(prompt_key("   \n\t "), None);
        let oversized = "a ".repeat(playbook_store::schema::MAX_PROMPT_KEY_LEN);
        assert_eq!(prompt_key(&oversized), None);
    }

    #[test]
    fn learned_prompt_keys_win_routing_without_bypassing_guards() -> Result<(), url::ParseError> {
        let portal = portal()?;
        // An exact key outranks a stronger token-overlap competitor: the
        // phrasing was taught, so it is not a guess to be outvoted.
        let with_key = vec![
            saved("1", "github-invoices", "https://github.com/"),
            learned(
                "2",
                "aws-bills",
                "https://aws.amazon.com/",
                "get my github bills",
            ),
        ];
        assert_eq!(
            resolve_command("get my github bills", Some(&portal), &with_key),
            Some(CommandMatch::Saved { id: "2".into() }),
            "the learned key wins over name overlap"
        );
        // Without the key, the same prompt routes by overlap as before, so
        // the key is additive rather than a behavior change.
        let unlearned = vec![
            saved("1", "github-invoices", "https://github.com/"),
            saved("2", "aws-bills", "https://aws.amazon.com/"),
        ];
        assert_eq!(
            resolve_command("get my github bills", Some(&portal), &unlearned),
            Some(CommandMatch::Saved { id: "1".into() })
        );
        // The key ranks candidates; it never overrides the guards that keep
        // replay honest. Saved replay resolves one control per step, so a
        // plural prompt still takes the dynamic path even with its own key
        // stored — replaying it would click once and under-deliver.
        let plural = vec![learned(
            "9",
            "all-invoices",
            "https://github.com/",
            "download all my invoices from github",
        )];
        assert!(
            matches!(
                resolve_command(
                    "download all my invoices from github",
                    Some(&portal),
                    &plural
                ),
                Some(CommandMatch::Ephemeral { .. })
            ),
            "plurality still forces the dynamic batch lane"
        );
        // Same for a prompt carrying a specific scope: the recording has no
        // parameter slot for `0LWQXDWW`.
        let scoped = vec![learned(
            "9",
            "one-invoice",
            "https://github.com/",
            "download invoice 0lwqxdww",
        )];
        assert!(matches!(
            resolve_command("download invoice 0LWQXDWW", Some(&portal), &scoped),
            Some(CommandMatch::Ephemeral { .. })
        ));
        Ok(())
    }

    #[test]
    fn primary_noun_strips_modifiers_and_singularizes() -> Result<(), url::ParseError> {
        // Modifiers never survive: the anchor is always the content word in
        // stem form. Lone identifiers carry no anchor at all.
        assert_eq!(
            extract_primary_noun("download all my invoices", None).as_deref(),
            Some("invoice")
        );
        assert_eq!(
            extract_primary_noun("open the Analytics", None).as_deref(),
            Some("analytic")
        );
        // Trailing site words yield to the object noun: the grammar reads
        // them as the destination complement, not the batch target.
        let portal = portal()?;
        assert_eq!(
            extract_primary_noun("download all my invoices from github", Some(&portal)).as_deref(),
            Some("invoice")
        );
        // Cross-portal starts hold too: parked on `google.com`, the `github`
        // trailer is still the destination slot, so the anchor stays the
        // content noun and pre-navigation can fire.
        let google = url::Url::parse("https://google.com")?;
        assert_eq!(
            extract_primary_noun("download all my invoices from github", Some(&google)).as_deref(),
            Some("invoice")
        );
        // Identifier-stripped streams only: a lone identifier arrives
        // empty after cleaning, exactly as `resolve_command` passes it.
        assert_eq!(extract_primary_noun("", None), None);
        assert_eq!(extract_primary_noun("the", None), None);
        // End to end: the ephemeral intent carries the anchor.
        assert_eq!(
            resolve_command("download all my invoices", Some(&portal), &[]),
            Some(CommandMatch::Ephemeral {
                intent: SemanticIntent {
                    role: "link".into(),
                    label_query: "invoices".into(),
                    container_query: None,
                    raw_prompt: "download all my invoices".into(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: true,
                    entry_url: None,
                    primary_target_noun: Some("invoice".into()),
                },
            })
        );
        Ok(())
    }

    #[test]
    fn scoped_attributes_skip_saved_replays() -> Result<(), url::ParseError> {
        let portal = portal()?;
        let saved = vec![saved("1", "invoices", "https://github.com/")];
        // Name overlap alone would replay — but amount/status/date scope
        // forces the dynamic path, since the recording has no parameter slots.
        let matched = resolve_command(
            "download the $4 declined invoice from June 12",
            Some(&portal),
            &saved,
        );
        assert!(matches!(matched, Some(CommandMatch::Ephemeral { .. })));
        Ok(())
    }

    #[test]
    fn tier_zero_matches_lifecycle_commands_through_filler() {
        // Every phrasing of "start the browser" resolves without a search —
        // the frozen normalizer strips politeness affixes from the ends and
        // matches the closed phrase set exactly, so "new blank page" works
        // (the full stopword filter would eat "new").
        for prompt in [
            "spin up browser for me",
            "spin up the browser",
            "please spin up browser",
            "spin up browser for us",
            "spin up browser, thanks",
            "spin up browser thank you",
            "open the browser",
            "launch browser",
            "show me the browser",
            "open a blank tab",
            "open a blank tab for me",
            "new blank page",
        ] {
            assert_eq!(
                resolve_app_command(prompt),
                Some(AppCommand::OpenBlankBrowser),
                "{prompt}"
            );
        }
    }

    #[test]
    fn tier_zero_rejects_near_misses() {
        // Closed-world: an extra content token, or no lifecycle verb, falls
        // through instead of being swallowed.
        for prompt in [
            "open amazon for me",
            "open browser settings",
            "download the browser",
            "browser",
            "spin up",
            "",
            "open amazon and flipkart",
        ] {
            assert_eq!(resolve_app_command(prompt), None, "{prompt}");
        }
    }

    #[test]
    fn direct_open_covers_open_phrasings_and_filler() {
        // High-confidence single-target opens, with and without filler.
        for prompt in [
            "open amazon",
            "open amazon for me",
            "please open amazon",
            "kindly open amazon",
            "open amazon.in",
            "visit github",
            "launch amazon",
            "go to amazon",
        ] {
            let grammar = parse_grammar(prompt, None);
            assert!(grammar.confidence.is_high(), "{prompt}");
            assert!(is_direct_open(prompt, &grammar), "{prompt}");
        }
    }

    #[test]
    fn direct_open_rejects_artifacts_clauses_and_batches() {
        // Retrieval keeps the search path.
        for prompt in [
            "download all my invoices from github",
            "find amazon",
            "check amazon prices",
        ] {
            let grammar = parse_grammar(prompt, None);
            assert!(!is_direct_open(prompt, &grammar), "{prompt}");
        }
        // Multi-target prompts must re-resolve through batch consent, never
        // silently open one target.
        for prompt in ["open amazon and flipkart", "open amazon or flipkart"] {
            let grammar = parse_grammar(prompt, None);
            assert!(!is_direct_open(prompt, &grammar), "{prompt}");
        }
        // A subordinate clause is not a plain imperative.
        let grammar = parse_grammar("pull up what I owe on aws", None);
        assert!(!is_direct_open("pull up what I owe on aws", &grammar));
    }
}
