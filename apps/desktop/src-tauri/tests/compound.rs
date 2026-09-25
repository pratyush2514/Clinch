//! Compound-prompt dispatch: a prompt chaining two or more verb-led
//! actions ("log out me from the reddit and re-open the reddit") splits
//! deterministically and runs its segments sequentially through the same
//! single-prompt dispatch, sharing one journal run. No browser or network
//! — the segments fail honestly before any attach, and the journal order
//! is what these tests pin.
//!
//! Regression for the live bug: the same prompt used to navigate managed
//! Chromium to a visible Google results page and settle COMPLETED. Now
//! every segment journals in order, no search navigation happens, and a
//! segment that cannot run fails the whole dispatch instead of settling
//! COMPLETED.

use clinch_desktop::{AppError, AppService, PlaybookEvent};

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
async fn compound_prompt_splits_and_dispatches_segments_in_order() {
    // The exact live-bug prompt. No portal is connected and no browser is
    // attached, so segment 1 fails honestly before any launch; the run
    // stops there and never reports COMPLETED. What this test pins is the
    // deterministic decomposition in journal order and the absence of any
    // visible-search routing.
    let dir = tempfile::tempdir().unwrap();
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.unwrap();
    let mut emit = |_: PlaybookEvent| {};
    let result = service
        .dispatch_natural_command(
            "log out me from the reddit and re-open the reddit".to_owned(),
            &mut emit,
        )
        .await;
    let journal = journal_lines(dir.path()).await;
    assert_eq!(
        journal[0], "compound: 2 segments",
        "compound decomposition journals first, got: {journal:?}"
    );
    assert_eq!(
        journal[1], "compound_segment: 1/2 'log out me from the reddit'",
        "segment 1 journals in order, got: {journal:?}"
    );
    assert_eq!(
        journal[2], "compound_segment: 2/2 're-open the reddit'",
        "segment 2 journals in order, got: {journal:?}"
    );
    // No visible-search routing anywhere in the run.
    for line in &journal {
        assert!(
            !line.to_lowercase().contains("google.com"),
            "no search-engine navigation journaled: {line}"
        );
        assert!(
            !line.starts_with("route_fallback:"),
            "no search fallback journaled: {line}"
        );
    }
    // The run did not complete: segment 1 has no portal to run against,
    // so it fails honestly, and the compound stops at the first failure
    // instead of settling COMPLETED on a search page.
    match result {
        Err(AppError::InvalidInput(_) | AppError::WorkflowFailed(_)) => {}
        Err(other) => panic!("expected an honest failure, got: {other:?}"),
        Ok(outcome) => {
            let value = serde_json::to_value(&outcome).unwrap();
            assert_ne!(
                value["result"]["status"], "completed",
                "a partial compound run must never report completed"
            );
        }
    }
}

#[tokio::test]
async fn single_action_prompt_never_takes_the_compound_lane() {
    // A prompt with no coordinator is not compound: it dispatches as one
    // prompt and journals no compound lines at all.
    let dir = tempfile::tempdir().unwrap();
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.unwrap();
    let mut emit = |_: PlaybookEvent| {};
    let _ = service
        .dispatch_natural_command("download my report".to_owned(), &mut emit)
        .await;
    let journal = journal_lines(dir.path()).await;
    assert!(
        !journal.iter().any(|line| line.starts_with("compound")),
        "single prompts skip the compound lane, got: {journal:?}"
    );
}
