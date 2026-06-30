use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::store::atomic_write;

/// Where `HEAD` currently points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    /// Attached to a bookmark by name (the common case).
    Bookmark(String),
    /// Detached directly at a commit.
    Detached(ObjectId),
    /// No commits exist yet.
    Unborn,
}

/// Reads and writes the mutable pointers of a repo: `HEAD`, bookmarks, tags.
///
/// These are small, frequently-rewritten files kept on the local filesystem
/// (the immutable object data lives in the [`crate::store::ObjectStore`]).
pub struct RefStore {
    chip_dir: PathBuf,
}

impl RefStore {
    pub fn new(chip_dir: impl Into<PathBuf>) -> Self {
        RefStore {
            chip_dir: chip_dir.into(),
        }
    }

    fn head_path(&self) -> PathBuf {
        self.chip_dir.join("HEAD")
    }

    fn bookmarks_dir(&self) -> PathBuf {
        self.chip_dir.join("refs").join("bookmarks")
    }

    fn tags_dir(&self) -> PathBuf {
        self.chip_dir.join("refs").join("tags")
    }

    // HEAD ---------------------------------------------------------------------

    pub fn read_head(&self) -> Result<Head> {
        let content = match fs::read_to_string(self.head_path()) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Head::Unborn),
            Err(e) => return Err(e.into()),
        };
        let content = content.trim();
        if content.is_empty() {
            Ok(Head::Unborn)
        } else if let Some(name) = content.strip_prefix("ref: ") {
            Ok(Head::Bookmark(name.trim().to_string()))
        } else {
            Ok(Head::Detached(ObjectId::from_str(content)?))
        }
    }

    pub fn write_head(&self, head: &Head) -> Result<()> {
        let content = match head {
            Head::Bookmark(name) => format!("ref: {name}\n"),
            Head::Detached(id) => format!("{id}\n"),
            Head::Unborn => String::new(),
        };
        atomic_write(&self.head_path(), content.as_bytes())
    }

    /// Resolve `HEAD` to a commit id, if any.
    pub fn head_commit(&self) -> Result<Option<ObjectId>> {
        match self.read_head()? {
            Head::Unborn => Ok(None),
            Head::Detached(id) => Ok(Some(id)),
            Head::Bookmark(name) => self.read_bookmark(&name),
        }
    }

    // Bookmarks ----------------------------------------------------------------

    pub fn read_bookmark(&self, name: &str) -> Result<Option<ObjectId>> {
        read_ref_file(&self.bookmarks_dir().join(name))
    }

    pub fn set_bookmark(&self, name: &str, target: ObjectId) -> Result<()> {
        atomic_write(
            &self.bookmarks_dir().join(name),
            format!("{target}\n").as_bytes(),
        )
    }

    pub fn delete_bookmark(&self, name: &str) -> Result<()> {
        let path = self.bookmarks_dir().join(name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::RefNotFound(name.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn list_bookmarks(&self) -> Result<Vec<(String, ObjectId)>> {
        list_ref_dir(&self.bookmarks_dir())
    }

    // Tags ---------------------------------------------------------------------

    pub fn set_tag(&self, name: &str, target: ObjectId) -> Result<()> {
        atomic_write(
            &self.tags_dir().join(name),
            format!("{target}\n").as_bytes(),
        )
    }

    pub fn read_tag(&self, name: &str) -> Result<Option<ObjectId>> {
        read_ref_file(&self.tags_dir().join(name))
    }

    pub fn list_tags(&self) -> Result<Vec<(String, ObjectId)>> {
        list_ref_dir(&self.tags_dir())
    }

    /// Advance whatever `HEAD` points at to `target`. If `HEAD` is attached to a
    /// bookmark the bookmark moves; if detached, `HEAD` moves; if unborn we
    /// create and attach to the default bookmark `main`.
    pub fn move_head_to(&self, target: ObjectId, default_bookmark: &str) -> Result<()> {
        match self.read_head()? {
            Head::Bookmark(name) => self.set_bookmark(&name, target),
            Head::Detached(_) => self.write_head(&Head::Detached(target)),
            Head::Unborn => {
                self.set_bookmark(default_bookmark, target)?;
                self.write_head(&Head::Bookmark(default_bookmark.to_string()))
            }
        }
    }
}

fn read_ref_file(path: &Path) -> Result<Option<ObjectId>> {
    match fs::read_to_string(path) {
        Ok(c) => Ok(Some(ObjectId::from_str(c.trim())?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn list_ref_dir(dir: &Path) -> Result<Vec<(String, ObjectId)>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = read_ref_file(&entry.path())? {
                out.push((name, id));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}
