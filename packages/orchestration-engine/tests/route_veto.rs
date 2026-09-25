//! Directory-rung plausibility veto: a wrong result becomes a miss, never a
//! confident wrong navigation.
//!
//! The structured directory (`ChainedSiteSearch`: Brave API primary,
//! keyless `DuckDuckGo` fallback) accepts any structurally-valid URL, so a
//! site query once proposed a Microsoft support article and the run
//! navigated there confidently. Every directory hit now passes a veto
//! before acceptance: the returned host's registrable label must contain
//! the queried name or vice versa, with exactly one blessed alias
//! (`gmail` → `mail.google.com`). A vetoed hit falls through to the
//! grounder rung (3c); the `route_vetoed: …` note rides the fallthrough
//! route so the journal reads as a veto, not a silent miss.
//!
//! Hermetic: stub/fake adapters only — no network, no model.

use orchestration_engine::{
    DomainGrounder, ResolutionContext, RouteSource, SiteHit, SiteSearchClient, resolve_entry_url,
};

/// Fake directory: answers every query with the configured URL, labeled
/// with the configured backend (`"brave"` / `"ddg"` / …).
struct FakeDirectory {
    url: Option<String>,
    backend: &'static str,
}

impl SiteSearchClient for FakeDirectory {
    fn search_site(&self, _site_name: &str) -> Option<SiteHit> {
        self.url.clone().map(|url| SiteHit {
            url,
            backend: self.backend,
        })
    }
}

/// Fake grounder: answers only for the configured site name.
struct FakeGrounder {
    site: &'static str,
    domain: Option<String>,
}

impl DomainGrounder for FakeGrounder {
    fn ground_domain(&self, site_name: &str, _region_hint: &str) -> Option<String> {
        if site_name == self.site {
            self.domain.clone()
        } else {
            None
        }
    }
}

fn ctx<'a>(
    search: Option<&'a dyn SiteSearchClient>,
    grounder: Option<&'a dyn DomainGrounder>,
) -> ResolutionContext<'a> {
    ResolutionContext {
        account_dir: None,
        llm: None,
        parser: None,
        shortcuts: None,
        site_search: search,
        domain_grounder: grounder,
        region_hint: "",
    }
}

#[test]
fn veto_accepts_name_matching_host() {
    // The happy path: `reddit` → `www.reddit.com` — the registrable label
    // `reddit.com` contains the queried name, so the hit is accepted and
    // carries the serving backend.
    let directory = FakeDirectory {
        url: Some("https://www.reddit.com/".to_owned()),
        backend: "ddg",
    };
    let Some(resolved) =
        resolve_entry_url("open reddit for me", None, &ctx(Some(&directory), None))
    else {
        panic!("matching host must be accepted");
    };
    assert_eq!(resolved.source, RouteSource::SiteSearch);
    assert_eq!(resolved.url.as_str(), "https://www.reddit.com/");
    assert_eq!(resolved.directory_backend, Some("ddg"));
    assert_eq!(resolved.directory_veto, None);
}

#[test]
fn veto_rejects_wrong_host_and_falls_through_to_grounder() {
    // The live failure mode: the directory proposed a Microsoft support
    // article for `reddit`. The veto must reject it — and because the
    // grounder rung is next, the run still grounds via 3c instead of
    // navigating to the article.
    let directory = FakeDirectory {
        url: Some("https://support.microsoft.com/en-us/windows".to_owned()),
        backend: "brave",
    };
    let grounder = FakeGrounder {
        site: "reddit",
        domain: Some("reddit.com".to_owned()),
    };
    let Some(resolved) = resolve_entry_url(
        "open reddit for me",
        None,
        &ctx(Some(&directory), Some(&grounder)),
    ) else {
        panic!("vetoed hit must fall through to the grounder rung");
    };
    assert_eq!(resolved.source, RouteSource::DomainGrounded);
    assert_eq!(resolved.url.as_str(), "https://reddit.com/");
    assert_eq!(resolved.directory_backend, None);
    assert_eq!(
        resolved.directory_veto.as_deref(),
        Some(
            "route_vetoed: site_search 'reddit' → support.microsoft.com · host mismatch; trying grounder"
        ),
    );
}

#[test]
fn veto_without_grounder_is_an_honest_miss() {
    // Same wrong hit, no grounder wired: the run misses honestly instead
    // of navigating to the support article. A wrong result becomes a
    // miss, never a confident wrong navigation.
    let directory = FakeDirectory {
        url: Some("https://support.microsoft.com/en-us/windows".to_owned()),
        backend: "ddg",
    };
    assert_eq!(
        resolve_entry_url("open reddit for me", None, &ctx(Some(&directory), None)),
        None
    );
}

#[test]
fn alias_gmail_to_mail_google_com_is_accepted() {
    // The single blessed alias: `gmail` is served from `mail.google.com`,
    // whose registrable label (`google.com`) matches neither containment
    // direction — without the alias this legitimate hit would veto.
    let directory = FakeDirectory {
        url: Some("https://mail.google.com/".to_owned()),
        backend: "ddg",
    };
    let Some(resolved) = resolve_entry_url("open gmail for me", None, &ctx(Some(&directory), None))
    else {
        panic!("blessed alias must be accepted");
    };
    assert_eq!(resolved.source, RouteSource::SiteSearch);
    assert_eq!(resolved.url.as_str(), "https://mail.google.com/");
    assert_eq!(resolved.directory_backend, Some("ddg"));
    assert_eq!(resolved.directory_veto, None);
}

#[test]
fn alias_does_not_bless_other_hosts_for_gmail() {
    // The alias is exact: `gmail` → any host whose registrable domain is
    // not `google.com` still vetoes. No grounder wired, so this is a miss.
    let directory = FakeDirectory {
        url: Some("https://evil.example/".to_owned()),
        backend: "brave",
    };
    assert_eq!(
        resolve_entry_url("open gmail for me", None, &ctx(Some(&directory), None)),
        None
    );
}

#[test]
fn backend_label_is_recorded_per_backend() {
    // Backend transparency: the route names whichever backend served the
    // hit, so the journal can say `· via brave` vs `· via ddg`.
    for backend in ["brave", "ddg"] {
        let directory = FakeDirectory {
            url: Some("https://www.reddit.com/".to_owned()),
            backend,
        };
        let Some(resolved) =
            resolve_entry_url("open reddit for me", None, &ctx(Some(&directory), None))
        else {
            panic!("{backend} hit must be accepted");
        };
        assert_eq!(resolved.source, RouteSource::SiteSearch, "{backend}");
        assert_eq!(resolved.directory_backend, Some(backend), "{backend}");
    }
}

#[test]
fn veto_documents_the_registrable_label_approximation() {
    // `amazon.co.uk` labels as `co.uk` under the last-two-labels
    // approximation (no public-suffix list), so the veto rejects it even
    // though it is a legitimate Amazon host. The ladder falls through to
    // the grounder rung rather than navigating — a documented heuristic
    // tradeoff, journaled as a veto.
    let directory = FakeDirectory {
        url: Some("https://www.amazon.co.uk/".to_owned()),
        backend: "ddg",
    };
    let grounder = FakeGrounder {
        site: "amazon",
        domain: Some("amazon.in".to_owned()),
    };
    let Some(resolved) = resolve_entry_url(
        "open amazon for me",
        None,
        &ctx(Some(&directory), Some(&grounder)),
    ) else {
        panic!("vetoed co.uk hit must fall through to the grounder rung");
    };
    assert_eq!(resolved.source, RouteSource::DomainGrounded);
    assert_eq!(resolved.url.as_str(), "https://amazon.in/");
    assert_eq!(
        resolved.directory_veto.as_deref(),
        Some(
            "route_vetoed: site_search 'amazon' → www.amazon.co.uk · host mismatch; trying grounder"
        ),
    );
}

#[test]
fn veto_applies_to_in_page_site_grounding_too() {
    // Tier 3b grounds only the site for in-page goals
    // ("open my profile on the reddit"): a mismatched directory hit there
    // must veto identically, falling through to the grounder rung.
    let directory = FakeDirectory {
        url: Some("https://support.microsoft.com/en-us/windows".to_owned()),
        backend: "ddg",
    };
    let grounder = FakeGrounder {
        site: "reddit",
        domain: Some("reddit.com".to_owned()),
    };
    let Some(resolved) = resolve_entry_url(
        "open my profile on the reddit",
        None,
        &ctx(Some(&directory), Some(&grounder)),
    ) else {
        panic!("in-page site hit must veto and fall through");
    };
    assert_eq!(resolved.source, RouteSource::DomainGrounded);
    assert_eq!(resolved.url.as_str(), "https://reddit.com/");
    assert!(
        resolved
            .directory_veto
            .as_deref()
            .is_some_and(|note| note.starts_with("route_vetoed: site_search 'reddit'")),
        "veto note carried, got {:?}",
        resolved.directory_veto
    );
}
