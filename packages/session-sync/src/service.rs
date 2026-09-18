#![deny(unsafe_code)]
use crate::{BrowserSource, Cookie, SyncError, ValidatedRequest, read_profile};
use serde::Serialize;
use std::{path::Path, time::Duration};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    UnsupportedPlatform,
    KeychainUnavailable,
    ProfileUnavailable,
    UnsupportedFormat,
    DecryptionFailed,
    NoCookies,
    TimedOut,
    InjectionFailed,
    /// Cookies injected but the portal still shows a login/2FA challenge;
    /// the embedded in-app auth panel (not an external window) continues.
    ReauthRequired,
    /// The profile seals its cookie key with OS app-bound encryption
    /// (Chrome 127+ on Windows): only the browser's own elevation service
    /// can unwrap it, so zero-touch import is impossible from this source.
    AppBoundLocked,
}

pub enum PreparedSync {
    Cookies(Vec<Cookie>),
    ManualLogin(FallbackReason),
}

impl From<&SyncError> for FallbackReason {
    fn from(error: &SyncError) -> Self {
        match error {
            SyncError::UnsupportedFormat => Self::UnsupportedFormat,
            SyncError::Decryption | SyncError::WorkerFailed => Self::DecryptionFailed,
            SyncError::MissingCookies => Self::NoCookies,
            _ => Self::ProfileUnavailable,
        }
    }
}

/// Extract after consent validation; extraction failures become explicit fallback.
/// Cookie injection alone does not establish that the portal accepted the session.
pub async fn prepare(request: &ValidatedRequest, home: &Path) -> PreparedSync {
    let browser = match request.browser() {
        BrowserSource::Chrome => credential_vault::BrowserKey::Chrome,
        BrowserSource::Brave => credential_vault::BrowserKey::Brave,
        BrowserSource::Edge => credential_vault::BrowserKey::Edge,
    };
    let secret = match tokio::time::timeout(
        Duration::from_mins(2),
        credential_vault::browser_safe_storage(browser),
    )
    .await
    {
        Ok(Ok(secret)) => secret,
        Ok(Err(credential_vault::VaultError::UnsupportedPlatform)) => {
            return PreparedSync::ManualLogin(FallbackReason::UnsupportedPlatform);
        }
        Ok(Err(_)) => {
            // Chrome 127+ seals its key with App-Bound encryption: tell the
            // UI exactly that (switch to Brave) instead of a dead-end
            // "keychain unavailable" that reads as a bug.
            let app_bound =
                tokio::task::spawn_blocking(move || credential_vault::has_app_bound_key(browser))
                    .await
                    .unwrap_or(false);
            return PreparedSync::ManualLogin(if app_bound {
                FallbackReason::AppBoundLocked
            } else {
                FallbackReason::KeychainUnavailable
            });
        }
        Err(_) => return PreparedSync::ManualLogin(FallbackReason::TimedOut),
    };
    let Some(host) = request.portal().host_str() else {
        return PreparedSync::ManualLogin(FallbackReason::ProfileUnavailable);
    };
    // Resolve the on-disk profile first: a missing `Default` falls back to a
    // surviving `Profile 1`, and a miss names the attempted path in the error.
    let profile_dir = match crate::paths::resolve_profile_dir(
        &crate::paths::user_data_dir(request.browser(), home),
        request.profile_name(),
    )
    .await
    {
        Ok(dir) => dir,
        Err(error) => return PreparedSync::ManualLogin(FallbackReason::from(&error)),
    };
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        read_profile(&profile_dir, host, secret),
    )
    .await;
    match result {
        Ok(Ok(cookies)) => PreparedSync::Cookies(cookies),
        Ok(Err(error)) => PreparedSync::ManualLogin(FallbackReason::from(&error)),
        Err(_) => PreparedSync::ManualLogin(FallbackReason::TimedOut),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let request = crate::SyncRequest {
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
}
