//! Import a local Git repository into a new chip repository.
//!
//! Git objects map cleanly onto chip's: a Git blob's bytes become a chip blob, a
//! Git tree becomes a chip tree, and a Git commit becomes a chip commit that
//! preserves the author, message, timestamp, and parent topology. Each imported
//! commit gets a fresh chip change-id. Content is re-hashed with BLAKE3, so the
//! ids differ from Git's SHA-1 but the history is faithful.
//!
//! Local repositories only (clone a remote with `git clone` first). Symlinks and
//! submodules are skipped (and counted in the summary).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};

use chip_core::change::ChangeId;
use chip_core::hash::ObjectId;
use chip_core::object::{Blob, Commit, EntryKind, Tree, TreeEntry};
use chip_core::refs::Head;
use chip_core::repo::{Repo, DEFAULT_BOOKMARK};
use chip_core::store::ObjectStore;
use chip_core::working_copy;

type GitId = gix::ObjectId;

struct Importer<'a> {
    git: &'a gix::Repository,
    store: &'a ObjectStore,
    commits: HashMap<GitId, ObjectId>,
    objects: HashMap<GitId, ObjectId>,
    count: usize,
    skipped: usize,
}

impl Importer<'_> {
    fn map_blob(&mut self, oid: GitId) -> Result<ObjectId> {
        if let Some(id) = self.objects.get(&oid) {
            return Ok(*id);
        }
        let data = self.git.find_object(oid)?.data.clone();
        let id = self.store.put_blob(Blob { data })?;
        self.objects.insert(oid, id);
        Ok(id)
    }

    fn map_tree(&mut self, oid: GitId) -> Result<ObjectId> {
        if let Some(id) = self.objects.get(&oid) {
            return Ok(*id);
        }
        let obj = self.git.find_object(oid)?.into_tree();
        let tree_ref = obj.decode()?;
        let mut entries = Vec::new();
        for e in tree_ref.entries.iter() {
            let name = e.filename.to_string();
            let child = e.oid.to_owned();
            use gix::objs::tree::EntryKind as K;
            match e.mode.kind() {
                K::Tree => {
                    let id = self.map_tree(child)?;
                    entries.push(TreeEntry {
                        name,
                        kind: EntryKind::Tree,
                        mode: 0o040000,
                        id,
                    });
                }
                K::Blob => {
                    let id = self.map_blob(child)?;
                    entries.push(TreeEntry {
                        name,
                        kind: EntryKind::Blob,
                        mode: 0o644,
                        id,
                    });
                }
                K::BlobExecutable => {
                    let id = self.map_blob(child)?;
                    entries.push(TreeEntry {
                        name,
                        kind: EntryKind::Blob,
                        mode: 0o755,
                        id,
                    });
                }
                // Symlinks and submodules: chip has no equivalent, so skip and
                // count them so the omission is visible in the summary.
                _ => self.skipped += 1,
            }
        }
        let id = self.store.put_tree(Tree::new(entries))?;
        self.objects.insert(oid, id);
        Ok(id)
    }

    /// Map `root` and all of its ancestors to chip commits. Iterative (explicit
    /// work stack) so deep histories don't overflow the call stack.
    fn map_commit(&mut self, root: GitId) -> Result<ObjectId> {
        let mut stack = vec![root];
        while let Some(&oid) = stack.last() {
            if self.commits.contains_key(&oid) {
                stack.pop();
                continue;
            }
            let obj = self.git.find_object(oid)?.into_commit();
            let commit_ref = obj.decode()?;

            // Push any unmapped parents; revisit this commit once they're done.
            let mut pending = Vec::new();
            for p in commit_ref.parents() {
                if !self.commits.contains_key(&p) {
                    pending.push(p);
                }
            }
            if !pending.is_empty() {
                stack.extend(pending);
                continue;
            }

            let tree = self.map_tree(commit_ref.tree())?;
            let parents: Vec<ObjectId> = commit_ref.parents().map(|p| self.commits[&p]).collect();
            let author = format!("{} <{}>", commit_ref.author.name, commit_ref.author.email);
            let timestamp = obj.time()?.seconds;
            let message = commit_ref.message.to_string();
            let id = self.store.put_commit(Commit {
                tree,
                parents,
                change_id: ChangeId::generate(),
                author,
                timestamp,
                message,
                conflicts: vec![],
            })?;
            self.commits.insert(oid, id);
            self.count += 1;
            stack.pop();
        }
        Ok(self.commits[&root])
    }
}

fn default_dir(src: &str) -> String {
    Path::new(src.trim_end_matches('/'))
        .file_name()
        .map(|s| s.to_string_lossy().trim_end_matches(".git").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "imported".to_string())
}

/// Import the local Git repository at `src` into a new chip repo at `dir`
/// (default: the repo's directory name).
pub fn import_git(src: &str, dir: Option<String>) -> Result<()> {
    let git =
        gix::open(src).with_context(|| format!("could not open a Git repository at '{src}'"))?;
    let target = dir.unwrap_or_else(|| default_dir(src));
    if Path::new(&target).join(".chip").exists() {
        bail!("'{target}' already contains a chip repository");
    }
    std::fs::create_dir_all(&target)?;
    let repo = Repo::init(&target)?;

    let head_short = git
        .head_name()
        .ok()
        .flatten()
        .map(|n| n.shorten().to_string());

    let mut imp = Importer {
        git: &git,
        store: repo.store(),
        commits: HashMap::new(),
        objects: HashMap::new(),
        count: 0,
        skipped: 0,
    };

    // Map branches (→ bookmarks, keeping their real Git names) and tags.
    let mut branch_tips: Vec<(String, ObjectId)> = Vec::new();
    let mut tags = 0usize;
    for r in git.references()?.all()? {
        let mut r = match r {
            Ok(r) => r,
            Err(_) => continue,
        };
        let git_oid = match r.peel_to_id_in_place() {
            Ok(peeled) => peeled.detach(),
            Err(_) => continue,
        };
        let category = r.name().category();
        let short = r.name().shorten().to_string();
        match category {
            Some(gix::reference::Category::LocalBranch) => {
                let chip_id = imp.map_commit(git_oid)?;
                repo.refs().set_bookmark(&short, chip_id)?;
                branch_tips.push((short, chip_id));
            }
            Some(gix::reference::Category::Tag) => {
                if let Ok(chip_id) = imp.map_commit(git_oid) {
                    let _ = repo.refs().set_tag(&short, chip_id);
                    tags += 1;
                }
            }
            _ => {}
        }
    }

    // Choose the default branch deterministically: the Git HEAD branch, else a
    // branch named `main`, else `master`, else the lexicographically first.
    branch_tips.sort_by(|a, b| a.0.cmp(&b.0));
    let default = head_short
        .as_deref()
        .and_then(|h| branch_tips.iter().find(|(n, _)| n == h))
        .or_else(|| branch_tips.iter().find(|(n, _)| n == DEFAULT_BOOKMARK))
        .or_else(|| branch_tips.iter().find(|(n, _)| n == "master"))
        .or_else(|| branch_tips.first())
        .cloned();

    if let Some((name, tip)) = default {
        repo.refs().write_head(&Head::Bookmark(name))?;
        let commit = repo.store().get_commit(&tip)?;
        working_copy::restore(&repo, &commit.tree)?;
    }

    let bookmarks = branch_tips.len();
    let skip_note = if imp.skipped > 0 {
        format!(
            " ({} symlink/submodule entr{} skipped)",
            imp.skipped,
            if imp.skipped == 1 { "y" } else { "ies" }
        )
    } else {
        String::new()
    };
    println!(
        "imported {} commit(s), {bookmarks} bookmark(s), {tags} tag(s) into {target}{skip_note}",
        imp.count
    );
    println!("  cd {target} && chip log");
    Ok(())
}
