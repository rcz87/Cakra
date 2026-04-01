use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use tracing::{info, warn};

use crate::models::token::LpStatus;

/// Well-known burn address on Solana (system program / zero address).
const BURN_ADDRESS: &str = "1nc1nerator11111111111111111111111111111111";

/// Known LP lock program addresses (e.g. Uncx, Team Finance equivalents on Solana).
const KNOWN_LOCK_PROGRAMS: &[&str] = &[
    // Raydium LP locker
    "LockrpuRAp7B7VqBC93FMkJFdJMzQoMKnVKFjtsRFxH",
];

/// Check the status of LP tokens for a given pool.
///
/// Inspects the token accounts that hold LP tokens to determine whether
/// they have been burned (sent to a known burn address) or locked in a
/// known locker program.
pub fn check_lp_status(rpc: &RpcClient, pool_address: &str) -> Result<LpStatus> {
    let pool_pubkey =
        Pubkey::from_str(pool_address).context("Invalid pool address")?;

    // Fetch the pool account to read LP mint from its data.
    let pool_account = rpc
        .get_account(&pool_pubkey)
        .context("Failed to fetch pool account")?;

    // Raydium AMM v4 layout: LP mint is at bytes 128..160 in the pool state.
    // This may vary per DEX; we attempt Raydium first.
    let data = &pool_account.data;
    if data.len() < 160 {
        warn!(pool = %pool_address, "Pool account data too short to extract LP mint");
        return Ok(LpStatus::Unknown);
    }

    let lp_mint_bytes: [u8; 32] = data[128..160]
        .try_into()
        .context("Failed to slice LP mint bytes")?;
    let lp_mint = Pubkey::new_from_array(lp_mint_bytes);

    info!(pool = %pool_address, lp_mint = %lp_mint, "Extracted LP mint from pool");

    // Get the largest token accounts holding this LP mint.
    let token_accounts = rpc
        .get_token_largest_accounts(&lp_mint)
        .context("Failed to fetch largest LP token accounts")?;

    if token_accounts.is_empty() {
        return Ok(LpStatus::Unknown);
    }

    let burn_pubkey = Pubkey::from_str(BURN_ADDRESS).ok();

    for account_info in &token_accounts {
        let holder = Pubkey::from_str(&account_info.address)
            .unwrap_or_default();

        // Check burn address
        if let Some(ref burn) = burn_pubkey {
            if holder == *burn {
                info!(pool = %pool_address, "LP tokens burned");
                return Ok(LpStatus::Burned);
            }
        }

        // Fetch the token account to check its owner
        if let Ok(holder_account) = rpc.get_account(&holder) {
            let owner = holder_account.owner;
            let owner_str = owner.to_string();

            if KNOWN_LOCK_PROGRAMS.contains(&owner_str.as_str()) {
                info!(pool = %pool_address, locker = %owner_str, "LP tokens locked");
                return Ok(LpStatus::Locked);
            }
        }
    }

    // Check if total supply is very low (indicating most tokens were burned directly)
    let supply = rpc
        .get_token_supply(&lp_mint)
        .context("Failed to get LP token supply")?;

    let total_supply: u64 = supply
        .amount
        .parse()
        .unwrap_or(0);

    if total_supply == 0 {
        info!(pool = %pool_address, "LP total supply is zero, considered burned");
        return Ok(LpStatus::Burned);
    }

    info!(pool = %pool_address, "LP tokens not burned or locked");
    Ok(LpStatus::NotBurned)
}
