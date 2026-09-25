//! Funnel routing (work item B) and the settle contract (work item D):
//! the pure planner decisions, the claim gate, the object-noun exclusion,
//! and the alias-aware landing check. No network, no browser, no model.

use orchestration_engine::{
    FunnelDecision, ObjectClass, SiteHit, SiteSearchClient, funnel_claims, funnel_landing_matches,
    funnel_plan, site_matches_host,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Recording directory client: counts how often the funnel's routing
/// consults it. The funnel plan is pure — it takes no `SiteSearchClient`
/// at all — so routing an already-on-origin prompt must leave this
/// counter at zero: the directory is never in the loop.
struct RecordingDirectory {
    calls: Arc<AtomicUsize>,
}

impl SiteSearchClient for RecordingDirectory {
    fn search_site(&self, _site_name: &str) -> Option<SiteHit> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        None
    }
}

#[test]
fn already_on_origin_never_consults_the_directory() {
    let calls = Arc::new(AtomicUsize::new(0));
    let _directory = RecordingDirectory {
        calls: calls.clone(),
    };
    // "open settings on reddit" with a live reddit.com portal: the site
    // slot matches the portal through the shared alias-aware host check,
    // so the funnel routes the object in-page. The plan carries everything
    // the dispatcher needs — no client, no ladder, no search.
    let plan = funnel_plan("open settings on reddit", false, Some("www.reddit.com"));
    assert_eq!(
        plan.decision,
        FunnelDecision::AlreadyOnOrigin {
            site: "reddit".to_owned(),
            object: Some(ObjectClass::Settings),
        }
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the directory client is never consulted on this path"
    );
    // Slots are journaled for the Session Activity UI.
    assert!(
        plan.journal_lines
            .iter()
            .any(|line| line.starts_with("funnel_slots:")),
        "journal: {}",
        plan.journal_lines.join("\n")
    );
}

#[test]
fn login_aside_is_journaled_never_routed() {
    // The aside strip runs before the claim gate: the login announcement
    // is journaled as policy, and routing sees only the cleaned prompt.
    let plan = funnel_plan(
        "open the X for me and make sure ill do the login first",
        false,
        None,
    );
    assert_eq!(
        plan.decision,
        FunnelDecision::GroundSite {
            site: "x".to_owned(),
            object: None,
        }
    );
    assert!(
        plan.journal_lines
            .iter()
            .any(|line| line.starts_with("aside_policy: user_will_login")),
        "journal: {}",
        plan.journal_lines.join("\n")
    );
    assert!(
        !plan.cleaned.contains("login"),
        "asides never reach the parser or the ladder: {:?}",
        plan.cleaned
    );
}

#[test]
fn funnel_claims_only_open_verb_non_plural() {
    // Polite framing still claims: the aside strip runs first.
    assert!(funnel_claims(
        "open the X for me and make sure ill do the login first"
    ));
    assert!(funnel_claims("i want you to open X for me"));
    assert!(funnel_claims("please open my profile on reddit"));
    // Retrieval verbs keep the search-grounded pipeline.
    assert!(!funnel_claims("download all my invoices from github"));
    assert!(!funnel_claims("find amazon"));
    // Plurals keep the batch lane even when open-led.
    let plan = funnel_plan("open all my tabs", true, None);
    assert_eq!(plan.decision, FunnelDecision::Declined);
    let plan = funnel_plan("download all my invoices from github", false, None);
    assert_eq!(plan.decision, FunnelDecision::Declined);
}

#[test]
fn object_noun_exclusion_routes_settings_in_page() {
    // "open settings" with a live Reddit portal: `settings` is the goal,
    // not a site — the site demotes to None, the portal derived from the
    // current URL is the implicit site, and the object is pursued in-page.
    // No SiteSearch on any arm of this decision.
    let plan = funnel_plan("open settings", false, Some("www.reddit.com"));
    assert_eq!(
        plan.decision,
        FunnelDecision::ImplicitSite {
            object: ObjectClass::Settings,
        }
    );
    // ...but an explicit prepositional site survives: "open settings on
    // reddit" keeps site `reddit`.
    let plan = funnel_plan("open settings on reddit", false, Some("www.reddit.com"));
    assert!(
        matches!(
            &plan.decision,
            FunnelDecision::AlreadyOnOrigin { site, object: Some(ObjectClass::Settings) }
            if site == "reddit"
        ),
        "got: {:?}",
        plan.decision
    );
}

#[test]
fn adjectival_site_mention_grounds_through_the_ladder() {
    // "open my reddit profile": the cold parse drops the adjective, so the
    // leftover content token is a site mention for the ladder to verify —
    // never an in-page pursuit on the wrong portal.
    let plan = funnel_plan("open my reddit profile", false, Some("www.google.com"));
    assert_eq!(
        plan.decision,
        FunnelDecision::GroundSite {
            site: "reddit".to_owned(),
            object: Some(ObjectClass::AccountHome),
        }
    );
    // An ungroundable mention ("work") still routes to the ladder, which
    // fails it honestly — it is never silently grounded as a site.
    let plan = funnel_plan("open my work profile", false, Some("www.reddit.com"));
    assert_eq!(
        plan.decision,
        FunnelDecision::GroundSite {
            site: "work".to_owned(),
            object: Some(ObjectClass::AccountHome),
        }
    );
}

#[test]
fn siteless_objectless_prompt_reaches_tier2b() {
    // No site, no usable object: the funnel's last resort is the fenced
    // Tier 2B parser shot on the cleaned prompt — never a search fallback.
    let plan = funnel_plan("open it", false, None);
    assert_eq!(plan.decision, FunnelDecision::AskParser);
    assert!(!plan.cleaned.is_empty());
}

#[test]
fn settle_contract_fails_search_shaped_mismatch() {
    // Work item D: site slot `x`, landed on a search-shaped google.com
    // URL — the settle contract reports mismatch. FAILED, never Completed.
    let landed = url::Url::parse("https://www.google.com/search?q=x").unwrap();
    assert!(
        !funnel_landing_matches("x", &landed),
        "search-shaped landing on a non-matching domain is a miss"
    );
    // An ordinary domain mismatch fails too.
    let landed = url::Url::parse("https://www.bing.com/").unwrap();
    assert!(!funnel_landing_matches("x", &landed));
    // The site itself matches — path and query are the site's business.
    let landed = url::Url::parse("https://x.com/search?q=shoes").unwrap();
    assert!(funnel_landing_matches("x", &landed));
    // The single blessed alias still matches.
    let landed = url::Url::parse("https://mail.google.com/mail/u/0/").unwrap();
    assert!(funnel_landing_matches("gmail", &landed));
    // Empty slots never match.
    let landed = url::Url::parse("https://www.reddit.com/").unwrap();
    assert!(!funnel_landing_matches("", &landed));
}

#[test]
fn site_host_match_is_alias_aware() {
    // The one rule shared by the directory veto, the funnel's
    // already-on-origin check, and the settle contract.
    assert!(site_matches_host("reddit", "www.reddit.com"));
    assert!(site_matches_host("reddit", "reddit.com"));
    assert!(site_matches_host("gmail", "mail.google.com"));
    assert!(!site_matches_host("gmail", "gmail.com"));
    assert!(!site_matches_host("reddit", "www.google.com"));
    assert!(!site_matches_host("", "www.reddit.com"));
    assert!(!site_matches_host("reddit", ""));
}
