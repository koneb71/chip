//! Short-lived token → user cache.
//!
//! Authenticated requests previously hit Postgres on *every* call (a SELECT plus
//! a `last_used` UPDATE). This cache collapses that to at most one DB round-trip
//! per token per TTL, which is the single biggest win for request throughput.
//! Trade-off: token revocation takes effect after at most `ttl`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::db::{Db, User};

pub struct TokenCache {
    db: Db,
    ttl: Duration,
    entries: DashMap<String, (User, Instant)>,
}

impl TokenCache {
    pub fn new(db: Db, ttl: Duration) -> Arc<TokenCache> {
        Arc::new(TokenCache {
            db,
            ttl,
            entries: DashMap::new(),
        })
    }

    /// Resolve a token hash to a user, using the cache when fresh. On a miss the
    /// underlying query also stamps `last_used` (so its granularity is `ttl`).
    pub async fn user_for_token(&self, token_hash: &str) -> anyhow::Result<Option<User>> {
        if let Some(entry) = self.entries.get(token_hash) {
            if entry.1.elapsed() < self.ttl {
                return Ok(Some(entry.0.clone()));
            }
        }
        match self.db.user_for_token(token_hash).await? {
            Some(user) => {
                self.entries
                    .insert(token_hash.to_string(), (user.clone(), Instant::now()));
                Ok(Some(user))
            }
            None => {
                self.entries.remove(token_hash);
                Ok(None)
            }
        }
    }
}
