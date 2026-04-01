use anyhow::{Context, Result};
use chrono::Utc;
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

/// How many seconds a token must be younger than to try PumpFun direct
/// as a fallback (tokens this new may not be indexed by Jupiter yet).
const PUMPFUN_FALLBACK_AGE_SECS: i64 = 30;

/// Find the best route for buying a token.
///
/// Strategy:
/// 1. Always try Jupiter first (it aggregates all DEXes including Raydium and PumpFun).
/// 2. If the token is a PumpFun token AND Jupiter returns no route (e.g. token is
///    too new to be indexed), fall back to PumpFun direct bonding curve buy.
///
/// # Arguments
/// * `token` - The token to buy
/// * `amount_sol` - Amount of SOL to spend in lamports
/// * `slippage_bps` - Slippage tolerance in basis points
/// * `jupiter` - Jupiter client for getting quotes
/// * `rpc` - Solana RPC client (used for PumpFun fallback)
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

    // 1. Try Jupiter aggregator (covers Raydium, PumpFun, and all other DEXes)
    match get_jupiter_quote(jupiter, &token.mint, amount_sol, slippage_bps).await {
        Ok(candidate) => {
            debug!(
                output = candidate.expected_output,
                "Jupiter quote received"
            );

            let min_output = calculate_min_output(candidate.expected_output, slippage_bps);

            let route = Route {
                route_type: candidate.route_type,
                expected_output: candidate.expected_output,
                min_output,
                price_impact_pct: candidate.price_impact,
                estimated_fee_lamports: 5000,
            };

            info!(
                route = ?route.route_type,
                expected_output = route.expected_output,
                min_output = route.min_output,
                price_impact = route.price_impact_pct,
                "Best route selected via Jupiter"
            );

            return Ok(route);
        }
        Err(e) => {
            warn!("Jupiter quote failed: {e}");
        }
    }

    // 2. If Jupiter had no route and token is from PumpFun and very new, try direct bonding curve
    let token_age_secs = Utc::now()
        .signed_duration_since(token.detected_at)
        .num_seconds();

    if matches!(token.source, TokenSource::PumpFun) && token_age_secs < PUMPFUN_FALLBACK_AGE_SECS {
        info!(
            mint = %token.mint,
            age_secs = token_age_secs,
            "Token is very new PumpFun token, trying direct bonding curve"
        );

        match get_pumpfun_quote(token, amount_sol).await {
            Ok(candidate) => {
                debug!(
                    output = candidate.expected_output,
                    "PumpFun direct quote received"
                );

                let min_output = calculate_min_output(candidate.expected_output, slippage_bps);

                let route = Route {
                    route_type: candidate.route_type,
                    expected_output: candidate.expected_output,
                    min_output,
                    price_impact_pct: candidate.price_impact,
                    estimated_fee_lamports: 5000,
                };

                info!(
                    route = ?route.route_type,
                    expected_output = route.expected_output,
                    min_output = route.min_output,
                    price_impact = route.price_impact_pct,
                    "Fallback route selected via PumpFun direct"
                );

                return Ok(route);
            }
            Err(e) => {
                warn!("PumpFun direct quote also failed: {e}");
            }
        }
    }

    anyhow::bail!(
        "No routes available for token {} ({})",
        token.symbol,
        token.mint
    );
}

/// Get a quote from PumpFun bonding curve (direct on-chain calculation).
///
/// Uses the bonding curve math from the pumpfun_buy module. The virtual
/// reserves are derived from the initial liquidity, which is a rough
/// approximation for very new tokens before Jupiter indexes them.
async fn get_pumpfun_quote(token: &TokenInfo, amount_sol: u64) -> Result<RouteCandidate> {
    let virtual_sol_reserves = (token.initial_liquidity_sol * 1_000_000_000.0) as u64;
    let virtual_token_reserves: u64 = 1_000_000_000_000_000; // 1B tokens with 6 decimals

    if virtual_sol_reserves == 0 {
        anyhow::bail!("Zero virtual SOL reserves for PumpFun token");
    }

    let expected_output = super::pumpfun_buy::calculate_bonding_curve_price(
        amount_sol,
        virtual_sol_reserves,
        virtual_token_reserves,
    );

    let price_impact = (amount_sol as f64 / virtual_sol_reserves as f64) * 100.0;

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

/// Calculate minimum output with slippage tolerance.
fn calculate_min_output(expected: u64, slippage_bps: u16) -> u64 {
    let factor = 10_000u64 - slippage_bps as u64;
    (expected as u128 * factor as u128 / 10_000) as u64
}
