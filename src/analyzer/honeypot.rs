use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use crate::models::token::HoneypotResult;

/// Native SOL mint (wrapped SOL).
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Simulated swap amount in lamports (0.001 SOL for testing).
const SIM_AMOUNT_LAMPORTS: u64 = 1_000_000;

/// Tax threshold above which we classify as "high tax" (percent).
const HIGH_TAX_THRESHOLD: f64 = 10.0;

/// Minimal Jupiter quote response for honeypot detection.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JupiterQuote {
    #[serde(alias = "inAmount")]
    _in_amount: String,
    out_amount: String,
}

/// Simulate a buy and sell via Jupiter quotes to detect honeypot behaviour.
///
/// 1. Gets a buy quote (SOL → token) for 0.001 SOL.
/// 2. Gets a sell quote (token → SOL) using the output from step 1.
/// 3. If the buy quote fails, the token might be a honeypot or unlisted.
/// 4. If the sell quote fails, the token is a confirmed honeypot.
/// 5. Compares sell output to buy input to estimate buy_tax and sell_tax.
pub async fn simulate_honeypot(
    jupiter_api_url: &str,
    mint: &str,
) -> Result<HoneypotResult> {
    let base_url = jupiter_api_url.trim_end_matches('/');
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("Failed to build HTTP client")?;

    // --- Step 1: Buy quote (SOL -> Token) ---
    let buy_url = format!(
        "{}/quote?inputMint={}&outputMint={}&amount={}&slippageBps=500&onlyDirectRoutes=false",
        base_url, WSOL_MINT, mint, SIM_AMOUNT_LAMPORTS,
    );

    let buy_resp = http.get(&buy_url).send().await;
    let buy_quote: JupiterQuote = match buy_resp {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<JupiterQuote>().await {
                Ok(q) => q,
                Err(e) => {
                    warn!(mint = %mint, err = %e, "Failed to parse buy quote — token may be unlisted");
                    return Ok(HoneypotResult::Unknown);
                }
            }
        }
        Ok(resp) => {
            warn!(
                mint = %mint,
                status = %resp.status(),
                "Buy quote returned non-success — token may be honeypot or unlisted"
            );
            return Ok(HoneypotResult::Honeypot);
        }
        Err(e) => {
            warn!(mint = %mint, err = %e, "Buy quote request failed — treating as potential honeypot");
            return Ok(HoneypotResult::Honeypot);
        }
    };

    let tokens_received: u64 = buy_quote
        .out_amount
        .parse()
        .context("Failed to parse buy quote out_amount")?;

    if tokens_received == 0 {
        warn!(mint = %mint, "Buy quote returned 0 tokens — possible honeypot");
        return Ok(HoneypotResult::Honeypot);
    }

    // --- Step 2: Sell quote (Token -> SOL) ---
    let sell_url = format!(
        "{}/quote?inputMint={}&outputMint={}&amount={}&slippageBps=500&onlyDirectRoutes=false",
        base_url, mint, WSOL_MINT, tokens_received,
    );

    let sell_resp = http.get(&sell_url).send().await;
    let sell_quote: JupiterQuote = match sell_resp {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<JupiterQuote>().await {
                Ok(q) => q,
                Err(e) => {
                    warn!(mint = %mint, err = %e, "Failed to parse sell quote — HONEYPOT detected");
                    return Ok(HoneypotResult::Honeypot);
                }
            }
        }
        Ok(resp) => {
            warn!(
                mint = %mint,
                status = %resp.status(),
                "Sell quote returned non-success — HONEYPOT detected"
            );
            return Ok(HoneypotResult::Honeypot);
        }
        Err(e) => {
            warn!(mint = %mint, err = %e, "Sell quote request failed — HONEYPOT detected");
            return Ok(HoneypotResult::Honeypot);
        }
    };

    let sol_returned: u64 = sell_quote
        .out_amount
        .parse()
        .context("Failed to parse sell quote out_amount")?;

    // --- Step 3: Compute taxes ---
    // Round-trip estimation: buy SOL→token, then sell token→SOL.
    // The difference between input and final output reveals combined tax + fees.
    let input_lamports = SIM_AMOUNT_LAMPORTS as f64;
    let output_lamports = sol_returned as f64;
    let round_trip_loss_pct = if input_lamports > 0.0 {
        ((input_lamports - output_lamports) / input_lamports * 100.0).max(0.0)
    } else {
        0.0
    };

    // Approximate: split round-trip loss evenly as buy_tax and sell_tax.
    // A more accurate split would require a no-tax baseline, but for honeypot
    // detection the total loss is what matters.
    let buy_tax = round_trip_loss_pct / 2.0;
    let sell_tax = round_trip_loss_pct / 2.0;

    info!(
        mint = %mint,
        tokens_received,
        sol_returned,
        buy_tax,
        sell_tax,
        round_trip_loss_pct,
        "Honeypot simulation complete"
    );

    if buy_tax > HIGH_TAX_THRESHOLD || sell_tax > HIGH_TAX_THRESHOLD {
        Ok(HoneypotResult::HighTax { buy_tax, sell_tax })
    } else {
        Ok(HoneypotResult::Safe { buy_tax, sell_tax })
    }
}
