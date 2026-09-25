//! Settings memory: per-origin remembered settings destinations for the
//! settings goal class, keyed beside identity rows in the
//! (`origin`, `goal_class`) table. Write/read/upsert, `www.` normalization,
//! forget-site cleanup covering identity rows too, and href validation
//! at the store boundary.
use playbook_store::{PlaybookStore, initialize};

async fn store() -> Result<(tempfile::TempDir, PlaybookStore), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let pool = initialize(&dir.path().join("clinch.db")).await?;
    Ok((dir, PlaybookStore::new(pool)))
}

#[tokio::test]
async fn miss_before_any_write() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    assert!(
        store
            .recall_settings_destination("reddit.com")
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn write_then_recall_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    store
        .remember_settings_destination("www.reddit.com", "https://www.reddit.com/settings/")
        .await?;
    let Some(remembered) = store.recall_settings_destination("www.reddit.com").await? else {
        return Err("remembered settings destination recalls".into());
    };
    // The origin key is normalized: `www.` stripped, lowercased.
    assert_eq!(remembered.origin, "reddit.com");
    assert_eq!(remembered.href, "https://www.reddit.com/settings/");
    // Provenance stamps the write path: verified landing, never a guess.
    assert_eq!(remembered.source, "verified_landing");
    Ok(())
}

#[tokio::test]
async fn upsert_replaces_the_row() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    store
        .remember_settings_destination("reddit.com", "https://www.reddit.com/settings/")
        .await?;
    store
        .remember_settings_destination("reddit.com", "https://www.reddit.com/settings/privacy/")
        .await?;
    let Some(remembered) = store.recall_settings_destination("reddit.com").await? else {
        return Err("remembered settings destination recalls".into());
    };
    assert_eq!(remembered.href, "https://www.reddit.com/settings/privacy/");
    Ok(())
}

#[tokio::test]
async fn www_and_bare_forms_key_the_same_row() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    store
        .remember_settings_destination("reddit.com", "https://www.reddit.com/settings/")
        .await?;
    // Recalling with the other form hits the same row.
    let Some(remembered) = store.recall_settings_destination("WWW.REDDIT.COM").await? else {
        return Err("remembered settings destination recalls".into());
    };
    assert_eq!(remembered.href, "https://www.reddit.com/settings/");
    Ok(())
}

#[tokio::test]
async fn identity_and_settings_rows_are_independent() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    // The profile row for the origin is written first, like the live flow.
    store
        .remember_identity(
            "www.reddit.com",
            "account_home",
            Some("MangoTree-1233"),
            "https://www.reddit.com/user/MangoTree-1233/",
            "page_menu",
        )
        .await?;
    store
        .remember_settings_destination("reddit.com", "https://www.reddit.com/settings/")
        .await?;
    // Each class recalls its own row; neither write clobbered the other.
    let Some(profile) = store.recall_identity("reddit.com", "account_home").await? else {
        return Err("profile row survives the settings write".into());
    };
    assert_eq!(profile.username.as_deref(), Some("MangoTree-1233"));
    assert_eq!(profile.href, "https://www.reddit.com/user/MangoTree-1233/");
    let Some(settings) = store.recall_settings_destination("reddit.com").await? else {
        return Err("settings row survives alongside the profile row".into());
    };
    assert_eq!(settings.href, "https://www.reddit.com/settings/");
    // Refreshing the profile row leaves the settings row untouched, and
    // refreshing the settings row leaves the profile row untouched.
    store
        .remember_identity(
            "reddit.com",
            "account_home",
            Some("newuser"),
            "https://www.reddit.com/user/newuser/",
            "page_menu",
        )
        .await?;
    store
        .remember_settings_destination("reddit.com", "https://www.reddit.com/settings/feed/")
        .await?;
    let Some(profile) = store.recall_identity("reddit.com", "account_home").await? else {
        return Err("profile row kept after both rewrites".into());
    };
    assert_eq!(profile.href, "https://www.reddit.com/user/newuser/");
    let Some(settings) = store.recall_settings_destination("reddit.com").await? else {
        return Err("settings row kept after both rewrites".into());
    };
    assert_eq!(settings.href, "https://www.reddit.com/settings/feed/");
    Ok(())
}

#[tokio::test]
async fn forget_site_clears_settings_and_identity_rows() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    store
        .remember_identity(
            "www.reddit.com",
            "account_home",
            None,
            "https://www.reddit.com/user/someuser/",
            "page_menu",
        )
        .await?;
    store
        .remember_settings_destination("reddit.com", "https://www.reddit.com/settings/")
        .await?;
    store
        .remember_identity(
            "other.com",
            "account_home",
            None,
            "https://other.com/user/someuser/",
            "page_menu",
        )
        .await?;
    store
        .remember_settings_destination("other.com", "https://other.com/settings/")
        .await?;
    // Forgetting by the bare form clears the `www.`-keyed rows too —
    // settings and identity alike.
    let removed = store.forget_site("reddit.com").await?;
    assert_eq!(removed, 2);
    assert!(
        store
            .recall_settings_destination("www.reddit.com")
            .await?
            .is_none()
    );
    assert!(
        store
            .recall_identity("reddit.com", "account_home")
            .await?
            .is_none()
    );
    // The other origin's rows are untouched.
    assert!(
        store
            .recall_settings_destination("other.com")
            .await?
            .is_some()
    );
    assert!(
        store
            .recall_identity("other.com", "account_home")
            .await?
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn forget_site_is_idempotent_on_unknown_origin() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    assert_eq!(store.forget_site("unknown.example").await?, 0);
    Ok(())
}

#[tokio::test]
async fn rejects_invalid_inputs() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    for (origin, href) in [
        // Non-https destinations.
        ("reddit.com", "http://www.reddit.com/settings/"),
        ("reddit.com", "javascript:alert(1)"),
        ("reddit.com", "/settings"),
        ("reddit.com", "www.reddit.com/settings"),
        // Credentials embedded in the destination.
        ("reddit.com", "https://user:secret@www.reddit.com/settings/"),
        ("reddit.com", "https://user@www.reddit.com/settings/"),
        // Empty origin or destination.
        ("", "https://www.reddit.com/settings/"),
        ("   ", "https://www.reddit.com/settings/"),
        ("reddit.com", ""),
        ("reddit.com", "   "),
    ] {
        let Err(err) = store.remember_settings_destination(origin, href).await else {
            panic!("invalid settings destination must fail: ({origin:?}, {href:?})")
        };
        assert!(
            matches!(err, playbook_store::StoreError::Shortcut(_)),
            "unexpected error for ({origin:?}, {href:?}): {err}"
        );
    }
    // Nothing invalid was persisted.
    assert!(
        store
            .recall_settings_destination("reddit.com")
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn settings_memory_is_a_first_class_row_for_identity_forget()
-> Result<(), Box<dyn std::error::Error>> {
    // The legacy forget entry point covers settings rows too: one table,
    // one site scope, whichever name the caller used.
    let (_dir, store) = store().await?;
    store
        .remember_settings_destination("reddit.com", "https://www.reddit.com/settings/")
        .await?;
    assert_eq!(store.forget_identity_for_origin("www.reddit.com").await?, 1);
    assert!(
        store
            .recall_settings_destination("reddit.com")
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn scoped_forget_keeps_the_other_goal_class() -> Result<(), Box<dyn std::error::Error>> {
    // The stale-row eviction path is goal-class scoped: a stale profile
    // row must not evict the origin's settings row, and vice versa.
    let (_dir, store) = store().await?;
    store
        .remember_identity(
            "www.reddit.com",
            "account_home",
            None,
            "https://www.reddit.com/user/someuser/",
            "page_menu",
        )
        .await?;
    store
        .remember_settings_destination("reddit.com", "https://www.reddit.com/settings/")
        .await?;
    // Evicting the stale account_home row (by the other www form, like
    // the live stale path) leaves the settings row in place.
    assert_eq!(
        store
            .forget_identity_for_origin_and_class("www.reddit.com", "account_home")
            .await?,
        1
    );
    assert!(
        store
            .recall_identity("reddit.com", "account_home")
            .await?
            .is_none()
    );
    assert!(
        store
            .recall_settings_destination("reddit.com")
            .await?
            .is_some()
    );
    // And the mirror direction: evicting settings keeps the profile row.
    store
        .remember_identity(
            "reddit.com",
            "account_home",
            None,
            "https://www.reddit.com/user/someuser/",
            "page_menu",
        )
        .await?;
    assert_eq!(
        store
            .forget_identity_for_origin_and_class("reddit.com", "settings")
            .await?,
        1
    );
    assert!(
        store
            .recall_settings_destination("reddit.com")
            .await?
            .is_none()
    );
    assert!(
        store
            .recall_identity("reddit.com", "account_home")
            .await?
            .is_some()
    );
    // An unknown origin or class removes nothing.
    assert_eq!(
        store
            .forget_identity_for_origin_and_class("unknown.example", "account_home")
            .await?,
        0
    );
    assert_eq!(
        store
            .forget_identity_for_origin_and_class("reddit.com", "pricing")
            .await?,
        0
    );
    Ok(())
}
