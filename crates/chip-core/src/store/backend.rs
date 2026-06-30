use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::Result;

/// Storage abstraction for content-addressed object bytes.
///
/// Objects are addressed by an opaque string key (the hex of their hash). This
/// trait is the seam that lets chip move from a local filesystem to
/// S3-compatible object storage without touching any VCS logic: implement
/// `ObjectBackend` for an `object_store`-backed type and the rest of the engine
/// is unchanged.
pub trait ObjectBackend: Send + Sync {
    /// Fetch the stored bytes for `key`, or `None` if absent.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// Store `bytes` at `key`. Writes are idempotent (same key => same content,
    /// because keys are content hashes), so re-puts are cheap no-ops.
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()>;

    fn exists(&self, key: &str) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }
}

/// Filesystem-backed object storage. Objects are sharded into 256 directories
/// by the first byte of their key to keep directory sizes reasonable.
pub struct FilesystemBackend {
    root: PathBuf,
}

impl FilesystemBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        FilesystemBackend { root: root.into() }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        if key.len() >= 2 {
            self.root.join(&key[..2]).join(&key[2..])
        } else {
            self.root.join(key)
        }
    }
}

impl ObjectBackend for FilesystemBackend {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.path_for(key);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path_for(key);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Write to a temp file in the same directory then rename, so a reader
        // never observes a half-written object.
        atomic_write(&path, bytes)
    }
}

/// Write `bytes` to `path` atomically via a sibling temp file + rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.persist(path)
        .map_err(|e| crate::error::Error::Io(e.error))?;
    Ok(())
}
