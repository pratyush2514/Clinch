#![deny(unsafe_code)]
//! Allowlist validation for machine-proposed navigation targets. Every
//! resolver tier — account entities, the grounded search template, even a
//! future LLM adapter — funnels through [`validate_proposed_url`], so a
//! compromised directory response or an untrusted model can never yield a
//! navigable URL outside the contract below.
//!
//! Scope note: this guards *proposed* URLs only. A Stage-2 destination is
//! observed from a real click on a live search result, not proposed, so it
//! re-anchors confinement instead of passing through here.

use super::route_proposer::RouteSource;

/// Hosts navigation may target. Exact matches only.
/// `www.google.com` / `google.com` back the grounded search tier
/// (`/search?q=…` template, never guessed TLDs); the entity tier resolves
/// solely to portal hosts.
const ALLOWED_HOSTS: &[&str] = &["github.com", "www.google.com", "google.com"];

/// Why a proposed URL was rejected. Variants stay coarse on purpose: the
/// caller only needs fail-closed, and messages never echo the URL (which
/// may carry tokens in query strings).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UrlRejected {
    #[error("proposed URL is not parseable")]
    Unparseable,
    #[error("proposed URL must use https")]
    Scheme,
    #[error("proposed URL host is not allowlisted")]
    Host,
    #[error("proposed URL carries credentials")]
    Credentials,
}

/// Validate a proposed navigation target: absolute `https` URL on an
/// exactly-allowlisted host with no embedded credentials. Query strings
/// and fragments pass through (routes may need them); `javascript:`,
/// `data:`, and `blob:` schemes fail on the scheme check, and lookalike
/// hosts (`github.com.evil.com`) fail the exact host match.
///
/// # Errors
/// Returns [`UrlRejected`] for unparseable, non-`https`, off-allowlist,
/// or credential-carrying URLs.
pub fn validate_proposed_url(url: &str) -> Result<url::Url, UrlRejected> {
    let parsed = url::Url::parse(url).map_err(|_| UrlRejected::Unparseable)?;
    if parsed.scheme() != "https" {
        return Err(UrlRejected::Scheme);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(UrlRejected::Credentials);
    }
    if !ALLOWED_HOSTS.contains(&parsed.host_str().unwrap_or("")) {
        return Err(UrlRejected::Host);
    }
    Ok(parsed)
}

/// Validate a user-directed navigation target: the user named the
/// destination themselves (a typed domain, a saved site shortcut) or named
/// the site a directory resolved. Absolute `https`, no embedded
/// credentials — the same structural bar as [`validate_proposed_url`], but
/// with no host allowlist.
///
/// The allowlist guards *machine-proposed* URLs (entity tier, LLM tier,
/// search template) against a compromised proposer inventing destinations.
/// Here the user is the authority for where they asked to go: refusing
/// `https://amazon.in` because no tier predicted it would make direct opens
/// impossible by construction. Approval gates still guard submits and
/// downloads after navigation, and portal confinement re-anchors to the
/// landed origin instead of trusting a predicted one.
///
/// # Errors
/// Returns [`UrlRejected`] for unparseable, non-`https`, or
/// credential-carrying URLs.
pub fn validate_user_directed_url(url: &str) -> Result<url::Url, UrlRejected> {
    let parsed = url::Url::parse(url).map_err(|_| UrlRejected::Unparseable)?;
    if parsed.scheme() != "https" {
        return Err(UrlRejected::Scheme);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(UrlRejected::Credentials);
    }
    if parsed.host_str().is_none() {
        return Err(UrlRejected::Unparseable);
    }
    Ok(parsed)
}

/// Whether the landed URL is still on the search-results page the
/// dispatcher itself navigated to.
///
/// Derived entirely from the entry URL — same origin (with the usual
/// `www.` folding), the entry's own search path, and a shared query key —
/// so there is no search-engine allowlist and no hardcoded host names or
/// paths. The template's shape comes from the URL the dispatcher
/// navigated to, which today is always the grounded `?q=` template, but
/// the check assumes nothing about that.
///
/// A Stage-2 follow that never left the results page is a miss, not a
/// landing: without this check a pure direct open "completes" its zero
/// steps on the results page, claiming a destination the run never
/// reached.
#[must_use]
pub fn still_on_search_page(entry_url: &url::Url, landed_url: &url::Url) -> bool {
    // Same site, with the usual `www.` folding: a results page on another
    // host is a different page, however search-shaped its URL looks.
    if !browser_driver::same_site_origin(entry_url, landed_url) {
        return false;
    }
    // The search path is the template's identity, taken from the entry
    // URL itself rather than a hardcoded "/search".
    if entry_url.path() != landed_url.path() {
        return false;
    }
    // The query shape: both carry a query sharing at least one key (the
    // template's parameter, e.g. `q`). A bare search path with no query
    // is not a results page.
    entry_url.query_pairs().any(|(key, _)| {
        landed_url
            .query_pairs()
            .any(|(landed_key, _)| landed_key == key)
    })
}

/// Validation bar for a proposed entry URL, by route provenance.
/// User-directed destinations — a typed domain, a saved shortcut, a site
/// the directory resolved, a site the domain grounder resolved — were named
/// by the user; the model or directory only resolved the name. Structural
/// validation (absolute `https`, no embedded credentials) suffices there.
/// Everything else keeps the host allowlist.
///
/// Refusing `https://amazon.in` here because no tier predicted it would
/// make direct opens impossible by construction: the grounder exists
/// precisely to resolve names no table knows.
#[must_use]
pub fn entry_url_valid(source: Option<RouteSource>, entry_url: &str) -> bool {
    match source {
        Some(
            RouteSource::ExplicitDomain
            | RouteSource::Shortcut
            | RouteSource::SiteSearch
            | RouteSource::DomainGrounded,
        ) => validate_user_directed_url(entry_url).is_ok(),
        _ => validate_proposed_url(entry_url).is_ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn url_policy_allows_grounded_search_but_rejects_its_abuse() {
        // Tier 4 template host passes; credentials and non-https fail even
        // on the search host, and lookalikes never pass.
        assert!(
            validate_proposed_url("https://www.google.com/search?q=open+amazon+for+me").is_ok()
        );
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
            Some(RouteSource::SearchFallback),
            "https://www.amazon.in/"
        ));
        assert!(entry_url_valid(
            Some(RouteSource::SearchFallback),
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
}
