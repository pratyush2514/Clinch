//! Already-on-origin fast path: the pure follow-up decision behind
//! `dispatch_adhoc_auto_acquire`. The prompt must name an explicit site +
//! artifact noun with high grammar confidence, and the live browser host
//! must already contain the site token — otherwise the normal resolution
//! ladder runs unchanged.
use orchestration_engine::{follow_up_on_origin, parse_grammar};

fn decide(prompt: &str, host: Option<&str>) -> Option<(String, String)> {
    let grammar = parse_grammar(prompt, None);
    follow_up_on_origin(&grammar, host)
}

#[test]
fn fast_path_fires_when_prompt_names_the_live_origin() {
    let hit = decide("open my profile on reddit", Some("www.reddit.com"));
    assert!(hit.is_some(), "expected the fast path on the named origin");
    let (site, artifact) = hit.expect("fast path fires on the named origin");
    assert_eq!(site, "reddit");
    assert_eq!(artifact, "profile");
}

#[test]
fn fast_path_declines_when_parked_elsewhere() {
    assert_eq!(
        decide("open my profile on reddit", Some("www.google.com")),
        None
    );
}

#[test]
fn fast_path_declines_without_a_live_page() {
    assert_eq!(decide("open my profile on reddit", None), None);
}

#[test]
fn fast_path_declines_without_an_artifact_noun() {
    // A pure direct open ("open reddit for me") has no in-page goal.
    assert_eq!(decide("open reddit for me", Some("www.reddit.com")), None);
}

#[test]
fn fast_path_declines_without_a_site() {
    // A bare artifact ("open settings") never skips the ladder.
    assert_eq!(decide("open settings", Some("www.reddit.com")), None);
}

#[test]
fn fast_path_declines_low_confidence_prompts() {
    // Vague prompts stay on the ladder even on a matching host.
    assert_eq!(decide("do the thing", Some("www.reddit.com")), None);
}

#[test]
fn fast_path_host_match_is_case_insensitive() {
    assert!(decide("open my profile on reddit", Some("WWW.REDDIT.COM")).is_some());
}
