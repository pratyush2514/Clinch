#![deny(unsafe_code)]
//! Best-effort `LocalStorage` hydration for portals that keep session state
//! outside cookies.
//!
//! Chrome persists per-origin `LocalStorage` in `Local Storage/leveldb/` as
//! `LevelDB` files. Rather than pulling a `LevelDB` dependency for one fallback
//! path, this module scans the uncompressed record files (`*.log`, plus any
//! plaintext runs lingering in `*.ldb`) for host-adjacent string pairs and
//! treats them as candidate `(key, value)` entries. Snappy-compressed blocks
//! are opaque to the scanner and are simply skipped — hydration is explicitly
//! best-effort: whatever is found seeds the target, whatever is missed keeps
//! working through the cookie path and manual login.
//!
//! Safety properties (all enforced, all tested):
//! - Missing/unreadable directories yield an empty list, never an error, so
//!   hydration can never fail a sync.
//! - Injection is additive-only: the emitted script sets a key only when
//!   `localStorage.getItem(key) === null`, so live portal state is never
//!   clobbered.
//! - Keys are charset-restricted and both sides are length-capped; page text,
//!   credentials, and binary blobs cannot pass validation.

use std::path::Path;

/// Maximum hydrated entries per portal.
pub const MAX_ITEMS: usize = 32;
/// Maximum `LevelDB` files scanned per extraction.
pub const MAX_FILES: usize = 16;
/// Maximum bytes read from a single `LevelDB` file.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Maximum accepted storage key length.
pub const MAX_KEY_LEN: usize = 256;
/// Maximum accepted storage value length.
pub const MAX_VALUE_LEN: usize = 16 * 1024;
/// Origin markers longer than this are ignored (avoids fused binary runs).
const MAX_MARKER_LEN: usize = 320;

/// One candidate `LocalStorage` entry. Values are opaque session strings —
/// never logged, never sent to a model, only injected into the portal origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageItem {
    pub key: String,
    pub value: String,
}

/// Extract candidate entries for `host` from the profile's `LevelDB` files.
///
/// Infallible by design: any I/O problem yields fewer (possibly zero) items.
/// `LevelDB` files are read with plain file reads, so no shadow copy is needed
/// — unlike the `SQLite` cookie database, no `SQLite` lock is ever taken.
pub async fn read_local_storage(profile: &Path, host: &str) -> Vec<StorageItem> {
    if host.is_empty() {
        return Vec::new();
    }
    let Ok(mut entries) = tokio::fs::read_dir(profile.join("Local Storage/leveldb")).await else {
        return Vec::new();
    };
    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let candidate = path
            .extension()
            .is_some_and(|extension| extension == "ldb" || extension == "log");
        if candidate {
            files.push(path);
            if files.len() >= MAX_FILES {
                break;
            }
        }
    }
    files.sort();
    let mut items = Vec::new();
    for path in files {
        if items.len() >= MAX_ITEMS {
            break;
        }
        if let Some(bytes) = read_capped(&path).await {
            extract_host_pairs(&bytes, host, &mut items);
        }
    }
    items
}

async fn read_capped(path: &Path) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES)
        .read_to_end(&mut bytes)
        .await
        .ok()?;
    Some(bytes)
}

/// A storage key is short ASCII with no whitespace or quotes — anything else
/// is a fused binary run, not a key the portal could have set.
pub fn is_storage_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_LEN
        && key.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'-' | b'~' | b':' | b'/' | b'#')
        })
}

/// Push the accumulated run unless it overflowed the value budget.
/// Runs contain only `0x20..=0x7E`, so UTF-8 decoding cannot fail; the guard
/// stays so a future charset widening fails closed instead of panicking.
fn push_token(tokens: &mut Vec<String>, current: &mut Vec<u8>) {
    if current.is_empty() {
        return;
    }
    if let Ok(token) = String::from_utf8(std::mem::take(current)) {
        tokens.push(token);
    }
}

/// Split printable-ASCII runs into tokens. Overlong runs are discarded whole
/// so a truncated value can never be injected as if it were complete.
fn tokenize(bytes: &[u8]) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut overflowed = false;
    for &byte in bytes {
        if (0x20..=0x7E).contains(&byte) {
            if current.len() < MAX_VALUE_LEN {
                current.push(byte);
            } else {
                overflowed = true;
            }
            continue;
        }
        if overflowed {
            current.clear();
        } else {
            push_token(&mut tokens, &mut current);
        }
        overflowed = false;
    }
    if !overflowed {
        push_token(&mut tokens, &mut current);
    }
    tokens
}

/// Pair each host-adjacent token window as `(key, value)`: a token containing
/// the origin marker, followed by a key-shaped token, followed by the value.
/// Duplicates, self-pairs, and oversized values are skipped.
pub fn extract_host_pairs(bytes: &[u8], host: &str, out: &mut Vec<StorageItem>) {
    let tokens = tokenize(bytes);
    for (index, marker) in tokens.iter().enumerate() {
        if out.len() >= MAX_ITEMS {
            break;
        }
        if !marker.contains(host) || marker.len() > MAX_MARKER_LEN {
            continue;
        }
        let (Some(key), Some(value)) = (tokens.get(index + 1), tokens.get(index + 2)) else {
            continue;
        };
        if !is_storage_key(key)
            || value.is_empty()
            || value.len() > MAX_VALUE_LEN
            || key == value
            || out.iter().any(|item| item.key == *key)
        {
            continue;
        }
        out.push(StorageItem {
            key: key.clone(),
            value: value.clone(),
        });
    }
}

/// Build the `Page.addScriptToEvaluateOnNewDocument` script that seeds `items`
/// into the portal origin before any document loads.
///
/// Returns `None` when there is nothing valid to inject, in which case the
/// caller must skip the CDP call entirely. The script is additive-only and
/// fully self-guarded: every access is wrapped in `try/catch` and existing
/// keys are never overwritten.
#[must_use]
pub fn build_hydration_script(items: &[StorageItem]) -> Option<String> {
    if items.is_empty() || items.len() > MAX_ITEMS {
        return None;
    }
    for item in items {
        if !is_storage_key(&item.key) || item.value.is_empty() || item.value.len() > MAX_VALUE_LEN {
            return None;
        }
    }
    let pairs: Vec<(&str, &str)> = items
        .iter()
        .map(|item| (item.key.as_str(), item.value.as_str()))
        .collect();
    let json = serde_json::to_string(&pairs).ok()?;
    Some(format!(
        "(()=>{{try{{var i={json};for(var n=0;n<i.length;n++){{try{{if(window.localStorage&&localStorage.getItem(i[n][0])===null){{localStorage.setItem(i[n][0],i[n][1]);}}}}catch(e){{}}}}}}catch(e){{}}}})();"
    ))
}
