//! Integration tests for `orchestration_engine::intent_parser`.
//!
//! Moved out of `src/intent_parser.rs` so the main source stays test-free.

use orchestration_engine::intent_parser::*;
use std::sync::Arc;

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
    for role in orchestration_engine::intent_resolver::INTENT_ROLES {
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
