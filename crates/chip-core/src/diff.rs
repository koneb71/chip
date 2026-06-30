use std::fmt;

use similar::{ChangeTag, TextDiff};

use crate::error::Result;
use crate::hash::ObjectId;
use crate::store::ObjectStore;
use crate::working_copy::flatten;

/// A path-level change between two trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Added(String),
    Modified(String),
    Deleted(String),
}

impl Change {
    pub fn path(&self) -> &str {
        match self {
            Change::Added(p) | Change::Modified(p) | Change::Deleted(p) => p,
        }
    }
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Added(p) => write!(f, "added    {p}"),
            Change::Modified(p) => write!(f, "modified {p}"),
            Change::Deleted(p) => write!(f, "deleted  {p}"),
        }
    }
}

/// Path-level differences between `base_tree` and `new_tree`.
pub fn status(
    store: &ObjectStore,
    base_tree: Option<&ObjectId>,
    new_tree: &ObjectId,
) -> Result<Vec<Change>> {
    let base = match base_tree {
        Some(t) => flatten(store, t)?,
        None => Default::default(),
    };
    let new = flatten(store, new_tree)?;

    let mut changes = Vec::new();
    for (path, entry) in &new {
        match base.get(path) {
            None => changes.push(Change::Added(path.clone())),
            Some(old) if old.id != entry.id => changes.push(Change::Modified(path.clone())),
            Some(_) => {}
        }
    }
    for path in base.keys() {
        if !new.contains_key(path) {
            changes.push(Change::Deleted(path.clone()));
        }
    }
    changes.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(changes)
}

/// A unified textual diff between `base_tree` and `new_tree`.
pub fn unified_diff(
    store: &ObjectStore,
    base_tree: Option<&ObjectId>,
    new_tree: &ObjectId,
) -> Result<String> {
    let base = match base_tree {
        Some(t) => flatten(store, t)?,
        None => Default::default(),
    };
    let new = flatten(store, new_tree)?;
    let mut out = String::new();

    let changes = status(store, base_tree, new_tree)?;
    for change in changes {
        let path = change.path().to_string();
        let old_text = match base.get(&path) {
            Some(e) => read_text(store, &e.id)?,
            None => String::new(),
        };
        let new_text = match new.get(&path) {
            Some(e) => read_text(store, &e.id)?,
            None => String::new(),
        };
        let diff = TextDiff::from_lines(&old_text, &new_text);
        out.push_str(
            &diff
                .unified_diff()
                .context_radius(3)
                .header(&format!("a/{path}"), &format!("b/{path}"))
                .to_string(),
        );
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

fn read_text(store: &ObjectStore, id: &ObjectId) -> Result<String> {
    let blob = store.get_blob(id)?;
    Ok(String::from_utf8_lossy(&blob.data).into_owned())
}

// --- Structured diff model -------------------------------------------------

/// File-level change kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
}

impl FileStatus {
    /// Single-letter badge (`A`/`M`/`D`).
    pub fn letter(self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
        }
    }
}

/// The role of a single diff line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Insert,
    Delete,
}

/// One line within a hunk, with its old/new line numbers (when applicable).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    pub content: String,
}

/// A contiguous run of changes with its `@@ … @@` header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// A structured per-file diff, shared by the CLI and web renderers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub status: FileStatus,
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

/// Aggregate insertion/deletion totals across a change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub files: usize,
    pub added: usize,
    pub removed: usize,
}

/// Build structured per-file diffs between two trees.
pub fn file_diffs(
    store: &ObjectStore,
    base_tree: Option<&ObjectId>,
    new_tree: &ObjectId,
) -> Result<Vec<FileDiff>> {
    let base = match base_tree {
        Some(t) => flatten(store, t)?,
        None => Default::default(),
    };
    let new = flatten(store, new_tree)?;

    let mut out = Vec::new();
    for change in status(store, base_tree, new_tree)? {
        let (path, fstatus) = match change {
            Change::Added(p) => (p, FileStatus::Added),
            Change::Modified(p) => (p, FileStatus::Modified),
            Change::Deleted(p) => (p, FileStatus::Deleted),
        };
        let old_bytes = match base.get(&path) {
            Some(e) => store.get_blob(&e.id)?.data,
            None => Vec::new(),
        };
        let new_bytes = match new.get(&path) {
            Some(e) => store.get_blob(&e.id)?.data,
            None => Vec::new(),
        };

        if is_binary(&old_bytes) || is_binary(&new_bytes) {
            out.push(FileDiff {
                path,
                status: fstatus,
                added: 0,
                removed: 0,
                binary: true,
                hunks: Vec::new(),
            });
            continue;
        }

        let old_text = String::from_utf8_lossy(&old_bytes).into_owned();
        let new_text = String::from_utf8_lossy(&new_bytes).into_owned();
        let diff = TextDiff::from_lines(&old_text, &new_text);

        let mut hunks = Vec::new();
        let mut added = 0;
        let mut removed = 0;
        for group in diff.grouped_ops(3) {
            let (Some(first), Some(last)) = (group.first(), group.last()) else {
                continue;
            };
            let o_start = first.old_range().start;
            let o_len = last.old_range().end - o_start;
            let n_start = first.new_range().start;
            let n_len = last.new_range().end - n_start;
            let header = format!(
                "@@ -{},{} +{},{} @@",
                o_start + 1,
                o_len,
                n_start + 1,
                n_len
            );

            let mut lines = Vec::new();
            for op in &group {
                for ch in diff.iter_changes(op) {
                    let kind = match ch.tag() {
                        ChangeTag::Equal => LineKind::Context,
                        ChangeTag::Delete => {
                            removed += 1;
                            LineKind::Delete
                        }
                        ChangeTag::Insert => {
                            added += 1;
                            LineKind::Insert
                        }
                    };
                    lines.push(DiffLine {
                        kind,
                        old_no: ch.old_index().map(|i| i + 1),
                        new_no: ch.new_index().map(|i| i + 1),
                        content: ch.value().trim_end_matches('\n').to_string(),
                    });
                }
            }
            hunks.push(Hunk { header, lines });
        }

        out.push(FileDiff {
            path,
            status: fstatus,
            added,
            removed,
            binary: false,
            hunks,
        });
    }
    Ok(out)
}

/// Aggregate insertion/deletion counts between two trees.
pub fn diff_stat(
    store: &ObjectStore,
    base_tree: Option<&ObjectId>,
    new_tree: &ObjectId,
) -> Result<DiffStat> {
    let diffs = file_diffs(store, base_tree, new_tree)?;
    Ok(DiffStat {
        files: diffs.len(),
        added: diffs.iter().map(|d| d.added).sum(),
        removed: diffs.iter().map(|d| d.removed).sum(),
    })
}

/// Heuristic: treat content with a NUL byte in its first 8 KiB as binary.
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Blob;
    use crate::store::{FilesystemBackend, ObjectStore};
    use crate::working_copy::{build_tree, FileEntry};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn store() -> ObjectStore {
        let dir = tempfile::tempdir().unwrap().keep();
        ObjectStore::new(Arc::new(FilesystemBackend::new(dir)))
    }

    fn tree(store: &ObjectStore, files: &[(&str, &[u8])]) -> ObjectId {
        let mut map = BTreeMap::new();
        for (name, content) in files {
            let id = store
                .put_blob(Blob {
                    data: content.to_vec(),
                })
                .unwrap();
            map.insert((*name).to_string(), FileEntry { mode: 0o644, id });
        }
        build_tree(store, &map).unwrap()
    }

    #[test]
    fn structured_diff_counts_and_status() {
        let store = store();
        let base = tree(&store, &[("keep.txt", b"a\nb\nc\n")]);
        let new = tree(
            &store,
            &[("keep.txt", b"a\nB\nc\n"), ("new.txt", b"x\ny\n")],
        );
        let diffs = file_diffs(&store, Some(&base), &new).unwrap();
        assert_eq!(diffs.len(), 2);

        let added = diffs.iter().find(|d| d.path == "new.txt").unwrap();
        assert_eq!(added.status, FileStatus::Added);
        assert_eq!((added.added, added.removed), (2, 0));

        let modified = diffs.iter().find(|d| d.path == "keep.txt").unwrap();
        assert_eq!(modified.status, FileStatus::Modified);
        assert_eq!((modified.added, modified.removed), (1, 1));
        // Gutter line numbers are present on context lines.
        let ctx = modified.hunks[0]
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Context)
            .unwrap();
        assert!(ctx.old_no.is_some() && ctx.new_no.is_some());

        let stat = diff_stat(&store, Some(&base), &new).unwrap();
        assert_eq!((stat.files, stat.added, stat.removed), (2, 3, 1));
    }

    #[test]
    fn binary_files_are_flagged() {
        let store = store();
        let new = tree(&store, &[("data.bin", &[0u8, 1, 2, 3])]);
        let diffs = file_diffs(&store, None, &new).unwrap();
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].binary);
        assert!(diffs[0].hunks.is_empty());
    }
}
