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

#[derive(Deserialize, Serialize)]
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
