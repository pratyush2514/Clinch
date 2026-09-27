#![deny(unsafe_code)]
use crate::{EngineError, Task, TaskId};
use sqlx::SqlitePool;

/// Initialize the task store schema.
///
/// # Errors
///
/// Returns [`EngineError`] if the database cannot be initialized.
pub async fn initialize(pool: &SqlitePool) -> Result<(), EngineError> {
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

/// Persist a new task.
///
/// # Errors
///
/// Returns [`EngineError`] if the task cannot be written.
pub async fn create(pool: &SqlitePool, task: &mut Task) -> Result<(), EngineError> {
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

/// Write a task checkpoint.
///
/// # Errors
///
/// Returns [`EngineError`] on conflict or database failure.
pub async fn checkpoint(pool: &SqlitePool, task: &mut Task) -> Result<(), EngineError> {
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
