use crate::models::token::{CreatorHistory, HoneypotResult, LpStatus, SecurityAnalysis};
use tracing::info;

/// Weight configuration for the scoring algorithm.
/// Each weight is expressed as a percentage (total = 100).
const WEIGHT_MINT_RENOUNCED: f64 = 15.0;
const WEIGHT_FREEZE_AUTH: f64 = 15.0;
const WEIGHT_METADATA_IMMUTABLE: f64 = 5.0;
const WEIGHT_LP_STATUS: f64 = 15.0;
const WEIGHT_HONEYPOT: f64 = 20.0;
const WEIGHT_GOPLUS: f64 = 10.0;
const WEIGHT_CREATOR: f64 = 10.0;
const WEIGHT_SOCIALS: f64 = 5.0;
const WEIGHT_LIQUIDITY: f64 = 5.0;

/// Calculate the final safety score (0..100) for a token based on all analysis results.
///
/// Scoring breakdown:
/// - Mint Renounced: 15% (renounced=100, not=0)
/// - Freeze Auth Null: 15% (null=100, not=0)
/// - Metadata Immutable: 5% (immutable=100, mutable=0)
/// - LP Burned/Locked: 15% (Burned=100, Locked=80, NotBurned/Unknown=0)
/// - Honeypot Simulation: 20% (Safe=100, HighTax=50, Honeypot/Unknown=0)
/// - GoPlus Safety: 10% (0..100 from API)
/// - Creator History: 10% (Clean=100, Suspicious=30, Rugger/Unknown=0)
/// - Social Links: 5% (3+=100, 1-2=50, 0=0)
/// - Initial Liquidity: 5% (>$10K=100, $1K-$10K=50, <$1K=0)
pub fn calculate_score(analysis: &SecurityAnalysis, initial_liquidity_usd: f64) -> u8 {
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

    let liquidity_score = if initial_liquidity_usd >= 10_000.0 {
        100.0
    } else if initial_liquidity_usd >= 1_000.0 {
        50.0
    } else {
        0.0
    };

    let weighted_total = (mint_score * WEIGHT_MINT_RENOUNCED
        + freeze_score * WEIGHT_FREEZE_AUTH
        + metadata_score * WEIGHT_METADATA_IMMUTABLE
        + lp_score * WEIGHT_LP_STATUS
        + honeypot_score * WEIGHT_HONEYPOT
        + goplus_score * WEIGHT_GOPLUS
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
        creator_score,
        social_score,
        liquidity_score,
        "Score calculated"
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

        let score = calculate_score(&analysis, 50_000.0);
        assert_eq!(score, 100);
    }

    #[test]
    fn test_zero_score() {
        let analysis = SecurityAnalysis::default();
        let score = calculate_score(&analysis, 0.0);
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

        let score = calculate_score(&analysis, 5_000.0);
        // 15 + 15 + 0 + 12 + 10 + 7 + 3 + 2.5 + 2.5 = 67
        assert!(score > 50 && score < 80, "Score was {}", score);
    }
}
