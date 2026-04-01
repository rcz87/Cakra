use anyhow::Result;
use chrono::Utc;
use tracing::{info, warn};

use crate::models::token::{TokenInfo, TokenSource};
use super::parser::{self, RawTransaction};

/// Pump.fun program ID on Solana mainnet.
pub const PUMPFUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

/// Anchor discriminator for the Pump.fun "create" instruction.
/// This is the first 8 bytes of SHA256("global:create").
const CREATE_DISCRIMINATOR: [u8; 8] = [0x18, 0x1e, 0xc8, 0x28, 0x05, 0x1c, 0x07, 0x77];

/// Parse a Pump.fun "create" instruction to extract token information.
///
/// Pump.fun create instruction layout (after 8-byte discriminator):
///   - name: length-prefixed string (4 bytes LE length + string bytes)
///   - symbol: length-prefixed string
///   - uri: length-prefixed string
///
/// Account layout for create:
///   [0] mint
///   [1] mint_authority
///   [2] bonding_curve
///   [3] associated_bonding_curve
///   [4] global
///   [5] mpl_token_metadata
///   [6] metadata
///   [7] user (creator)
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
        mint: String::new(), // Will be populated from accounts
        name,
        symbol,
        source: TokenSource::PumpFun,
        creator: String::new(), // Will be populated from accounts
        initial_liquidity_sol: 0.0,
        initial_liquidity_usd: 0.0,
        pool_address: None,
        metadata_uri: Some(uri),
        decimals: 6, // Pump.fun tokens use 6 decimals
        detected_at: Utc::now(),
    })
}

/// Process a raw transaction and attempt to parse it as a Pump.fun create event.
/// Populates mint and creator from the transaction's account list.
pub fn process_pumpfun_transaction(tx: &RawTransaction) -> Option<TokenInfo> {
    let mut token_info = parse_pumpfun_create(&tx.data)?;

    // Account[0] = mint address
    if let Some(mint) = tx.accounts.first() {
        token_info.mint = mint.clone();
    }

    // Account[7] = creator (user)
    if let Some(creator) = tx.accounts.get(7) {
        token_info.creator = creator.clone();
    }

    info!(
        mint = %token_info.mint,
        creator = %token_info.creator,
        name = %token_info.name,
        symbol = %token_info.symbol,
        "Pump.fun token creation detected"
    );

    Some(token_info)
}
