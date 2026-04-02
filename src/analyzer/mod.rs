pub mod authority;
pub mod creator;
pub mod entry_confirmation;
pub mod goplus;
pub mod honeypot;
pub mod liquidity;
pub mod metadata;
pub mod opportunity;
pub mod rugcheck;
pub mod scoring;
pub mod socials;

use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use tracing::{info, warn};

use crate::config::Config;
use crate::models::token::{SecurityAnalysis, TokenInfo};

use self::authority::{check_freeze_authority, check_mint_authority};
use self::creator::{analyze_creator, CreatorCache};
use self::goplus::check_goplus;
use self::honeypot::simulate_honeypot;
use self::liquidity::check_lp_status;
use self::metadata::check_metadata_immutable;
use self::rugcheck::check_rugcheck;
use self::scoring::{calculate_score, calculate_score_fast};
use self::socials::check_socials;

/// Central analyzer service for RICOZ SNIPER.
/// Runs all security checks on a newly detected token and produces a final safety score.
pub struct AnalyzerService {
    pub config: Config,
    pub creator_cache: CreatorCache,
}

impl AnalyzerService {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            creator_cache: CreatorCache::new(3600), // 1 hour TTL
        }
    }

    /// Fast security filter for snipe mode (< 500ms target).
    ///
    /// Only runs on-chain checks: mint authority, freeze authority, creator basic.
    /// Skips slow external API calls (GoPlus, RugCheck, honeypot, socials).
    /// Returns a score based on available data — unscored fields default to neutral.
    pub async fn analyze_token_fast(
        &self,
        token: &TokenInfo,
        rpc_client: &RpcClient,
    ) -> Result<SecurityAnalysis> {
        info!(
            mint = %token.mint,
            symbol = %token.symbol,
            "Starting FAST security filter (snipe mode)"
        );

        let mut analysis = SecurityAnalysis::default();

        // --- Mint authority (on-chain, fast) ---
        match check_mint_authority(rpc_client, &token.mint) {
            Ok(renounced) => analysis.mint_renounced = renounced,
            Err(e) => warn!(mint = %token.mint, err = %e, "Fast: mint authority failed"),
        }

        // --- Freeze authority (on-chain, fast) ---
        match check_freeze_authority(rpc_client, &token.mint) {
            Ok(null) => analysis.freeze_authority_null = null,
            Err(e) => warn!(mint = %token.mint, err = %e, "Fast: freeze authority failed"),
        }

        // --- Creator history (cached, fast if cached) ---
        match analyze_creator(rpc_client, &token.creator, &self.creator_cache).await {
            Ok(history) => analysis.creator_history = history,
            Err(e) => warn!(mint = %token.mint, err = %e, "Fast: creator check failed"),
        }

        // --- LP status (on-chain, fast) ---
        if let Some(ref pool_address) = token.pool_address {
            match check_lp_status(rpc_client, pool_address) {
                Ok(status) => analysis.lp_status = status,
                Err(e) => warn!(mint = %token.mint, err = %e, "Fast: LP check failed"),
            }
        }

        // Score using only checked fields — don't penalize unknown/unchecked data
        analysis.final_score = calculate_score_fast(
            &analysis,
            token.initial_liquidity_usd,
            token.initial_liquidity_sol,
        );

        info!(
            mint = %token.mint,
            score = analysis.final_score,
            "FAST security filter complete"
        );

        Ok(analysis)
    }

    /// Run every security check against the given token and compute the final score.
    pub async fn analyze_token(
        &self,
        token: &TokenInfo,
        rpc_client: &RpcClient,
    ) -> Result<SecurityAnalysis> {
        info!(
            mint = %token.mint,
            symbol = %token.symbol,
            "Starting security analysis"
        );

        let mut analysis = SecurityAnalysis::default();

        // --- Mint authority ---
        match check_mint_authority(rpc_client, &token.mint) {
            Ok(renounced) => {
                analysis.mint_renounced = renounced;
                info!(mint = %token.mint, renounced, "Mint authority check complete");
            }
            Err(e) => {
                warn!(mint = %token.mint, err = %e, "Mint authority check failed");
            }
        }

        // --- Freeze authority ---
        match check_freeze_authority(rpc_client, &token.mint) {
            Ok(null) => {
                analysis.freeze_authority_null = null;
                info!(mint = %token.mint, freeze_null = null, "Freeze authority check complete");
            }
            Err(e) => {
                warn!(mint = %token.mint, err = %e, "Freeze authority check failed");
            }
        }

        // --- Metadata immutability ---
        match check_metadata_immutable(rpc_client, &token.mint) {
            Ok(immutable) => {
                analysis.metadata_immutable = immutable;
                info!(mint = %token.mint, immutable, "Metadata immutability check complete");
            }
            Err(e) => {
                warn!(mint = %token.mint, err = %e, "Metadata immutability check failed");
            }
        }

        // --- LP status ---
        if let Some(ref pool_address) = token.pool_address {
            match check_lp_status(rpc_client, pool_address) {
                Ok(status) => {
                    analysis.lp_status = status;
                    info!(mint = %token.mint, "LP status check complete");
                }
                Err(e) => {
                    warn!(mint = %token.mint, err = %e, "LP status check failed");
                }
            }
        }

        // --- Honeypot simulation ---
        match simulate_honeypot(
            &self.config.jupiter_api_url,
            &token.mint,
        )
        .await
        {
            Ok(result) => {
                analysis.honeypot_result = result;
                info!(mint = %token.mint, "Honeypot simulation complete");
            }
            Err(e) => {
                warn!(mint = %token.mint, err = %e, "Honeypot simulation failed");
            }
        }

        // --- GoPlus API ---
        if !self.config.goplus_api_key.is_empty() {
            match check_goplus(&token.mint, &self.config.goplus_api_key).await {
                Ok(gp) => {
                    analysis.goplus_score = Some(gp.safety_score);
                    info!(mint = %token.mint, score = gp.safety_score, "GoPlus check complete");
                }
                Err(e) => {
                    warn!(mint = %token.mint, err = %e, "GoPlus check failed");
                }
            }
        }

        // --- RugCheck API ---
        match check_rugcheck(&token.mint, &self.config.rugcheck_api_url).await {
            Ok(rc) => {
                analysis.rugcheck_score = Some(rc.score);
                info!(mint = %token.mint, score = rc.score, "RugCheck check complete");
            }
            Err(e) => {
                warn!(mint = %token.mint, err = %e, "RugCheck check failed");
            }
        }

        // --- Creator history ---
        match analyze_creator(rpc_client, &token.creator, &self.creator_cache).await {
            Ok(history) => {
                analysis.creator_history = history;
                info!(mint = %token.mint, "Creator analysis complete");
            }
            Err(e) => {
                warn!(mint = %token.mint, err = %e, "Creator analysis failed");
            }
        }

        // --- Social links ---
        match check_socials(token.metadata_uri.as_deref()).await {
            Ok(links) => {
                analysis.social_links = links;
                info!(mint = %token.mint, "Social links check complete");
            }
            Err(e) => {
                warn!(mint = %token.mint, err = %e, "Social links check failed");
            }
        }

        // --- Final score ---
        analysis.final_score = calculate_score(
            &analysis,
            token.initial_liquidity_usd,
            token.initial_liquidity_sol,
        );
        info!(
            mint = %token.mint,
            score = analysis.final_score,
            "Security analysis complete"
        );

        Ok(analysis)
    }
}
