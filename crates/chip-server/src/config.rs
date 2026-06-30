use std::env;

use crate::crypto;

const DEFAULT_SECRET: &str = "dev-insecure-secret";

/// Runtime configuration, sourced from environment variables so it slots
/// cleanly into a Dokploy / Docker deployment.
#[derive(Clone)]
pub struct Config {
    /// Address to bind the combined gRPC + HTTP server to.
    pub bind: String,
    /// Postgres connection string.
    pub database_url: String,
    /// Object store location: `local:///path` or `s3://bucket`.
    pub object_store: String,
    /// Secret used to derive CSRF tokens and (in dev) the data key.
    pub secret: String,
    /// Public base URL, used in the web UI.
    pub base_url: String,
    /// AES-256 key used to encrypt object data at rest.
    pub data_key: [u8; 32],
    /// Whether to mark the session cookie `Secure` (HTTPS deployments).
    pub cookie_secure: bool,
    /// PEM cert/key paths for native TLS; `None` serves plaintext.
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    /// Max Postgres connections in the pool (per server instance).
    pub db_max_connections: u32,
    /// SSH transport bind address (empty disables SSH).
    pub ssh_bind: String,
    /// Path to the persisted SSH host key (generated on first run).
    pub ssh_host_key: String,
}

fn truthy(var: &str) -> bool {
    matches!(
        env::var(var).ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes")
    )
}

impl Config {
    pub fn from_env() -> anyhow::Result<Config> {
        let dev = truthy("CHIP_DEV");

        let secret = env::var("CHIP_SECRET").unwrap_or_else(|_| DEFAULT_SECRET.to_string());
        if secret == DEFAULT_SECRET && !dev {
            anyhow::bail!(
                "refusing to start with the default CHIP_SECRET. Set CHIP_SECRET to a strong \
                 random value (or set CHIP_DEV=1 for local development)."
            );
        }

        // Data-at-rest key: explicit in production, derived from the secret in dev.
        let data_key = match env::var("CHIP_DATA_KEY") {
            Ok(hex) => crypto::parse_key_hex(&hex)?,
            Err(_) if dev => {
                tracing::warn!(
                    "CHIP_DATA_KEY not set; deriving an insecure dev key from CHIP_SECRET (CHIP_DEV=1)"
                );
                crypto::derive_dev_key(&secret)
            }
            Err(_) => anyhow::bail!(
                "CHIP_DATA_KEY is required (64 hex chars / 32 bytes) so object data is encrypted \
                 at rest. Generate one with `openssl rand -hex 32` (or set CHIP_DEV=1 for a derived \
                 dev key). WARNING: losing this key makes stored repositories unrecoverable."
            ),
        };

        let base_url =
            env::var("CHIP_BASE_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
        let cookie_secure = truthy("CHIP_COOKIE_SECURE") || base_url.starts_with("https://");

        let tls_cert = env::var("CHIP_TLS_CERT").ok().filter(|s| !s.is_empty());
        let tls_key = env::var("CHIP_TLS_KEY").ok().filter(|s| !s.is_empty());
        if tls_cert.is_some() != tls_key.is_some() {
            anyhow::bail!("CHIP_TLS_CERT and CHIP_TLS_KEY must be set together");
        }

        Ok(Config {
            bind: env::var("CHIP_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            database_url: env::var("DATABASE_URL")
                .map_err(|_| anyhow::anyhow!("DATABASE_URL is required"))?,
            object_store: env::var("CHIP_OBJECT_STORE")
                .unwrap_or_else(|_| "local:///data/repos".to_string()),
            secret,
            base_url,
            data_key,
            cookie_secure,
            tls_cert,
            tls_key,
            db_max_connections: env::var("CHIP_DB_MAX_CONNECTIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(25),
            ssh_bind: env::var("CHIP_SSH_BIND").unwrap_or_else(|_| "0.0.0.0:2222".to_string()),
            ssh_host_key: env::var("CHIP_SSH_HOST_KEY")
                .unwrap_or_else(|_| "chip_ssh_host_key".to_string()),
        })
    }

    pub fn tls(&self) -> Option<(&str, &str)> {
        match (&self.tls_cert, &self.tls_key) {
            (Some(c), Some(k)) => Some((c.as_str(), k.as_str())),
            _ => None,
        }
    }
}
