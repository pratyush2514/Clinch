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

    /// Adopt an approved drift proposal for one semantic step: the previous
    /// intent JSON lands in `signature_history` first (rollback by replay),
    /// then the step takes the new signature and the whole playbook
    /// re-validates before the upsert. Explicit-approval only — drift
    /// detection itself never writes; unknown ids, out-of-range steps,
    /// legacy steps, and invalid replacements all fail closed.
    ///
    /// # Errors
    /// Returns not-found, invalid-definition, serialization, or database
    /// errors.
    pub async fn update_playbook_signature(
        &self,
        playbook_id: &str,
        step_index: usize,
        new_signature: &macro_engine::SemanticIntent,
    ) -> Result<(), StoreError> {
        let mut playbook = self.load_playbook(playbook_id).await?;
        let step = playbook
            .steps
            .get_mut(step_index)
            .ok_or(StoreError::Invalid(crate::schema::SchemaError::Invalid))?;
        let crate::schema::Step::Semantic { intent } = step else {
            return Err(StoreError::Invalid(crate::schema::SchemaError::Invalid));
        };
        let previous_json = serde_json::to_string(&intent)?;
        *intent = new_signature.clone();
        playbook.validate()?;
        let step_index = i64::try_from(step_index)
            .map_err(|_| StoreError::Invalid(crate::schema::SchemaError::Invalid))?;
        sqlx::query(
            "INSERT INTO signature_history(playbook_id, step_index, previous_json) VALUES(?, ?, ?)",
        )
        .bind(playbook_id)
        .bind(step_index)
        .bind(&previous_json)
        .execute(&self.pool)
        .await?;
        self.save_playbook(&playbook).await?;
        Ok(())
    }

    /// Set a playbook's navigation pre-condition: runs must hold this URL
    /// before grounding. Validates shape (absolute `http(s)` with a host)
    /// and playbook existence first; unknown ids and malformed URLs fail
    /// closed without writing.
    ///
    /// # Errors
    /// Returns not-found, invalid-definition, or database errors.
    pub async fn set_entry_url(
        &self,
        playbook_id: &str,
        entry_url: &str,
    ) -> Result<(), StoreError> {
        let valid = entry_url.len() <= 2048
            && url::Url::parse(entry_url).is_ok_and(|url| {
                matches!(url.scheme(), "https" | "http") && url.host_str().is_some()
            });
        if !valid {
            return Err(StoreError::Invalid(crate::schema::SchemaError::Invalid));
        }
        self.load_playbook(playbook_id).await?;
        sqlx::query(
            "INSERT INTO entry_urls(playbook_id, entry_url, updated_at) VALUES(?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(playbook_id) DO UPDATE SET entry_url=excluded.entry_url, updated_at=CURRENT_TIMESTAMP",
        )
        .bind(playbook_id)
        .bind(entry_url)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Read a playbook's navigation pre-condition, if one was set. Unknown
    /// ids read back as unset rather than erroring: no constraint either way.
    ///
    /// # Errors
    /// Returns database errors.
    pub async fn entry_url(&self, playbook_id: &str) -> Result<Option<String>, StoreError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT entry_url FROM entry_urls WHERE playbook_id = ?")
                .bind(playbook_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(url,)| url))
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

    /// Journal a started run. Telemetry lives beside execution, never inside
    /// it: callers record start, run, then record finish. Unknown ids on
    /// finish are ignored — a restarted app must not crash on stale rows.
    ///
    /// # Errors
    /// Returns database errors.
    pub async fn record_run_start(
        &self,
        id: &str,
        playbook_id: Option<&str>,
        kind: RunKind,
        total_steps: usize,
    ) -> Result<(), StoreError> {
        let total_steps = i64::try_from(total_steps)
            .map_err(|_| StoreError::Invalid(crate::schema::SchemaError::Invalid))?;
        sqlx::query(
            "INSERT INTO runs(id, playbook_id, kind, status, total_steps) VALUES(?, ?, ?, 'running', ?)",
        )
        .bind(id)
        .bind(playbook_id)
        .bind(kind.as_str())
        .bind(total_steps)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Close a journaled run with its terminal status. `completed_at` is
    /// stamped by `SQLite`, so writer clocks never skew the timeline.
    ///
    /// # Errors
    /// Returns database errors.
    pub async fn record_run_finish(
        &self,
        id: &str,
        status: &str,
        completed_steps: usize,
    ) -> Result<(), StoreError> {
        let completed_steps = i64::try_from(completed_steps)
            .map_err(|_| StoreError::Invalid(crate::schema::SchemaError::Invalid))?;
        sqlx::query(
            "UPDATE runs SET status = ?, completed_steps = ?, completed_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(status)
        .bind(completed_steps)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Run totals grouped by status for POC metrics.
    ///
    /// # Errors
    /// Returns database errors.
    pub async fn run_stats(&self) -> Result<Vec<(String, i64)>, StoreError> {
        Ok(
            sqlx::query_as("SELECT status, COUNT(*) FROM runs GROUP BY status")
                .fetch_all(&self.pool)
                .await?,
        )
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
    // Run journal for POC metrics (reuse rates, sync outcomes). Additive and
    // idempotent like every table here: existing databases gain it on next
    // open, no ALTER or data migration involved. `completed_at` stays NULL
    // while a run is in flight, so crashes read as interrupted, not silent.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS runs (id TEXT PRIMARY KEY, playbook_id TEXT, kind TEXT NOT NULL, status TEXT NOT NULL, total_steps INTEGER NOT NULL, completed_steps INTEGER NOT NULL DEFAULT 0, started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, completed_at TEXT)",
    )
    .execute(&pool)
    .await?;
    // Signature history for approved self-healing: every adopted signature
    // pushes its predecessor here first, so any drift update rolls back by
    // replaying the latest row. Same additive story — no migration.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS signature_history (id INTEGER PRIMARY KEY AUTOINCREMENT, playbook_id TEXT NOT NULL, step_index INTEGER NOT NULL, previous_json TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
    )
    .execute(&pool)
    .await?;
    // Per-playbook navigation pre-conditions: the entry URL a run must hold
    // before grounding. Keyed by playbook row id, upserted on set, absent
    // when unset — steps without entries resolve exactly as before.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS entry_urls (playbook_id TEXT PRIMARY KEY, entry_url TEXT NOT NULL, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
    )
    .execute(&pool)
    .await?;
    Ok(pool)
}

/// Whether a journaled run came from a stored playbook or an ad-hoc intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Saved,
    Ephemeral,
}

impl RunKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Saved => "saved",
            Self::Ephemeral => "ephemeral",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PlaybookStore, RunKind};
    use crate::schema::{Playbook, Step};
    use macro_engine::SemanticIntent;

    fn reports_playbook() -> Result<Playbook, Box<dyn std::error::Error>> {
        Ok(Playbook::new(
            "reports".into(),
            url::Url::parse("https://portal.example.com/")?,
            vec![Step::Semantic {
                intent: SemanticIntent {
                    role: "button".into(),
                    label_query: "Pay now".into(),
                    container_query: None,
                    raw_prompt: String::new(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: false,
                    entry_url: None,
                    primary_target_noun: None,
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
        let playbook = reports_playbook()?;
        let id = store.save_playbook(&playbook).await?;
        let revived = store.load_playbook(&id).await?;
        assert_eq!(revived, playbook);
        let listed = store.list_playbooks().await?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].name, "reports");
        assert_eq!(listed[0].portal_url, "https://portal.example.com/");
        assert_eq!(listed[0].step_count, 1);
        assert!(!listed[0].updated_at.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn signature_drift_emits_repair_event_without_db_mutation()
    -> Result<(), Box<dyn std::error::Error>> {
        use browser_driver::AxElement;
        use macro_engine::{ResolveOutcome, SemanticIntent};
        let dir = tempfile::tempdir()?;
        let pool = super::initialize(&dir.path().join("drift.db")).await?;
        let store = PlaybookStore::new(pool.clone());
        // Saved intent labels the control "Pay now"; the live portal shows
        // "Pay Now!" — same control, drifted signature.
        let id = store
            .save_playbook(&Playbook::new(
                "drift".into(),
                url::Url::parse("https://portal.example.com/")?,
                vec![Step::Semantic {
                    intent: SemanticIntent {
                        role: "button".into(),
                        label_query: "Pay now".into(),
                        container_query: None,
                        raw_prompt: String::new(),
                        ordinal_index: None,
                        is_last: false,
                        is_plural: false,
                        entry_url: None,
                        primary_target_noun: None,
                    },
                }],
            )?)
            .await?;
        let live = vec![AxElement {
            backend_node_id: 1,
            role: "button".into(),
            name: "Pay Now!".into(),
            description: String::new(),
            container_text: Vec::new(),
            landmark: None,
        }];
        let stored = store.load_playbook(&id).await?;
        let crate::schema::Step::Semantic { intent } = &stored.steps[0] else {
            panic!("semantic step stored");
        };
        // The drift path returns its repair event and touches nothing.
        let ResolveOutcome::Drift { detail, .. } = macro_engine::resolve_with_drift(&live, intent)
        else {
            panic!("drift detected, not silence");
        };
        assert!(
            detail.old_signature.contains("pay now"),
            "{}",
            detail.old_signature
        );
        assert!(
            detail.new_signature.contains("pay now!"),
            "{}",
            detail.new_signature
        );
        assert_eq!(store.load_playbook(&id).await?.steps, stored.steps);
        // Explicit approval adopts the proposal; the predecessor lands in
        // history first for rollback.
        let mut approved = intent.clone();
        approved.label_query = "Pay Now!".into();
        store.update_playbook_signature(&id, 0, &approved).await?;
        let revived = store.load_playbook(&id).await?;
        let crate::schema::Step::Semantic { intent: updated } = &revived.steps[0] else {
            panic!("semantic step kept");
        };
        assert_eq!(updated.label_query, "Pay Now!");
        let history: Vec<(String,)> =
            sqlx::query_as("SELECT previous_json FROM signature_history WHERE playbook_id = ?")
                .bind(&id)
                .fetch_all(&pool)
                .await?;
        assert_eq!(history.len(), 1);
        assert!(history[0].0.contains("Pay now"), "{}", history[0].0);
        // Unknown ids, out-of-range steps, and oversized replacements fail
        // closed without writing history.
        assert!(
            store
                .update_playbook_signature("9999", 0, &approved)
                .await
                .is_err()
        );
        assert!(
            store
                .update_playbook_signature(&id, 7, &approved)
                .await
                .is_err()
        );
        let mut invalid = approved.clone();
        invalid.label_query = String::new();
        assert!(
            store
                .update_playbook_signature(&id, 0, &invalid)
                .await
                .is_err()
        );
        let history: Vec<(String,)> =
            sqlx::query_as("SELECT previous_json FROM signature_history WHERE playbook_id = ?")
                .bind(&id)
                .fetch_all(&pool)
                .await?;
        assert_eq!(history.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn entry_url_round_trips_validated_preconditions()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store) = store().await?;
        let id = store.save_playbook(&reports_playbook()?).await?;
        // Unset reads back as no constraint.
        assert_eq!(store.entry_url(&id).await?, None);
        assert_eq!(store.entry_url("9999").await?, None);
        // Valid entries persist and upsert.
        store
            .set_entry_url(&id, "https://portal.example.com/invoices")
            .await?;
        assert_eq!(
            store.entry_url(&id).await?.as_deref(),
            Some("https://portal.example.com/invoices")
        );
        store
            .set_entry_url(&id, "https://portal.example.com/settings")
            .await?;
        assert_eq!(
            store.entry_url(&id).await?.as_deref(),
            Some("https://portal.example.com/settings")
        );
        // Malformed URLs and unknown playbooks fail closed; the stored
        // entry survives every rejection.
        assert!(store.set_entry_url(&id, "not a url").await.is_err());
        assert!(
            store
                .set_entry_url(&id, "ftp://portal.example.com/x")
                .await
                .is_err()
        );
        assert!(
            store
                .set_entry_url("9999", "https://portal.example.com/")
                .await
                .is_err()
        );
        assert_eq!(
            store.entry_url(&id).await?.as_deref(),
            Some("https://portal.example.com/settings")
        );
        Ok(())
    }

    #[tokio::test]
    async fn save_is_idempotent_per_name() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store) = store().await?;
        let id = store.save_playbook(&reports_playbook()?).await?;
        // Re-saving the same name replaces the row instead of duplicating it.
        let mut updated = reports_playbook()?;
        updated.steps.push(updated.steps[0].clone());
        let same_id = store.save_playbook(&updated).await?;
        assert_eq!(same_id, id);
        let listed = store.list_playbooks().await?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].step_count, 2);
        Ok(())
    }

    #[tokio::test]
    async fn run_journal_tracks_start_to_terminal_state() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_dir, store) = store().await?;
        let id = store.save_playbook(&reports_playbook()?).await?;
        store
            .record_run_start("run-1", Some(&id), RunKind::Saved, 3)
            .await?;
        // In-flight runs read back as running with no completion timestamp.
        let stats = store.run_stats().await?;
        assert_eq!(stats, vec![("running".to_owned(), 1)]);
        store.record_run_finish("run-1", "completed", 3).await?;
        let stats = store.run_stats().await?;
        assert_eq!(stats, vec![("completed".to_owned(), 1)]);
        // Unknown ids on finish are ignored, never an error.
        store.record_run_finish("no-such-run", "failed", 0).await?;
        // Ephemeral runs carry no playbook row.
        store
            .record_run_start("run-2", None, RunKind::Ephemeral, 1)
            .await?;
        store.record_run_finish("run-2", "failed", 0).await?;
        let mut stats = store.run_stats().await?;
        stats.sort_unstable();
        assert_eq!(
            stats,
            vec![("completed".to_owned(), 1), ("failed".to_owned(), 1),]
        );
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
        let mut bad = reports_playbook()?;
        bad.name.clear();
        assert!(store.save_playbook(&bad).await.is_err());
        assert!(store.list_playbooks().await?.is_empty());
        // A tampered row fails the listing closed instead of hiding a workflow.
        sqlx::query("INSERT INTO playbooks(name, portal_url, steps_json) VALUES('tampered', 'https://portal.example.com/', 'not-json')")
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
