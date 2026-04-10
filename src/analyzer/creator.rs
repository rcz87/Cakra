use anyhow::{Context, Result};
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::info;

use crate::models::token::CreatorHistory;

/// In-memory cache for creator analysis results with a configurable TTL.
pub struct CreatorCache {
    cache: Mutex<HashMap<String, (CreatorHistory, Instant)>>,
    ttl: Duration,
}

impl CreatorCache {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// Return a cached `CreatorHistory` if present and not expired.
    pub fn get(&self, creator: &str) -> Option<CreatorHistory> {
        let map = self.cache.lock().expect("CreatorCache lock poisoned");
        if let Some((history, inserted_at)) = map.get(creator) {
            if inserted_at.elapsed() < self.ttl {
                return Some(history.clone());
            }
        }
        None
    }

    /// Store a `CreatorHistory` in the cache.
    pub fn set(&self, creator: &str, history: CreatorHistory) {
        let mut map = self.cache.lock().expect("CreatorCache lock poisoned");
        map.insert(creator.to_string(), (history, Instant::now()));
    }
}

/// Minimum number of rugs to classify creator as a rugger.
/// Framework: even 1 rug = instant reject.
const RUG_THRESHOLD: u32 = 1;

/// If liquidity is removed within this many slots after creation, it's suspicious.
const QUICK_REMOVE_SLOT_WINDOW: u64 = 1000;

/// Analyze the history of a creator wallet to determine if they have a pattern
/// of creating tokens and quickly removing liquidity (rug-pulling).
pub async fn analyze_creator(rpc: &RpcClient, creator: &str, cache: &CreatorCache) -> Result<CreatorHistory> {
    // Check cache first
    if let Some(cached) = cache.get(creator) {
        info!(creator = %creator, "Using cached creator analysis");
        return Ok(cached);
    }

    let creator_pubkey =
        Pubkey::from_str(creator).context("Invalid creator public key")?;

    // Fetch recent transaction signatures for the creator wallet (limited to 20).
    let signatures = rpc
        .get_signatures_for_address_with_config(
            &creator_pubkey,
            GetConfirmedSignaturesForAddress2Config {
                limit: Some(20),
                ..Default::default()
            },
        )
        .context("Failed to fetch creator transaction history")?;

    if signatures.is_empty() {
        info!(creator = %creator, "No transaction history found for creator");
        return Ok(CreatorHistory::Unknown);
    }

    let mut tokens_created: u32 = 0;
    let mut suspected_rugs: u32 = 0;

    // Track token creation events and subsequent liquidity removals.
    // We look for InitializeMint instructions followed by close/remove-liquidity
    // patterns within a short window.
    let mut creation_slots: Vec<(String, u64)> = Vec::new(); // (mint, slot)

    for sig_info in &signatures {
        let slot = sig_info.slot;

        // Check if this transaction involved token creation
        if let Ok(sig) = Signature::from_str(&sig_info.signature) {
            if let Ok(tx) = rpc.get_transaction(
                &sig,
                solana_transaction_status::UiTransactionEncoding::Json,
            ) {
                let logs: Option<&Vec<String>> = tx
                    .transaction
                    .meta
                    .as_ref()
                    .and_then(|m| m.log_messages.as_ref().into());

                if let Some(log_msgs) = logs {
                    let is_token_creation = log_msgs.iter().any(|log: &String| {
                        log.contains("InitializeMint") || log.contains("initializeMint")
                    });

                    let is_liquidity_removal = log_msgs.iter().any(|log: &String| {
                        log.contains("RemoveLiquidity")
                            || log.contains("removeLiquidity")
                            || log.contains("Withdraw")
                    });

                    if is_token_creation {
                        tokens_created += 1;
                        creation_slots.push((sig_info.signature.clone(), slot));
                    }

                    if is_liquidity_removal {
                        // Check if any recent token creation is within the rug window
                        for (ref _create_sig, create_slot) in &creation_slots {
                            if slot > *create_slot
                                && slot - create_slot < QUICK_REMOVE_SLOT_WINDOW
                            {
                                suspected_rugs += 1;
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    let history = if suspected_rugs >= RUG_THRESHOLD {
        CreatorHistory::Rugger {
            tokens_created,
            rugs: suspected_rugs,
        }
    } else if suspected_rugs > 0 {
        CreatorHistory::Suspicious {
            tokens_created,
            rugs: suspected_rugs,
        }
    } else if tokens_created > 0 {
        CreatorHistory::Clean { tokens_created }
    } else {
        CreatorHistory::Unknown
    };

    info!(
        creator = %creator,
        tokens_created,
        suspected_rugs,
        "Creator analysis complete"
    );

    // Store in cache for future lookups
    cache.set(creator, history.clone());

    Ok(history)
}
