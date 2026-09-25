#![deny(unsafe_code)]
//! `still_on_search_page`: the search-follow lane's goal check.
//!
//! A Stage-2 follow that never left the results page is a miss, not a
//! landing. The predicate derives the search shape from the entry URL the
//! dispatcher itself navigated to — same origin, same path, a shared query
//! key — so no search-engine names are hardcoded and no host allowlist is
//! consulted. Genuinely navigated landings (different origin, different
//! path, query-less) must read as navigated.

use orchestration_engine::still_on_search_page;

fn url(text: &str) -> url::Url {
    let Ok(parsed) = url::Url::parse(text) else {
        panic!("fixture URL parses: {text}");
    };
    parsed
}

/// The grounded search template the dispatcher navigates to.
const ENTRY: &str = "https://www.google.com/search?q=want+you";

#[test]
fn identical_and_paginated_results_pages_are_still_search() {
    let entry = url(ENTRY);
    assert!(still_on_search_page(&entry, &url(ENTRY)));
    // Pagination keeps the template: same path, same `q` key.
    assert!(still_on_search_page(
        &entry,
        &url("https://www.google.com/search?q=want+you&start=10")
    ));
    // The query value may change; the shared key is the shape.
    assert!(still_on_search_page(
        &url("https://www.google.com/search?q=a"),
        &url("https://www.google.com/search?q=b")
    ));
    // Extra parameters on either side do not change the shape.
    assert!(still_on_search_page(
        &url("https://www.google.com/search?q=a&udm=14"),
        &url("https://www.google.com/search?q=a")
    ));
}

#[test]
fn www_folding_matches_the_origin_policy() {
    // `same_site_origin` folds a leading `www.`: a bare-host twin of the
    // results page is the same search page.
    assert!(still_on_search_page(
        &url(ENTRY),
        &url("https://google.com/search?q=want+you")
    ));
}

#[test]
fn genuinely_navigated_landings_are_not_search() {
    let entry = url(ENTRY);
    // A real destination on another host.
    assert!(!still_on_search_page(
        &entry,
        &url("https://music.example/song")
    ));
    // Same host, different page.
    assert!(!still_on_search_page(
        &entry,
        &url("https://www.google.com/")
    ));
    assert!(!still_on_search_page(
        &entry,
        &url("https://www.google.com/maps?q=want+you")
    ));
    // The search path with no query is not a results page.
    assert!(!still_on_search_page(
        &entry,
        &url("https://www.google.com/search")
    ));
    // Same path but no shared query key: a different request shape.
    assert!(!still_on_search_page(
        &entry,
        &url("https://www.google.com/search?hl=en")
    ));
    // A search-shaped URL on another origin is out of scope: the check
    // never guesses across hosts.
    assert!(!still_on_search_page(
        &entry,
        &url("https://www.youtube.com/results?search_query=want+you")
    ));
    // Scheme is strict: http is not the https search page.
    assert!(!still_on_search_page(
        &entry,
        &url("http://www.google.com/search?q=want+you")
    ));
}
