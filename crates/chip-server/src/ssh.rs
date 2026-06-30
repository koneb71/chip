//! Embedded SSH transport.
//!
//! An SSH server authenticates clients by public key (mapping a key fingerprint
//! to a user) and then **tunnels the existing gRPC sync service over the SSH
//! channel** — the authenticated user is injected as a [`SshIdentity`] request
//! extension, so every sync RPC and all access control works unchanged.

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use chip_proto::chip_sync_server::ChipSyncServer;
use rand::RngCore;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{HashAlg, PrivateKey};
use russh::server::{Auth, Config, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, ChannelStream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tonic::transport::server::Connected;
use tonic::Request;

use crate::cache::TokenCache;
use crate::db::{Db, User};
use crate::grpc::{ChipService, SshIdentity};
use crate::ratelimit::RateLimiter;
use crate::store::StoreFactory;

/// Shared dependencies used to build a `ChipService` per connection.
#[derive(Clone)]
pub struct SshDeps {
    pub db: Db,
    pub stores: StoreFactory,
    pub tokens: Arc<TokenCache>,
    pub limiter: Arc<RateLimiter>,
}

/// Load the persisted ed25519 host key, generating + saving it on first run.
fn load_or_create_host_key(path: &str) -> anyhow::Result<PrivateKey> {
    if Path::new(path).exists() {
        Ok(russh::keys::load_secret_key(path, None)?)
    } else {
        // Build an ed25519 key from 32 random bytes (avoids ssh-key's rand_core
        // version pinning).
        let mut seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let pem = key.to_openssh(russh::keys::ssh_key::LineEnding::LF)?;
        std::fs::write(path, pem)?;
        tracing::info!("generated SSH host key at {path}");
        Ok(key)
    }
}

/// Run the SSH server forever on `bind`.
pub async fn serve(bind: String, host_key_path: String, deps: SshDeps) -> anyhow::Result<()> {
    let host_key = load_or_create_host_key(&host_key_path)?;
    let config = Arc::new(Config {
        keys: vec![host_key],
        ..Default::default()
    });
    let mut server = SshServer { deps };
    tracing::info!("SSH listening on {bind}");
    server.run_on_address(config, bind).await?;
    Ok(())
}

struct SshServer {
    deps: SshDeps,
}

impl Server for SshServer {
    type Handler = SshHandler;

    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> SshHandler {
        SshHandler {
            deps: self.deps.clone(),
            user: None,
        }
    }
}

struct SshHandler {
    deps: SshDeps,
    user: Option<User>,
}

impl Handler for SshHandler {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        _login: &str,
        public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        let fingerprint = public_key.fingerprint(HashAlg::Sha256).to_string();
        match self.deps.db.user_for_ssh_key(&fingerprint).await {
            Ok(Some(user)) => {
                self.user = Some(user);
                Ok(Auth::Accept)
            }
            _ => Ok(Auth::reject()),
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        // Identity established at auth time; tunnel gRPC over this channel.
        let Some(user) = self.user.clone() else {
            return Ok(false);
        };
        let deps = self.deps.clone();
        let stream = channel.into_stream();
        tokio::spawn(async move {
            if let Err(e) = serve_grpc(stream, deps, user).await {
                tracing::debug!("ssh gRPC session ended: {e}");
            }
        });
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // The gRPC service is already serving on the channel stream; just accept.
        session.channel_success(channel)?;
        Ok(())
    }
}

/// Serve the gRPC sync service over a single SSH channel stream, injecting the
/// authenticated user into every request.
async fn serve_grpc(stream: ChannelStream<Msg>, deps: SshDeps, user: User) -> anyhow::Result<()> {
    let service = ChipService {
        db: deps.db,
        stores: deps.stores,
        tokens: deps.tokens,
        limiter: deps.limiter,
    };
    let server = ChipSyncServer::with_interceptor(service, move |mut req: Request<()>| {
        req.extensions_mut().insert(SshIdentity(user.clone()));
        Ok(req)
    });

    let incoming = futures::stream::once(async move { Ok::<_, std::io::Error>(SshConn(stream)) });
    tonic::transport::Server::builder()
        .add_service(server)
        .serve_with_incoming(incoming)
        .await?;
    Ok(())
}

/// Wraps a russh channel stream so tonic can serve over it.
struct SshConn(ChannelStream<Msg>);

impl Connected for SshConn {
    type ConnectInfo = ();
    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl AsyncRead for SshConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for SshConn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
