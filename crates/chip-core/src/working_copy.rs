use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use ignore::WalkBuilder;

use crate::error::Result;
use crate::hash::ObjectId;
use crate::object::{Blob, EntryKind, Tree, TreeEntry};
use crate::repo::Repo;
use crate::store::ObjectStore;

/// In-memory directory node used while building a snapshot.
enum Node {
    File { mode: u32, id: ObjectId },
    Dir(BTreeMap<String, Node>),
}

impl Node {
    fn dir_mut(&mut self) -> &mut BTreeMap<String, Node> {
        match self {
            Node::Dir(m) => m,
            _ => unreachable!("expected directory node"),
        }
    }
}

/// Snapshot the working tree into the object store, returning the root tree id.
///
/// There is no staging area: the entire working tree (minus `.chip` and
/// anything matched by `.chipignore`) is captured as-is.
pub fn snapshot(repo: &Repo) -> Result<ObjectId> {
    let store = repo.store();
    let mut root = Node::Dir(BTreeMap::new());

    let walker = WalkBuilder::new(repo.root())
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .add_custom_ignore_filename(".chipignore")
        .filter_entry(|e| e.file_name() != ".chip")
        .build();

    for entry in walker {
        let entry = entry.map_err(|e| crate::error::Error::Other(e.to_string()))?;
        let path = entry.path();
        if path == repo.root() {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) if m.is_file() => m,
            _ => continue, // skip directories (created implicitly) and symlinks for now
        };
        let rel = path.strip_prefix(repo.root()).unwrap();
        let data = fs::read(path)?;
        let id = store.put_blob(Blob { data })?;
        let mode = file_mode(&meta);
        insert(&mut root, rel, mode, id);
    }

    write_tree(store, &root)
}

fn insert(root: &mut Node, rel: &Path, mode: u32, id: ObjectId) {
    let components: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let mut cur = root;
    for comp in &components[..components.len() - 1] {
        cur = cur
            .dir_mut()
            .entry(comp.clone())
            .or_insert_with(|| Node::Dir(BTreeMap::new()));
    }
    let leaf = components.last().unwrap().clone();
    cur.dir_mut().insert(leaf, Node::File { mode, id });
}

fn write_tree(store: &ObjectStore, node: &Node) -> Result<ObjectId> {
    let map = match node {
        Node::Dir(m) => m,
        Node::File { .. } => unreachable!(),
    };
    let mut entries = Vec::with_capacity(map.len());
    for (name, child) in map {
        let entry = match child {
            Node::File { mode, id } => TreeEntry {
                name: name.clone(),
                kind: EntryKind::Blob,
                mode: *mode,
                id: *id,
            },
            Node::Dir(_) => {
                let id = write_tree(store, child)?;
                TreeEntry {
                    name: name.clone(),
                    kind: EntryKind::Tree,
                    mode: 0o040000,
                    id,
                }
            }
        };
        entries.push(entry);
    }
    store.put_tree(Tree::new(entries))
}

#[cfg(unix)]
fn file_mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 != 0 {
        0o755
    } else {
        0o644
    }
}

#[cfg(not(unix))]
fn file_mode(_meta: &fs::Metadata) -> u32 {
    0o644
}

/// A flat (path -> entry) view of a tree, recursively, with paths relative to
/// the tree root. Useful for diffing and status.
pub fn flatten(store: &ObjectStore, tree_id: &ObjectId) -> Result<BTreeMap<String, FileEntry>> {
    let mut out = BTreeMap::new();
    flatten_into(store, tree_id, "", &mut out)?;
    Ok(out)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub mode: u32,
    pub id: ObjectId,
}

/// Build a (possibly nested) tree from a flat `path -> FileEntry` map and store
/// it, returning the root tree id. The inverse of [`flatten`].
pub fn build_tree(store: &ObjectStore, files: &BTreeMap<String, FileEntry>) -> Result<ObjectId> {
    let mut root = Node::Dir(BTreeMap::new());
    for (path, entry) in files {
        insert(&mut root, Path::new(path), entry.mode, entry.id);
    }
    write_tree(store, &root)
}

fn flatten_into(
    store: &ObjectStore,
    tree_id: &ObjectId,
    prefix: &str,
    out: &mut BTreeMap<String, FileEntry>,
) -> Result<()> {
    let tree = store.get_tree(tree_id)?;
    for entry in tree.entries {
        let path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        match entry.kind {
            EntryKind::Blob => {
                out.insert(
                    path,
                    FileEntry {
                        mode: entry.mode,
                        id: entry.id,
                    },
                );
            }
            EntryKind::Tree => flatten_into(store, &entry.id, &path, out)?,
        }
    }
    Ok(())
}

/// Replace the working tree contents with the snapshot in `tree_id`.
///
/// Removes tracked files that are not present in the target tree, then writes
/// every file from the target. Leaves `.chip` untouched.
pub fn restore(repo: &Repo, tree_id: &ObjectId) -> Result<()> {
    let store = repo.store();
    let target = flatten(store, tree_id)?;

    // Remove existing tracked files not in the target.
    let walker = WalkBuilder::new(repo.root())
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .add_custom_ignore_filename(".chipignore")
        .filter_entry(|e| e.file_name() != ".chip")
        .build();
    let mut existing = Vec::new();
    for entry in walker {
        let entry = entry.map_err(|e| crate::error::Error::Other(e.to_string()))?;
        if entry.path() == repo.root() {
            continue;
        }
        if entry.metadata().map(|m| m.is_file()).unwrap_or(false) {
            let rel = entry
                .path()
                .strip_prefix(repo.root())
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            existing.push(rel);
        }
    }
    for rel in existing {
        if !target.contains_key(&rel) {
            let _ = fs::remove_file(repo.root().join(&rel));
        }
    }

    // Write target files.
    for (path, entry) in &target {
        let abs = repo.root().join(path);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        let blob = store.get_blob(&entry.id)?;
        fs::write(&abs, &blob.data)?;
        set_mode(&abs, entry.mode)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}
