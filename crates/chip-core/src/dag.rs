use std::collections::{HashSet, VecDeque};

use crate::error::Result;
use crate::hash::ObjectId;
use crate::object::{Commit, EntryKind, Object};
use crate::store::ObjectStore;

/// All ancestors of `start` (inclusive), as a set of commit ids.
pub fn ancestors(store: &ObjectStore, start: ObjectId) -> Result<HashSet<ObjectId>> {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([start]);
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        let commit = store.get_commit(&id)?;
        for parent in commit.parents {
            queue.push_back(parent);
        }
    }
    Ok(seen)
}

/// True if `maybe_ancestor` is an ancestor of (or equal to) `descendant`.
pub fn is_ancestor(
    store: &ObjectStore,
    maybe_ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<bool> {
    Ok(ancestors(store, descendant)?.contains(&maybe_ancestor))
}

/// The merge base (nearest common ancestor) of two commits, if any.
///
/// We collect the full ancestor set of `a`, then breadth-first walk `b`'s
/// ancestry and return the first commit also present in `a`'s set — i.e. the
/// common ancestor closest to `b`.
pub fn merge_base(store: &ObjectStore, a: ObjectId, b: ObjectId) -> Result<Option<ObjectId>> {
    let a_anc = ancestors(store, a)?;
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([b]);
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        if a_anc.contains(&id) {
            return Ok(Some(id));
        }
        let commit = store.get_commit(&id)?;
        for parent in commit.parents {
            queue.push_back(parent);
        }
    }
    Ok(None)
}

/// Every object id (commits, trees, blobs) reachable from `roots`, following
/// commit parents and walking each commit's tree. Used by sync to compute the
/// transitive closure to transfer. Missing objects are skipped (a `have` set
/// may reference commits this store does not hold).
pub fn reachable_objects(store: &ObjectStore, roots: &[ObjectId]) -> Result<HashSet<ObjectId>> {
    let mut acc = HashSet::new();
    // Iterative walk (explicit stacks) so deep histories / large trees can't
    // overflow the call stack.
    let mut commit_stack: Vec<ObjectId> = roots.to_vec();
    let mut tree_stack: Vec<ObjectId> = Vec::new();

    while let Some(id) = commit_stack.pop() {
        if !acc.insert(id) {
            continue;
        }
        let commit = match store.get(&id) {
            Ok(Object::Commit(c)) => c,
            Ok(_) => continue,
            Err(_) => {
                // A `have` set may reference commits this store lacks.
                acc.remove(&id);
                continue;
            }
        };
        tree_stack.push(commit.tree);
        commit_stack.extend(commit.parents);

        while let Some(tid) = tree_stack.pop() {
            if !acc.insert(tid) {
                continue;
            }
            if let Ok(Object::Tree(tree)) = store.get(&tid) {
                for entry in tree.entries {
                    match entry.kind {
                        EntryKind::Blob => {
                            acc.insert(entry.id);
                        }
                        EntryKind::Tree => tree_stack.push(entry.id),
                    }
                }
            }
        }
    }
    Ok(acc)
}

/// History reachable from `head`, newest first (by timestamp, then id).
pub fn history(store: &ObjectStore, head: ObjectId) -> Result<Vec<(ObjectId, Commit)>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut queue = VecDeque::from([head]);
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        let commit = store.get_commit(&id)?;
        for parent in &commit.parents {
            queue.push_back(*parent);
        }
        out.push((id, commit));
    }
    out.sort_by(|a, b| {
        b.1.timestamp
            .cmp(&a.1.timestamp)
            .then_with(|| b.0.to_hex().cmp(&a.0.to_hex()))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::ChangeId;
    use crate::object::Tree;
    use crate::store::{FilesystemBackend, ObjectStore};
    use std::sync::Arc;

    fn store() -> ObjectStore {
        let dir = tempfile::tempdir().unwrap().keep();
        ObjectStore::new(Arc::new(FilesystemBackend::new(dir)))
    }

    fn commit(store: &ObjectStore, parents: Vec<ObjectId>, msg: &str) -> ObjectId {
        let tree = store.put_tree(Tree::default()).unwrap();
        store
            .put_commit(Commit {
                tree,
                parents,
                change_id: ChangeId::generate(),
                author: "t".into(),
                timestamp: 0,
                message: msg.into(),
                conflicts: vec![],
            })
            .unwrap()
    }

    #[test]
    fn merge_base_of_diverged_branches() {
        // root -> a -> b   (branch 1)
        //      \-> c       (branch 2)
        let store = store();
        let root = commit(&store, vec![], "root");
        let a = commit(&store, vec![root], "a");
        let b = commit(&store, vec![a], "b");
        let c = commit(&store, vec![root], "c");
        assert_eq!(merge_base(&store, b, c).unwrap(), Some(root));
    }

    #[test]
    fn is_ancestor_works() {
        let store = store();
        let root = commit(&store, vec![], "root");
        let a = commit(&store, vec![root], "a");
        assert!(is_ancestor(&store, root, a).unwrap());
        assert!(!is_ancestor(&store, a, root).unwrap());
    }

    #[test]
    fn reachable_handles_deep_history_without_overflow() {
        // A long linear chain that the previous recursive walk would overflow on.
        let store = store();
        let mut tip = commit(&store, vec![], "root");
        for i in 0..20_000 {
            tip = commit(&store, vec![tip], &format!("c{i}"));
        }
        let objs = reachable_objects(&store, &[tip]).unwrap();
        // 20001 commits, each sharing the same empty tree => 20002 objects.
        assert_eq!(objs.len(), 20_002);
    }
}
