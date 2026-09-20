#![deny(unsafe_code)]
use crate::{EngineError, Task, TaskId};
use sqlx::SqlitePool;

pub(crate) async fn initialize(pool: &SqlitePool) -> Result<(), EngineError> {
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(pool)
        .await?;
    if mode != "wal" {
        return Err(EngineError::Invalid);
    }
    sqlx::query("CREATE TABLE IF NOT EXISTS tasks (id INTEGER PRIMARY KEY AUTOINCREMENT, revision INTEGER NOT NULL, snapshot TEXT NOT NULL)").execute(pool).await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS task_checkpoints (task_id INTEGER NOT NULL REFERENCES tasks(id), revision INTEGER NOT NULL, snapshot TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY(task_id, revision))").execute(pool).await?;
    Ok(())
}

pub(crate) async fn create(pool: &SqlitePool, task: &mut Task) -> Result<(), EngineError> {
    let mut transaction = pool.begin().await?;
    let id = sqlx::query("INSERT INTO tasks(revision,snapshot) VALUES(0,'{}')")
        .execute(&mut *transaction)
        .await?
        .last_insert_rowid();
    let mut next = task.clone();
    next.id = TaskId(id);
    let json = serde_json::to_string(&next)?;
    sqlx::query("UPDATE tasks SET snapshot=? WHERE id=?")
        .bind(&json)
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("INSERT INTO task_checkpoints(task_id,revision,snapshot) VALUES(?,0,?)")
        .bind(id)
        .bind(json)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    *task = next;
    Ok(())
}

pub(crate) async fn checkpoint(pool: &SqlitePool, task: &mut Task) -> Result<(), EngineError> {
    let mut next = task.clone();
    next.revision = task.revision.checked_add(1).ok_or(EngineError::Invalid)?;
    let json = serde_json::to_string(&next)?;
    let mut transaction = pool.begin().await?;
    let changed = sqlx::query("UPDATE tasks SET revision=?,snapshot=? WHERE id=? AND revision=?")
        .bind(next.revision)
        .bind(&json)
        .bind(task.id.0)
        .bind(task.revision)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
    if changed != 1 {
        return Err(EngineError::Conflict);
    }
    sqlx::query("INSERT INTO task_checkpoints(task_id,revision,snapshot) VALUES(?,?,?)")
        .bind(task.id.0)
        .bind(next.revision)
        .bind(json)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    *task = next;
    Ok(())
}

pub(crate) async fn load(pool: &SqlitePool, id: TaskId) -> Result<Task, EngineError> {
    let json: String = sqlx::query_scalar("SELECT snapshot FROM tasks WHERE id=?")
        .bind(id.0)
        .fetch_one(pool)
        .await?;
    Ok(serde_json::from_str(&json)?)
}

pub(crate) async fn unfinished(pool: &SqlitePool) -> Result<Vec<Task>, EngineError> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT snapshot FROM tasks WHERE json_extract(snapshot,'$.state') IN ('planned','running')").fetch_all(pool).await?;
    rows.iter()
        .map(|json| serde_json::from_str(json).map_err(EngineError::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, RunMode, StepState, TaskRequest, TaskState};
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
}
