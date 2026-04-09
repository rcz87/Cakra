use anyhow::Result;
use chrono::Utc;
use tracing::{info, warn};

use crate::models::token::{DetectionBackend, TokenInfo, TokenSource};
use super::parser::{self, RawTransaction};

/// Pump.fun program ID on Solana mainnet.
pub const PUMPFUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

/// Discriminator for the current Pump.fun "create" instruction.
/// PumpFun updated their program — this is the live discriminator as of 2026-04.
const CREATE_DISCRIMINATOR: [u8; 8] = [0xd6, 0x90, 0x4c, 0xec, 0x5f, 0x8b, 0x31, 0xb4];

/// Parse a Pump.fun "create" instruction to extract token information.
///
/// Pump.fun create instruction layout (after 8-byte discriminator):
///   - name: length-prefixed string (4 bytes LE length + string bytes)
///   - symbol: length-prefixed string
///   - uri: length-prefixed string
///
/// Account layout for create (current, 16 accounts):
///   [0]  mint
///   [1]  mint_authority
///   [2]  bonding_curve
///   [3]  associated_bonding_curve
///   [4]  global_config
///   [5]  user (creator / signer)
///   [6]  system_program
///   [7]  token_program
///   [8]  associated_token_program
///   [9]  event_authority
///   [10..15] other accounts
///   ...
pub fn parse_pumpfun_create(data: &[u8]) -> Option<TokenInfo> {
    if data.len() < 8 {
        return None;
    }

    // Check discriminator
    if data[..8] != CREATE_DISCRIMINATOR {
        return None;
    }

    let result = parse_create_fields(&data[8..]);
    match result {
        Ok(info) => Some(info),
        Err(e) => {
            warn!("Failed to parse Pump.fun create instruction: {}", e);
            None
        }
    }
}

/// Parse the fields from a Pump.fun create instruction (data after discriminator).
fn parse_create_fields(data: &[u8]) -> Result<TokenInfo> {
    let mut offset = 0;

    // Parse name
    let (name, consumed) = parser::extract_length_prefixed_string(data, offset)?;
    offset += consumed;

    // Parse symbol
    let (symbol, consumed) = parser::extract_length_prefixed_string(data, offset)?;
    offset += consumed;

    // Parse metadata URI
    let (uri, _consumed) = parser::extract_length_prefixed_string(data, offset)?;

    info!(
        name = %name,
        symbol = %symbol,
        uri = %uri,
        "Detected Pump.fun token creation"
    );

    Ok(TokenInfo {
        mint: String::new(),
        name,
        symbol,
        source: TokenSource::PumpFun,
        creator: String::new(),
        initial_liquidity_sol: 0.0,
        initial_liquidity_usd: 0.0,
        pool_address: None,
        metadata_uri: Some(uri),
        decimals: 6,
        detected_at: Utc::now(),
        backend: DetectionBackend::Helius,
        market_cap_sol: 0.0,
        v_sol_in_bonding_curve: 0.0,
        initial_buy_sol: 0.0,
    })
}

/// Known program addresses that should NOT be treated as creator.
const KNOWN_PROGRAMS: &[&str] = &[
    "11111111111111111111111111111111",
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
    "TSLvdd1pWpHVjahSpsvCXUbgwsL3JAcvokwaKt1eokM",
    "MAyhSmzXzV1pTf7LsNkrNwkWKTo4ougAJ1PPg47MD4e",
    PUMPFUN_PROGRAM_ID,
];

/// Process a raw transaction and attempt to parse it as a Pump.fun create event.
/// Populates mint and creator from the transaction's account list.
pub fn process_pumpfun_transaction(tx: &RawTransaction) -> Option<TokenInfo> {
    let mut token_info = parse_pumpfun_create(&tx.data)?;

    // Account[0] = mint address
    if let Some(mint) = tx.accounts.first() {
        token_info.mint = mint.clone();
    }

    // Creator is at account[5] in the new layout, but may vary.
    // Try [5] first, then scan for first non-program address after index 3.
    let creator = tx
        .accounts
        .get(5)
        .filter(|a| !KNOWN_PROGRAMS.contains(&a.as_str()))
        .or_else(|| {
            tx.accounts
                .iter()
                .skip(3)
                .find(|a| !KNOWN_PROGRAMS.contains(&a.as_str()))
        })
        .cloned()
        .unwrap_or_default();
    token_info.creator = creator;

    info!(
        mint = %token_info.mint,
        creator = %token_info.creator,
        name = %token_info.name,
        symbol = %token_info.symbol,
        "Pump.fun token creation detected"
    );

    Some(token_info)
}
