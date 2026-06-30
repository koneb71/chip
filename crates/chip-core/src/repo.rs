use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::refs::RefStore;
use crate::store::{FilesystemBackend, ObjectStore};

/// The default bookmark a fresh repo commits onto.
pub const DEFAULT_BOOKMARK: &str = "main";

/// An opened chip repository: the working tree root plus its `.chip` metadata,
/// object store, and ref store.
pub struct Repo {
    root: PathBuf,
    chip_dir: PathBuf,
    store: ObjectStore,
    refs: RefStore,
}

impl Repo {
    /// Create a new repository rooted at `root`. Fails if one already exists.
    pub fn init(root: impl AsRef<Path>) -> Result<Repo> {
        let root = root.as_ref().to_path_buf();
        let chip_dir = root.join(".chip");
        if chip_dir.exists() {
            return Err(Error::RepoExists(root));
        }
        fs::create_dir_all(chip_dir.join("store").join("objects"))?;
        fs::create_dir_all(chip_dir.join("store").join("changes"))?;
        fs::create_dir_all(chip_dir.join("refs").join("bookmarks"))?;
        fs::create_dir_all(chip_dir.join("refs").join("tags"))?;
        fs::create_dir_all(chip_dir.join("oplog"))?;
        fs::write(chip_dir.join("config"), "author = you <you@localhost>\n")?;
        // Start unborn, attached to the default bookmark.
        fs::write(chip_dir.join("HEAD"), format!("ref: {DEFAULT_BOOKMARK}\n"))?;
        Repo::open_at(root, chip_dir)
    }

    /// Open the repository containing `start`, walking up to find `.chip`.
    pub fn discover(start: impl AsRef<Path>) -> Result<Repo> {
        let start =
            fs::canonicalize(start.as_ref()).unwrap_or_else(|_| start.as_ref().to_path_buf());
        let mut cur = start.as_path();
        loop {
            let chip_dir = cur.join(".chip");
            if chip_dir.is_dir() {
                return Repo::open_at(cur.to_path_buf(), chip_dir);
            }
            match cur.parent() {
                Some(parent) => cur = parent,
                None => return Err(Error::NotARepo(start)),
            }
        }
    }

    fn open_at(root: PathBuf, chip_dir: PathBuf) -> Result<Repo> {
        let backend = FilesystemBackend::new(chip_dir.join("store").join("objects"));
        let store = ObjectStore::new(Arc::new(backend));
        let refs = RefStore::new(chip_dir.clone());
        Ok(Repo {
            root,
            chip_dir,
            store,
            refs,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn chip_dir(&self) -> &Path {
        &self.chip_dir
    }

    pub fn store(&self) -> &ObjectStore {
        &self.store
    }

    pub fn refs(&self) -> &RefStore {
        &self.refs
    }

    /// The configured author string (`name <email>`).
    pub fn author(&self) -> String {
        if let Ok(env) = std::env::var("CHIP_AUTHOR") {
            if !env.trim().is_empty() {
                return env;
            }
        }
        let config = fs::read_to_string(self.chip_dir.join("config")).unwrap_or_default();
        for line in config.lines() {
            if let Some(rest) = line.strip_prefix("author = ") {
                return rest.trim().to_string();
            }
        }
        "you <you@localhost>".to_string()
    }
}
