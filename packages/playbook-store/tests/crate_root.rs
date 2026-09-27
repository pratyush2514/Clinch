//! Integration tests for `playbook_store`.
//!
//! Moved out of `src/lib.rs` so the main source stays test-free.

use macro_engine::SemanticIntent;
use playbook_store::schema::{Playbook, Step};
use playbook_store::{PlaybookStore, RunKind};

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
    let pool = playbook_store::initialize(&dir.path().join("clinch.db")).await?;
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
async fn initialization_seeds_runnable_default_playbooks() -> Result<(), Box<dyn std::error::Error>>
{
    let (_dir, store) = store().await?;
    // Every seed definition validates, so a database open can never fail
    // on its own default data.
    let seeds = playbook_store::schema::seeded_playbooks()?;
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
    let playbook_store::schema::Step::Semantic { intent } =
        github.steps.first().ok_or("seeded step")?
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
    playbook_store::seed_default_playbooks(&pool).await?;
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
    let pool = playbook_store::initialize(&dir.path().join("drift.db")).await?;
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
    let playbook_store::schema::Step::Semantic { intent } = &stored.steps[0] else {
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
    let playbook_store::schema::Step::Semantic { intent: updated } = &revived.steps[0] else {
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
    let pool = playbook_store::initialize(&path).await?;
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
async fn entry_url_round_trips_validated_preconditions() -> Result<(), Box<dyn std::error::Error>> {
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
async fn run_journal_tracks_start_to_terminal_state() -> Result<(), Box<dyn std::error::Error>> {
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
async fn unknown_ids_and_invalid_definitions_fail_closed() -> Result<(), Box<dyn std::error::Error>>
{
    let (_dir, store) = store().await?;
    assert!(matches!(
        store.load_playbook("999").await,
        Err(playbook_store::StoreError::NotFound)
    ));
    assert!(matches!(
        store.load_playbook("not-an-id").await,
        Err(playbook_store::StoreError::NotFound)
    ));
    let mut bad = reports_playbook()?;
    bad.name.clear();
    assert!(store.save_playbook(&bad).await.is_err());
    // The rejected definition wrote nothing: only seeded defaults remain.
    assert_eq!(
        store.list_playbooks().await?.len(),
        playbook_store::schema::seeded_playbooks()?.len()
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
    let pool = playbook_store::initialize(&path).await?;
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&pool)
        .await?;
    assert_eq!(mode, "wal");
    pool.close().await;
    let pool = playbook_store::initialize(&path).await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn site_shortcuts_round_trip_with_normalization() -> Result<(), Box<dyn std::error::Error>> {
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
        vec![playbook_store::SiteShortcut {
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
            matches!(err, playbook_store::StoreError::Shortcut(_)),
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
    let pool = playbook_store::initialize(&path).await?;
    let store = PlaybookStore::new(pool);
    store
        .set_site_shortcut("github", "https://github.com/")
        .await?;
    drop(store);
    let pool = playbook_store::initialize(&path).await?;
    let store = PlaybookStore::new(pool);
    assert_eq!(
        store.site_shortcut("github").await?.as_deref(),
        Some("https://github.com/")
    );
    Ok(())
}
