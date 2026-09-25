//! Funnel dispatcher (work item B) service tests: the claim gate, the
//! already-there completion, and the Tier 2B honest miss — all without a
//! browser or network. In-page pursuit arms need a live browser and are
//! covered by the engine planner tests plus live acceptance.

use clinch_desktop::{AppError, AppService, PlaybookEvent};
use macro_engine::SemanticIntent;

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
