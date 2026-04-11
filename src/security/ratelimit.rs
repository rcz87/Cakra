//! Simple in-memory token-bucket rate limiter.
//!
//! Used to throttle Telegram commands per chat so that even if the admin
//! account is compromised (or mis-clicks a button repeatedly), the bot can
//! refuse to queue up dozens of buy orders in a short window.
//!
//! The limiter is intentionally minimal:
//! - One bucket per key (e.g. chat_id as `i64`).
//! - Refills linearly at `refill_per_sec` up to `capacity`.
//! - `try_acquire()` returns `true` if a token was consumed, `false` otherwise.
//! - Old, idle buckets are lazily reclaimed during `try_acquire`.
//!
//! Thread-safe via `Mutex`; lock contention is negligible for Telegram traffic.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// Generic token-bucket rate limiter keyed by `K`.
#[derive(Debug)]
pub struct RateLimiter<K: Eq + Hash + Clone> {
    capacity: f64,
    refill_per_sec: f64,
    /// Idle buckets older than this are reclaimed on access.
    idle_ttl: Duration,
    buckets: Mutex<HashMap<K, Bucket>>,
}

impl<K: Eq + Hash + Clone> RateLimiter<K> {
    /// Create a new limiter.
    ///
    /// * `capacity` — maximum burst size (tokens held at rest).
    /// * `refill_per_sec` — steady-state rate.
    pub fn new(capacity: u32, refill_per_sec: f64) -> Self {
        Self {
            capacity: capacity as f64,
            refill_per_sec,
            idle_ttl: Duration::from_secs(3600),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Attempt to consume one token for `key`.
    ///
    /// Returns `true` if the request is allowed, `false` if rate-limited.
    /// If the internal lock is poisoned the call fails open — logs the
    /// condition via `tracing::warn` and allows the request, because we
    /// prefer availability over strict enforcement for trading UX. Upstream
    /// auth and risk checks remain the real safety boundary.
    pub fn try_acquire(&self, key: &K) -> bool {
        let mut buckets = match self.buckets.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!("RateLimiter lock poisoned — failing open");
                poisoned.into_inner()
            }
        };

        let now = Instant::now();

        // Reclaim idle buckets (cheap sweep — we only scan on contention).
        if buckets.len() > 64 {
            buckets.retain(|_, b| now.duration_since(b.last_refill) < self.idle_ttl);
        }

        let bucket = buckets.entry(key.clone()).or_insert(Bucket {
            tokens: self.capacity,
            last_refill: now,
        });

        // Refill proportional to elapsed time.
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn burst_then_deny() {
        let limiter: RateLimiter<i64> = RateLimiter::new(3, 1.0);
        assert!(limiter.try_acquire(&1));
        assert!(limiter.try_acquire(&1));
        assert!(limiter.try_acquire(&1));
        // Bucket empty — next call denied.
        assert!(!limiter.try_acquire(&1));
    }

    #[test]
    fn refill_after_wait() {
        let limiter: RateLimiter<i64> = RateLimiter::new(1, 10.0);
        assert!(limiter.try_acquire(&42));
        assert!(!limiter.try_acquire(&42));
        sleep(Duration::from_millis(150)); // 10 token/s × 0.15s ≈ 1.5 tokens
        assert!(limiter.try_acquire(&42));
    }

    #[test]
    fn isolated_per_key() {
        let limiter: RateLimiter<i64> = RateLimiter::new(1, 1.0);
        assert!(limiter.try_acquire(&1));
        assert!(limiter.try_acquire(&2));
        assert!(!limiter.try_acquire(&1));
        assert!(!limiter.try_acquire(&2));
    }
}
