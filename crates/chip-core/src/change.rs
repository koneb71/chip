use std::fmt;

use rand::Rng;
use serde::{Deserialize, Serialize};

/// A stable change identity.
///
/// Unlike an [`crate::hash::ObjectId`] (which is derived from content and
/// therefore changes whenever the content changes), a `ChangeId` is a random
/// value generated once and carried forward across rewrites/amendments. It is
/// what gives a change a persistent identity in `chip log`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChangeId(String);

impl ChangeId {
    /// Generate a fresh random change id (12 hex characters / 48 bits).
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();
        let bytes: [u8; 6] = rng.gen();
        let mut s = String::with_capacity(12);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        ChangeId(s)
    }

    pub fn from_string(s: String) -> Self {
        ChangeId(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChangeId({})", self.0)
    }
}
