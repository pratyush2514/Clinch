//! `www.` alias folding in portal confinement: a grounded bare domain and a
//! live `www.` page are the same site. Regression test for the live failure
//! where the grounder returned bare `reddit.com` while the tab sat on
//! `https://www.reddit.com/`: strict `Url::origin()` equality marked every
//! snapshot as drifted, so `ax_snapshot` returned an empty vec and the
//! account-home worker, the auth probe, and the model fallback all went
//! blind at once — "no identity control found in the header chrome" on a
//! page whose header plainly rendered the avatar button.

use browser_driver::same_site_origin;
use url::Url;

fn url(s: &str) -> Url {
    match Url::parse(s) {
        Ok(url) => url,
        Err(error) => panic!("test URL parses: {s}: {error}"),
    }
}

#[test]
fn www_prefix_folds_both_directions() {
    assert!(same_site_origin(
        &url("https://www.reddit.com/"),
        &url("https://reddit.com/")
    ));
    assert!(same_site_origin(
        &url("https://reddit.com/"),
        &url("https://www.reddit.com/")
    ));
}

#[test]
fn www_prefix_folds_with_paths_and_query() {
    assert!(same_site_origin(
        &url("https://www.reddit.com/r/rust/"),
        &url("https://reddit.com/")
    ));
}

#[test]
fn identical_origins_stay_same_site() {
    assert!(same_site_origin(
        &url("https://www.reddit.com/"),
        &url("https://www.reddit.com/")
    ));
    assert!(same_site_origin(
        &url("https://reddit.com/"),
        &url("https://reddit.com/")
    ));
}

#[test]
fn different_hosts_are_not_same_site() {
    assert!(!same_site_origin(
        &url("https://www.reddit.com/"),
        &url("https://www.google.com/")
    ));
    assert!(!same_site_origin(
        &url("https://reddit.com/"),
        &url("https://evil-reddit.com/")
    ));
}

#[test]
fn non_www_subdomains_are_not_folded() {
    // Only the `www.` alias folds: `mail.` is a different site.
    assert!(!same_site_origin(
        &url("https://mail.reddit.com/"),
        &url("https://reddit.com/")
    ));
    assert!(!same_site_origin(
        &url("https://old.reddit.com/"),
        &url("https://www.reddit.com/")
    ));
}

#[test]
fn scheme_and_port_still_compare_strictly() {
    assert!(!same_site_origin(
        &url("http://www.reddit.com/"),
        &url("https://reddit.com/")
    ));
    assert!(!same_site_origin(
        &url("https://www.reddit.com:8443/"),
        &url("https://reddit.com/")
    ));
}

#[test]
fn host_case_is_irrelevant() {
    assert!(same_site_origin(
        &url("https://WWW.REDDIT.COM/"),
        &url("https://reddit.com/")
    ));
}
