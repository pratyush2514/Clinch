//! Integration tests for `session_sync::service`.
//!
//! Moved out of `src/service.rs` so the main source stays test-free.

use session_sync::service::*;
use session_sync::{BrowserSource, SyncError, SyncRequest};
use std::path::Path;

#[test]
fn errors_select_explicit_fallbacks() {
    assert_eq!(
        FallbackReason::from(&SyncError::MissingCookies),
        FallbackReason::NoCookies
    );
    assert_eq!(
        FallbackReason::from(&SyncError::Decryption),
        FallbackReason::DecryptionFailed
    );
    assert_eq!(
        FallbackReason::from(&SyncError::UnsupportedFormat),
        FallbackReason::UnsupportedFormat
    );
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[tokio::test]
async fn unsupported_host_offers_manual_login() -> Result<(), SyncError> {
    let request = SyncRequest {
        browser: BrowserSource::Chrome,
        profile: "Default".into(),
        portal_url: "https://example.com".into(),
        consent: true,
    }
    .validate()?;
    assert!(matches!(
        prepare(&request, Path::new("/nonexistent")).await,
        PreparedSync::ManualLogin(FallbackReason::UnsupportedPlatform)
    ));
    Ok(())
}
