//! The content-addressed object store: (de)serialization + compression on top
//! of a pluggable [`ObjectBackend`].

mod backend;

pub use backend::{atomic_write, FilesystemBackend, ObjectBackend};

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::object::{Blob, Commit, Object, Tree};

const ZSTD_LEVEL: i32 = 3;

/// Wraps an [`ObjectBackend`] and handles the canonical encoding of objects:
/// `bincode` serialize -> BLAKE3 hash (the id) -> zstd compress -> store.
#[derive(Clone)]
pub struct ObjectStore {
    backend: Arc<dyn ObjectBackend>,
}

impl ObjectStore {
    pub fn new(backend: Arc<dyn ObjectBackend>) -> Self {
        ObjectStore { backend }
    }

    /// Serialize and store an object, returning its content id. If an object
    /// with the same id already exists this is a no-op (writes are idempotent).
    pub fn put(&self, object: &Object) -> Result<ObjectId> {
        let raw = bincode::serialize(object)?;
        let id = ObjectId::hash(&raw);
        let compressed = zstd::encode_all(&raw[..], ZSTD_LEVEL)?;
        self.backend.put(&id.to_hex(), &compressed)?;
        Ok(id)
    }

    /// Load and decode an object by id.
    pub fn get(&self, id: &ObjectId) -> Result<Object> {
        let compressed = self
            .backend
            .get(&id.to_hex())?
            .ok_or_else(|| Error::ObjectNotFound(id.short()))?;
        let raw = zstd::decode_all(&compressed[..])?;
        let object = bincode::deserialize(&raw)?;
        Ok(object)
    }

    pub fn contains(&self, id: &ObjectId) -> Result<bool> {
        self.backend.exists(&id.to_hex())
    }

    /// Fetch the raw stored (compressed) bytes for an object, for streaming over
    /// the wire without re-encoding.
    pub fn get_raw(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        self.backend.get(&id.to_hex())
    }

    /// Store raw (compressed) object bytes received over the wire, verifying
    /// that they decompress to content whose hash matches `id`. This prevents a
    /// peer from poisoning the store with mislabeled objects.
    pub fn put_raw(&self, id: &ObjectId, compressed: &[u8]) -> Result<()> {
        let raw = zstd::decode_all(compressed)?;
        let actual = ObjectId::hash(&raw);
        if &actual != id {
            return Err(Error::Other(format!(
                "object hash mismatch: claimed {}, actual {}",
                id.short(),
                actual.short()
            )));
        }
        // Ensure it deserializes into a valid object before persisting.
        let _: Object = bincode::deserialize(&raw)?;
        self.backend.put(&id.to_hex(), compressed)
    }

    // Typed convenience wrappers ------------------------------------------------

    pub fn put_blob(&self, blob: Blob) -> Result<ObjectId> {
        self.put(&Object::Blob(blob))
    }

    pub fn put_tree(&self, tree: Tree) -> Result<ObjectId> {
        self.put(&Object::Tree(tree))
    }

    pub fn put_commit(&self, commit: Commit) -> Result<ObjectId> {
        self.put(&Object::Commit(commit))
    }

    pub fn get_blob(&self, id: &ObjectId) -> Result<Blob> {
        self.get(id)?.as_blob()
    }

    pub fn get_tree(&self, id: &ObjectId) -> Result<Tree> {
        self.get(id)?.as_tree()
    }

    pub fn get_commit(&self, id: &ObjectId) -> Result<Commit> {
        self.get(id)?.as_commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::ChangeId;
    use crate::object::{EntryKind, TreeEntry};

    fn mem_store() -> ObjectStore {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir for the lifetime of the test process.
        let path = dir.keep();
        ObjectStore::new(Arc::new(FilesystemBackend::new(path)))
    }

    #[test]
    fn blob_round_trip() {
        let store = mem_store();
        let id = store
            .put_blob(Blob {
                data: b"hello".to_vec(),
            })
            .unwrap();
        let got = store.get_blob(&id).unwrap();
        assert_eq!(got.data, b"hello");
    }

    #[test]
    fn identical_content_same_id() {
        let store = mem_store();
        let a = store
            .put_blob(Blob {
                data: b"abc".to_vec(),
            })
            .unwrap();
        let b = store
            .put_blob(Blob {
                data: b"abc".to_vec(),
            })
            .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn commit_round_trip() {
        let store = mem_store();
        let tree = store.put_tree(Tree::default()).unwrap();
        let commit = Commit {
            tree,
            parents: vec![],
            change_id: ChangeId::generate(),
            author: "tester".into(),
            timestamp: 0,
            message: "first".into(),
            conflicts: vec![],
        };
        let id = store.put_commit(commit.clone()).unwrap();
        assert_eq!(store.get_commit(&id).unwrap(), commit);
    }

    #[test]
    fn tree_is_canonical_regardless_of_input_order() {
        let store = mem_store();
        let blob = store
            .put_blob(Blob {
                data: b"x".to_vec(),
            })
            .unwrap();
        let mk = |names: &[&str]| {
            Tree::new(
                names
                    .iter()
                    .map(|n| TreeEntry {
                        name: (*n).to_string(),
                        kind: EntryKind::Blob,
                        mode: 0o644,
                        id: blob,
                    })
                    .collect(),
            )
        };
        let a = store.put_tree(mk(&["a", "b", "c"])).unwrap();
        let b = store.put_tree(mk(&["c", "a", "b"])).unwrap();
        assert_eq!(a, b);
    }
}
