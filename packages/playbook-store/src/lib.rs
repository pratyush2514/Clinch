#![deny(unsafe_code)]
//! Phase 0 local database initialization plus the versioned Playbook schema.
pub mod schema;
pub use schema::{Playbook, SCHEMA_VERSION, SchemaError, Step};
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
    #[error("Invalid playbook definition")]
    Invalid(#[from] crate::schema::SchemaError),
    #[error("Playbook data is invalid")]
    Json(#[from] serde_json::Error),
    #[error("Playbook not found")]
    NotFound,
}

/// Summary row for the workflow list: identity and shape, never secrets.
/// Steps carry no credentials by construction (selectors plus intents).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybookSummary {
    pub id: String,
    pub name: String,
    pub portal_url: String,
    pub step_count: usize,
    pub updated_at: String,
}

/// SQLite-backed Playbook repository. Shares the application's single WAL
/// pool; every write validates first, so stored rows always re-validate.
#[derive(Clone)]
pub struct PlaybookStore {
    pool: SqlitePool,
}

impl PlaybookStore {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a new playbook or replace the row with the same name (upsert):
    /// saving is idempotent per name, and every save re-validates, so a
    /// stored row can never be unrunnable. Returns the row id as text.
    ///
    /// # Errors
    /// Returns definition, serialization, or database errors.
    pub async fn save_playbook(
        &self,
        playbook: &crate::schema::Playbook,
    ) -> Result<String, StoreError> {
        playbook.validate()?;
        let steps = serde_json::to_string(&playbook.steps)?;
        sqlx::query(
            "INSERT INTO playbooks(name, portal_url, steps_json, updated_at) VALUES(?, ?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(name) DO UPDATE SET portal_url=excluded.portal_url, steps_json=excluded.steps_json, updated_at=CURRENT_TIMESTAMP",
        )
        .bind(&playbook.name)
        .bind(playbook.origin.as_str())
        .bind(&steps)
        .execute(&self.pool)
        .await?;
        let id: i64 = sqlx::query_scalar("SELECT id FROM playbooks WHERE name = ?")
            .bind(&playbook.name)
            .fetch_one(&self.pool)
            .await?;
        Ok(id.to_string())
    }

    /// Load and re-validate one stored playbook. Unknown or malformed ids
    /// report [`StoreError::NotFound`]; corrupt rows fail closed.
    ///
    /// # Errors
    /// Returns not-found, definition, serialization, or database errors.
    pub async fn load_playbook(&self, id: &str) -> Result<crate::schema::Playbook, StoreError> {
        let row_id: i64 = id.parse().map_err(|_| StoreError::NotFound)?;
        let row: Option<(String, String, String)> =
            sqlx::query_as("SELECT name, portal_url, steps_json FROM playbooks WHERE id = ?")
                .bind(row_id)
                .fetch_optional(&self.pool)
                .await?;
        let (name, portal_url, steps_json) = row.ok_or(StoreError::NotFound)?;
        let origin = url::Url::parse(&portal_url)
            .map_err(|_| StoreError::Invalid(crate::schema::SchemaError::Invalid))?;
        let steps: Vec<crate::schema::Step> = serde_json::from_str(&steps_json)?;
        let playbook = crate::schema::Playbook {
            version: crate::schema::SCHEMA_VERSION,
            name,
            origin,
            steps,
        };
        playbook.validate()?;
        Ok(playbook)
    }

    /// Newest-first summaries for the workflow list. A single corrupt row
    /// fails the listing closed rather than silently hiding a workflow.
    ///
    /// # Errors
    /// Returns serialization or database errors.
    pub async fn list_playbooks(&self) -> Result<Vec<PlaybookSummary>, StoreError> {
        let rows: Vec<(i64, String, String, String, String)> = sqlx::query_as(
            "SELECT id, name, portal_url, steps_json, updated_at FROM playbooks ORDER BY updated_at DESC, id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(id, name, portal_url, steps_json, updated_at)| {
                let steps: Vec<crate::schema::Step> = serde_json::from_str(&steps_json)?;
                Ok(PlaybookSummary {
                    id: id.to_string(),
                    name,
                    portal_url,
                    step_count: steps.len(),
                    updated_at,
                })
            })
            .collect()
    }
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
    // Single-schema store (no migrations yet): validated Playbook envelopes
    // land here via `PlaybookStore`; rows always re-validate on read.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS playbooks (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE, portal_url TEXT NOT NULL, steps_json TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
    )
    .execute(&pool)
    .await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::PlaybookStore;
    use crate::schema::{Playbook, Step};
    use macro_engine::SemanticIntent;

    fn bills_playbook() -> Result<Playbook, Box<dyn std::error::Error>> {
        Ok(Playbook::new(
            "bills".into(),
            url::Url::parse("https://billing.example.com/")?,
            vec![Step::Semantic {
                intent: SemanticIntent {
                    role: "button".into(),
                    label_query: "Pay now".into(),
                },
            }],
        )?)
    }

    async fn store() -> Result<(tempfile::TempDir, PlaybookStore), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let pool = super::initialize(&dir.path().join("clinch.db")).await?;
        Ok((dir, PlaybookStore::new(pool)))
    }

    #[tokio::test]
    async fn save_load_and_list_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store) = store().await?;
        let playbook = bills_playbook()?;
        let id = store.save_playbook(&playbook).await?;
        let revived = store.load_playbook(&id).await?;
        assert_eq!(revived, playbook);
        let listed = store.list_playbooks().await?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].name, "bills");
        assert_eq!(listed[0].portal_url, "https://billing.example.com/");
        assert_eq!(listed[0].step_count, 1);
        assert!(!listed[0].updated_at.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn save_is_idempotent_per_name() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store) = store().await?;
        let id = store.save_playbook(&bills_playbook()?).await?;
        // Re-saving the same name replaces the row instead of duplicating it.
        let mut updated = bills_playbook()?;
        updated.steps.push(updated.steps[0].clone());
        let same_id = store.save_playbook(&updated).await?;
        assert_eq!(same_id, id);
        let listed = store.list_playbooks().await?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].step_count, 2);
        Ok(())
    }

    #[tokio::test]
    async fn unknown_ids_and_invalid_definitions_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store) = store().await?;
        assert!(matches!(
            store.load_playbook("999").await,
            Err(super::StoreError::NotFound)
        ));
        assert!(matches!(
            store.load_playbook("not-an-id").await,
            Err(super::StoreError::NotFound)
        ));
        let mut bad = bills_playbook()?;
        bad.name.clear();
        assert!(store.save_playbook(&bad).await.is_err());
        assert!(store.list_playbooks().await?.is_empty());
        // A tampered row fails the listing closed instead of hiding a workflow.
        sqlx::query("INSERT INTO playbooks(name, portal_url, steps_json) VALUES('tampered', 'https://billing.example.com/', 'not-json')")
            .execute(&store.pool)
            .await?;
        assert!(store.list_playbooks().await.is_err());
        assert!(store.load_playbook("1").await.is_err());
        Ok(())
    }

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
