//! High-level repository operations composing the lower-level modules. These
//! are the verbs the CLI (and later the server) call.

use std::fs;

use crate::change::ChangeId;
use crate::dag::{is_ancestor, merge_base};
use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::object::{Commit, Tree};
use crate::refs::Head;
use crate::repo::{Repo, DEFAULT_BOOKMARK};
use crate::{merge, working_copy};

/// Snapshot the working tree and record it as a new commit, advancing `HEAD`.
/// Returns the new commit id.
pub fn commit(repo: &Repo, message: &str) -> Result<ObjectId> {
    let tree = working_copy::snapshot(repo)?;
    let parents: Vec<ObjectId> = repo.refs().head_commit()?.into_iter().collect();
    let commit = Commit {
        tree,
        parents,
        change_id: ChangeId::generate(),
        author: repo.author(),
        timestamp: crate::now(),
        message: message.to_string(),
        conflicts: vec![],
    };
    let id = repo.store().put_commit(commit)?;
    repo.refs().move_head_to(id, DEFAULT_BOOKMARK)?;
    Ok(id)
}

/// Amend the current change: re-snapshot the working tree and replace the tip
/// commit, **reusing its change-id and parents**. This is what gives a change a
/// stable identity across edits — `chip log` shows the same change-id while the
/// commit (content) hash changes. Returns the new commit id.
pub fn amend(repo: &Repo, message: Option<&str>) -> Result<ObjectId> {
    let head = repo.refs().head_commit()?.ok_or(Error::EmptyHistory)?;
    let old = repo.store().get_commit(&head)?;
    let tree = working_copy::snapshot(repo)?;
    let conflicts = remaining_conflicts(repo, &tree, &old.conflicts)?;
    let commit = Commit {
        tree,
        parents: old.parents.clone(),
        change_id: old.change_id.clone(),
        author: repo.author(),
        timestamp: crate::now(),
        message: message.map(str::to_string).unwrap_or(old.message),
        conflicts,
    };
    let id = repo.store().put_commit(commit)?;
    repo.refs().move_head_to(id, DEFAULT_BOOKMARK)?;
    Ok(id)
}

/// Result of resolving conflicts via [`resolve`].
pub struct ResolveOutcome {
    pub commit: ObjectId,
    pub remaining: Vec<String>,
}

/// Re-snapshot the working tree and clear any conflicts whose markers are gone,
/// keeping the change-id. The normal way to record a conflict resolution.
pub fn resolve(repo: &Repo) -> Result<ResolveOutcome> {
    let head = repo.refs().head_commit()?.ok_or(Error::EmptyHistory)?;
    let old = repo.store().get_commit(&head)?;
    let commit = amend(repo, None)?;
    let _ = old;
    let remaining = repo.store().get_commit(&commit)?.conflicts;
    Ok(ResolveOutcome { commit, remaining })
}

/// Apply the diff that `pick` introduced (relative to its first parent) on top
/// of `onto_tree`, via three-way merge: base = pick's parent tree, ours =
/// `onto_tree`, theirs = `pick.tree`. The shared core of cherry-pick and rebase.
fn apply_onto(
    store: &crate::store::ObjectStore,
    onto_tree: &ObjectId,
    pick: &Commit,
) -> Result<merge::MergeResult> {
    let base_tree = match pick.parents.first() {
        Some(p) => Some(store.get_commit(p)?.tree),
        None => None,
    };
    merge::merge_trees(store, base_tree.as_ref(), onto_tree, &pick.tree)
}

/// Cherry-pick: copy the change `rev` introduced on top of the current `HEAD` as
/// a brand-new change (fresh change-id — it is a *copy*, not the same change).
/// Conflicts stay first-class.
pub fn cherry_pick(repo: &Repo, rev: &str) -> Result<MergeOutcome> {
    let store = repo.store();
    let onto = repo.refs().head_commit()?.ok_or(Error::EmptyHistory)?;
    let pick_id = resolve_commit(repo, rev)?;
    let pick = store.get_commit(&pick_id)?;
    let onto_commit = store.get_commit(&onto)?;

    let result = apply_onto(store, &onto_commit.tree, &pick)?;
    let commit = Commit {
        tree: result.tree,
        parents: vec![onto],
        change_id: ChangeId::generate(),
        author: repo.author(),
        timestamp: crate::now(),
        message: pick.message.clone(),
        conflicts: result.conflicts.clone(),
    };
    let id = store.put_commit(commit)?;
    working_copy::restore(repo, &result.tree)?;
    repo.refs().move_head_to(id, DEFAULT_BOOKMARK)?;
    Ok(MergeOutcome {
        commit: id,
        conflicts: result.conflicts,
        fast_forward: false,
        already_up_to_date: false,
    })
}

/// Revert: create a new commit on top of `HEAD` that **undoes** the change `rev`
/// introduced (the inverse of its diff). Conflicts stay first-class.
pub fn revert(repo: &Repo, rev: &str) -> Result<MergeOutcome> {
    let store = repo.store();
    let onto = repo.refs().head_commit()?.ok_or(Error::EmptyHistory)?;
    let rev_id = resolve_commit(repo, rev)?;
    let rc = store.get_commit(&rev_id)?;
    let onto_commit = store.get_commit(&onto)?;

    // Apply the reverse of rev's diff: base = rev's tree, theirs = rev's parent
    // tree (or the empty tree for a root commit), ours = current HEAD tree.
    let parent_tree = match rc.parents.first() {
        Some(p) => store.get_commit(p)?.tree,
        None => store.put_tree(Tree::default())?,
    };
    let result = merge::merge_trees(store, Some(&rc.tree), &onto_commit.tree, &parent_tree)?;

    let first_line = rc.message.lines().next().unwrap_or("");
    let commit = Commit {
        tree: result.tree,
        parents: vec![onto],
        change_id: ChangeId::generate(),
        author: repo.author(),
        timestamp: crate::now(),
        message: format!("revert \"{first_line}\""),
        conflicts: result.conflicts.clone(),
    };
    let id = store.put_commit(commit)?;
    working_copy::restore(repo, &result.tree)?;
    repo.refs().move_head_to(id, DEFAULT_BOOKMARK)?;
    Ok(MergeOutcome {
        commit: id,
        conflicts: result.conflicts,
        fast_forward: false,
        already_up_to_date: false,
    })
}

/// Discard uncommitted working-tree changes by restoring from the last commit.
/// With `path`, restore a single file; without, reset the whole working tree.
/// Returns the number of files restored (0 or 1 for the single-file form).
pub fn restore(repo: &Repo, path: Option<&str>) -> Result<usize> {
    let store = repo.store();
    let head = repo.refs().head_commit()?.ok_or(Error::EmptyHistory)?;
    let tree = store.get_commit(&head)?.tree;

    match path {
        None => {
            working_copy::restore(repo, &tree)?;
            Ok(0)
        }
        Some(p) => {
            let files = working_copy::flatten(store, &tree)?;
            let entry = files
                .get(p)
                .ok_or_else(|| Error::Other(format!("'{p}' is not tracked in the last commit")))?;
            let blob = store.get_blob(&entry.id)?;
            let abs = repo.root().join(p);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&abs, &blob.data)?;
            Ok(1)
        }
    }
}

/// Rebase the current branch onto `dest`: replay every change on the current
/// first-parent line that `dest` doesn't already have, on top of `dest`,
/// preserving each change's change-id. Conflicts stay first-class.
///
/// Limitation: replays the first-parent chain; merge commits within the rebased
/// range are flattened.
pub fn rebase(repo: &Repo, dest: &str) -> Result<MergeOutcome> {
    let store = repo.store();
    let ours = repo.refs().head_commit()?.ok_or(Error::EmptyHistory)?;
    let dest_id = resolve_commit(repo, dest)?;

    // Nothing to do if we already contain dest.
    if ours == dest_id || is_ancestor(store, dest_id, ours)? {
        return Ok(MergeOutcome {
            commit: ours,
            conflicts: vec![],
            fast_forward: false,
            already_up_to_date: true,
        });
    }
    // Pure fast-forward: our work is already an ancestor of dest.
    if is_ancestor(store, ours, dest_id)? {
        let commit = store.get_commit(&dest_id)?;
        working_copy::restore(repo, &commit.tree)?;
        repo.refs().move_head_to(dest_id, DEFAULT_BOOKMARK)?;
        return Ok(MergeOutcome {
            commit: dest_id,
            conflicts: vec![],
            fast_forward: true,
            already_up_to_date: false,
        });
    }

    let range = rebase_range(store, ours, dest_id)?;

    let mut onto = dest_id;
    let mut all_conflicts: Vec<String> = Vec::new();
    for cid in range {
        let c = store.get_commit(&cid)?;
        let onto_tree = store.get_commit(&onto)?.tree;
        let result = apply_onto(store, &onto_tree, &c)?;
        for path in &result.conflicts {
            if !all_conflicts.contains(path) {
                all_conflicts.push(path.clone());
            }
        }
        let commit = Commit {
            tree: result.tree,
            parents: vec![onto],
            change_id: c.change_id.clone(),
            author: c.author.clone(),
            timestamp: crate::now(),
            message: c.message.clone(),
            conflicts: result.conflicts,
        };
        onto = store.put_commit(commit)?;
    }

    let final_tree = store.get_commit(&onto)?.tree;
    working_copy::restore(repo, &final_tree)?;
    repo.refs().move_head_to(onto, DEFAULT_BOOKMARK)?;
    Ok(MergeOutcome {
        commit: onto,
        conflicts: all_conflicts,
        fast_forward: false,
        already_up_to_date: false,
    })
}

/// The commits to replay when rebasing `ours` onto `dest`: the first-parent
/// chain from `ours` back to (but excluding) the first commit already contained
/// in `dest`, returned oldest-first.
fn rebase_range(
    store: &crate::store::ObjectStore,
    ours: ObjectId,
    dest: ObjectId,
) -> Result<Vec<ObjectId>> {
    let dest_ancestors = crate::dag::ancestors(store, dest)?;
    let mut chain = Vec::new();
    let mut cur = Some(ours);
    while let Some(id) = cur {
        if dest_ancestors.contains(&id) {
            break;
        }
        chain.push(id);
        cur = store.get_commit(&id)?.parents.first().copied();
    }
    chain.reverse();
    Ok(chain)
}

/// Of `candidates`, the paths in `tree` whose contents still contain conflict
/// markers.
fn remaining_conflicts(repo: &Repo, tree: &ObjectId, candidates: &[String]) -> Result<Vec<String>> {
    if candidates.is_empty() {
        return Ok(vec![]);
    }
    let files = working_copy::flatten(repo.store(), tree)?;
    let mut out = Vec::new();
    for path in candidates {
        if let Some(entry) = files.get(path) {
            let blob = repo.store().get_blob(&entry.id)?;
            if merge::has_conflict_markers(&String::from_utf8_lossy(&blob.data)) {
                out.push(path.clone());
            }
        }
    }
    Ok(out)
}

/// Point `HEAD` at `target` (a bookmark name or commit id) and update the
/// working tree to match.
pub fn checkout(repo: &Repo, target: &str) -> Result<ObjectId> {
    let refs = repo.refs();
    let (head, commit_id) = if let Some(id) = refs.read_bookmark(target)? {
        (Head::Bookmark(target.to_string()), id)
    } else {
        let id = resolve_commit(repo, target)?;
        (Head::Detached(id), id)
    };
    let commit = repo.store().get_commit(&commit_id)?;
    working_copy::restore(repo, &commit.tree)?;
    refs.write_head(&head)?;
    Ok(commit_id)
}

/// The result of a merge.
pub struct MergeOutcome {
    pub commit: ObjectId,
    pub conflicts: Vec<String>,
    pub fast_forward: bool,
    pub already_up_to_date: bool,
}

/// Merge `target` into the current `HEAD`.
pub fn merge(repo: &Repo, target: &str) -> Result<MergeOutcome> {
    let store = repo.store();
    let ours = repo.refs().head_commit()?.ok_or(Error::EmptyHistory)?;
    let theirs = resolve_commit(repo, target)?;

    if ours == theirs || is_ancestor(store, theirs, ours)? {
        return Ok(MergeOutcome {
            commit: ours,
            conflicts: vec![],
            fast_forward: false,
            already_up_to_date: true,
        });
    }

    // Fast-forward: our commit is an ancestor of theirs.
    if is_ancestor(store, ours, theirs)? {
        let commit = store.get_commit(&theirs)?;
        working_copy::restore(repo, &commit.tree)?;
        repo.refs().move_head_to(theirs, DEFAULT_BOOKMARK)?;
        return Ok(MergeOutcome {
            commit: theirs,
            conflicts: vec![],
            fast_forward: true,
            already_up_to_date: false,
        });
    }

    let base = merge_base(store, ours, theirs)?;
    let our_commit = store.get_commit(&ours)?;
    let their_commit = store.get_commit(&theirs)?;
    let base_tree = match base {
        Some(b) => Some(store.get_commit(&b)?.tree),
        None => None,
    };

    let result = merge::merge_trees(
        store,
        base_tree.as_ref(),
        &our_commit.tree,
        &their_commit.tree,
    )?;

    let message = if result.conflicts.is_empty() {
        format!("merge {}", short(&theirs))
    } else {
        format!("merge {} (with conflicts)", short(&theirs))
    };

    let merge_commit = Commit {
        tree: result.tree,
        parents: vec![ours, theirs],
        change_id: ChangeId::generate(),
        author: repo.author(),
        timestamp: crate::now(),
        message,
        conflicts: result.conflicts.clone(),
    };
    let id = store.put_commit(merge_commit)?;
    working_copy::restore(repo, &result.tree)?;
    repo.refs().move_head_to(id, DEFAULT_BOOKMARK)?;

    Ok(MergeOutcome {
        commit: id,
        conflicts: result.conflicts,
        fast_forward: false,
        already_up_to_date: false,
    })
}

/// Resolve a user-supplied revision (bookmark name, tag name, or commit hex)
/// to a commit id.
pub fn resolve_commit(repo: &Repo, rev: &str) -> Result<ObjectId> {
    let refs = repo.refs();
    if rev == "@" || rev.eq_ignore_ascii_case("HEAD") {
        return refs.head_commit()?.ok_or(Error::EmptyHistory);
    }
    if let Some(id) = refs.read_bookmark(rev)? {
        return Ok(id);
    }
    if let Some(id) = refs.read_tag(rev)? {
        return Ok(id);
    }
    // Try as a full commit id.
    if let Ok(id) = rev.parse::<ObjectId>() {
        if repo.store().contains(&id)? {
            return Ok(id);
        }
    }
    // Try as an abbreviated commit id among the commits visible from refs.
    if rev.len() >= 4 && rev.len() < 64 && rev.bytes().all(|b| b.is_ascii_hexdigit()) {
        if let Some(id) = resolve_prefix(repo, &rev.to_lowercase())? {
            return Ok(id);
        }
    }
    Err(Error::RefNotFound(rev.to_string()))
}

/// Resolve an abbreviated hex commit id by matching it against every commit
/// reachable from a ref (HEAD, bookmarks, tags) — i.e. anything visible in
/// `chip log`. Errors if the prefix is ambiguous.
fn resolve_prefix(repo: &Repo, prefix: &str) -> Result<Option<ObjectId>> {
    let refs = repo.refs();
    let mut roots: Vec<ObjectId> = Vec::new();
    if let Some(h) = refs.head_commit()? {
        roots.push(h);
    }
    for (_, id) in refs.list_bookmarks()? {
        roots.push(id);
    }
    for (_, id) in refs.list_tags()? {
        roots.push(id);
    }

    let mut matches: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
    let mut seen: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
    for root in roots {
        for (id, _) in crate::dag::history(repo.store(), root)? {
            if !seen.insert(id) {
                continue;
            }
            if id.to_hex().starts_with(prefix) {
                matches.insert(id);
            }
        }
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(*matches.iter().next().unwrap())),
        _ => Err(Error::Other(format!("ambiguous commit prefix '{prefix}'"))),
    }
}

fn short(id: &ObjectId) -> String {
    id.short()
}
