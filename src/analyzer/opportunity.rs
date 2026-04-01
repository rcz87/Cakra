use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::info;

/// Opportunity score factors and their weights.
const WEIGHT_EARLY_DETECTION: f64 = 20.0;
const WEIGHT_LIQUIDITY_DEPTH: f64 = 20.0;
const WEIGHT_BUY_MOMENTUM: f64 = 25.0;
const WEIGHT_PRICE_STABILITY: f64 = 15.0;
const WEIGHT_SOL_TREND: f64 = 10.0;
const WEIGHT_VOLUME_CONSISTENCY: f64 = 10.0;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpportunityAnalysis {
    pub buy_count: u32,            // Number of buys detected so far
    pub unique_buyers: u32,        // Unique wallet addresses buying
    pub seconds_since_creation: u64,
    pub liquidity_usd: f64,
    pub price_change_pct: f64,     // Price change since creation (negative = dump)
    pub sol_trend_1h_pct: f64,     // SOL price change in last 1h
    pub largest_buyer_pct: f64,    // % of buys from single largest wallet
    pub opportunity_score: u8,
}

/// Calculate the opportunity score (0-100) for a token.
///
/// This is separate from safety scoring — a token can be "safe" (not a scam)
/// but still a bad opportunity (no momentum, whale-dominated, etc.)
pub fn calculate_opportunity_score(analysis: &OpportunityAnalysis) -> u8 {
    // Early detection: fewer buys = earlier = better opportunity
    let early_score = if analysis.buy_count <= 5 {
        100.0
    } else if analysis.buy_count <= 20 {
        50.0
    } else if analysis.buy_count <= 50 {
        25.0
    } else {
        0.0
    };

    // Liquidity depth: sweet spot is $5K-$50K
    let liquidity_score = if analysis.liquidity_usd >= 5_000.0 && analysis.liquidity_usd <= 50_000.0 {
        100.0
    } else if analysis.liquidity_usd >= 1_000.0 && analysis.liquidity_usd <= 100_000.0 {
        60.0
    } else if analysis.liquidity_usd >= 500.0 {
        30.0
    } else {
        0.0
    };

    // Buy momentum: unique buyers in early window
    let momentum_score = if analysis.unique_buyers >= 10 {
        100.0
    } else if analysis.unique_buyers >= 5 {
        70.0
    } else if analysis.unique_buyers >= 3 {
        40.0
    } else {
        10.0 // very few buyers, risky
    };

    // Price stability: hasn't dumped from creation
    let stability_score = if analysis.price_change_pct >= 0.0 {
        100.0 // price still up or flat
    } else if analysis.price_change_pct >= -15.0 {
        60.0 // minor pullback
    } else if analysis.price_change_pct >= -30.0 {
        30.0 // significant dump
    } else {
        0.0 // crashed
    };

    // SOL market trend
    let sol_trend_score = if analysis.sol_trend_1h_pct >= 1.0 {
        100.0 // SOL pumping, memes follow
    } else if analysis.sol_trend_1h_pct >= -1.0 {
        60.0 // SOL flat, neutral
    } else if analysis.sol_trend_1h_pct >= -3.0 {
        30.0 // SOL slightly bearish
    } else {
        0.0 // SOL dumping, bad time for memes
    };

    // Volume consistency: reject whale-dominated tokens
    let consistency_score = if analysis.largest_buyer_pct <= 20.0 {
        100.0 // well distributed
    } else if analysis.largest_buyer_pct <= 40.0 {
        60.0 // somewhat concentrated
    } else if analysis.largest_buyer_pct <= 60.0 {
        30.0 // whale dominated
    } else {
        0.0 // single whale, very risky
    };

    let weighted_total = (early_score * WEIGHT_EARLY_DETECTION
        + liquidity_score * WEIGHT_LIQUIDITY_DEPTH
        + momentum_score * WEIGHT_BUY_MOMENTUM
        + stability_score * WEIGHT_PRICE_STABILITY
        + sol_trend_score * WEIGHT_SOL_TREND
        + consistency_score * WEIGHT_VOLUME_CONSISTENCY)
        / 100.0;

    let final_score = weighted_total.round().clamp(0.0, 100.0) as u8;

    info!(
        opportunity_score = final_score,
        buy_count = analysis.buy_count,
        unique_buyers = analysis.unique_buyers,
        liquidity_usd = analysis.liquidity_usd,
        price_change_pct = analysis.price_change_pct,
        sol_trend = analysis.sol_trend_1h_pct,
        whale_pct = analysis.largest_buyer_pct,
        "Opportunity score calculated"
    );

    final_score
}

/// Fetch SOL/USD price trend (1h change percentage).
/// Uses CoinGecko free API or Jupiter price API.
pub async fn get_sol_trend_1h() -> Result<f64> {
    let http = reqwest::Client::new();

    // Use Jupiter price API (free, no key needed)
    let resp = http
        .get("https://price.jup.ag/v6/price?ids=SOL&vsToken=USDC")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?;

    #[derive(Deserialize)]
    struct PriceResponse {
        data: std::collections::HashMap<String, PriceData>,
    }
    #[derive(Deserialize)]
    struct PriceData {
        price: f64,
    }

    let price_resp: PriceResponse = resp.json().await?;

    // Jupiter price API doesn't give historical change directly.
    // For a real implementation, you'd track prices over time or use CoinGecko.
    // For now, return 0.0 (neutral) as a placeholder that will be improved.
    // TODO: Implement real 1h price tracking by storing periodic SOL prices
    let _current_price = price_resp.data.get("SOL").map(|d| d.price).unwrap_or(0.0);

    Ok(0.0) // Placeholder — returns neutral until historical tracking is added
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_perfect_opportunity() {
        let analysis = OpportunityAnalysis {
            buy_count: 3,
            unique_buyers: 12,
            seconds_since_creation: 10,
            liquidity_usd: 15_000.0,
            price_change_pct: 5.0,
            sol_trend_1h_pct: 2.0,
            largest_buyer_pct: 10.0,
            opportunity_score: 0,
        };
        let score = calculate_opportunity_score(&analysis);
        assert!(score >= 90, "Score was {}", score);
    }

    #[test]
    fn test_bad_opportunity() {
        let analysis = OpportunityAnalysis {
            buy_count: 100,
            unique_buyers: 2,
            seconds_since_creation: 300,
            liquidity_usd: 200.0,
            price_change_pct: -50.0,
            sol_trend_1h_pct: -5.0,
            largest_buyer_pct: 80.0,
            opportunity_score: 0,
        };
        let score = calculate_opportunity_score(&analysis);
        assert!(score <= 15, "Score was {}", score);
    }
}
