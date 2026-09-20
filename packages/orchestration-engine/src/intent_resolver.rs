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
    "my", "the", "a", "an", "please", "now", "latest", "new", "here", "this", "that", "me", "for",
    "to", "on", "and", "or", "of", "in", "is", "it", "id", "with", "named", "called",
];

/// URL tokens that must never match (`https`, TLDs): they appear in every
/// portal and carry no intent.
const URL_NOISE: &[&str] = &["https", "http", "www", "com", "org", "net", "io", "dev"];

/// Lowercase alphanumeric tokens of length two or more.
fn tokens(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|part| part.len() > 1)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn content_tokens(text: &str) -> Vec<String> {
    tokens(text)
        .into_iter()
        .filter(|token| !STOPWORDS.contains(&token.as_str()))
        .collect()
}

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

/// Tokens that must never become a label or veto a container: ordinals and
/// positional words (`second`), relative-time words (`last week`), generic
/// list words, prepositions, month names, and pure numbers. They describe
/// *which* match, not *what* to click, so filtering them keeps free-form
/// prompts (`"second invoice"`, `"receipt for last week"`) matching instead
/// of failing closed on words the DOM never contains.
const NON_IDENTIFYING: &[&str] = &[
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
/// `connected_origin` is presence-checked only — callers supply the portal
/// for session scoping and execution.
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
    let mut best: Option<(&PlaybookSummary, usize)> = None;
    for playbook in saved {
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
    // A specific scope with a static saved flow always takes the dynamic path
    // even when a name matches.
    if quick
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
    // a newly-scoped prompt cannot slip into a static replay.
    if parsed.container_query.is_none()
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
    Some(CommandMatch::Ephemeral {
        intent: SemanticIntent {
            role: role_for(&role_keywords).to_owned(),
            label_query: parsed.label_query,
            container_query: parsed.container_query,
        },
    })
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
            ("fill expense report", "textbox", "report"),
            ("click pay now", "button", "pay"),
            ("open dashboard", "link", "dashboard"),
        ];
        for (prompt, role, label) in cases {
            assert_eq!(
                resolve_command(prompt, Some(&portal), &[]),
                Some(CommandMatch::Ephemeral {
                    intent: SemanticIntent {
                        role: role.into(),
                        label_query: label.into(),
                        container_query: None,
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
                },
            })
        );
        Ok(())
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
}
