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
use crate::executor::pumpfun_buy::derive_bonding_curve;
use crate::executor::raydium::{load_pool_meta, pool_price_per_base_unit, read_reserves};
use crate::models::Position;

#[derive(Deserialize)]
struct JupiterQuoteResponse {
    #[serde(rename = "outAmount")]
    out_amount: String,
}

/// Where the price was sourced from.
#[derive(Debug, Clone, PartialEq)]
pub enum PriceSource {
    Jupiter,
    PumpFunBondingCurve,
    RaydiumPool,
}

impl PriceSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            PriceSource::Jupiter => "Jupiter",
            PriceSource::PumpFunBondingCurve => "PumpFunBondingCurve",
            PriceSource::RaydiumPool => "RaydiumPool",
        }
    }
}

/// A price update sent to TP/SL monitor.
/// `stale = true` signals that price feed could not get fresh data —
/// TP/SL should hold off aggressive triggers but still honor SL safety net.
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub mint: String,
    pub price_sol: f64,
    pub source: PriceSource,
    pub stale: bool,
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
        if let Some(&d) = self.decimals_cache.lock().unwrap().get(mint) {
            return Ok(d);
        }

        let pubkey = Pubkey::from_str(mint).context("Invalid mint pubkey")?;

        let decimals = match self.rpc.get_account(&pubkey) {
            Ok(account) => match Mint::unpack(&account.data) {
                Ok(mint_state) => mint_state.decimals,
                Err(_) => {
                    if account.data.len() >= 45 {
                        let d = account.data[44];
                        info!(mint = %mint, decimals = d, "Decimals from raw data (Token-2022)");
                        d
                    } else {
                        info!(mint = %mint, "Using default 6 decimals (unpack failed)");
                        6
                    }
                }
            },
            Err(_) => {
                info!(mint = %mint, "Mint account fetch failed, using 6 decimals");
                6
            }
        };

        self.decimals_cache
            .lock()
            .unwrap()
            .insert(mint.to_string(), decimals);

        Ok(decimals)
    }

    pub async fn run(
        self,
        positions: PositionManager,
        price_tx: mpsc::Sender<PriceUpdate>,
    ) -> Result<()> {
        info!(
            poll_interval_secs = self.poll_interval.as_secs(),
            "Starting price feed (multi-source dispatcher)"
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
                let update = self.get_price_for_position(position).await;
                if let Err(e) = price_tx.send(update).await {
                    warn!(error = %e, "Failed to send price update, receiver dropped");
                    return Ok(());
                }
            }
        }
    }

    /// Source-aware price dispatcher. Returns a PriceUpdate (possibly stale).
    async fn get_price_for_position(&self, pos: &Position) -> PriceUpdate {
        let source_hint = pos.price_source.as_deref().unwrap_or("Jupiter");
        let mint = pos.token_mint.clone();

        // Try the preferred source first
        let primary = match source_hint {
            "PumpFunBondingCurve" => self.get_pumpfun_price(pos),
            "RaydiumPool" => self.get_raydium_pool_price(pos),
            _ => self.get_jupiter_price(pos).await,
        };

        if let Ok(update) = primary {
            return update;
        }

        // Fallback chain: try Jupiter as last resort for any source
        if source_hint != "Jupiter" {
            if let Ok(update) = self.get_jupiter_price(pos).await {
                debug!(mint = %mint, "Fallback to Jupiter succeeded");
                return update;
            }
        }

        // All sources failed → return stale update with last known price
        warn!(
            mint = %mint,
            source = %source_hint,
            "All price sources failed — returning stale"
        );
        PriceUpdate {
            mint,
            price_sol: pos.current_price_sol,
            source: PriceSource::Jupiter,
            stale: true,
        }
    }

    /// Read PumpFun bonding curve account directly and compute price per base unit.
    /// Layout: [discriminator(8) | virtual_token_reserves(u64) | virtual_sol_reserves(u64) | ...]
    fn get_pumpfun_price(&self, pos: &Position) -> Result<PriceUpdate> {
        let mint_pubkey = Pubkey::from_str(&pos.token_mint).context("Invalid mint pubkey")?;
        let (bc_pda, _) = derive_bonding_curve(&mint_pubkey)?;

        let account = self
            .rpc
            .get_account(&bc_pda)
            .context("Failed to fetch bonding curve account")?;

        if account.data.len() < 24 {
            anyhow::bail!("Bonding curve account data too short ({})", account.data.len());
        }

        // Skip 8-byte discriminator, read 2x u64
        let virtual_token_reserves = u64::from_le_bytes(
            account.data[8..16]
                .try_into()
                .context("Failed to read vTokenReserves")?,
        );
        let virtual_sol_reserves = u64::from_le_bytes(
            account.data[16..24]
                .try_into()
                .context("Failed to read vSolReserves")?,
        );

        if virtual_token_reserves == 0 || virtual_sol_reserves == 0 {
            anyhow::bail!("Bonding curve has zero reserves (token migrated?)");
        }

        // Price per base unit (consistent with Jupiter probe approach):
        //   sol_per_token_base_unit = (virtual_sol_reserves / 1e9) / virtual_token_reserves
        let price_per_base_unit =
            (virtual_sol_reserves as f64 / 1_000_000_000.0) / virtual_token_reserves as f64;

        debug!(
            mint = %pos.token_mint,
            v_sol = virtual_sol_reserves,
            v_token = virtual_token_reserves,
            price = price_per_base_unit,
            "PumpFun bonding curve price"
        );

        Ok(PriceUpdate {
            mint: pos.token_mint.clone(),
            price_sol: price_per_base_unit,
            source: PriceSource::PumpFunBondingCurve,
            stale: false,
        })
    }

    /// Read a Raydium pool's vault reserves and compute price (Sprint 3a).
    /// Currently supports CPMM only. AMM v4 falls back to Jupiter.
    fn get_raydium_pool_price(&self, pos: &Position) -> Result<PriceUpdate> {
        let pool_address = pos
            .pool_address
            .as_ref()
            .context("Position has no pool_address — cannot read Raydium pool")?;

        let meta = load_pool_meta(&self.rpc, pool_address)
            .context("Failed to load Raydium pool metadata")?;

        // Verify mint match (defensive — pool could have been mis-stored)
        if meta.token_mint.to_string() != pos.token_mint {
            anyhow::bail!(
                "Pool token mint {} does not match position mint {}",
                meta.token_mint, pos.token_mint
            );
        }

        let (sol_reserves, token_reserves) = read_reserves(&self.rpc, &meta)?;
        let price = pool_price_per_base_unit(sol_reserves, token_reserves)?;

        debug!(
            mint = %pos.token_mint,
            pool = %meta.pool,
            sol_reserves,
            token_reserves,
            price,
            "Raydium pool price"
        );

        Ok(PriceUpdate {
            mint: pos.token_mint.clone(),
            price_sol: price,
            source: PriceSource::RaydiumPool,
            stale: false,
        })
    }

    /// Jupiter price fetcher (existing logic, wrapped to return PriceUpdate).
    async fn get_jupiter_price(&self, pos: &Position) -> Result<PriceUpdate> {
        let mint = &pos.token_mint;
        let probe_amount = 10u64.pow(self.get_decimals(mint).unwrap_or(6) as u32);

        let price = get_token_price(
            &self.http,
            &self.jupiter_api_url,
            &self.jupiter_api_key,
            mint,
            probe_amount,
        )
        .await?;

        Ok(PriceUpdate {
            mint: mint.clone(),
            price_sol: price,
            source: PriceSource::Jupiter,
            stale: false,
        })
    }
}

/// Fetch a single token's current price in SOL via Jupiter Quote API.
///
/// Sells `probe_amount` base units of the token (= 1 whole token, i.e.
/// `10^decimals`) for SOL and derives the price **per single base unit**.
/// This matches `entry_price_sol` in positions (= `amount_sol / output_base_units`).
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

    let price_per_token = (out_amount_lamports / 1_000_000_000.0) / probe_amount as f64;

    Ok(price_per_token)
}
