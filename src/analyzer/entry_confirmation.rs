use anyhow::Result;
use tracing::{info, warn};

use crate::models::token::{TokenInfo, TokenSource};

/// Configuration for entry confirmation delay.
pub struct EntryConfirmation {
    /// Delay in seconds before buying (wait for initial activity).
    pub delay_secs: u64,
}

impl Default for EntryConfirmation {
    fn default() -> Self {
        Self {
            delay_secs: 3,
        }
    }
}

/// Result of the entry confirmation check.
#[derive(Debug)]
pub enum EntryDecision {
    /// Safe to enter.
    Proceed,
    /// Entry rejected with reason.
    Reject(String),
}

/// Perform pre-entry confirmation checks.
///
/// Waits a short delay after token detection, then verifies:
/// 1. Liquidity is still present (not removed by creator)
/// 2. Price hasn't dumped massively (not a pump-and-dump)
/// 3. Pool still exists on-chain
///
/// This prevents buying into tokens where the creator already rugged
/// in the seconds between detection and execution.
pub async fn confirm_entry(
    token: &TokenInfo,
    jupiter_api_url: &str,
    confirmation: &EntryConfirmation,
) -> Result<EntryDecision> {
    info!(
        mint = %token.mint,
        delay_secs = confirmation.delay_secs,
        "Entry confirmation: waiting before buy"
    );

    // Wait the configured delay
    tokio::time::sleep(tokio::time::Duration::from_secs(confirmation.delay_secs)).await;

    let http = reqwest::Client::new();

    // Check if token is still tradeable via Jupiter quote
    let quote_url = format!(
        "{}/quote?inputMint=So11111111111111111111111111111111111111112&outputMint={}&amount=100000000&slippageBps=500",
        jupiter_api_url, token.mint
    );

    let response = http
        .get(&quote_url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;

    match response {
        Ok(resp) if resp.status().is_success() => {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct QuoteCheck {
                out_amount: String,
                price_impact_pct: Option<String>,
            }

            match resp.json::<QuoteCheck>().await {
                Ok(quote) => {
                    let out_amount: u64 = quote.out_amount.parse().unwrap_or(0);
                    if out_amount == 0 {
                        return Ok(EntryDecision::Reject(
                            "Jupiter returned zero output — token may be dead".to_string(),
                        ));
                    }

                    // Check price impact
                    if let Some(impact_str) = quote.price_impact_pct {
                        let impact: f64 = impact_str.parse().unwrap_or(0.0);
                        if impact.abs() > 10.0 {
                            warn!(
                                mint = %token.mint,
                                price_impact = impact,
                                "High price impact detected"
                            );
                            return Ok(EntryDecision::Reject(format!(
                                "Price impact too high: {:.1}%",
                                impact
                            )));
                        }
                    }

                    info!(
                        mint = %token.mint,
                        out_amount = out_amount,
                        "Entry confirmation passed — token still active"
                    );
                    Ok(EntryDecision::Proceed)
                }
                Err(e) => {
                    warn!(error = %e, "Failed to parse Jupiter quote response");
                    Ok(EntryDecision::Reject(format!(
                        "Failed to verify token status: {}",
                        e
                    )))
                }
            }
        }
        Ok(resp) => {
            let status = resp.status();
            Ok(EntryDecision::Reject(format!(
                "Jupiter quote failed with status {} — token may not be tradeable",
                status
            )))
        }
        Err(e) => {
            warn!(error = %e, "Jupiter quote request failed");
            Ok(EntryDecision::Reject(format!(
                "Could not verify token: {}",
                e
            )))
        }
    }
}

/// Fast entry confirmation for snipe mode — no Jupiter dependency.
///
/// Checks:
/// 1. Token has minimum liquidity (from detection data)
/// 2. Token source is supported (PumpFun, Raydium, PumpSwap)
/// 3. Token is not too old (stale snipe = bad risk/reward)
///
/// Does NOT call any external APIs — pure local data check.
pub fn confirm_entry_fast(token: &TokenInfo) -> EntryDecision {
    // Reject if liquidity too low — min 30 SOL (~$4k) for viable snipe
    if token.initial_liquidity_sol < 30.0 {
        return EntryDecision::Reject(format!(
            "Liquidity too low for snipe: {:.2} SOL (min 30.0)",
            token.initial_liquidity_sol
        ));
    }

    // Reject if bonding curve progress < 5% (too early, high rug risk)
    // PumpFun bonding curve starts at ~30 SOL virtual, migrates at ~85 SOL.
    // 5% progress ≈ market_cap_sol barely above base.
    if token.v_sol_in_bonding_curve > 0.0 && token.market_cap_sol > 0.0 {
        let base_sol = 30.0; // PumpFun base virtual SOL
        let progress_pct = if token.v_sol_in_bonding_curve > base_sol {
            ((token.v_sol_in_bonding_curve - base_sol) / base_sol) * 100.0
        } else {
            0.0
        };
        if progress_pct < 5.0 {
            return EntryDecision::Reject(format!(
                "Bonding curve too early: {:.1}% progress (min 5%)",
                progress_pct
            ));
        }
    }

    // Reject if deployer holds > 10% of supply (via initial_buy / market_cap proxy)
    if token.market_cap_sol > 0.0 && token.initial_buy_sol > 0.0 {
        let deployer_pct = (token.initial_buy_sol / token.market_cap_sol) * 100.0;
        if deployer_pct > 10.0 {
            return EntryDecision::Reject(format!(
                "Deployer holds {:.1}% of supply (max 10%)",
                deployer_pct
            ));
        }
    }

    // Reject if token source is unknown/unsupported
    if matches!(token.source, TokenSource::Unknown) {
        return EntryDecision::Reject(
            "Unknown token source — cannot snipe safely".to_string(),
        );
    }

    // Reject if token is too old for snipe (> 60 seconds since detection)
    let age_secs = chrono::Utc::now()
        .signed_duration_since(token.detected_at)
        .num_seconds();
    if age_secs > 60 {
        return EntryDecision::Reject(format!(
            "Token too old for snipe: {}s since detection (max 60s)",
            age_secs
        ));
    }

    info!(
        mint = %token.mint,
        liquidity_sol = token.initial_liquidity_sol,
        age_secs = age_secs,
        source = %token.source,
        "Fast entry confirmation passed"
    );

    EntryDecision::Proceed
}
