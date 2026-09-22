#![deny(unsafe_code)]
use crate::{SyncError, crypto};
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

const CHROME_EPOCH_SECONDS: i64 = 11_644_473_600;
const MAX_COOKIES: i64 = 2_000;
/// `SQLite` busy timeout on the read-only shadow copy: the live browser may
/// briefly hold its cookie database while we snapshot it.
const SHADOW_BUSY_TIMEOUT: Duration = Duration::from_secs(2);
/// Monotonic suffix so concurrent syncs never share a shadow-copy path.
static SHADOW_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Strict ancestor suffixes for wildcard SSO matching, minimum two labels
/// (`portal.example.com` → `["example.com"]`; `a.b.example.com` →
/// `["b.example.com", "example.com"]`; bare `example.com` → none).
/// A bare TLD can never become a root, so `example.co.uk`-style portals only
/// ever match their own site family — never the whole public suffix.
#[must_use]
pub fn ancestor_roots(host: &str) -> Vec<String> {
    let host = host.trim_end_matches('.');
    let labels: Vec<&str> = host.split('.').collect();
    let mut roots = Vec::new();
    for start in 1..labels.len().saturating_sub(1) {
        roots.push(labels[start..].join("."));
    }
    roots
}

/// Curated cross-root SSO secondaries no suffix rule can derive
/// (`chatgpt.com` shares nothing with `openai.com`). Entries require observed
/// evidence — this is an allowlist, not a guess list. Looks up the host and
/// its parent, so portals nested under a known root inherit the entry.
#[must_use]
pub fn sso_secondaries(host: &str) -> Vec<String> {
    fn table(key: &str, out: &mut Vec<String>) {
        if key == "chatgpt.com" {
            out.extend(
                ["openai.com", "auth.openai.com"]
                    .iter()
                    .map(ToString::to_string),
            );
        }
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let mut roots = Vec::new();
    table(&host, &mut roots);
    if let Some((_, parent)) = host.split_once('.') {
        table(parent, &mut roots);
    }
    roots
}

/// Cookie selection statement: exact host, its subdomains, one `LIKE`
/// wildcard per ancestor root and curated cross-root secondary.
/// `?1` is the exact host, `?2..=?{n+1}` the escaped roots, the final
/// parameter the row cap. Every parameter is bound — no string interpolation
/// of host data into SQL.
fn cookie_query(root_count: usize) -> String {
    use std::fmt::Write as _;
    let mut sql = String::from(
        "SELECT host_key, name, value, encrypted_value, path, expires_utc, is_secure, is_httponly, samesite, has_expires, top_frame_site_key FROM cookies WHERE host_key = ?1 OR host_key = '.' || ?1 OR host_key LIKE '%.' || ?1",
    );
    for index in 0..root_count {
        let _ = write!(sql, " OR host_key LIKE '%.' || ?{} ESCAPE '\\'", index + 2);
    }
    let _ = write!(
        sql,
        " OR (substr(host_key, 1, 1) = '.' AND substr(?1, -length(host_key)) = host_key) LIMIT ?{}",
        root_count + 2
    );
    sql
}

/// Escape `LIKE` metacharacters in a validated hostname. Hostnames cannot
/// legitimately contain `%`, `_`, or `\`, but the pattern is still escaped so
/// a malformed host can never widen the match.
fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

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

/// RAII guard for the shadow copy. The temporary files are removed on drop,
/// so every return path — including decryption failures — cleans up.
struct ShadowCopy {
    main: PathBuf,
    wal: PathBuf,
}

impl ShadowCopy {
    fn path(&self) -> &Path {
        &self.main
    }
}

impl Drop for ShadowCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.main);
        let _ = std::fs::remove_file(&self.wal);
    }
}

/// Copy the live cookie database into `std::env::temp_dir()` before opening
/// it. The running browser holds a file lock on `Cookies`; a plain
/// `tokio::fs::copy` (OS-level read) still succeeds where a `SQLite` open
/// would fail with a locking error. The `-wal` sibling is copied
/// best-effort so committed-but-uncheckpointed rows survive in the copy;
/// `-shm` is transient shared memory and is never copied.
///
/// # Errors
/// Returns `ProfileUnavailable` when the source is absent and `Unstageable`
/// when the copy itself fails (sharing violation, permissions).
async fn stage_shadow_copy(source: &Path, host: &str) -> Result<ShadowCopy, SyncError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos().to_string())
        .unwrap_or_default();
    let sanitized: String = host
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!(
        "clinch-cookies-{}-{nanos}-{sanitized}-{}",
        std::process::id(),
        SHADOW_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let dest = std::env::temp_dir().join(name);
    tokio::fs::copy(source, &dest).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SyncError::ProfileUnavailable {
                path: source.to_owned(),
            }
        } else {
            SyncError::Unstageable
        }
    })?;
    let mut wal_source = source.as_os_str().to_owned();
    wal_source.push("-wal");
    let mut wal_dest = dest.as_os_str().to_owned();
    wal_dest.push("-wal");
    // Best effort: absence is normal when Chrome has checkpointed everything.
    let _ = tokio::fs::copy(Path::new(&wal_source), Path::new(&wal_dest)).await;
    Ok(ShadowCopy {
        main: dest,
        wal: PathBuf::from(wal_dest),
    })
}

/// Read a consistent, read-only `SQLite` snapshot, including committed WAL rows.
/// Supports schema versions 23/24; unfamiliar encryption/partitioning fails closed.
///
/// The source database is never opened directly: it is staged through
/// [`stage_shadow_copy`] first, so extraction succeeds while the browser is
/// running. The shadow copy is removed once rows are in memory, before
/// decryption starts.
///
/// # Errors
/// Returns typed errors for missing, locked, incompatible or corrupt profiles.
pub async fn read_profile(
    profile: &Path,
    host: &str,
    safe_storage: Zeroizing<Vec<u8>>,
) -> Result<Vec<Cookie>, SyncError> {
    let [modern, legacy] = crate::paths::cookie_db_candidates(profile);
    let source =
        if tokio::fs::try_exists(&modern)
            .await
            .map_err(|_| SyncError::ProfileUnavailable {
                path: modern.clone(),
            })?
        {
            modern
        } else {
            legacy
        };
    let shadow = stage_shadow_copy(&source, host).await?;
    let options = SqliteConnectOptions::new()
        .filename(shadow.path())
        .read_only(true)
        .busy_timeout(SHADOW_BUSY_TIMEOUT);
    let mut conn = SqliteConnection::connect_with(&options).await?;
    let mut tx = conn.begin().await?;
    let version: i64 =
        sqlx::query_scalar("SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'version'")
            .fetch_one(&mut *tx)
            .await?;
    if !(23..=24).contains(&version) {
        return Err(SyncError::UnsupportedFormat);
    }
    // Wildcard SSO extraction: the exact host, its subdomains, every strict
    // ancestor as a `LIKE` root (`%.example.com` covers `auth.example.com`
    // and `.sso.example.com`), plus curated cross-root secondaries for pairs
    // like `chatgpt.com`/`openai.com` that share no suffix — and the legacy
    // dot-boundary parent check. `httpOnly`, `secure`, and path columns are
    // selected unfiltered, so full SSO token chains survive intact.
    let mut roots = ancestor_roots(host);
    roots.extend(sso_secondaries(host));
    let sql = cookie_query(roots.len());
    let mut query = sqlx::query(&sql).bind(host);
    for root in &roots {
        query = query.bind(escape_like(root));
    }
    let rows = query.bind(MAX_COOKIES + 1).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    conn.close().await?;
    // Rows are owned in memory now; remove the temporary copy immediately,
    // before CPU-bound decryption (the `Drop` impl is the backstop).
    drop(shadow);
    if rows.len() > usize::try_from(MAX_COOKIES).map_err(|_| SyncError::UnsupportedFormat)? {
        return Err(SyncError::UnsupportedFormat);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SyncError::Decryption)?;
    let now = i64::try_from(now.as_secs()).map_err(|_| SyncError::Decryption)?;
    // Decryption/derivation are CPU work and do not occupy an async runtime worker.
    tokio::task::spawn_blocking(move || {
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
                // Legacy AES-128-CBC and AES-256-GCM share the `v10` prefix;
                // `decrypt_auto` dispatches on the OS secret shape.
                crypto::decrypt_auto(&encrypted, &domain, version, &safe_storage)?
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
    async fn extracts_subdomain_and_cross_root_sso_cookies()
    -> Result<(), Box<dyn std::error::Error>> {
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
    async fn shadow_copy_stages_wal_and_cleans_up_immediately()
    -> Result<(), Box<dyn std::error::Error>> {
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
    #[tokio::test]
    async fn gcm_encrypted_cookies_read_through_the_same_snapshot()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::crypto::{derive_gcm_key, encrypt_gcm_fixture};
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
}
