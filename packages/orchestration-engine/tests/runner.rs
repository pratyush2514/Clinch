//! Integration tests for `orchestration_engine::runner`.
//!
//! Moved out of `src/runner.rs` so the main source stays test-free.

use browser_driver::{AxElement, BrowserError};
use macro_engine::SemanticIntent;
use orchestration_engine::runner::*;

fn intent(plural: bool) -> SemanticIntent {
    SemanticIntent {
        role: "link".into(),
        label_query: "invoice".into(),
        container_query: None,
        raw_prompt: "download all my invoices".into(),
        ordinal_index: None,
        is_last: false,
        is_plural: plural,
        entry_url: None,
        primary_target_noun: Some("invoice".into()),
    }
}

fn candidate(id: i64, name: &str) -> AxElement {
    AxElement {
        backend_node_id: id,
        role: "link".into(),
        name: name.into(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: None,
    }
}

#[test]
fn terminal_states_cover_every_step_error() {
    assert_eq!(
        terminal_status(&StepError::ApprovalRequired),
        SequenceStatus::Denied
    );
    assert_eq!(
        terminal_status(&StepError::NoMatch(String::new())),
        SequenceStatus::Failed
    );
    assert_eq!(terminal_status(&StepError::Invalid), SequenceStatus::Failed);
    assert_eq!(
        terminal_status(&StepError::Browser(BrowserError::Timeout)),
        SequenceStatus::Failed
    );
    // An approved batch that drifted did act on the page, so it fails
    // rather than reporting a denial the user never gave.
    assert_eq!(
        terminal_status(&StepError::BatchHalted {
            reason: "UrlDriftDetected",
            clicks_completed: 2,
            failed_candidate_index: 2,
        }),
        SequenceStatus::Failed
    );
}

#[test]
fn halted_batches_report_progress_without_leaking_the_diverged_url() {
    // The drift URL stays out of every rendered string: it can carry
    // tokens, and step errors reach journals and UI copy.
    let error = StepError::BatchHalted {
        reason: "UrlDriftDetected",
        clicks_completed: 2,
        failed_candidate_index: 2,
    };
    let rendered = error.to_string();
    assert!(rendered.contains('2'), "progress is reported: {rendered}");
    assert!(!rendered.contains("http"), "no URL is rendered: {rendered}");
}

#[test]
fn approval_requests_distinguish_batch_from_single_by_intent() {
    // Batch-ness is read off the intent, never inferred from candidate
    // emptiness, so a resolved-but-empty set cannot pass as single.
    let single = IntentApproval::single(intent(false));
    assert!(!single.is_batch());
    assert!(single.candidates.is_empty());
    let batch = IntentApproval::batch(
        intent(true),
        vec![candidate(1, "Download Aug"), candidate(2, "Download Sep")],
    );
    assert!(batch.is_batch());
    assert_eq!(batch.candidates.len(), 2);
    assert!(IntentApproval::batch(intent(true), Vec::new()).is_batch());
}

#[test]
fn event_and_outcome_shapes_match_the_ipc_contract() -> Result<(), Box<dyn std::error::Error>> {
    let event = SequenceEvent {
        step_index: 1,
        total_steps: 3,
        phase: SequencePhase::Running,
        highlight: None,
    };
    let json = serde_json::to_string(&event)?;
    assert!(json.contains("\"stepIndex\":1"));
    assert!(json.contains("\"phase\":\"running\""));
    let outcome = SequenceOutcome {
        completed_steps: 2,
        total_steps: 3,
        status: SequenceStatus::NeedsRepair,
        stopped_at: Some(2),
    };
    let json = serde_json::to_string(&outcome)?;
    assert!(json.contains("\"status\":\"needs_repair\""));
    assert!(json.contains("\"stoppedAt\":2"));
    Ok(())
}
