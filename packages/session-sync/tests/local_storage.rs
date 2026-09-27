//! Integration tests for `session_sync::local_storage`.
//!
//! Moved out of `src/local_storage.rs` so the main source stays test-free.

use session_sync::local_storage::*;

fn leveldb_fixture(entries: &[(&str, &str)], host: &str) -> Vec<u8> {
    // Mimics uncompressed LevelDB record layout: binary framing around
    // `origin + key` and value blobs.
    let mut bytes = vec![0xAA, 0x00, 0xFF];
    for (key, value) in entries {
        bytes.extend_from_slice(b"\x01META");
        bytes.push(0x00);
        bytes.extend_from_slice(host.as_bytes());
        bytes.push(0x00);
        bytes.extend_from_slice(key.as_bytes());
        bytes.push(0x00);
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0x00);
    }
    bytes
}

#[test]
fn pairs_host_adjacent_entries_and_dedupes() {
    let bytes = leveldb_fixture(
        &[
            ("session_id", "abc123"),
            ("session_id", "abc123"),
            ("theme", "dark"),
        ],
        "https://portal.example.com",
    );
    let mut items = Vec::new();
    extract_host_pairs(&bytes, "portal.example.com", &mut items);
    assert_eq!(items.len(), 2);
    assert!(
        items
            .iter()
            .any(|item| item.key == "session_id" && item.value == "abc123")
    );
    assert!(
        items
            .iter()
            .any(|item| item.key == "theme" && item.value == "dark")
    );
}

#[test]
fn ignores_unrelated_hosts_and_binary_noise() {
    let mut bytes = vec![0x00, 0xFF, 0x80, 0xFE];
    bytes.extend_from_slice(b"\x01other.example.com\x00somekey\x00somevalue\x00");
    let mut items = Vec::new();
    extract_host_pairs(&bytes, "portal.example.com", &mut items);
    assert!(items.is_empty());
}

#[test]
fn rejects_key_injection_shapes() {
    assert!(!is_storage_key(""));
    assert!(!is_storage_key("key with spaces"));
    assert!(!is_storage_key("key\";drop"));
    assert!(!is_storage_key(&"k".repeat(MAX_KEY_LEN + 1)));
    assert!(is_storage_key("app.session-id:v1"));
}

#[test]
fn script_is_additive_and_self_guarded() -> Result<(), Box<dyn std::error::Error>> {
    assert!(build_hydration_script(&[]).is_none());
    let invalid = vec![StorageItem {
        key: "bad key".into(),
        value: "v".into(),
    }];
    assert!(build_hydration_script(&invalid).is_none());
    let items = vec![StorageItem {
        key: "session_id".into(),
        value: "abc\"123\\".into(),
    }];
    let Some(script) = build_hydration_script(&items) else {
        return Err(std::io::Error::other("valid items build").into());
    };
    // Additive-only: existing portal state is never overwritten.
    assert!(script.contains("getItem(i[n][0])===null"));
    assert!(script.contains("localStorage.setItem"));
    // Payload round-trips as JSON: quoting/escapes cannot break the script.
    let Some(payload) = script.find("var i=").map(|start| start + 6) else {
        return Err(std::io::Error::other("payload present").into());
    };
    let Some(end) = script[payload..].find(';').map(|offset| offset + payload) else {
        return Err(std::io::Error::other("payload terminator").into());
    };
    let pairs: Vec<(String, String)> = serde_json::from_str(&script[payload..end])?;
    assert_eq!(
        pairs,
        vec![("session_id".to_string(), "abc\"123\\".to_string())]
    );
    Ok(())
}

#[tokio::test]
async fn missing_leveldb_yields_no_items_without_error() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    assert!(
        read_local_storage(dir.path(), "portal.example.com")
            .await
            .is_empty()
    );
    assert!(read_local_storage(dir.path(), "").await.is_empty());
    Ok(())
}

#[tokio::test]
async fn reads_log_files_and_ignores_other_extensions() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let leveldb = dir.path().join("Local Storage/leveldb");
    tokio::fs::create_dir_all(&leveldb).await?;
    tokio::fs::write(
        leveldb.join("000003.log"),
        leveldb_fixture(&[("session_id", "abc123")], "https://portal.example.com"),
    )
    .await?;
    tokio::fs::write(leveldb.join("MANIFEST-000001"), b"noise").await?;
    let items = read_local_storage(dir.path(), "portal.example.com").await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "session_id");
    Ok(())
}
