//! Integration tests for `orchestration_engine::route_proposer`.
//!
//! Moved out of `src/route_proposer.rs` so the main source stays test-free.

use orchestration_engine::route_proposer::*;

struct MockLlm {
    answer: Option<String>,
}

impl LlmUrlProposer for MockLlm {
    fn propose_url(&self, _prompt: &str) -> Option<String> {
        self.answer.clone()
    }
}

fn empty_ctx() -> ResolutionContext<'static> {
    ResolutionContext {
        llm: None,
        parser: None,
        shortcuts: None,
        site_search: None,
        domain_grounder: None,
        region_hint: "",
    }
}

#[test]
fn direct_open_without_grounding_is_a_miss_not_a_search() {
    // Behavior change, by design: "open amazon for me" is a direct
    // open, so with no shortcut, no explicit domain, and no directory
    // key it misses instead of scraping a search page. The caller
    // turns the miss into "try a full domain or save a site shortcut".
    // TLDs are still never guessed: the miss carries no URL at all.
    assert_eq!(
        resolve_entry_url("open amazon for me", None, &empty_ctx()),
        None
    );
    assert_eq!(resolve_entry_url("open amazon", None, &empty_ctx()), None);
    // Non-direct-open prompts are no different: with no rung grounding
    // the grammar's site slots, they miss honestly too — there is no
    // search page to fall back to.
    assert_eq!(
        resolve_entry_url("download all my invoices from github", None, &empty_ctx()),
        None
    );
    // Empty prompts keep the miss dead-end (no empty search navigation).
    assert_eq!(resolve_entry_url("   ", None, &empty_ctx()), None);
}

struct StubSiteSearch {
    answer: Option<String>,
}

impl SiteSearchClient for StubSiteSearch {
    fn search_site(&self, _site_name: &str) -> Option<SiteHit> {
        self.answer.clone().map(|url| SiteHit {
            url,
            backend: "stub",
        })
    }
}

fn in_page_ctx(search: &StubSiteSearch) -> ResolutionContext<'_> {
    ResolutionContext {
        llm: None,
        parser: None,
        shortcuts: None,
        site_search: Some(search),
        domain_grounder: None,
        region_hint: "",
    }
}

#[test]
fn in_page_goal_grounds_site_never_searches() {
    // "open my profile on the reddit" names a site and carries an
    // artifact noun: the ladder grounds the SITE, and the prompt must
    // never become a search query. The artifact noun is the
    // dispatcher's business, not the URL's.
    let search = StubSiteSearch {
        answer: Some("https://www.reddit.com/".to_owned()),
    };
    let Some(resolved) =
        resolve_entry_url("open my profile on the reddit", None, &in_page_ctx(&search))
    else {
        panic!("in-page goal grounds the site");
    };
    assert_eq!(resolved.source, RouteSource::SiteSearch);
    assert_eq!(resolved.url.as_str(), "https://www.reddit.com/");
}

#[test]
fn in_page_goal_with_ungrounded_site_is_a_miss() {
    // No rung knows the site and there is nowhere to navigate: the
    // prompt is an honest miss — no search page stands in for a
    // destination. (In production the directory rung is always live via
    // the keyless DDG fallback, so this corner is theoretical.)
    let search = StubSiteSearch { answer: None };
    assert_eq!(
        resolve_entry_url("open my profile on the reddit", None, &in_page_ctx(&search)),
        None
    );
}

#[test]
fn coordinated_prompt_is_honest_miss_in_tier_4() {
    // "open my profile on reddit and twitter" is multi-target: tier 3b
    // refuses the in-page goal and tier 4 now vetoes the site ladder
    // too — no silent pursuit of one target's site, honest miss
    // instead of a search page or one site's home page.
    let search = StubSiteSearch {
        answer: Some("https://www.reddit.com/".to_owned()),
    };
    assert!(
        resolve_entry_url(
            "open my profile on reddit and twitter",
            None,
            &in_page_ctx(&search),
        )
        .is_none(),
        "multi-target prompt must miss honestly"
    );
}

#[test]
fn validation_rejects_credentials_and_non_https_across_all_tiers() {
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
            llm: Some(&evil),
            parser: None,
            shortcuts: None,
            site_search: None,
            domain_grounder: None,
            region_hint: "",
        };
        assert_eq!(
            resolve_entry_url("open the dashboard thing on github", None, &ctx),
            None,
            "{answer} must fail closed"
        );
    }
    // A direct open the ladder cannot ground fails closed — it never
    // falls back to a search page. "open amazon for me" names a
    // destination, so a miss asks the user rather than scraping a SERP.
    assert_eq!(
        resolve_entry_url("open amazon for me", None, &empty_ctx()),
        None,
        "ungrounded direct open must miss, never search"
    );
    // The site-ladder retry is the last tier and it never invents a
    // destination either: an ungrounded non-direct-open prompt is a
    // miss, not a search page — nothing here can emit credentials,
    // non-https, or a guessed host.
    assert_eq!(
        resolve_entry_url("download the monthly site report", None, &empty_ctx()),
        None,
        "ungrounded non-direct-open prompt must miss, never search"
    );
}

#[test]
fn llm_fallback_output_must_pass_validation() {
    // A hostile model answer fails closed instead of navigating.
    let evil = MockLlm {
        answer: Some("https://github.com.evil.com/x".into()),
    };
    let ctx = ResolutionContext {
        llm: Some(&evil),
        parser: None,
        shortcuts: None,
        site_search: None,
        domain_grounder: None,
        region_hint: "",
    };
    assert_eq!(
        resolve_entry_url("open the dashboard thing on github", None, &ctx),
        None
    );
    // A well-formed answer from a configured adapter flows through.
    let kind = MockLlm {
        answer: Some("https://github.com/settings/billing".into()),
    };
    let ctx = ResolutionContext {
        llm: Some(&kind),
        parser: None,
        shortcuts: None,
        site_search: None,
        domain_grounder: None,
        region_hint: "",
    };
    let Some(resolved) = resolve_entry_url("open the dashboard thing on github", None, &ctx) else {
        panic!("valid LLM answer resolves");
    };
    assert_eq!(resolved.source, RouteSource::LlmFallback);
}

#[test]
fn explicit_domain_outranks_configured_adapters() {
    // The user's typed destination is ground truth: neither a saved
    // shortcut, a structured directory, nor a configured model gets
    // to reinterpret it.
    let mut shortcuts = std::collections::HashMap::new();
    shortcuts.insert("github".into(), "https://example.com/other".into());
    let store = InMemoryShortcuts::new(shortcuts);
    let directory = MockDirectory {
        url: Some("https://directory.example/result".into()),
    };
    let llm = MockLlm {
        answer: Some("https://github.com/settings/billing".into()),
    };
    let ctx = ResolutionContext {
        llm: Some(&llm),
        parser: None,
        shortcuts: Some(&store),
        site_search: Some(&directory),
        domain_grounder: None,
        region_hint: "",
    };
    let Some(resolved) = resolve_entry_url("open github.com/pratyush2514", None, &ctx) else {
        panic!("explicit domain resolves");
    };
    assert_eq!(resolved.url.as_str(), "https://github.com/pratyush2514");
    assert_eq!(resolved.source, RouteSource::ExplicitDomain);
}

#[test]
fn resolution_is_prompt_only_with_no_static_route_table() {
    // No rung between the ladder and the honest miss: a
    // portal-shaped prompt that the deleted table used to answer now
    // misses instead of inventing a deep link — no static route table,
    // no search page standing in for a destination.
    let ctx = empty_ctx();
    for prompt in [
        "download all my invoices from github",
        "download my github billing invoices",
    ] {
        assert_eq!(
            resolve_entry_url(prompt, None, &ctx),
            None,
            "{prompt} misses honestly"
        );
    }
}

struct MockDirectory {
    url: Option<String>,
}

impl SiteSearchClient for MockDirectory {
    fn search_site(&self, _site_name: &str) -> Option<SiteHit> {
        self.url.clone().map(|url| SiteHit {
            url,
            backend: "mock",
        })
    }
}

fn ladder_ctx<'a>(
    shortcuts: Option<&'a dyn ShortcutStore>,
    site_search: Option<&'a dyn SiteSearchClient>,
    domain_grounder: Option<&'a dyn DomainGrounder>,
    region_hint: &'a str,
) -> ResolutionContext<'a> {
    ResolutionContext {
        llm: None,
        parser: None,
        shortcuts,
        site_search,
        domain_grounder,
        region_hint,
    }
}

#[test]
fn ladder_explicit_domain_wins_without_any_lookup() {
    // "open amazon.in" names the destination outright: no shortcut, no
    // directory, no network — and no TLD was guessed, it was typed.
    let ctx = ladder_ctx(None, None, None, "");
    let Some(resolved) = resolve_entry_url("open amazon.in", None, &ctx) else {
        panic!("explicit domain resolves");
    };
    assert_eq!(resolved.source, RouteSource::ExplicitDomain);
    assert_eq!(resolved.url.as_str(), "https://amazon.in/");
    // A full typed URL (with path) travels verbatim.
    let Some(resolved) = resolve_entry_url(
        "open https://github.com/settings/billing for me",
        None,
        &ctx,
    ) else {
        panic!("typed URL resolves");
    };
    assert_eq!(resolved.source, RouteSource::ExplicitDomain);
    assert_eq!(resolved.url.as_str(), "https://github.com/settings/billing");
}

#[test]
fn ladder_explicit_domain_beats_shortcut_and_directory() {
    // The prompt's own domain outranks stored data: this invocation
    // named amazon.in, even if a shortcut says otherwise.
    let shortcuts = InMemoryShortcuts::new(
        [("amazon".to_owned(), "https://www.amazon.com/".to_owned())]
            .into_iter()
            .collect(),
    );
    let directory = MockDirectory {
        url: Some("https://www.amazon.in/".to_owned()),
    };
    let ctx = ladder_ctx(Some(&shortcuts), Some(&directory), None, "");
    let Some(resolved) = resolve_entry_url("open amazon.in", None, &ctx) else {
        panic!("explicit domain resolves");
    };
    assert_eq!(resolved.source, RouteSource::ExplicitDomain);
    assert_eq!(resolved.url.as_str(), "https://amazon.in/");
}

#[test]
fn ladder_shortcut_grounds_a_bare_site_name() {
    // The learned path: "open amazon for me" hits the user's saved
    // shortcut with no network at all — politeness filler included.
    let shortcuts = InMemoryShortcuts::new(
        [("amazon".to_owned(), "https://www.amazon.in/".to_owned())]
            .into_iter()
            .collect(),
    );
    let ctx = ladder_ctx(Some(&shortcuts), None, None, "");
    for prompt in ["open amazon", "open amazon for me", "please open amazon"] {
        let Some(resolved) = resolve_entry_url(prompt, None, &ctx) else {
            panic!("{prompt} resolves via shortcut");
        };
        assert_eq!(resolved.source, RouteSource::Shortcut, "{prompt}");
        assert_eq!(resolved.url.as_str(), "https://www.amazon.in/", "{prompt}");
    }
}

#[test]
fn ladder_site_search_grounds_a_cold_name() {
    // No shortcut, no typed domain, but a directory is configured: the
    // structured lookup grounds the name — still no SERP scraping.
    let directory = MockDirectory {
        url: Some("https://www.amazon.in/".to_owned()),
    };
    let ctx = ladder_ctx(None, Some(&directory), None, "");
    let Some(resolved) = resolve_entry_url("open amazon for me", None, &ctx) else {
        panic!("directory resolves the cold name");
    };
    assert_eq!(resolved.source, RouteSource::SiteSearch);
    assert_eq!(resolved.url.as_str(), "https://www.amazon.in/");
}

struct MockGrounder {
    domain: Option<String>,
    expect_region: Option<String>,
}

impl DomainGrounder for MockGrounder {
    fn ground_domain(&self, site_name: &str, region_hint: &str) -> Option<String> {
        if let Some(expected) = &self.expect_region {
            assert_eq!(region_hint, expected, "region hint passed through");
        }
        // Only answer for the site the test set up — proves the ladder
        // passes the parsed slot, not the raw prompt.
        if site_name == "amazon" {
            self.domain.clone()
        } else {
            None
        }
    }
}

#[test]
fn ladder_domain_grounder_grounds_with_region_hint() {
    // Muse-like: "open amazon for me" with no shortcut and no directory
    // key still opens directly via the fenced grounder — site slot
    // ("amazon") + region hint ("IN") → bare domain ("amazon.in").
    let grounder = MockGrounder {
        domain: Some("amazon.in".to_owned()),
        expect_region: Some("IN".to_owned()),
    };
    let ctx = ladder_ctx(None, None, Some(&grounder), "IN");
    let Some(resolved) = resolve_entry_url("open amazon for me", None, &ctx) else {
        panic!("grounder resolves the cold name");
    };
    assert_eq!(resolved.source, RouteSource::DomainGrounded);
    assert_eq!(resolved.url.as_str(), "https://amazon.in/");
}

#[test]
fn ladder_site_search_beats_grounder_but_loses_to_shortcut() {
    // Precedence: shortcut (user data) > site directory (ranked search
    // data) > grounder (generative fallback). Ranking is the ground
    // truth of what a site name means; it cannot hallucinate.
    let shortcuts = InMemoryShortcuts::new(
        [("amazon".to_owned(), "https://www.amazon.in/".to_owned())]
            .into_iter()
            .collect(),
    );
    let grounder = MockGrounder {
        domain: Some("amazon.com".to_owned()),
        expect_region: None,
    };
    let directory = MockDirectory {
        // `amazon.in`: the plausibility veto accepts it (registrable
        // label `amazon.in` contains the queried name). `amazon.co.uk`
        // would veto here — the last-two-labels approximation labels it
        // `co.uk` — which is exactly what the veto is for; see the
        // integration tests in `tests/route_veto.rs`.
        url: Some("https://www.amazon.in/".to_owned()),
    };
    // Shortcut wins over directory.
    let ctx = ladder_ctx(Some(&shortcuts), Some(&directory), Some(&grounder), "IN");
    let Some(resolved) = resolve_entry_url("open amazon", None, &ctx) else {
        panic!("shortcut wins");
    };
    assert_eq!(resolved.source, RouteSource::Shortcut);
    // Without shortcut, directory wins over grounder.
    let ctx = ladder_ctx(None, Some(&directory), Some(&grounder), "IN");
    let Some(resolved) = resolve_entry_url("open amazon", None, &ctx) else {
        panic!("directory beats grounder");
    };
    assert_eq!(resolved.source, RouteSource::SiteSearch);
    assert_eq!(resolved.url.as_str(), "https://www.amazon.in/");
    // Without directory, the grounder is the fallback.
    let ctx = ladder_ctx(None, None, Some(&grounder), "IN");
    let Some(resolved) = resolve_entry_url("open amazon", None, &ctx) else {
        panic!("grounder is the fallback");
    };
    assert_eq!(resolved.source, RouteSource::DomainGrounded);
    assert_eq!(resolved.url.as_str(), "https://amazon.com/");
}

#[test]
fn ladder_domain_grounder_malformed_falls_through() {
    // A grounder that returns junk never navigates: malformed domains
    // degrade to the next rung, never to a guessed URL.
    for bad in [
        "not a domain",
        "192.168.1.1",
        "https://amazon.in/", // full URL, not a bare domain
        "amazon",
        "",
    ] {
        let grounder = MockGrounder {
            domain: Some(bad.to_owned()),
            expect_region: None,
        };
        let directory = MockDirectory {
            url: Some("https://www.amazon.in/".to_owned()),
        };
        let ctx = ladder_ctx(None, Some(&directory), Some(&grounder), "IN");
        let Some(resolved) = resolve_entry_url("open amazon", None, &ctx) else {
            panic!("falls through to directory for {bad:?}");
        };
        assert_eq!(
            resolved.source,
            RouteSource::SiteSearch,
            "bad grounder output {bad:?} must not navigate"
        );
    }
    // And when nothing else grounds it, the miss is honest.
    let grounder = MockGrounder {
        domain: Some("bogus!!".to_owned()),
        expect_region: None,
    };
    let ctx = ladder_ctx(None, None, Some(&grounder), "IN");
    assert_eq!(resolve_entry_url("open amazon", None, &ctx), None);
}

#[test]
fn validate_grounded_domain_policy() {
    // Valid bare domains → https URLs.
    assert_eq!(
        validate_grounded_domain("amazon.in").as_deref(),
        Some("https://amazon.in")
    );
    assert_eq!(
        validate_grounded_domain("  WWW.AMAZON.IN. ").as_deref(),
        Some("https://www.amazon.in")
    );
    // Rejected: no TLD, raw IPs, credentials, URLs, empty.
    assert_eq!(validate_grounded_domain("amazon"), None);
    assert_eq!(validate_grounded_domain("192.168.1.1"), None);
    assert_eq!(validate_grounded_domain("::1"), None);
    assert_eq!(validate_grounded_domain("user:pass@amazon.in"), None);
    assert_eq!(validate_grounded_domain("https://amazon.in"), None);
    assert_eq!(validate_grounded_domain(""), None);
    assert_eq!(validate_grounded_domain("amazon.in/path"), None);
}

#[test]
fn region_hint_mapping() {
    assert_eq!(region_hint_from_timezone("Asia/Kolkata"), Some("IN"));
    assert_eq!(region_hint_from_timezone("Asia/Calcutta"), Some("IN"));
    assert_eq!(region_hint_from_timezone("America/New_York"), Some("US"));
    assert_eq!(region_hint_from_timezone("Europe/Paris"), Some("FR"));
    assert_eq!(region_hint_from_timezone("Europe/London"), Some("GB"));
    assert_eq!(region_hint_from_timezone("Mars/Olympus"), None);
}

#[test]
fn stub_grounder_declines() {
    let stub = StubDomainGrounder;
    assert_eq!(stub.ground_domain("amazon", "IN"), None);
    // With only the stub wired, the cold name still misses honestly.
    let ctx = ladder_ctx(None, None, Some(&stub), "IN");
    assert_eq!(resolve_entry_url("open amazon for me", None, &ctx), None);
}

#[test]
fn ladder_malformed_explicit_domain_fails_closed() {
    // A typed destination that is not navigable (non-https) is a miss,
    // not a search: Googling a typo'd scheme fixes nothing.
    let ctx = ladder_ctx(None, None, None, "");
    assert_eq!(resolve_entry_url("open http://amazon.in", None, &ctx), None);
}

#[test]
fn ladder_skips_multi_target_and_retrieval_prompts() {
    // "open amazon and flipkart" must never silently open one target:
    // it is not a direct open, and with no rung grounding the grammar's
    // site slots it misses honestly instead of searching.
    let ctx = empty_ctx();
    assert_eq!(
        resolve_entry_url("open amazon and flipkart", None, &ctx),
        None,
        "multi-target misses, never searches"
    );
    // A retrieval verb is not an open: "find amazon" misses the same
    // way when no rung knows the site.
    assert_eq!(
        resolve_entry_url("find amazon", None, &ctx),
        None,
        "retrieval verb misses, never searches"
    );
}

#[test]
fn explicit_url_shapes() {
    assert_eq!(
        explicit_url_in_prompt("open amazon.in").as_deref(),
        Some("https://amazon.in")
    );
    assert_eq!(
        explicit_url_in_prompt("open amazon.in.").as_deref(),
        Some("https://amazon.in")
    );
    assert_eq!(
        explicit_url_in_prompt("visit github.com/settings").as_deref(),
        Some("https://github.com/settings")
    );
    assert_eq!(
        explicit_url_in_prompt("open https://example.com/a?b=c").as_deref(),
        Some("https://example.com/a?b=c")
    );
    // Not domains: bare names, versions, and numbers stay ungrounded.
    assert_eq!(explicit_url_in_prompt("open amazon"), None);
    assert_eq!(explicit_url_in_prompt("open chapter 2.0"), None);
    assert_eq!(explicit_url_in_prompt("call 911"), None);
    assert_eq!(explicit_url_in_prompt(""), None);
}

#[test]
fn brave_top_url_parses_the_response_envelope() {
    let payload = serde_json::json!({
        "web": { "results": [
            { "title": "Amazon.in", "url": "https://www.amazon.in/" },
            { "title": "Other", "url": "https://example.com/" },
        ]},
    });
    assert_eq!(
        brave_top_url(&payload).as_deref(),
        Some("https://www.amazon.in/")
    );
    assert_eq!(brave_top_url(&serde_json::json!({})), None);
    assert_eq!(
        brave_top_url(&serde_json::json!({"web": {"results": []}})),
        None
    );
}

#[test]
fn brave_client_is_absent_without_a_key() {
    // No key, no client: the ladder degrades to ask-and-learn rather
    // than failing. (The live call itself is never made in tests.)
    let key = std::env::var("CLINCH_BRAVE_API_KEY").ok();
    if key.is_none_or(|key| key.trim().is_empty()) {
        assert!(BraveSiteSearch::from_env().is_none());
    }
}

/// Fixture shaped like the live HTML endpoint: a paid ad anchor first,
/// then organic results. The parser must skip the ad and unwrap the
/// `uddg` param of the first organic hit.
const DDG_FIXTURE: &str = r#"
<div class="result results_links results_links_deep result--ad ">
  <div class="links_main links_deep result__body">
<h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fduckduckgo.com%2Fy.js%3Fad_domain%3Dask%252Dchat.ai&amp;rut=aaa">Ad</a></h2>
  </div>
</div>
<div class="result results_links results_links_deep web-result ">
  <div class="links_main links_deep result__body">
<h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fclaude.com%2F&amp;rut=bbb">Claude</a></h2>
  </div>
</div>
<div class="result results_links results_links_deep web-result ">
  <div class="links_main links_deep result__body">
<h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FClaude&amp;rut=ccc">Wikipedia</a></h2>
  </div>
</div>
"#;

#[test]
fn ddg_top_url_skips_ads_and_unwraps_uddg() {
    assert_eq!(
        ddg_top_url(DDG_FIXTURE).as_deref(),
        Some("https://claude.com/")
    );
}

#[test]
fn ddg_top_url_returns_none_without_organic_results() {
    // Ad-only page: paid results are never destinations.
    let ads_only = r#"<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fduckduckgo.com%2Fy.js%3Fad_domain%3Dx&amp;rut=a">Ad</a>"#;
    assert_eq!(ddg_top_url(ads_only), None);
    // Anomaly / throttle page and empty bodies carry no results.
    assert_eq!(ddg_top_url(""), None);
    assert_eq!(ddg_top_url("<html><body>anomaly</body></html>"), None);
}

#[test]
fn ddg_unwrap_target_decodes_and_passes_through() {
    assert_eq!(
        ddg_unwrap_target("//duckduckgo.com/l/?uddg=https%3A%2F%2Fclaude.com%2F&amp;rut=x")
            .as_deref(),
        Some("https://claude.com/")
    );
    // Direct links pass through; junk is rejected.
    assert_eq!(
        ddg_unwrap_target("https://example.com/").as_deref(),
        Some("https://example.com/")
    );
    assert_eq!(ddg_unwrap_target("/relative/path"), None);
    assert_eq!(ddg_unwrap_target("javascript:void(0)"), None);
}

#[test]
fn chained_site_search_always_offers_ddg() {
    // The composite rung is never "unconfigured": even without a Brave
    // key the keyless fallback is present. (No network in tests — only
    // the wiring is asserted here.)
    let chain = ChainedSiteSearch {
        primary: None,
        fallback: DuckDuckGoSiteSearch::new(),
    };
    assert_eq!(chain.backend_label(), "ddg");
}
