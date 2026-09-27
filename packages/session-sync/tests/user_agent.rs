//! Integration tests for `session_sync::user_agent`.
//!
//! Moved out of `src/user_agent.rs` so the main source stays test-free.

use session_sync::BrowserSource;
use session_sync::user_agent::*;

#[test]
fn builds_channel_uas_without_guessing() -> Result<(), Box<dyn std::error::Error>> {
    let Some(chrome) = build_user_agent(BrowserSource::Chrome, "131.0.6778.87") else {
        return Err(std::io::Error::other("valid version builds").into());
    };
    assert!(chrome.starts_with("Mozilla/5.0 ("));
    assert!(chrome.contains("Chrome/131.0.6778.87"));
    assert!(!chrome.contains("Edg/"));
    // Brave intentionally ships the stock Chromium UA.
    assert_eq!(
        build_user_agent(BrowserSource::Brave, "131.0.6778.87"),
        Some(chrome)
    );
    let Some(edge) = build_user_agent(BrowserSource::Edge, "131.0.6778.87") else {
        return Err(std::io::Error::other("edge builds").into());
    };
    assert!(edge.ends_with(" Edg/131.0.6778.87"));
    // Malformed versions never produce a UA rather than a wrong one.
    for bad in [
        "",
        "abc",
        "131",
        ".131.0",
        "131.0.",
        "131..0",
        "131.0.6778.87 ",
    ] {
        assert_eq!(build_user_agent(BrowserSource::Chrome, bad), None);
    }
    Ok(())
}

#[test]
fn compares_dotted_versions_numerically() {
    assert!(version_greater("131.0.6778.87", "130.0.6723.58"));
    assert!(!version_greater("130.0.6723.58", "131.0.6778.87"));
    assert!(!version_greater("131.0.6778.87", "131.0.6778.87"));
    assert!(version_greater("131.0.6778.100", "131.0.6778.87"));
}

#[cfg(target_os = "windows")]
#[test]
fn picks_highest_installed_version_dir() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    for version in ["130.0.6723.58", "131.0.6778.87", "not-a-version"] {
        std::fs::create_dir(root.path().join(version))?;
    }
    std::fs::write(root.path().join("chrome.exe"), b"binary")?;
    assert_eq!(
        application_dir_version(std::slice::from_ref(&root.path().to_owned())),
        Some("131.0.6778.87".to_string())
    );
    let empty = tempfile::tempdir()?;
    assert_eq!(
        application_dir_version(std::slice::from_ref(&empty.path().to_owned())),
        None
    );
    Ok(())
}

#[test]
fn source_lookup_never_panics_and_stays_bounded() {
    // Environment-dependent: installed or not, the result must be a
    // well-formed UA or `None` — never a guess, never a panic.
    for browser in [
        BrowserSource::Chrome,
        BrowserSource::Brave,
        BrowserSource::Edge,
    ] {
        if let Some(ua) = source_user_agent(browser) {
            assert!(ua.len() <= 512);
            assert!(ua.starts_with("Mozilla/5.0 ("));
        }
    }
}
