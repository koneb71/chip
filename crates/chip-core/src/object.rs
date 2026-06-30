use serde::{Deserialize, Serialize};

use crate::change::ChangeId;
use crate::error::{Error, Result};
use crate::hash::ObjectId;

/// File contents. The unit of stored data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blob {
    pub data: Vec<u8>,
}

/// What a tree entry points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    Blob,
    Tree,
}

/// A single entry in a directory listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    /// Unix mode bits (we only really care about the executable bit, 0o755 vs 0o644).
    pub mode: u32,
    pub id: ObjectId,
}

/// A directory: a sorted list of entries. Sorting by name makes the serialized
/// form canonical so identical trees hash identically.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

impl Tree {
    pub fn new(mut entries: Vec<TreeEntry>) -> Self {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Tree { entries }
    }

    pub fn get(&self, name: &str) -> Option<&TreeEntry> {
        self.entries
            .binary_search_by(|e| e.name.as_str().cmp(name))
            .ok()
            .map(|i| &self.entries[i])
    }
}

/// A commit: an immutable snapshot of a tree plus history metadata.
///
/// `change_id` is the *stable* identity (survives rewrites); the commit's own
/// `ObjectId` is the *content* identity. A merge commit has multiple parents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub change_id: ChangeId,
    pub author: String,
    /// Unix timestamp (seconds).
    pub timestamp: i64,
    pub message: String,
    /// Files left in a conflicted state by a merge (first-class conflicts).
    /// Empty for ordinary commits.
    #[serde(default)]
    pub conflicts: Vec<String>,
}

impl Commit {
    pub fn is_conflicted(&self) -> bool {
        !self.conflicts.is_empty()
    }
}

/// The tagged union of everything that lives in the object store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Object {
    Blob(Blob),
    Tree(Tree),
    Commit(Commit),
}

impl Object {
    pub fn kind(&self) -> &'static str {
        match self {
            Object::Blob(_) => "blob",
            Object::Tree(_) => "tree",
            Object::Commit(_) => "commit",
        }
    }

    pub fn as_blob(self) -> Result<Blob> {
        match self {
            Object::Blob(b) => Ok(b),
            other => Err(Error::WrongObjectKind {
                expected: "blob",
                found: other.kind(),
            }),
        }
    }

    pub fn as_tree(self) -> Result<Tree> {
        match self {
            Object::Tree(t) => Ok(t),
            other => Err(Error::WrongObjectKind {
                expected: "tree",
                found: other.kind(),
            }),
        }
    }

    pub fn as_commit(self) -> Result<Commit> {
        match self {
            Object::Commit(c) => Ok(c),
            other => Err(Error::WrongObjectKind {
                expected: "commit",
                found: other.kind(),
            }),
        }
    }
}
