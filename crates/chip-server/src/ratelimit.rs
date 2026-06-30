//! Cluster-wide login rate limiting, backed by Postgres so it holds across all
//! server replicas. Login is low-QPS, so the per-attempt DB cost is negligible.
//! Fails open (allows) on a DB error, favoring availability.

use std::sync::Arc;
use std::time::Duration;

use crate::db::Db;

pub struct RateLimiter {
    db: Db,
    max_failures: i32,
    window_secs: i64,
}

impl RateLimiter {
    pub fn new(db: Db, max_failures: u32, window: Duration) -> Arc<RateLimiter> {
        Arc::new(RateLimiter {
            db,
            max_failures: max_failures as i32,
            window_secs: window.as_secs() as i64,
        })
    }

    /// Whether `key` (a username) may currently attempt to log in.
    pub async fn allowed(&self, key: &str) -> bool {
        self.db
            .login_allowed(key, self.max_failures, self.window_secs)
            .await
            .unwrap_or(true)
    }

    pub async fn record_failure(&self, key: &str) {
        if let Err(e) = self.db.record_login_failure(key, self.window_secs).await {
            tracing::warn!("failed to record login failure: {e}");
        }
    }

    pub async fn record_success(&self, key: &str) {
        let _ = self.db.clear_login_failures(key).await;
    }
}
