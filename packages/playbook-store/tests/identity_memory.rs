//! Identity memory: per-origin remembered profile destinations for the
//! account-home goal class. Write/read/upsert, `www.` normalization,
//! forget-site cleanup, and href validation at the store boundary.
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
            .recall_identity("example.com", "account_home")
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn write_then_recall_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    store
        .remember_identity(
            "www.example.com",
            "account_home",
            Some("someuser"),
            "https://www.example.com/user/someuser/",
            "page_menu",
        )
        .await?;
    let remembered = store
        .recall_identity("www.example.com", "account_home")
        .await?
        .expect("remembered identity recalls");
    // The origin key is normalized: `www.` stripped, lowercased.
    assert_eq!(remembered.origin, "example.com");
    assert_eq!(remembered.goal_class, "account_home");
    assert_eq!(remembered.username.as_deref(), Some("someuser"));
    assert_eq!(remembered.href, "https://www.example.com/user/someuser/");
    assert_eq!(remembered.source, "page_menu");
    Ok(())
}

#[tokio::test]
async fn upsert_replaces_the_row() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    store
        .remember_identity(
            "example.com",
            "account_home",
            Some("olduser"),
            "https://www.example.com/user/olduser/",
            "page_menu",
        )
        .await?;
    store
        .remember_identity(
            "example.com",
            "account_home",
            Some("newuser"),
            "https://www.example.com/user/newuser/",
            "page_menu",
        )
        .await?;
    let remembered = store
        .recall_identity("example.com", "account_home")
        .await?
        .expect("remembered identity recalls");
    assert_eq!(remembered.username.as_deref(), Some("newuser"));
    assert_eq!(remembered.href, "https://www.example.com/user/newuser/");
    Ok(())
}

#[tokio::test]
async fn www_and_bare_forms_key_the_same_row() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    store
        .remember_identity(
            "example.com",
            "account_home",
            None,
            "https://example.com/settings",
            "page_menu",
        )
        .await?;
    // Recalling with the other form hits the same row.
    let remembered = store
        .recall_identity("www.example.com", "account_home")
        .await?
        .expect("remembered identity recalls");
    assert_eq!(remembered.href, "https://example.com/settings");
    Ok(())
}

#[tokio::test]
async fn goal_classes_do_not_share_rows() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    store
        .remember_identity(
            "example.com",
            "account_home",
            None,
            "https://example.com/settings",
            "page_menu",
        )
        .await?;
    assert!(
        store
            .recall_identity("example.com", "other_goal")
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn rejects_non_https_href() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    assert!(
        store
            .remember_identity(
                "example.com",
                "account_home",
                None,
                "http://example.com/settings",
                "page_menu",
            )
            .await
            .is_err()
    );
    assert!(
        store
            .remember_identity(
                "example.com",
                "account_home",
                None,
                "javascript:alert(1)",
                "page_menu",
            )
            .await
            .is_err()
    );
    assert!(
        store
            .remember_identity(
                "example.com",
                "account_home",
                None,
                "/settings",
                "page_menu",
            )
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn forget_removes_the_origin_and_keeps_others() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    store
        .remember_identity(
            "www.example.com",
            "account_home",
            None,
            "https://www.example.com/settings",
            "page_menu",
        )
        .await?;
    store
        .remember_identity(
            "other.com",
            "account_home",
            None,
            "https://other.com/settings",
            "page_menu",
        )
        .await?;
    // Forgetting by the bare form clears the `www.`-keyed row too.
    let removed = store.forget_identity_for_origin("example.com").await?;
    assert_eq!(removed, 1);
    assert!(
        store
            .recall_identity("www.example.com", "account_home")
            .await?
            .is_none()
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
async fn forget_is_idempotent_on_unknown_origin() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    assert_eq!(
        store.forget_identity_for_origin("unknown.example").await?,
        0
    );
    Ok(())
}
