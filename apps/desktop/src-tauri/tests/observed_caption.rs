//! Observed-state captions: pure functions of what the live page showed,
//! never of what the run intended.

use clinch_desktop::service::{
    ObservedState, ObservedVerdict, auth_unknown_label, caption_observed_state,
};

#[test]
fn completed_caption_carries_observed_url_and_title() {
    let caption = caption_observed_state(
        ObservedVerdict::Completed,
        &ObservedState {
            final_url: Some("https://www.reddit.com/settings/account"),
            page_title: Some("Reddit  Settings"),
            acted_on: Some("Settings"),
            note: Some("verifier: settings surface"),
        },
    );
    assert!(caption.contains("completed"));
    assert!(caption.contains("www.reddit.com/settings/account"));
    assert!(caption.contains("'Reddit Settings'"));
    assert!(caption.contains("acted on 'Settings'"));
    assert!(caption.contains("verifier: settings surface"));
}

#[test]
fn caption_never_carries_intended_values_or_query_strings() {
    let intended = "https://intended.example/profile";
    let caption = caption_observed_state(
        ObservedVerdict::Completed,
        &ObservedState {
            final_url: Some("https://landed.example/login?token=secret#frag"),
            page_title: Some("Sign in"),
            ..ObservedState::default()
        },
    );
    assert!(!caption.contains(intended));
    assert!(!caption.contains("intended.example"));
    assert!(caption.contains("landed.example/login"));
    assert!(!caption.contains("secret"));
    assert!(!caption.contains("frag"));
}

#[test]
fn failure_caption_carries_observed_state_and_reason() {
    let caption = caption_observed_state(
        ObservedVerdict::Failed,
        &ObservedState {
            final_url: Some("https://example.com/search"),
            page_title: Some("Search results"),
            acted_on: None,
            note: Some("navigation not verified"),
        },
    );
    assert!(caption.contains("failed"));
    assert!(caption.contains("example.com/search"));
    assert!(caption.contains("'Search results'"));
    assert!(caption.contains("navigation not verified"));
}

#[test]
fn unread_page_is_stated_not_invented() {
    let caption = caption_observed_state(ObservedVerdict::Failed, &ObservedState::default());
    assert!(caption.contains("landed unread"));
    assert!(caption.contains("title unread"));
    let blank_title = caption_observed_state(
        ObservedVerdict::Completed,
        &ObservedState {
            final_url: Some("about:blank"),
            page_title: Some("   "),
            ..ObservedState::default()
        },
    );
    assert!(blank_title.contains("title unread"));
}

#[test]
fn caption_is_deterministic() {
    let observed = ObservedState {
        final_url: Some("https://a.example/x"),
        page_title: Some("A"),
        acted_on: Some("B"),
        note: Some("C"),
    };
    assert_eq!(
        caption_observed_state(ObservedVerdict::Completed, &observed),
        caption_observed_state(ObservedVerdict::Completed, &observed)
    );
}

#[test]
fn unknown_auth_label_does_not_claim_probe_failure_when_page_was_read() {
    let read = auth_unknown_label(Some("https://a.example/"));
    assert!(read.contains("page read"));
    assert!(!read.contains("probe failed"));
    assert!(auth_unknown_label(None).contains("no readable page"));
}
