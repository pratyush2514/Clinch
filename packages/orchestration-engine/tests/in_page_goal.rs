#![deny(unsafe_code)]
//! In-page follow-up detection: the Muse-style continuation.
//!
//! A prompt that explicitly names the connected portal as its site and
//! carries an artifact noun ("open my profile in reddit" on reddit.com) is
//! a follow-up on the live page, not a new portal open. Proven without a
//! browser: detection is pure prompt + origin.

use orchestration_engine::detect_in_page_goal;

fn origin(url: &str) -> url::Url {
    url::Url::parse(url).expect("test origin parses")
}

#[test]
fn names_connected_portal_with_artifact() {
    let reddit = origin("https://www.reddit.com/");
    // The reported case, plus phrasing variants. Artifact nouns are
    // singular-stemmed by the grammar ("settings" → "setting").
    for (prompt, artifact) in [
        ("open my profile in reddit", "profile"),
        ("open profile for me in the reddit", "profile"),
        ("open settings on reddit", "setting"),
    ] {
        assert_eq!(
            detect_in_page_goal(prompt, Some(&reddit)),
            Some(artifact.to_string()),
            "{prompt}"
        );
    }
}

#[test]
fn rejects_non_follow_ups() {
    let reddit = origin("https://www.reddit.com/");
    let google = origin("https://www.google.com/");
    // Different named site: still a fresh portal open.
    assert_eq!(detect_in_page_goal("open amazon for me", Some(&reddit)), None);
    // Named site is not the connected portal.
    assert_eq!(
        detect_in_page_goal("open my profile in reddit", Some(&google)),
        None
    );
    // No connected portal: nothing to follow up on.
    assert_eq!(detect_in_page_goal("open my profile in reddit", None), None);
    // Artifact on a different site: batch-style complement, not a follow-up.
    assert_eq!(
        detect_in_page_goal("download invoices from github", Some(&reddit)),
        None
    );
    // Empty prompt: no goal.
    assert_eq!(detect_in_page_goal("", Some(&reddit)), None);
}
