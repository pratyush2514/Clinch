//! Settle honesty for search-shaped landings: a follow that remains on a
//! search page must never settle COMPLETED — it is a miss, not a landing.
//! Pure regression over `orchestration_engine::still_on_search_page`.
//!
//! The engine's check derives the search shape from the entry URL the
//! dispatcher itself navigated to (same site, same path, a shared query
//! key): no production search engine is named here, so the domain is a
//! generic fixture — `search.example` — that can never be mistaken for a
//! real destination.

use orchestration_engine::still_on_search_page;

fn url(text: &str) -> url::Url {
    url::Url::parse(text).unwrap()
}

#[test]
fn remaining_on_the_search_page_is_always_a_miss_condition() {
    // Same search-page shape on the entry's own site: the results page with
    // a scrolled page parameter is still the search page, regardless of
    // which prompt or lane produced the entry URL.
    let entry = url("https://search.example/results?q=reddit+logout");
    let paged = url("https://search.example/results?q=reddit+logout&start=10");
    assert!(
        still_on_search_page(&entry, &paged),
        "a follow that never left the results page is a miss, never a landing"
    );
    // Query rewritten by the page itself is the same shape.
    let rewritten = url("https://search.example/results?q=reddit");
    assert!(
        still_on_search_page(&entry, &rewritten),
        "the shared query key keeps it the same search page"
    );
}

#[test]
fn leaving_the_search_page_is_not_a_search_miss() {
    let entry = url("https://search.example/results?q=reddit+logout");
    // The follow reached a real destination on another origin: not the
    // search page, however search-shaped its path looks.
    let destination = url("https://www.reddit.com/");
    assert!(
        !still_on_search_page(&entry, &destination),
        "an off-origin landing is not remaining on the search page"
    );
    // Same origin, different path: a different page, not the results page.
    let other_path = url("https://search.example/about?q=reddit+logout");
    assert!(
        !still_on_search_page(&entry, &other_path),
        "a different path on the same origin is not the search page"
    );
    // Same origin and path but no shared query key: not the results page.
    let no_query = url("https://search.example/results");
    assert!(
        !still_on_search_page(&entry, &no_query),
        "a bare search path with no query is not a results page"
    );
}
