//! Funnel steps 1–2: aside stripping and slot splitting. Regression fixtures
//! for the live failures where asides polluted the grammar slots
//! (`"i want you to open X for me"` → target `"you"`, `"…make sure ill do
//! the login first"` → target `"login"`).
use orchestration_engine::{
    AsideInfo, FunnelSlots, ObjectClass, object_noun, split_slots, strip_asides,
};

fn slots(prompt: &str) -> FunnelSlots {
    split_slots(prompt)
}

#[test]
fn settings_on_reddit_splits_site_and_object() {
    let got = slots("open settings on reddit");
    assert_eq!(got.site_slot.as_deref(), Some("reddit"));
    assert_eq!(got.object_slot, Some(ObjectClass::Settings));
    assert!(!got.login_hint);
}

#[test]
fn settings_on_reddit_with_articles_splits_site_and_object() {
    let got = slots("open the Settings on the reddit");
    assert_eq!(got.site_slot.as_deref(), Some("reddit"));
    assert_eq!(got.object_slot, Some(ObjectClass::Settings));
}

#[test]
fn login_clause_is_one_aside_and_sets_login_hint() {
    let got = slots("can you open the X for me and make sure ill do the login first");
    assert_eq!(got.site_slot.as_deref(), Some("x"));
    assert_eq!(got.object_slot, None);
    for expected in ["can you", "for me", "make sure ill do the login first"] {
        assert!(
            got.asides.iter().any(|aside| aside == expected),
            "expected aside {expected:?} in {:?}",
            got.asides
        );
    }
    assert!(got.login_hint, "login clause must set the login hint");
}

#[test]
fn framing_aside_does_not_donate_you_as_site() {
    let got = slots("i want you to open X for me");
    assert_eq!(got.site_slot.as_deref(), Some("x"));
    assert_eq!(got.object_slot, None);
    assert!(
        got.asides.iter().any(|aside| aside == "i want you to"),
        "expected the framing aside in {:?}",
        got.asides
    );
}

#[test]
fn profile_on_reddit_is_account_home_not_a_regression() {
    let got = slots("open my profile on reddit");
    assert_eq!(got.site_slot.as_deref(), Some("reddit"));
    assert_eq!(got.object_slot, Some(ObjectClass::AccountHome));
}

#[test]
fn you_is_never_a_site_slot() {
    for prompt in ["open for you", "can you open it for you"] {
        let got = slots(prompt);
        assert_ne!(
            got.site_slot.as_deref(),
            Some("you"),
            "pronoun must never be a site slot for {prompt:?}"
        );
    }
}

#[test]
fn one_char_recovery_is_whole_token_only() {
    assert_eq!(slots("open x for me").site_slot.as_deref(), Some("x"));
    let boxed = slots("open the box for me").site_slot;
    assert_ne!(
        boxed.as_deref(),
        Some("x"),
        "substring recovery must not fire inside {boxed:?}"
    );
}

#[test]
fn aside_variants_still_ground_the_site() {
    let got = slots("Could you please open reddit for me");
    assert_eq!(got.site_slot.as_deref(), Some("reddit"));
    for expected in ["could you", "please", "for me"] {
        assert!(
            got.asides.iter().any(|aside| aside == expected),
            "expected aside {expected:?} in {:?}",
            got.asides
        );
    }
}

#[test]
fn object_noun_matches_canonical_in_page_vocabulary() {
    assert_eq!(object_noun(ObjectClass::AccountHome), "profile");
    assert_eq!(object_noun(ObjectClass::Settings), "settings");
    assert_eq!(object_noun(ObjectClass::Notifications), "notifications");
    assert_eq!(object_noun(ObjectClass::Messages), "messages");
}

#[test]
fn object_slot_fires_after_the_site_too() {
    let got = slots("open reddit's settings");
    assert_eq!(got.object_slot, Some(ObjectClass::Settings));
}

#[test]
fn strip_asides_reports_cleaned_asides_and_hint() {
    let AsideInfo {
        cleaned,
        asides,
        login_hint,
    } = strip_asides("Please open my notifications on github for me");
    assert_eq!(cleaned, "open my notifications on github");
    assert_eq!(asides, vec!["please", "for me"]);
    assert!(!login_hint);

    let hinted = strip_asides("open x for me and make sure you will sign in first");
    assert!(hinted.login_hint);
    assert_eq!(hinted.cleaned, "open x and");
}

#[test]
fn contracted_framing_variant_is_stripped() {
    let got = slots("I'd like you to open X for me");
    assert_eq!(got.site_slot.as_deref(), Some("x"));
    assert!(
        got.asides.iter().any(|aside| aside == "i'd like you to"),
        "expected the contracted framing aside in {:?}",
        got.asides
    );
}
