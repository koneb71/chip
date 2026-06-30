//! Per-repository object stores, built from the configured backend.
//!
//! `local://` deployments use chip-core's filesystem backend directly.
//! `s3://` deployments use an adapter over the `object_store` crate, so moving
//! to S3-compatible storage (MinIO, AWS) is purely a config change.

use std::sync::Arc;

use bytes::Bytes;
use chip_core::error::{Error as CoreError, Result as CoreResult};
use chip_core::store::{FilesystemBackend, ObjectBackend, ObjectStore};
use dashmap::DashMap;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as StorePath;
use object_store::ObjectStore as RemoteStore;
use tokio::runtime::Runtime;

use crate::config::Config;
use crate::crypto::EncryptedBackend;
use crate::validate::valid_name;

/// Factory that produces (and caches) an [`ObjectStore`] per repository.
///
/// Shared resources — the S3 client and its bridging runtime — are built once at
/// startup, and constructed stores are cached, so the request hot path does no
/// client/runtime construction.
#[derive(Clone)]
pub struct StoreFactory {
    spec: StoreSpec,
    data_key: [u8; 32],
    cache: Arc<DashMap<String, ObjectStore>>,
}

#[derive(Clone)]
enum StoreSpec {
    Local {
        root: String,
    },
    S3 {
        client: Arc<dyn RemoteStore>,
        rt: Arc<Runtime>,
    },
}

impl StoreFactory {
    pub fn from_config(config: &Config) -> anyhow::Result<StoreFactory> {
        let spec = if let Some(path) = config.object_store.strip_prefix("local://") {
            StoreSpec::Local {
                root: path.to_string(),
            }
        } else if let Some(bucket) = config.object_store.strip_prefix("s3://") {
            // Build the S3 client + a dedicated multi-thread runtime ONCE. The
            // multi-thread runtime allows concurrent `block_on` calls from the
            // server's spawn_blocking threads.
            let client = AmazonS3Builder::from_env()
                .with_bucket_name(bucket)
                .build()?;
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?;
            StoreSpec::S3 {
                client: Arc::new(client),
                rt: Arc::new(rt),
            }
        } else {
            anyhow::bail!(
                "CHIP_OBJECT_STORE must start with local:// or s3:// (got {})",
                config.object_store
            );
        };
        Ok(StoreFactory {
            spec,
            data_key: config.data_key,
            cache: Arc::new(DashMap::new()),
        })
    }

    /// An object store for `{owner}/{repo}`, encrypted at rest. Cached after the
    /// first construction.
    pub fn repo_store(&self, owner: &str, repo: &str) -> anyhow::Result<ObjectStore> {
        // Defense in depth: never let an unvalidated name reach the filesystem
        // or an S3 key, even if it somehow bypassed the input boundaries.
        if !valid_name(owner) || !valid_name(repo) {
            anyhow::bail!("invalid repository path component");
        }
        let key = format!("{owner}/{repo}");
        if let Some(store) = self.cache.get(&key) {
            return Ok(store.clone());
        }

        let prefix = format!("{key}/objects");
        let inner: Arc<dyn ObjectBackend> = match &self.spec {
            StoreSpec::Local { root } => {
                let dir = std::path::Path::new(root).join(&prefix);
                std::fs::create_dir_all(&dir)?;
                Arc::new(FilesystemBackend::new(dir))
            }
            StoreSpec::S3 { client, rt } => Arc::new(S3Backend {
                inner: client.clone(),
                rt: rt.clone(),
                prefix,
            }),
        };
        // Encrypt object data at rest, transparently to content-addressing.
        let backend = Arc::new(EncryptedBackend::new(inner, &self.data_key));
        let store = ObjectStore::new(backend);
        self.cache.insert(key, store.clone());
        Ok(store)
    }
}

/// Bridges chip-core's synchronous [`ObjectBackend`] onto the async
/// `object_store` crate by driving calls on a shared multi-thread runtime.
/// Backend methods are always invoked from blocking contexts (the gRPC handlers
/// wrap store access in `spawn_blocking`), so blocking here is safe.
struct S3Backend {
    inner: Arc<dyn RemoteStore>,
    prefix: String,
    rt: Arc<Runtime>,
}

impl S3Backend {
    fn key_path(&self, key: &str) -> StorePath {
        StorePath::from(format!("{}/{}", self.prefix, key))
    }
}

impl ObjectBackend for S3Backend {
    fn get(&self, key: &str) -> CoreResult<Option<Vec<u8>>> {
        let path = self.key_path(key);
        self.rt.block_on(async {
            match self.inner.get(&path).await {
                Ok(res) => {
                    let bytes = res
                        .bytes()
                        .await
                        .map_err(|e| CoreError::Other(e.to_string()))?;
                    Ok(Some(bytes.to_vec()))
                }
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(CoreError::Other(e.to_string())),
            }
        })
    }

    fn put(&self, key: &str, bytes: &[u8]) -> CoreResult<()> {
        let path = self.key_path(key);
        let payload = Bytes::copy_from_slice(bytes);
        self.rt.block_on(async {
            self.inner
                .put(&path, payload.into())
                .await
                .map(|_| ())
                .map_err(|e| CoreError::Other(e.to_string()))
        })
    }
}
