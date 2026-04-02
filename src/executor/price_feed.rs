use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::Deserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::program_pack::Pack;
use solana_sdk::pubkey::Pubkey;
use spl_token::state::Mint;
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
    rpc: Arc<RpcClient>,
    poll_interval: Duration,
    /// Cache of mint address → token decimals to avoid repeated on-chain fetches.
    decimals_cache: Mutex<HashMap<String, u8>>,
}

impl PriceFeed {
    pub fn new(
        jupiter_api_url: &str,
        jupiter_api_key: &str,
        poll_interval_secs: u64,
        rpc: Arc<RpcClient>,
    ) -> Self {
        Self {
            jupiter_api_url: jupiter_api_url.trim_end_matches('/').to_string(),
            jupiter_api_key: jupiter_api_key.to_string(),
            http: reqwest::Client::new(),
            rpc,
            poll_interval: Duration::from_secs(if poll_interval_secs == 0 {
                3
            } else {
                poll_interval_secs
            }),
            decimals_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Fetch token decimals from on-chain mint account, using a local cache.
    fn get_decimals(&self, mint: &str) -> Result<u8> {
        // Check cache first
        if let Some(&d) = self.decimals_cache.lock().unwrap().get(mint) {
            return Ok(d);
        }

        let pubkey = Pubkey::from_str(mint).context("Invalid mint pubkey")?;
        let account = self
            .rpc
            .get_account(&pubkey)
            .with_context(|| format!("Failed to fetch mint account for {}", mint))?;
        let mint_state = Mint::unpack(&account.data)
            .with_context(|| format!("Failed to deserialize mint account for {}", mint))?;

        let decimals = mint_state.decimals;
        self.decimals_cache
            .lock()
            .unwrap()
            .insert(mint.to_string(), decimals);

        info!(mint = %mint, decimals, "Cached token decimals");
        Ok(decimals)
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

                let probe_amount = match self.get_decimals(mint) {
                    Ok(d) => 10u64.pow(d as u32),
                    Err(e) => {
                        warn!(
                            mint = %mint,
                            error = %e,
                            "Failed to fetch token decimals, skipping price poll"
                        );
                        continue;
                    }
                };

                match get_token_price(
                    &self.http,
                    &self.jupiter_api_url,
                    &self.jupiter_api_key,
                    mint,
                    probe_amount,
                )
                .await
                {
                    Ok(price) => {
                        debug!(
                            mint = %mint,
                            price = price,
                            decimals = probe_amount.trailing_zeros(),
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
/// Sells `probe_amount` base units of the token (= 1 whole token, i.e.
/// `10^decimals`) for SOL and derives the price **per single base unit**.
/// This matches `entry_price_sol` in positions (= `amount_sol / output_base_units`).
///
/// Math:
///   out_lamports = Jupiter outAmount (lamports of SOL received)
///   price_per_base_unit = (out_lamports / 1e9) / probe_amount
pub async fn get_token_price(
    http: &reqwest::Client,
    jupiter_url: &str,
    api_key: &str,
    mint: &str,
    probe_amount: u64,
) -> Result<f64> {
    let url = format!(
        "{}/quote?inputMint={}&outputMint=So11111111111111111111111111111111111111112&amount={}&slippageBps=100",
        jupiter_url.trim_end_matches('/'),
        mint,
        probe_amount,
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
    let price_per_token = (out_amount_lamports / 1_000_000_000.0) / probe_amount as f64;

    Ok(price_per_token)
}
