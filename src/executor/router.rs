use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::models::token::{TokenInfo, TokenSource};

use super::jupiter::JupiterClient;

/// The type of route selected for the swap.
#[derive(Debug, Clone, PartialEq)]
pub enum RouteType {
    PumpFun,
    Jupiter,
    Raydium,
}

/// A swap route with pricing information.
#[derive(Debug, Clone)]
pub struct Route {
    pub route_type: RouteType,
    pub expected_output: u64,
    pub min_output: u64,
    pub price_impact_pct: f64,
    pub estimated_fee_lamports: u64,
}

/// Result of comparing a single route source.
struct RouteCandidate {
    route_type: RouteType,
    expected_output: u64,
    price_impact: f64,
}

const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Find the best route for buying a token.
///
/// Compares prices from Pump.fun direct, Jupiter aggregator, and Raydium direct.
/// Returns the route with the highest expected output.
///
/// # Arguments
/// * `token` - The token to buy
/// * `amount_sol` - Amount of SOL to spend in lamports
/// * `slippage_bps` - Slippage tolerance in basis points
/// * `jupiter` - Jupiter client for getting quotes
/// * `rpc` - Solana RPC client
/// * `config` - Bot configuration
pub async fn find_best_route(
    token: &TokenInfo,
    amount_sol: u64,
    slippage_bps: u16,
    jupiter: &JupiterClient,
    _rpc: &RpcClient,
    _config: &Config,
) -> Result<Route> {
    info!(
        mint = %token.mint,
        amount_sol = amount_sol,
        source = %token.source,
        "Finding best route"
    );

    let mut candidates: Vec<RouteCandidate> = Vec::new();

    // 1. Try Pump.fun direct if token is from Pump.fun
    if matches!(token.source, TokenSource::PumpFun) {
        match get_pumpfun_quote(token, amount_sol).await {
            Ok(candidate) => {
                debug!(
                    output = candidate.expected_output,
                    "Pump.fun quote received"
                );
                candidates.push(candidate);
            }
            Err(e) => {
                warn!("Pump.fun quote failed: {e}");
            }
        }
    }

    // 2. Try Jupiter aggregator (covers most routes)
    match get_jupiter_quote(jupiter, &token.mint, amount_sol, slippage_bps).await {
        Ok(candidate) => {
            debug!(
                output = candidate.expected_output,
                "Jupiter quote received"
            );
            candidates.push(candidate);
        }
        Err(e) => {
            warn!("Jupiter quote failed: {e}");
        }
    }

    // 3. Try Raydium direct if pool address is available
    if token.pool_address.is_some() && matches!(token.source, TokenSource::Raydium) {
        match get_raydium_quote(token, amount_sol).await {
            Ok(candidate) => {
                debug!(
                    output = candidate.expected_output,
                    "Raydium quote received"
                );
                candidates.push(candidate);
            }
            Err(e) => {
                warn!("Raydium quote failed: {e}");
            }
        }
    }

    if candidates.is_empty() {
        anyhow::bail!(
            "No routes available for token {} ({})",
            token.symbol,
            token.mint
        );
    }

    // Select the candidate with the highest expected output
    let best = candidates
        .into_iter()
        .max_by_key(|c| c.expected_output)
        .expect("candidates is not empty");

    let min_output = calculate_min_output(best.expected_output, slippage_bps);

    let route = Route {
        route_type: best.route_type,
        expected_output: best.expected_output,
        min_output,
        price_impact_pct: best.price_impact,
        estimated_fee_lamports: 5000, // base fee estimate
    };

    info!(
        route = ?route.route_type,
        expected_output = route.expected_output,
        min_output = route.min_output,
        price_impact = route.price_impact_pct,
        "Best route selected"
    );

    Ok(route)
}

/// Get a quote from Pump.fun bonding curve.
async fn get_pumpfun_quote(token: &TokenInfo, amount_sol: u64) -> Result<RouteCandidate> {
    // In production this would query the on-chain bonding curve state
    // to get the virtual reserves. For now we estimate using the
    // initial liquidity as a proxy.
    let virtual_sol_reserves = (token.initial_liquidity_sol * 1_000_000_000.0) as u64;
    let virtual_token_reserves: u64 = 1_000_000_000_000_000; // 1B tokens with 6 decimals

    let expected_output = super::pumpfun_buy::calculate_bonding_curve_price(
        amount_sol,
        virtual_sol_reserves,
        virtual_token_reserves,
    );

    let price_impact = if virtual_sol_reserves > 0 {
        (amount_sol as f64 / virtual_sol_reserves as f64) * 100.0
    } else {
        100.0
    };

    Ok(RouteCandidate {
        route_type: RouteType::PumpFun,
        expected_output,
        price_impact,
    })
}

/// Get a quote from Jupiter aggregator.
async fn get_jupiter_quote(
    jupiter: &JupiterClient,
    output_mint: &str,
    amount_sol: u64,
    slippage_bps: u16,
) -> Result<RouteCandidate> {
    let quote = jupiter
        .get_quote(WSOL_MINT, output_mint, amount_sol, slippage_bps)
        .await?;

    let expected_output: u64 = quote
        .out_amount
        .parse()
        .context("Failed to parse Jupiter output amount")?;

    let price_impact: f64 = quote
        .price_impact_pct
        .parse()
        .unwrap_or(0.0);

    Ok(RouteCandidate {
        route_type: RouteType::Jupiter,
        expected_output,
        price_impact,
    })
}

/// Get a quote from Raydium AMM.
async fn get_raydium_quote(token: &TokenInfo, amount_sol: u64) -> Result<RouteCandidate> {
    // In production, this would query the on-chain Raydium pool state
    // for current reserves and calculate output using the AMM formula.
    // For now use the Raydium HTTP API as a fallback.
    let pool_address = token
        .pool_address
        .as_ref()
        .context("No pool address available")?;

    let url = format!(
        "https://api.raydium.io/v2/ammV4/{}",
        pool_address
    );

    let http = reqwest::Client::new();
    let response = http.get(&url).send().await;

    match response {
        Ok(resp) if resp.status().is_success() => {
            // Parse reserves and calculate output
            // Simplified estimate based on constant product formula
            let sol_reserves = (token.initial_liquidity_sol * 1_000_000_000.0) as u64;
            if sol_reserves == 0 {
                anyhow::bail!("Zero SOL reserves in Raydium pool");
            }

            let token_reserves: u64 = 1_000_000_000_000; // estimate
            let k = sol_reserves as u128 * token_reserves as u128;
            let new_sol = sol_reserves as u128 + amount_sol as u128;
            let new_token = k / new_sol;
            let expected_output = (token_reserves as u128 - new_token) as u64;

            let price_impact = (amount_sol as f64 / sol_reserves as f64) * 100.0;

            Ok(RouteCandidate {
                route_type: RouteType::Raydium,
                expected_output,
                price_impact,
            })
        }
        _ => {
            anyhow::bail!("Failed to fetch Raydium pool data");
        }
    }
}

/// Calculate minimum output with slippage tolerance.
fn calculate_min_output(expected: u64, slippage_bps: u16) -> u64 {
    let factor = 10_000u64 - slippage_bps as u64;
    (expected as u128 * factor as u128 / 10_000) as u64
}
