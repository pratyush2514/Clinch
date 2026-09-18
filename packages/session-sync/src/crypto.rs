#![deny(unsafe_code)]
use crate::SyncError;
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use aes_gcm::{AeadInPlace, KeyInit, aead::generic_array::GenericArray};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub(crate) fn derive_key(secret: &[u8]) -> Zeroizing<[u8; 16]> {
    let mut key = Zeroizing::new([0; 16]);
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(secret, b"saltysalt", 1003, key.as_mut());
    key
}

/// 32-byte sibling of [`derive_key`] for AES-256-GCM cookie payloads
/// (Chrome 80+). Same salt/iterations, extended output — no new secret.
pub(crate) fn derive_gcm_key(secret: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut key = Zeroizing::new([0; 32]);
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(secret, b"saltysalt", 1003, key.as_mut());
    key
}

pub(crate) fn decrypt(
    encrypted: &[u8],
    host: &str,
    version: i64,
    key: &[u8; 16],
) -> Result<Zeroizing<String>, SyncError> {
    let ciphertext = encrypted
        .strip_prefix(b"v10")
        .ok_or(SyncError::UnsupportedFormat)?;
    let mut buffer = Zeroizing::new(ciphertext.to_vec());
    let plaintext = cbc::Decryptor::<aes::Aes128>::new(key.into(), (&[b' '; 16]).into())
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .map_err(|_| SyncError::Decryption)?;
    let value = if version >= 24 {
        let digest = Sha256::digest(host.as_bytes());
        if plaintext.len() < 32 || plaintext[..32] != digest[..] {
            return Err(SyncError::Decryption);
        }
        &plaintext[32..]
    } else {
        plaintext
    };
    let value = std::str::from_utf8(value).map_err(|_| SyncError::Decryption)?;
    Ok(Zeroizing::new(value.to_owned()))
}

/// Decrypt a `v10`/`v11` AES-256-GCM payload: prefix || 12-byte nonce ||
/// ciphertext || 16-byte tag. Schema 24+ binds the plaintext to the host.
pub(crate) fn decrypt_gcm(
    encrypted: &[u8],
    host: &str,
    version: i64,
    key: &[u8; 32],
) -> Result<Zeroizing<String>, SyncError> {
    let payload = encrypted
        .strip_prefix(b"v10")
        .or_else(|| encrypted.strip_prefix(b"v11"))
        .ok_or(SyncError::UnsupportedFormat)?;
    if payload.len() < 12 + 16 {
        return Err(SyncError::Decryption);
    }
    let (nonce, ciphertext) = payload.split_at(12);
    let cipher = aes_gcm::Aes256Gcm::new(GenericArray::from_slice(key));
    // `aead::Buffer` is implemented for `Vec<u8>` only; wrap the plaintext
    // in `Zeroizing` immediately after authenticated decryption succeeds.
    let mut buffer = ciphertext.to_vec();
    cipher
        .decrypt_in_place(GenericArray::from_slice(nonce), b"", &mut buffer)
        .map_err(|_| SyncError::Decryption)?;
    let buffer = Zeroizing::new(buffer);
    let value = if version >= 24 {
        let digest = Sha256::digest(host.as_bytes());
        if buffer.len() < 32 || buffer[..32] != digest[..] {
            return Err(SyncError::Decryption);
        }
        &buffer[32..]
    } else {
        &buffer[..]
    };
    let value = std::str::from_utf8(value).map_err(|_| SyncError::Decryption)?;
    Ok(Zeroizing::new(value.to_owned()))
}

/// Dispatch between the legacy AES-128-CBC path and AES-256-GCM.
///
/// A 32-byte OS secret (Windows DPAPI-unwrapped `Local State` key) is used
/// directly as the GCM key. Any other secret goes through the Keychain
/// derivation: CBC first for backward compatibility, then GCM.
pub(crate) fn decrypt_auto(
    encrypted: &[u8],
    host: &str,
    version: i64,
    secret: &[u8],
) -> Result<Zeroizing<String>, SyncError> {
    if encrypted.len() < 4 {
        return Err(SyncError::UnsupportedFormat);
    }
    if secret.len() == 32 {
        let mut key = [0; 32];
        key.copy_from_slice(&secret[..32]);
        return decrypt_gcm(encrypted, host, version, &Zeroizing::new(key));
    }
    match decrypt(encrypted, host, version, &derive_key(secret)) {
        Ok(value) => Ok(value),
        Err(SyncError::Decryption) => {
            decrypt_gcm(encrypted, host, version, &derive_gcm_key(secret))
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
pub(crate) fn encrypt_fixture(value: &str, host: &str, version: i64, key: &[u8; 16]) -> Vec<u8> {
    use aes::cipher::BlockEncryptMut;
    let mut plain = Vec::new();
    if version >= 24 {
        plain.extend_from_slice(&Sha256::digest(host.as_bytes()));
    }
    plain.extend_from_slice(value.as_bytes());
    let mut out = b"v10".to_vec();
    out.extend(
        cbc::Encryptor::<aes::Aes128>::new(key.into(), (&[b' '; 16]).into())
            .encrypt_padded_vec_mut::<Pkcs7>(&plain),
    );
    out
}

#[cfg(test)]
pub(crate) fn encrypt_gcm_fixture(
    value: &str,
    host: &str,
    version: i64,
    key: &[u8; 32],
) -> Result<Vec<u8>, SyncError> {
    use aes_gcm::aead::Aead;
    let mut plain = Vec::new();
    if version >= 24 {
        plain.extend_from_slice(&Sha256::digest(host.as_bytes()));
    }
    plain.extend_from_slice(value.as_bytes());
    // Deterministic nonce for fixtures only; production payloads use random nonces.
    let nonce = GenericArray::from_slice(&[7; 12]);
    let cipher = aes_gcm::Aes256Gcm::new(GenericArray::from_slice(key));
    let mut out = b"v10".to_vec();
    out.extend_from_slice(&[7; 12]);
    out.extend(
        cipher
            .encrypt(nonce, plain.as_ref())
            .map_err(|_| SyncError::Decryption)?,
    );
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
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
}
