use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::debug;

/// Manages per-wallet trade cooldown timers for RICOZ SNIPER.
/// Prevents rapid-fire trading by enforcing a minimum interval between trades.
pub struct CooldownManager {
    cooldown_duration: Duration,
    last_trade: Mutex<HashMap<String, Instant>>,
}

impl CooldownManager {
    /// Create a new cooldown manager with the given cooldown duration in seconds.
    pub fn new(cooldown_secs: u64) -> Self {
        Self {
            cooldown_duration: Duration::from_secs(cooldown_secs),
            last_trade: Mutex::new(HashMap::new()),
        }
    }

    /// Check whether the given wallet is allowed to trade (cooldown has elapsed).
    pub fn can_trade(&self, wallet: &str) -> bool {
        let map = self.last_trade.lock().expect("Cooldown lock poisoned");
        match map.get(wallet) {
            Some(last) => {
                let elapsed = last.elapsed();
                let allowed = elapsed >= self.cooldown_duration;
                if !allowed {
                    let remaining = self.cooldown_duration - elapsed;
                    debug!(
                        wallet = %wallet,
                        remaining_secs = remaining.as_secs(),
                        "Trade cooldown active"
                    );
                }
                allowed
            }
            None => true,
        }
    }

    /// Record that a trade was just executed for the given wallet,
    /// resetting the cooldown timer.
    pub fn record_trade(&self, wallet: &str) {
        let mut map = self.last_trade.lock().expect("Cooldown lock poisoned");
        map.insert(wallet.to_string(), Instant::now());
        debug!(wallet = %wallet, "Trade cooldown recorded");
    }

    /// Get the remaining cooldown time in seconds for a wallet.
    /// Returns 0 if the wallet is not on cooldown.
    pub fn remaining_secs(&self, wallet: &str) -> u64 {
        let map = self.last_trade.lock().expect("Cooldown lock poisoned");
        match map.get(wallet) {
            Some(last) => {
                let elapsed = last.elapsed();
                if elapsed >= self.cooldown_duration {
                    0
                } else {
                    (self.cooldown_duration - elapsed).as_secs()
                }
            }
            None => 0,
        }
    }

    /// Clear the cooldown for a specific wallet (e.g. after manual override).
    pub fn clear(&self, wallet: &str) {
        let mut map = self.last_trade.lock().expect("Cooldown lock poisoned");
        map.remove(wallet);
    }
}
