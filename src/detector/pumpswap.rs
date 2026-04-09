use anyhow::Result;
use chrono::Utc;
use tracing::{info, warn};

use crate::models::token::{DetectionBackend, TokenInfo, TokenSource};
use super::parser::{self, RawTransaction};

/// PumpSwap program ID (migration from Pump.fun bonding curve to AMM).
pub const PUMPSWAP_PROGRAM_ID: &str = "PSwapMdSai8tjrEXcxFeQth87xC4rRsa4VA5mhGhXkP";

/// Anchor discriminator for the PumpSwap "migrate" / "create_pool" instruction.
/// First 8 bytes of SHA256("global:create_pool").
const CREATE_POOL_DISCRIMINATOR: [u8; 8] = [0xe9, 0x92, 0xd1, 0x8e, 0xcf, 0x6c, 0x5a, 0x11];

/// PumpSwap create_pool account layout:
///   [0]  pool (pool state PDA)
///   [1]  authority (pool authority PDA)
///   [2]  token_mint_a (typically the meme token)
///   [3]  token_mint_b (typically WSOL)
///   [4]  token_vault_a
///   [5]  token_vault_b
///   [6]  lp_mint
///   [7]  creator_lp_token_account
///   [8]  creator_token_a_account
///   [9]  creator_token_b_account
///   [10] creator (signer)
///   [11] token_program
///   [12] associated_token_program
///   [13] system_program

/// Wrapped SOL mint.
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Parse a PumpSwap pool creation (migration) instruction.
pub fn parse_pumpswap_create_pool(data: &[u8], accounts: &[String]) -> Option<TokenInfo> {
    if data.len() < 8 {
        return None;
    }

    if data[..8] != CREATE_POOL_DISCRIMINATOR {
        return None;
    }

    match parse_create_pool_fields(data, accounts) {
        Ok(info) => Some(info),
        Err(e) => {
            warn!("Failed to parse PumpSwap create_pool instruction: {}", e);
            None
        }
    }
}

fn parse_create_pool_fields(data: &[u8], accounts: &[String]) -> Result<TokenInfo> {
    // After 8-byte discriminator:
    //   init_amount_a: u64 (LE) at offset 8  - token amount
    //   init_amount_b: u64 (LE) at offset 16 - SOL amount
    let init_amount_b = if data.len() >= 24 {
        parser::extract_u64_le(data, 16)?
    } else {
        0
    };

    let pool_address = accounts.first().cloned().unwrap_or_default();
    let token_mint_a = accounts.get(2).cloned().unwrap_or_default();
    let token_mint_b = accounts.get(3).cloned().unwrap_or_default();
    let creator = accounts.get(10).cloned().unwrap_or_default();

    // Determine which is the new token (non-SOL side)
    let (token_mint, initial_sol_lamports) = if token_mint_a == WSOL_MINT {
        let amount = if data.len() >= 16 {
            parser::extract_u64_le(data, 8).unwrap_or(0)
        } else {
            0
        };
        (token_mint_b, amount)
    } else {
        (token_mint_a, init_amount_b)
    };

    let initial_liquidity_sol = initial_sol_lamports as f64 / 1_000_000_000.0;

    info!(
        mint = %token_mint,
        pool = %pool_address,
        liquidity_sol = initial_liquidity_sol,
        creator = %creator,
        "Detected PumpSwap migration / pool creation"
    );

    Ok(TokenInfo {
        mint: token_mint,
        name: String::new(),
        symbol: String::new(),
        source: TokenSource::PumpSwap,
        creator,
        initial_liquidity_sol,
        initial_liquidity_usd: 0.0,
        pool_address: Some(pool_address),
        metadata_uri: None,
        decimals: 6,
        detected_at: Utc::now(),
        backend: DetectionBackend::Helius,
        market_cap_sol: 0.0,
    })
}

/// Process a raw transaction and attempt to parse it as a PumpSwap event.
pub fn process_pumpswap_transaction(tx: &RawTransaction) -> Option<TokenInfo> {
    if tx.program_id != PUMPSWAP_PROGRAM_ID {
        return None;
    }
    parse_pumpswap_create_pool(&tx.data, &tx.accounts)
}
