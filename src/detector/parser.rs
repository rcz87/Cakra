use anyhow::{Context, Result};

/// Raw transaction data received from gRPC or other sources.
#[derive(Debug, Clone)]
pub struct RawTransaction {
    pub _signature: String,
    pub program_id: String,
    pub data: Vec<u8>,
    pub accounts: Vec<String>,
    pub _slot: u64,
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

