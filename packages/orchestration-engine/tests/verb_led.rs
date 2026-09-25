//! Verb-led action detection (`detect_verb_led_action`) and site-context
//! extraction (`verb_site_context`): prompts that START with a verb
//! phrase from a spec's closed vocabulary route before the
//! funnel/search machinery, with the site context grounded through the
//! normal ladder — no site list anywhere in this layer.

use orchestration_engine::{VerbKind, detect_verb_led_action, verb_site_context};

/// "log out from reddit": the verb-led shape from the bug report —
///
/// it must preempt the funnel/search path and name the log-out spec.
#[test]
fn log_out_from_site_is_verb_led() {
    let action = detect_verb_led_action("log out from reddit").expect("verb-led prompt detected");
    assert_eq!(action.spec.kind, VerbKind::LogOut);
    assert_eq!(action.site_text, "from reddit");
}

/// Every vocabulary phrase of the log-out spec is verb-led.
#[test]
fn all_logout_phrases_are_verb_led() {
    for prompt in ["log out", "log off", "sign out", "Sign Out"] {
        let action = detect_verb_led_action(prompt).unwrap_or_else(|| panic!("detected: {prompt}"));
        assert_eq!(action.spec.kind, VerbKind::LogOut, "prompt: {prompt}");
        assert!(action.site_text.is_empty(), "prompt: {prompt}");
    }
}

/// Whitespace and case never matter; the remainder is the verbatim site
/// context.
#[test]
fn verb_led_detection_is_case_and_space_insensitive() {
    let action =
        detect_verb_led_action("  LOG   OUT   of  GitHub  ").expect("verb-led prompt detected");
    assert_eq!(action.spec.kind, VerbKind::LogOut);
    // The detector collapses internal whitespace before matching.
    assert_eq!(action.site_text, "of github");
}

/// A verb phrase must start the prompt and end on a word boundary:
/// "logout" (one word) and "log outerwear" are not "log out".
#[test]
fn verb_phrase_needs_a_word_boundary() {
    assert!(
        detect_verb_led_action("logout").is_none(),
        "one-word 'logout' is not verb-led"
    );
    assert!(
        detect_verb_led_action("log outerwear").is_none(),
        "a longer word sharing the prefix is not verb-led"
    );
    assert!(
        detect_verb_led_action("catalog out the issue").is_none(),
        "the phrase must start the prompt"
    );
}

/// Verb-led detection is log-out only: settings/account nouns keep
/// their noun-led dispatch and are never preempted here, even when they
/// start the prompt.
#[test]
fn non_logout_specs_are_not_verb_led() {
    for prompt in [
        "settings on reddit",
        "settings",
        "profile on reddit",
        "open settings on reddit",
    ] {
        assert!(
            detect_verb_led_action(prompt).is_none(),
            "not verb-led: {prompt}"
        );
    }
}

/// Open-led prompts belong to the funnel, not the verb-led path — the
/// two detectors never claim the same prompt.
#[test]
fn open_led_prompts_are_not_verb_led() {
    for prompt in [
        "open reddit for me",
        "open settings on reddit",
        "open my profile on reddit",
    ] {
        assert!(
            detect_verb_led_action(prompt).is_none(),
            "funnel territory: {prompt}"
        );
    }
}

/// One leading preposition is stripped so the ladder grounds the site
/// name, not the phrase.
#[test]
fn site_context_strips_one_leading_preposition() {
    for (remainder, expected) in [
        ("from reddit", "reddit"),
        ("of github", "github"),
        ("on x", "x"),
        ("in slack", "slack"),
        ("reddit", "reddit"),
    ] {
        assert_eq!(
            verb_site_context(remainder).as_deref(),
            Some(expected),
            "remainder: {remainder}"
        );
    }
}

/// A bare verb or an aside-only remainder names no site: the dispatcher
/// acts on the live portal instead of grounding "me".
#[test]
fn site_context_is_none_without_a_site() {
    for remainder in ["", "   ", "for me", "me", "please"] {
        assert!(
            verb_site_context(remainder).is_none(),
            "no site in: '{remainder}'"
        );
    }
}
