//! Integration tests for `orchestration_engine::url_policy`.
//!
//! Moved out of `src/url_policy.rs` so the main source stays test-free.

use orchestration_engine::RouteSource;
use orchestration_engine::url_policy::*;

#[test]
fn url_policy_rejects_lookalike_and_non_https() {
    assert!(validate_proposed_url("https://github.com.evil.com/x").is_err());
    assert!(validate_proposed_url("http://github.com/x").is_err());
    assert!(validate_proposed_url("https://user:pass@github.com/").is_err());
    assert!(validate_proposed_url("javascript:alert(1)").is_err());
    assert!(validate_proposed_url("data:text/html,hi").is_err());
    assert!(validate_proposed_url("not a url at all").is_err());
    assert!(validate_proposed_url("").is_err());
    assert!(validate_proposed_url("https://github.com/settings/billing").is_ok());
}

#[test]
fn url_policy_allows_google_hosts_but_rejects_their_abuse() {
    // Google hosts stay allowlisted for adapter-proposed URLs;
    // credentials and non-https fail even on those hosts, and
    // lookalikes never pass.
    assert!(validate_proposed_url("https://www.google.com/search?q=open+amazon+for+me").is_ok());
    assert!(validate_proposed_url("https://google.com/search?q=hi").is_ok());
    assert!(validate_proposed_url("https://user:pass@www.google.com/search?q=hi").is_err());
    assert!(validate_proposed_url("http://www.google.com/search?q=hi").is_err());
    assert!(validate_proposed_url("https://www.google.com.evil.com/search?q=hi").is_err());
}

#[test]
fn user_directed_validation_allows_any_https_host_but_keeps_structure() {
    // User-directed targets skip the host allowlist (the user named the
    // destination) but keep every structural check.
    assert!(validate_user_directed_url("https://www.amazon.in/").is_ok());
    assert!(validate_user_directed_url("https://github.com/settings/billing").is_ok());
    assert!(validate_user_directed_url("http://amazon.in/").is_err());
    assert!(validate_user_directed_url("https://user:pass@amazon.in/").is_err());
    assert!(validate_user_directed_url("javascript:alert(1)").is_err());
    assert!(validate_user_directed_url("not a url").is_err());
    assert!(validate_user_directed_url("").is_err());
    // The machine-proposed gate still refuses what the user never named.
    assert!(validate_proposed_url("https://www.amazon.in/").is_err());
}

#[test]
fn entry_url_valid_treats_domain_grounded_as_user_directed() {
    // Regression: a grounder hit for amazon.in must take the
    // user-directed bar, not the machine-proposed host allowlist (which
    // only knows github.com and google.com). Routing it through the
    // allowlist made every grounded direct open fail dispatch with
    // "The derived intent is not runnable."
    assert!(entry_url_valid(
        Some(RouteSource::DomainGrounded),
        "https://www.amazon.in/"
    ));
    assert!(entry_url_valid(
        Some(RouteSource::ExplicitDomain),
        "https://www.amazon.in/"
    ));
    assert!(entry_url_valid(
        Some(RouteSource::Shortcut),
        "https://www.amazon.in/"
    ));
    assert!(entry_url_valid(
        Some(RouteSource::SiteSearch),
        "https://www.amazon.in/"
    ));
    // Machine-proposed tiers keep the allowlist.
    assert!(!entry_url_valid(
        Some(RouteSource::LlmFallback),
        "https://www.amazon.in/"
    ));
    assert!(entry_url_valid(
        Some(RouteSource::LlmFallback),
        "https://www.google.com/search?q=open+amazon"
    ));
    // The structural bar still applies to user-directed URLs.
    assert!(!entry_url_valid(
        Some(RouteSource::DomainGrounded),
        "http://www.amazon.in/"
    ));
    assert!(!entry_url_valid(
        Some(RouteSource::DomainGrounded),
        "https://user:pass@www.amazon.in/"
    ));
}
