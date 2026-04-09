use crate::models::token::{CreatorHistory, HoneypotResult, LpStatus, SecurityAnalysis};
use tracing::info;

/// Weight configuration for the scoring algorithm.
/// Each weight is expressed as a percentage (total = 100).
const WEIGHT_MINT_RENOUNCED: f64 = 12.0;
const WEIGHT_FREEZE_AUTH: f64 = 12.0;
const WEIGHT_METADATA_IMMUTABLE: f64 = 6.0;
const WEIGHT_LP_STATUS: f64 = 12.0;
const WEIGHT_HONEYPOT: f64 = 18.0;
const WEIGHT_GOPLUS: f64 = 8.0;
const WEIGHT_RUGCHECK: f64 = 12.0;
const WEIGHT_CREATOR: f64 = 10.0;
const WEIGHT_SOCIALS: f64 = 4.0;
const WEIGHT_LIQUIDITY: f64 = 6.0;

/// Calculate the final safety score (0..100) for a token based on all analysis results.
///
/// Scoring breakdown:
/// - Mint Renounced: 12% (renounced=100, not=0)
/// - Freeze Auth Null: 12% (null=100, not=0)
/// - Metadata Immutable: 6% (immutable=100, mutable=0)
/// - LP Burned/Locked: 12% (Burned=100, Locked=80, NotBurned/Unknown=0)
/// - Honeypot Simulation: 18% (Safe=100, HighTax=50, Honeypot/Unknown=0)
/// - GoPlus Safety: 8% (0..100 from API)
/// - RugCheck: 12% (0..100 from API)
/// - Creator History: 10% (Clean=100, Suspicious=30, Rugger/Unknown=0)
/// - Social Links: 4% (3+=100, 1-2=50, 0=0)
/// - Initial Liquidity: 6% (>$10K=100, $1K-$10K=50, <$1K=0)
///
/// If `initial_liquidity_usd` is 0 but `initial_liquidity_sol` > 0,
/// estimates USD using a conservative $100/SOL as fallback.
pub fn calculate_score(
    analysis: &SecurityAnalysis,
    initial_liquidity_usd: f64,
    initial_liquidity_sol: f64,
    market_cap_sol: f64,
) -> u8 {
    // Fallback: if USD is unknown, estimate from SOL (conservative $100/SOL)
    let effective_liquidity_usd = if initial_liquidity_usd > 0.0 {
        initial_liquidity_usd
    } else if initial_liquidity_sol > 0.0 {
        initial_liquidity_sol * 100.0 // conservative estimate
    } else {
        0.0
    };
    let mint_score = if analysis.mint_renounced { 100.0 } else { 0.0 };

    let freeze_score = if analysis.freeze_authority_null { 100.0 } else { 0.0 };

    let metadata_score = if analysis.metadata_immutable { 100.0 } else { 0.0 };

    let lp_score = match analysis.lp_status {
        LpStatus::Burned => 100.0,
        LpStatus::Locked => 80.0,
        LpStatus::NotBurned => 0.0,
        LpStatus::Unknown => 0.0,
    };

    let honeypot_score = match analysis.honeypot_result {
        HoneypotResult::Safe { .. } => 100.0,
        HoneypotResult::HighTax { .. } => 50.0,
        HoneypotResult::Honeypot => 0.0,
        HoneypotResult::Unknown => 0.0,
    };

    let goplus_score = analysis.goplus_score.unwrap_or(0.0).clamp(0.0, 100.0);

    let rugcheck_score = analysis.rugcheck_score.unwrap_or(0.0).clamp(0.0, 100.0);

    let creator_score = match analysis.creator_history {
        CreatorHistory::Clean { .. } => 100.0,
        CreatorHistory::Suspicious { .. } => 30.0,
        CreatorHistory::Rugger { .. } => 0.0,
        CreatorHistory::Unknown => 0.0,
    };

    let social_count = analysis.social_links.count();
    let social_score = if social_count >= 3 {
        100.0
    } else if social_count >= 1 {
        50.0
    } else {
        0.0
    };

    let liq_usd_score = if effective_liquidity_usd >= 10_000.0 {
        100.0
    } else if effective_liquidity_usd >= 1_000.0 {
        50.0
    } else {
        0.0
    };
    let mcap_score = if market_cap_sol >= 50.0 {
        100.0
    } else if market_cap_sol >= 20.0 {
        70.0
    } else if market_cap_sol >= 5.0 {
        40.0
    } else {
        0.0
    };
    let liquidity_score = f64::max(liq_usd_score, mcap_score);

    let weighted_total = (mint_score * WEIGHT_MINT_RENOUNCED
        + freeze_score * WEIGHT_FREEZE_AUTH
        + metadata_score * WEIGHT_METADATA_IMMUTABLE
        + lp_score * WEIGHT_LP_STATUS
        + honeypot_score * WEIGHT_HONEYPOT
        + goplus_score * WEIGHT_GOPLUS
        + rugcheck_score * WEIGHT_RUGCHECK
        + creator_score * WEIGHT_CREATOR
        + social_score * WEIGHT_SOCIALS
        + liquidity_score * WEIGHT_LIQUIDITY)
        / 100.0;

    let final_score = weighted_total.round().clamp(0.0, 100.0) as u8;

    info!(
        final_score,
        mint_score,
        freeze_score,
        metadata_score,
        lp_score,
        honeypot_score,
        goplus_score,
        rugcheck_score,
        creator_score,
        social_score,
        liquidity_score,
        "Score calculated"
    );

    final_score
}

/// Fast-mode score for PumpFun sniper: uses data that actually DIFFERS
/// between tokens at launch time.
///
/// PumpFun tokens at birth ALL have: mint not renounced, freeze active,
/// LP not burned, creator unknown. These fields are USELESS for scoring
/// because they're identical for every token.
///
/// What DOES differ:
/// - initial_buy_sol: creator's skin in the game (0.001 vs 10 SOL = huge signal)
/// - market_cap_sol: how much SOL in the bonding curve
/// - honeypot_result: can we actually sell this token?
/// - rugcheck_score: third-party safety rating
///
/// Scoring philosophy: filter by BEHAVIOR, not by checkboxes.
pub fn calculate_score_fast(
    analysis: &SecurityAnalysis,
    initial_liquidity_usd: f64,
    initial_liquidity_sol: f64,
    market_cap_sol: f64,
) -> u8 {
    // === Signal 1: Creator's initial buy (35% weight) ===
    // Biggest differentiator. Creator who buys 5+ SOL = serious.
    // Creator who buys 0.01 SOL = likely rug.
    let creator_buy_score = if initial_liquidity_sol >= 10.0 {
        100.0
    } else if initial_liquidity_sol >= 5.0 {
        85.0
    } else if initial_liquidity_sol >= 2.0 {
        65.0
    } else if initial_liquidity_sol >= 1.0 {
        45.0
    } else if initial_liquidity_sol >= 0.5 {
        25.0
    } else {
        5.0  // Tiny buy = very suspicious
    };

    // === Signal 2: Market cap at detection (25% weight) ===
    // Higher mcap = more people already bought = more momentum
    let mcap_score = if market_cap_sol >= 80.0 {
        100.0
    } else if market_cap_sol >= 50.0 {
        80.0
    } else if market_cap_sol >= 35.0 {
        55.0
    } else if market_cap_sol >= 28.0 {
        30.0  // Base PumpFun mcap (~28 SOL) = nobody bought yet
    } else {
        10.0
    };

    // === Signal 3: Honeypot check (20% weight) ===
    // Can we actually sell? Critical for not getting trapped.
    let honeypot_score = match analysis.honeypot_result {
        HoneypotResult::Safe { buy_tax, sell_tax } => {
            if buy_tax < 5.0 && sell_tax < 5.0 {
                100.0
            } else {
                60.0
            }
        }
        HoneypotResult::HighTax { .. } => 20.0,
        HoneypotResult::Honeypot => 0.0,
        HoneypotResult::Unknown => 40.0,  // Neutral — can't check on brand new tokens
    };

    // === Signal 4: RugCheck score (10% weight) ===
    let rugcheck_score = analysis.rugcheck_score.unwrap_or(50.0).clamp(0.0, 100.0);

    // === Signal 5: Creator history (10% weight) ===
    let creator_score = match analysis.creator_history {
        CreatorHistory::Clean { tokens_created } => {
            if tokens_created >= 3 { 100.0 } else { 70.0 }
        }
        CreatorHistory::Suspicious { .. } => 15.0,
        CreatorHistory::Rugger { .. } => 0.0,
        CreatorHistory::Unknown => 40.0,
    };

    // Weights (sum = 100)
    const W_CREATOR_BUY: f64 = 35.0;
    const W_MCAP: f64 = 25.0;
    const W_HONEYPOT: f64 = 20.0;
    const W_RUGCHECK: f64 = 10.0;
    const W_CREATOR: f64 = 10.0;

    let weighted = (creator_buy_score * W_CREATOR_BUY
        + mcap_score * W_MCAP
        + honeypot_score * W_HONEYPOT
        + rugcheck_score * W_RUGCHECK
        + creator_score * W_CREATOR)
        / 100.0;

    let final_score = weighted.round().clamp(0.0, 100.0) as u8;

    info!(
        final_score,
        creator_buy_score,
        mcap_score,
        honeypot_score,
        rugcheck_score,
        creator_score,
        initial_buy_sol = initial_liquidity_sol,
        market_cap_sol,
        "Fast score (behavior-based)"
    );

    final_score
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::token::{SocialLinks, SecurityAnalysis};

    #[test]
    fn test_perfect_score() {
        let analysis = SecurityAnalysis {
            mint_renounced: true,
            freeze_authority_null: true,
            metadata_immutable: true,
            lp_status: LpStatus::Burned,
            honeypot_result: HoneypotResult::Safe {
                buy_tax: 0.0,
                sell_tax: 0.0,
            },
            goplus_score: Some(100.0),
            rugcheck_score: Some(100.0),
            creator_history: CreatorHistory::Clean { tokens_created: 5 },
            social_links: SocialLinks {
                website: Some("https://example.com".to_string()),
                twitter: Some("https://twitter.com/example".to_string()),
                telegram: Some("https://t.me/example".to_string()),
            },
            final_score: 0,
        };

        let score = calculate_score(&analysis, 50_000.0, 0.0, 0.0);
        assert_eq!(score, 100);
    }

    #[test]
    fn test_zero_score() {
        let analysis = SecurityAnalysis::default();
        let score = calculate_score(&analysis, 0.0, 0.0, 0.0);
        assert_eq!(score, 0);
    }

    #[test]
    fn test_partial_score() {
        let analysis = SecurityAnalysis {
            mint_renounced: true,
            freeze_authority_null: true,
            metadata_immutable: false,
            lp_status: LpStatus::Locked,
            honeypot_result: HoneypotResult::HighTax {
                buy_tax: 12.0,
                sell_tax: 15.0,
            },
            goplus_score: Some(70.0),
            rugcheck_score: Some(50.0),
            creator_history: CreatorHistory::Suspicious {
                tokens_created: 3,
                rugs: 1,
            },
            social_links: SocialLinks {
                website: Some("https://example.com".to_string()),
                twitter: None,
                telegram: None,
            },
            final_score: 0,
        };

        let score = calculate_score(&analysis, 5_000.0, 0.0, 0.0);
        assert!(score > 40 && score < 80, "Score was {}", score);
    }
}
