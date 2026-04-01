use anyhow::{Context, Result};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use tracing::info;

/// Raydium AMM V4 program ID.
const RAYDIUM_AMM_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

/// SPL Token program ID.
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// Raydium AMM authority (PDA).
const RAYDIUM_AUTHORITY: &str = "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1";

/// Serum/OpenBook DEX program ID (used by Raydium AMM).
const SERUM_PROGRAM: &str = "srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX";

/// Raydium swap instruction discriminator (instruction index 9 = swap).
const SWAP_INSTRUCTION: u8 = 9;

/// Build a Raydium AMM V4 swap instruction.
///
/// # Arguments
/// * `pool` - The Raydium AMM pool address (amm_id)
/// * `input_mint` - The input token mint
/// * `output_mint` - The output token mint
/// * `amount_in` - Amount of input tokens (in smallest denomination)
/// * `min_amount_out` - Minimum acceptable output amount (slippage protection)
/// * `user` - The user's wallet public key
///
/// # Returns
/// A Solana `Instruction` for the Raydium swap.
pub fn build_raydium_swap(
    pool: &str,
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    amount_in: u64,
    min_amount_out: u64,
    user: &Pubkey,
) -> Result<Instruction> {
    let program_id: Pubkey = RAYDIUM_AMM_PROGRAM
        .parse()
        .context("Invalid Raydium AMM program")?;
    let token_program: Pubkey = TOKEN_PROGRAM.parse()?;
    let amm_authority: Pubkey = RAYDIUM_AUTHORITY.parse()?;
    let serum_program: Pubkey = SERUM_PROGRAM.parse()?;
    let amm_id: Pubkey = pool.parse().context("Invalid pool address")?;

    // Derive Raydium AMM PDAs from the pool address.
    // These are deterministic based on the AMM ID.
    let (amm_open_orders, _) =
        Pubkey::find_program_address(&[amm_id.as_ref(), b"open_orders"], &program_id);
    let (amm_target_orders, _) =
        Pubkey::find_program_address(&[amm_id.as_ref(), b"target_orders"], &program_id);

    // Pool coin and PC vaults are derived from the AMM
    let (pool_coin_vault, _) =
        Pubkey::find_program_address(&[amm_id.as_ref(), b"coin_vault"], &program_id);
    let (pool_pc_vault, _) =
        Pubkey::find_program_address(&[amm_id.as_ref(), b"pc_vault"], &program_id);

    // Serum market accounts (derived from AMM)
    let (serum_market, _) =
        Pubkey::find_program_address(&[amm_id.as_ref(), b"serum_market"], &program_id);
    let (serum_bids, _) =
        Pubkey::find_program_address(&[amm_id.as_ref(), b"serum_bids"], &program_id);
    let (serum_asks, _) =
        Pubkey::find_program_address(&[amm_id.as_ref(), b"serum_asks"], &program_id);
    let (serum_event_queue, _) =
        Pubkey::find_program_address(&[amm_id.as_ref(), b"serum_event_queue"], &program_id);
    let (serum_coin_vault, _) =
        Pubkey::find_program_address(&[amm_id.as_ref(), b"serum_coin_vault"], &program_id);
    let (serum_pc_vault, _) =
        Pubkey::find_program_address(&[amm_id.as_ref(), b"serum_pc_vault"], &program_id);
    let (serum_vault_signer, _) =
        Pubkey::find_program_address(&[amm_id.as_ref(), b"serum_vault_signer"], &program_id);

    // Derive user's associated token accounts
    let ata_program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse()?;
    let user_source_ata = derive_ata(user, input_mint, &token_program, &ata_program);
    let user_dest_ata = derive_ata(user, output_mint, &token_program, &ata_program);

    // Build instruction data: [1 byte instruction][8 bytes amount_in][8 bytes min_amount_out]
    let mut data = Vec::with_capacity(17);
    data.push(SWAP_INSTRUCTION);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());

    let accounts = vec![
        // Raydium AMM accounts
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new(amm_id, false),
        AccountMeta::new_readonly(amm_authority, false),
        AccountMeta::new(amm_open_orders, false),
        AccountMeta::new(amm_target_orders, false),
        AccountMeta::new(pool_coin_vault, false),
        AccountMeta::new(pool_pc_vault, false),
        // Serum/OpenBook accounts
        AccountMeta::new_readonly(serum_program, false),
        AccountMeta::new(serum_market, false),
        AccountMeta::new(serum_bids, false),
        AccountMeta::new(serum_asks, false),
        AccountMeta::new(serum_event_queue, false),
        AccountMeta::new(serum_coin_vault, false),
        AccountMeta::new(serum_pc_vault, false),
        AccountMeta::new_readonly(serum_vault_signer, false),
        // User accounts
        AccountMeta::new(user_source_ata, false),
        AccountMeta::new(user_dest_ata, false),
        AccountMeta::new_readonly(*user, true),
    ];

    info!(
        pool = %pool,
        amount_in = amount_in,
        min_amount_out = min_amount_out,
        "Built Raydium swap instruction"
    );

    Ok(Instruction {
        program_id,
        accounts,
        data,
    })
}

/// Derive an associated token account address.
fn derive_ata(
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
    ata_program: &Pubkey,
) -> Pubkey {
    let (ata, _) = Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        ata_program,
    );
    ata
}

/// Calculate minimum output amount given slippage in basis points.
pub fn calculate_min_output(expected_output: u64, slippage_bps: u16) -> u64 {
    let slippage_factor = 10_000u64 - slippage_bps as u64;
    (expected_output as u128 * slippage_factor as u128 / 10_000) as u64
}
