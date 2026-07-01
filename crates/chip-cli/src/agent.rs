//! Token-efficient, machine-readable output for agents.
//!
//! `chip log/show/diff/status` default to human output, but agents can pass
//! `--oneline`, `--stat`, `--name-status`, or `--format json` to read history and
//! diffs cheaply and without regex. JSON is compact (single line, no ANSI) and
//! diffs default to a summary — hunk *line content* is only included with
//! `--patch`.

use anyhow::Result;
use serde::Serialize;

use chip_core::diff::{self, Change, FileDiff, LineKind};
use chip_core::hash::ObjectId;
use chip_core::object::Commit;
use chip_core::repo::Repo;

fn is_false(b: &bool) -> bool {
    !*b
}

// --- log -------------------------------------------------------------------

#[derive(Serialize)]
struct LogEntry {
    change: String,
    commit: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    parents: Vec<String>,
    author: String,
    ts: i64,
    #[serde(skip_serializing_if = "is_false")]
    conflicted: bool,
    files: usize,
    added: usize,
    removed: usize,
    summary: String,
}

fn log_entries(repo: &Repo) -> Result<Vec<LogEntry>> {
    let store = repo.store();
    let head = match repo.refs().head_commit()? {
        Some(h) => h,
        None => return Ok(vec![]),
    };
    let mut out = Vec::new();
    for (id, commit) in chip_core::dag::history(store, head)? {
        let base_tree = commit
            .parents
            .first()
            .and_then(|p| store.get_commit(p).ok())
            .map(|c| c.tree);
        let stat = diff::diff_stat(store, base_tree.as_ref(), &commit.tree).unwrap_or_default();
        out.push(LogEntry {
            change: commit.change_id.to_string(),
            commit: id.short(),
            parents: commit.parents.iter().map(|p| p.short()).collect(),
            author: commit.author.clone(),
            ts: commit.timestamp,
            conflicted: commit.is_conflicted(),
            files: stat.files,
            added: stat.added,
            removed: stat.removed,
            summary: commit.message.lines().next().unwrap_or("").to_string(),
        });
    }
    Ok(out)
}

/// `chip log --format json`: a compact JSON array, newest change first.
pub fn log_json(repo: &Repo) -> Result<String> {
    Ok(serde_json::to_string(&log_entries(repo)?)?)
}

/// `chip log --oneline`: one dense line per change.
pub fn log_oneline(repo: &Repo) -> Result<String> {
    let current = repo.refs().head_commit()?;
    let store = repo.store();
    let head = match current {
        Some(h) => h,
        None => return Ok(String::new()),
    };
    let mut out = String::new();
    for (id, commit) in chip_core::dag::history(store, head)? {
        let base_tree = commit
            .parents
            .first()
            .and_then(|p| store.get_commit(p).ok())
            .map(|c| c.tree);
        let stat = diff::diff_stat(store, base_tree.as_ref(), &commit.tree).unwrap_or_default();
        let marker = if Some(id) == current { '@' } else { 'o' };
        let flag = if commit.is_conflicted() {
            " !conflict"
        } else {
            ""
        };
        out.push_str(&format!(
            "{marker} {} {}  {}f +{} -{}{flag}  {}\n",
            commit.change_id,
            id.short(),
            stat.files,
            stat.added,
            stat.removed,
            commit.message.lines().next().unwrap_or("")
        ));
    }
    Ok(out)
}

// --- diffs (show / diff) ---------------------------------------------------

#[derive(Serialize)]
struct LineEntry {
    op: char,
    #[serde(skip_serializing_if = "Option::is_none")]
    old: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new: Option<usize>,
    text: String,
}

#[derive(Serialize)]
struct HunkEntry {
    header: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    lines: Vec<LineEntry>,
}

#[derive(Serialize)]
struct FileEntry {
    status: char,
    path: String,
    added: usize,
    removed: usize,
    #[serde(skip_serializing_if = "is_false")]
    binary: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    hunks: Vec<HunkEntry>,
}

/// Commit metadata for `show --format json` (absent for a working-tree `diff`).
pub struct Meta<'a> {
    pub commit: &'a ObjectId,
    pub c: &'a Commit,
}

#[derive(Serialize)]
struct DiffDoc {
    #[serde(skip_serializing_if = "Option::is_none")]
    change: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    parents: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    conflicts: Vec<String>,
    added: usize,
    removed: usize,
    files: Vec<FileEntry>,
}

fn file_entries(diffs: &[FileDiff], patch: bool) -> Vec<FileEntry> {
    diffs
        .iter()
        .map(|d| FileEntry {
            status: d.status.letter(),
            path: d.path.clone(),
            added: d.added,
            removed: d.removed,
            binary: d.binary,
            hunks: if patch {
                d.hunks
                    .iter()
                    .map(|h| HunkEntry {
                        header: h.header.clone(),
                        lines: h
                            .lines
                            .iter()
                            .map(|l| LineEntry {
                                op: match l.kind {
                                    LineKind::Insert => '+',
                                    LineKind::Delete => '-',
                                    LineKind::Context => ' ',
                                },
                                old: l.old_no,
                                new: l.new_no,
                                text: l.content.clone(),
                            })
                            .collect(),
                    })
                    .collect()
            } else {
                Vec::new()
            },
        })
        .collect()
}

/// JSON for a set of file diffs. `meta` adds commit fields (for `show`); `patch`
/// includes hunk line content (otherwise just a per-file summary + hunk headers).
pub fn diff_json(diffs: &[FileDiff], meta: Option<Meta>, patch: bool) -> Result<String> {
    let added = diffs.iter().map(|d| d.added).sum();
    let removed = diffs.iter().map(|d| d.removed).sum();
    let doc = DiffDoc {
        change: meta.as_ref().map(|m| m.c.change_id.to_string()),
        commit: meta.as_ref().map(|m| m.commit.short()),
        parents: meta
            .as_ref()
            .map(|m| m.c.parents.iter().map(|p| p.short()).collect())
            .unwrap_or_default(),
        author: meta.as_ref().map(|m| m.c.author.clone()),
        ts: meta.as_ref().map(|m| m.c.timestamp),
        message: meta.as_ref().map(|m| m.c.message.clone()),
        conflicts: meta.map(|m| m.c.conflicts.clone()).unwrap_or_default(),
        added,
        removed,
        files: file_entries(diffs, patch),
    };
    Ok(serde_json::to_string(&doc)?)
}

/// `--stat`: per-file `+A -R  path` plus a totals line.
pub fn stat_text(diffs: &[FileDiff]) -> String {
    if diffs.is_empty() {
        return "0 files changed\n".to_string();
    }
    let mut out = String::new();
    let (mut ta, mut tr) = (0, 0);
    for d in diffs {
        ta += d.added;
        tr += d.removed;
        if d.binary {
            out.push_str(&format!("{}  bin  {}\n", d.status.letter(), d.path));
        } else {
            out.push_str(&format!(
                "{}  +{} -{}  {}\n",
                d.status.letter(),
                d.added,
                d.removed,
                d.path
            ));
        }
    }
    out.push_str(&format!("{} file(s), +{ta} -{tr}\n", diffs.len()));
    out
}

/// `--name-status`: one `A|M|D  path` line per file.
pub fn name_status_text(diffs: &[FileDiff]) -> String {
    diffs
        .iter()
        .map(|d| format!("{}  {}\n", d.status.letter(), d.path))
        .collect()
}

// --- status ----------------------------------------------------------------

#[derive(Serialize)]
struct StatusEntry {
    status: char,
    path: String,
}

/// `chip status --format json`: the working-tree changes as `{status, path}`.
pub fn status_json(changes: &[Change]) -> Result<String> {
    let entries: Vec<StatusEntry> = changes
        .iter()
        .map(|c| StatusEntry {
            status: match c {
                Change::Added(_) => 'A',
                Change::Modified(_) => 'M',
                Change::Deleted(_) => 'D',
            },
            path: c.path().to_string(),
        })
        .collect();
    Ok(serde_json::to_string(&entries)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chip_core::diff::{FileStatus, Hunk};

    fn sample() -> Vec<FileDiff> {
        vec![FileDiff {
            path: "src/main.rs".into(),
            status: FileStatus::Modified,
            added: 2,
            removed: 1,
            binary: false,
            hunks: vec![Hunk {
                header: "@@ -1,2 +1,3 @@".into(),
                lines: vec![diff::DiffLine {
                    kind: LineKind::Insert,
                    old_no: None,
                    new_no: Some(2),
                    content: "let x = 1;".into(),
                }],
            }],
        }]
    }

    #[test]
    fn summary_json_omits_line_content() {
        let json = diff_json(&sample(), None, false).unwrap();
        assert!(json.contains("\"path\":\"src/main.rs\""));
        assert!(json.contains("\"status\":\"M\""));
        assert!(json.contains("\"added\":2"));
        assert!(!json.contains("let x = 1;")); // no patch content in summary mode
        assert!(!json.contains("\"binary\"")); // false is skipped
    }

    #[test]
    fn patch_json_includes_lines() {
        let json = diff_json(&sample(), None, true).unwrap();
        assert!(json.contains("let x = 1;"));
        assert!(json.contains("\"op\":\"+\""));
        // Valid JSON.
        let _: serde_json::Value = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn stat_and_name_status_are_compact() {
        assert_eq!(name_status_text(&sample()), "M  src/main.rs\n");
        let s = stat_text(&sample());
        assert!(s.contains("M  +2 -1  src/main.rs"));
        assert!(s.contains("1 file(s), +2 -1"));
    }
}
