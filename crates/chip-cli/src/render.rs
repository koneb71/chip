//! Terminal rendering for changed-file lists and diffs.
//!
//! Colour is emitted as raw ANSI and automatically disabled when stdout is not a
//! TTY, when `NO_COLOR` is set, or when `TERM=dumb` — so piped output stays clean.

use std::io::IsTerminal;

use chip_core::diff::{Change, FileDiff, FileStatus, LineKind};

/// A colour gate computed once per command.
pub struct Painter {
    color: bool,
}

impl Default for Painter {
    fn default() -> Self {
        Painter::new()
    }
}

impl Painter {
    pub fn new() -> Self {
        Painter { color: use_color() }
    }

    fn wrap(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn green(&self, s: &str) -> String {
        self.wrap("32", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.wrap("31", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.wrap("33", s)
    }
    pub fn cyan(&self, s: &str) -> String {
        self.wrap("36", s)
    }
    pub fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    pub fn green_bold(&self, s: &str) -> String {
        self.wrap("1;32", s)
    }
}

fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
}

/// Render a changed-file list (for `chip status`) with coloured badges and a
/// one-line summary.
pub fn render_changes(changes: &[Change]) -> String {
    let p = Painter::new();
    if changes.is_empty() {
        return "working tree clean".to_string();
    }
    let (mut a, mut m, mut d) = (0, 0, 0);
    let mut out = String::new();
    for ch in changes {
        let (badge, path) = match ch {
            Change::Added(path) => {
                a += 1;
                (p.green("A"), path)
            }
            Change::Modified(path) => {
                m += 1;
                (p.yellow("M"), path)
            }
            Change::Deleted(path) => {
                d += 1;
                (p.red("D"), path)
            }
        };
        out.push_str(&format!("  {badge}  {path}\n"));
    }
    out.push_str(&p.dim(&format!(
        "{} file(s) changed ({a} added, {m} modified, {d} deleted)",
        changes.len()
    )));
    out
}

/// Render structured per-file diffs (for `chip diff` / `chip show`) with a
/// summary line, coloured hunk headers, and +/- lines.
pub fn render_file_diffs(diffs: &[FileDiff]) -> String {
    let p = Painter::new();
    if diffs.is_empty() {
        return "no changes".to_string();
    }
    let total_add: usize = diffs.iter().map(|d| d.added).sum();
    let total_del: usize = diffs.iter().map(|d| d.removed).sum();

    let mut out = String::new();
    out.push_str(&format!(
        "{}, {} {}\n\n",
        p.bold(&format!("{} file(s) changed", diffs.len())),
        p.green(&format!("+{total_add}")),
        p.red(&format!("-{total_del}")),
    ));

    for d in diffs {
        let badge = match d.status {
            FileStatus::Added => p.green("A"),
            FileStatus::Modified => p.yellow("M"),
            FileStatus::Deleted => p.red("D"),
        };
        if d.binary {
            out.push_str(&format!("{} {}\n", badge, p.bold(&d.path)));
            out.push_str(&format!("{}\n\n", p.dim("  Binary file changed")));
            continue;
        }
        out.push_str(&format!(
            "{} {}  {} {}\n",
            badge,
            p.bold(&d.path),
            p.green(&format!("+{}", d.added)),
            p.red(&format!("-{}", d.removed)),
        ));
        for hunk in &d.hunks {
            out.push_str(&p.cyan(&hunk.header));
            out.push('\n');
            for line in &hunk.lines {
                let rendered = match line.kind {
                    LineKind::Insert => p.green(&format!("+{}", line.content)),
                    LineKind::Delete => p.red(&format!("-{}", line.content)),
                    LineKind::Context => format!(" {}", line.content),
                };
                out.push_str(&rendered);
                out.push('\n');
            }
        }
        out.push('\n');
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}
