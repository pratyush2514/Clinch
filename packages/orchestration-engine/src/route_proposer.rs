#![deny(unsafe_code)]
//! Tiered cold-path entry resolution: attach a navigable `entry_url` to
//! ad-hoc intents that have none, before dispatch reaches the macro engine.
//!
//! Tier order, first hit wins (tier 1 — the saved-playbook lookup — is the
//! caller's pre-step: this runs only when `resolve_command` already yielded
//! an ephemeral intent, so nothing here reimplements matching):
//!
//! 1. Saved playbook — decided upstream, never re-run here.
//! 2. Connected-account entity (`entity_resolver`), only with a directory.
//! 3. Curated portal route table (`portal_routes`).
//! 4. LLM fallback, only with a configured adapter — and its output is
//!    untrusted input, validated like every other tier.
//! 5. Grounded search fallback — fixed `https://www.google.com/search?q=…`
//!    template over the raw prompt. Never guesses TLDs; the dispatcher
//!    navigates to the search page and grounds the top result link from the
//!    live AX tree.
//!
//! Every tier's output passes through `url_policy` validation, including
//! the route table itself. Any validation failure returns `None`
//! immediately — a corrupt tier never falls through to a weaker one. The
//! search tier only runs when no stronger tier proposed anything; an
//! invalid stronger proposal still fails closed without falling through.

use crate::{
    entity_resolver::{AccountDirectory, resolve_repo_entity},
    portal_routes::{PORTALS, portal_route},
    url_policy::validate_proposed_url,
};

/// Where a resolved route came from. Recorded on the resolution and logged
/// with the navigation proposal so wrong sources are debuggable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteSource {
    AccountEntity,
    PortalRouteTable,
    LlmFallback,
    SearchFallback,
}

/// Fixed grounded search entry: the only dynamic URL the resolver ever
/// invents, and it invents no host — always `www.google.com/search`.
/// The raw prompt becomes the `q` value; downstream grounding clicks a
/// real AX result link, never a guessed TLD.
const SEARCH_BASE: &str = "https://www.google.com/search";

/// Build the grounded search-fallback URL for an ad-hoc prompt.
/// `None` for empty prompts (no query to ground), so callers keep the
/// `route_resolution_miss` dead-end instead of navigating to an empty
/// search. Never derives hosts from prompt words — the host is fixed.
#[must_use]
pub fn search_fallback_url(prompt: &str) -> Option<String> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        return None;
    }
    // `form_urlencoded` byte-serializes spaces as `+`, matching the
    // `?q=open+amazon+for+me` contract.
    let query: String = url::form_urlencoded::byte_serialize(trimmed.as_bytes()).collect();
    if query.is_empty() {
        return None;
    }
    Some(format!("{SEARCH_BASE}?q={query}"))
}

/// A validated navigation target plus its provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRoute {
    pub url: url::Url,
    pub source: RouteSource,
}

/// Optional resolution inputs. Both tiers are inert when unset, which is
/// the production default: no account directory is wired (no stored GitHub
/// credential exists to back one) and no LLM adapter is configured. The
/// table tier always runs.
pub struct ResolutionContext<'a> {
    pub account_dir: Option<&'a dyn AccountDirectory>,
    pub llm: Option<&'a dyn LlmUrlProposer>,
}

/// Last-resort URL proposer. Synchronous by contract: adapters needing I/O
/// must bound it internally, and whatever comes back is validated as
/// untrusted input — never navigated on good faith.
pub trait LlmUrlProposer: Send + Sync {
    /// Propose a single URL for the prompt, or nothing.
    fn propose_url(&self, prompt: &str) -> Option<String>;
}

/// Detect a portal token from the table's own vocabulary. Exact tokens
/// only — unknown spellings fall through instead of guessing.
/// Public so desktop callers can derive `(portal, intent_class)` from prompt
/// tokens without hardcoding portal strings.
#[must_use]
pub fn detect_portal(prompt: &str) -> Option<&'static str> {
    let tokens = crate::intent_resolver::tokens(prompt);
    PORTALS
        .iter()
        .find(|portal| tokens.iter().any(|token| token == **portal))
        .copied()
}

/// Validate one tier's output, halting the whole resolution on failure.
fn accept(url: &str, source: RouteSource) -> Option<ResolvedRoute> {
    validate_proposed_url(url)
        .map(|url| ResolvedRoute { url, source })
        .ok()
}

/// Tab-independent route proposal: derive `(portal, intent_class)` purely
/// from natural-language prompt tokens plus the caller's topic word (the
/// primary target noun), querying the normalized `portal_route` table
/// directly. The active tab's URL is never inspected, vetoed, or filtered —
/// `current_url` exists only for call-site compatibility and is ignored so
/// starting on `google.com` or `about:blank` never blocks cross-domain
/// pre-navigation.
#[must_use]
pub fn propose_route(
    prompt: &str,
    intent_class: &str,
    current_url: Option<&url::Url>,
    ctx: &ResolutionContext<'_>,
) -> Option<ResolvedRoute> {
    let _ = current_url;
    resolve_entry_url(prompt, intent_class, ctx)
}

/// Resolve an entry URL for an ad-hoc prompt and intent class (the
/// caller's normalized topic word — today, the ephemeral label or primary
/// target noun).
/// Deterministic, offline unless an adapter is configured. Tier 5 (search
/// fallback) guarantees a grounded entry for any non-empty prompt, so
/// `None` now means only empty prompts or fail-closed validation —
/// table misses advance instead of terminating.
/// Tab-independent: no current-tab URL is inspected.
#[must_use]
pub fn resolve_entry_url(
    prompt: &str,
    intent_class: &str,
    ctx: &ResolutionContext<'_>,
) -> Option<ResolvedRoute> {
    // Tier 2: connected-account entity, only with a directory wired.
    if let Some(dir) = ctx.account_dir
        && let Some(url) = resolve_repo_entity(prompt, dir)
    {
        return accept(&url, RouteSource::AccountEntity);
    }
    // Tier 3: curated route table on detected portal + class.
    if let Some(portal) = detect_portal(prompt)
        && let Some(url) = portal_route(portal, intent_class)
    {
        return accept(url, RouteSource::PortalRouteTable);
    }
    // Tier 4: configured LLM adapter, output untrusted until validated.
    if let Some(llm) = ctx.llm
        && let Some(url) = llm.propose_url(prompt)
    {
        return accept(&url, RouteSource::LlmFallback);
    }
    // Tier 5: grounded search fallback — fixed template, no TLD guessing.
    // Only runs when no stronger tier proposed anything; invalid stronger
    // proposals already returned `None` above without falling through.
    if let Some(url) = search_fallback_url(prompt) {
        return accept(&url, RouteSource::SearchFallback);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_resolver::{DirectoryError, RepoRef};

    struct FixtureDirectory {
        repos: Vec<RepoRef>,
    }

    impl AccountDirectory for FixtureDirectory {
        fn github_repos(&self) -> Result<Vec<RepoRef>, DirectoryError> {
            Ok(self.repos.clone())
        }
    }

    struct MockLlm {
        answer: Option<String>,
    }

    impl LlmUrlProposer for MockLlm {
        fn propose_url(&self, _prompt: &str) -> Option<String> {
            self.answer.clone()
        }
    }

    fn repo(owner: &str, name: &str) -> RepoRef {
        RepoRef {
            owner: owner.into(),
            name: name.into(),
            html_url: format!("https://github.com/{owner}/{name}"),
        }
    }

    fn empty_ctx() -> ResolutionContext<'static> {
        ResolutionContext {
            account_dir: None,
            llm: None,
        }
    }

    #[test]
    fn tier_order_prefers_account_entity_over_route_table() {
        // The prompt matches both tiers: a connected repo named like the
        // class word, and the portal table. The entity must win.
        let dir = FixtureDirectory {
            repos: vec![
                repo("fixture-owner", "invoices"),
                repo("fixture-owner", "website"),
            ],
        };
        let ctx = ResolutionContext {
            account_dir: Some(&dir),
            llm: None,
        };
        let Some(resolved) =
            resolve_entry_url("download all my invoices from github", "invoices", &ctx)
        else {
            panic!("entity tier resolves");
        };
        assert_eq!(resolved.source, RouteSource::AccountEntity);
        assert_eq!(
            resolved.url.as_str(),
            "https://github.com/fixture-owner/invoices"
        );
    }

    #[test]
    fn entity_resolver_falls_through_to_search_without_connected_account() {
        // No directory wired: entity-dependent prompts fall past tier 2.
        // The class here matches no table row either, so tier 5 (grounded
        // search) resolves instead of terminating as a miss.
        let Some(resolved) = resolve_entry_url(
            "check out my portopsy on github",
            "repositories",
            &empty_ctx(),
        ) else {
            panic!("search fallback resolves");
        };
        assert_eq!(resolved.source, RouteSource::SearchFallback);
        assert!(
            resolved
                .url
                .as_str()
                .starts_with("https://www.google.com/search?q=")
        );
    }

    #[test]
    fn unknown_prompts_advance_to_search_template_without_tld_guessing() {
        // Regression: "open amazon for me" must never become amazon.com /
        // amazon.in — the only dynamic URL is the fixed search template.
        let Some(resolved) = resolve_entry_url("open amazon for me", "amazon", &empty_ctx()) else {
            panic!("unknown prompt advances to search");
        };
        assert_eq!(resolved.source, RouteSource::SearchFallback);
        assert_eq!(
            resolved.url.as_str(),
            "https://www.google.com/search?q=open+amazon+for+me"
        );
        assert!(!resolved.url.as_str().contains("amazon.com"));
        assert!(!resolved.url.as_str().contains("amazon.in"));
        // Search template helper is pure and total on non-empty prompts.
        assert_eq!(
            search_fallback_url("open amazon for me").as_deref(),
            Some("https://www.google.com/search?q=open+amazon+for+me")
        );
        assert_eq!(search_fallback_url("   "), None);
        assert_eq!(search_fallback_url(""), None);
        // Empty prompts keep the miss dead-end (no empty search navigation).
        assert_eq!(resolve_entry_url("   ", "amazon", &empty_ctx()), None);
    }

    #[test]
    fn validation_rejects_credentials_and_non_https_across_all_tiers() {
        // Entity tier with a hostile directory URL fails closed without
        // falling through to search.
        struct EvilDirectory;
        impl AccountDirectory for EvilDirectory {
            fn github_repos(&self) -> Result<Vec<RepoRef>, DirectoryError> {
                Ok(vec![RepoRef {
                    owner: "evil".into(),
                    name: "portopsy".into(),
                    html_url: "https://user:secret@github.com/evil/portopsy".into(),
                }])
            }
        }
        let evil_dir = EvilDirectory;
        let ctx = ResolutionContext {
            account_dir: Some(&evil_dir),
            llm: None,
        };
        assert_eq!(
            resolve_entry_url("check out my portopsy on github", "portopsy", &ctx),
            None
        );
        // LLM tier: credentials and non-https both fail closed.
        for answer in [
            "https://user:pass@github.com/settings/billing",
            "http://github.com/settings/billing",
            "javascript:alert(1)",
            "data:text/html,hi",
        ] {
            let evil = MockLlm {
                answer: Some(answer.into()),
            };
            let ctx = ResolutionContext {
                account_dir: None,
                llm: Some(&evil),
            };
            assert_eq!(
                resolve_entry_url("open the dashboard thing on github", "dashboard", &ctx),
                None,
                "{answer} must fail closed"
            );
        }
        // Search tier itself never emits credentials or non-https: fixed
        // https template over an allowlisted host.
        let Some(resolved) = resolve_entry_url("open amazon for me", "amazon", &empty_ctx()) else {
            panic!("search resolves");
        };
        assert_eq!(resolved.url.scheme(), "https");
        assert!(resolved.url.username().is_empty());
        assert!(crate::url_policy::validate_proposed_url(resolved.url.as_str()).is_ok());
    }

    #[test]
    fn llm_fallback_output_must_pass_validation() {
        // A hostile model answer fails closed instead of navigating.
        let evil = MockLlm {
            answer: Some("https://github.com.evil.com/x".into()),
        };
        let ctx = ResolutionContext {
            account_dir: None,
            llm: Some(&evil),
        };
        assert_eq!(
            resolve_entry_url("open the dashboard thing on github", "dashboard", &ctx),
            None
        );
        // A well-formed answer from a configured adapter flows through.
        let kind = MockLlm {
            answer: Some("https://github.com/settings/billing".into()),
        };
        let ctx = ResolutionContext {
            account_dir: None,
            llm: Some(&kind),
        };
        let Some(resolved) =
            resolve_entry_url("open the dashboard thing on github", "dashboard", &ctx)
        else {
            panic!("valid LLM answer resolves");
        };
        assert_eq!(resolved.source, RouteSource::LlmFallback);
    }

    #[test]
    fn propose_route_ignores_current_tab_url_and_resolves_from_prompt() {
        // Tab-independent by design: the active tab never vetoes resolution.
        // The same prompt resolves from `google.com` or `about:blank` purely
        // via prompt tokens + primary noun against the normalized table.
        let ctx = empty_ctx();
        let prompt = "download all my invoices from github";
        let expected = "https://github.com/account/billing/history";
        for current in [
            url::Url::parse("https://google.com").ok(),
            url::Url::parse("about:blank").ok(),
            None,
        ] {
            let current_ref = current.as_ref();
            for intent_class in ["invoice", "invoices", "billing"] {
                let Some(resolved) = propose_route(prompt, intent_class, current_ref, &ctx) else {
                    panic!("prompt resolves from {current:?} for {intent_class}");
                };
                assert_eq!(resolved.source, RouteSource::PortalRouteTable);
                assert_eq!(resolved.url.as_str(), expected);
            }
        }
    }
}
