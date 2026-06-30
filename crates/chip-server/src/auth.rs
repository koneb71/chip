//! Password hashing and API-token handling.

/// Minimum accepted password length.
pub const MIN_PASSWORD_LEN: usize = 8;

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::RngCore;

/// Hash a password for storage with Argon2 (salted).
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hash error: {e}"))?
        .to_string();
    Ok(hash)
}

/// Verify a password against a stored Argon2 hash.
pub fn verify_password(password: &str, stored: &str) -> bool {
    match PasswordHash::new(stored) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Generate a fresh opaque API token (the plaintext shown to the user once).
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The stored form of a token: a BLAKE3 hash of the plaintext, so a database
/// leak does not expose usable tokens. Reuses chip-core's content hashing.
pub fn hash_token(token: &str) -> String {
    chip_core::hash::ObjectId::hash(token.as_bytes()).to_hex()
}
