#![deny(unsafe_code)]
use crate::{SyncError, crypto};
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

const CHROME_EPOCH_SECONDS: i64 = 11_644_473_600;
const MAX_COOKIES: i64 = 2_000;

#[derive(Clone, Copy, Debug)]
pub enum CookieSameSite {
    Unspecified,
    None,
    Lax,
    Strict,
}

// Deliberately no Debug or Serialize: a cookie is never an IPC/log payload.
pub struct Cookie {
    pub name: String,
    pub value: Zeroizing<String>,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: CookieSameSite,
    pub expires: Option<i64>,
}

/// Read a consistent, read-only `SQLite` snapshot, including committed WAL rows.
/// Supports schema versions 23/24; unfamiliar encryption/partitioning fails closed.
///
/// # Errors
/// Returns typed errors for missing, locked, incompatible or corrupt profiles.
pub async fn read_profile(
    profile: &Path,
    host: &str,
    safe_storage: Zeroizing<Vec<u8>>,
) -> Result<Vec<Cookie>, SyncError> {
    let modern = profile.join("Network/Cookies");
    let legacy = profile.join("Cookies");
    let path = if tokio::fs::try_exists(&modern)
        .await
        .map_err(|_| SyncError::ProfileUnavailable)?
    {
        modern
    } else {
        legacy
    };
    let options = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .busy_timeout(Duration::from_secs(2));
    let mut conn = SqliteConnection::connect_with(&options).await?;
    let mut tx = conn.begin().await?;
    let version: i64 =
        sqlx::query_scalar("SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'version'")
            .fetch_one(&mut *tx)
            .await?;
    if !(23..=24).contains(&version) {
        return Err(SyncError::UnsupportedFormat);
    }
    // Host cookies match exactly; domain cookies match only at a dot boundary.
    let rows = sqlx::query("SELECT host_key, name, value, encrypted_value, path, expires_utc, is_secure, is_httponly, samesite, has_expires, top_frame_site_key FROM cookies WHERE host_key = ? OR (substr(host_key, 1, 1) = '.' AND (host_key = '.' || ? OR substr(?, -length(host_key)) = host_key)) LIMIT ?")
        .bind(host).bind(host).bind(host).bind(MAX_COOKIES + 1).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    conn.close().await?;
    if rows.len() > usize::try_from(MAX_COOKIES).map_err(|_| SyncError::UnsupportedFormat)? {
        return Err(SyncError::UnsupportedFormat);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SyncError::Decryption)?;
    let now = i64::try_from(now.as_secs()).map_err(|_| SyncError::Decryption)?;
    // Decryption/derivation are CPU work and do not occupy an async runtime worker.
    tokio::task::spawn_blocking(move || {
        let key = crypto::derive_key(&safe_storage);
        let mut cookies = Vec::with_capacity(rows.len());
        for row in rows {
            let persistent: bool = row.try_get("has_expires")?;
            let chrome_expiry: i64 = row.try_get("expires_utc")?;
            let expires = persistent.then_some(chrome_expiry / 1_000_000 - CHROME_EPOCH_SECONDS);
            if expires.is_some_and(|expiry| expiry <= now) {
                continue;
            }
            let partition: String = row.try_get("top_frame_site_key")?;
            // Do not silently turn CHIPS cookies into unpartitioned cookies.
            if !partition.is_empty() {
                return Err(SyncError::UnsupportedFormat);
            }
            let domain: String = row.try_get("host_key")?;
            let plaintext = Zeroizing::new(row.try_get::<String, _>("value")?);
            let encrypted: Vec<u8> = row.try_get("encrypted_value")?;
            let value = if encrypted.is_empty() {
                plaintext
            } else {
                if !plaintext.is_empty() {
                    return Err(SyncError::UnsupportedFormat);
                }
                crypto::decrypt(&encrypted, &domain, version, &key)?
            };
            let same_site = match row.try_get::<i64, _>("samesite")? {
                -1 => CookieSameSite::Unspecified,
                0 => CookieSameSite::None,
                1 => CookieSameSite::Lax,
                2 => CookieSameSite::Strict,
                _ => return Err(SyncError::UnsupportedFormat),
            };
            cookies.push(Cookie {
                name: row.try_get("name")?,
                value,
                domain,
                path: row.try_get("path")?,
                secure: row.try_get("is_secure")?,
                http_only: row.try_get("is_httponly")?,
                same_site,
                expires,
            });
        }
        if cookies.is_empty() {
            return Err(SyncError::MissingCookies);
        }
        Ok(cookies)
    })
    .await
    .map_err(|_| SyncError::WorkerFailed)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{derive_key, encrypt_fixture};

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
    #[tokio::test]
    async fn modern_profile_plaintext_expiry_and_missing_database()
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
        assert!(matches!(
            read_profile(empty.path(), "example.com", Zeroizing::new(vec![])).await,
            Err(SyncError::Database(_))
        ));
        assert!(!empty.path().join("Cookies").exists());
        Ok(())
    }

    #[tokio::test]
    async fn reads_live_wal_scopes_domains_and_preserves_flags()
    -> Result<(), Box<dyn std::error::Error>> {
        let (dir, mut db) = fixture(24).await?;
        let key = derive_key(b"fixture");
        for host in [
            ".example.com",
            "billing.example.com",
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
            "billing.example.com",
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
    async fn wrong_key_and_partitioned_cookie_fail_closed() -> Result<(), Box<dyn std::error::Error>>
    {
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
}
