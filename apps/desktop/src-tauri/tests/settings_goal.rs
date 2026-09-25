//! Settings goal wiring: the planner-level contract.
//!
//! `spec_for_noun("settings")` resolves to the settings verb spec (both
//! the funnel's canonical noun and the follow-up detector's stemmed noun
//! land there); "log out" phrases resolve to the log-out spec; the
//! funnel decisions for the screenshot prompts are unchanged, and "open
//! my profile on reddit" still routes to the account-home spec.
//!
//! The live pursuit and memory-write arms need a browser and are covered
//! by the macro-engine fake-browser tests, the playbook-store settings
//! tests, and the service's remembered-href validation unit tests.

use orchestration_engine::{FunnelDecision, ObjectClass, VerbKind, funnel_plan, spec_for_noun};

fn spec_kind(noun: &str) -> Option<VerbKind> {
    spec_for_noun(noun).map(|spec| spec.kind)
}

/// The funnel's canonical object noun and the stemmed noun the
/// follow-up detector hands the dispatcher both hit the settings spec
/// — "settings" is what `dispatch_in_page_goal` receives from either
/// entry point.
#[test]
fn settings_nouns_route_to_the_settings_spec() {
    for noun in ["setting", "settings", "preference", "preferences"] {
        assert_eq!(spec_kind(noun), Some(VerbKind::Settings), "noun: {noun}");
    }
    assert_eq!(VerbKind::Settings.as_str(), "settings");
}

/// The profile branch is untouched: identity nouns still route to the
/// account-home spec.
#[test]
fn profile_nouns_still_route_to_account_home() {
    for noun in ["profile", "account"] {
        assert_eq!(spec_kind(noun), Some(VerbKind::AccountHome), "noun: {noun}");
    }
}

/// The log-out branch: verb phrases route to the log-out spec.
#[test]
fn logout_phrases_route_to_the_logout_spec() {
    for noun in ["log out", "log off", "sign out", "Sign Out"] {
        assert_eq!(spec_kind(noun), Some(VerbKind::LogOut), "noun: {noun}");
    }
    assert_eq!(VerbKind::LogOut.as_str(), "log_out");
}

#[test]
fn open_settings_on_reddit_stays_already_on_origin() {
    // The funnel decision is unchanged: site `reddit` matches the live
    // portal, so the object is pursued in-page. The dispatcher now maps
    // the canonical "settings" noun onto the settings verb spec.
    let plan = funnel_plan("open settings on reddit", false, Some("www.reddit.com"));
    assert!(
        matches!(
            &plan.decision,
            FunnelDecision::AlreadyOnOrigin { site, object: Some(ObjectClass::Settings) }
            if site == "reddit"
        ),
        "got: {:?}",
        plan.decision
    );
    assert_eq!(
        spec_kind("settings"),
        Some(VerbKind::Settings),
        "the funnel's canonical noun hits the settings verb spec"
    );
}

#[test]
fn open_settings_without_a_site_still_implies_the_portal() {
    // "open settings" on a live portal: implicit-site arm, settings
    // object — the object-noun exclusion arm, unchanged.
    let plan = funnel_plan("open settings", false, Some("www.reddit.com"));
    assert_eq!(
        plan.decision,
        FunnelDecision::ImplicitSite {
            object: ObjectClass::Settings
        }
    );
}

#[test]
fn open_my_profile_on_reddit_is_unchanged() {
    // The account-home lane keeps its exact funnel decision and its
    // spec mapping.
    let plan = funnel_plan("open my profile on reddit", false, Some("www.reddit.com"));
    assert!(
        matches!(
            &plan.decision,
            FunnelDecision::AlreadyOnOrigin { site, object: Some(ObjectClass::AccountHome) }
            if site == "reddit"
        ),
        "got: {:?}",
        plan.decision
    );
    assert_eq!(spec_kind("profile"), Some(VerbKind::AccountHome));
}
