use anyhow::{bail, Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use tracing::info;

/// Metaplex Token Metadata program ID.
const METADATA_PROGRAM_ID: &str = "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s";

/// Derive the metadata PDA for a given mint address.
fn derive_metadata_pda(mint: &Pubkey) -> Result<Pubkey> {
    let program_id =
        Pubkey::from_str(METADATA_PROGRAM_ID).context("Invalid metadata program ID")?;
    let seeds = &[
        b"metadata".as_ref(),
        program_id.as_ref(),
        mint.as_ref(),
    ];
    let (pda, _bump) = Pubkey::find_program_address(seeds, &program_id);
    Ok(pda)
}

/// Returns `true` if the token metadata account has `is_mutable` set to `false`
/// (i.e. the metadata is immutable and cannot be changed by the update authority).
pub fn check_metadata_immutable(rpc: &RpcClient, mint: &str) -> Result<bool> {
    let mint_pubkey = Pubkey::from_str(mint).context("Invalid mint public key")?;
    let metadata_pda = derive_metadata_pda(&mint_pubkey)?;

    let account = rpc
        .get_account(&metadata_pda)
        .context("Failed to fetch metadata account")?;

    let data = &account.data;

    // Metaplex metadata v1 layout:
    //   0      : key (1 byte)
    //   1..33  : update_authority (32 bytes)
    //   33..65 : mint (32 bytes)
    //   65..   : name (4-byte length prefix + data)
    //   ...    : symbol, uri, seller_fee_basis_points, creators, ...
    //
    // The `is_mutable` flag is a single byte (bool) located after the
    // variable-length fields. We walk through the borsh-serialized data
    // to find it.

    if data.len() < 66 {
        bail!("Metadata account data too short");
    }

    // Walk past key (1), update_authority (32), mint (32) = offset 65
    let mut offset: usize = 65;

    // name: 4-byte LE length + bytes
    if offset + 4 > data.len() {
        bail!("Metadata truncated at name length");
    }
    let name_len = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .context("name length bytes")?,
    ) as usize;
    offset += 4 + name_len;

    // symbol: 4-byte LE length + bytes
    if offset + 4 > data.len() {
        bail!("Metadata truncated at symbol length");
    }
    let symbol_len = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .context("symbol length bytes")?,
    ) as usize;
    offset += 4 + symbol_len;

    // uri: 4-byte LE length + bytes
    if offset + 4 > data.len() {
        bail!("Metadata truncated at uri length");
    }
    let uri_len = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .context("uri length bytes")?,
    ) as usize;
    offset += 4 + uri_len;

    // seller_fee_basis_points: u16 (2 bytes)
    offset += 2;

    // creators: Option<Vec<Creator>>
    //   1 byte for Option tag
    if offset >= data.len() {
        bail!("Metadata truncated at creators option");
    }
    let has_creators = data[offset] != 0;
    offset += 1;

    if has_creators {
        if offset + 4 > data.len() {
            bail!("Metadata truncated at creators vec length");
        }
        let num_creators = u32::from_le_bytes(
            data[offset..offset + 4]
                .try_into()
                .context("creators vec length bytes")?,
        ) as usize;
        offset += 4;
        // Each Creator: 32 (address) + 1 (verified) + 1 (share) = 34 bytes
        offset += num_creators * 34;
    }

    // primary_sale_happened: bool (1 byte)
    offset += 1;

    // is_mutable: bool (1 byte)
    if offset >= data.len() {
        bail!("Metadata truncated at is_mutable flag");
    }

    let is_mutable = data[offset] != 0;
    let is_immutable = !is_mutable;

    info!(
        mint = %mint,
        is_mutable,
        is_immutable,
        "Metadata mutability check"
    );

    Ok(is_immutable)
}
