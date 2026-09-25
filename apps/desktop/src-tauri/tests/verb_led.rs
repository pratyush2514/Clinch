//! Verb-led routing: the planner-level contract.
//!
//! `detect_verb_led_action` (re-exported from orchestration-engine)
//! claims prompts that START with a closed-vocabulary verb phrase
//! ("log out from reddit") before the funnel/search machinery; the site
//! context is extracted generically (`verb_site_context`) and grounded
//! through the normal ladder — no site list in this layer.
//!
//! The live pursuit and landing arms need a browser and are covered by
//! the macro-engine fake-browser tests (`verb_model_loop`) and the
//! orchestration-engine unit tests (`verb_led`).

use orchestration_engine::{
    VerbKind, detect_verb_led_action, funnel_claims, spec_for_noun, verb_site_context,
};

/// "log out from reddit" is verb-led: the log-out spec is claimed and
/// the remainder names the site context for the ladder.
#[test]
fn log_out_from_site_is_verb_led() {
    let action = detect_verb_led_action("log out from reddit").expect("verb-led prompt detected");
    assert_eq!(action.spec.kind, VerbKind::LogOut);
    assert_eq!(action.site_text, "from reddit");
    assert_eq!(
        verb_site_context(&action.site_text).as_deref(),
        Some("reddit")
    );
}

/// A bare "log out" is verb-led with no site context: the dispatcher
/// acts on the live portal instead of grounding anything.
#[test]
fn bare_log_out_is_verb_led_without_site_context() {
    let action = detect_verb_led_action("log out").expect("verb-led prompt detected");
    assert_eq!(action.spec.kind, VerbKind::LogOut);
    assert!(verb_site_context(&action.site_text).is_none());
}

/// The verb-led detector and the funnel never claim the same prompt:
/// open-led prompts stay funnel territory, verb-led prompts stay out.
#[test]
fn verb_led_and_funnel_do_not_overlap() {
    for prompt in ["log out from reddit", "log out", "sign out of github"] {
        assert!(
            detect_verb_led_action(prompt).is_some(),
            "verb-led: {prompt}"
        );
        assert!(!funnel_claims(prompt), "funnel must not claim: {prompt}");
    }
    for prompt in ["open reddit for me", "open settings on reddit"] {
        assert!(
            detect_verb_led_action(prompt).is_none(),
            "verb-led must not claim: {prompt}"
        );
        assert!(funnel_claims(prompt), "funnel claims: {prompt}");
    }
}

/// The claimed spec is the same log-out spec the noun table resolves:
/// one verb, one vocabulary, one verifier, whichever entry point claimed
/// the prompt.
#[test]
fn verb_led_spec_matches_the_noun_table_spec() {
    let action = detect_verb_led_action("log out from reddit").expect("verb-led detected");
    let from_noun = spec_for_noun("log out").expect("noun maps");
    assert_eq!(action.spec.kind, from_noun.kind);
    assert_eq!(action.spec.kind, VerbKind::LogOut);
}
