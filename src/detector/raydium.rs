use anyhow::Result;
use chrono::Utc;
use tracing::{info, warn};

use crate::models::token::{TokenInfo, TokenSource};
use super::parser::{self, RawTransaction};

/// Raydium AMM V4 program ID on Solana mainnet.
pub const RAYDIUM_AMM_PROGRAM_ID: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

/// Raydium CPMM (Concentrated Position Market Maker) program ID.
pub const RAYDIUM_CPMM_PROGRAM_ID: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";

/// WSOL mint address (Wrapped SOL).
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Discriminator for Raydium AMM `initialize2` instruction.
/// First byte is the instruction index in the AMM program.
const INITIALIZE2_DISCRIMINATOR: u8 = 1;

/// Raydium AMM initialize2 instruction layout (after 1-byte instruction index):
///   - nonce: u8
///   - open_time: u64 (LE)
///   - init_pc_amount: u64 (LE) - initial quote token amount
///   - init_coin_amount: u64 (LE) - initial base token amount
///
/// Account layout for initialize2:
///   [0]  token_program
///   [1]  system_program
///   [2]  rent
///   [3]  amm (pool address)
///   [4]  amm_authority
///   [5]  amm_open_orders
///   [6]  lp_mint
///   [7]  coin_mint (base token)
///   [8]  pc_mint (quote token)
///   [9]  coin_vault
///   [10] pc_vault
///   [11] target_orders
///   [12] amm_config (market config)
///   [13] fee_destination
///   [14] market_program
///   [15] market
///   [16] user_wallet
///   [17] user_coin_token_account
///   [18] user_pc_token_account

/// Parse a Raydium AMM `initialize2` instruction.
pub fn parse_raydium_initialize2(data: &[u8], accounts: &[String]) -> Option<TokenInfo> {
    if data.is_empty() {
        return None;
    }

    // Check instruction discriminator
    if data[0] != INITIALIZE2_DISCRIMINATOR {
        return None;
    }

    match parse_initialize2_fields(data, accounts) {
        Ok(info) => Some(info),
        Err(e) => {
            warn!("Failed to parse Raydium initialize2 instruction: {}", e);
            None
        }
    }
}

fn parse_initialize2_fields(data: &[u8], accounts: &[String]) -> Result<TokenInfo> {
    if data.len() < 26 {
        anyhow::bail!(
            "Raydium initialize2 data too short: {} bytes (need at least 26)",
            data.len()
        );
    }

    // Skip instruction index (1 byte) and nonce (1 byte)
    let _nonce = data[1];

    // open_time: u64 at offset 2
    let _open_time = parser::extract_u64_le(data, 2)?;

    // init_pc_amount (quote/SOL amount): u64 at offset 10
    let init_pc_amount = parser::extract_u64_le(data, 10)?;

    // init_coin_amount (base/coin amount): u64 at offset 18
    let init_coin_amount = parser::extract_u64_le(data, 18)?;

    // Extract accounts
    let pool_address = accounts.get(3).cloned().unwrap_or_default();
    let coin_mint = accounts.get(7).cloned().unwrap_or_default();
    let pc_mint = accounts.get(8).cloned().unwrap_or_default();
    let user_wallet = accounts.get(16).cloned().unwrap_or_default();

    // Determine which mint is the new token (the non-SOL side)
    // Raydium convention: coin = base, pc = quote
    let (token_mint, initial_sol_lamports) = if coin_mint == WSOL_MINT {
        // SOL is on the coin (base) side → SOL amount = init_coin_amount
        (pc_mint, init_coin_amount)
    } else if pc_mint == WSOL_MINT {
        // SOL is the quote; the new token is the base
        (coin_mint, init_pc_amount)
    } else {
        // Neither side is SOL -- still track it, use pc_mint amount as liquidity proxy
        (coin_mint, init_pc_amount)
    };

    let initial_liquidity_sol = initial_sol_lamports as f64 / 1_000_000_000.0;

    info!(
        mint = %token_mint,
        pool = %pool_address,
        liquidity_sol = initial_liquidity_sol,
        "Detected Raydium AMM pool creation"
    );

    Ok(TokenInfo {
        mint: token_mint,
        name: String::new(),   // Must be fetched from metadata
        symbol: String::new(), // Must be fetched from metadata
        source: TokenSource::Raydium,
        creator: user_wallet,
        initial_liquidity_sol,
        initial_liquidity_usd: 0.0, // Will be calculated later
        pool_address: Some(pool_address),
        metadata_uri: None,
        decimals: 0, // Must be fetched on-chain
        detected_at: Utc::now(),
    })
}

/// Discriminator for Raydium CPMM `initialize` instruction.
/// Anchor discriminator: first 8 bytes of SHA256("global:initialize").
const CPMM_INITIALIZE_DISCRIMINATOR: [u8; 8] = [0xaf, 0xaf, 0x6d, 0x1f, 0x0d, 0x98, 0x9b, 0xed];

/// Parse a Raydium CPMM initialize instruction.
///
/// CPMM initialize account layout:
///   [0]  creator
///   [1]  amm_config
///   [2]  authority
///   [3]  pool_state
///   [4]  token_0_mint
///   [5]  token_1_mint
///   [6]  creator_token_0_account
///   [7]  creator_token_1_account
///   [8]  token_0_vault
///   [9]  token_1_vault
///   [10] create_pool_fee_account
///   [11] observe_state
///   [12] token_program
///   [13] token_0_program
///   [14] token_1_program
///   [15] associated_token_program
///   [16] system_program
///   [17] rent
pub fn parse_raydium_cpmm_initialize(data: &[u8], accounts: &[String]) -> Option<TokenInfo> {
    if data.len() < 8 {
        return None;
    }

    if data[..8] != CPMM_INITIALIZE_DISCRIMINATOR {
        return None;
    }

    match parse_cpmm_initialize_fields(data, accounts) {
        Ok(info) => Some(info),
        Err(e) => {
            warn!("Failed to parse Raydium CPMM initialize: {}", e);
            None
        }
    }
}

fn parse_cpmm_initialize_fields(data: &[u8], accounts: &[String]) -> Result<TokenInfo> {
    // After 8-byte discriminator:
    //   init_amount_0: u64 (LE) at offset 8
    //   init_amount_1: u64 (LE) at offset 16
    //   open_time: u64 (LE) at offset 24
    if data.len() < 32 {
        anyhow::bail!(
            "CPMM initialize data too short: {} bytes (need at least 32)",
            data.len()
        );
    }

    let init_amount_0 = parser::extract_u64_le(data, 8)?;
    let init_amount_1 = parser::extract_u64_le(data, 16)?;

    let creator = accounts.first().cloned().unwrap_or_default();
    let pool_state = accounts.get(3).cloned().unwrap_or_default();
    let token_0_mint = accounts.get(4).cloned().unwrap_or_default();
    let token_1_mint = accounts.get(5).cloned().unwrap_or_default();

    // Determine which side is the new token
    let (token_mint, initial_sol_lamports) = if token_0_mint == WSOL_MINT {
        (token_1_mint, init_amount_0)
    } else if token_1_mint == WSOL_MINT {
        (token_0_mint, init_amount_1)
    } else {
        (token_0_mint, init_amount_1)
    };

    let initial_liquidity_sol = initial_sol_lamports as f64 / 1_000_000_000.0;

    info!(
        mint = %token_mint,
        pool = %pool_state,
        liquidity_sol = initial_liquidity_sol,
        "Detected Raydium CPMM pool creation"
    );

    Ok(TokenInfo {
        mint: token_mint,
        name: String::new(),
        symbol: String::new(),
        source: TokenSource::Raydium,
        creator,
        initial_liquidity_sol,
        initial_liquidity_usd: 0.0,
        pool_address: Some(pool_state),
        metadata_uri: None,
        decimals: 0,
        detected_at: Utc::now(),
    })
}

/// Process a raw transaction and attempt to parse it as a Raydium pool creation.
/// Tries both AMM V4 and CPMM.
pub fn process_raydium_transaction(tx: &RawTransaction) -> Option<TokenInfo> {
    if tx.program_id == RAYDIUM_AMM_PROGRAM_ID {
        parse_raydium_initialize2(&tx.data, &tx.accounts)
    } else if tx.program_id == RAYDIUM_CPMM_PROGRAM_ID {
        parse_raydium_cpmm_initialize(&tx.data, &tx.accounts)
    } else {
        None
    }
}
