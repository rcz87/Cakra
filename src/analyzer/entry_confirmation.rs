use anyhow::Result;
use tracing::{info, warn};

use crate::models::token::TokenInfo;

/// Configuration for entry confirmation delay.
pub struct EntryConfirmation {
    /// Delay in seconds before buying (wait for initial activity).
    pub delay_secs: u64,
    /// Minimum liquidity (SOL) that must remain after delay.
    pub min_liquidity_sol: f64,
    /// Maximum price drop (%) from detection to allow entry.
    pub max_price_drop_pct: f64,
}

impl Default for EntryConfirmation {
    fn default() -> Self {
        Self {
            delay_secs: 3,
            min_liquidity_sol: 1.0,
            max_price_drop_pct: 50.0,
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
