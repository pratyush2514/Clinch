//! Integration tests for `session_sync::crypto`.
//!
//! Moved out of `src/crypto.rs` so the main source stays test-free.

use proptest::prelude::*;
use session_sync::crypto::*;

#[test]
fn decrypts_independent_node_crypto_vector() -> Result<(), Box<dyn std::error::Error>> {
    // Generated independently with Node's PBKDF2 + AES-CBC, not encrypt_fixture.
    let hex = "7631301b04736c854fdeed93d36784fe833a7146120bf025299cd7705e8e09bdcfcf0368634e4fa28eeed23611bab140ea483a1f980ffbad1bae1c5a56452185d0fcee";
    let bytes: Result<Vec<_>, _> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
        .collect();
    let value = decrypt(&bytes?, ".example.com", 24, &derive_key(b"fixture-only"))?;
    assert_eq!(value.as_str(), "synthetic-session");
    Ok(())
}
#[test]
fn verifies_host_digest_and_format() {
    let key = derive_key(b"test-password");
    let encrypted = encrypt_fixture("session", ".example.com", 24, &key);
    assert!(decrypt(&encrypted, ".other.com", 24, &key).is_err());
    assert!(decrypt(b"v20unsupported", ".example.com", 24, &key).is_err());
    assert!(decrypt(b"v10broken", ".example.com", 24, &key).is_err());
}
proptest! {
    #[test]
    fn round_trip_unicode(value in ".{0,512}", version in 23i64..=24) {
        let key = derive_key(b"fixture-only");
        let encrypted = encrypt_fixture(&value, ".example.com", version, &key);
        let decoded = decrypt(&encrypted, ".example.com", version, &key)?;
        prop_assert_eq!(decoded.as_str(), value);
    }
    #[test]
    fn gcm_round_trip_unicode(value in ".{0,512}", version in 23i64..=24) {
        let key = derive_gcm_key(b"fixture-only");
        let encrypted = encrypt_gcm_fixture(&value, ".example.com", version, &key)?;
        let decoded = decrypt_gcm(&encrypted, ".example.com", version, &key)?;
        prop_assert_eq!(decoded.as_str(), value);
    }
    #[test]
    fn auto_dispatch_covers_both_cipher_suites(value in ".{0,256}") {
        let secret = b"dispatch-secret";
        let cbc = encrypt_fixture(&value, ".example.com", 24, &derive_key(secret));
        let decoded = decrypt_auto(&cbc, ".example.com", 24, secret)?;
        prop_assert_eq!(decoded.as_str(), value.as_str());
        let gcm = encrypt_gcm_fixture(&value, ".example.com", 24, &derive_gcm_key(secret))?;
        let decoded = decrypt_auto(&gcm, ".example.com", 24, secret)?;
        prop_assert_eq!(decoded.as_str(), value.as_str());
    }
    #[test]
    fn malformed_ciphertext_never_panics(value in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = decrypt(&value, ".example.com", 24, &[0;16]);
        let _ = decrypt_gcm(&value, ".example.com", 24, &[0;32]);
        let _ = decrypt_auto(&value, ".example.com", 24, b"secret");
    }
}

#[test]
fn gcm_rejects_wrong_host_and_raw_key_path() -> Result<(), Box<dyn std::error::Error>> {
    let key = derive_gcm_key(b"test-password");
    let encrypted = encrypt_gcm_fixture("session", ".example.com", 24, &key)?;
    assert!(decrypt_gcm(&encrypted, ".other.com", 24, &key).is_err());
    assert!(decrypt_gcm(b"v20unsupported", ".example.com", 24, &key).is_err());
    // A raw 32-byte DPAPI key decrypts GCM directly without derivation.
    let raw: Vec<u8> = key.to_vec();
    assert_eq!(
        decrypt_auto(&encrypted, ".example.com", 24, &raw)?.as_str(),
        "session"
    );
    Ok(())
}
