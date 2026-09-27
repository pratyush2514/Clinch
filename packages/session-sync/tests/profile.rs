//! Integration tests for `session_sync::profile`.
//!
//! Moved out of `src/profile.rs` so the main source stays test-free.

use proptest::prelude::*;
use session_sync::profile::*;
use std::path::Path;

#[test]
fn requires_consent_and_rejects_traversal() {
    for (profile, consent) in [
        ("Default", false),
        ("../../Cookies", true),
        ("Profile /1", true),
    ] {
        assert!(
            SyncRequest {
                browser: BrowserSource::Chrome,
                profile: profile.into(),
                portal_url: "https://portal.example.com".into(),
                consent
            }
            .validate()
            .is_err()
        );
    }
}
#[test]
fn rejects_non_web_and_credentials() {
    for url in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "http://example.com",
        "https://u:p@example.com",
        "https://127.0.0.1",
    ] {
        assert!(validate_portal(url).is_err());
    }
}
proptest! {
    #[test]
    fn valid_numbered_profiles_stay_under_browser_root(n in 0u32..100_000) {
        let req = SyncRequest { browser: BrowserSource::Chrome, profile: format!("Profile {n}"),
            portal_url: "https://example.com".into(), consent: true }.validate()?;
        // The profile segment must stay nested under the browser root on
        // every OS; traversal is rejected by `validate` above.
        let dir = req.profile_directory(Path::new("/home/test"));
        let expected = format!("Profile {n}");
        prop_assert!(dir.ends_with(&expected));
        prop_assert!(dir.components().count() > 3);
    }
    #[test]
    fn edge_profiles_resolve_without_traversal(n in 0u32..100_000) {
        let req = SyncRequest { browser: BrowserSource::Edge, profile: format!("Profile {n}"),
            portal_url: "https://example.com".into(), consent: true }.validate()?;
        let dir = req.profile_directory(Path::new("/home/test"));
        let expected = format!("Profile {n}");
        prop_assert!(dir.ends_with(&expected));
        // Canonical roots differ per OS (`Microsoft/Edge/User Data` on
        // Windows, `Microsoft Edge` elsewhere) — both nest Edge profiles
        // under a vendor directory, never the filesystem root.
        #[cfg(target_os = "windows")]
        {
            let tail: std::path::PathBuf =
                ["Microsoft", "Edge", "User Data", &expected].iter().collect();
            prop_assert!(dir.ends_with(&tail));
        }
        #[cfg(not(target_os = "windows"))]
        {
            prop_assert!(dir.to_string_lossy().contains("Microsoft Edge"));
        }
    }
    #[test]
    fn arbitrary_urls_never_panic(value in ".{0,300}") {
        if let Ok(url) = validate_portal(&value) {
            prop_assert_eq!(url.scheme(), "https");
            prop_assert!(url.username().is_empty());
            prop_assert!(url.password().is_none());
        }
    }
}
