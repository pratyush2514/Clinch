//! Integration tests for `credential_vault`.
//!
//! Moved out of `src/lib.rs` so the main source stays test-free.

use credential_vault::*;

#[test]
fn browser_key_names_are_stable() {
    assert_eq!(
        BrowserKey::Chrome.service_account(),
        ("Chrome Safe Storage", "Chrome")
    );
    assert_eq!(
        BrowserKey::Brave.service_account(),
        ("Brave Safe Storage", "Brave")
    );
    assert_eq!(
        BrowserKey::Edge.service_account(),
        ("Microsoft Edge Safe Storage", "Microsoft Edge")
    );
    // Canonical on-disk roots. Windows roots always nest under a
    // per-browser `User Data` level — including Edge.
    #[cfg(target_os = "windows")]
    {
        assert_eq!(
            BrowserKey::Chrome.profile_root_components(),
            &["Google", "Chrome", "User Data"]
        );
        assert_eq!(
            BrowserKey::Brave.profile_root_components(),
            &["BraveSoftware", "Brave-Browser", "User Data"]
        );
        assert_eq!(
            BrowserKey::Edge.profile_root_components(),
            &["Microsoft", "Edge", "User Data"]
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(
            BrowserKey::Edge.profile_root_components(),
            &["Microsoft Edge"]
        );
    }
}

#[cfg(target_os = "windows")]
#[test]
fn local_state_without_dpapi_prefix_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD.encode(b"not-dpapi");
    let document = serde_json::json!({ "os_crypt": { "encrypted_key": raw } });
    let bytes = serde_json::to_vec(&document)?;
    assert!(matches!(
        decrypt_local_state_key(&bytes),
        Err(VaultError::Unavailable)
    ));
    assert!(matches!(
        decrypt_local_state_key(b"{not-json"),
        Err(VaultError::Unavailable)
    ));
    Ok(())
}

#[cfg(target_os = "windows")]
#[test]
fn app_bound_shape_detection_ignores_legacy_keys() -> Result<(), Box<dyn std::error::Error>> {
    use base64::Engine;
    let appb = base64::engine::general_purpose::STANDARD.encode(b"APPB-sealed");
    let document = serde_json::json!({ "os_crypt": { "app_bound_encrypted_key": appb } });
    assert!(has_app_bound_bytes(&serde_json::to_vec(&document)?));
    let legacy = serde_json::json!({ "os_crypt": { "encrypted_key": appb } });
    assert!(!has_app_bound_bytes(&serde_json::to_vec(&legacy)?));
    assert!(!has_app_bound_bytes(b"{not-json"));
    Ok(())
}

#[cfg(not(target_os = "windows"))]
#[test]
fn app_bound_detection_is_windows_only() {
    for browser in [BrowserKey::Chrome, BrowserKey::Brave, BrowserKey::Edge] {
        assert!(!has_app_bound_key(browser));
    }
}
