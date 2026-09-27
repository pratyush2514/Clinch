//! Integration tests for the `orchestration_engine` crate root.
//!
//! Moved out of `src/lib.rs` so the main source stays test-free.

use browser_driver::Action;
use orchestration_engine::*;
use url::Url;
#[tokio::test]
async fn gate_blocks_and_consumes_only_matching_decisions() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = tempfile::tempdir()?;
    let pool = playbook_store::initialize(&dir.path().join("gate.db")).await?;
    let engine = Engine::new(pool.clone()).await?;
    let request = TaskRequest {
        workflow: "gate".into(),
        portal_url: Url::parse("https://example.com")?,
        link_selector: None,
        download_selector: "a.report".into(),
    };
    let mut task = Task::new("gate".into(), request.plan()?, RunMode::Record);
    task.id = TaskId(42);
    for approved in [false, true] {
        let mut emit = |event: TaskEvent| {
            let Some(gate) = event.approval.as_ref() else {
                panic!("Pending gate missing");
            };
            assert!(engine.decide(TaskId(41), gate.step_index, true).is_err());
        };
        let events = std::sync::Mutex::new(&mut emit);
        let consent = engine.consent(
            &task,
            0,
            Action::Submit {
                selector: "form".into(),
            },
            &events,
        );
        tokio::pin!(consent);
        tokio::select! {
            biased;
            _ = &mut consent => panic!("Gate resolved without a decision"),
            () = tokio::task::yield_now() => {}
        }
        engine.decide(task.id, 0, approved)?;
        assert!(engine.decide(task.id, 0, true).is_err());
        assert_eq!(consent.await, approved);
    }
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sentinel_decisions WHERE task_id=42")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count.0, 2);
    Ok(())
}

#[test]
fn task_plans_reject_paths_and_invalid_selectors() -> Result<(), Box<dyn std::error::Error>> {
    let mut request = TaskRequest {
        workflow: "reports".into(),
        portal_url: Url::parse("https://example.com/files")?,
        link_selector: None,
        download_selector: "a.report".into(),
    };
    assert_eq!(request.plan()?.steps.len(), 2);
    for workflow in ["../reports", "C:\\reports", "reports/name", "", "."] {
        request.workflow = workflow.into();
        assert!(request.plan().is_err());
    }
    request.workflow = "reports".into();
    request.link_selector = Some(String::new());
    assert!(request.plan().is_err());
    request.link_selector = None;
    request.portal_url = Url::parse("https://user:secret@example.com/")?;
    assert!(request.plan().is_err());
    Ok(())
}
