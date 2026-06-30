use std::env;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use time::OffsetDateTime;

use chip_core::dag;
use chip_core::diff;
use chip_core::oplog::OpLog;
use chip_core::ops;
use chip_core::refs::Head;
use chip_core::repo::Repo;
use chip_core::working_copy;

mod remote;
mod render;
mod ssh;
mod sync;

#[derive(Parser)]
#[command(
    name = "chip",
    version,
    about = "A changeset-oriented version control system"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new chip repository in the current directory
    Init,
    /// Snapshot the working tree as a new commit (no staging step)
    Commit {
        #[arg(short, long)]
        message: String,
    },
    /// Show the history reachable from HEAD
    Log,
    /// Show what changed in the working tree since the last commit
    Status,
    /// Show a unified diff of working-tree changes
    Diff,
    /// Create, move, list, or delete bookmarks (named branches)
    Bookmark {
        /// Bookmark name; omit to list all bookmarks
        name: Option<String>,
        /// Delete the named bookmark instead of creating it
        #[arg(short, long)]
        delete: bool,
    },
    /// Switch HEAD to a bookmark or commit and update the working tree
    Checkout {
        /// Bookmark/commit to switch to, or new bookmark name with --create
        target: String,
        /// Create a new bookmark at the current commit and switch to it
        #[arg(short = 'b', long = "create")]
        create: bool,
    },
    /// Create a tag pointing at the current commit
    Tag { name: String },
    /// Merge another bookmark/commit into the current one
    Merge { target: String },
    /// Replace the current change in place, keeping its change-id
    Amend {
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Re-snapshot and clear resolved conflicts, keeping the change-id
    Resolve,
    /// Rebase the current branch onto another bookmark/commit
    Rebase { target: String },
    /// Copy the change a commit introduced onto the current change
    CherryPick { target: String },
    /// Create a new commit that undoes a previous commit
    Revert { target: String },
    /// Discard uncommitted changes (whole tree, or a single file)
    Restore {
        /// File to restore; omit to reset the entire working tree
        path: Option<String>,
    },
    /// Show a change's metadata and diff
    Show {
        /// Revision to show (default: @)
        rev: Option<String>,
    },
    /// Reverse the most recent operation
    Undo,
    /// Inspect the operation log
    Op {
        #[command(subcommand)]
        command: OpCommand,
    },
    /// Create an account on a chip server and store its token
    Register {
        /// Server endpoint, e.g. http://localhost:8080
        url: String,
        #[arg(short, long)]
        username: String,
        #[arg(short, long)]
        email: String,
        /// Password (omit to be prompted securely)
        #[arg(short, long)]
        password: Option<String>,
    },
    /// Log in to a chip server and store its token
    Login {
        /// Server endpoint, e.g. http://localhost:8080
        url: String,
        #[arg(short, long)]
        username: String,
        /// Password (omit to be prompted securely)
        #[arg(short, long)]
        password: Option<String>,
    },
    /// Manage server-side repositories
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Manage remotes for this repository
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Clone a repository from a chip server URL
    Clone {
        /// Repository URL, e.g. http://localhost:8080/owner/repo
        url: String,
        /// Target directory (defaults to the repo name)
        dir: Option<String>,
    },
    /// Upload commits and a bookmark to a remote
    Push {
        /// Remote name (default: origin)
        remote: Option<String>,
        /// Bookmark to push (default: current)
        #[arg(short, long)]
        bookmark: Option<String>,
        /// Allow a non-fast-forward update (overwrite server history)
        #[arg(short, long)]
        force: bool,
    },
    /// Download commits from a remote and integrate local bookmarks
    Pull {
        /// Remote name (default: origin)
        remote: Option<String>,
        /// On divergence, rebase local changes onto the remote
        #[arg(long, conflicts_with = "merge")]
        rebase: bool,
        /// On divergence, create a merge commit
        #[arg(long)]
        merge: bool,
    },
}

#[derive(Subcommand)]
enum RepoCommand {
    /// Create a repository on a chip server, e.g. http://host:8080/alice/proj
    Create {
        /// Repository URL: <server>/<owner>/<name>
        url: String,
        /// Make the repository public (default: private)
        #[arg(long)]
        public: bool,
        /// Optional one-line description
        #[arg(long)]
        description: Option<String>,
    },
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Add a named remote
    Add { name: String, url: String },
    /// List configured remotes
    List,
}

#[derive(Subcommand)]
enum OpCommand {
    /// List recorded operations, newest first
    Log,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => cmd_init(),
        Command::Commit { message } => with_oplog("commit", |repo| {
            let id = ops::commit(repo, &message)?;
            println!("committed {}", id.short());
            Ok(())
        }),
        Command::Log => cmd_log(),
        Command::Status => cmd_status(),
        Command::Diff => cmd_diff(),
        Command::Bookmark { name, delete } => cmd_bookmark(name, delete),
        Command::Checkout { target, create } => with_oplog("checkout", |repo| {
            if create {
                let head = repo
                    .refs()
                    .head_commit()?
                    .context("no commit yet to branch from")?;
                repo.refs().set_bookmark(&target, head)?;
            }
            let id = ops::checkout(repo, &target)?;
            println!("now at {} ({target})", id.short());
            Ok(())
        }),
        Command::Tag { name } => with_oplog("tag", |repo| {
            let id = repo.refs().head_commit()?.context("no commit to tag")?;
            repo.refs().set_tag(&name, id)?;
            println!("tagged {} as {name}", id.short());
            Ok(())
        }),
        Command::Merge { target } => cmd_merge(target),
        Command::Amend { message } => with_oplog("amend", |repo| {
            let id = ops::amend(repo, message.as_deref())?;
            println!("amended change is now {}", id.short());
            Ok(())
        }),
        Command::Resolve => with_oplog("resolve", |repo| {
            let outcome = ops::resolve(repo)?;
            if outcome.remaining.is_empty() {
                println!("all conflicts resolved ({})", outcome.commit.short());
            } else {
                println!("still conflicted: {}", outcome.remaining.join(", "));
            }
            Ok(())
        }),
        Command::Rebase { target } => cmd_rebase(target),
        Command::CherryPick { target } => cmd_cherry_pick(target),
        Command::Revert { target } => cmd_revert(target),
        Command::Restore { path } => with_oplog("restore", |repo| {
            let n = ops::restore(repo, path.as_deref())?;
            match &path {
                Some(p) if n == 1 => println!("restored {p} from the last commit"),
                Some(p) => println!("restored {p}"),
                None => println!("working tree reset to the last commit"),
            }
            Ok(())
        }),
        Command::Show { rev } => cmd_show(rev),
        Command::Undo => cmd_undo(),
        Command::Op { command } => match command {
            OpCommand::Log => cmd_op_log(),
        },
        Command::Register {
            url,
            username,
            email,
            password,
        } => {
            let password = resolve_password(password)?;
            block_on(async move {
                let name = sync::register(&url, &username, &email, &password).await?;
                println!("registered and logged in as {name}");
                Ok(())
            })
        }
        Command::Login {
            url,
            username,
            password,
        } => {
            let password = resolve_password(password)?;
            block_on(async move {
                let name = sync::login(&url, &username, &password).await?;
                println!("logged in as {name}");
                Ok(())
            })
        }
        Command::Repo { command } => match command {
            RepoCommand::Create {
                url,
                public,
                description,
            } => block_on(
                async move { sync::create_repo(&url, public, description.as_deref()).await },
            ),
        },
        Command::Remote { command } => cmd_remote(command),
        Command::Clone { url, dir } => block_on(async move {
            let parsed = remote::RemoteUrl::parse(&url)?;
            let target = dir.unwrap_or_else(|| parsed.repo.clone());
            sync::clone(&url, std::path::Path::new(&target)).await
        }),
        Command::Push {
            remote,
            bookmark,
            force,
        } => {
            let repo = open()?;
            block_on(async move {
                sync::push(
                    &repo,
                    remote.as_deref().unwrap_or("origin"),
                    bookmark,
                    force,
                )
                .await
            })
        }
        Command::Pull {
            remote,
            rebase,
            merge,
        } => {
            let repo = open()?;
            let strategy = if rebase {
                sync::PullStrategy::Rebase
            } else if merge {
                sync::PullStrategy::Merge
            } else {
                sync::PullStrategy::FfOnly
            };
            block_on(async move {
                sync::pull(&repo, remote.as_deref().unwrap_or("origin"), strategy).await
            })
        }
    }
}

/// Run an async future to completion on a fresh Tokio runtime.
fn block_on<F: std::future::Future<Output = Result<()>>>(f: F) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(f)
}

fn cmd_remote(command: RemoteCommand) -> Result<()> {
    let repo = open()?;
    match command {
        RemoteCommand::Add { name, url } => {
            remote::add_remote(&repo, &name, &url)?;
            println!("added remote {name} -> {url}");
        }
        RemoteCommand::List => {
            for (name, url) in remote::load_remotes(&repo)? {
                println!("{name}\t{url}");
            }
        }
    }
    Ok(())
}

fn open() -> Result<Repo> {
    Repo::discover(env::current_dir()?).context("could not find a chip repository")
}

/// Run a mutating closure, recording an undo point in the op log.
fn with_oplog<F>(description: &str, f: F) -> Result<()>
where
    F: FnOnce(&Repo) -> Result<()>,
{
    let repo = open()?;
    let before = OpLog::capture(&repo)?;
    f(&repo)?;
    OpLog::new(&repo).append(&repo, description, before)?;
    Ok(())
}

fn cmd_init() -> Result<()> {
    let cwd = env::current_dir()?;
    Repo::init(&cwd)?;
    println!(
        "initialized empty chip repository in {}/.chip",
        cwd.display()
    );
    Ok(())
}

fn cmd_log() -> Result<()> {
    let repo = open()?;
    let head = match repo.refs().head_commit()? {
        Some(h) => h,
        None => {
            println!("(no commits yet)");
            return Ok(());
        }
    };
    let current = repo.refs().head_commit()?;
    let p = render::Painter::new();
    for (id, commit) in dag::history(repo.store(), head)? {
        let marker = if Some(id) == current {
            p.green_bold("@")
        } else {
            p.dim("o")
        };
        let conflict = if commit.is_conflicted() {
            p.red(" (conflict)")
        } else {
            String::new()
        };
        println!(
            "{marker} change {}  commit {}{conflict}",
            p.yellow(&commit.change_id.to_string()),
            p.dim(&id.short())
        );
        println!("  {}  {}", commit.author, format_time(commit.timestamp));
        println!("  {}", commit.message.lines().next().unwrap_or(""));
        // Per-change stat: diff against the first parent.
        let base_tree = commit
            .parents
            .first()
            .and_then(|parent| repo.store().get_commit(parent).ok())
            .map(|c| c.tree);
        if let Ok(stat) = diff::diff_stat(repo.store(), base_tree.as_ref(), &commit.tree) {
            if stat.files > 0 {
                println!(
                    "  {}",
                    p.dim(&format!(
                        "{} file(s), +{} -{}",
                        stat.files, stat.added, stat.removed
                    ))
                );
            }
        }
        println!();
    }
    Ok(())
}

fn cmd_status() -> Result<()> {
    let repo = open()?;
    let tree = working_copy::snapshot(&repo)?;
    let base = repo
        .refs()
        .head_commit()?
        .map(|c| repo.store().get_commit(&c))
        .transpose()?
        .map(|c| c.tree);
    let changes = diff::status(repo.store(), base.as_ref(), &tree)?;

    match repo.refs().read_head()? {
        Head::Bookmark(name) => println!("on bookmark {name}"),
        Head::Detached(id) => println!("detached at {}", id.short()),
        Head::Unborn => println!("no commits yet"),
    }
    println!("{}", render::render_changes(&changes));
    Ok(())
}

fn cmd_diff() -> Result<()> {
    let repo = open()?;
    let tree = working_copy::snapshot(&repo)?;
    let base = repo
        .refs()
        .head_commit()?
        .map(|c| repo.store().get_commit(&c))
        .transpose()?
        .map(|c| c.tree);
    let diffs = diff::file_diffs(repo.store(), base.as_ref(), &tree)?;
    println!("{}", render::render_file_diffs(&diffs));
    Ok(())
}

fn cmd_bookmark(name: Option<String>, delete: bool) -> Result<()> {
    let repo = open()?;
    match name {
        None => {
            let head = repo.refs().read_head()?;
            for (n, id) in repo.refs().list_bookmarks()? {
                let marker = matches!(&head, Head::Bookmark(b) if *b == n);
                println!("{} {n}  {}", if marker { "*" } else { " " }, id.short());
            }
            Ok(())
        }
        Some(name) if delete => {
            repo.refs().delete_bookmark(&name)?;
            println!("deleted bookmark {name}");
            Ok(())
        }
        Some(name) => {
            let before = OpLog::capture(&repo)?;
            let id = repo
                .refs()
                .head_commit()?
                .context("no commit to bookmark")?;
            repo.refs().set_bookmark(&name, id)?;
            OpLog::new(&repo).append(&repo, "bookmark", before)?;
            println!("bookmark {name} -> {}", id.short());
            Ok(())
        }
    }
}

fn cmd_merge(target: String) -> Result<()> {
    let repo = open()?;
    let before = OpLog::capture(&repo)?;
    let outcome = ops::merge(&repo, &target)?;
    OpLog::new(&repo).append(&repo, &format!("merge {target}"), before)?;

    if outcome.already_up_to_date {
        println!("already up to date");
    } else if outcome.fast_forward {
        println!("fast-forwarded to {}", outcome.commit.short());
    } else if outcome.conflicts.is_empty() {
        println!("merged cleanly into {}", outcome.commit.short());
    } else {
        println!(
            "merged into {} with {} conflicted file(s):",
            outcome.commit.short(),
            outcome.conflicts.len()
        );
        for path in &outcome.conflicts {
            println!("  {path}");
        }
        println!("resolve the markers, then `chip commit` to record the resolution");
    }
    Ok(())
}

fn cmd_rebase(target: String) -> Result<()> {
    let repo = open()?;
    let before = OpLog::capture(&repo)?;
    let outcome = ops::rebase(&repo, &target)?;
    OpLog::new(&repo).append(&repo, &format!("rebase {target}"), before)?;
    if outcome.already_up_to_date {
        println!("already based on {target}");
    } else if outcome.conflicts.is_empty() {
        println!("rebased onto {target} ({})", outcome.commit.short());
    } else {
        println!(
            "rebased onto {target} with {} conflict(s); resolve then `chip resolve`",
            outcome.conflicts.len()
        );
        for path in &outcome.conflicts {
            println!("  {path}");
        }
    }
    Ok(())
}

fn cmd_cherry_pick(target: String) -> Result<()> {
    let repo = open()?;
    let before = OpLog::capture(&repo)?;
    let outcome = ops::cherry_pick(&repo, &target)?;
    OpLog::new(&repo).append(&repo, &format!("cherry-pick {target}"), before)?;
    if outcome.conflicts.is_empty() {
        println!("cherry-picked {target} ({})", outcome.commit.short());
    } else {
        println!(
            "cherry-picked {target} with {} conflict(s); resolve then `chip resolve`",
            outcome.conflicts.len()
        );
        for path in &outcome.conflicts {
            println!("  {path}");
        }
    }
    Ok(())
}

fn cmd_revert(target: String) -> Result<()> {
    let repo = open()?;
    let before = OpLog::capture(&repo)?;
    let outcome = ops::revert(&repo, &target)?;
    OpLog::new(&repo).append(&repo, &format!("revert {target}"), before)?;
    if outcome.conflicts.is_empty() {
        println!("reverted {target} ({})", outcome.commit.short());
    } else {
        println!(
            "reverted {target} with {} conflict(s); resolve then `chip resolve`",
            outcome.conflicts.len()
        );
        for path in &outcome.conflicts {
            println!("  {path}");
        }
    }
    Ok(())
}

fn cmd_show(rev: Option<String>) -> Result<()> {
    let repo = open()?;
    let rev = rev.unwrap_or_else(|| "@".to_string());
    let id = ops::resolve_commit(&repo, &rev)?;
    let commit = repo.store().get_commit(&id)?;
    let p = render::Painter::new();
    println!(
        "{} {}  commit {}",
        p.bold("change"),
        p.yellow(&commit.change_id.to_string()),
        p.dim(&id.short())
    );
    println!("author {}", commit.author);
    println!("date   {}", format_time(commit.timestamp));
    if commit.is_conflicted() {
        println!(
            "{}",
            p.red(&format!("conflicts: {}", commit.conflicts.join(", ")))
        );
    }
    println!("\n    {}\n", commit.message);
    let base_tree = commit
        .parents
        .first()
        .map(|p| repo.store().get_commit(p))
        .transpose()?
        .map(|c| c.tree);
    let diffs = diff::file_diffs(repo.store(), base_tree.as_ref(), &commit.tree)?;
    println!("{}", render::render_file_diffs(&diffs));
    Ok(())
}

/// Resolve a password from the flag, the `CHIP_PASSWORD` env var, or a secure
/// prompt — so it never has to appear in argv / shell history.
fn resolve_password(flag: Option<String>) -> Result<String> {
    if let Some(p) = flag {
        return Ok(p);
    }
    if let Ok(p) = std::env::var("CHIP_PASSWORD") {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    let p = rpassword::prompt_password("Password: ").context("failed to read password")?;
    Ok(p)
}

fn cmd_undo() -> Result<()> {
    let repo = open()?;
    let op = OpLog::new(&repo).undo(&repo)?;
    println!("undid operation: {}", op.description);
    Ok(())
}

fn cmd_op_log() -> Result<()> {
    let repo = open()?;
    let mut ops = OpLog::new(&repo).list()?;
    ops.reverse();
    if ops.is_empty() {
        println!("(no operations recorded)");
    }
    for op in ops {
        println!(
            "#{:<4} {}  {}",
            op.seq,
            format_time(op.timestamp),
            op.description
        );
    }
    Ok(())
}

fn format_time(ts: i64) -> String {
    OffsetDateTime::from_unix_timestamp(ts)
        .map(|t| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                t.year(),
                t.month() as u8,
                t.day(),
                t.hour(),
                t.minute(),
                t.second()
            )
        })
        .unwrap_or_else(|_| ts.to_string())
}
