//! Server-side merge for change requests.
//!
//! The server has no working copy, so it merges directly over the object store:
//! `merge::merge_trees` on the two ref tips + their merge base, then writes a
//! merge commit. Conflicts stay first-class (recorded on the commit and reported
//! back), matching chip's model.

use std::str::FromStr;

use chip_core::change::ChangeId;
use chip_core::hash::ObjectId;
use chip_core::object::Commit;
use chip_core::store::ObjectStore;
use chip_core::{dag, merge};

/// Outcome of merging a change request.
pub struct MergeSummary {
    /// The commit the target ref should point at after the merge.
    pub commit: ObjectId,
    /// Files left conflicted (empty on a clean merge / fast-forward).
    pub conflicts: Vec<String>,
}

/// Merge commit `source_hex` into `target_hex` within `store`. Fast-forwards when
/// possible; otherwise performs a three-way merge and writes a merge commit.
pub fn merge_refs(
    store: &ObjectStore,
    author: &str,
    message: &str,
    source_hex: &str,
    target_hex: &str,
) -> anyhow::Result<MergeSummary> {
    let source = ObjectId::from_str(source_hex)?;
    let target = ObjectId::from_str(target_hex)?;

    // Source already reachable from target — nothing to do.
    if dag::is_ancestor(store, source, target)? {
        return Ok(MergeSummary {
            commit: target,
            conflicts: vec![],
        });
    }
    // Fast-forward: target is an ancestor of source — advance to source.
    if dag::is_ancestor(store, target, source)? {
        return Ok(MergeSummary {
            commit: source,
            conflicts: vec![],
        });
    }

    let base_tree = match dag::merge_base(store, source, target)? {
        Some(b) => Some(store.get_commit(&b)?.tree),
        None => None,
    };
    let ours = store.get_commit(&target)?;
    let theirs = store.get_commit(&source)?;
    let result = merge::merge_trees(store, base_tree.as_ref(), &ours.tree, &theirs.tree)?;

    let commit = Commit {
        tree: result.tree,
        parents: vec![target, source],
        change_id: ChangeId::generate(),
        author: author.to_string(),
        timestamp: chip_core::now(),
        message: message.to_string(),
        conflicts: result.conflicts.clone(),
    };
    let id = store.put_commit(commit)?;
    Ok(MergeSummary {
        commit: id,
        conflicts: result.conflicts,
    })
}
