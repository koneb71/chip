//! chip-core — the changeset-oriented version control engine shared by the
//! `chip` CLI and the chip server.
//!
//! The model deliberately departs from Git: there is no staging area (the whole
//! working tree is snapshotted), changes carry a stable [`change::ChangeId`]
//! distinct from their content hash, merges keep conflicts first-class, and an
//! [`oplog`] backs a universal `undo`.

pub mod change;
pub mod dag;
pub mod diff;
pub mod error;
pub mod evolution;
pub mod hash;
pub mod merge;
pub mod object;
pub mod oplog;
pub mod ops;
pub mod refs;
pub mod repo;
pub mod store;
pub mod working_copy;

pub use change::ChangeId;
pub use error::{Error, Result};
pub use hash::ObjectId;
pub use object::{Blob, Commit, Object, Tree};
pub use repo::{Repo, DEFAULT_BOOKMARK};

/// Current unix timestamp in seconds.
pub fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
