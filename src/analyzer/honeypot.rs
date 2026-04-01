use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    transaction::Transaction,
};
use std::str::FromStr;
use tracing::{info, warn};

use crate::models::token::HoneypotResult;

/// Native SOL mint (wrapped SOL).
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Raydium AMM program ID.
const RAYDIUM_AMM_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

/// Simulated swap amount in lamports (0.001 SOL for testing).
const SIM_AMOUNT_LAMPORTS: u64 = 1_000_000;

/// Tax threshold above which we classify as "high tax" (percent).
const HIGH_TAX_THRESHOLD: f64 = 10.0;

/// Simulate a buy and sell transaction to detect honeypot behaviour.
///
/// Uses the `simulateTransaction` RPC method to test whether a sell would
/// revert after a successful buy. Also estimates buy/sell tax from the
/// simulated output amounts.
pub fn simulate_honeypot(
    rpc: &RpcClient,
    mint: &str,
    pool_address: Option<&str>,
) -> Result<HoneypotResult> {
    let pool_addr = match pool_address {
        Some(p) => p,
        None => {
            warn!(mint = %mint, "No pool address provided, skipping honeypot simulation");
            return Ok(HoneypotResult::Unknown);
        }
    };

    let mint_pubkey = Pubkey::from_str(mint).context("Invalid mint pubkey")?;
    let pool_pubkey = Pubkey::from_str(pool_addr).context("Invalid pool pubkey")?;
    let wsol_pubkey = Pubkey::from_str(WSOL_MINT).context("Invalid WSOL pubkey")?;
    let amm_program = Pubkey::from_str(RAYDIUM_AMM_PROGRAM).context("Invalid AMM program ID")?;

    // Build a minimal swap instruction (buy: SOL -> Token).
    let buy_ix = build_swap_instruction(
        &amm_program,
        &pool_pubkey,
        &wsol_pubkey,
        &mint_pubkey,
        SIM_AMOUNT_LAMPORTS,
    );

    // Use a dummy payer for simulation (the transaction will not be submitted).
    let dummy_payer = Pubkey::new_unique();

    // --- Simulate BUY ---
    let buy_msg = Message::new(&[buy_ix], Some(&dummy_payer));
    let buy_tx = Transaction::new_unsigned(buy_msg);

    let buy_sim = rpc.simulate_transaction(&buy_tx).context("Buy simulation RPC call failed")?;

    if let Some(ref err) = buy_sim.value.err {
        warn!(mint = %mint, error = ?err, "Buy simulation reverted — possible honeypot");
        return Ok(HoneypotResult::Honeypot);
    }

    // Estimate tokens received from buy (from simulation logs).
    let buy_output = parse_swap_output_from_logs(&buy_sim.value.logs.unwrap_or_default());

    // --- Simulate SELL ---
    let sell_amount = buy_output.unwrap_or(SIM_AMOUNT_LAMPORTS);
    let sell_ix = build_swap_instruction(
        &amm_program,
        &pool_pubkey,
        &mint_pubkey,
        &wsol_pubkey,
        sell_amount,
    );

    let sell_msg = Message::new(&[sell_ix], Some(&dummy_payer));
    let sell_tx = Transaction::new_unsigned(sell_msg);

    let sell_sim = rpc
        .simulate_transaction(&sell_tx)
        .context("Sell simulation RPC call failed")?;

    if let Some(ref err) = sell_sim.value.err {
        warn!(mint = %mint, error = ?err, "Sell simulation reverted — HONEYPOT detected");
        return Ok(HoneypotResult::Honeypot);
    }

    let sell_output = parse_swap_output_from_logs(&sell_sim.value.logs.unwrap_or_default());

    // --- Compute taxes ---
    let buy_tax = if let Some(tokens_out) = buy_output {
        let expected = SIM_AMOUNT_LAMPORTS as f64;
        let actual = tokens_out as f64;
        if expected > 0.0 {
            ((expected - actual) / expected * 100.0).max(0.0)
        } else {
            0.0
        }
    } else {
        0.0
    };

    let sell_tax = if let (Some(tokens_in), Some(sol_out)) = (buy_output, sell_output) {
        let expected = tokens_in as f64;
        let actual = sol_out as f64;
        if expected > 0.0 {
            ((expected - actual) / expected * 100.0).max(0.0)
        } else {
            0.0
        }
    } else {
        0.0
    };

    info!(
        mint = %mint,
        buy_tax,
        sell_tax,
        "Honeypot simulation complete"
    );

    if buy_tax > HIGH_TAX_THRESHOLD || sell_tax > HIGH_TAX_THRESHOLD {
        Ok(HoneypotResult::HighTax { buy_tax, sell_tax })
    } else {
        Ok(HoneypotResult::Safe { buy_tax, sell_tax })
    }
}

/// Build a minimal Raydium-style swap instruction for simulation purposes.
fn build_swap_instruction(
    program_id: &Pubkey,
    pool: &Pubkey,
    source_mint: &Pubkey,
    dest_mint: &Pubkey,
    amount_in: u64,
) -> Instruction {
    // Instruction data: swap discriminator (9) + amount_in + min_out (0 for sim)
    let mut data = Vec::with_capacity(17);
    data.push(9u8); // swap instruction index
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes()); // minimum amount out

    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*pool, false),
            AccountMeta::new_readonly(*source_mint, false),
            AccountMeta::new_readonly(*dest_mint, false),
        ],
        data,
    }
}

/// Attempt to parse the output amount from simulation log messages.
/// Looks for a pattern like "amount_out: <number>" in the program logs.
fn parse_swap_output_from_logs(logs: &[String]) -> Option<u64> {
    for log in logs {
        // Common Raydium log format
        if let Some(idx) = log.find("amount_out:") {
            let after = &log[idx + 11..];
            let trimmed = after.trim();
            if let Some(num_str) = trimmed.split_whitespace().next() {
                if let Ok(val) = num_str.parse::<u64>() {
                    return Some(val);
                }
            }
        }
        // Alternative: look for "output_amount" or similar
        if let Some(idx) = log.find("output_amount") {
            let after = &log[idx + 13..];
            let trimmed = after.trim().trim_start_matches([':', '=', ' ']);
            if let Some(num_str) = trimmed.split_whitespace().next() {
                if let Ok(val) = num_str.parse::<u64>() {
                    return Some(val);
                }
            }
        }
    }
    None
}
