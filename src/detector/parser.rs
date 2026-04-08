use anyhow::{Context, Result};

use super::pumpfun::PUMPFUN_PROGRAM_ID;
use super::pumpswap::PUMPSWAP_PROGRAM_ID;
use super::raydium::{RAYDIUM_AMM_PROGRAM_ID, RAYDIUM_CPMM_PROGRAM_ID};

/// Target program IDs for filtering instructions.
pub const TARGET_PROGRAMS: &[&str] = &[
    PUMPFUN_PROGRAM_ID,
    RAYDIUM_AMM_PROGRAM_ID,
    RAYDIUM_CPMM_PROGRAM_ID,
    PUMPSWAP_PROGRAM_ID,
];

fn is_target_program(program_id: &str) -> bool {
    TARGET_PROGRAMS.contains(&program_id)
}

/// Raw transaction data received from gRPC, WebSocket, or other sources.
#[derive(Debug, Clone)]
pub struct RawTransaction {
    pub _signature: String,
    pub program_id: String,
    pub data: Vec<u8>,
    pub accounts: Vec<String>,
    pub _slot: u64,
}

/// Instruction data in a generic format usable by both gRPC and WS backends.
pub struct GenericInstruction {
    pub program_id_index: usize,
    pub accounts: Vec<usize>,
    pub data: Vec<u8>,
}

/// Extract RawTransactions from a parsed transaction's instructions.
/// Used by both gRPC and WebSocket detector backends.
pub fn extract_raw_transactions(
    signature: &str,
    slot: u64,
    account_keys: &[String],
    instructions: &[GenericInstruction],
    inner_instructions: &[GenericInstruction],
) -> Vec<RawTransaction> {
    let mut results = Vec::new();

    let all_instructions = instructions.iter().chain(inner_instructions.iter());

    for ix in all_instructions {
        let program_id = match account_keys.get(ix.program_id_index) {
            Some(id) => id.clone(),
            None => continue,
        };

        if !is_target_program(&program_id) {
            continue;
        }

        let instruction_accounts: Vec<String> = ix
            .accounts
            .iter()
            .filter_map(|&idx| account_keys.get(idx).cloned())
            .collect();

        results.push(RawTransaction {
            _signature: signature.to_string(),
            program_id,
            data: ix.data.clone(),
            accounts: instruction_accounts,
            _slot: slot,
        });
    }

    results
}

/// Extract a u64 value (little-endian) from a byte slice at a given offset.
pub fn extract_u64_le(data: &[u8], offset: usize) -> Result<u64> {
    if data.len() < offset + 8 {
        anyhow::bail!(
            "Data too short to extract u64 at offset {}: need {} bytes, got {}",
            offset,
            offset + 8,
            data.len()
        );
    }
    let bytes: [u8; 8] = data[offset..offset + 8]
        .try_into()
        .context("Failed to convert slice to u64")?;
    Ok(u64::from_le_bytes(bytes))
}

/// Extract a length-prefixed UTF-8 string (4-byte LE length prefix) from data at offset.
/// Returns the string and the total number of bytes consumed (4 + length).
pub fn extract_length_prefixed_string(data: &[u8], offset: usize) -> Result<(String, usize)> {
    if data.len() < offset + 4 {
        anyhow::bail!("Data too short to read string length at offset {}", offset);
    }
    let len_bytes: [u8; 4] = data[offset..offset + 4]
        .try_into()
        .context("Failed to read string length")?;
    let len = u32::from_le_bytes(len_bytes) as usize;

    if data.len() < offset + 4 + len {
        anyhow::bail!(
            "Data too short to read string of length {} at offset {}",
            len,
            offset
        );
    }

    let s = String::from_utf8(data[offset + 4..offset + 4 + len].to_vec())
        .context("Invalid UTF-8 in string field")?;

    Ok((s, 4 + len))
}

