//! Remote bookkeeping: per-repo remote URLs and per-server auth tokens.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chip_core::repo::Repo;

/// Which wire transport a remote uses.
pub enum Transport {
    /// gRPC over HTTP(S); `endpoint` is `scheme://host[:port]`.
    Http,
    /// gRPC tunneled over SSH.
    Ssh {
        host: String,
        port: u16,
        login: String,
    },
}

/// A repository URL split into its transport and `owner/repo` path.
pub struct RemoteUrl {
    pub transport: Transport,
    /// For HTTP: `scheme://host[:port]` (also the credentials key). For SSH: a
    /// display string `ssh://login@host:port`.
    pub endpoint: String,
    pub owner: String,
    pub repo: String,
}

impl RemoteUrl {
    /// Parse `http(s)://host[:port]/owner/repo`, `ssh://[user@]host[:port]/owner/repo`,
    /// or scp-style `[user@]host:owner/repo` (SSH, port 22).
    pub fn parse(url: &str) -> Result<RemoteUrl> {
        if let Some(rest) = url.strip_prefix("ssh://") {
            let (hostpart, path) = rest
                .split_once('/')
                .context("ssh URL must include /owner/repo")?;
            let (login, host, port) = parse_ssh_authority(hostpart, 22);
            let (owner, repo) = split_owner_repo(path)?;
            return Ok(RemoteUrl {
                endpoint: format!("ssh://{login}@{host}:{port}"),
                transport: Transport::Ssh { host, port, login },
                owner,
                repo,
            });
        }
        if let Some((scheme, rest)) = url.split_once("://") {
            // http / https
            let (host, path) = rest
                .split_once('/')
                .context("remote URL must include /owner/repo")?;
            let (owner, repo) = split_owner_repo(path)?;
            return Ok(RemoteUrl {
                transport: Transport::Http,
                endpoint: format!("{scheme}://{host}"),
                owner,
                repo,
            });
        }
        // scp-style: [user@]host:owner/repo  (SSH on port 22)
        if let Some((authority, path)) = url.split_once(':') {
            let (login, host, port) = parse_ssh_authority(authority, 22);
            let (owner, repo) = split_owner_repo(path)?;
            return Ok(RemoteUrl {
                endpoint: format!("ssh://{login}@{host}:{port}"),
                transport: Transport::Ssh { host, port, login },
                owner,
                repo,
            });
        }
        anyhow::bail!("unrecognized remote URL: {url}")
    }
}

/// Parse `[user@]host[:port]` into (login, host, port).
fn parse_ssh_authority(authority: &str, default_port: u16) -> (String, String, u16) {
    let (login, hostport) = match authority.split_once('@') {
        Some((u, h)) => (u.to_string(), h),
        None => ("chip".to_string(), authority),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default_port)),
        None => (hostport.to_string(), default_port),
    };
    (login, host, port)
}

fn split_owner_repo(path: &str) -> Result<(String, String)> {
    let mut parts = path.trim_matches('/').splitn(2, '/');
    let owner = parts
        .next()
        .filter(|s| !s.is_empty())
        .context("missing owner in URL")?;
    let repo = parts
        .next()
        .filter(|s| !s.is_empty())
        .context("missing repo in URL")?;
    Ok((
        owner.to_string(),
        repo.trim_end_matches(".chip").to_string(),
    ))
}

/// chip's config directory, cross-platform: `~/.config/chip` on Linux/macOS
/// (XDG), `%APPDATA%\chip` on Windows.
pub fn config_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .or_else(dirs::config_dir)
        .context("could not determine config directory")?;
    Ok(base.join("chip"))
}

/// Path to the global credentials file.
fn credentials_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("credentials"))
}

/// Load all `endpoint -> token` credentials.
fn load_credentials() -> Result<BTreeMap<String, String>> {
    let path = credentials_path()?;
    let mut map = BTreeMap::new();
    if let Ok(content) = fs::read_to_string(&path) {
        for line in content.lines() {
            if let Some((url, token)) = line.split_once(' ') {
                map.insert(url.to_string(), token.to_string());
            }
        }
    }
    Ok(map)
}

/// Store the auth token for an endpoint.
pub fn save_token(endpoint: &str, token: &str) -> Result<()> {
    let mut creds = load_credentials()?;
    creds.insert(endpoint.to_string(), token.to_string());
    let path = credentials_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body: String = creds.iter().map(|(u, t)| format!("{u} {t}\n")).collect();
    fs::write(&path, body)?;
    Ok(())
}

/// The stored token for an endpoint, if any.
pub fn token_for(endpoint: &str) -> Result<Option<String>> {
    Ok(load_credentials()?.get(endpoint).cloned())
}

// Per-repo remotes ----------------------------------------------------------

fn remotes_path(repo: &Repo) -> PathBuf {
    repo.chip_dir().join("remotes")
}

pub fn load_remotes(repo: &Repo) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    if let Ok(content) = fs::read_to_string(remotes_path(repo)) {
        for line in content.lines() {
            if let Some((name, url)) = line.split_once(' ') {
                map.insert(name.to_string(), url.to_string());
            }
        }
    }
    Ok(map)
}

pub fn add_remote(repo: &Repo, name: &str, url: &str) -> Result<()> {
    let mut remotes = load_remotes(repo)?;
    remotes.insert(name.to_string(), url.to_string());
    let body: String = remotes.iter().map(|(n, u)| format!("{n} {u}\n")).collect();
    fs::write(remotes_path(repo), body)?;
    Ok(())
}

pub fn get_remote(repo: &Repo, name: &str) -> Result<String> {
    load_remotes(repo)?
        .get(name)
        .cloned()
        .with_context(|| format!("no remote named '{name}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http() {
        let r = RemoteUrl::parse("http://h:8090/alice/proj").unwrap();
        assert!(matches!(r.transport, Transport::Http));
        assert_eq!(
            (r.endpoint.as_str(), r.owner.as_str(), r.repo.as_str()),
            ("http://h:8090", "alice", "proj")
        );
    }

    #[test]
    fn parses_ssh_url_with_user_and_port() {
        let r = RemoteUrl::parse("ssh://chip@host:2222/alice/proj").unwrap();
        match r.transport {
            Transport::Ssh { host, port, login } => {
                assert_eq!(
                    (host.as_str(), port, login.as_str()),
                    ("host", 2222, "chip")
                );
            }
            _ => panic!("expected ssh"),
        }
        assert_eq!((r.owner.as_str(), r.repo.as_str()), ("alice", "proj"));
    }

    #[test]
    fn config_dir_honors_xdg() {
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/chip-xdg-test");
        let d = config_dir().unwrap();
        std::env::remove_var("XDG_CONFIG_HOME");
        assert_eq!(d, std::path::Path::new("/tmp/chip-xdg-test/chip"));
    }

    #[test]
    fn parses_scp_style_defaults() {
        let r = RemoteUrl::parse("host:alice/proj").unwrap();
        match r.transport {
            Transport::Ssh { host, port, login } => {
                assert_eq!((host.as_str(), port, login.as_str()), ("host", 22, "chip"));
            }
            _ => panic!("expected ssh"),
        }
    }
}
