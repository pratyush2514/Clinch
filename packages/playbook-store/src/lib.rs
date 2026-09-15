#![deny(unsafe_code)]
//! Phase 0 local database initialization; Playbook schemas arrive in later phases.
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{path::Path, time::Duration};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("Local storage directory is unavailable")]
    Directory(#[from] std::io::Error),
    #[error("Local database initialization failed")]
    Database(#[from] sqlx::Error),
}

/// Open the single application pool in WAL mode. Stores metadata only.
///
/// # Errors
/// Returns directory or database errors.
pub async fn initialize(path: &Path) -> Result<SqlitePool, StoreError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS session_events (id INTEGER PRIMARY KEY, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, outcome TEXT NOT NULL)")
        .execute(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn initializes_wal_idempotently() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("clinch.db");
        let pool = super::initialize(&path).await?;
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await?;
        assert_eq!(mode, "wal");
        pool.close().await;
        let pool = super::initialize(&path).await?;
        pool.close().await;
        Ok(())
    }
}
