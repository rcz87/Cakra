use anyhow::{Context, Result};
use serde::Deserialize;

/// Raw transaction data received from gRPC or other sources.
#[derive(Debug, Clone)]
pub struct RawTransaction {
    pub signature: String,
    pub program_id: String,
    pub data: Vec<u8>,
    pub accounts: Vec<String>,
    pub slot: u64,
}

/// Token metadata fetched from a metadata URI (Metaplex standard).
#[derive(Debug, Clone, Deserialize)]
pub struct TokenMetadataJson {
    pub name: Option<String>,
    pub symbol: Option<String>,
    pub description: Option<String>,
    pub image: Option<String>,
}

/// Decode a base58-encoded string into bytes.
pub fn decode_base58(encoded: &str) -> Result<Vec<u8>> {
    bs58::decode(encoded)
        .into_vec()
        .context("Failed to decode base58 string")
}

/// Encode bytes as a base58 string.
pub fn encode_base58(data: &[u8]) -> String {
    bs58::encode(data).into_string()
}

/// Extract a 32-byte public key from a byte slice at a given offset and return as base58.
pub fn extract_pubkey(data: &[u8], offset: usize) -> Result<String> {
    if data.len() < offset + 32 {
        anyhow::bail!(
            "Data too short to extract pubkey at offset {}: need {} bytes, got {}",
            offset,
            offset + 32,
            data.len()
        );
    }
    Ok(encode_base58(&data[offset..offset + 32]))
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

/// Fetch token metadata JSON from a URI.
pub async fn fetch_token_metadata(uri: &str) -> Result<TokenMetadataJson> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let metadata: TokenMetadataJson = client
        .get(uri)
        .send()
        .await
        .context("Failed to fetch metadata URI")?
        .json()
        .await
        .context("Failed to parse metadata JSON")?;

    Ok(metadata)
}
