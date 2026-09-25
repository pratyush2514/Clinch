//! Settings goal wiring (work item B2): the planner-level contract.
//!
//! `goal_class_for("settings")` routes to the new `GoalClass::Settings`
//! branch in the in-page dispatcher (both the funnel's canonical noun and
//! the follow-up detector's stemmed noun land there); the funnel
//! decisions for the screenshot prompts are unchanged, and
//! "open my profile on reddit" still routes to `AccountHome`.
//!
//! The live pursuit and memory-write arms need a browser and are covered
//! by the macro-engine fake-browser tests, the playbook-store settings
//! tests, and the service's remembered-href validation unit tests.

use orchestration_engine::{FunnelDecision, GoalClass, ObjectClass, funnel_plan, goal_class_for};

/// The funnel's canonical object noun and the stemmed noun the
/// follow-up detector hands the dispatcher both hit the settings
/// branch — "settings" is what `dispatch_in_page_goal` receives from
/// either entry point.
#[test]
fn settings_nouns_route_to_the_settings_goal_branch() {
    for noun in ["setting", "settings", "preference", "preferences"] {
        assert_eq!(
            goal_class_for(noun),
            Some(GoalClass::Settings),
            "noun: {noun}"
        );
    }
    assert_eq!(GoalClass::Settings.as_str(), "settings");
}

/// The profile branch is untouched: identity nouns still route to
/// `AccountHome`.
#[test]
fn profile_nouns_still_route_to_account_home() {
    for noun in ["profile", "account"] {
        assert_eq!(
            goal_class_for(noun),
            Some(GoalClass::AccountHome),
            "noun: {noun}"
        );
    }
}

#[test]
fn open_settings_on_reddit_stays_already_on_origin() {
    // The funnel decision is unchanged: site `reddit` matches the live
    // portal, so the object is pursued in-page. The dispatcher now maps
    // the canonical "settings" noun onto `GoalClass::Settings`.
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
        goal_class_for("settings"),
        Some(GoalClass::Settings),
        "the funnel's canonical noun hits the settings goal branch"
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
    // goal-class mapping.
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
    assert_eq!(goal_class_for("profile"), Some(GoalClass::AccountHome));
}
