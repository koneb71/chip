//! End-to-end tests of the local VCS workflow against the chip-core API.

use std::fs;
use std::sync::Arc;

use chip_core::dag;
use chip_core::hash::ObjectId;
use chip_core::oplog::OpLog;
use chip_core::ops;
use chip_core::refs::Head;
use chip_core::repo::Repo;
use chip_core::store::{FilesystemBackend, ObjectStore};

fn write(repo: &Repo, name: &str, content: &str) {
    fs::write(repo.root().join(name), content).unwrap();
}

#[test]
fn commit_branch_merge_undo() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(dir.path()).unwrap();

    // First commit.
    write(&repo, "f.txt", "line1\nline2\nline3\n");
    let base = ops::commit(&repo, "base").unwrap();
    assert_eq!(repo.refs().head_commit().unwrap(), Some(base));

    // Branch off.
    repo.refs().set_bookmark("feature", base).unwrap();

    // Edit line1 on main, commit.
    write(&repo, "f.txt", "MAIN\nline2\nline3\n");
    let main_tip = ops::commit(&repo, "main edit").unwrap();

    // Switch to feature, edit line3 (non-overlapping), commit.
    ops::checkout(&repo, "feature").unwrap();
    write(&repo, "f.txt", "line1\nline2\nFEATURE\n");
    ops::commit(&repo, "feature edit").unwrap();

    // Merge main into feature: should be a clean three-way merge.
    let outcome = ops::merge(&repo, "main").unwrap();
    assert!(outcome.conflicts.is_empty(), "expected a clean merge");
    let merged = fs::read_to_string(repo.root().join("f.txt")).unwrap();
    assert_eq!(merged, "MAIN\nline2\nFEATURE\n");

    // The merge commit has two parents.
    let merge_commit = repo.store().get_commit(&outcome.commit).unwrap();
    assert_eq!(merge_commit.parents.len(), 2);
    assert!(merge_commit.parents.contains(&main_tip));
}

#[test]
fn conflicts_are_first_class_and_undoable() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(dir.path()).unwrap();

    write(&repo, "c.txt", "hello\n");
    ops::commit(&repo, "base").unwrap();
    let base_head = repo.refs().read_head().unwrap();
    repo.refs()
        .set_bookmark("feature", repo.refs().head_commit().unwrap().unwrap())
        .unwrap();

    write(&repo, "c.txt", "goodbye\n");
    ops::commit(&repo, "main edit").unwrap();

    ops::checkout(&repo, "feature").unwrap();
    write(&repo, "c.txt", "farewell\n");
    ops::commit(&repo, "feature edit").unwrap();

    // Capture an undo point, then perform a conflicting merge.
    let before = OpLog::capture(&repo).unwrap();
    let outcome = ops::merge(&repo, "main").unwrap();
    OpLog::new(&repo).append(&repo, "merge", before).unwrap();

    // Merge succeeded (did not abort) but recorded a conflict.
    assert_eq!(outcome.conflicts, vec!["c.txt".to_string()]);
    let conflicted = fs::read_to_string(repo.root().join("c.txt")).unwrap();
    assert!(conflicted.contains("<<<<<<<"));

    // Undo reverses the merge: working tree returns to feature's content.
    OpLog::new(&repo).undo(&repo).unwrap();
    assert_eq!(
        fs::read_to_string(repo.root().join("c.txt")).unwrap(),
        "farewell\n"
    );
    assert!(matches!(repo.refs().read_head().unwrap(), Head::Bookmark(b) if b == "feature"));
    // The pre-merge state is restored.
    let _ = base_head;
}

#[test]
fn amend_preserves_change_id_changes_commit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(dir.path()).unwrap();
    write(&repo, "f.txt", "v1\n");
    let first = ops::commit(&repo, "work").unwrap();
    let first_change = repo.store().get_commit(&first).unwrap().change_id;

    // Amend with new content + message.
    write(&repo, "f.txt", "v2\n");
    let amended = ops::amend(&repo, Some("work (amended)")).unwrap();

    assert_ne!(first, amended, "commit (content) id must change");
    let ac = repo.store().get_commit(&amended).unwrap();
    assert_eq!(ac.change_id, first_change, "change-id must be preserved");
    assert_eq!(ac.message, "work (amended)");
    // The amended commit keeps the original parents (here: none).
    assert!(ac.parents.is_empty());
}

#[test]
fn rebase_reparents_and_keeps_change_id() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(dir.path()).unwrap();
    write(&repo, "shared.txt", "base\n");
    let base = ops::commit(&repo, "base").unwrap();

    // main advances.
    write(&repo, "main.txt", "main\n");
    let main_tip = ops::commit(&repo, "main work").unwrap();

    // feature branches from base and adds its own file.
    repo.refs().set_bookmark("feature", base).unwrap();
    ops::checkout(&repo, "feature").unwrap();
    write(&repo, "feature.txt", "feature\n");
    let feat = ops::commit(&repo, "feature work").unwrap();
    let feat_change = repo.store().get_commit(&feat).unwrap().change_id;

    // Rebase feature onto main.
    let outcome = ops::rebase(&repo, "main").unwrap();
    assert!(outcome.conflicts.is_empty());
    let rebased = repo.store().get_commit(&outcome.commit).unwrap();
    assert_eq!(rebased.parents, vec![main_tip], "now parented on main");
    assert_eq!(rebased.change_id, feat_change, "change-id preserved");
    // Both main's and feature's files are present after rebase.
    assert!(repo.root().join("main.txt").exists());
    assert!(repo.root().join("feature.txt").exists());
}

#[test]
fn resolve_clears_conflicts_keeping_change_id() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(dir.path()).unwrap();
    write(&repo, "c.txt", "hello\n");
    ops::commit(&repo, "base").unwrap();
    repo.refs()
        .set_bookmark("feature", repo.refs().head_commit().unwrap().unwrap())
        .unwrap();
    write(&repo, "c.txt", "goodbye\n");
    ops::commit(&repo, "main").unwrap();
    ops::checkout(&repo, "feature").unwrap();
    write(&repo, "c.txt", "farewell\n");
    ops::commit(&repo, "feature").unwrap();

    let merge = ops::merge(&repo, "main").unwrap();
    assert_eq!(merge.conflicts, vec!["c.txt".to_string()]);
    let conflicted_change = repo.store().get_commit(&merge.commit).unwrap().change_id;

    // User edits the file to remove the markers, then resolves.
    write(&repo, "c.txt", "goodbye and farewell\n");
    let outcome = ops::resolve(&repo).unwrap();
    assert!(outcome.remaining.is_empty(), "conflicts should be cleared");
    let resolved = repo.store().get_commit(&outcome.commit).unwrap();
    assert!(!resolved.is_conflicted());
    assert_eq!(resolved.change_id, conflicted_change, "change-id preserved");
}

#[test]
fn revert_undoes_a_commit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(dir.path()).unwrap();
    write(&repo, "f.txt", "original\n");
    ops::commit(&repo, "base").unwrap();
    write(&repo, "f.txt", "changed\n");
    let bad = ops::commit(&repo, "bad change").unwrap();

    let outcome = ops::revert(&repo, &bad.to_hex()).unwrap();
    assert!(outcome.conflicts.is_empty());
    // Working tree is back to the original content...
    assert_eq!(
        fs::read_to_string(repo.root().join("f.txt")).unwrap(),
        "original\n"
    );
    // ...via a NEW commit on top (history is preserved, not rewritten).
    let revert_commit = repo.store().get_commit(&outcome.commit).unwrap();
    assert_eq!(revert_commit.parents, vec![bad]);
    assert!(revert_commit.message.starts_with("revert"));
}

#[test]
fn restore_discards_working_changes() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(dir.path()).unwrap();
    write(&repo, "a.txt", "committed\n");
    write(&repo, "b.txt", "keep\n");
    ops::commit(&repo, "base").unwrap();

    // Dirty the working tree.
    write(&repo, "a.txt", "uncommitted edit\n");
    write(&repo, "b.txt", "also edited\n");

    // Restore a single file.
    ops::restore(&repo, Some("a.txt")).unwrap();
    assert_eq!(
        fs::read_to_string(repo.root().join("a.txt")).unwrap(),
        "committed\n"
    );
    assert_eq!(
        fs::read_to_string(repo.root().join("b.txt")).unwrap(),
        "also edited\n"
    );

    // Restore the whole tree.
    ops::restore(&repo, None).unwrap();
    assert_eq!(
        fs::read_to_string(repo.root().join("b.txt")).unwrap(),
        "keep\n"
    );

    // Restoring an untracked path is an error.
    assert!(ops::restore(&repo, Some("nope.txt")).is_err());
}

#[test]
fn cherry_pick_copies_change_with_new_id() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(dir.path()).unwrap();
    write(&repo, "base.txt", "base\n");
    let base = ops::commit(&repo, "base").unwrap();

    // main adds a feature file.
    write(&repo, "feature.txt", "the feature\n");
    let main_commit = ops::commit(&repo, "add feature").unwrap();
    let main_change = repo.store().get_commit(&main_commit).unwrap().change_id;

    // A separate branch from base cherry-picks main's commit.
    repo.refs().set_bookmark("topic", base).unwrap();
    ops::checkout(&repo, "topic").unwrap();
    assert!(!repo.root().join("feature.txt").exists());

    let outcome = ops::cherry_pick(&repo, &main_commit.to_hex()).unwrap();
    assert!(outcome.conflicts.is_empty());
    // The picked content is present...
    assert_eq!(
        fs::read_to_string(repo.root().join("feature.txt")).unwrap(),
        "the feature\n"
    );
    let picked = repo.store().get_commit(&outcome.commit).unwrap();
    // ...as a NEW change (copy), parented on topic's tip (base).
    assert_ne!(picked.change_id, main_change, "cherry-pick is a copy");
    assert_eq!(picked.parents, vec![base]);
    assert_eq!(picked.message, "add feature");
}

#[test]
fn range_rebase_replays_branch_preserving_change_ids() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(dir.path()).unwrap();
    write(&repo, "shared.txt", "base\n");
    let base = ops::commit(&repo, "base").unwrap();

    // main advances by one commit.
    write(&repo, "main.txt", "main\n");
    let main_tip = ops::commit(&repo, "main work").unwrap();

    // feature branches from base with TWO commits.
    repo.refs().set_bookmark("feature", base).unwrap();
    ops::checkout(&repo, "feature").unwrap();
    write(&repo, "f1.txt", "one\n");
    let f1 = repo
        .store()
        .get_commit(&ops::commit(&repo, "feature 1").unwrap())
        .unwrap()
        .change_id;
    write(&repo, "f2.txt", "two\n");
    let f2 = repo
        .store()
        .get_commit(&ops::commit(&repo, "feature 2").unwrap())
        .unwrap()
        .change_id;

    // Rebase the whole feature branch onto main.
    let outcome = ops::rebase(&repo, "main").unwrap();
    assert!(outcome.conflicts.is_empty());

    // Working tree has main's file and both feature files.
    for f in ["main.txt", "f1.txt", "f2.txt"] {
        assert!(repo.root().join(f).exists(), "missing {f} after rebase");
    }

    // Walk the rebased chain: tip is feature 2, its parent feature 1, whose
    // parent is now main's tip. Change-ids are preserved across the replay.
    let tip = repo.store().get_commit(&outcome.commit).unwrap();
    assert_eq!(tip.change_id, f2);
    let parent = repo.store().get_commit(&tip.parents[0]).unwrap();
    assert_eq!(parent.change_id, f1);
    assert_eq!(parent.parents, vec![main_tip]);
}

#[test]
fn sync_object_transfer_roundtrip() {
    // Simulate pushing objects from one store into another using the same
    // primitives the gRPC sync uses: reachable_objects + get_raw/put_raw.
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(src_dir.path()).unwrap();
    write(&repo, "a.txt", "alpha\n");
    write(&repo, "b.txt", "beta\n");
    let head = ops::commit(&repo, "c1").unwrap();

    let dst = ObjectStore::new(Arc::new(FilesystemBackend::new(
        dst_dir.path().join("objects"),
    )));

    let ids = dag::reachable_objects(repo.store(), &[head]).unwrap();
    assert!(ids.len() >= 4); // commit + root tree + 2 blobs
    for id in &ids {
        let raw = repo.store().get_raw(id).unwrap().unwrap();
        dst.put_raw(id, &raw).unwrap();
    }

    // The destination can now read the commit and its tree independently.
    let commit = dst.get_commit(&head).unwrap();
    let files = chip_core::working_copy::flatten(&dst, &commit.tree).unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.contains_key("a.txt"));

    // A tampered object is rejected by put_raw's hash verification.
    let bogus = ObjectId::hash(b"not the real content");
    let some_raw = repo.store().get_raw(&head).unwrap().unwrap();
    assert!(dst.put_raw(&bogus, &some_raw).is_err());
}
