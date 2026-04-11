//! DexScreener public API client — used exclusively for migration event
//! enrichment. PumpPortal's `migrate` WS event only carries {mint, pool,
//! signature, txType}, so we need a side-channel to get real liquidity /
//! mcap / price numbers before running the migration filter.
//!
//! Endpoint: https://api.dexscreener.com/latest/dex/tokens/{mint}
//! Rate limit: 300 req/min per IP (we expect <1 req/min in practice).
//! Timeout: 3s — bounded to avoid blocking the analyzer loop.
//!
//! This module is read-only and has zero effect on trading decisions
//! unless the caller (main.rs migration branch) consumes the output.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::time::Duration;
use tracing::{debug, warn};

const BASE_URL: &str = "https://api.dexscreener.com/latest/dex/tokens";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// Subset of DexScreener pair data we actually need.
///
/// Numeric fields arrive as JSON strings ("0.000123") or numbers depending
/// on the endpoint variant, so we deserialize via `Value` + manual parse
/// for robustness instead of `#[serde(with = "...")]` gymnastics.
///
/// `market_cap_sol` is computed here from `marketCap_usd * (priceNative /
/// priceUsd)` — using both fields from the SAME DexScreener snapshot so
/// the implied SOL/USD rate is consistent with the pair's own quote,
/// eliminating staleness from any external SOL price source.
#[derive(Debug, Clone)]
pub struct EnrichedPair {
    pub pair_address: String,
    pub dex_id: String,           // "pumpswap", "raydium", etc.
    pub quote_symbol: String,     // should be "SOL" for migrations
    pub price_native: f64,        // price in SOL per base token
    pub price_usd: f64,           // price in USD per base token
    pub liquidity_usd: f64,
    pub liquidity_quote: f64,     // SOL side of the pool (when quote == SOL)
    pub market_cap_usd: f64,
    pub market_cap_sol: f64,      // derived from the pair's own implied rate
    pub fdv_usd: f64,
    pub pair_created_at_ms: u64,
}

#[derive(Debug, Deserialize)]
struct DsResponse {
    pairs: Option<Vec<serde_json::Value>>,
}

/// Fetch enrichment data for a mint. Returns the SOL-quoted pair matching
/// `preferred_dex` if available. If `preferred_dex` is set but not indexed
/// yet, returns `None` instead of falling back to an arbitrary SOL pair —
/// this prevents returning the stale pre-migration pumpfun bonding curve
/// pair (which has `liquidity: null` on DexScreener) as if it were the
/// freshly-minted AMM pool. The caller can treat "no pair" as a retry
/// signal instead of acting on wrong data.
///
/// If `preferred_dex` is empty, falls back to the highest-liquidity SOL pair
/// — this path is for non-migration lookups that have no preference.
///
/// `preferred_dex` examples: "pumpswap" (from pump-amm migration),
/// "raydium" (raydium-cpmm migration).
pub async fn fetch_enrichment(
    mint: &str,
    preferred_dex: &str,
) -> Result<Option<EnrichedPair>> {
    let http = reqwest::Client::builder()
        .timeout(DEFAULT_TIMEOUT)
        .user_agent(concat!("ricoz-sniper/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("Failed to build DexScreener HTTP client")?;

    let url = format!("{}/{}", BASE_URL, mint);
    debug!(mint, url, "DexScreener fetch");

    let resp = http.get(&url).send().await.context("DexScreener request failed")?;
    let status = resp.status();
    if !status.is_success() {
        warn!(mint, %status, "DexScreener non-2xx");
        return Ok(None);
    }

    let body: DsResponse = resp.json().await.context("DexScreener decode failed")?;
    let pairs = body.pairs.unwrap_or_default();
    if pairs.is_empty() {
        debug!(mint, "DexScreener: no pairs (token too new?)");
        return Ok(None);
    }

    // Filter: SOL-quoted pairs on Solana only.
    let mut candidates: Vec<EnrichedPair> = pairs
        .iter()
        .filter(|p| p.get("chainId").and_then(|c| c.as_str()) == Some("solana"))
        .filter_map(parse_pair)
        .filter(|p| p.quote_symbol == "SOL")
        .collect();

    if candidates.is_empty() {
        return Ok(None);
    }

    Ok(select_pair(candidates, preferred_dex))
}

/// Pick the best pair from a list of SOL-quoted candidates.
///
/// If `preferred_dex` is non-empty, returns the highest-liquidity pair whose
/// `dex_id` matches (case-insensitive), or `None` if no match exists — this
/// is the migration-enrichment path and we MUST NOT fall back to a different
/// dex, because doing so picks up stale pre-migration pumpfun pairs.
///
/// If `preferred_dex` is empty, returns the highest-liquidity pair across
/// all candidates — this is the "no preference" path for generic lookups.
fn select_pair(mut candidates: Vec<EnrichedPair>, preferred_dex: &str) -> Option<EnrichedPair> {
    // NaN-safe comparator — treat NaN as Equal to avoid panic.
    let cmp_by_liq = |a: &EnrichedPair, b: &EnrichedPair| {
        a.liquidity_usd
            .partial_cmp(&b.liquidity_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    };

    if !preferred_dex.is_empty() {
        // Strict mode: preference was specified, refuse to fall back.
        return candidates
            .into_iter()
            .filter(|p| p.dex_id.eq_ignore_ascii_case(preferred_dex))
            .max_by(cmp_by_liq);
    }

    // No preference: highest-liquidity wins.
    candidates.sort_by(|a, b| cmp_by_liq(b, a));
    candidates.into_iter().next()
}

/// Parse a DexScreener pair JSON blob into our lean struct. Returns None
/// ONLY if the truly-required fields (`pairAddress`) are missing — every
/// other field degrades gracefully to a sane default so we never panic
/// on upstream schema drift.
fn parse_pair(p: &serde_json::Value) -> Option<EnrichedPair> {
    // Required field — without an address we can't identify the pool.
    let pair_address = p.get("pairAddress")?.as_str()?.to_string();

    // Everything below is best-effort. A missing/unexpected field becomes
    // an empty string or 0.0 rather than killing the whole parse.
    let dex_id = p
        .get("dexId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let quote_symbol = p
        .get("quoteToken")
        .and_then(|q| q.get("symbol"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_uppercase();

    // Prices arrive as strings in DexScreener's public endpoint.
    let price_native = p
        .get("priceNative")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let price_usd = p
        .get("priceUsd")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);

    let liquidity_usd = p
        .get("liquidity")
        .and_then(|l| l.get("usd"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let liquidity_quote = p
        .get("liquidity")
        .and_then(|l| l.get("quote"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let market_cap_usd = p.get("marketCap").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let fdv_usd = p.get("fdv").and_then(|v| v.as_f64()).unwrap_or(0.0);

    // Market cap in SOL derived from the pair's own implied SOL/USD rate.
    // This avoids reliance on any external SOL price source — whatever
    // DexScreener uses for this pair's USD calc is exactly what we use.
    //
    // Guard against divide-by-zero and NaN propagation.
    let market_cap_sol = if price_usd > 0.0 && price_native.is_finite() && market_cap_usd.is_finite() {
        let implied_sol_per_usd = price_native / price_usd; // SOL per USD
        let m = market_cap_usd * implied_sol_per_usd;
        if m.is_finite() { m } else { 0.0 }
    } else {
        0.0
    };

    let pair_created_at_ms = p
        .get("pairCreatedAt")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    Some(EnrichedPair {
        pair_address,
        dex_id,
        quote_symbol,
        price_native,
        price_usd,
        liquidity_usd,
        liquidity_quote,
        market_cap_usd,
        market_cap_sol,
        fdv_usd,
        pair_created_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_pair_happy_path() {
        let v = json!({
            "pairAddress": "PAIR123",
            "dexId": "pumpswap",
            "chainId": "solana",
            "quoteToken": { "symbol": "SOL" },
            "priceNative": "0.0000123",
            "priceUsd": "0.00246",
            "liquidity": { "usd": 12345.67, "quote": 50.5 },
            "marketCap": 45000.0,
            "fdv": 50000.0,
            "pairCreatedAt": 1712800000000u64
        });
        let p = parse_pair(&v).unwrap();
        assert_eq!(p.pair_address, "PAIR123");
        assert_eq!(p.dex_id, "pumpswap");
        assert_eq!(p.quote_symbol, "SOL");
        assert!((p.price_native - 0.0000123).abs() < 1e-10);
        assert!((p.price_usd - 0.00246).abs() < 1e-10);
        assert!((p.liquidity_usd - 12345.67).abs() < 1e-6);
        assert!((p.liquidity_quote - 50.5).abs() < 1e-6);
        assert_eq!(p.pair_created_at_ms, 1712800000000);

        // Derived mcap_sol: 45000 * (0.0000123 / 0.00246) = 45000 * 0.005 = 225.0
        assert!(
            (p.market_cap_sol - 225.0).abs() < 1e-6,
            "expected ~225.0 SOL, got {}",
            p.market_cap_sol
        );
    }

    #[test]
    fn parse_pair_missing_optional_fields_ok() {
        // Only pairAddress is required. Everything else degrades to zero/empty.
        let v = json!({
            "pairAddress": "PAIR456",
            "quoteToken": { "symbol": "usdc" }
        });
        let p = parse_pair(&v).unwrap();
        assert_eq!(p.pair_address, "PAIR456");
        assert_eq!(p.dex_id, ""); // missing → empty (regression: was `?` bail)
        assert_eq!(p.quote_symbol, "USDC");
        assert_eq!(p.price_native, 0.0);
        assert_eq!(p.price_usd, 0.0);
        assert_eq!(p.liquidity_usd, 0.0);
        assert_eq!(p.market_cap_sol, 0.0);
    }

    #[test]
    fn parse_pair_missing_required_fails() {
        let v = json!({ "dexId": "pumpswap" }); // no pairAddress
        assert!(parse_pair(&v).is_none());
    }

    #[test]
    fn parse_pair_zero_price_usd_safe() {
        // Divide-by-zero guard: price_usd = 0 must not produce NaN/inf mcap.
        let v = json!({
            "pairAddress": "PAIR789",
            "quoteToken": { "symbol": "SOL" },
            "priceNative": "0.001",
            "priceUsd": "0",
            "marketCap": 1000.0
        });
        let p = parse_pair(&v).unwrap();
        assert_eq!(p.market_cap_sol, 0.0);
        assert!(p.market_cap_sol.is_finite());
    }

    /// Helper: build a minimal EnrichedPair for selection tests.
    fn mk_pair(dex: &str, liq_usd: f64) -> EnrichedPair {
        EnrichedPair {
            pair_address: format!("P_{}_{}", dex, liq_usd as u64),
            dex_id: dex.to_string(),
            quote_symbol: "SOL".to_string(),
            price_native: 1e-7,
            price_usd: 1e-5,
            liquidity_usd: liq_usd,
            liquidity_quote: liq_usd / 150.0,
            market_cap_usd: 0.0,
            market_cap_sol: 0.0,
            fdv_usd: 0.0,
            pair_created_at_ms: 0,
        }
    }

    #[test]
    fn select_pair_skip_fallback_when_preference_unmatched() {
        // Regression: migration events fire the moment a pool graduates, but
        // DexScreener may not have indexed the new pumpswap pair yet — only
        // the stale pumpfun bonding-curve pair shows up. The old fallback
        // silently picked that stale pair, producing enrichment_ok=true with
        // wrong liquidity/mcap. Strict mode must return None in that case.
        let candidates = vec![mk_pair("pumpfun", 0.0)];
        assert!(select_pair(candidates, "pumpswap").is_none());
    }

    #[test]
    fn select_pair_preferred_match_wins_over_higher_liquidity_other() {
        // When the preference IS indexed, pick it even if another dex has
        // higher liquidity. Exact-dex semantics, not best-effort.
        let candidates = vec![
            mk_pair("raydium", 100_000.0), // higher liq but wrong dex
            mk_pair("pumpswap", 500.0),    // preferred
        ];
        let chosen = select_pair(candidates, "pumpswap").unwrap();
        assert_eq!(chosen.dex_id, "pumpswap");
    }

    #[test]
    fn select_pair_empty_preference_picks_highest_liquidity() {
        // Non-migration lookup path: no preference → highest liquidity wins.
        let candidates = vec![
            mk_pair("meteora", 100.0),
            mk_pair("raydium", 5000.0),
            mk_pair("pumpswap", 1000.0),
        ];
        let chosen = select_pair(candidates, "").unwrap();
        assert_eq!(chosen.dex_id, "raydium");
    }

    #[test]
    fn parse_pair_nan_inputs_safe() {
        // Malformed numeric strings should coerce to 0.0, never NaN.
        let v = json!({
            "pairAddress": "PAIRNAN",
            "quoteToken": { "symbol": "SOL" },
            "priceNative": "not_a_number",
            "priceUsd": "NaN",
            "marketCap": 1000.0
        });
        let p = parse_pair(&v).unwrap();
        assert_eq!(p.price_native, 0.0);
        // "NaN".parse::<f64>() succeeds as NaN on most platforms — our
        // guard (price_usd > 0.0) rejects that so mcap_sol stays 0.
        assert_eq!(p.market_cap_sol, 0.0);
        assert!(p.market_cap_sol.is_finite());
    }
}
