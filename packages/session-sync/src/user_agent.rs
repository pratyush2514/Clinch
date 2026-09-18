#![deny(unsafe_code)]
//! Source-browser User-Agent mirroring for anti-bot fidelity.
//!
//! Portals fingerprint the `User-Agent` (and its Client-Hints derivatives)
//! alongside cookies; replaying a synced session under a mismatched UA is a
//! classic bot signal. The UA string is not stored in the cookie database —
//! it is derived from the installed source browser's version, read from
//! OS-standard install metadata:
//! - Windows: `<Application>` directories contain one version-named
//!   subdirectory per installed build (`...\Application\131.0.6778.87\`).
//! - macOS: `<Name>.app/Contents/Info.plist` carries
//!   `CFBundleShortVersionString`.
//!
//! Everything here is best-effort and allocation-bounded: unknown versions
//! yield `None` (the managed browser keeps its own UA) rather than a guess,
//! so a wrong UA can never be worse than the status quo.

use crate::BrowserSource;
use std::path::PathBuf;

/// Longest version token accepted from install metadata (`major.minor.build.patch`).
const MAX_VERSION_LEN: usize = 32;

/// Build the full UA string for `browser` at `version` on this OS.
/// Brave ships the stock Chromium UA; Edge appends its `Edg/` token.
#[must_use]
pub fn build_user_agent(browser: BrowserSource, version: &str) -> Option<String> {
    if !is_version(version) {
        return None;
    }
    let platform = platform_token();
    let chromium = format!(
        "Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{version} Safari/537.36"
    );
    Some(match browser {
        BrowserSource::Edge => format!("{chromium} Edg/{version}"),
        BrowserSource::Chrome | BrowserSource::Brave => chromium,
    })
}

/// Detect the installed source-browser version, then build its UA.
/// Returns `None` when the browser is not installed or its version cannot be
/// parsed — the caller keeps the managed browser's native UA in that case.
#[must_use]
pub fn source_user_agent(browser: BrowserSource) -> Option<String> {
    build_user_agent(browser, &installed_version(browser)?)
}

#[cfg(target_os = "windows")]
fn platform_token() -> &'static str {
    "Windows NT 10.0; Win64; x64"
}

#[cfg(target_os = "macos")]
fn platform_token() -> &'static str {
    "Macintosh; Intel Mac OS X 10_15_7"
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_token() -> &'static str {
    "X11; Linux x86_64"
}

fn is_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= MAX_VERSION_LEN
        && version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        && version.contains('.')
        && version.split('.').all(|part| !part.is_empty())
}

/// Read the installed version for `browser` from OS install metadata.
fn installed_version(browser: BrowserSource) -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        application_dir_version(&windows_application_dirs(browser))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = browser;
        info_plist_version(macos_app_path(browser))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = browser;
        None
    }
}

/// Candidate `Application` directories (machine + user installs) for `browser`.
#[cfg(target_os = "windows")]
fn windows_application_dirs(browser: BrowserSource) -> Vec<PathBuf> {
    let relative: PathBuf = match browser {
        BrowserSource::Chrome => ["Google", "Chrome", "Application"].iter().collect(),
        BrowserSource::Brave => ["BraveSoftware", "Brave-Browser", "Application"]
            .iter()
            .collect(),
        BrowserSource::Edge => ["Microsoft", "Edge", "Application"].iter().collect(),
    };
    let mut dirs = Vec::with_capacity(3);
    for root in [
        std::env::var_os("PROGRAMFILES"),
        std::env::var_os("PROGRAMFILES(X86)"),
        std::env::var_os("LOCALAPPDATA"),
    ]
    .into_iter()
    .flatten()
    {
        dirs.push(PathBuf::from(root).join(&relative));
    }
    dirs
}

/// Highest version-named subdirectory (`131.0.6778.87`) across `dirs`.
#[cfg(target_os = "windows")]
fn application_dir_version(dirs: &[PathBuf]) -> Option<String> {
    let mut best: Option<String> = None;
    for dir in dirs {
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_version(&name)
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
                && best
                    .as_ref()
                    .is_none_or(|current| version_greater(&name, current))
            {
                best = Some(name);
            }
        }
    }
    best
}

/// Numeric dotted-version comparison (`131.0.6778.87` > `130.0.1.0`).
fn version_greater(left: &str, right: &str) -> bool {
    let parse = |version: &str| {
        version
            .split('.')
            .map(|part| part.parse::<u32>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    parse(left) > parse(right)
}

#[cfg(target_os = "macos")]
fn macos_app_path(browser: BrowserSource) -> PathBuf {
    let name = match browser {
        BrowserSource::Chrome => "Google Chrome.app",
        BrowserSource::Brave => "Brave Browser.app",
        BrowserSource::Edge => "Microsoft Edge.app",
    };
    PathBuf::from("/Applications").join(name)
}

/// Extract `CFBundleShortVersionString` with a bounded string scan (no plist
/// dependency for one fallback path). Fails closed on any shape deviation.
#[cfg(target_os = "macos")]
fn info_plist_version(app: PathBuf) -> Option<String> {
    let text = std::fs::read_to_string(app.join("Contents/Info.plist")).ok()?;
    if text.len() > 1_048_576 {
        return None;
    }
    let key = "<key>CFBundleShortVersionString</key>";
    let start = text.find(key)? + key.len();
    let value = text[start..].trim_start();
    let value = value.strip_prefix("<string>")?;
    let end = value.find("</string>")?;
    let version = value[..end].trim();
    is_version(version).then(|| version.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
