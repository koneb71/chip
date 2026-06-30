//! Encryption at rest for repository object data.
//!
//! [`EncryptedBackend`] is a transparent decorator over any
//! [`chip_core::store::ObjectBackend`]: it AES-256-GCM-encrypts each object's
//! bytes on `put` and decrypts on `get`. Because the object *id* is the BLAKE3
//! hash of the plaintext (computed above this layer), content-addressing, the
//! `put_raw`/`get_raw` sync path, and the gRPC wire format are all unaffected —
//! only the bytes that land on disk / in S3 change.

use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use chip_core::error::{Error as CoreError, Result as CoreResult};
use chip_core::store::ObjectBackend;
use rand::rngs::OsRng;
use rand::RngCore;

/// On-disk framing version, so the format can evolve.
const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;

/// Wraps an inner backend, encrypting object bytes with AES-256-GCM.
///
/// Stored layout per object: `[version:1][nonce:12][ciphertext+tag]`. The
/// object's storage key (its content-hash hex) is bound in as AEAD associated
/// data, so a ciphertext cannot be relocated to a different address.
pub struct EncryptedBackend {
    inner: Arc<dyn ObjectBackend>,
    cipher: Aes256Gcm,
}

impl EncryptedBackend {
    pub fn new(inner: Arc<dyn ObjectBackend>, key: &[u8; 32]) -> Self {
        EncryptedBackend {
            inner,
            cipher: Aes256Gcm::new(key.into()),
        }
    }
}

impl ObjectBackend for EncryptedBackend {
    fn get(&self, key: &str) -> CoreResult<Option<Vec<u8>>> {
        let framed = match self.inner.get(key)? {
            Some(b) => b,
            None => return Ok(None),
        };
        if framed.len() < 1 + NONCE_LEN || framed[0] != VERSION {
            return Err(CoreError::Other(format!(
                "stored object {key} has an unrecognized encryption frame"
            )));
        }
        let nonce = Nonce::from_slice(&framed[1..1 + NONCE_LEN]);
        let ciphertext = &framed[1 + NONCE_LEN..];
        let plaintext = self
            .cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext,
                    aad: key.as_bytes(),
                },
            )
            .map_err(|_| {
                CoreError::Other(format!(
                    "decryption failed for object {key} (tampered data or wrong CHIP_DATA_KEY)"
                ))
            })?;
        Ok(Some(plaintext))
    }

    fn put(&self, key: &str, bytes: &[u8]) -> CoreResult<()> {
        // A fresh random nonce per write. Re-writing an already-present key is a
        // no-op in the inner backend, so objects aren't needlessly re-encrypted.
        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(
                nonce,
                Payload {
                    msg: bytes,
                    aad: key.as_bytes(),
                },
            )
            .map_err(|_| CoreError::Other("object encryption failed".into()))?;

        let mut framed = Vec::with_capacity(1 + NONCE_LEN + ciphertext.len());
        framed.push(VERSION);
        framed.extend_from_slice(&nonce_bytes);
        framed.extend_from_slice(&ciphertext);
        self.inner.put(key, &framed)
    }

    fn exists(&self, key: &str) -> CoreResult<bool> {
        // Cheap existence check that avoids a decrypt.
        self.inner.exists(key)
    }
}

/// Parse a 32-byte data key from a 64-char hex string.
pub fn parse_key_hex(s: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = hex::decode(s.trim())
        .map_err(|_| anyhow::anyhow!("CHIP_DATA_KEY must be 64 hex characters"))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("CHIP_DATA_KEY must decode to exactly 32 bytes"))?;
    Ok(arr)
}

/// Derive a deterministic dev key from the server secret (BLAKE3). Used only in
/// `CHIP_DEV=1` mode when no explicit `CHIP_DATA_KEY` is provided, so local runs
/// still encrypt without managing a key.
pub fn derive_dev_key(secret: &str) -> [u8; 32] {
    *chip_core::hash::ObjectId::hash(format!("chip-data-key::{secret}").as_bytes()).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chip_core::store::FilesystemBackend;

    fn backend() -> EncryptedBackend {
        let dir = tempfile::tempdir().unwrap().keep();
        let inner = Arc::new(FilesystemBackend::new(dir));
        EncryptedBackend::new(inner, &[7u8; 32])
    }

    #[test]
    fn round_trip() {
        let b = backend();
        b.put("abc123", b"hello secret data").unwrap();
        assert_eq!(b.get("abc123").unwrap().unwrap(), b"hello secret data");
        assert!(b.exists("abc123").unwrap());
        assert!(b.get("missing").unwrap().is_none());
    }

    #[test]
    fn stored_bytes_are_not_plaintext() {
        let dir = tempfile::tempdir().unwrap().keep();
        let inner = Arc::new(FilesystemBackend::new(dir));
        let enc = EncryptedBackend::new(inner.clone(), &[9u8; 32]);
        enc.put("k", b"TOPSECRET").unwrap();
        // The raw stored bytes must not contain the plaintext.
        let raw = inner.get("k").unwrap().unwrap();
        assert!(!raw.windows(9).any(|w| w == b"TOPSECRET"));
        assert_eq!(raw[0], VERSION);
    }

    #[test]
    fn tampering_is_detected() {
        let dir = tempfile::tempdir().unwrap().keep();
        let inner = Arc::new(FilesystemBackend::new(dir));
        let enc = EncryptedBackend::new(inner.clone(), &[3u8; 32]);
        enc.put("k", b"payload").unwrap();
        // Flip a byte in the ciphertext and confirm the AEAD tag rejects it.
        let mut raw = inner.get("k").unwrap().unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        // Overwrite via a fresh filesystem write (inner.put skips existing keys,
        // so write to a different inner to simulate corruption).
        let dir2 = tempfile::tempdir().unwrap().keep();
        let inner2 = Arc::new(FilesystemBackend::new(dir2));
        inner2.put("k", &raw).unwrap();
        let enc2 = EncryptedBackend::new(inner2, &[3u8; 32]);
        assert!(enc2.get("k").is_err());
    }

    #[test]
    fn wrong_key_fails() {
        let dir = tempfile::tempdir().unwrap().keep();
        let inner = Arc::new(FilesystemBackend::new(dir));
        EncryptedBackend::new(inner.clone(), &[1u8; 32])
            .put("k", b"data")
            .unwrap();
        let other = EncryptedBackend::new(inner, &[2u8; 32]);
        assert!(other.get("k").is_err());
    }
}
