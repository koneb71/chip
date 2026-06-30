//! gRPC client logic for the remote commands: login/register and clone/push/pull.

use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use chip_core::dag;
use chip_core::hash::ObjectId;
use chip_core::refs::Head;
use chip_core::repo::Repo;
use chip_core::working_copy;
use tonic::transport::Channel;
use tonic::Request;

use chip_proto::chip_sync_client::ChipSyncClient;
use chip_proto::{
    push_request, CreateRepoRequest, FetchRequest, ListRefsRequest, ListRefsResponse, LoginRequest,
    ObjectChunk, PushHeader, PushRequest, RefUpdate, RefUpdates, RegisterRequest,
};

use crate::remote::{self, RemoteUrl, Transport};

/// Build a sync client for a remote, choosing the HTTP or SSH transport.
async fn client_for(remote: &RemoteUrl) -> Result<ChipSyncClient<Channel>> {
    match &remote.transport {
        Transport::Http => connect(&remote.endpoint).await,
        Transport::Ssh { host, port, login } => {
            let channel = crate::ssh::channel(host, *port, login).await?;
            Ok(ChipSyncClient::new(channel))
        }
    }
}

/// The bearer token to use, if any. SSH carries identity via the key, so no
/// token is needed there.
fn token_for_remote(remote: &RemoteUrl) -> Result<Option<String>> {
    match remote.transport {
        Transport::Http => remote::token_for(&remote.endpoint),
        Transport::Ssh { .. } => Ok(None),
    }
}

async fn connect(endpoint: &str) -> Result<ChipSyncClient<Channel>> {
    let mut ep = Channel::from_shared(endpoint.to_string())
        .with_context(|| format!("invalid endpoint {endpoint}"))?;
    if endpoint.starts_with("https://") {
        // Use the system's trusted CA roots for TLS verification.
        ep = ep
            .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
            .context("failed to configure TLS")?;
    }
    let channel = ep
        .connect()
        .await
        .with_context(|| format!("could not connect to {endpoint}"))?;
    Ok(ChipSyncClient::new(channel))
}

fn authed<T>(token: Option<&str>, msg: T) -> Request<T> {
    let mut req = Request::new(msg);
    if let Some(t) = token {
        req.metadata_mut()
            .insert("authorization", format!("Bearer {t}").parse().unwrap());
    }
    req
}

pub async fn register(
    endpoint: &str,
    username: &str,
    email: &str,
    password: &str,
) -> Result<String> {
    let mut client = connect(endpoint).await?;
    let resp = client
        .register(Request::new(RegisterRequest {
            username: username.to_string(),
            email: email.to_string(),
            password: password.to_string(),
        }))
        .await
        .map_err(status)?
        .into_inner();
    remote::save_token(endpoint, &resp.token)?;
    Ok(resp.username)
}

pub async fn login(endpoint: &str, username: &str, password: &str) -> Result<String> {
    let mut client = connect(endpoint).await?;
    let resp = client
        .login(Request::new(LoginRequest {
            username: username.to_string(),
            password: password.to_string(),
        }))
        .await
        .map_err(status)?
        .into_inner();
    remote::save_token(endpoint, &resp.token)?;
    Ok(resp.username)
}

pub async fn create_repo(url: &str, public: bool, description: Option<&str>) -> Result<()> {
    let remote = RemoteUrl::parse(url)?;
    let token = token_for_remote(&remote)?;
    let mut client = client_for(&remote).await?;
    let resp = client
        .create_repo(authed(
            token.as_deref(),
            CreateRepoRequest {
                owner: remote.owner.clone(),
                repo: remote.repo.clone(),
                public,
                description: description.unwrap_or("").to_string(),
            },
        ))
        .await
        .map_err(status)?
        .into_inner();
    println!("{}", resp.message);
    println!("  push to it with:  chip remote add origin {url}  &&  chip push origin");
    Ok(())
}

pub async fn clone(url: &str, dir: &Path) -> Result<()> {
    let remote = RemoteUrl::parse(url)?;
    let token = token_for_remote(&remote)?;
    let mut client = client_for(&remote).await?;

    let refs = client
        .list_refs(authed(
            token.as_deref(),
            ListRefsRequest {
                owner: remote.owner.clone(),
                repo: remote.repo.clone(),
            },
        ))
        .await
        .map_err(status)?
        .into_inner();

    let repo = Repo::init(dir)?;

    let mut wants: HashSet<String> = HashSet::new();
    for r in refs.bookmarks.iter().chain(refs.tags.iter()) {
        wants.insert(r.target.clone());
    }

    download(
        &mut client,
        &remote,
        token.as_deref(),
        &repo,
        wants.into_iter().collect(),
        vec![],
    )
    .await?;

    // Recreate refs locally.
    for b in &refs.bookmarks {
        repo.refs()
            .set_bookmark(&b.name, ObjectId::from_str(&b.target)?)?;
    }
    for t in &refs.tags {
        repo.refs()
            .set_tag(&t.name, ObjectId::from_str(&t.target)?)?;
    }

    // Pick a default bookmark to check out.
    let default = refs
        .bookmarks
        .iter()
        .find(|b| b.name == "main")
        .or_else(|| refs.bookmarks.first());
    if let Some(b) = default {
        repo.refs().write_head(&Head::Bookmark(b.name.clone()))?;
        let commit = repo.store().get_commit(&ObjectId::from_str(&b.target)?)?;
        working_copy::restore(&repo, &commit.tree)?;
    }

    remote::add_remote(&repo, "origin", url)?;
    println!(
        "cloned {}/{} ({} bookmark(s)) into {}",
        remote.owner,
        remote.repo,
        refs.bookmarks.len(),
        dir.display()
    );
    Ok(())
}

pub async fn push(
    repo: &Repo,
    remote_name: &str,
    bookmark: Option<String>,
    force: bool,
) -> Result<()> {
    let url = remote::get_remote(repo, remote_name)?;
    let remote = RemoteUrl::parse(&url)?;
    let token = token_for_remote(&remote)?;
    if matches!(remote.transport, Transport::Http) && token.is_none() {
        bail!("not logged in to this server; run `chip login` first");
    }
    let mut client = client_for(&remote).await?;

    // Which bookmark to push.
    let name = match bookmark {
        Some(b) => b,
        None => match repo.refs().read_head()? {
            Head::Bookmark(n) => n,
            _ => bail!("HEAD is detached; specify a bookmark to push"),
        },
    };
    let local_target = repo
        .refs()
        .read_bookmark(&name)?
        .with_context(|| format!("no local bookmark '{name}'"))?;

    // Server's current refs become our "have" set. A not-yet-existing repo has
    // no refs — push will create it server-side (if you own the namespace), so
    // treat NotFound as an empty ref set rather than an error.
    let server_refs = match client
        .list_refs(authed(
            token.as_deref(),
            ListRefsRequest {
                owner: remote.owner.clone(),
                repo: remote.repo.clone(),
            },
        ))
        .await
    {
        Ok(resp) => resp.into_inner(),
        Err(s) if s.code() == tonic::Code::NotFound => ListRefsResponse::default(),
        Err(s) => return Err(status(s)),
    };
    let have: Vec<ObjectId> = server_refs
        .bookmarks
        .iter()
        .chain(server_refs.tags.iter())
        .filter_map(|r| ObjectId::from_str(&r.target).ok())
        .collect();

    let send = dag::reachable_objects(repo.store(), &[local_target])?;
    let have_objects = dag::reachable_objects(repo.store(), &have)?;

    // Build the push stream: header, objects, then the ref update.
    let mut messages: Vec<PushRequest> = Vec::new();
    messages.push(PushRequest {
        body: Some(push_request::Body::Header(PushHeader {
            owner: remote.owner.clone(),
            repo: remote.repo.clone(),
        })),
    });
    let mut sent = 0;
    for id in send.difference(&have_objects) {
        if let Some(bytes) = repo.store().get_raw(id)? {
            messages.push(PushRequest {
                body: Some(push_request::Body::Object(ObjectChunk {
                    id: id.to_hex(),
                    data: bytes,
                })),
            });
            sent += 1;
        }
    }
    messages.push(PushRequest {
        body: Some(push_request::Body::Updates(RefUpdates {
            updates: vec![RefUpdate {
                name: name.clone(),
                new_target: local_target.to_hex(),
                is_tag: false,
                force,
            }],
        })),
    });

    let stream = futures::stream::iter(messages);
    let resp = client
        .push(authed(token.as_deref(), stream))
        .await
        .map_err(status)?
        .into_inner();
    if !resp.accepted {
        bail!("push rejected: {}", resp.message);
    }
    println!(
        "pushed {} -> {}/{} ({} object(s))",
        name, remote.owner, remote.repo, sent
    );
    Ok(())
}

/// How `pull` reconciles a checked-out bookmark that has diverged from the
/// remote. Fast-forwards always apply regardless of strategy.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PullStrategy {
    /// Only fast-forward; warn (and change nothing) on divergence.
    FfOnly,
    /// Rebase local-only changes onto the remote target.
    Rebase,
    /// Create a merge commit combining local and remote.
    Merge,
}

pub async fn pull(repo: &Repo, remote_name: &str, strategy: PullStrategy) -> Result<()> {
    let url = remote::get_remote(repo, remote_name)?;
    let remote = RemoteUrl::parse(&url)?;
    let token = token_for_remote(&remote)?;
    let mut client = client_for(&remote).await?;

    let server_refs = client
        .list_refs(authed(
            token.as_deref(),
            ListRefsRequest {
                owner: remote.owner.clone(),
                repo: remote.repo.clone(),
            },
        ))
        .await
        .map_err(status)?
        .into_inner();

    let wants: Vec<String> = server_refs
        .bookmarks
        .iter()
        .chain(server_refs.tags.iter())
        .map(|r| r.target.clone())
        .collect();
    let have: Vec<String> = repo
        .refs()
        .list_bookmarks()?
        .into_iter()
        .map(|(_, id)| id.to_hex())
        .collect();

    download(&mut client, &remote, token.as_deref(), repo, wants, have).await?;

    let current = repo.refs().read_head()?;
    for b in &server_refs.bookmarks {
        let new_target = ObjectId::from_str(&b.target)?;
        let old = repo.refs().read_bookmark(&b.name)?;
        let is_current = matches!(&current, Head::Bookmark(n) if *n == b.name);

        // Fast-forward (including a brand-new local bookmark) always applies.
        let ff = match old {
            Some(o) => dag::is_ancestor(repo.store(), o, new_target)?,
            None => true,
        };
        if ff {
            repo.refs().set_bookmark(&b.name, new_target)?;
            if is_current {
                let commit = repo.store().get_commit(&new_target)?;
                working_copy::restore(repo, &commit.tree)?;
            }
            continue;
        }

        // Diverged: never clobber the bookmark (that would orphan local commits).
        if !is_current {
            println!(
                "note: bookmark '{}' diverged from remote; left unchanged",
                b.name
            );
            continue;
        }
        match strategy {
            PullStrategy::FfOnly => {
                println!(
                    "note: '{}' diverged; use `chip pull --rebase` or `--merge` to integrate",
                    b.name
                );
            }
            PullStrategy::Rebase => {
                let outcome = chip_core::ops::rebase(repo, &new_target.to_hex())?;
                report_integration("rebased", &b.name, &outcome);
            }
            PullStrategy::Merge => {
                let outcome = chip_core::ops::merge(repo, &new_target.to_hex())?;
                report_integration("merged", &b.name, &outcome);
            }
        }
    }
    for t in &server_refs.tags {
        repo.refs()
            .set_tag(&t.name, ObjectId::from_str(&t.target)?)?;
    }
    println!("pulled from {}/{}", remote.owner, remote.repo);
    Ok(())
}

fn report_integration(verb: &str, bookmark: &str, outcome: &chip_core::ops::MergeOutcome) {
    if outcome.conflicts.is_empty() {
        println!(
            "{verb} '{bookmark}' onto remote ({})",
            outcome.commit.short()
        );
    } else {
        println!(
            "{verb} '{bookmark}' with {} conflict(s); resolve then `chip resolve`/`chip commit`",
            outcome.conflicts.len()
        );
        for path in &outcome.conflicts {
            println!("  {path}");
        }
    }
}

/// Stream objects from the server into the local store.
async fn download(
    client: &mut ChipSyncClient<Channel>,
    remote: &RemoteUrl,
    token: Option<&str>,
    repo: &Repo,
    want: Vec<String>,
    have: Vec<String>,
) -> Result<()> {
    let mut stream = client
        .fetch_objects(authed(
            token,
            FetchRequest {
                owner: remote.owner.clone(),
                repo: remote.repo.clone(),
                want,
                have,
            },
        ))
        .await
        .map_err(status)?
        .into_inner();

    while let Some(chunk) = stream.message().await.map_err(status)? {
        let id = ObjectId::from_str(&chunk.id)?;
        repo.store().put_raw(&id, &chunk.data)?;
    }
    Ok(())
}

fn status(s: tonic::Status) -> anyhow::Error {
    anyhow::anyhow!("{}: {}", s.code(), s.message())
}
