#![deny(unsafe_code)]
use crate::SyncError;
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub(crate) fn derive_key(secret: &[u8]) -> Zeroizing<[u8; 16]> {
    let mut key = Zeroizing::new([0; 16]);
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
        fn malformed_ciphertext_never_panics(value in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = decrypt(&value, ".example.com", 24, &[0;16]);
        }
    }
}
