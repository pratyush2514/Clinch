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
//!
//! Every tier's output passes through `url_policy` validation, including
//! the route table itself. Any validation failure returns `None`
//! immediately — a corrupt tier never falls through to a weaker one.

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
fn detect_portal(prompt: &str) -> Option<&'static str> {
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

/// Resolve an entry URL for an ad-hoc prompt and intent class (the
/// caller's normalized topic word — today, the ephemeral label).
/// Deterministic, offline unless an adapter is configured, and total on
/// failure: `None` keeps current behavior (manual/portal flow, no silent
/// navigation).
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
    fn entity_resolver_returns_none_without_connected_account() {
        // No directory wired: entity-dependent prompts fall past tier 2.
        // The class here matches no table row either, so nothing resolves.
        assert_eq!(
            resolve_entry_url(
                "check out my portopsy on github",
                "repositories",
                &empty_ctx()
            ),
            None
        );
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
}
