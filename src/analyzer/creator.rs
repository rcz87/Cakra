use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use std::str::FromStr;
use tracing::{info, warn};

use crate::models::token::CreatorHistory;

/// The SPL Token program ID.
const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// Minimum number of rugs to classify creator as a serial rugger.
const RUG_THRESHOLD: u32 = 2;

/// If liquidity is removed within this many slots after creation, it's suspicious.
const QUICK_REMOVE_SLOT_WINDOW: u64 = 1000;

/// Analyze the history of a creator wallet to determine if they have a pattern
/// of creating tokens and quickly removing liquidity (rug-pulling).
pub fn analyze_creator(rpc: &RpcClient, creator: &str) -> Result<CreatorHistory> {
    let creator_pubkey =
        Pubkey::from_str(creator).context("Invalid creator public key")?;

    // Fetch recent transaction signatures for the creator wallet.
    let signatures = rpc
        .get_signatures_for_address(&creator_pubkey)
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

    Ok(history)
}
