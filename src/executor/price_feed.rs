use anyhow::Result;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

use crate::executor::positions::PositionManager;

/// Probe amount for price queries (in token base units).
/// For a 6-decimal token this equals 1.0 token; for a 9-decimal token 0.001 tokens.
const PRICE_PROBE_AMOUNT: u64 = 1_000_000;

#[derive(Deserialize)]
struct JupiterQuoteResponse {
    #[serde(rename = "outAmount")]
    out_amount: String,
}

pub struct PriceFeed {
    jupiter_api_url: String,
    jupiter_api_key: String,
    http: reqwest::Client,
    poll_interval: Duration,
}

impl PriceFeed {
    pub fn new(jupiter_api_url: &str, jupiter_api_key: &str, poll_interval_secs: u64) -> Self {
        Self {
            jupiter_api_url: jupiter_api_url.trim_end_matches('/').to_string(),
            jupiter_api_key: jupiter_api_key.to_string(),
            http: reqwest::Client::new(),
            poll_interval: Duration::from_secs(if poll_interval_secs == 0 {
                3
            } else {
                poll_interval_secs
            }),
        }
    }

    pub async fn run(
        self,
        positions: PositionManager,
        price_tx: mpsc::Sender<(String, f64)>,
    ) -> Result<()> {
        info!(
            poll_interval_secs = self.poll_interval.as_secs(),
            "Starting price feed"
        );

        let mut ticker = interval(self.poll_interval);

        loop {
            ticker.tick().await;

            let open_positions = positions.get_open_positions();
            if open_positions.is_empty() {
                debug!("No open positions, skipping price poll");
                continue;
            }

            debug!(count = open_positions.len(), "Polling prices for open positions");

            for position in &open_positions {
                let mint = &position.token_mint;

                match get_token_price(&self.http, &self.jupiter_api_url, &self.jupiter_api_key, mint).await {
                    Ok(price) => {
                        debug!(
                            mint = %mint,
                            price = price,
                            "Got price for token"
                        );
                        if let Err(e) = price_tx.send((mint.clone(), price)).await {
                            warn!(error = %e, "Failed to send price update, receiver dropped");
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        warn!(
                            mint = %mint,
                            error = %e,
                            "Failed to get price for token, skipping"
                        );
                    }
                }
            }
        }
    }
}

/// Fetch a single token's current price in SOL via Jupiter Quote API.
///
/// Sells [`PRICE_PROBE_AMOUNT`] base units of the token for SOL and derives
/// the price **per single base unit** in SOL.  This matches the unit used by
/// `entry_price_sol` in positions (= `amount_sol / actual_output_base_units`).
///
/// Math:
///   out_lamports = Jupiter outAmount (lamports of SOL received)
///   price_per_base_unit = (out_lamports / 1e9) / PRICE_PROBE_AMOUNT
pub async fn get_token_price(
    http: &reqwest::Client,
    jupiter_url: &str,
    api_key: &str,
    mint: &str,
) -> Result<f64> {
    let url = format!(
        "{}/quote?inputMint={}&outputMint=So11111111111111111111111111111111111111112&amount={}&slippageBps=100",
        jupiter_url.trim_end_matches('/'),
        mint,
        PRICE_PROBE_AMOUNT,
    );

    let mut req = http.get(&url).timeout(Duration::from_secs(5));
    if !api_key.is_empty() {
        req = req.header("x-api-key", api_key);
    }
    let resp = req
        .send()
        .await?
        .error_for_status()?
        .json::<JupiterQuoteResponse>()
        .await?;

    let out_amount_lamports: f64 = resp
        .out_amount
        .parse()
        .map_err(|e| anyhow::anyhow!("Failed to parse outAmount '{}': {}", resp.out_amount, e))?;

    // Convert lamports → SOL, then divide by probe amount to get price per
    // single base unit.  Matches entry_price_sol = amount_sol / output_base_units.
    let price_per_token = (out_amount_lamports / 1_000_000_000.0) / PRICE_PROBE_AMOUNT as f64;

    Ok(price_per_token)
}
