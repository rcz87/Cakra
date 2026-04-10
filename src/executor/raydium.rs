// Raydium pool reader (Sprint 3a — read-only).
//
// This module decodes Raydium pool state accounts and computes prices
// from on-chain reserve balances. It does NOT yet build swap instructions —
// that's reserved for Sprint 3b.
//
// Currently implemented:
//   - CPMM (Constant Product Market Maker) — used by PumpSwap migrations
//
// Deferred to Sprint 3b:
//   - AMM v4 layout (legacy, less common for newborns)
//   - Direct buy/sell instruction builders

use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use tracing::{debug, warn};

/// WSOL mint.
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Raydium CPMM Anchor pool state layout (well-known offsets):
///
/// ```text
/// 0..8     discriminator
/// 8..40    amm_config: Pubkey
/// 40..72   pool_creator: Pubkey
/// 72..104  token_0_vault: Pubkey
/// 104..136 token_1_vault: Pubkey
/// 136..168 lp_mint: Pubkey
/// 168..200 token_0_mint: Pubkey
/// 200..232 token_1_mint: Pubkey
/// 232..264 token_0_program: Pubkey
/// 264..296 token_1_program: Pubkey
/// 296..304 observation_key: Pubkey (start)
/// ...
/// ```
const CPMM_TOKEN_0_VAULT_OFFSET: usize = 72;
const CPMM_TOKEN_1_VAULT_OFFSET: usize = 104;
const CPMM_TOKEN_0_MINT_OFFSET: usize = 168;
const CPMM_TOKEN_1_MINT_OFFSET: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub enum RaydiumPoolKind {
    /// Constant Product Market Maker — Anchor program. Used by PumpSwap migrations.
    Cpmm,
    /// Legacy AMM v4 — not yet implemented for direct reading.
    AmmV4,
}

/// Decoded Raydium pool metadata: who's who in the vault layout.
#[derive(Debug, Clone)]
pub struct RaydiumPoolMeta {
    pub pool: Pubkey,
    pub kind: RaydiumPoolKind,
    /// The non-WSOL token mint
    pub token_mint: Pubkey,
    /// The non-WSOL vault (holds the project token)
    pub token_vault: Pubkey,
    /// The WSOL vault (holds wrapped SOL)
    pub sol_vault: Pubkey,
}

/// Read pool state and decode into RaydiumPoolMeta.
/// Currently supports CPMM only. Returns error for unsupported layouts.
pub fn load_pool_meta(rpc: &RpcClient, pool_address: &str) -> Result<RaydiumPoolMeta> {
    let pool_pubkey = Pubkey::from_str(pool_address)
        .with_context(|| format!("Invalid pool pubkey: {}", pool_address))?;

    let account = rpc
        .get_account(&pool_pubkey)
        .context("Failed to fetch pool account")?;

    // Heuristic: CPMM accounts are owned by the CPMM program ID.
    let cpmm_program: Pubkey = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C"
        .parse()
        .unwrap();
    let amm_v4_program: Pubkey = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"
        .parse()
        .unwrap();

    if account.owner == cpmm_program {
        decode_cpmm_pool(&pool_pubkey, &account.data)
    } else if account.owner == amm_v4_program {
        // Sprint 3b will implement this. Return error so dispatcher falls back.
        anyhow::bail!("AMM v4 pool reader not yet implemented (Sprint 3b)")
    } else {
        anyhow::bail!(
            "Pool {} owned by unknown program {}",
            pool_address,
            account.owner
        )
    }
}

fn decode_cpmm_pool(pool: &Pubkey, data: &[u8]) -> Result<RaydiumPoolMeta> {
    if data.len() < CPMM_TOKEN_1_MINT_OFFSET + 32 {
        anyhow::bail!(
            "CPMM pool data too short: {} bytes (need at least {})",
            data.len(),
            CPMM_TOKEN_1_MINT_OFFSET + 32
        );
    }

    let token_0_vault = read_pubkey(data, CPMM_TOKEN_0_VAULT_OFFSET)?;
    let token_1_vault = read_pubkey(data, CPMM_TOKEN_1_VAULT_OFFSET)?;
    let token_0_mint = read_pubkey(data, CPMM_TOKEN_0_MINT_OFFSET)?;
    let token_1_mint = read_pubkey(data, CPMM_TOKEN_1_MINT_OFFSET)?;

    let wsol: Pubkey = WSOL_MINT.parse().unwrap();

    let (token_mint, token_vault, sol_vault) = if token_0_mint == wsol {
        (token_1_mint, token_1_vault, token_0_vault)
    } else if token_1_mint == wsol {
        (token_0_mint, token_0_vault, token_1_vault)
    } else {
        // Neither side is SOL — non-WSOL pair, can't price in SOL
        anyhow::bail!(
            "CPMM pool {} has no WSOL side ({} / {})",
            pool, token_0_mint, token_1_mint
        );
    };

    debug!(
        pool = %pool,
        token_mint = %token_mint,
        token_vault = %token_vault,
        sol_vault = %sol_vault,
        "Decoded CPMM pool meta"
    );

    Ok(RaydiumPoolMeta {
        pool: *pool,
        kind: RaydiumPoolKind::Cpmm,
        token_mint,
        token_vault,
        sol_vault,
    })
}

fn read_pubkey(data: &[u8], offset: usize) -> Result<Pubkey> {
    if data.len() < offset + 32 {
        anyhow::bail!(
            "Cannot read pubkey at offset {}: data too short",
            offset
        );
    }
    let bytes: [u8; 32] = data[offset..offset + 32]
        .try_into()
        .context("Failed to slice pubkey bytes")?;
    Ok(Pubkey::new_from_array(bytes))
}

/// Read both vault balances and return (sol_amount_lamports, token_amount_base_units).
pub fn read_reserves(rpc: &RpcClient, meta: &RaydiumPoolMeta) -> Result<(u64, u64)> {
    let sol_balance = rpc
        .get_token_account_balance(&meta.sol_vault)
        .context("Failed to read SOL vault balance")?;
    let token_balance = rpc
        .get_token_account_balance(&meta.token_vault)
        .context("Failed to read token vault balance")?;

    let sol_amount: u64 = sol_balance
        .amount
        .parse()
        .context("Failed to parse SOL vault amount")?;
    let token_amount: u64 = token_balance
        .amount
        .parse()
        .context("Failed to parse token vault amount")?;

    Ok((sol_amount, token_amount))
}

/// Compute price per base unit of token, in SOL.
/// Formula: price = (sol_reserves / 1e9) / token_reserves
/// (consistent with PriceFeed and Position entry_price_sol convention)
pub fn pool_price_per_base_unit(sol_reserves: u64, token_reserves: u64) -> Result<f64> {
    if sol_reserves == 0 || token_reserves == 0 {
        anyhow::bail!("Pool has zero reserves");
    }
    Ok((sol_reserves as f64 / 1_000_000_000.0) / token_reserves as f64)
}

/// Quote: how many tokens (base units) you get for a given SOL amount?
/// Constant product formula with 0.25% Raydium fee (typical CPMM).
/// Returns expected output in token base units.
pub fn quote_buy_exact_in(sol_in_lamports: u64, sol_reserves: u64, token_reserves: u64) -> u64 {
    if sol_reserves == 0 || token_reserves == 0 || sol_in_lamports == 0 {
        return 0;
    }
    // Apply 0.25% fee on input (Raydium CPMM)
    let fee_numerator: u128 = 9_975;
    let fee_denominator: u128 = 10_000;
    let amount_in_after_fee =
        (sol_in_lamports as u128 * fee_numerator) / fee_denominator;

    // x * y = k → token_out = token_reserves - (k / (sol_reserves + amount_in))
    let k = (sol_reserves as u128) * (token_reserves as u128);
    let new_sol = sol_reserves as u128 + amount_in_after_fee;
    let new_token = k / new_sol;
    token_reserves
        .saturating_sub(new_token.try_into().unwrap_or(u64::MAX))
}

/// Quote: how much SOL (lamports) you get for selling N base units of token?
pub fn quote_sell_exact_in(token_in: u64, sol_reserves: u64, token_reserves: u64) -> u64 {
    if sol_reserves == 0 || token_reserves == 0 || token_in == 0 {
        return 0;
    }
    let fee_numerator: u128 = 9_975;
    let fee_denominator: u128 = 10_000;
    let amount_in_after_fee = (token_in as u128 * fee_numerator) / fee_denominator;

    let k = (sol_reserves as u128) * (token_reserves as u128);
    let new_token = token_reserves as u128 + amount_in_after_fee;
    let new_sol = k / new_token;
    sol_reserves
        .saturating_sub(new_sol.try_into().unwrap_or(u64::MAX))
}

#[allow(dead_code)]
pub fn warn_unsupported(pool: &str) {
    warn!(pool = %pool, "Raydium pool kind not supported by reader yet");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quote_buy_constant_product() {
        // 100 SOL : 1_000_000 tokens (price = 0.0001 SOL per token base unit)
        let sol_reserves: u64 = 100_000_000_000; // 100 SOL in lamports
        let token_reserves: u64 = 1_000_000;

        let out = quote_buy_exact_in(1_000_000_000, sol_reserves, token_reserves); // buy with 1 SOL
        // Expected ~ 1_000_000 - (k / (101 SOL after fee))
        // Should be a few thousand tokens out, much less than reserves
        assert!(out > 0);
        assert!(out < token_reserves);
    }

    #[test]
    fn test_quote_sell_round_trip() {
        let sol_reserves: u64 = 100_000_000_000;
        let token_reserves: u64 = 1_000_000;

        let bought = quote_buy_exact_in(1_000_000_000, sol_reserves, token_reserves);
        // After buy, reserves change. Selling back should yield slightly less than 1 SOL due to fees.
        let new_sol = sol_reserves + 1_000_000_000;
        let new_token = token_reserves - bought;
        let sold = quote_sell_exact_in(bought, new_sol, new_token);
        assert!(sold < 1_000_000_000);  // less than 1 SOL due to fees
        assert!(sold > 990_000_000);    // but not by much
    }

    #[test]
    fn test_pool_price_per_base_unit() {
        let price = pool_price_per_base_unit(100_000_000_000, 1_000_000).unwrap();
        // 100 SOL / 1M base units = 1e-4 SOL per base unit
        assert!((price - 0.0001).abs() < 1e-9);
    }
}
