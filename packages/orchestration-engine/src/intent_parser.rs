#![deny(unsafe_code)]
//! Fenced structured-intent parser seam.
//!
//! When the deterministic grammar fast path reports
//! [`crate::Confidence::Low`], a prompt like `"pull up what I owe on aws"`
//! needs more English than a token parser models. This module is the seam a
//! language model plugs into — and the fence that keeps it harmless.
//!
//! # What the seam may return
//!
//! Slots, and only slots: `{ action, artifact_noun, site_context }`. A
//! parser never returns a URL, a CSS selector, an `XPath`, CDP code, or page
//! text. [`ParsedSlots::sanitized`] enforces that structurally rather than
//! by trust — a single-token character fence that a URL, path, or selector
//! cannot survive — so a hostile or merely sloppy model cannot widen its own
//! authority. Everything downstream (URL policy, search-and-follow
//! grounding, Sentinel approval gates) still runs exactly as it does for a
//! deterministic parse.
//!
//! # What the seam receives
//!
//! The raw prompt string, nothing else. No page HTML, no accessibility
//! tree, no cookies, no session state. The parser reads what the user typed
//! and answers with slots.
//!
//! # No model ships here
//!
//! This pass lands the boundary, not a provider. [`StubIntentParser`] is
//! the production default and answers `None`, which degrades to raw search.
//! Choosing local (Ollama) versus a hosted API is a product decision about
//! dependency weight and whether prompts leave the machine; either is a
//! small adapter behind [`IntentParser`] once that call is made.
//!
//! # Bounded by construction
//!
//! [`parse_prompt_bounded`] returns within [`PARSER_TIMEOUT_MS`] whatever
//! the adapter does, so an unreachable model degrades instead of hanging a
//! run. Offline is a normal outcome, not an error path.

use std::sync::{Arc, mpsc};

/// Hard ceiling on one slot-parse round trip. A parse is a latency budget
/// spent once per novel prompt, in front of a multi-second browser task;
/// past this the answer is worth less than the delay.
pub const PARSER_TIMEOUT_MS: u64 = 1500;

/// Upper bound on one slot value. Slots are single words
/// (`invoice`, `github`), so this is generous; its real job is refusing
/// anything long enough to be a URL, a selector, or smuggled prose.
const MAX_SLOT_LEN: usize = 64;

/// Upper bound on the prompt handed to a parser. Mirrors the raw-prompt
/// bound the intent carries, so a parser never sees more than the engine
/// itself would replay.
const MAX_PROMPT_CHARS: usize = 2000;

/// Structured slots parsed out of one prompt.
///
/// `action` is a [`crate::SemanticIntent`] role value — `link`, `button`,
/// `textbox`, or `combobox` — validated against the deterministic path's
/// own vocabulary rather than a second enum, so the two can never drift.
/// The field is named for the `action` key in the fenced JSON contract a
/// model answers with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSlots {
    pub action: String,
    pub artifact_noun: Option<String>,
    pub site_context: Option<String>,
}

/// Whether one slot value is a bare single token: ASCII alphanumerics plus
/// internal hyphens, nothing else.
///
/// This is the fence. Every shape a parser must never return fails it:
/// `https://github.com/account/billing` (scheme punctuation), `a.btn > span`
/// (selector syntax), `//div[@id]` (`XPath`), `document.querySelector(…)`
/// (code), and any multi-word span (whitespace). Rejecting is cheap because
/// a rejected parse simply degrades to search.
fn is_bare_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SLOT_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && value.bytes().any(|byte| byte.is_ascii_alphanumeric())
}

impl ParsedSlots {
    /// Validate and normalize parser output, or reject it entirely.
    ///
    /// Fails closed as a unit: a bogus `action` discards the whole parse
    /// rather than half-trusting the nouns, because a model that violated
    /// the schema in one field has not earned belief in the others. Empty
    /// and whitespace-only nouns normalize to `None` (a parser declining a
    /// slot is legitimate); malformed ones reject.
    ///
    /// Returning `None` is not an error path — the caller degrades to raw
    /// search, which is exactly what an absent parser does.
    #[must_use]
    pub fn sanitized(&self) -> Option<Self> {
        let action = self.action.trim().to_ascii_lowercase();
        if !crate::intent_resolver::INTENT_ROLES.contains(&action.as_str()) {
            return None;
        }
        let (artifact_noun, site_context) = match (
            sanitize_slot(self.artifact_noun.as_deref()),
            sanitize_slot(self.site_context.as_deref()),
        ) {
            (SlotCheck::Rejected, _) | (_, SlotCheck::Rejected) => return None,
            (artifact, site) => (artifact.into_token(), site.into_token()),
        };
        Some(Self {
            action,
            artifact_noun,
            site_context,
        })
    }
}

/// What normalizing one optional noun slot found.
enum SlotCheck {
    /// Absent or blank. A parser declining a slot is legitimate.
    Absent,
    /// A bare token, normalized.
    Token(String),
    /// Failed the fence, so the whole parse is discarded.
    Rejected,
}

impl SlotCheck {
    /// The normalized token, if any. Only called once rejection is ruled out.
    fn into_token(self) -> Option<String> {
        match self {
            Self::Token(token) => Some(token),
            Self::Absent | Self::Rejected => None,
        }
    }
}

/// Normalize one optional noun slot against the fence.
fn sanitize_slot(value: Option<&str>) -> SlotCheck {
    let Some(value) = value else {
        return SlotCheck::Absent;
    };
    let trimmed = value.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        SlotCheck::Absent
    } else if is_bare_token(&trimmed) {
        SlotCheck::Token(trimmed)
    } else {
        SlotCheck::Rejected
    }
}

/// A structured-intent parser: prompt text in, fenced slots out.
///
/// Synchronous by contract, matching [`crate::LlmUrlProposer`]. Adapters
/// doing I/O should still bound it internally; [`parse_prompt_bounded`]
/// guarantees the *caller* is never blocked past [`PARSER_TIMEOUT_MS`]
/// regardless.
///
/// `None` means "no opinion" and is always safe: the cascade degrades to
/// raw search. Implementations must not panic — a panicking adapter is
/// treated as `None` by [`parse_prompt_bounded`], but silence is cheaper.
pub trait IntentParser: Send + Sync {
    /// Parse `prompt` into slots, or decline.
    fn parse_prompt(&self, prompt: &str) -> Option<ParsedSlots>;
}

/// The production default: declines every prompt.
///
/// With no provider configured this keeps the cascade total — low-confidence
/// prompts degrade to raw search instead of failing — so the app ships
/// offline-first with no model dependency, no API key, and no prompt
/// leaving the machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct StubIntentParser;

impl IntentParser for StubIntentParser {
    fn parse_prompt(&self, _prompt: &str) -> Option<ParsedSlots> {
        None
    }
}

/// Deterministic parser fixture for tests: answers with fixed slots after an
/// optional delay, and counts calls so a test can prove the fast path
/// bypassed it.
///
/// Public for the same reason `browser_driver::test_utils` is: the
/// integration suite lives outside this crate and needs to drive the seam
/// without a model. It performs no I/O and reads no state beyond its own
/// configuration.
#[derive(Debug, Default)]
pub struct TestDoubleIntentParser {
    answer: Option<ParsedSlots>,
    delay: Option<std::time::Duration>,
    calls: std::sync::atomic::AtomicUsize,
}

impl TestDoubleIntentParser {
    /// A double that answers `slots` immediately.
    #[must_use]
    pub fn answering(slots: ParsedSlots) -> Self {
        Self {
            answer: Some(slots),
            delay: None,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// A double that blocks for `delay` before answering, for proving the
    /// timeout degrades rather than waits.
    #[must_use]
    pub fn stalling(delay: std::time::Duration) -> Self {
        Self {
            answer: None,
            delay: Some(delay),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// How many times the cascade actually consulted this parser.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl IntentParser for TestDoubleIntentParser {
    fn parse_prompt(&self, _prompt: &str) -> Option<ParsedSlots> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(delay) = self.delay {
            std::thread::sleep(delay);
        }
        self.answer.clone()
    }
}

/// Consult `parser` for `prompt`, returning within [`PARSER_TIMEOUT_MS`] and
/// only ever handing back slots that survived [`ParsedSlots::sanitized`].
///
/// The adapter runs on a detached worker thread and the result arrives over
/// a channel, so a wedged or offline provider costs the caller the timeout
/// and nothing more — its late answer is discarded rather than awaited. A
/// panicking adapter drops the sender, which reads as a decline.
///
/// Empty prompts skip the call: there is nothing to parse, and spending the
/// budget to learn that would be waste.
///
/// Blocking note: this parks the calling thread for up to the timeout. The
/// cascade is synchronous today and the shipped [`StubIntentParser`] answers
/// instantly, so nothing blocks in practice. An adapter doing real I/O
/// should be driven from a blocking-safe context at the service boundary.
#[must_use]
pub fn parse_prompt_bounded(parser: &Arc<dyn IntentParser>, prompt: &str) -> Option<ParsedSlots> {
    let bounded: String = prompt.trim().chars().take(MAX_PROMPT_CHARS).collect();
    if bounded.is_empty() {
        return None;
    }
    let (send, receive) = mpsc::sync_channel::<Option<ParsedSlots>>(1);
    let worker = Arc::clone(parser);
    // Detached on purpose: a synchronous adapter cannot be cancelled, so the
    // timeout bounds the wait rather than the work.
    std::thread::Builder::new()
        .name("clinch-intent-parse".to_owned())
        .spawn(move || {
            let _ = send.send(worker.parse_prompt(&bounded));
        })
        .ok()?;
    receive
        .recv_timeout(std::time::Duration::from_millis(PARSER_TIMEOUT_MS))
        .ok()
        .flatten()
        .as_ref()
        .and_then(ParsedSlots::sanitized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(action: &str, artifact: Option<&str>, site: Option<&str>) -> ParsedSlots {
        ParsedSlots {
            action: action.into(),
            artifact_noun: artifact.map(str::to_owned),
            site_context: site.map(str::to_owned),
        }
    }

    #[test]
    fn fence_rejects_urls_selectors_and_code_in_every_slot() {
        // The whole point of the seam: a model can answer with slots and
        // nothing else. Each of these is a capability escalation attempt.
        for hostile in [
            "https://github.com/account/billing",
            "//github.com/x",
            "github.com/account",
            "a.btn > span",
            "#invoice-table tr",
            "//div[@id='x']",
            "document.querySelector('a')",
            "javascript:alert(1)",
            "two words",
            "github\namazon",
            "../../etc/passwd",
        ] {
            assert_eq!(
                slots("link", None, Some(hostile)).sanitized(),
                None,
                "site slot must reject {hostile:?}"
            );
            assert_eq!(
                slots("link", Some(hostile), None).sanitized(),
                None,
                "artifact slot must reject {hostile:?}"
            );
        }
        // Length is bounded too, so no slot can carry a payload.
        let long = "a".repeat(MAX_SLOT_LEN + 1);
        assert_eq!(slots("link", None, Some(&long)).sanitized(), None);
    }

    #[test]
    fn fence_accepts_bare_tokens_and_normalizes_them() {
        let Some(parsed) = slots("LINK", Some("  Invoices "), Some("GitHub")).sanitized() else {
            panic!("bare tokens pass the fence");
        };
        assert_eq!(parsed.action, "link");
        assert_eq!(parsed.artifact_noun.as_deref(), Some("invoices"));
        assert_eq!(parsed.site_context.as_deref(), Some("github"));
        // Hyphenated product names are one token.
        let Some(parsed) = slots("button", None, Some("my-vendor")).sanitized() else {
            panic!("hyphenated token passes");
        };
        assert_eq!(parsed.site_context.as_deref(), Some("my-vendor"));
        // Declining a slot is legitimate; blank is the same as absent.
        let Some(parsed) = slots("link", Some("   "), None).sanitized() else {
            panic!("blank slot normalizes");
        };
        assert_eq!(parsed.artifact_noun, None);
        assert_eq!(parsed.site_context, None);
    }

    #[test]
    fn action_must_come_from_the_existing_role_vocabulary() {
        // No parallel taxonomy: the parser cannot invent an action the
        // deterministic path would never have produced.
        for role in crate::intent_resolver::INTENT_ROLES {
            assert!(slots(role, None, None).sanitized().is_some(), "{role}");
        }
        for invented in [
            "download", "open", "search", "apply", "navigate", "", "LINKS",
        ] {
            assert_eq!(
                slots(invented, Some("invoice"), Some("github")).sanitized(),
                None,
                "{invented} is not a role"
            );
        }
    }

    #[test]
    fn stub_parser_declines_so_the_cascade_stays_total() {
        let parser: Arc<dyn IntentParser> = Arc::new(StubIntentParser);
        assert_eq!(
            parse_prompt_bounded(&parser, "pull up what I owe on aws"),
            None
        );
    }

    #[test]
    fn bounded_parse_returns_sanitized_slots() {
        let parser: Arc<dyn IntentParser> = Arc::new(TestDoubleIntentParser::answering(slots(
            "link",
            Some("bill"),
            Some("aws"),
        )));
        let Some(parsed) = parse_prompt_bounded(&parser, "pull up what I owe on aws") else {
            panic!("the double answers");
        };
        assert_eq!(parsed.artifact_noun.as_deref(), Some("bill"));
        assert_eq!(parsed.site_context.as_deref(), Some("aws"));
        // Empty prompts never spend the budget.
        assert_eq!(parse_prompt_bounded(&parser, "   "), None);
    }

    #[test]
    fn hostile_adapter_output_is_rejected_after_the_call() {
        // Sanitization runs on the way back, not on trust at the boundary.
        let parser: Arc<dyn IntentParser> = Arc::new(TestDoubleIntentParser::answering(slots(
            "link",
            None,
            Some("https://evil.example/x"),
        )));
        assert_eq!(parse_prompt_bounded(&parser, "open something"), None);
    }

    #[test]
    fn stalled_adapter_returns_within_the_timeout() {
        // Proves the caller is bounded, not the adapter: the double sleeps
        // well past the budget and the call still returns promptly.
        let parser: Arc<dyn IntentParser> = Arc::new(TestDoubleIntentParser::stalling(
            std::time::Duration::from_millis(PARSER_TIMEOUT_MS * 4),
        ));
        let started = std::time::Instant::now();
        assert_eq!(parse_prompt_bounded(&parser, "open something"), None);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(PARSER_TIMEOUT_MS * 3),
            "bounded wait, took {:?}",
            started.elapsed()
        );
    }
}
