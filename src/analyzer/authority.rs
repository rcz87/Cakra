use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::program_pack::Pack;
use solana_sdk::pubkey::Pubkey;
use spl_token::state::Mint;
use std::str::FromStr;
use tracing::info;

/// Returns `true` if the mint authority has been renounced (set to `None`).
pub fn check_mint_authority(rpc: &RpcClient, mint: &str) -> Result<bool> {
    let mint_pubkey =
        Pubkey::from_str(mint).context("Invalid mint public key")?;
    let account = rpc
        .get_account(&mint_pubkey)
        .context("Failed to fetch mint account")?;

    let mint_state =
        Mint::unpack(&account.data).context("Failed to deserialize mint account")?;

    let renounced = mint_state.mint_authority.is_none()
        || mint_state
            .mint_authority
            .map(|a| a == Pubkey::default())
            .unwrap_or(false);

    info!(
        mint = %mint,
        renounced,
        "Mint authority check"
    );
    Ok(renounced)
}

/// Returns `true` if the freeze authority is null (set to `None`).
pub fn check_freeze_authority(rpc: &RpcClient, mint: &str) -> Result<bool> {
    let mint_pubkey =
        Pubkey::from_str(mint).context("Invalid mint public key")?;
    let account = rpc
        .get_account(&mint_pubkey)
        .context("Failed to fetch mint account")?;

    let mint_state =
        Mint::unpack(&account.data).context("Failed to deserialize mint account")?;

    let is_null = mint_state.freeze_authority.is_none()
        || mint_state
            .freeze_authority
            .map(|a| a == Pubkey::default())
            .unwrap_or(false);

    info!(
        mint = %mint,
        freeze_null = is_null,
        "Freeze authority check"
    );
    Ok(is_null)
}
