// Raydium pool reader + direct swap builder.
//
// Sprint 3a (read-only): pool meta decoder + price reader for CPMM
// Sprint 3b (offensive): direct swap_base_input instruction builder for CPMM
//
// IMPORTANT — Sprint 3b safety:
//   The CPMM swap builder is GATED behind ENABLE_RAYDIUM_DIRECT env var.
//   Default OFF. Test manually with small position before enabling for real.
//
// Layout sources:
//   - Pool state offsets verified against Raydium CPMM source code
//   - Discriminators computed from sha256("global:swap_base_input")[..8]
//   - Authority PDA seed b"vault_and_lp_mint_auth_seed"

use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use tracing::{debug, warn};

/// WSOL mint.
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Raydium CPMM program ID.
pub const CPMM_PROGRAM_ID: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";

/// Authority PDA seed used by Raydium CPMM for vault & LP mint authority.
const CPMM_AUTHORITY_SEED: &[u8] = b"vault_and_lp_mint_auth_seed";

/// Anchor discriminator for `swap_base_input` instruction.
/// sha256("global:swap_base_input")[..8] = [143, 190, 90, 218, 196, 30, 51, 222]
const SWAP_BASE_INPUT_DISCRIMINATOR: [u8; 8] = [143, 190, 90, 218, 196, 30, 51, 222];

/// Raydium CPMM Anchor pool state layout (well-known offsets):
///
/// ```text
/// 0..8     discriminator
/// 8..40    amm_config: Pubkey
/// 40..72   pool_creator: Pubkey
/// 72..104  token_0_vault: Pubkey
/// 104..136 token_1_vault: Pubkey
/// 136..168 lp_mint: Pubkey
/// 168..200 token_0_mint: Pubkey
/// 200..232 token_1_mint: Pubkey
/// 232..264 token_0_program: Pubkey
/// 264..296 token_1_program: Pubkey
/// 296..304 observation_key: Pubkey (start)
/// ...
/// ```
const CPMM_AMM_CONFIG_OFFSET: usize = 8;
const CPMM_TOKEN_0_VAULT_OFFSET: usize = 72;
const CPMM_TOKEN_1_VAULT_OFFSET: usize = 104;
const CPMM_TOKEN_0_MINT_OFFSET: usize = 168;
const CPMM_TOKEN_1_MINT_OFFSET: usize = 200;
const CPMM_TOKEN_0_PROGRAM_OFFSET: usize = 232;
const CPMM_TOKEN_1_PROGRAM_OFFSET: usize = 264;
const CPMM_OBSERVATION_KEY_OFFSET: usize = 296;

/// Raydium AMM v4 (legacy AmmInfo) layout — C-style packed, NOT Anchor.
/// Pool state account size is ~752 bytes. Key offsets for read-only access:
///
/// ```text
/// 0..8     status (u64)
/// 8..16    nonce (u64)
/// ...
/// 336..368  coin_vault (Pubkey)       — base token vault
/// 368..400  pc_vault (Pubkey)         — quote token vault (usually WSOL)
/// 400..432  coin_vault_mint (Pubkey)  — base mint
/// 432..464  pc_vault_mint (Pubkey)    — quote mint
/// ```
const AMM_V4_COIN_VAULT_OFFSET: usize = 336;
const AMM_V4_PC_VAULT_OFFSET: usize = 368;
const AMM_V4_COIN_MINT_OFFSET: usize = 400;
const AMM_V4_PC_MINT_OFFSET: usize = 432;
const AMM_V4_MIN_DATA_LEN: usize = 752;

/// SPL Token classic program (used by AMM v4 — predates Token-2022).
const SPL_TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

#[derive(Debug, Clone, PartialEq)]
pub enum RaydiumPoolKind {
    /// Constant Product Market Maker — Anchor program. Used by PumpSwap migrations.
    Cpmm,
    /// Legacy AMM v4 — not yet implemented for direct reading.
    AmmV4,
}

/// Decoded Raydium pool metadata: who's who in the vault layout.
/// Sprint 3a populated only the basic fields (pool, kind, mints, vaults).
/// Sprint 3b adds amm_config, observation_state, token programs for swap building.
#[derive(Debug, Clone)]
pub struct RaydiumPoolMeta {
    pub pool: Pubkey,
    pub kind: RaydiumPoolKind,
    /// The non-WSOL token mint
    pub token_mint: Pubkey,
    /// The non-WSOL vault (holds the project token)
    pub token_vault: Pubkey,
    /// The WSOL vault (holds wrapped SOL)
    pub sol_vault: Pubkey,
    // ── Sprint 3b: extra context for swap instruction building ──
    pub amm_config: Pubkey,
    pub observation_state: Pubkey,
    /// Token program for the project token (Token or Token-2022)
    pub token_program: Pubkey,
    /// Token program for WSOL (always classic Token)
    pub sol_program: Pubkey,
}

/// Read pool state and decode into RaydiumPoolMeta.
/// Currently supports CPMM only. Returns error for unsupported layouts.
pub fn load_pool_meta(rpc: &RpcClient, pool_address: &str) -> Result<RaydiumPoolMeta> {
    let pool_pubkey = Pubkey::from_str(pool_address)
        .with_context(|| format!("Invalid pool pubkey: {}", pool_address))?;

    let account = rpc
        .get_account(&pool_pubkey)
        .context("Failed to fetch pool account")?;

    // Heuristic: CPMM accounts are owned by the CPMM program ID.
    let cpmm_program: Pubkey = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C"
        .parse()
        .unwrap();
    let amm_v4_program: Pubkey = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"
        .parse()
        .unwrap();

    if account.owner == cpmm_program {
        decode_cpmm_pool(&pool_pubkey, &account.data)
    } else if account.owner == amm_v4_program {
        decode_amm_v4_pool(&pool_pubkey, &account.data)
    } else {
        anyhow::bail!(
            "Pool {} owned by unknown program {}",
            pool_address,
            account.owner
        )
    }
}

/// Decode a Raydium AMM v4 pool state. Read-only.
/// Note: AMM v4 swap building is NOT supported here — it requires Serum/OpenBook
/// market accounts (asks, bids, event_queue, vault_signer, etc.) which is a
/// large surface area. Use Jupiter for AMM v4 swaps.
fn decode_amm_v4_pool(pool: &Pubkey, data: &[u8]) -> Result<RaydiumPoolMeta> {
    if data.len() < AMM_V4_MIN_DATA_LEN {
        anyhow::bail!(
            "AMM v4 pool data too short: {} bytes (need at least {})",
            data.len(),
            AMM_V4_MIN_DATA_LEN
        );
    }

    let coin_vault = read_pubkey(data, AMM_V4_COIN_VAULT_OFFSET)?;
    let pc_vault = read_pubkey(data, AMM_V4_PC_VAULT_OFFSET)?;
    let coin_mint = read_pubkey(data, AMM_V4_COIN_MINT_OFFSET)?;
    let pc_mint = read_pubkey(data, AMM_V4_PC_MINT_OFFSET)?;

    let wsol: Pubkey = WSOL_MINT.parse().unwrap();
    let token_program: Pubkey = SPL_TOKEN_PROGRAM_ID.parse().unwrap();

    // Determine which side is the project token (non-WSOL)
    let (token_mint, token_vault, sol_vault) = if coin_mint == wsol {
        (pc_mint, pc_vault, coin_vault)
    } else if pc_mint == wsol {
        (coin_mint, coin_vault, pc_vault)
    } else {
        anyhow::bail!(
            "AMM v4 pool {} has no WSOL side ({} / {})",
            pool, coin_mint, pc_mint
        );
    };

    debug!(
        pool = %pool,
        token_mint = %token_mint,
        token_vault = %token_vault,
        sol_vault = %sol_vault,
        "Decoded AMM v4 pool meta (read-only)"
    );

    // AMM v4 has no amm_config or observation_state in the same sense as CPMM.
    // We populate them with default Pubkey values — they're only used by the
    // CPMM swap builder, which AMM v4 must not call. Direct AMM v4 swap is
    // unsupported (Serum market accounts required).
    Ok(RaydiumPoolMeta {
        pool: *pool,
        kind: RaydiumPoolKind::AmmV4,
        token_mint,
        token_vault,
        sol_vault,
        // These fields are not meaningful for AMM v4 — placeholder defaults
        // ensure the struct can be returned. Any caller that tries to build
        // a CPMM swap with kind=AmmV4 should be guarded against this.
        amm_config: Pubkey::default(),
        observation_state: Pubkey::default(),
        token_program,
        sol_program: token_program,
    })
}

fn decode_cpmm_pool(pool: &Pubkey, data: &[u8]) -> Result<RaydiumPoolMeta> {
    if data.len() < CPMM_OBSERVATION_KEY_OFFSET + 32 {
        anyhow::bail!(
            "CPMM pool data too short: {} bytes (need at least {})",
            data.len(),
            CPMM_OBSERVATION_KEY_OFFSET + 32
        );
    }

    let amm_config = read_pubkey(data, CPMM_AMM_CONFIG_OFFSET)?;
    let token_0_vault = read_pubkey(data, CPMM_TOKEN_0_VAULT_OFFSET)?;
    let token_1_vault = read_pubkey(data, CPMM_TOKEN_1_VAULT_OFFSET)?;
    let token_0_mint = read_pubkey(data, CPMM_TOKEN_0_MINT_OFFSET)?;
    let token_1_mint = read_pubkey(data, CPMM_TOKEN_1_MINT_OFFSET)?;
    let token_0_program = read_pubkey(data, CPMM_TOKEN_0_PROGRAM_OFFSET)?;
    let token_1_program = read_pubkey(data, CPMM_TOKEN_1_PROGRAM_OFFSET)?;
    let observation_state = read_pubkey(data, CPMM_OBSERVATION_KEY_OFFSET)?;

    let wsol: Pubkey = WSOL_MINT.parse().unwrap();

    let (token_mint, token_vault, sol_vault, token_program, sol_program) =
        if token_0_mint == wsol {
            (token_1_mint, token_1_vault, token_0_vault, token_1_program, token_0_program)
        } else if token_1_mint == wsol {
            (token_0_mint, token_0_vault, token_1_vault, token_0_program, token_1_program)
        } else {
            anyhow::bail!(
                "CPMM pool {} has no WSOL side ({} / {})",
                pool, token_0_mint, token_1_mint
            );
        };

    debug!(
        pool = %pool,
        token_mint = %token_mint,
        token_vault = %token_vault,
        sol_vault = %sol_vault,
        amm_config = %amm_config,
        observation = %observation_state,
        "Decoded CPMM pool meta"
    );

    Ok(RaydiumPoolMeta {
        pool: *pool,
        kind: RaydiumPoolKind::Cpmm,
        token_mint,
        token_vault,
        sol_vault,
        amm_config,
        observation_state,
        token_program,
        sol_program,
    })
}

fn read_pubkey(data: &[u8], offset: usize) -> Result<Pubkey> {
    if data.len() < offset + 32 {
        anyhow::bail!(
            "Cannot read pubkey at offset {}: data too short",
            offset
        );
    }
    let bytes: [u8; 32] = data[offset..offset + 32]
        .try_into()
        .context("Failed to slice pubkey bytes")?;
    Ok(Pubkey::new_from_array(bytes))
}

/// Read both vault balances and return (sol_amount_lamports, token_amount_base_units).
pub fn read_reserves(rpc: &RpcClient, meta: &RaydiumPoolMeta) -> Result<(u64, u64)> {
    let sol_balance = rpc
        .get_token_account_balance(&meta.sol_vault)
        .context("Failed to read SOL vault balance")?;
    let token_balance = rpc
        .get_token_account_balance(&meta.token_vault)
        .context("Failed to read token vault balance")?;

    let sol_amount: u64 = sol_balance
        .amount
        .parse()
        .context("Failed to parse SOL vault amount")?;
    let token_amount: u64 = token_balance
        .amount
        .parse()
        .context("Failed to parse token vault amount")?;

    Ok((sol_amount, token_amount))
}

/// Compute price per base unit of token, in SOL.
/// Formula: price = (sol_reserves / 1e9) / token_reserves
/// (consistent with PriceFeed and Position entry_price_sol convention)
pub fn pool_price_per_base_unit(sol_reserves: u64, token_reserves: u64) -> Result<f64> {
    if sol_reserves == 0 || token_reserves == 0 {
        anyhow::bail!("Pool has zero reserves");
    }
    Ok((sol_reserves as f64 / 1_000_000_000.0) / token_reserves as f64)
}

/// Quote: how many tokens (base units) you get for a given SOL amount?
/// Constant product formula with 0.25% Raydium fee (typical CPMM).
/// Returns expected output in token base units.
pub fn quote_buy_exact_in(sol_in_lamports: u64, sol_reserves: u64, token_reserves: u64) -> u64 {
    if sol_reserves == 0 || token_reserves == 0 || sol_in_lamports == 0 {
        return 0;
    }
    // Apply 0.25% fee on input (Raydium CPMM)
    let fee_numerator: u128 = 9_975;
    let fee_denominator: u128 = 10_000;
    let amount_in_after_fee =
        (sol_in_lamports as u128 * fee_numerator) / fee_denominator;

    // x * y = k → token_out = token_reserves - (k / (sol_reserves + amount_in))
    let k = (sol_reserves as u128) * (token_reserves as u128);
    let new_sol = sol_reserves as u128 + amount_in_after_fee;
    let new_token = k / new_sol;
    token_reserves
        .saturating_sub(new_token.try_into().unwrap_or(u64::MAX))
}

/// Quote: how much SOL (lamports) you get for selling N base units of token?
pub fn quote_sell_exact_in(token_in: u64, sol_reserves: u64, token_reserves: u64) -> u64 {
    if sol_reserves == 0 || token_reserves == 0 || token_in == 0 {
        return 0;
    }
    let fee_numerator: u128 = 9_975;
    let fee_denominator: u128 = 10_000;
    let amount_in_after_fee = (token_in as u128 * fee_numerator) / fee_denominator;

    let k = (sol_reserves as u128) * (token_reserves as u128);
    let new_token = token_reserves as u128 + amount_in_after_fee;
    let new_sol = k / new_token;
    sol_reserves
        .saturating_sub(new_sol.try_into().unwrap_or(u64::MAX))
}

#[allow(dead_code)]
pub fn warn_unsupported(pool: &str) {
    warn!(pool = %pool, "Raydium pool kind not supported by reader yet");
}

// ════════════════════════════════════════════════════════════════
// SPRINT 3b — DIRECT CPMM SWAP BUILDER (gated by ENABLE_RAYDIUM_DIRECT)
// ════════════════════════════════════════════════════════════════

/// Derive the global authority PDA for Raydium CPMM.
/// This authority owns all vaults across all CPMM pools.
pub fn derive_cpmm_authority() -> Result<(Pubkey, u8)> {
    let program_id: Pubkey = CPMM_PROGRAM_ID.parse().context("Invalid CPMM program ID")?;
    Ok(Pubkey::find_program_address(&[CPMM_AUTHORITY_SEED], &program_id))
}

/// Build a Raydium CPMM `swap_base_input` instruction.
///
/// Account order (CRITICAL — must match Raydium CPMM ABI):
///   0  payer            signer mut
///   1  authority        readonly  (global PDA)
///   2  amm_config       readonly
///   3  pool_state       mut
///   4  input_account    mut       (user's WSOL ATA for buy / token ATA for sell)
///   5  output_account   mut       (user's token ATA for buy / WSOL ATA for sell)
///   6  input_vault      mut       (pool's vault for input side)
///   7  output_vault     mut       (pool's vault for output side)
///   8  input_token_program  readonly
///   9  output_token_program readonly
///   10 input_mint       readonly
///   11 output_mint      readonly
///   12 observation_state mut
///
/// Args (after 8-byte discriminator):
///   amount_in: u64 LE
///   minimum_amount_out: u64 LE
pub fn build_cpmm_swap_ix(
    meta: &RaydiumPoolMeta,
    payer: &Pubkey,
    user_input_ata: &Pubkey,
    user_output_ata: &Pubkey,
    is_buy: bool,
    amount_in: u64,
    minimum_amount_out: u64,
) -> Result<Instruction> {
    // Guard: refuse to build CPMM swap for AMM v4 pool — different program, different layout.
    if meta.kind != RaydiumPoolKind::Cpmm {
        anyhow::bail!(
            "build_cpmm_swap_ix called with non-CPMM pool kind {:?}",
            meta.kind
        );
    }

    let program_id: Pubkey = CPMM_PROGRAM_ID.parse().context("Invalid CPMM program ID")?;
    let (authority, _bump) = derive_cpmm_authority()?;

    // For BUY: input = WSOL, output = token
    // For SELL: input = token, output = WSOL
    let (
        input_vault,
        output_vault,
        input_token_program,
        output_token_program,
        input_mint,
        output_mint,
    ) = if is_buy {
        (
            meta.sol_vault,
            meta.token_vault,
            meta.sol_program,
            meta.token_program,
            // WSOL mint
            WSOL_MINT.parse::<Pubkey>().unwrap(),
            meta.token_mint,
        )
    } else {
        (
            meta.token_vault,
            meta.sol_vault,
            meta.token_program,
            meta.sol_program,
            meta.token_mint,
            WSOL_MINT.parse::<Pubkey>().unwrap(),
        )
    };

    let accounts = vec![
        AccountMeta::new(*payer, true),                       // 0  signer mut
        AccountMeta::new_readonly(authority, false),          // 1  readonly
        AccountMeta::new_readonly(meta.amm_config, false),    // 2  readonly
        AccountMeta::new(meta.pool, false),                   // 3  mut
        AccountMeta::new(*user_input_ata, false),             // 4  mut
        AccountMeta::new(*user_output_ata, false),            // 5  mut
        AccountMeta::new(input_vault, false),                 // 6  mut
        AccountMeta::new(output_vault, false),                // 7  mut
        AccountMeta::new_readonly(input_token_program, false),// 8  readonly
        AccountMeta::new_readonly(output_token_program, false),// 9 readonly
        AccountMeta::new_readonly(input_mint, false),         // 10 readonly
        AccountMeta::new_readonly(output_mint, false),        // 11 readonly
        AccountMeta::new(meta.observation_state, false),      // 12 mut
    ];

    // Build instruction data: discriminator + amount_in + min_amount_out
    let mut data = Vec::with_capacity(8 + 8 + 8);
    data.extend_from_slice(&SWAP_BASE_INPUT_DISCRIMINATOR);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_amount_out.to_le_bytes());

    Ok(Instruction {
        program_id,
        accounts,
        data,
    })
}

/// Build a complete Raydium CPMM buy instruction sequence:
///   1. Create user's token ATA (idempotent)
///   2. Create user's WSOL ATA (idempotent)
///   3. Transfer SOL → WSOL ATA
///   4. sync_native to reflect lamport balance as WSOL
///   5. swap_base_input (WSOL → token)
///   6. Close WSOL ATA → unwrap any leftover SOL
pub fn build_raydium_buy_instructions(
    meta: &RaydiumPoolMeta,
    payer: &Pubkey,
    amount_sol_lamports: u64,
    minimum_token_out: u64,
) -> Result<Vec<Instruction>> {
    use solana_sdk::system_instruction;

    let wsol_mint: Pubkey = WSOL_MINT.parse().unwrap();
    let token_program: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse().unwrap();

    // Derive user ATAs
    let user_wsol_ata = spl_associated_token_account::get_associated_token_address(payer, &wsol_mint);
    let user_token_ata =
        spl_associated_token_account::get_associated_token_address_with_program_id(
            payer, &meta.token_mint, &meta.token_program,
        );

    let mut ixs = Vec::new();

    // 1. Create user token ATA (use the right token program — handles Token-2022)
    ixs.push(
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            payer, payer, &meta.token_mint, &meta.token_program,
        ),
    );

    // 2. Create user WSOL ATA (always classic Token program)
    ixs.push(
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            payer, payer, &wsol_mint, &token_program,
        ),
    );

    // 3. Transfer SOL → WSOL ATA
    ixs.push(system_instruction::transfer(payer, &user_wsol_ata, amount_sol_lamports));

    // 4. sync_native — instruction discriminator 17 in spl-token (single byte)
    ixs.push(spl_token::instruction::sync_native(
        &token_program,
        &user_wsol_ata,
    ).context("Failed to build sync_native")?);

    // 5. The actual swap
    ixs.push(build_cpmm_swap_ix(
        meta,
        payer,
        &user_wsol_ata,
        &user_token_ata,
        true, // is_buy
        amount_sol_lamports,
        minimum_token_out,
    )?);

    // 6. Close WSOL ATA — unwrap any leftover SOL back to wallet
    ixs.push(spl_token::instruction::close_account(
        &token_program,
        &user_wsol_ata,
        payer,
        payer,
        &[],
    ).context("Failed to build close_account for WSOL")?);

    Ok(ixs)
}

/// Build a complete Raydium CPMM sell instruction sequence:
///   1. Create user's WSOL ATA (idempotent — receives output)
///   2. swap_base_input (token → WSOL)
///   3. Close WSOL ATA → unwrap to SOL
pub fn build_raydium_sell_instructions(
    meta: &RaydiumPoolMeta,
    payer: &Pubkey,
    token_amount: u64,
    minimum_sol_out_lamports: u64,
) -> Result<Vec<Instruction>> {
    let wsol_mint: Pubkey = WSOL_MINT.parse().unwrap();
    let token_program: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse().unwrap();

    let user_wsol_ata = spl_associated_token_account::get_associated_token_address(payer, &wsol_mint);
    let user_token_ata =
        spl_associated_token_account::get_associated_token_address_with_program_id(
            payer, &meta.token_mint, &meta.token_program,
        );

    let mut ixs = Vec::new();

    // 1. Ensure WSOL ATA exists (output destination)
    ixs.push(
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            payer, payer, &wsol_mint, &token_program,
        ),
    );

    // 2. Swap token → WSOL
    ixs.push(build_cpmm_swap_ix(
        meta,
        payer,
        &user_token_ata,
        &user_wsol_ata,
        false, // is_buy = false
        token_amount,
        minimum_sol_out_lamports,
    )?);

    // 3. Unwrap WSOL → SOL (close ATA, returns lamports to payer)
    ixs.push(spl_token::instruction::close_account(
        &token_program,
        &user_wsol_ata,
        payer,
        payer,
        &[],
    ).context("Failed to build close_account for WSOL")?);

    Ok(ixs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quote_buy_constant_product() {
        // 100 SOL : 1_000_000 tokens (price = 0.0001 SOL per token base unit)
        let sol_reserves: u64 = 100_000_000_000; // 100 SOL in lamports
        let token_reserves: u64 = 1_000_000;

        let out = quote_buy_exact_in(1_000_000_000, sol_reserves, token_reserves); // buy with 1 SOL
        // Expected ~ 1_000_000 - (k / (101 SOL after fee))
        // Should be a few thousand tokens out, much less than reserves
        assert!(out > 0);
        assert!(out < token_reserves);
    }

    #[test]
    fn test_quote_sell_round_trip() {
        let sol_reserves: u64 = 100_000_000_000;
        let token_reserves: u64 = 1_000_000;

        let bought = quote_buy_exact_in(1_000_000_000, sol_reserves, token_reserves);
        // After buy, reserves change. Selling back should yield slightly less than 1 SOL due to fees.
        let new_sol = sol_reserves + 1_000_000_000;
        let new_token = token_reserves - bought;
        let sold = quote_sell_exact_in(bought, new_sol, new_token);
        assert!(sold < 1_000_000_000);  // less than 1 SOL due to fees
        assert!(sold > 990_000_000);    // but not by much
    }

    #[test]
    fn test_pool_price_per_base_unit() {
        let price = pool_price_per_base_unit(100_000_000_000, 1_000_000).unwrap();
        // 100 SOL / 1M base units = 1e-4 SOL per base unit
        assert!((price - 0.0001).abs() < 1e-9);
    }

    #[test]
    fn test_cpmm_swap_rejects_amm_v4_meta() {
        use solana_sdk::signature::Keypair;
        use solana_sdk::signer::Signer;

        let kp = Keypair::new();
        let payer = kp.pubkey();

        // Build a fake AMM v4 meta — must be rejected by CPMM swap builder
        let meta = RaydiumPoolMeta {
            pool: Pubkey::new_unique(),
            kind: RaydiumPoolKind::AmmV4,
            token_mint: Pubkey::new_unique(),
            token_vault: Pubkey::new_unique(),
            sol_vault: Pubkey::new_unique(),
            amm_config: Pubkey::default(),
            observation_state: Pubkey::default(),
            token_program: SPL_TOKEN_PROGRAM_ID.parse().unwrap(),
            sol_program: SPL_TOKEN_PROGRAM_ID.parse().unwrap(),
        };

        let result = build_cpmm_swap_ix(
            &meta, &payer, &Pubkey::new_unique(), &Pubkey::new_unique(),
            true, 1000, 100,
        );
        assert!(result.is_err(), "CPMM builder must reject AMM v4 meta");
    }

    #[test]
    fn test_cpmm_authority_pda_derives() {
        let (auth, _bump) = derive_cpmm_authority().unwrap();
        // Just verify it produces a deterministic, non-default pubkey
        assert_ne!(auth, Pubkey::default());
    }
}
