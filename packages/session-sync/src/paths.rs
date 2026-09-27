#![deny(unsafe_code)]
//! Single source of truth for source-browser profile paths on disk.
//!
//! All browser-data resolution flows through here so cookie extraction,
//! `Local State` key reads, and `LocalStorage` hydration can never disagree
//! about where a profile lives (they once did for Edge on Windows). The
//! per-browser directory components are canonical in
//! `credential_vault::BrowserKey`; this module only contributes the platform
//! base directory and the profile-selection policy.

use crate::{BrowserSource, SyncError};
use std::path::{Path, PathBuf};

fn vault_key(browser: BrowserSource) -> credential_vault::BrowserKey {
    match browser {
        BrowserSource::Chrome => credential_vault::BrowserKey::Chrome,
        BrowserSource::Brave => credential_vault::BrowserKey::Brave,
        BrowserSource::Edge => credential_vault::BrowserKey::Edge,
    }
}

/// Platform local-app-data root for browser profiles.
///
/// Windows resolves via `dirs::data_local_dir()` — `%LOCALAPPDATA%`, the
/// *local* non-roaming hive — with `%LOCALAPPDATA%` env and
/// `home/AppData/Local` as progressively weaker fallbacks. `%APPDATA%`
/// (roaming) is never used: browser profiles live in `Local`, and roaming
/// redirection would point at a directory Chrome never writes to.
#[must_use]
pub fn local_base(home: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        dirs::data_local_dir()
            .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
            .unwrap_or_else(|| home.join("AppData").join("Local"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        home.join("Library").join("Application Support")
    }
}

/// `<base>/<browser-user-data>` for `browser` (no profile segment).
#[must_use]
pub fn user_data_dir(browser: BrowserSource, home: &Path) -> PathBuf {
    let mut dir = local_base(home);
    for component in vault_key(browser).profile_root_components() {
        dir.push(component);
    }
    dir
}

/// Cookie database candidates in priority order: modern
/// `<profile>/Network/Cookies` first, legacy `<profile>/Cookies` second.
#[must_use]
pub fn cookie_db_candidates(profile_dir: &Path) -> [PathBuf; 2] {
    [
        profile_dir.join("Network").join("Cookies"),
        profile_dir.join("Cookies"),
    ]
}

/// Resolve the on-disk profile directory for a validated profile name.
///
/// Selection is validated upstream to `Default` or `Profile N`, so joining is
/// traversal-safe. If `Default` was selected but only `Profile 1` exists on
/// disk (manual profile deletion leaves exactly this shape), the surviving
/// profile is used automatically. Any other miss fails with the exact
/// absolute path that was attempted, so a `ProfileUnavailable` diagnostic
/// always names the directory to inspect.
///
/// # Errors
/// Returns `ProfileUnavailable` (carrying the attempted absolute path) when
/// neither the selected profile nor the `Default` → `Profile 1` fallback
/// exists on disk.
pub async fn resolve_profile_dir(
    user_data_dir: &Path,
    profile: &str,
) -> Result<PathBuf, SyncError> {
    let selected = user_data_dir.join(profile);
    if is_dir(&selected).await {
        return Ok(selected);
    }
    if profile == "Default" {
        let fallback = user_data_dir.join("Profile 1");
        if is_dir(&fallback).await {
            return Ok(fallback);
        }
    }
    Err(SyncError::ProfileUnavailable { path: selected })
}

async fn is_dir(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
}
