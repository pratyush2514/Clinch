//! Integration tests for `session_sync::reader`.
//!
//! Moved out of `src/reader.rs` so the main source stays test-free.

use session_sync::SyncError;
use session_sync::crypto::{derive_gcm_key, derive_key, encrypt_fixture, encrypt_gcm_fixture};
use session_sync::reader::*;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::Path;
use zeroize::Zeroizing;

async fn fixture(
    version: i64,
) -> Result<(tempfile::TempDir, SqliteConnection), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("Cookies"))
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
    let mut db = SqliteConnection::connect_with(&options).await?;
    sqlx::query("CREATE TABLE meta (key TEXT, value TEXT)")
        .execute(&mut db)
        .await?;
    sqlx::query("INSERT INTO meta VALUES ('version', ?)")
        .bind(version.to_string())
        .execute(&mut db)
        .await?;
    sqlx::query("CREATE TABLE cookies (host_key TEXT, name TEXT, value TEXT, encrypted_value BLOB, path TEXT, expires_utc INTEGER, is_secure INTEGER, is_httponly INTEGER, samesite INTEGER, has_expires INTEGER, top_frame_site_key TEXT)").execute(&mut db).await?;
    Ok((dir, db))
}
async fn insert(
    db: &mut SqliteConnection,
    host: &str,
    encrypted: Vec<u8>,
    expiry: i64,
    persistent: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO cookies VALUES (?, 'session', '', ?, '/', ?, 1, 1, 1, ?, '')")
        .bind(host)
        .bind(encrypted)
        .bind(expiry)
        .bind(persistent)
        .execute(db)
        .await?;
    Ok(())
}
#[test]
fn wildcard_roots_cover_strict_ancestors_only() {
    assert_eq!(ancestor_roots("portal.example.com"), vec!["example.com"]);
    assert_eq!(
        ancestor_roots("a.portal.example.com"),
        vec!["portal.example.com", "example.com"]
    );
    // Bare domains and bare TLDs yield no wildcard: no behavior change.
    assert!(ancestor_roots("example.com").is_empty());
    assert!(ancestor_roots("localhost").is_empty());
    assert_eq!(escape_like("exa%mple_com\\x"), "exa\\%mple\\_com\\\\x");
    // The statement numbers every parameter explicitly, including the
    // subdomain wildcard on the exact host.
    let sql = cookie_query(2);
    assert!(sql.contains("host_key = ?1"));
    assert!(sql.contains("LIKE '%.' || ?1"));
    assert!(sql.contains("LIKE '%.' || ?2 ESCAPE '\\'"));
    assert!(sql.contains("LIKE '%.' || ?3 ESCAPE '\\'"));
    assert!(sql.contains("LIMIT ?4"));
}

#[test]
fn curated_secondaries_need_no_shared_suffix() {
    assert_eq!(
        sso_secondaries("chatgpt.com"),
        vec!["openai.com".to_owned(), "auth.openai.com".to_owned()]
    );
    assert_eq!(
        sso_secondaries("app.chatgpt.com"),
        vec!["openai.com".to_owned(), "auth.openai.com".to_owned()]
    );
    assert!(sso_secondaries("portal.example.com").is_empty());
    assert!(sso_secondaries("").is_empty());
}

#[tokio::test]
async fn extracts_sibling_auth_domain_cookies_without_cross_site_leak()
-> Result<(), Box<dyn std::error::Error>> {
    let (dir, mut db) = fixture(24).await?;
    let key = derive_key(b"fixture");
    // SSO family: exact host, dotted parent, bare + dotted siblings.
    for host in [
        "portal.example.com",
        ".example.com",
        "auth.example.com",
        ".sso.example.com",
    ] {
        insert(
            &mut db,
            host,
            encrypt_fixture("test-secret", host, 24, &key),
            0,
            false,
        )
        .await?;
    }
    // Lookalikes and foreign domains must never match the wildcard.
    for host in ["evil-example.com", "notexample.com", ".unrelated.com"] {
        insert(
            &mut db,
            host,
            encrypt_fixture("test-secret", host, 24, &key),
            0,
            false,
        )
        .await?;
    }
    db.close().await?;
    let cookies = read_profile(
        dir.path(),
        "portal.example.com",
        Zeroizing::new(b"fixture".to_vec()),
    )
    .await?;
    let mut domains: Vec<&str> = cookies
        .iter()
        .map(|cookie| cookie.domain.as_str())
        .collect();
    domains.sort_unstable();
    assert_eq!(
        domains,
        vec![
            ".example.com",
            ".sso.example.com",
            "auth.example.com",
            "portal.example.com",
        ]
    );
    assert!(
        cookies
            .iter()
            .all(|cookie| cookie.secure && cookie.http_only)
    );
    Ok(())
}

#[tokio::test]
async fn extracts_subdomain_and_cross_root_sso_cookies() -> Result<(), Box<dyn std::error::Error>> {
    let (dir, mut db) = fixture(24).await?;
    let key = derive_key(b"fixture");
    for host in [
        "chatgpt.com",
        "auth.openai.com",
        ".openai.com",
        "evil-openai.com",
        ".unrelated.com",
    ] {
        insert(
            &mut db,
            host,
            encrypt_fixture("test-secret", host, 24, &key),
            0,
            false,
        )
        .await?;
    }
    db.close().await?;
    let cookies = read_profile(
        dir.path(),
        "chatgpt.com",
        Zeroizing::new(b"fixture".to_vec()),
    )
    .await?;
    let mut domains: Vec<&str> = cookies
        .iter()
        .map(|cookie| cookie.domain.as_str())
        .collect();
    domains.sort_unstable();
    assert_eq!(
        domains,
        vec![".openai.com", "auth.openai.com", "chatgpt.com",]
    );
    Ok(())
}

#[tokio::test]
async fn extracts_same_root_subdomain_cookies() -> Result<(), Box<dyn std::error::Error>> {
    let (dir, mut db) = fixture(24).await?;
    let key = derive_key(b"fixture");
    for host in ["claude.ai", "auth.claude.ai", "evilclaude.ai"] {
        insert(
            &mut db,
            host,
            encrypt_fixture("test-secret", host, 24, &key),
            0,
            false,
        )
        .await?;
    }
    db.close().await?;
    let cookies =
        read_profile(dir.path(), "claude.ai", Zeroizing::new(b"fixture".to_vec())).await?;
    let mut domains: Vec<&str> = cookies
        .iter()
        .map(|cookie| cookie.domain.as_str())
        .collect();
    domains.sort_unstable();
    assert_eq!(domains, vec!["auth.claude.ai", "claude.ai",]);
    Ok(())
}

#[tokio::test]
async fn modern_profile_plaintext_expiry_and_missing_profile()
-> Result<(), Box<dyn std::error::Error>> {
    let (dir, mut db) = fixture(23).await?;
    let future = (4_102_444_800 + CHROME_EPOCH_SECONDS) * 1_000_000;
    insert(&mut db, "example.com", vec![], future, true).await?;
    sqlx::query("UPDATE cookies SET value = 'plain-fixture', samesite = 2")
        .execute(&mut db)
        .await?;
    db.close().await?;
    tokio::fs::create_dir(dir.path().join("Network")).await?;
    tokio::fs::rename(
        dir.path().join("Cookies"),
        dir.path().join("Network/Cookies"),
    )
    .await?;
    let cookies = read_profile(dir.path(), "example.com", Zeroizing::new(vec![])).await?;
    assert_eq!(cookies[0].value.as_str(), "plain-fixture");
    assert_eq!(cookies[0].expires, Some(4_102_444_800));
    assert!(matches!(cookies[0].same_site, CookieSameSite::Strict));
    let empty = tempfile::tempdir()?;
    // A profile without a cookie database never reaches SQLite: the
    // shadow-copy staging fails closed before any connection opens, and
    // the error names the exact path that was attempted.
    match read_profile(empty.path(), "example.com", Zeroizing::new(vec![])).await {
        Err(error @ SyncError::ProfileUnavailable { .. }) => {
            let message = error.to_string();
            assert!(message.contains("Cookies"), "{message}");
        }
        other => panic!("wrong result: {}", other.is_ok()),
    }
    assert!(!empty.path().join("Cookies").exists());
    Ok(())
}

#[tokio::test]
async fn shadow_copy_stages_wal_and_cleans_up_immediately() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("Cookies");
    tokio::fs::write(&source, b"cookie-bytes").await?;
    let wal = dir.path().join("Cookies-wal");
    tokio::fs::write(&wal, b"wal-bytes").await?;
    let shadow = stage_shadow_copy(&source, "portal.example.com").await?;
    assert!(shadow.path().starts_with(std::env::temp_dir()));
    assert_eq!(tokio::fs::read(shadow.path()).await?, b"cookie-bytes");
    let mut staged_wal = shadow.path().as_os_str().to_owned();
    staged_wal.push("-wal");
    assert_eq!(tokio::fs::read(Path::new(&staged_wal)).await?, b"wal-bytes");
    let main = shadow.path().to_owned();
    drop(shadow);
    assert!(!main.exists());
    assert!(!Path::new(&staged_wal).exists());
    // The live source is never modified or removed.
    assert!(source.exists());
    assert!(wal.exists());
    Ok(())
}

#[tokio::test]
async fn shadow_copy_missing_source_is_profile_unavailable()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let missing = dir.path().join("Cookies");
    match stage_shadow_copy(&missing, "portal.example.com").await {
        Err(SyncError::ProfileUnavailable { path }) => assert_eq!(path, missing),
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("missing source must fail"),
    }
    Ok(())
}

#[tokio::test]
async fn reads_live_wal_scopes_domains_and_preserves_flags()
-> Result<(), Box<dyn std::error::Error>> {
    let (dir, mut db) = fixture(24).await?;
    let key = derive_key(b"fixture");
    for host in [
        ".example.com",
        "portal.example.com",
        "evil-example.com",
        ".unrelated.com",
    ] {
        insert(
            &mut db,
            host,
            encrypt_fixture("test-secret", host, 24, &key),
            0,
            false,
        )
        .await?;
    }
    insert(
        &mut db,
        ".example.com",
        encrypt_fixture("expired", ".example.com", 24, &key),
        1,
        true,
    )
    .await?;
    let cookies = read_profile(
        dir.path(),
        "portal.example.com",
        Zeroizing::new(b"fixture".to_vec()),
    )
    .await?;
    assert_eq!(cookies.len(), 2);
    assert!(cookies.iter().all(|c| c.value.as_str() == "test-secret"
        && c.secure
        && c.http_only
        && c.expires.is_none()));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM cookies")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(count, 5);
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn incompatible_missing_and_corrupt_profiles_fail_cleanly()
-> Result<(), Box<dyn std::error::Error>> {
    let (dir, mut db) = fixture(24).await?;
    assert!(matches!(
        read_profile(dir.path(), "example.com", Zeroizing::new(vec![])).await,
        Err(SyncError::MissingCookies)
    ));
    insert(&mut db, "example.com", b"v20unsupported".to_vec(), 0, false).await?;
    assert!(matches!(
        read_profile(dir.path(), "example.com", Zeroizing::new(vec![])).await,
        Err(SyncError::UnsupportedFormat)
    ));
    sqlx::query("UPDATE meta SET value = '99'")
        .execute(&mut db)
        .await?;
    assert!(matches!(
        read_profile(dir.path(), "example.com", Zeroizing::new(vec![])).await,
        Err(SyncError::UnsupportedFormat)
    ));
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn wrong_key_and_partitioned_cookie_fail_closed() -> Result<(), Box<dyn std::error::Error>> {
    let (dir, mut db) = fixture(24).await?;
    let encrypted = encrypt_fixture("secret", "example.com", 24, &derive_key(b"correct"));
    insert(&mut db, "example.com", encrypted, 0, false).await?;
    assert!(matches!(
        read_profile(dir.path(), "example.com", Zeroizing::new(b"wrong".to_vec())).await,
        Err(SyncError::Decryption)
    ));
    sqlx::query("UPDATE cookies SET top_frame_site_key = 'https://other.com'")
        .execute(&mut db)
        .await?;
    assert!(matches!(
        read_profile(
            dir.path(),
            "example.com",
            Zeroizing::new(b"correct".to_vec())
        )
        .await,
        Err(SyncError::UnsupportedFormat)
    ));
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn gcm_encrypted_cookies_read_through_the_same_snapshot()
-> Result<(), Box<dyn std::error::Error>> {
    let (dir, mut db) = fixture(24).await?;
    let encrypted = encrypt_gcm_fixture(
        "gcm-session",
        "example.com",
        24,
        &derive_gcm_key(b"correct"),
    )?;
    insert(&mut db, "example.com", encrypted, 0, false).await?;
    let cookies = read_profile(
        dir.path(),
        "example.com",
        Zeroizing::new(b"correct".to_vec()),
    )
    .await?;
    assert_eq!(cookies.len(), 1);
    assert_eq!(cookies[0].value.as_str(), "gcm-session");
    db.close().await?;
    Ok(())
}
