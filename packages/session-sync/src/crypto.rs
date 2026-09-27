#![deny(unsafe_code)]
use crate::SyncError;
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use aes_gcm::{AeadInPlace, KeyInit, aead::generic_array::GenericArray};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub fn derive_key(secret: &[u8]) -> Zeroizing<[u8; 16]> {
    let mut key = Zeroizing::new([0; 16]);
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(secret, b"saltysalt", 1003, key.as_mut());
    key
}

/// 32-byte sibling of [`derive_key`] for AES-256-GCM cookie payloads
/// (Chrome 80+). Same salt/iterations, extended output — no new secret.
pub fn derive_gcm_key(secret: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut key = Zeroizing::new([0; 32]);
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(secret, b"saltysalt", 1003, key.as_mut());
    key
}

pub fn decrypt(
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
pub fn decrypt_gcm(
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
pub fn decrypt_auto(
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

pub fn encrypt_fixture(value: &str, host: &str, version: i64, key: &[u8; 16]) -> Vec<u8> {
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

pub fn encrypt_gcm_fixture(
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
