//! In-process caches for expensive, deterministic renders.
//!
//! Every input is an immutable, content-addressed object (a blob, tree, or commit
//! id), so a rendered blob, README, diff, or history walk never changes for a
//! given key — **no invalidation is needed and stale reads are impossible**. A new
//! commit or blob is simply a new key; old entries age out via the LRU. Entries
//! are bounded (and HTML is size-limited per entry) so memory stays flat.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use chip_core::hash::ObjectId;
use chip_core::object::Commit;

/// Rendered HTML larger than this isn't cached, so a few huge files can't evict
/// everything else.
const MAX_HTML_BYTES: usize = 256 * 1024;

/// A cached history walk: `(commit id, commit)` pairs, shared cheaply via `Arc`.
pub type CachedHistory = Arc<Vec<(ObjectId, Commit)>>;

/// A minimal bounded LRU: a map from key to `(value, last-use tick)`. Eviction
/// scans for the smallest tick, which is fine at the small capacities used here
/// and avoids an external dependency (and its advisories).
struct Lru<K, V> {
    map: HashMap<K, (V, u64)>,
    cap: usize,
    tick: u64,
}

impl<K: Eq + Hash + Clone, V> Lru<K, V> {
    fn new(cap: usize) -> Self {
        Lru {
            map: HashMap::new(),
            cap: cap.max(1),
            tick: 0,
        }
    }

    fn get(&mut self, k: &K) -> Option<&V> {
        self.tick += 1;
        let t = self.tick;
        let entry = self.map.get_mut(k)?;
        entry.1 = t;
        Some(&entry.0)
    }

    fn put(&mut self, k: K, v: V) {
        self.tick += 1;
        let t = self.tick;
        if self.map.len() >= self.cap && !self.map.contains_key(&k) {
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, tick))| *tick)
                .map(|(key, _)| key.clone())
            {
                self.map.remove(&oldest);
            }
        }
        self.map.insert(k, (v, t));
    }
}

pub struct RenderCache {
    html: Mutex<Lru<String, Arc<str>>>,
    history: Mutex<Lru<ObjectId, CachedHistory>>,
}

impl RenderCache {
    pub fn new(html_cap: usize, history_cap: usize) -> Arc<RenderCache> {
        Arc::new(RenderCache {
            html: Mutex::new(Lru::new(html_cap)),
            history: Mutex::new(Lru::new(history_cap)),
        })
    }

    /// Cached rendered HTML for `key`, if present (also marks it recently used).
    pub fn get_html(&self, key: &str) -> Option<Arc<str>> {
        self.html.lock().unwrap().get(&key.to_string()).cloned()
    }

    /// Cache rendered HTML under `key`, unless it exceeds the per-entry limit.
    pub fn put_html(&self, key: String, html: &str) {
        if html.len() > MAX_HTML_BYTES {
            return;
        }
        self.html.lock().unwrap().put(key, Arc::from(html));
    }

    /// A cached history walk for `head`, or `compute()` cached under it. `head` is
    /// a content hash that fully determines the result. The lock is not held
    /// during `compute` (a concurrent miss just recomputes — idempotent).
    pub fn history_or_else(
        &self,
        head: ObjectId,
        compute: impl FnOnce() -> Vec<(ObjectId, Commit)>,
    ) -> CachedHistory {
        if let Some(v) = self.history.lock().unwrap().get(&head) {
            return v.clone();
        }
        let v = Arc::new(compute());
        self.history.lock().unwrap().put(head, v.clone());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_round_trip_and_size_guard() {
        let c = RenderCache::new(4, 4);
        c.put_html("k".into(), "<p>hi</p>");
        assert_eq!(c.get_html("k").as_deref(), Some("<p>hi</p>"));
        assert!(c.get_html("missing").is_none());
        // Oversized entries are skipped.
        let big = "x".repeat(MAX_HTML_BYTES + 1);
        c.put_html("big".into(), &big);
        assert!(c.get_html("big").is_none());
    }

    #[test]
    fn evicts_least_recently_used() {
        let c = RenderCache::new(2, 2);
        c.put_html("a".into(), "1");
        c.put_html("b".into(), "2");
        let _ = c.get_html("a"); // touch a so b is now least-recently-used
        c.put_html("c".into(), "3"); // evicts b
        assert!(c.get_html("b").is_none());
        assert_eq!(c.get_html("a").as_deref(), Some("1"));
        assert_eq!(c.get_html("c").as_deref(), Some("3"));
    }

    #[test]
    fn history_is_cached_by_head() {
        let c = RenderCache::new(4, 4);
        let head = ObjectId::hash(b"head");
        let v = c.history_or_else(head, Vec::new);
        assert_eq!(v.len(), 0);
        // Second lookup must not re-run the closure.
        let v2 = c.history_or_else(head, || panic!("should have been cached"));
        assert_eq!(v2.len(), 0);
    }
}
