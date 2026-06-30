use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// A content-addressed object identifier: the BLAKE3 hash of an object's
/// canonical (uncompressed) serialized bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectId([u8; 32]);

impl ObjectId {
    /// Hash arbitrary bytes into an `ObjectId`.
    pub fn hash(bytes: &[u8]) -> Self {
        ObjectId(*blake3::hash(bytes).as_bytes())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        ObjectId(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Full 64-char lowercase hex representation.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// Short hex prefix used in human-facing output.
    pub fn short(&self) -> String {
        self.to_hex()[..12].to_string()
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.short())
    }
}

impl FromStr for ObjectId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.len() != 64 {
            return Err(Error::InvalidObjectId(s.to_string()));
        }
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|_| Error::InvalidObjectId(s.to_string()))?;
        }
        Ok(ObjectId(bytes))
    }
}
