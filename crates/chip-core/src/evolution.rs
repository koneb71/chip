//! Change evolution: an append-only log of `(change_id, old_commit → new_commit)`
//! edges, recorded whenever a change is **rewritten** (amend / rebase / resolve).
//!
//! This is what lets `chip evolution` show a change's versions over time — the
//! change-id stays stable while the commit (content) hash moves, so the edges
//! reconstruct the sequence of commits a single change has been.

use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use crate::change::ChangeId;
use crate::error::Result;
use crate::hash::ObjectId;
use crate::repo::Repo;

/// One rewrite: `change_id` moved from commit `old` to commit `new`.
#[derive(Clone, Debug)]
pub struct Edge {
    pub change_id: ChangeId,
    pub old: ObjectId,
    pub new: ObjectId,
    pub timestamp: i64,
}

fn log_path(repo: &Repo) -> PathBuf {
    repo.chip_dir().join("evolution")
}

/// Append an evolution edge. Best-effort: callers ignore the error so a failed
/// write never breaks the underlying operation. Appends in O(1) rather than
/// rewriting the whole log, so a K-change rebase stays O(K) instead of O(K²).
pub fn record(repo: &Repo, change_id: &ChangeId, old: ObjectId, new: ObjectId) -> Result<()> {
    use std::io::Write;
    if old == new {
        return Ok(());
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(repo))?;
    writeln!(
        f,
        "{} {} {} {}",
        change_id,
        old.to_hex(),
        new.to_hex(),
        crate::now()
    )?;
    Ok(())
}

/// All recorded edges, oldest first.
pub fn read(repo: &Repo) -> Result<Vec<Edge>> {
    let content = match fs::read_to_string(log_path(repo)) {
        Ok(c) => c,
        Err(_) => return Ok(vec![]),
    };
    let mut out = Vec::new();
    for line in content.lines() {
        let parts: Vec<&str> = line.split(' ').collect();
        if parts.len() != 4 {
            continue;
        }
        let (Ok(old), Ok(new), Ok(ts)) = (
            ObjectId::from_str(parts[1]),
            ObjectId::from_str(parts[2]),
            parts[3].parse::<i64>(),
        ) else {
            continue;
        };
        out.push(Edge {
            change_id: ChangeId::from_string(parts[0].to_string()),
            old,
            new,
            timestamp: ts,
        });
    }
    Ok(out)
}

/// The chain of commit versions a `change_id` has had, oldest first, each with
/// the timestamp it was superseded (the last is the current version).
pub fn versions_for(edges: &[Edge], change_id: &ChangeId) -> Vec<(ObjectId, i64)> {
    let mut relevant: Vec<&Edge> = edges.iter().filter(|e| &e.change_id == change_id).collect();
    relevant.sort_by_key(|e| e.timestamp);
    let mut chain: Vec<(ObjectId, i64)> = Vec::new();
    for e in relevant {
        if chain.is_empty() {
            chain.push((e.old, e.timestamp));
        }
        chain.push((e.new, e.timestamp));
    }
    chain
}
