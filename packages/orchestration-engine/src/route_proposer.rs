#![deny(unsafe_code)]
//! Tiered cold-path entry resolution: attach a navigable `entry_url` to
//! ad-hoc intents that have none, before dispatch reaches the macro engine.
//!
//! Tier order, first hit wins. Tier 1 — the saved-playbook lookup — is the
//! caller's pre-step: this runs only when `resolve_command` already yielded
//! an ephemeral intent, so nothing here reimplements matching. Proven
//! workflows (including the seeded GitHub invoice harvester) therefore
//! answer before any tier below is consulted:
//!
//! 1. Saved playbook — decided upstream, never re-run here.
//! 2. Connected-account entity (`entity_resolver`), only with a directory.
//! 3. LLM fallback, only with a configured adapter — and its output is
//!    untrusted input, validated like every other tier.
//! 4. Grounded search-and-follow — fixed `https://www.google.com/search?q=…`
//!    template over the raw prompt. Never guesses TLDs; the dispatcher
//!    navigates to the search page and grounds the destination host from a
//!    real click on the live AX tree.
//!
//! There is deliberately no static route table between them: a curated
//! `(portal, class) → URL` list needed a portal whitelist to stay
//! meaningful, and both of its jobs are now covered — proven routes by
//! tier 1, unknown ones by tier 4.
//!
//! Every tier's output passes through `url_policy` validation. Any
//! validation failure returns `None` immediately — a corrupt tier never
//! falls through to a weaker one. The search tier only runs when no
//! stronger tier proposed anything; an invalid stronger proposal still
//! fails closed without falling through.
//!
//! # Slot resolution runs alongside URL resolution
//!
//! The tiers above answer *where to start*. [`resolve_slots`] answers *what
//! to look for once there* — the noun search-and-follow grounds on — and it
//! has its own sub-cascade, because the search template is the same URL
//! whether the slots came from grammar, a parser, or nowhere:
//!
//! * **Tier 2A** — the deterministic grammar parse reports
//!   [`crate::Confidence::High`], so it is used as-is at zero token cost.
//! * **Tier 2B** — low confidence with a parser configured: the fenced
//!   [`crate::IntentParser`] seam supplies slots, bounded by
//!   [`crate::PARSER_TIMEOUT_MS`].
//! * **Tier 2C** — no parser, a declined parse, a timeout, or output that
//!   failed the slot fence: the low-confidence grammar slots stand, and
//!   Stage 2 falls back to the intent's own probe text over the raw search
//!   page. Degradation, never failure — offline is a normal outcome.

use crate::{
    entity_resolver::{AccountDirectory, resolve_repo_entity},
    intent_parser::{IntentParser, parse_prompt_bounded},
    intent_resolver::ParsedGrammar,
    url_policy::validate_proposed_url,
};
use std::sync::Arc;

/// Where a resolved route came from. Recorded on the resolution and logged
/// with the navigation proposal so wrong sources are debuggable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteSource {
    AccountEntity,
    LlmFallback,
    SearchFallback,
}

/// Fixed grounded search entry: the only dynamic URL the resolver ever
/// invents, and it invents no host — always `www.google.com/search`.
/// The raw prompt becomes the `q` value; downstream grounding clicks a
/// real AX result link, never a guessed TLD.
const SEARCH_BASE: &str = "https://www.google.com/search";

/// Strip conversational filler from an ad-hoc prompt before it becomes a
/// search query: `"open amazon for me."` → `"open amazon"`.
///
/// Reuses the resolver's existing stopword vocabulary
/// ([`crate::intent_resolver::content_tokens`]) instead of introducing a
/// second filler list, so the words dropped here are exactly the words that
/// already never identify a target anywhere else in the pipeline.
/// Tokenization splits on non-alphanumerics, so trailing punctuation never
/// reaches the query either. Falls back to the trimmed prompt when filtering
/// would leave nothing, keeping the tier total on non-empty input.
#[must_use]
pub fn sanitize_search_query(prompt: &str) -> String {
    let trimmed = prompt.trim();
    let sanitized = crate::intent_resolver::content_tokens(trimmed).join(" ");
    if sanitized.is_empty() {
        trimmed.to_owned()
    } else {
        sanitized
    }
}

/// Build the grounded search-fallback URL for an ad-hoc prompt.
/// `None` for empty prompts (no query to ground), so callers keep the
/// `route_resolution_miss` dead-end instead of navigating to an empty
/// search. Never derives hosts from prompt words — the host is fixed.
#[must_use]
pub fn search_fallback_url(prompt: &str) -> Option<String> {
    let sanitized = sanitize_search_query(prompt);
    if sanitized.is_empty() {
        return None;
    }
    // `form_urlencoded` byte-serializes spaces as `+`, matching the
    // `?q=open+amazon` contract.
    let query: String = url::form_urlencoded::byte_serialize(sanitized.as_bytes()).collect();
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

/// Optional resolution inputs. Every one is inert when unset, which is the
/// production default: no account directory is wired (no stored GitHub
/// credential exists to back one), no URL adapter is configured, and the
/// shipped intent parser declines. Unset inputs degrade the cascade to
/// grounded search rather than failing it.
pub struct ResolutionContext<'a> {
    pub account_dir: Option<&'a dyn AccountDirectory>,
    pub llm: Option<&'a dyn LlmUrlProposer>,
    /// Fenced slot parser consulted only for low-confidence prompts, and
    /// only for slots — it never proposes a URL. See
    /// [`crate::intent_parser`] for the fence and the timeout.
    pub parser: Option<&'a Arc<dyn IntentParser>>,
}

/// Last-resort URL proposer. Synchronous by contract: adapters needing I/O
/// must bound it internally, and whatever comes back is validated as
/// untrusted input — never navigated on good faith.
pub trait LlmUrlProposer: Send + Sync {
    /// Propose a single URL for the prompt, or nothing.
    fn propose_url(&self, prompt: &str) -> Option<String>;
}

/// Validate one tier's output, halting the whole resolution on failure.
fn accept(url: &str, source: RouteSource) -> Option<ResolvedRoute> {
    validate_proposed_url(url)
        .map(|url| ResolvedRoute { url, source })
        .ok()
}

/// Which sub-tier produced the search-and-follow slots. Journaled so a
/// wrong follow is attributable to grammar, a parser, or neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotSource {
    /// Tier 2A: deterministic grammar, high confidence, zero tokens.
    GrammarFastPath,
    /// Tier 2B: fenced parser seam answered and its slots passed the fence.
    IntentParser,
    /// Tier 2C: nobody grounded the slots. Whatever low-confidence grammar
    /// found still travels, and Stage 2 falls back to the intent's own
    /// probe text.
    Ungrounded,
}

impl SlotSource {
    /// Short tier label for journal lines.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GrammarFastPath => "grammar",
            Self::IntentParser => "parser",
            Self::Ungrounded => "ungrounded",
        }
    }
}

/// Grounded slots plus the tier that produced them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSlots {
    pub grammar: ParsedGrammar,
    pub source: SlotSource,
}

/// Resolve the slots search-and-follow grounds on, deferring to the parser
/// seam only when the deterministic parse is not confident.
///
/// Total by construction — there is no failure mode, only progressively
/// weaker evidence:
///
/// 1. High-confidence grammar wins outright and the parser is never called,
///    so crisp commands cost nothing (tier 2A).
/// 2. Otherwise a configured parser gets one bounded shot (tier 2B). Its
///    output is sanitized before it is believed.
/// 3. A missing, declining, stalled, or fence-failing parser leaves the
///    low-confidence grammar in place (tier 2C).
///
/// Step 3 is why an offline machine still works: the raw search page is the
/// same destination either way, and Stage 2 simply grounds on the intent's
/// probe text instead of a parsed site name.
#[must_use]
pub fn resolve_slots(
    prompt: &str,
    connected_origin: Option<&url::Url>,
    ctx: &ResolutionContext<'_>,
) -> ResolvedSlots {
    let grammar = crate::intent_resolver::parse_grammar(prompt, connected_origin);
    if grammar.confidence.is_high() {
        return ResolvedSlots {
            grammar,
            source: SlotSource::GrammarFastPath,
        };
    }
    if let Some(parser) = ctx.parser
        && let Some(slots) = parse_prompt_bounded(parser, prompt)
    {
        return ResolvedSlots {
            grammar: ParsedGrammar::from_slots(&slots),
            source: SlotSource::IntentParser,
        };
    }
    ResolvedSlots {
        grammar,
        source: SlotSource::Ungrounded,
    }
}

/// Resolve an entry URL for an ad-hoc prompt.
///
/// Deterministic and offline unless an adapter is configured. The prompt is
/// the only input: no intent class, no topic word, and no current-tab URL,
/// so starting on `google.com` or `about:blank` never blocks cross-domain
/// pre-navigation. (The class parameter existed for the deleted route
/// table; every remaining tier reads the prompt itself.)
///
/// The search tier guarantees a grounded entry for any non-empty prompt, so
/// `None` means only an empty prompt or fail-closed validation.
#[must_use]
pub fn resolve_entry_url(prompt: &str, ctx: &ResolutionContext<'_>) -> Option<ResolvedRoute> {
    // Tier 2: connected-account entity, only with a directory wired.
    if let Some(dir) = ctx.account_dir
        && let Some(url) = resolve_repo_entity(prompt, dir)
    {
        return accept(&url, RouteSource::AccountEntity);
    }
    // Tier 3: configured LLM adapter, output untrusted until validated.
    if let Some(llm) = ctx.llm
        && let Some(url) = llm.propose_url(prompt)
    {
        return accept(&url, RouteSource::LlmFallback);
    }
    // Tier 4: grounded search fallback — fixed template, no TLD guessing.
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
            parser: None,
        }
    }

    #[test]
    fn tier_order_prefers_account_entity_over_search() {
        // A connected repo named like the prompt's artifact must win over the
        // grounded search tier below it.
        let dir = FixtureDirectory {
            repos: vec![
                repo("fixture-owner", "invoices"),
                repo("fixture-owner", "website"),
            ],
        };
        let ctx = ResolutionContext {
            account_dir: Some(&dir),
            llm: None,
            parser: None,
        };
        let Some(resolved) = resolve_entry_url("download all my invoices from github", &ctx) else {
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
        // No directory wired: entity-dependent prompts fall past tier 2 to
        // grounded search instead of terminating as a miss.
        let Some(resolved) = resolve_entry_url("check out my portopsy on github", &empty_ctx())
        else {
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
        let Some(resolved) = resolve_entry_url("open amazon for me", &empty_ctx()) else {
            panic!("unknown prompt advances to search");
        };
        assert_eq!(resolved.source, RouteSource::SearchFallback);
        // Filler is stripped before encoding: no `for`, no `me`, no period.
        assert_eq!(
            resolved.url.as_str(),
            "https://www.google.com/search?q=open+amazon"
        );
        assert!(!resolved.url.as_str().contains("amazon.com"));
        assert!(!resolved.url.as_str().contains("amazon.in"));
        // Search template helper is pure and total on non-empty prompts.
        assert_eq!(
            search_fallback_url("open amazon for me").as_deref(),
            Some("https://www.google.com/search?q=open+amazon")
        );
        assert_eq!(search_fallback_url("   "), None);
        assert_eq!(search_fallback_url(""), None);
        // Empty prompts keep the miss dead-end (no empty search navigation).
        assert_eq!(resolve_entry_url("   ", &empty_ctx()), None);
    }

    #[test]
    fn search_query_sanitization_strips_filler_and_punctuation() {
        // The reported formatting bug: conversational filler and trailing
        // punctuation must never reach `?q=`.
        assert_eq!(sanitize_search_query("open amazon for me."), "open amazon");
        assert_eq!(
            sanitize_search_query("  please open amazon!  "),
            "open amazon"
        );
        // Only conversational filler goes (`my`). Prepositions that read
        // naturally in a query (`from`) are left alone: this strips noise,
        // it does not rewrite the user's search.
        assert_eq!(
            sanitize_search_query("download my invoices from github"),
            "download invoices from github"
        );
        // Casing normalizes; multi-space collapses.
        assert_eq!(sanitize_search_query("Open   AMAZON"), "open amazon");
        // A prompt made only of filler still searches something rather than
        // producing an empty query (keeps the tier total on non-empty input).
        assert_eq!(sanitize_search_query("please the"), "please the");
        assert_eq!(sanitize_search_query(""), "");
        // And the URL built from it carries the sanitized form verbatim.
        assert_eq!(
            search_fallback_url("open amazon for me.").as_deref(),
            Some("https://www.google.com/search?q=open+amazon")
        );
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
            parser: None,
        };
        assert_eq!(
            resolve_entry_url("check out my portopsy on github", &ctx),
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
                parser: None,
            };
            assert_eq!(
                resolve_entry_url("open the dashboard thing on github", &ctx),
                None,
                "{answer} must fail closed"
            );
        }
        // Search tier itself never emits credentials or non-https: fixed
        // https template over an allowlisted host.
        let Some(resolved) = resolve_entry_url("open amazon for me", &empty_ctx()) else {
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
            parser: None,
        };
        assert_eq!(
            resolve_entry_url("open the dashboard thing on github", &ctx),
            None
        );
        // A well-formed answer from a configured adapter flows through.
        let kind = MockLlm {
            answer: Some("https://github.com/settings/billing".into()),
        };
        let ctx = ResolutionContext {
            account_dir: None,
            llm: Some(&kind),
            parser: None,
        };
        let Some(resolved) = resolve_entry_url("open the dashboard thing on github", &ctx) else {
            panic!("valid LLM answer resolves");
        };
        assert_eq!(resolved.source, RouteSource::LlmFallback);
    }

    #[test]
    fn resolution_is_prompt_only_with_no_static_route_table() {
        // Nothing between the entity tier and grounded search: a portal-shaped
        // prompt that the deleted table used to answer now advances to the
        // search template, where Stage 2 grounds the real destination from a
        // live click instead of a curated deep link.
        let ctx = empty_ctx();
        for prompt in [
            "download all my invoices from github",
            "download my github billing invoices",
        ] {
            let Some(resolved) = resolve_entry_url(prompt, &ctx) else {
                panic!("{prompt} resolves");
            };
            assert_eq!(resolved.source, RouteSource::SearchFallback);
            assert_eq!(resolved.url.host_str(), Some("www.google.com"));
            // No invented deep link survives anywhere in the proposal.
            assert!(!resolved.url.path().contains("billing"));
        }
    }
}
