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
        Ok(Err(_)) => return PreparedSync::ManualLogin(FallbackReason::KeychainUnavailable),
        Err(_) => return PreparedSync::ManualLogin(FallbackReason::TimedOut),
    };
    let Some(host) = request.portal().host_str() else {
        return PreparedSync::ManualLogin(FallbackReason::ProfileUnavailable);
    };
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        read_profile(&request.profile_directory(home), host, secret),
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
    #[cfg(not(target_os = "macos"))]
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
