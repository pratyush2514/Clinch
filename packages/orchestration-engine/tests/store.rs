//! Integration tests for `orchestration_engine::store`.
//!
//! Moved out of `src/store.rs` so the main source stays test-free.

use orchestration_engine::store::*;
use orchestration_engine::{Engine, EngineError, RunMode, StepState, Task, TaskRequest, TaskState};
#[tokio::test]
async fn checkpoints_survive_reopen_and_recover_without_reexecution()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("test.db");
    let pool = playbook_store::initialize(&path).await?;
    initialize(&pool).await?;
    let request: TaskRequest = serde_json::from_str(
        r#"{"workflow":"reports","portalUrl":"https://example.com","linkSelector":null,"downloadSelector":"a.report"}"#,
    )?;
    let mut task = Task::new("reports".into(), request.plan()?, RunMode::Record);
    create(&pool, &mut task).await?;
    let mut stale = task.clone();
    task.start_step(0)?;
    checkpoint(&pool, &mut task).await?;
    assert!(matches!(
        checkpoint(&pool, &mut stale).await,
        Err(EngineError::Conflict)
    ));
    pool.close().await;
    let pool = playbook_store::initialize(&path).await?;
    let engine = Engine::new(pool.clone()).await?;
    engine.recover().await?;
    let recovered = engine.load(task.id).await?;
    assert_eq!(recovered.state, TaskState::Interrupted);
    assert_eq!(recovered.plan.steps[0].state, StepState::Interrupted);
    assert_eq!(recovered.plan.steps[1].state, StepState::Pending);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM task_checkpoints")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 3);
    engine.recover().await?;
    assert_eq!(engine.load(task.id).await?.revision, 2);
    pool.close().await;
    Ok(())
}
