//! chip-server: a deployable host for chip repositories.
//!
//! Serves the gRPC sync protocol and the web UI on a single port (gRPC requests
//! are routed by their `/chip.ChipSync/*` paths; everything else is the web UI),
//! so a Dokploy deployment needs just one mapped domain.

// tonic's `Status` is a large error type used pervasively across the gRPC
// service; boxing it everywhere would obscure the handlers for no real gain.
#![allow(clippy::result_large_err)]

mod auth;
mod cache;
mod config;
mod crypto;
mod db;
mod grpc;
mod highlight;
mod ratelimit;
mod render_cache;
mod review;
mod ssh;
mod store;
mod validate;
mod web;

use std::sync::Arc;
use std::time::Duration;

use chip_proto::chip_sync_server::ChipSyncServer;
use tonic::service::Routes;
use tracing_subscriber::EnvFilter;

use crate::cache::TokenCache;
use crate::config::Config;
use crate::db::Db;
use crate::grpc::ChipService;
use crate::ratelimit::RateLimiter;
use crate::store::StoreFactory;
use crate::web::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::from_env()?;
    tracing::info!(bind = %config.bind, object_store = %config.object_store, "starting chip-server");

    let db = Db::connect(&config.database_url, config.db_max_connections).await?;
    db.migrate().await?;
    tracing::info!("database migrations applied");

    let stores = StoreFactory::from_config(&config)?;

    // Token cache (60s TTL) — collapses the per-request auth DB round-trip.
    let tokens = TokenCache::new(db.clone(), Duration::from_secs(60));
    // Cluster-wide login throttle: lock a username after 5 failures / 15 minutes.
    let limiter = RateLimiter::new(db.clone(), 5, Duration::from_secs(15 * 60));

    // Background task: prune expired tokens hourly so the table stays bounded.
    {
        let db = db.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tick.tick().await;
                match db.delete_expired_tokens().await {
                    Ok(n) if n > 0 => tracing::info!("pruned {n} expired tokens"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("token prune failed: {e}"),
                }
            }
        });
    }

    // SSH transport (key-authenticated), tunneling the gRPC sync service.
    if !config.ssh_bind.is_empty() {
        let deps = ssh::SshDeps {
            db: db.clone(),
            stores: stores.clone(),
            tokens: tokens.clone(),
            limiter: limiter.clone(),
        };
        let (bind, host_key) = (config.ssh_bind.clone(), config.ssh_host_key.clone());
        tokio::spawn(async move {
            if let Err(e) = ssh::serve(bind, host_key, deps).await {
                tracing::error!("SSH server stopped: {e}");
            }
        });
    }

    let state = AppState {
        db: db.clone(),
        stores: stores.clone(),
        config: Arc::new(config.clone()),
        limiter: limiter.clone(),
        tokens: tokens.clone(),
        // Bounded in-process cache for expensive, immutable renders (highlighted
        // blobs, rendered READMEs, diff HTML, history walks).
        renders: render_cache::RenderCache::new(512, 128),
    };

    // gRPC service as an axum router, merged with the web UI on one port.
    let grpc = ChipSyncServer::new(ChipService {
        db: db.clone(),
        stores: stores.clone(),
        limiter: limiter.clone(),
        tokens: tokens.clone(),
    });
    let grpc_router = Routes::new(grpc).into_axum_router();

    let app = web::router(state).merge(grpc_router);

    let addr: std::net::SocketAddr = config.bind.parse()?;
    match config.tls() {
        Some((cert, key)) => {
            // rustls 0.23 requires an explicit crypto provider when more than one
            // is compiled in (here via tonic + axum-server).
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await?;
            tracing::info!("listening on https://{} (TLS)", config.bind);
            axum_server::bind_rustls(addr, tls)
                .serve(app.into_make_service())
                .await?;
        }
        None => {
            let listener = tokio::net::TcpListener::bind(&config.bind).await?;
            tracing::info!("listening on http://{}", config.bind);
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
        }
    }
    Ok(())
}

/// Resolve when the process receives SIGTERM or Ctrl-C, so in-flight requests
/// drain cleanly during rolling deploys / replica scale-down.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received; draining");
}
