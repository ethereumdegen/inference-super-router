use moka::sync::Cache;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const DEFAULT_CACHE_TTL_SECS: u64 = 30;
const FAILURE_TTL_SECS: u64 = 60;

/// Cache of recently verified payer addresses.
///
/// Verified addresses skip the synchronous verification step.
/// Addresses with recent failures are downgraded to sequential verify-then-serve.
pub struct VerificationCache {
    cache: Cache<String, ()>,
    failures: Cache<String, ()>,
    hits: AtomicU64,
    misses: AtomicU64,
    downgrades: AtomicU64,
}

impl VerificationCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            cache: Cache::builder()
                .time_to_live(ttl)
                .max_capacity(10_000)
                .build(),
            failures: Cache::builder()
                .time_to_live(Duration::from_secs(FAILURE_TTL_SECS))
                .max_capacity(10_000)
                .build(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            downgrades: AtomicU64::new(0),
        }
    }

    pub fn with_default_ttl() -> Self {
        Self::new(Duration::from_secs(DEFAULT_CACHE_TTL_SECS))
    }

    pub fn is_verified(&self, address: &str) -> bool {
        if self.cache.get(address).is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    pub fn mark_verified(&self, address: &str) {
        self.cache.insert(address.to_string(), ());
        self.failures.invalidate(address);
    }

    pub fn has_recent_failure(&self, address: &str) -> bool {
        if self.failures.get(address).is_some() {
            self.downgrades.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub fn record_failure(&self, address: &str) {
        self.failures.insert(address.to_string(), ());
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
            self.downgrades.load(Ordering::Relaxed),
        )
    }
}
