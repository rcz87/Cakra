use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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

/// SOL wrapped mint for Price API V3 queries.
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// In-memory ring buffer that stores periodic SOL price samples
/// and calculates the 1-hour trend.
#[derive(Clone)]
pub struct SolTrendTracker {
    prices: Arc<Mutex<VecDeque<(Instant, f64)>>>,
    max_samples: usize,
    api_key: String,
}

impl SolTrendTracker {
    /// Create a new tracker with an empty buffer.
    /// Default capacity is 120 samples (one per 30 seconds for 1 hour).
    pub fn new(api_key: &str) -> Self {
        Self {
            prices: Arc::new(Mutex::new(VecDeque::with_capacity(120))),
            max_samples: 120,
            api_key: api_key.to_string(),
        }
    }

    /// Record a price sample, removing entries older than 1 hour.
    pub fn record_price(&self, price: f64) {
        let now = Instant::now();
        let one_hour = Duration::from_secs(3600);
        let mut buf = self.prices.lock().unwrap();

        // Remove entries older than 1 hour
        while let Some(&(ts, _)) = buf.front() {
            if now.duration_since(ts) > one_hour {
                buf.pop_front();
            } else {
                break;
            }
        }

        // If we're at capacity, drop the oldest entry
        if buf.len() >= self.max_samples {
            buf.pop_front();
        }

        buf.push_back((now, price));
    }

    /// Compare the oldest sample to the newest and return the percentage change.
    /// Returns 0.0 if fewer than 2 samples are available.
    pub fn get_1h_change_pct(&self) -> f64 {
        let buf = self.prices.lock().unwrap();
        if buf.len() < 2 {
            return 0.0;
        }
        let (_, oldest_price) = buf.front().unwrap();
        let (_, newest_price) = buf.back().unwrap();
        if *oldest_price == 0.0 {
            return 0.0;
        }
        ((newest_price - oldest_price) / oldest_price) * 100.0
    }

    /// Fetch the current SOL price from Jupiter Price API V3, record it, and return the price.
    pub async fn fetch_and_record(&self) -> Result<f64> {
        let http = reqwest::Client::new();

        let url = format!("https://api.jup.ag/price/v3?ids={}", SOL_MINT);
        let mut req = http.get(&url).timeout(Duration::from_secs(5));
        if !self.api_key.is_empty() {
            req = req.header("x-api-key", &self.api_key);
        }
        let resp = req.send().await?;
        let text = resp.text().await?;

        #[derive(Deserialize)]
        struct PriceData {
            #[serde(alias = "usdPrice")]
            usd_price: Option<f64>,
            // Legacy v2 format
            price: Option<String>,
        }

        let price_map: std::collections::HashMap<String, PriceData> =
            serde_json::from_str(&text).map_err(|e| {
                tracing::debug!(body = %text, "SOL price response body");
                anyhow::anyhow!("SOL price parse error: {e}")
            })?;
        let current_price = price_map
            .get(SOL_MINT)
            .and_then(|d| d.usd_price.or_else(|| d.price.as_ref()?.parse::<f64>().ok()))
            .unwrap_or(0.0);

        self.record_price(current_price);
        Ok(current_price)
    }
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
