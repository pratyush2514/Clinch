//! Integration tests for `session_sync::paths`.
//!
//! Moved out of `src/paths.rs` so the main source stays test-free.

#[cfg(target_os = "windows")]
use session_sync::BrowserSource;
use session_sync::SyncError;
use session_sync::paths::*;
use std::path::Path;
#[cfg(target_os = "windows")]
use std::path::PathBuf;

#[cfg(target_os = "windows")]
#[test]
fn brave_paths_match_the_required_windows_layout() {
    // `%LOCALAPPDATA%\BraveSoftware\Brave-Browser\User Data` and
    // `... \User Data\Local State`, with `Network\Cookies` first.
    let user_data = user_data_dir(BrowserSource::Brave, Path::new("C:/Users/test"));
    let tail: PathBuf = ["BraveSoftware", "Brave-Browser", "User Data"]
        .iter()
        .collect();
    assert!(user_data.ends_with(tail));
}

#[cfg(target_os = "windows")]
#[test]
fn edge_shares_the_user_data_nesting_on_windows() {
    let user_data = user_data_dir(BrowserSource::Edge, Path::new("C:/Users/test"));
    let tail: PathBuf = ["Microsoft", "Edge", "User Data"].iter().collect();
    assert!(user_data.ends_with(tail));
}

#[test]
fn cookie_candidates_prefer_the_network_subdirectory() {
    let [modern, legacy] = cookie_db_candidates(Path::new("/profiles/Default"));
    assert!(modern.ends_with(Path::new("Network/Cookies")));
    assert!(legacy.ends_with("Cookies"));
}

#[tokio::test]
async fn missing_default_falls_back_to_profile_one() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let user_data = root.path().join("User Data");
    tokio::fs::create_dir_all(user_data.join("Profile 1")).await?;
    let resolved = resolve_profile_dir(&user_data, "Default").await?;
    assert_eq!(resolved, user_data.join("Profile 1"));
    // An explicit numbered profile never falls back sideways.
    match resolve_profile_dir(&user_data, "Profile 2").await {
        Err(SyncError::ProfileUnavailable { path }) => {
            assert_eq!(path, user_data.join("Profile 2"));
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("Profile 2 is absent"),
    }
    Ok(())
}

#[tokio::test]
async fn missing_default_without_fallback_names_the_path() -> Result<(), Box<dyn std::error::Error>>
{
    let root = tempfile::tempdir()?;
    let user_data = root.path().join("User Data");
    tokio::fs::create_dir_all(&user_data).await?;
    match resolve_profile_dir(&user_data, "Default").await {
        Err(SyncError::ProfileUnavailable { path }) => {
            assert_eq!(path, user_data.join("Default"));
            // The `Display` impl renders this path, so the diagnostic
            // names the exact directory to inspect.
            assert!(path.display().to_string().contains("Default"));
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("no profile on disk"),
    }
    Ok(())
}
