//! Step 5: a direct open completes only when the re-read landing is the
//! intended destination, and a failed batch click is named in the journal.

use clinch_desktop::service::AppService;
use url::Url;

fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

#[test]
fn direct_open_landing_on_the_wrong_page_does_not_complete() {
    let entry = url("https://example.com/");
    // Wrong host for the site slot.
    assert!(!AppService::open_landing_valid(
        "example",
        Some(&entry),
        &url("https://search.other.net/results")
    ));
    // Right site slot, but a deep entry redirected elsewhere on the host.
    let deep = url("https://example.com/billing");
    assert!(!AppService::open_landing_valid(
        "example",
        Some(&deep),
        &url("https://example.com/login")
    ));
    // The intended destination, `www.` folded, still completes.
    assert!(AppService::open_landing_valid(
        "example",
        Some(&entry),
        &url("https://www.example.com/")
    ));
    assert!(AppService::open_landing_valid(
        "example",
        Some(&deep),
        &url("https://example.com/billing/")
    ));
}

#[test]
fn failed_batch_click_line_names_the_click() {
    let line = AppService::batch_click_failure_line(1, 1, "Download");
    assert!(line.contains("click 2 'Download'"), "{line}");
    assert!(line.contains("1 click(s) landed"), "{line}");
}
