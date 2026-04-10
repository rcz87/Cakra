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
    /// PumpPortal Local Trade API + Jito bundle (for PumpFun tokens)
    /// Fastest path: PumpPortal builds tx, we sign, Jito submits.
    /// 0.5% fee but includes MEV protection + optimized routing.
    PumpPortalDirect,
    /// Legacy direct PumpFun bonding curve buy (manual instruction building)
    PumpFun,
    /// Jupiter aggregator (covers all DEXes)
    Jupiter,
}

/// A swap route with pricing information.
#[derive(Debug, Clone)]
pub struct Route {
    pub route_type: RouteType,
    pub expected_output: u64,
    pub min_output: u64,
    pub price_impact_pct: f64,
}

/// Result of comparing a single route source.
struct RouteCandidate {
    route_type: RouteType,
    expected_output: u64,
    price_impact: f64,
}

const MAX_PRICE_IMPACT_PCT: f64 = 10.0;

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
    config: &Config,
) -> Result<Route> {
    let token_age_secs = Utc::now()
        .signed_duration_since(token.detected_at)
        .num_seconds();

    info!(
        mint = %token.mint,
        amount_sol = amount_sol,
        source = %token.source,
        age_secs = token_age_secs,
        mode = %config.trading_mode,
        "Finding best route"
    );

    // For PumpFun tokens (still on bonding curve): use PumpPortal Direct.
    // PumpPortal handles tx building + Jito tip, we just sign and submit.
    if matches!(token.source, TokenSource::PumpFun) {
        info!(
            mint = %token.mint,
            age_secs = token_age_secs,
            "PumpFun token → PumpPortal Direct route (Jito bundle)"
        );

        // PumpPortal doesn't provide a quote — it builds the full tx.
        // Use bonding curve estimate for expected output tracking.
        let estimated = get_pumpfun_quote(token, amount_sol).await;
        let (expected_output, price_impact) = match estimated {
            Ok(c) => (c.expected_output, c.price_impact),
            Err(_) => (0, 0.0), // PumpPortal will handle pricing
        };

        let route = Route {
            route_type: RouteType::PumpPortalDirect,
            expected_output,
            min_output: calculate_min_output(expected_output, slippage_bps),
            price_impact_pct: price_impact,
        };

        info!(
            route = ?route.route_type,
            estimated_output = expected_output,
            "PumpPortal Direct route selected"
        );

        return Ok(route);
    }

    // For PumpSwap (migrated PumpFun on AMM pool): use PumpPortal with pool="pump-amm".
    // CRITICAL: PumpSwap is NOT bonding curve. Never fall back to get_pumpfun_quote
    // for PumpSwap — that would use wrong math (bonding curve formula on AMM pool).
    if matches!(token.source, TokenSource::PumpSwap) {
        info!(
            mint = %token.mint,
            age_secs = token_age_secs,
            "PumpSwap token (migrated AMM) → PumpPortal Direct (pool=pump-amm)"
        );

        let route = Route {
            route_type: RouteType::PumpPortalDirect,
            expected_output: 0,  // PumpPortal will price it
            min_output: 0,
            price_impact_pct: 0.0,
        };

        return Ok(route);
    }

    // Standard path: try Jupiter aggregator first
    match get_jupiter_quote(jupiter, &token.mint, amount_sol, slippage_bps).await {
        Ok(candidate) => {
            debug!(
                output = candidate.expected_output,
                "Jupiter quote received"
            );

            // Reject routes with excessive price impact
            if candidate.price_impact > MAX_PRICE_IMPACT_PCT {
                warn!(price_impact = candidate.price_impact, "Route rejected: price impact > 10%");
                // Fall through to PumpFun fallback or error
            } else if candidate.expected_output == 0 {
                warn!("Route rejected: expected output is 0");
                // Fall through to PumpFun fallback or error
            } else {
                let min_output = calculate_min_output(candidate.expected_output, slippage_bps);

                let route = Route {
                    route_type: candidate.route_type,
                    expected_output: candidate.expected_output,
                    min_output,
                    price_impact_pct: candidate.price_impact,
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
        }
        Err(e) => {
            warn!("Jupiter quote failed: {e}");
        }
    }

    // 2. PumpFun-only fallback: if Jupiter had no route AND token is still on bonding curve
    //    (PumpFun source, not PumpSwap), try direct bonding curve buy.
    //    PumpSwap is excluded — it's an AMM pool, not bonding curve. Wrong math.
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

                // Mark PumpFun routes as low-confidence estimates
                if candidate.price_impact > 20.0 {
                    warn!("PumpFun fallback has very high estimated price impact");
                }

                let min_output = calculate_min_output(candidate.expected_output, slippage_bps);

                let route = Route {
                    route_type: candidate.route_type,
                    expected_output: candidate.expected_output,
                    min_output,
                    price_impact_pct: candidate.price_impact,
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
