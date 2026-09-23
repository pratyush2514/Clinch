#![deny(unsafe_code)]
//! Read-only Safe Storage access. No creation, mutation, or secret-bearing errors.

use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserKey {
    Chrome,
    Brave,
    Edge,
}

impl BrowserKey {
    #[must_use]
    pub fn service_account(self) -> (&'static str, &'static str) {
        match self {
            Self::Chrome => ("Chrome Safe Storage", "Chrome"),
            Self::Brave => ("Brave Safe Storage", "Brave"),
            Self::Edge => ("Microsoft Edge Safe Storage", "Microsoft Edge"),
        }
    }

    /// Canonical user-data directory components below the platform
    /// local-app-data root. Single source of truth for every crate that
    /// touches a source browser profile on disk — session-sync resolves its
    /// cookie/`Local State` paths from these components, so the two crates
    /// cannot disagree (as they once did for Edge on Windows).
    #[must_use]
    pub fn profile_root_components(self) -> &'static [&'static str] {
        #[cfg(target_os = "windows")]
        {
            match self {
                Self::Chrome => &["Google", "Chrome", "User Data"],
                Self::Brave => &["BraveSoftware", "Brave-Browser", "User Data"],
                Self::Edge => &["Microsoft", "Edge", "User Data"],
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            match self {
                Self::Chrome => &["Google", "Chrome"],
                Self::Brave => &["BraveSoftware", "Brave-Browser"],
                Self::Edge => &["Microsoft Edge"],
            }
        }
    }
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

/// Detect Chrome 127+ App-Bound key material (`os_crypt.app_bound_encrypted_key`,
/// `APPB` prefix) without attempting decryption. When present, plain
/// user-DPAPI unwrapping cannot succeed — only the browser's own SYSTEM-level
/// elevation service holds the other half — so the caller can explain the
/// fallback (switch source to Brave) instead of a generic access error.
/// Blocking filesystem I/O: call from `spawn_blocking`, never inline in
/// `async fn`.
#[must_use]
pub fn has_app_bound_key(browser: BrowserKey) -> bool {
    #[cfg(target_os = "windows")]
    {
        let Some(path) = local_state_path(browser) else {
            return false;
        };
        let Ok(document) = std::fs::read(path) else {
            return false;
        };
        has_app_bound_bytes(&document)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = browser;
        false
    }
}

/// Pure shape check for `has_app_bound_key`, kept testable without a real
/// browser profile on disk.
#[cfg(target_os = "windows")]
fn has_app_bound_bytes(document: &[u8]) -> bool {
    use base64::Engine;
    serde_json::from_slice::<serde_json::Value>(document)
        .ok()
        .and_then(|parsed| {
            parsed
                .pointer("/os_crypt/app_bound_encrypted_key")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .is_ok_and(|bytes| bytes.starts_with(b"APPB"))
        })
}

#[cfg(target_os = "macos")]
fn read_secret(browser: BrowserKey) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    use keyring_core::api::CredentialStoreApi;
    let (service, account) = browser.service_account();
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

#[cfg(target_os = "windows")]
fn read_secret(browser: BrowserKey) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    // Chrome 80+: the AES-256-GCM cookie key is DPAPI-wrapped in `Local State`.
    // Resolve the key off the async runtime; failures map to `Unavailable`
    // without leaking filesystem or DPAPI details.
    let local_state = local_state_path(browser).ok_or(VaultError::Unavailable)?;
    let bytes = std::fs::read(local_state).map_err(|_| VaultError::Unavailable)?;
    let key = decrypt_local_state_key(&bytes)?;
    if key.is_empty() {
        return Err(VaultError::Unavailable);
    }
    Ok(key)
}

/// Locate `<user-data-dir>/Local State` for the given browser on Windows.
/// The base resolves via `dirs::data_local_dir()` (`%LOCALAPPDATA%` — the
/// *local*, non-roaming app-data root), never `%APPDATA%`.
#[cfg(target_os = "windows")]
fn local_state_path(browser: BrowserKey) -> Option<std::path::PathBuf> {
    let root = dirs::data_local_dir()
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from))
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(std::path::PathBuf::from)
                .map(|home| home.join("AppData").join("Local"))
        })?;
    let mut path = root;
    for component in browser.profile_root_components() {
        path.push(component);
    }
    Some(path.join("Local State"))
}

/// Extract and DPAPI-unwrap `os_crypt.encrypted_key` from a `Local State` document.
///
/// This is the one function in the crate where `unsafe` is the point: DPAPI
/// is an OS FFI boundary with no safe wrapper at this dependency weight.
/// Every `unsafe` block below carries its own `SAFETY` justification.
/// # Errors
/// Returns sanitized `Unavailable` for malformed documents or DPAPI failures.
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn decrypt_local_state_key(document: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    use base64::Engine;
    use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptUnprotectData};
    // Chrome prefixes the DPAPI blob with ASCII `DPAPI`.
    const DPAPI_PREFIX: &[u8] = b"DPAPI";

    let parsed: serde_json::Value =
        serde_json::from_slice(document).map_err(|_| VaultError::Unavailable)?;
    let encoded = parsed
        .pointer("/os_crypt/encrypted_key")
        .and_then(serde_json::Value::as_str)
        .ok_or(VaultError::Unavailable)?;
    let mut wrapped = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| VaultError::Unavailable)?;
    if wrapped.starts_with(DPAPI_PREFIX) {
        wrapped.drain(..DPAPI_PREFIX.len());
    } else {
        return Err(VaultError::Unavailable);
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(wrapped.len()).map_err(|_| VaultError::Unavailable)?,
        pbData: wrapped.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: input/output are valid `CRYPT_INTEGER_BLOB`s; all optional
    // string/prompt parameters are null; the call has no shared-memory
    // aliasing with Clinch-owned buffers beyond `wrapped`.
    let ok = unsafe {
        CryptUnprotectData(
            &raw const input,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &raw mut output,
        )
    };
    if ok == 0 {
        return Err(VaultError::Unavailable);
    }
    // SAFETY: on success the OS owns `output.pbData` with `cbData` bytes;
    // it is copied out immediately and released with `LocalFree`.
    let key = unsafe {
        let slice = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
        let key = Zeroizing::new(slice.to_vec());
        windows_sys::Win32::Foundation::LocalFree(output.pbData.cast());
        key
    };
    Ok(key)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn read_secret(_browser: BrowserKey) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    Err(VaultError::UnsupportedPlatform)
}
