#![deny(unsafe_code)]
//! Read-only Safe Storage access. No creation, mutation, or secret-bearing errors.

use zeroize::Zeroizing;

#[derive(Clone, Copy)]
pub enum BrowserKey {
    Chrome,
    Brave,
}

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("Cookie import is available on macOS only")]
    UnsupportedPlatform,
    #[error("Safe Storage access was denied or unavailable")]
    Unavailable,
    #[error("Safe Storage worker failed")]
    WorkerFailed,
}

/// Read an existing browser Safe Storage secret after the caller obtains consent.
///
/// # Errors
/// Returns a sanitized error for unsupported hosts, denied access, or worker failure.
pub async fn browser_safe_storage(browser: BrowserKey) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    tokio::task::spawn_blocking(move || read_secret(browser))
        .await
        .map_err(|_| VaultError::WorkerFailed)?
}

#[cfg(target_os = "macos")]
fn read_secret(browser: BrowserKey) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    use keyring_core::api::CredentialStoreApi;
    let (service, account) = match browser {
        BrowserKey::Chrome => ("Chrome Safe Storage", "Chrome"),
        BrowserKey::Brave => ("Brave Safe Storage", "Brave"),
    };
    // Use a local store instead of changing keyring-core's process-global default.
    let store =
        apple_native_keyring_store::keychain::Store::new().map_err(|_| VaultError::Unavailable)?;
    let entry = store
        .build(service, account, None)
        .map_err(|_| VaultError::Unavailable)?;
    let secret = Zeroizing::new(entry.get_secret().map_err(|_| VaultError::Unavailable)?);
    if secret.is_empty() {
        return Err(VaultError::Unavailable);
    }
    Ok(secret)
}

#[cfg(not(target_os = "macos"))]
fn read_secret(_browser: BrowserKey) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    Err(VaultError::UnsupportedPlatform)
}
