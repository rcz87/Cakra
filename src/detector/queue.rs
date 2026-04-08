use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Deduplication queue that tracks seen mint addresses with a configurable TTL.
/// Entries expire after the TTL and are cleaned up periodically.
pub struct DeduplicationQueue {
    entries: HashMap<String, Instant>,
    ttl: Duration,
}

impl DeduplicationQueue {
    /// Create a new deduplication queue with the given TTL in seconds.
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// Create a new deduplication queue with the default TTL of 5 minutes.
    pub fn with_default_ttl() -> Self {
        Self::new(300)
    }

    /// Insert a mint address into the queue.
    /// Returns `true` if the mint is new (not seen or expired), `false` if already present.
    pub fn insert(&mut self, mint: &str) -> bool {
        let now = Instant::now();

        if let Some(inserted_at) = self.entries.get(mint) {
            if now.duration_since(*inserted_at) < self.ttl {
                return false;
            }
        }

        self.entries.insert(mint.to_string(), now);
        true
    }

    /// Remove expired entries from the queue.
    pub fn cleanup(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|_, inserted_at| now.duration_since(*inserted_at) < self.ttl);
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_new_mint() {
        let mut queue = DeduplicationQueue::with_default_ttl();
        assert!(queue.insert("mint1"));
        assert!(!queue.insert("mint1"));
    }

    #[test]
    fn test_different_mints() {
        let mut queue = DeduplicationQueue::with_default_ttl();
        assert!(queue.insert("mint1"));
        assert!(queue.insert("mint2"));
        assert!(!queue.insert("mint1"));
    }

    #[test]
    fn test_cleanup_with_short_ttl() {
        let mut queue = DeduplicationQueue::new(0); // instant expiry
        queue.entries.insert(
            "old_mint".to_string(),
            Instant::now() - Duration::from_secs(1),
        );
        queue.cleanup();
        assert!(queue.entries.is_empty());
    }
}
