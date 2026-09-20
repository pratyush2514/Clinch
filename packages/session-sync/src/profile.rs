#![deny(unsafe_code)]
use crate::SyncError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use url::Url;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserSource {
    Chrome,
    Brave,
    Edge,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SyncRequest {
    pub browser: BrowserSource,
    pub profile: String,
    pub portal_url: String,
    pub consent: bool,
}

pub struct ValidatedRequest {
    browser: BrowserSource,
    profile: String,
    portal: Url,
}

impl SyncRequest {
    /// Validate the request before any disk or Keychain access.
    ///
    /// # Errors
    /// Rejects missing consent, unsafe URLs, and profile path traversal.
    pub fn validate(self) -> Result<ValidatedRequest, SyncError> {
        if !self.consent {
            return Err(SyncError::ConsentRequired);
        }
        let portal = validate_portal(&self.portal_url)?;
        let numbered = self.profile.strip_prefix("Profile ").is_some_and(|n| {
            !n.is_empty() && n.len() <= 8 && n.bytes().all(|c| c.is_ascii_digit())
        });
        if self.profile != "Default" && !numbered {
            return Err(SyncError::InvalidProfile);
        }
        Ok(ValidatedRequest {
            browser: self.browser,
            profile: self.profile,
            portal,
        })
    }
}

/// Parse a portal address without allowing local files, scripts, or URL credentials.
///
/// # Errors
/// Returns an error for non-HTTPS URLs, IP addresses, or embedded credentials.
pub fn validate_portal(value: &str) -> Result<Url, SyncError> {
    let url = Url::parse(value).map_err(|_| SyncError::InvalidPortal)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
        || url.domain().is_none()
    {
        return Err(SyncError::InvalidPortal);
    }
    Ok(url)
}

impl ValidatedRequest {
    #[must_use]
    pub fn browser(&self) -> BrowserSource {
        self.browser
    }
    #[must_use]
    pub fn portal(&self) -> &Url {
        &self.portal
    }
    #[must_use]
    pub fn profile_name(&self) -> &str {
        &self.profile
    }
    /// Unresolved `<user-data>/<profile>` join. Prefer
    /// `crate::paths::resolve_profile_dir` at the call site: it verifies the
    /// directory exists, falls back `Default` → `Profile 1`, and reports the
    /// attempted path on failure.
    #[must_use]
    pub fn profile_directory(&self, home: &Path) -> PathBuf {
        crate::paths::user_data_dir(self.browser, home).join(&self.profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
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
}
