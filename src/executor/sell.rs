use anyhow::{Context, Result};
use solana_sdk::{
    instruction::Instruction,
    pubkey::Pubkey,
};
use solana_client::rpc_client::RpcClient;
use tracing::{info, warn};

use super::jupiter::JupiterClient;

/// SOL wrapped mint address.
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// SPL Token program.
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// Associated Token Account program.
const ATA_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

/// Build sell instructions for a token position.
///
/// Supports partial sells at common percentages (25%, 50%, 75%, 100%).
/// Queries the token account balance, calculates the sell amount,
/// and builds a Jupiter swap instruction to sell tokens back to SOL.
///
/// # Arguments
/// * `mint` - The token mint address as a string
/// * `amount_pct` - Percentage of holdings to sell (1-100)
/// * `wallet` - The wallet's public key
/// * `rpc` - Solana RPC client for balance queries
/// * `jupiter` - Jupiter client for building swap transactions
///
/// # Returns
/// A vector of instructions for the sell transaction.
pub async fn build_sell_instruction(
    mint: &str,
    amount_pct: u8,
    wallet: &Pubkey,
    rpc: &RpcClient,
    jupiter: &JupiterClient,
) -> Result<Vec<Instruction>> {
    let mint_pubkey: Pubkey = mint.parse().context("Invalid mint address")?;
    let token_program: Pubkey = TOKEN_PROGRAM.parse()?;
    let ata_program: Pubkey = ATA_PROGRAM.parse()?;

    // Derive user's associated token account
    let (user_ata, _) = Pubkey::find_program_address(
        &[wallet.as_ref(), token_program.as_ref(), mint_pubkey.as_ref()],
        &ata_program,
    );

    // Get current token balance
    let balance = get_token_balance(rpc, &user_ata)?;

    if balance == 0 {
        warn!(mint = %mint, "Token balance is zero, cannot sell");
        anyhow::bail!("Token balance is zero for mint {mint}");
    }

    // Calculate sell amount based on percentage
    let sell_amount = calculate_sell_amount(balance, amount_pct);

    if sell_amount == 0 {
        anyhow::bail!("Calculated sell amount is zero");
    }

    info!(
        mint = %mint,
        balance = balance,
        amount_pct = amount_pct,
        sell_amount = sell_amount,
        "Building sell instruction"
    );

    // Use Jupiter to swap tokens back to SOL
    let slippage_bps = 500u16; // 5% slippage for sells

    let quote = jupiter
        .get_quote(mint, WSOL_MINT, sell_amount, slippage_bps)
        .await
        .context("Failed to get Jupiter sell quote")?;

    info!(
        out_amount = %quote.out_amount,
        price_impact = %quote.price_impact_pct,
        "Jupiter sell quote received"
    );

    let tx = jupiter
        .build_swap_tx(&quote, &wallet.to_string())
        .await
        .context("Failed to build Jupiter sell transaction")?;

    // Extract instructions from the Jupiter transaction
    let message = tx.message;
    let instructions: Vec<Instruction> = message
        .instructions
        .iter()
        .map(|ix| {
            let program_id = message.account_keys[ix.program_id_index as usize];
            let accounts: Vec<solana_sdk::instruction::AccountMeta> = ix
                .accounts
                .iter()
                .map(|&idx| {
                    let pubkey = message.account_keys[idx as usize];
                    let is_signer = message.is_signer(idx as usize);
                    let is_writable = message.is_maybe_writable(idx as usize, None);
                    if is_writable {
                        solana_sdk::instruction::AccountMeta::new(pubkey, is_signer)
                    } else {
                        solana_sdk::instruction::AccountMeta::new_readonly(pubkey, is_signer)
                    }
                })
                .collect();
            Instruction {
                program_id,
                accounts,
                data: ix.data.clone(),
            }
        })
        .collect();

    Ok(instructions)
}

/// Get the token balance for a given token account.
fn get_token_balance(rpc: &RpcClient, token_account: &Pubkey) -> Result<u64> {
    let account_data = rpc
        .get_token_account_balance(token_account)
        .context("Failed to get token account balance")?;

    let amount: u64 = account_data
        .amount
        .parse()
        .context("Failed to parse token amount")?;

    Ok(amount)
}

/// Calculate the token amount to sell based on percentage.
///
/// Common presets: 25%, 50%, 75%, 100%.
/// Any value from 1-100 is accepted.
fn calculate_sell_amount(balance: u64, pct: u8) -> u64 {
    let pct = pct.min(100) as u64;
    (balance as u128 * pct as u128 / 100) as u64
}

/// Convenience function to get common sell percentages.
pub fn get_sell_presets() -> Vec<(u8, &'static str)> {
    vec![
        (25, "25% - Take small profit"),
        (50, "50% - Take half"),
        (75, "75% - Take most"),
        (100, "100% - Sell all"),
    ]
}
