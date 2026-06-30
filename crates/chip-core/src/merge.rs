//! Three-way merge with first-class conflicts.
//!
//! When two sides change the same file incompatibly, chip does *not* abort.
//! It writes the merged file with conflict markers, records the path in the
//! resulting commit's `conflicts` list, and lets you keep working — resolving
//! is just another edit + commit.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::Result;
use crate::hash::ObjectId;
use crate::object::Blob;
use crate::store::ObjectStore;
use crate::working_copy::{build_tree, flatten, FileEntry};

/// Conflict marker chip writes into a conflicted file (diffy's default).
pub const CONFLICT_START: &str = "<<<<<<<";
pub const CONFLICT_END: &str = ">>>>>>>";

/// Whether `text` still contains unresolved conflict markers.
pub fn has_conflict_markers(text: &str) -> bool {
    text.contains(CONFLICT_START) && text.contains(CONFLICT_END)
}

/// Outcome of merging two trees against their base.
pub struct MergeResult {
    pub tree: ObjectId,
    /// Paths that merged with conflict markers.
    pub conflicts: Vec<String>,
}

/// Three-way merge `ours` and `theirs` using `base` as the common ancestor.
pub fn merge_trees(
    store: &ObjectStore,
    base: Option<&ObjectId>,
    ours: &ObjectId,
    theirs: &ObjectId,
) -> Result<MergeResult> {
    let base_files = match base {
        Some(b) => flatten(store, b)?,
        None => BTreeMap::new(),
    };
    let our_files = flatten(store, ours)?;
    let their_files = flatten(store, theirs)?;

    let mut paths: BTreeSet<&String> = BTreeSet::new();
    paths.extend(our_files.keys());
    paths.extend(their_files.keys());

    let mut merged: BTreeMap<String, FileEntry> = BTreeMap::new();
    let mut conflicts = Vec::new();

    for path in paths {
        let b = base_files.get(path);
        let o = our_files.get(path);
        let t = their_files.get(path);

        match (o, t) {
            // Present on both sides.
            (Some(o), Some(t)) => {
                if o.id == t.id {
                    merged.insert(path.clone(), o.clone());
                } else if b.map(|b| b.id) == Some(o.id) {
                    // Only theirs changed.
                    merged.insert(path.clone(), t.clone());
                } else if b.map(|b| b.id) == Some(t.id) {
                    // Only ours changed.
                    merged.insert(path.clone(), o.clone());
                } else {
                    // Both changed: real three-way text merge.
                    let (entry, conflicted) =
                        merge_file(store, b.map(|e| &e.id), &o.id, &t.id, o.mode)?;
                    if conflicted {
                        conflicts.push(path.clone());
                    }
                    merged.insert(path.clone(), entry);
                }
            }
            // Added on one side only (or unchanged-delete on the other).
            (Some(o), None) => {
                if b.is_none() || b.map(|b| b.id) == Some(o.id) {
                    // Added by us, or unchanged by us & deleted by them -> keep ours
                    // when we added it; otherwise honour their delete.
                    if b.is_none() {
                        merged.insert(path.clone(), o.clone());
                    }
                    // (b == ours) means they deleted an unchanged file: drop it.
                } else {
                    // We modified, they deleted: modify/delete conflict.
                    merged.insert(path.clone(), o.clone());
                    conflicts.push(path.clone());
                }
            }
            (None, Some(t)) => {
                if b.is_none() || b.map(|b| b.id) == Some(t.id) {
                    if b.is_none() {
                        merged.insert(path.clone(), t.clone());
                    }
                } else {
                    merged.insert(path.clone(), t.clone());
                    conflicts.push(path.clone());
                }
            }
            (None, None) => {}
        }
    }

    let tree = build_tree(store, &merged)?;
    Ok(MergeResult { tree, conflicts })
}

/// Merge a single file's three versions. Returns the stored entry and whether
/// the result contains conflict markers.
fn merge_file(
    store: &ObjectStore,
    base: Option<&ObjectId>,
    ours: &ObjectId,
    theirs: &ObjectId,
    mode: u32,
) -> Result<(FileEntry, bool)> {
    let base_text = match base {
        Some(id) => read_text(store, id)?,
        None => String::new(),
    };
    let our_text = read_text(store, ours)?;
    let their_text = read_text(store, theirs)?;

    let (content, conflicted) = match diffy::merge(&base_text, &our_text, &their_text) {
        Ok(clean) => (clean, false),
        Err(with_markers) => (with_markers, true),
    };

    let id = store.put_blob(Blob {
        data: content.into_bytes(),
    })?;
    Ok((FileEntry { mode, id }, conflicted))
}

fn read_text(store: &ObjectStore, id: &ObjectId) -> Result<String> {
    let blob = store.get_blob(id)?;
    Ok(String::from_utf8_lossy(&blob.data).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FilesystemBackend;
    use crate::working_copy::build_tree;
    use std::sync::Arc;

    fn store() -> ObjectStore {
        let dir = tempfile::tempdir().unwrap().keep();
        ObjectStore::new(Arc::new(FilesystemBackend::new(dir)))
    }

    fn tree_with(store: &ObjectStore, path: &str, content: &str) -> ObjectId {
        let id = store
            .put_blob(Blob {
                data: content.as_bytes().to_vec(),
            })
            .unwrap();
        let mut files = BTreeMap::new();
        files.insert(path.to_string(), FileEntry { mode: 0o644, id });
        build_tree(store, &files).unwrap()
    }

    #[test]
    fn clean_merge_non_overlapping() {
        let store = store();
        let base = tree_with(&store, "f.txt", "line1\nline2\nline3\n");
        let ours = tree_with(&store, "f.txt", "LINE1\nline2\nline3\n");
        let theirs = tree_with(&store, "f.txt", "line1\nline2\nLINE3\n");
        let result = merge_trees(&store, Some(&base), &ours, &theirs).unwrap();
        assert!(result.conflicts.is_empty());
        let files = flatten(&store, &result.tree).unwrap();
        let merged = read_text(&store, &files["f.txt"].id).unwrap();
        assert_eq!(merged, "LINE1\nline2\nLINE3\n");
    }

    #[test]
    fn overlapping_edit_conflicts_but_succeeds() {
        let store = store();
        let base = tree_with(&store, "f.txt", "hello\n");
        let ours = tree_with(&store, "f.txt", "goodbye\n");
        let theirs = tree_with(&store, "f.txt", "farewell\n");
        let result = merge_trees(&store, Some(&base), &ours, &theirs).unwrap();
        assert_eq!(result.conflicts, vec!["f.txt".to_string()]);
        // Conflict markers are present, and the merge still produced a tree.
        let files = flatten(&store, &result.tree).unwrap();
        let merged = read_text(&store, &files["f.txt"].id).unwrap();
        assert!(merged.contains("<<<<<<<"));
        assert!(merged.contains(">>>>>>>"));
    }
}
