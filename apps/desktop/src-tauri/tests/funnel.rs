//! Funnel dispatcher (work item B) service tests: the claim gate, the
//! already-there completion, and the Tier 2B honest miss — all without a
//! browser or network. In-page pursuit arms need a live browser and are
//! covered by the engine planner tests plus live acceptance.
//!
//! Work item B4 adds the dispatch-preemption regression: a funnel-claimed
//! prompt must journal the funnel's slots before the old proposal
//! machinery runs — and the old machinery must not run at all.

use clinch_desktop::{AppError, AppService, PlaybookEvent};
use macro_engine::SemanticIntent;
use orchestration_engine::{FunnelDecision, ObjectClass, funnel_plan};

fn ad_hoc_intent(prompt: &str, is_plural: bool) -> SemanticIntent {
    SemanticIntent {
        role: "open".to_owned(),
        label_query: prompt.to_owned(),
        container_query: None,
        raw_prompt: prompt.to_owned(),
        ordinal_index: None,
        is_last: false,
        is_plural,
        entry_url: None,
        primary_target_noun: None,
    }
}

#[tokio::test]
async fn funnel_already_there_completes_without_site_search() {
    // "open reddit" on reddit.com: AlreadyOnOrigin with no object — the
    // portal is already the answer, so the run completes without pursuing
    // anything. This arm takes no SiteSearchClient: the ladder is never
    // constructed, so no directory call is possible. Settle runs fail-open
    // with no browser attached.
    let dir = tempfile::tempdir().unwrap();
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.unwrap();
    let portal = url::Url::parse("https://www.reddit.com/").unwrap();
    let mut emit = |_: PlaybookEvent| {};
    let claimed = service
        .dispatch_funnel(
            "open reddit".to_owned(),
            &ad_hoc_intent("open reddit", false),
            Some(portal),
            &mut emit,
        )
        .await;
    let Some(result) = claimed else {
        panic!("the funnel must claim open-verb prompts");
    };
    let outcome = result.expect("already-there completes");
    // `DispatchOutcome` keeps its fields private; the serialized IPC shape
    // is the public contract.
    let value = serde_json::to_value(&outcome).unwrap();
    assert_eq!(value["result"]["status"], "completed");
}

#[tokio::test]
async fn funnel_tier2b_miss_is_honest_never_search() {
    // Unconfigured (declining) parser, siteless + objectless prompt: the
    // funnel's last resort asks "Which site should I open?" — no Tier-4
    // search fallback, no browser, no network.
    let dir = tempfile::tempdir().unwrap();
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.unwrap();
    let mut emit = |_: PlaybookEvent| {};
    let claimed = service
        .dispatch_funnel(
            "open it".to_owned(),
            &ad_hoc_intent("open it", false),
            None,
            &mut emit,
        )
        .await;
    let Some(result) = claimed else {
        panic!("the funnel must claim open-verb prompts");
    };
    match result {
        Err(AppError::InvalidInput(message)) => assert!(
            message.starts_with("Which site should I open?"),
            "honest miss, got: {message}"
        ),
        Err(other) => panic!("expected the honest miss, got: {other:?}"),
        Ok(_) => panic!("a siteless prompt must not complete"),
    }
}

#[tokio::test]
async fn funnel_declines_non_open_and_plural_prompts() {
    let dir = tempfile::tempdir().unwrap();
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.unwrap();
    // Retrieval verbs keep the existing pipeline: the funnel declines.
    let mut emit = |_: PlaybookEvent| {};
    let declined = service
        .dispatch_funnel(
            "download all my invoices from github".to_owned(),
            &ad_hoc_intent("download all my invoices from github", false),
            None,
            &mut emit,
        )
        .await;
    assert!(declined.is_none(), "non-open verbs are not funnel prompts");
    // Plurals keep the batch lane even when open-led.
    let mut emit = |_: PlaybookEvent| {};
    let declined = service
        .dispatch_funnel(
            "open all my tabs".to_owned(),
            &ad_hoc_intent("open all my tabs", true),
            None,
            &mut emit,
        )
        .await;
    assert!(declined.is_none(), "plural prompts are not funnel prompts");
}

/// Read the Session Activity backing store to assert on journal order.
/// `test_session_events` is `#[cfg(test)]` (unit tests only), so
/// integration tests open the database file directly.
async fn journal_lines(data_dir: &std::path::Path) -> Vec<String> {
    let url = format!("sqlite:{}", data_dir.join("clinch.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
    let rows: Vec<(String,)> = sqlx::query_as("SELECT outcome FROM session_events ORDER BY rowid")
        .fetch_all(&pool)
        .await
        .unwrap();
    rows.into_iter()
        .map(|(outcome,)| outcome)
        .filter(|line| !line.starts_with("startup_build:"))
        .collect()
}

#[tokio::test]
async fn funnel_preempts_old_proposal_in_adhoc_dispatch() {
    // Work item B4: "open settings on reddit" claims the funnel, so the
    // ad-hoc lane must journal the funnel's slots BEFORE any old
    // propose_entry_url / follow-up / search machinery runs — and the old
    // machinery must not run at all. No portal is connected and no browser
    // is attached, so the funnel takes the GroundSite arm; the run itself
    // fails for lack of a browser, but the journal order is what this
    // test pins. If the funnel gate ever moved below the old proposal,
    // the first line would be `route_resolution_miss:` or an old-style
    // `route_proposed:` line instead of `funnel_slots:`.
    let dir = tempfile::tempdir().unwrap();
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.unwrap();
    let mut emit = |_: PlaybookEvent| {};
    let _ = service
        .dispatch_natural_command("open settings on reddit".to_owned(), &mut emit)
        .await;
    let journal = journal_lines(dir.path()).await;
    assert!(!journal.is_empty(), "the dispatch must journal something");
    assert!(
        journal[0].starts_with("funnel_slots:"),
        "the funnel must run before any old proposal machinery, first line was: {}",
        journal[0]
    );
    for line in &journal {
        // The funnel's own route lines carry the `funnel` marker
        // (`route_proposed: funnel 'reddit' → …`); any other line with
        // these prefixes is the old proposal machinery running first.
        assert!(
            !line.starts_with("route_proposed:") || line.contains("funnel"),
            "old proposal machinery ran on a funnel-claimed prompt: {line}"
        );
        assert!(
            !line.starts_with("route_fallback:"),
            "old search fallback ran on a funnel-claimed prompt: {line}"
        );
        assert!(
            !line.starts_with("route_resolution_miss:"),
            "old resolution miss ran on a funnel-claimed prompt: {line}"
        );
    }
}

#[test]
fn screenshot_prompts_keep_their_funnel_decisions() {
    // Regression for the screenshot prompts behind work items B/B4: their
    // funnel decisions must not drift. These are pure planner checks (no
    // browser, no network); dispatch-level preemption is pinned by
    // `funnel_preempts_old_proposal_in_adhoc_dispatch`.
    let settings = funnel_plan("open settings on reddit", false, Some("www.reddit.com"));
    assert!(
        matches!(
            &settings.decision,
            FunnelDecision::AlreadyOnOrigin { site, object: Some(ObjectClass::Settings) }
            if site == "reddit"
        ),
        "got: {:?}",
        settings.decision
    );
    // The X prompts keep working through the funnel's GroundSite arm —
    // never the old search pipeline.
    for prompt in [
        "open the X for me and make sure ill do the login first",
        "i want you to open X for me",
        "can you open the X for me and make sure ill do the login first",
        "open X for me",
    ] {
        let plan = funnel_plan(prompt, false, None);
        assert!(
            matches!(
                &plan.decision,
                FunnelDecision::GroundSite { site, object: None } if site == "x"
            ),
            "prompt {prompt:?} got: {:?}",
            plan.decision
        );
    }
}
