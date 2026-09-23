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
        // Tier 5 template host passes; credentials and non-https fail even
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
}
