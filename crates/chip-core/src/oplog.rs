//! Append-only operation log powering `chip undo`.
//!
//! Before each mutating command we capture a snapshot of the repo's refs
//! (`HEAD` + bookmarks). `undo` restores the most recent snapshot, so any
//! command can be reversed without the user reasoning about reflogs or resets.

use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::refs::Head;
use crate::repo::Repo;
use crate::store::atomic_write;
use crate::working_copy;

/// A captured snapshot of the mutable pointers, used to reverse an operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoState {
    /// Raw `HEAD` contents (`ref: name`, a hex commit id, or empty).
    pub head: String,
    /// `(bookmark name, commit hex)` pairs.
    pub bookmarks: Vec<(String, String)>,
}

/// One recorded operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Op {
    pub seq: u64,
    pub timestamp: i64,
    pub description: String,
    /// Repo state *before* the operation ran.
    pub before: RepoState,
}

pub struct OpLog {
    dir: PathBuf,
}

impl OpLog {
    pub fn new(repo: &Repo) -> Self {
        OpLog {
            dir: repo.chip_dir().join("oplog"),
        }
    }

    fn count_path(&self) -> PathBuf {
        self.dir.join("count")
    }

    fn entry_path(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("{seq:08}"))
    }

    fn count(&self) -> Result<u64> {
        match fs::read_to_string(self.count_path()) {
            Ok(s) => Ok(s.trim().parse().unwrap_or(0)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    fn set_count(&self, n: u64) -> Result<()> {
        atomic_write(&self.count_path(), format!("{n}\n").as_bytes())
    }

    /// Capture the current repo state so a future operation can be reversed.
    pub fn capture(repo: &Repo) -> Result<RepoState> {
        let refs = repo.refs();
        let head = match refs.read_head()? {
            Head::Bookmark(name) => format!("ref: {name}"),
            Head::Detached(id) => id.to_hex(),
            Head::Unborn => String::new(),
        };
        let bookmarks = refs
            .list_bookmarks()?
            .into_iter()
            .map(|(n, id)| (n, id.to_hex()))
            .collect();
        Ok(RepoState { head, bookmarks })
    }

    /// Append an operation record carrying the pre-operation `before` state.
    pub fn append(&self, repo: &Repo, description: &str, before: RepoState) -> Result<()> {
        let seq = self.count()? + 1;
        let op = Op {
            seq,
            timestamp: crate::now(),
            description: description.to_string(),
            before,
        };
        let _ = repo;
        let bytes = bincode::serialize(&op)?;
        atomic_write(&self.entry_path(seq), &bytes)?;
        self.set_count(seq)?;
        Ok(())
    }

    /// All operations, oldest first.
    pub fn list(&self) -> Result<Vec<Op>> {
        let count = self.count()?;
        let mut ops = Vec::new();
        for seq in 1..=count {
            if let Ok(bytes) = fs::read(self.entry_path(seq)) {
                ops.push(bincode::deserialize(&bytes)?);
            }
        }
        Ok(ops)
    }

    /// Reverse the most recent operation: restore its `before` ref state and
    /// the working tree, then drop the record. Returns the undone op.
    pub fn undo(&self, repo: &Repo) -> Result<Op> {
        let count = self.count()?;
        if count == 0 {
            return Err(Error::Other("nothing to undo".into()));
        }
        let bytes = fs::read(self.entry_path(count))?;
        let op: Op = bincode::deserialize(&bytes)?;

        restore_state(repo, &op.before)?;

        // Restore the working tree to match the now-current HEAD.
        if let Some(commit_id) = repo.refs().head_commit()? {
            let commit = repo.store().get_commit(&commit_id)?;
            working_copy::restore(repo, &commit.tree)?;
        }

        fs::remove_file(self.entry_path(count))?;
        self.set_count(count - 1)?;
        Ok(op)
    }
}

/// Restore refs to a captured state.
fn restore_state(repo: &Repo, state: &RepoState) -> Result<()> {
    let refs = repo.refs();

    // HEAD
    let head = if state.head.is_empty() {
        Head::Unborn
    } else if let Some(name) = state.head.strip_prefix("ref: ") {
        Head::Bookmark(name.to_string())
    } else {
        Head::Detached(ObjectId::from_str(&state.head)?)
    };
    refs.write_head(&head)?;

    // Bookmarks: set those recorded, delete those that now exist but didn't then.
    let wanted: std::collections::HashMap<&str, &str> = state
        .bookmarks
        .iter()
        .map(|(n, h)| (n.as_str(), h.as_str()))
        .collect();
    for (name, hex) in &state.bookmarks {
        refs.set_bookmark(name, ObjectId::from_str(hex)?)?;
    }
    for (name, _) in refs.list_bookmarks()? {
        if !wanted.contains_key(name.as_str()) {
            let _ = refs.delete_bookmark(&name);
        }
    }
    Ok(())
}
