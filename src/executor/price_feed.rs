use anyhow::Result;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

use crate::executor::positions::PositionManager;

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
/// Queries the price of 1_000_000 base units of the token denominated in SOL
/// and returns the price per single base-unit token in SOL (lamports converted).
pub async fn get_token_price(
    http: &reqwest::Client,
    jupiter_url: &str,
    api_key: &str,
    mint: &str,
) -> Result<f64> {
    let url = format!(
        "{}/quote?inputMint={}&outputMint=So11111111111111111111111111111111111111112&amount=1000000&slippageBps=100",
        jupiter_url.trim_end_matches('/'),
        mint,
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

    // Convert lamports to SOL and derive price per single input token unit
    let price_per_token = out_amount_lamports / 1_000_000_000.0;

    Ok(price_per_token)
}
