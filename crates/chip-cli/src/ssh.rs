//! SSH client transport: authenticate via ssh-agent (then key files) and tunnel
//! the gRPC sync client over the SSH channel. The server host key is verified
//! against a chip-specific known_hosts file (trust-on-first-use, then pinned).

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskCtx, Poll};

use anyhow::{Context, Result};
use hyper::rt::{Read as HyperRead, ReadBufCursor, Write as HyperWrite};
use hyper_util::rt::TokioIo;
use russh::client::{self, Config, Handle, Handler};
#[cfg(unix)]
use russh::keys::agent::client::AgentClient;
use russh::keys::PrivateKeyWithHashAlg;
use tonic::transport::server::Connected;
use tonic::transport::{Channel, Endpoint, Uri};

type SshStream = russh::ChannelStream<russh::client::Msg>;

/// Client handler verifying the server host key against a chip-specific
/// known_hosts file (trust-on-first-use, then pinned).
struct ClientHandler {
    host: String,
    port: u16,
    known_hosts: PathBuf,
}

impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        match russh::keys::check_known_hosts_path(&self.host, self.port, key, &self.known_hosts) {
            Ok(true) => Ok(true),
            Ok(false) => {
                // Unknown host: trust on first use and record it.
                if let Some(parent) = self.known_hosts.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = russh::keys::known_hosts::learn_known_hosts_path(
                    &self.host,
                    self.port,
                    key,
                    &self.known_hosts,
                );
                eprintln!(
                    "chip: trusting host key for {}:{} on first connection",
                    self.host, self.port
                );
                Ok(true)
            }
            Err(russh::keys::Error::KeyChanged { .. }) => {
                eprintln!(
                    "chip: WARNING — host key for {}:{} has CHANGED. Refusing to connect \
                     (possible man-in-the-middle). Remove the stale entry from {} if this is expected.",
                    self.host,
                    self.port,
                    self.known_hosts.display()
                );
                Ok(false)
            }
            Err(_) => Ok(false),
        }
    }
}

/// chip's own known_hosts file, so it doesn't disturb the user's OpenSSH
/// known_hosts. Cross-platform via [`crate::remote::config_dir`].
fn known_hosts_path() -> PathBuf {
    crate::remote::config_dir()
        .unwrap_or_default()
        .join("known_hosts")
}

/// Open an SSH connection, authenticate, and return a tonic `Channel` that runs
/// gRPC over the SSH session channel.
pub async fn channel(host: &str, port: u16, login: &str) -> Result<Channel> {
    let config = Arc::new(Config::default());
    let handler = ClientHandler {
        host: host.to_string(),
        port,
        known_hosts: known_hosts_path(),
    };
    let mut session = client::connect(config, (host, port), handler)
        .await
        .with_context(|| format!("ssh connect to {host}:{port} failed"))?;

    if !authenticate(&mut session, login).await? {
        anyhow::bail!(
            "ssh authentication failed — add your public key in the web UI under \"SSH keys\" \
             and ensure ~/.ssh/id_ed25519 (or id_rsa) is present"
        );
    }

    let ch = session
        .channel_open_session()
        .await
        .context("ssh channel open failed")?;
    let stream = ch.into_stream();

    // Hand the single channel stream to tonic once; keep the SSH session alive
    // for the lifetime of the connection.
    let keep = Arc::new(session);
    let slot = Arc::new(Mutex::new(Some(SshConn(TokioIo::new(stream)))));
    let connector = tower::service_fn(move |_uri: Uri| {
        let keep = keep.clone();
        let slot = slot.clone();
        async move {
            let _hold = keep;
            slot.lock()
                .unwrap()
                .take()
                .ok_or_else(|| std::io::Error::other("ssh stream already consumed"))
        }
    });

    let channel = Endpoint::try_from("http://ssh.invalid")?
        .connect_with_connector(connector)
        .await
        .context("gRPC-over-SSH connect failed")?;
    Ok(channel)
}

/// Authenticate via ssh-agent first (supports passphrase-protected keys), then
/// fall back to common key files.
async fn authenticate(session: &mut Handle<ClientHandler>, login: &str) -> Result<bool> {
    // ssh-agent is Unix-only here (Windows uses a named-pipe agent russh's
    // connect_env doesn't support) — Windows falls through to key files.
    #[cfg(unix)]
    if let Ok(mut agent) = AgentClient::connect_env().await {
        if let Ok(identities) = agent.request_identities().await {
            for id in identities {
                let pubkey = id.public_key().into_owned();
                if let Ok(res) = session
                    .authenticate_publickey_with(login, pubkey, None, &mut agent)
                    .await
                {
                    if res.success() {
                        return Ok(true);
                    }
                }
            }
        }
    }

    for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
        let path = ssh_dir().join(name);
        if !path.exists() {
            continue;
        }
        let Ok(key) = russh::keys::load_secret_key(&path, None) else {
            continue;
        };
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), None);
        if let Ok(res) = session.authenticate_publickey(login, key).await {
            if res.success() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The user's `.ssh` directory (`~/.ssh`, or `%USERPROFILE%\.ssh` on Windows).
fn ssh_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".ssh")
}

/// Wraps a russh channel stream so tonic (hyper 1) can connect over it.
struct SshConn(TokioIo<SshStream>);

impl Connected for SshConn {
    type ConnectInfo = ();
    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl HyperRead for SshConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskCtx<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl HyperWrite for SshConn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskCtx<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskCtx<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskCtx<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
