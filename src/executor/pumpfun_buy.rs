use anyhow::{Context, Result};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program,
    sysvar,
};
use tracing::info;

/// Pump.fun program ID
const PUMPFUN_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

/// Pump.fun global state account
const PUMPFUN_GLOBAL: &str = "4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf";

/// Pump.fun fee recipient
const PUMPFUN_FEE_RECIPIENT: &str = "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbCJ5GMKnVpump";

/// Pump.fun event authority
const PUMPFUN_EVENT_AUTHORITY: &str = "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1";

/// SPL Token program
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// Associated Token Account program
const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

/// System program rent sysvar
const RENT_SYSVAR: &str = "SysvarRent111111111111111111111111111111111";

/// Buy instruction discriminator for Pump.fun (first 8 bytes of sha256("global:buy"))
const BUY_DISCRIMINATOR: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];

/// Derive the bonding curve PDA for a given mint on Pump.fun.
pub fn derive_bonding_curve(mint: &Pubkey) -> Result<(Pubkey, u8)> {
    let program_id: Pubkey = PUMPFUN_PROGRAM.parse().context("Invalid Pump.fun program")?;

    let (pda, bump) = Pubkey::find_program_address(
        &[b"bonding-curve", mint.as_ref()],
        &program_id,
    );

    Ok((pda, bump))
}

/// Derive the associated bonding curve token account.
pub fn derive_bonding_curve_token_account(
    bonding_curve: &Pubkey,
    mint: &Pubkey,
) -> Result<Pubkey> {
    let ata_program: Pubkey = ASSOCIATED_TOKEN_PROGRAM.parse()?;
    let token_program: Pubkey = TOKEN_PROGRAM.parse()?;

    let (ata, _) = Pubkey::find_program_address(
        &[
            bonding_curve.as_ref(),
            token_program.as_ref(),
            mint.as_ref(),
        ],
        &ata_program,
    );

    Ok(ata)
}

/// Derive the buyer's associated token account for the given mint.
pub fn derive_buyer_token_account(buyer: &Pubkey, mint: &Pubkey) -> Result<Pubkey> {
    let ata_program: Pubkey = ASSOCIATED_TOKEN_PROGRAM.parse()?;
    let token_program: Pubkey = TOKEN_PROGRAM.parse()?;

    let (ata, _) = Pubkey::find_program_address(
        &[buyer.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program,
    );

    Ok(ata)
}

/// Calculate the expected token output from the bonding curve.
///
/// Pump.fun uses a constant-product bonding curve. This calculates
/// the approximate output based on the virtual reserves model.
///
/// # Arguments
/// * `amount_sol_lamports` - Amount of SOL in lamports to spend
/// * `virtual_sol_reserves` - Current virtual SOL reserves (lamports)
/// * `virtual_token_reserves` - Current virtual token reserves
///
/// # Returns
/// Expected token output amount.
pub fn calculate_bonding_curve_price(
    amount_sol_lamports: u64,
    virtual_sol_reserves: u64,
    virtual_token_reserves: u64,
) -> u64 {
    if virtual_sol_reserves == 0 {
        return 0;
    }

    // Constant product formula: x * y = k
    // tokens_out = virtual_token_reserves - (k / (virtual_sol_reserves + amount_in))
    let k = (virtual_sol_reserves as u128) * (virtual_token_reserves as u128);
    let new_sol_reserves = virtual_sol_reserves as u128 + amount_sol_lamports as u128;

    if new_sol_reserves == 0 {
        return 0;
    }

    let new_token_reserves = k / new_sol_reserves;
    let tokens_out = virtual_token_reserves as u128 - new_token_reserves;

    tokens_out as u64
}

/// Build a Pump.fun bonding curve buy instruction.
///
/// # Arguments
/// * `mint` - The token mint address
/// * `amount_sol` - Amount of SOL to spend in lamports
/// * `slippage_bps` - Slippage tolerance in basis points (e.g. 500 = 5%)
/// * `buyer` - The buyer's public key
///
/// # Returns
/// A Solana `Instruction` ready to be included in a transaction.
pub fn build_pumpfun_buy(
    mint: &Pubkey,
    amount_sol: u64,
    slippage_bps: u16,
    buyer: &Pubkey,
) -> Result<Instruction> {
    let program_id: Pubkey = PUMPFUN_PROGRAM.parse().context("Invalid Pump.fun program ID")?;
    let global: Pubkey = PUMPFUN_GLOBAL.parse().context("Invalid global account")?;
    let fee_recipient: Pubkey =
        PUMPFUN_FEE_RECIPIENT.parse().context("Invalid fee recipient")?;
    let event_authority: Pubkey =
        PUMPFUN_EVENT_AUTHORITY.parse().context("Invalid event authority")?;
    let token_program: Pubkey = TOKEN_PROGRAM.parse()?;
    let ata_program: Pubkey = ASSOCIATED_TOKEN_PROGRAM.parse()?;

    // Derive PDAs
    let (bonding_curve, _) = derive_bonding_curve(mint)?;
    let bonding_curve_ata = derive_bonding_curve_token_account(&bonding_curve, mint)?;
    let buyer_ata = derive_buyer_token_account(buyer, mint)?;

    // Calculate max SOL cost with slippage.
    // max_sol = amount_sol * (1 + slippage_bps / 10000)
    let max_sol_cost =
        amount_sol + (amount_sol as u128 * slippage_bps as u128 / 10_000) as u64;

    // We set max_token_amount to u64::MAX to let slippage be handled by max_sol_cost.
    // The actual output depends on the on-chain bonding curve state.
    let max_token_amount: u64 = u64::MAX;

    // Build instruction data:
    // [8 bytes discriminator][8 bytes token_amount][8 bytes max_sol_cost]
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&BUY_DISCRIMINATOR);
    data.extend_from_slice(&max_token_amount.to_le_bytes());
    data.extend_from_slice(&max_sol_cost.to_le_bytes());

    let accounts = vec![
        AccountMeta::new_readonly(global, false),
        AccountMeta::new(fee_recipient, false),
        AccountMeta::new_readonly(*mint, false),
        AccountMeta::new(bonding_curve, false),
        AccountMeta::new(bonding_curve_ata, false),
        AccountMeta::new(buyer_ata, false),
        AccountMeta::new(*buyer, true),
        AccountMeta::new_readonly(system_program::id(), false),
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new_readonly(sysvar::rent::id(), false),
        AccountMeta::new_readonly(event_authority, false),
        AccountMeta::new_readonly(program_id, false),
    ];

    info!(
        mint = %mint,
        bonding_curve = %bonding_curve,
        amount_sol = amount_sol,
        max_sol_cost = max_sol_cost,
        "Built Pump.fun buy instruction"
    );

    Ok(Instruction {
        program_id,
        accounts,
        data,
    })
}

/// Build a Pump.fun bonding curve sell instruction.
pub fn build_pumpfun_sell(
    mint: &Pubkey,
    token_amount: u64,
    min_sol_output: u64,
    seller: &Pubkey,
) -> Result<Instruction> {
    let program_id: Pubkey = PUMPFUN_PROGRAM.parse()?;
    let global: Pubkey = PUMPFUN_GLOBAL.parse()?;
    let fee_recipient: Pubkey = PUMPFUN_FEE_RECIPIENT.parse()?;
    let event_authority: Pubkey = PUMPFUN_EVENT_AUTHORITY.parse()?;
    let token_program: Pubkey = TOKEN_PROGRAM.parse()?;

    let (bonding_curve, _) = derive_bonding_curve(mint)?;
    let bonding_curve_ata = derive_bonding_curve_token_account(&bonding_curve, mint)?;
    let seller_ata = derive_buyer_token_account(seller, mint)?;

    // Sell discriminator: first 8 bytes of sha256("global:sell")
    let sell_discriminator: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&sell_discriminator);
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output.to_le_bytes());

    let accounts = vec![
        AccountMeta::new_readonly(global, false),
        AccountMeta::new(fee_recipient, false),
        AccountMeta::new_readonly(*mint, false),
        AccountMeta::new(bonding_curve, false),
        AccountMeta::new(bonding_curve_ata, false),
        AccountMeta::new(seller_ata, false),
        AccountMeta::new(*seller, true),
        AccountMeta::new_readonly(system_program::id(), false),
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new_readonly(event_authority, false),
        AccountMeta::new_readonly(program_id, false),
    ];

    Ok(Instruction {
        program_id,
        accounts,
        data,
    })
}
