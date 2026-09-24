#![deny(unsafe_code)]
//! Phase 0 local database initialization plus the versioned Playbook schema.
pub mod schema;
pub use schema::{Playbook, SCHEMA_VERSION, SchemaError, Step};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{path::Path, time::Duration};

/// `SQLite` busy timeout: writers briefly wait on each other's WAL locks
/// instead of failing instantly under concurrent runs.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

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
    #[error("Invalid site shortcut: {0}")]
    Shortcut(String),
}

/// One user-saved site shortcut: the direct-open ladder's learned rung.
/// Names are stored normalized (lowercase, trimmed); the URL is always an
/// absolute `https` URL, validated on write.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SiteShortcut {
    pub name: String,
    pub url: String,
}

/// One remembered identity fact per (origin, goal class): the profile URL
/// a previous run revealed from the live page (or a connector), never
/// guessed. Lets a repeat "open my profile" become a single validated
/// navigation instead of another header hunt. `username` is optional —
/// some menus disclose only an href. No cookies, no credentials.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdentityMemory {
    pub origin: String,
    pub goal_class: String,
    pub username: Option<String>,
    pub href: String,
    pub source: String,
}

/// Normalize an origin into the memory keyspace: lowercase, trimmed.
/// Origins never carry interior whitespace, so no collapsing is needed.
/// Normalize an identity-memory origin key: lowercase, trimmed, and with
/// a single leading `www.` stripped — the same normalization the
/// same-site host check uses. `www.reddit.com` and `reddit.com` name one
/// site, so they must key one row for remember, recall, and forget.
fn normalize_identity_origin(origin: &str) -> String {
    let origin = origin.trim().to_lowercase();
    origin.strip_prefix("www.").unwrap_or(&origin).to_owned()
}

/// Normalize a shortcut name into the ladder's keyspace: lowercase,
/// trimmed, interior whitespace collapsed. Grammar target nouns are
/// already lowercase single tokens, so saves and lookups meet here.
fn normalize_shortcut_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
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
    /// Free-text memo, if one was saved. `#[serde(default)]` keeps older
    /// consumers parsing summaries that predate the field.
    #[serde(default)]
    pub description: Option<String>,
    /// Normalized prompt this workflow was learned from, if any. Command
    /// routing reads it to turn a repeated phrasing into an exact match
    /// instead of a token-overlap race. `#[serde(default)]` keeps older
    /// consumers parsing summaries that predate the field.
    #[serde(default)]
    pub prompt_key: Option<String>,
}

/// One `playbooks` row fetched by id: `(name, portal_url, steps_json,
/// description, prompt_key)`. Named so the column order stays reviewable in
/// one place as the table gains additive columns.
type PlaybookRow = (String, String, String, Option<String>, Option<String>);

/// One `playbooks` row as listed: [`PlaybookRow`] prefixed by `id` and
/// suffixed by `updated_at`, matching the summary projection.
type PlaybookListRow = (
    i64,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
);

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
            "INSERT INTO playbooks(name, portal_url, steps_json, description, prompt_key, updated_at) VALUES(?, ?, ?, ?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(name) DO UPDATE SET portal_url=excluded.portal_url, steps_json=excluded.steps_json, description=excluded.description, prompt_key=excluded.prompt_key, updated_at=CURRENT_TIMESTAMP",
        )
        .bind(&playbook.name)
        .bind(playbook.origin.as_str())
        .bind(&steps)
        .bind(playbook.description.as_deref())
        .bind(playbook.prompt_key.as_deref())
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
        let row: Option<PlaybookRow> = sqlx::query_as(
            "SELECT name, portal_url, steps_json, description, prompt_key FROM playbooks WHERE id = ?",
        )
        .bind(row_id)
        .fetch_optional(&self.pool)
        .await?;
        let (name, portal_url, steps_json, description, prompt_key) =
            row.ok_or(StoreError::NotFound)?;
        let origin = url::Url::parse(&portal_url)
            .map_err(|_| StoreError::Invalid(crate::schema::SchemaError::Invalid))?;
        let steps: Vec<crate::schema::Step> = serde_json::from_str(&steps_json)?;
        let playbook = crate::schema::Playbook {
            version: crate::schema::SCHEMA_VERSION,
            name,
            origin,
            steps,
            description,
            prompt_key,
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

    /// Save (or replace) a site shortcut: a user-chosen name → destination
    /// URL. The name is normalized before storage; the URL must be an
    /// absolute `https` URL — anything else is a [`StoreError::Shortcut`],
    /// so a typo can never land a broken rung in the direct-open ladder.
    ///
    /// # Errors
    /// Returns shortcut-validation or database errors.
    pub async fn set_site_shortcut(
        &self,
        name: &str,
        url: &str,
    ) -> Result<SiteShortcut, StoreError> {
        let name = normalize_shortcut_name(name);
        if name.is_empty() || name.len() > 64 {
            return Err(StoreError::Shortcut(
                "name must be 1-64 characters".to_owned(),
            ));
        }
        let parsed = url::Url::parse(url)
            .ok()
            .filter(|parsed| parsed.scheme() == "https" && parsed.has_host());
        let Some(parsed) = parsed else {
            return Err(StoreError::Shortcut(
                "URL must be an absolute https URL".to_owned(),
            ));
        };
        sqlx::query(
            "INSERT INTO site_shortcuts(name, url, updated_at) VALUES(?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(name) DO UPDATE SET url=excluded.url, updated_at=CURRENT_TIMESTAMP",
        )
        .bind(&name)
        .bind(parsed.as_str())
        .execute(&self.pool)
        .await?;
        Ok(SiteShortcut {
            name,
            url: parsed.as_str().to_owned(),
        })
    }

    /// Read one shortcut by name. Unknown names read back as unset rather
    /// than erroring: the ladder treats them as "no shortcut", not a fault.
    ///
    /// # Errors
    /// Returns database errors.
    pub async fn site_shortcut(&self, name: &str) -> Result<Option<String>, StoreError> {
        let name = normalize_shortcut_name(name);
        let row: Option<(String,)> =
            sqlx::query_as("SELECT url FROM site_shortcuts WHERE name = ?")
                .bind(&name)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(url,)| url))
    }

    /// Every saved shortcut, newest-name-first for the palette editor. A
    /// single corrupt row fails the listing closed rather than silently
    /// dropping a shortcut.
    ///
    /// # Errors
    /// Returns database errors.
    pub async fn list_site_shortcuts(&self) -> Result<Vec<SiteShortcut>, StoreError> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT name, url FROM site_shortcuts ORDER BY name")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(name, url)| SiteShortcut { name, url })
            .collect())
    }

    /// Delete a shortcut. Returns whether a row existed: deleting a name
    /// the user never saved is a no-op, not an error.
    ///
    /// # Errors
    /// Returns database errors.
    pub async fn delete_site_shortcut(&self, name: &str) -> Result<bool, StoreError> {
        let name = normalize_shortcut_name(name);
        let done = sqlx::query("DELETE FROM site_shortcuts WHERE name = ?")
            .bind(&name)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Remember a revealed identity fact: the profile URL a run read from
    /// the live page (or a connector) for this origin and goal class.
    /// Upserted — a fresh discovery replaces a stale one, never appends.
    /// The href must be an absolute `https` URL; anything else is rejected
    /// so a malformed read can never poison future runs.
    ///
    /// # Errors
    /// Returns validation or database errors.
    pub async fn remember_identity(
        &self,
        origin: &str,
        goal_class: &str,
        username: Option<&str>,
        href: &str,
        source: &str,
    ) -> Result<IdentityMemory, StoreError> {
        let origin = normalize_identity_origin(origin);
        let goal_class = goal_class.trim().to_lowercase();
        let source = source.trim().to_lowercase();
        if origin.is_empty() || goal_class.is_empty() || source.is_empty() {
            return Err(StoreError::Shortcut(
                "origin, goal class, and source must be non-empty".to_owned(),
            ));
        }
        let parsed = url::Url::parse(href)
            .ok()
            .filter(|parsed| parsed.scheme() == "https" && parsed.has_host());
        let Some(parsed) = parsed else {
            return Err(StoreError::Shortcut(
                "identity href must be an absolute https URL".to_owned(),
            ));
        };
        sqlx::query(
            "INSERT INTO identity_memory(origin, goal_class, username, href, source, seen_at) VALUES(?, ?, ?, ?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(origin, goal_class) DO UPDATE SET username=excluded.username, href=excluded.href, source=excluded.source, seen_at=CURRENT_TIMESTAMP",
        )
        .bind(&origin)
        .bind(&goal_class)
        .bind(username)
        .bind(parsed.as_str())
        .bind(&source)
        .execute(&self.pool)
        .await?;
        Ok(IdentityMemory {
            origin,
            goal_class,
            username: username.map(str::to_owned),
            href: parsed.as_str().to_owned(),
            source,
        })
    }

    /// Recall a remembered identity fact. Unknown origins read back as
    /// unset rather than erroring: the worker falls back to the live
    /// header read, exactly as before memory existed.
    ///
    /// # Errors
    /// Returns database errors.
    pub async fn recall_identity(
        &self,
        origin: &str,
        goal_class: &str,
    ) -> Result<Option<IdentityMemory>, StoreError> {
        let origin = normalize_identity_origin(origin);
        let goal_class = goal_class.trim().to_lowercase();
        let row: Option<(String, String, Option<String>, String, String)> = sqlx::query_as(
            "SELECT origin, goal_class, username, href, source FROM identity_memory WHERE origin = ? AND goal_class = ?",
        )
        .bind(&origin)
        .bind(&goal_class)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(origin, goal_class, username, href, source)| IdentityMemory {
                origin,
                goal_class,
                username,
                href,
                source,
            },
        ))
    }

    /// Forget every remembered identity fact for an origin. Called by the
    /// "Forget this site" flow alongside cookie revocation, so a signed-out
    /// origin can never be navigated from a stale remembered profile.
    /// Matches the host with or without a `www.` prefix, since the row may
    /// have been keyed by either form. Returns the number of rows removed.
    ///
    /// # Errors
    /// Returns database errors.
    pub async fn forget_identity_for_origin(&self, origin: &str) -> Result<u64, StoreError> {
        let origin = normalize_identity_origin(origin);
        // Match with or without a `www.` prefix: the row may have been
        // keyed by either form, and the forget call may name the other.
        let bare = origin.strip_prefix("www.").unwrap_or(&origin);
        let done =
            sqlx::query("DELETE FROM identity_memory WHERE origin = ? OR origin = 'www.' || ?")
                .bind(bare)
                .bind(bare)
                .execute(&self.pool)
                .await?;
        Ok(done.rows_affected())
    }

    /// Newest-first summaries for the workflow list. A single corrupt row
    /// fails the listing closed rather than silently hiding a workflow.
    ///
    /// # Errors
    /// Returns serialization or database errors.
    pub async fn list_playbooks(&self) -> Result<Vec<PlaybookSummary>, StoreError> {
        let rows: Vec<PlaybookListRow> = sqlx::query_as(
            "SELECT id, name, portal_url, steps_json, description, prompt_key, updated_at FROM playbooks ORDER BY updated_at DESC, id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(
                |(id, name, portal_url, steps_json, description, prompt_key, updated_at)| {
                    let steps: Vec<crate::schema::Step> = serde_json::from_str(&steps_json)?;
                    Ok(PlaybookSummary {
                        id: id.to_string(),
                        name,
                        portal_url,
                        step_count: steps.len(),
                        updated_at,
                        description,
                        prompt_key,
                    })
                },
            )
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
        .busy_timeout(BUSY_TIMEOUT);
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS session_events (id INTEGER PRIMARY KEY, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, outcome TEXT NOT NULL)")
        .execute(&pool).await?;
    // Single-schema store (no migrations yet): validated Playbook envelopes
    // land here via `PlaybookStore`; rows always re-validate on read.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS playbooks (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE, portal_url TEXT NOT NULL, steps_json TEXT NOT NULL, description TEXT, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
    )
    .execute(&pool)
    .await?;
    // First additive migration: memo column for databases created before
    // descriptions existed. PRAGMA-guarded so reopening is idempotent.
    let has_description: bool = sqlx::query_scalar(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('playbooks') WHERE name = 'description'",
    )
    .fetch_one(&pool)
    .await?;
    if !has_description {
        sqlx::query("ALTER TABLE playbooks ADD COLUMN description TEXT")
            .execute(&pool)
            .await?;
    }
    // Second additive migration: the learning loop's index. Nullable, so
    // every existing row stays valid and unkeyed — workflows saved before
    // prompts were remembered keep matching by name tokens exactly as
    // before. PRAGMA-guarded like the column above, so reopening is
    // idempotent.
    let has_prompt_key: bool = sqlx::query_scalar(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('playbooks') WHERE name = 'prompt_key'",
    )
    .fetch_one(&pool)
    .await?;
    if !has_prompt_key {
        sqlx::query("ALTER TABLE playbooks ADD COLUMN prompt_key TEXT")
            .execute(&pool)
            .await?;
    }
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
    // User-saved site shortcuts: the direct-open ladder's learned rung
    // (`open amazon` → the user's URL, no search involved). Names are
    // normalized at the Rust boundary — lowercase, trimmed, interior
    // whitespace collapsed — so saves and ladder lookups share one
    // keyspace regardless of SQLite collation. Upserted on set, absent
    // when unset; prompts without a saved shortcut resolve exactly as
    // before. Additive and idempotent like every table here: existing
    // databases gain it on next open.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS site_shortcuts (name TEXT PRIMARY KEY, url TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
    )
    .execute(&pool)
    .await?;
    // Remembered identity facts: one row per (origin, goal class) holding
    // the profile URL a run revealed from the live page. Same additive,
    // idempotent story — existing databases gain it on next open, and the
    // composite key keeps one origin from collecting stale duplicates.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS identity_memory (origin TEXT NOT NULL, goal_class TEXT NOT NULL, username TEXT, href TEXT NOT NULL, source TEXT NOT NULL, seen_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY (origin, goal_class))",
    )
    .execute(&pool)
    .await?;
    seed_default_playbooks(&pool).await?;
    Ok(pool)
}

/// Insert the proven default workflows on a fresh database, so resolution
/// tier 1 answers them instantly with no static route table in the picture.
///
/// Runs last, after every `CREATE`/`ALTER` above, so the `description`
/// column exists even on databases created before it did. `DO NOTHING` on
/// the name conflict makes reopening idempotent *and* non-destructive:
/// once a seed row exists, later opens never overwrite edits, adopted
/// signatures, or memos the user has since made to it.
///
/// # Errors
/// Returns definition, serialization, or database errors.
async fn seed_default_playbooks(pool: &SqlitePool) -> Result<(), StoreError> {
    for playbook in crate::schema::seeded_playbooks()? {
        let steps = serde_json::to_string(&playbook.steps)?;
        sqlx::query(
            "INSERT INTO playbooks(name, portal_url, steps_json, description, prompt_key) VALUES(?, ?, ?, ?, ?) \
             ON CONFLICT(name) DO NOTHING",
        )
        .bind(&playbook.name)
        .bind(playbook.origin.as_str())
        .bind(&steps)
        .bind(playbook.description.as_deref())
        .bind(playbook.prompt_key.as_deref())
        .execute(pool)
        .await?;
    }
    Ok(())
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
        let playbook = reports_playbook()?.with_description(Some("Monthly site report".into()));
        let id = store.save_playbook(&playbook).await?;
        let revived = store.load_playbook(&id).await?;
        assert_eq!(revived, playbook);
        let listed = store.list_playbooks().await?;
        // Seeded defaults share the list, so match by id rather than position.
        let row = listed
            .iter()
            .find(|summary| summary.id == id)
            .ok_or("saved row listed")?;
        assert_eq!(row.name, "reports");
        assert_eq!(row.portal_url, "https://portal.example.com/");
        assert_eq!(row.step_count, 1);
        assert_eq!(row.description.as_deref(), Some("Monthly site report"));
        assert!(!row.updated_at.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn initialization_seeds_runnable_default_playbooks()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store) = store().await?;
        // Every seed definition validates, so a database open can never fail
        // on its own default data.
        let seeds = crate::schema::seeded_playbooks()?;
        assert!(!seeds.is_empty());
        let listed = store.list_playbooks().await?;
        for seed in &seeds {
            let row = listed
                .iter()
                .find(|summary| summary.name == seed.name)
                .ok_or("seed row listed")?;
            // Round-trips through storage as a runnable envelope, exactly like
            // a user's own recording — no special-casing on read.
            assert_eq!(store.load_playbook(&row.id).await?, *seed);
        }
        // The GitHub harvester carries the billing-history entry route the
        // deleted static table used to supply, plus the invoice batch anchor.
        let github = seeds
            .iter()
            .find(|seed| seed.name == "github-invoices")
            .ok_or("github seed")?;
        let crate::schema::Step::Semantic { intent } = github.steps.first().ok_or("seeded step")?
        else {
            return Err("seeded step is semantic".into());
        };
        assert_eq!(
            intent.entry_url.as_deref(),
            Some("https://github.com/account/billing/history")
        );
        assert_eq!(intent.primary_target_noun.as_deref(), Some("invoice"));
        // Single-target by choice, not by limitation: the saved-replay lane
        // now honors `is_plural` through the batch gate, so this seed stays
        // singular because it promises one invoice, not because a plural seed
        // would be inert.
        assert!(!intent.is_plural);
        // Reopening is idempotent and never clobbers later edits to the row.
        let mut edited = github.clone().with_description(Some("edited".into()));
        edited.steps.push(edited.steps[0].clone());
        let id = store.save_playbook(&edited).await?;
        let pool = store.pool.clone();
        super::seed_default_playbooks(&pool).await?;
        assert_eq!(store.load_playbook(&id).await?, edited);
        let names: Vec<String> = store
            .list_playbooks()
            .await?
            .into_iter()
            .map(|summary| summary.name)
            .collect();
        assert_eq!(
            names
                .iter()
                .filter(|name| *name == "github-invoices")
                .count(),
            1
        );
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
    async fn description_migrates_legacy_databases() -> Result<(), Box<dyn std::error::Error>> {
        // Databases created before the memo column existed gain it on next
        // open: hand-roll the v1 table, run the real `initialize`, then
        // save and read back a memo through the migrated schema.
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("legacy.db");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await?;
        sqlx::query(
            "CREATE TABLE playbooks (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE, portal_url TEXT NOT NULL, steps_json TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
        )
        .execute(&pool)
        .await?;
        pool.close().await;
        let pool = super::initialize(&path).await?;
        let store = PlaybookStore::new(pool);
        let id = store
            .save_playbook(&reports_playbook()?.with_description(Some("Migrated memo".into())))
            .await?;
        assert_eq!(
            store.load_playbook(&id).await?.description.as_deref(),
            Some("Migrated memo")
        );
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
        let row = listed
            .iter()
            .find(|summary| summary.id == id)
            .ok_or("saved row listed")?;
        assert_eq!(row.step_count, 2);
        // Re-saving replaced the row rather than adding one.
        assert_eq!(listed.iter().filter(|row| row.name == "reports").count(), 1);
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
        // The rejected definition wrote nothing: only seeded defaults remain.
        assert_eq!(
            store.list_playbooks().await?.len(),
            crate::schema::seeded_playbooks()?.len()
        );
        // A tampered row fails the listing closed instead of hiding a workflow.
        sqlx::query("INSERT INTO playbooks(name, portal_url, steps_json) VALUES('tampered', 'https://portal.example.com/', 'not-json')")
            .execute(&store.pool)
            .await?;
        assert!(store.list_playbooks().await.is_err());
        let tampered: i64 = sqlx::query_scalar("SELECT id FROM playbooks WHERE name = 'tampered'")
            .fetch_one(&store.pool)
            .await?;
        assert!(store.load_playbook(&tampered.to_string()).await.is_err());
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

    #[tokio::test]
    async fn site_shortcuts_round_trip_with_normalization() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_dir, store) = store().await?;
        // Names normalize to the ladder's keyspace: case and padding
        // never create a second entry.
        let saved = store
            .set_site_shortcut("  Amazon ", "https://www.amazon.in/")
            .await?;
        assert_eq!(saved.name, "amazon");
        assert_eq!(saved.url, "https://www.amazon.in/");
        assert_eq!(
            store.site_shortcut("AMAZON").await?.as_deref(),
            Some("https://www.amazon.in/")
        );
        // Overwrite is an upsert, not a duplicate.
        store
            .set_site_shortcut("amazon", "https://www.amazon.com/")
            .await?;
        assert_eq!(
            store.site_shortcut("amazon").await?.as_deref(),
            Some("https://www.amazon.com/")
        );
        let listed = store.list_site_shortcuts().await?;
        assert_eq!(
            listed,
            vec![super::SiteShortcut {
                name: "amazon".to_owned(),
                url: "https://www.amazon.com/".to_owned(),
            }]
        );
        // Unknown names read as unset; deleting them is a no-op.
        assert_eq!(store.site_shortcut("flipkart").await?, None);
        assert!(!store.delete_site_shortcut("flipkart").await?);
        // Delete removes the row for real.
        assert!(store.delete_site_shortcut("Amazon").await?);
        assert_eq!(store.site_shortcut("amazon").await?, None);
        assert!(store.list_site_shortcuts().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn site_shortcuts_reject_bad_names_and_urls() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store) = store().await?;
        for (name, url) in [
            ("", "https://www.amazon.in/"),
            ("   ", "https://www.amazon.in/"),
            ("amazon", "http://www.amazon.in/"),
            ("amazon", "www.amazon.in"),
            ("amazon", "not a url"),
            ("amazon", "https://"),
        ] {
            let Err(err) = store.set_site_shortcut(name, url).await else {
                panic!("invalid shortcut must fail: ({name:?}, {url:?})")
            };
            assert!(
                matches!(err, super::StoreError::Shortcut(_)),
                "unexpected error for ({name:?}, {url:?}): {err}"
            );
        }
        // Nothing invalid was persisted.
        assert!(store.list_site_shortcuts().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn site_shortcuts_survive_reopen() -> Result<(), Box<dyn std::error::Error>> {
        // The additive table is there on databases created before it, and
        // rows persist across opens — the ladder's learned rung is durable.
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("clinch.db");
        let pool = super::initialize(&path).await?;
        let store = PlaybookStore::new(pool);
        store
            .set_site_shortcut("github", "https://github.com/")
            .await?;
        drop(store);
        let pool = super::initialize(&path).await?;
        let store = PlaybookStore::new(pool);
        assert_eq!(
            store.site_shortcut("github").await?.as_deref(),
            Some("https://github.com/")
        );
        Ok(())
    }
}
