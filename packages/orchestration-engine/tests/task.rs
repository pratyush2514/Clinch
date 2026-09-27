//! Integration tests for `orchestration_engine::task`.
//!
//! Moved out of `src/task.rs` so the main source stays test-free.

use browser_driver::ActionOutput;
use orchestration_engine::EngineError;
use orchestration_engine::TaskRequest;
use orchestration_engine::task::*;
#[test]
fn transitions_reject_skipping_repeating_and_false_completion() -> Result<(), EngineError> {
    let request: TaskRequest = serde_json::from_str(
        r#"{"workflow":"reports","portalUrl":"https://example.com","linkSelector":null,"downloadSelector":"a.report"}"#,
    )?;
    let mut task = Task::new("reports".into(), request.plan()?, RunMode::Record);
    assert!(task.start_step(1).is_err());
    assert!(task.finish().is_err());
    assert!(task.start_step(99).is_err());
    task.start_step(0)?;
    assert!(task.start_step(0).is_err());
    task.complete_step(0, ActionOutput::default(), 1)?;
    assert!(task.complete_step(0, ActionOutput::default(), 1).is_err());
    assert!(task.finish().is_err());
    task.start_step(1)?;
    task.fail_step(1, 2, FailureReason::Browser)?;
    assert!(task.start_step(1).is_err());
    assert!(task.finish().is_err());
    Ok(())
}
