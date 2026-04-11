pub mod grpc;
pub mod parser;
pub mod pumpfun;
pub mod pumpportal;
pub mod pumpswap;
pub mod queue;
pub mod raydium;
pub mod ws;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::{Config, DetectorMode};
use crate::models::token::TokenInfo;

use self::parser::RawTransaction;
use self::pumpfun::process_pumpfun_transaction;
use self::pumpswap::process_pumpswap_transaction;
use self::queue::DeduplicationQueue;
use self::raydium::process_raydium_transaction;

/// DetectorService spawns a streaming listener (gRPC or WebSocket) and
/// processes incoming raw transactions through all detectors, sending
/// newly detected tokens through a channel.
pub struct DetectorService {
    config: Config,
    token_sender: mpsc::Sender<TokenInfo>,
}

impl DetectorService {
    /// Create a new DetectorService.
    /// Returns the service and a receiver for detected tokens.
    pub fn new(config: Config) -> (Self, mpsc::Receiver<TokenInfo>) {
        let (token_sender, token_receiver) = mpsc::channel(256);
        let service = Self {
            config,
            token_sender,
        };
        (service, token_receiver)
    }

    /// Start the detector service.
    /// Runs PumpPortal (for PumpFun creates + migrations) in parallel with
    /// Helius gRPC/WS (for Raydium/PumpSwap pool creates).
    pub async fn start(self) -> Result<()> {
        info!("Starting RICOZ SNIPER detector service");

        let (raw_tx_sender, raw_tx_receiver) = mpsc::channel::<RawTransaction>(1024);

        // Shared dedup channel: both PumpPortal and Helius tokens go through here
        let (dedup_tx, dedup_rx) = mpsc::channel::<TokenInfo>(512);
        let final_sender = self.token_sender.clone();
        let dedup_handle = tokio::spawn(async move {
            deduplicate_tokens(dedup_rx, final_sender).await;
        });

        // Spawn the transaction processing loop for gRPC/WS raw transactions
        let token_sender = dedup_tx.clone();
        let processing_handle = tokio::spawn(async move {
            process_raw_transactions(raw_tx_receiver, token_sender).await;
        });

        // === PumpPortal detector (parallel, always on) ===
        // Sends TokenInfo directly — covers PumpFun creates + PumpSwap migrations
        let pp_token_sender = dedup_tx.clone();
        let pumpportal_handle = tokio::spawn(async move {
            let mut backoff_secs: u64 = 1;
            const MAX_BACKOFF_SECS: u64 = 60;

            loop {
                match pumpportal::start_pumpportal_listener(pp_token_sender.clone()).await {
                    Ok(()) => {
                        warn!("PumpPortal listener exited cleanly, reconnecting...");
                        backoff_secs = 1;
                    }
                    Err(e) => {
                        error!(
                            error = %e,
                            backoff_secs,
                            "PumpPortal listener error, reconnecting after backoff"
                        );
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
            }
        });

        // === Helius gRPC/WS detector (for Raydium coverage) ===
        let use_grpc = match self.config.detector_mode {
            DetectorMode::Grpc => true,
            DetectorMode::WebSocket => false,
            DetectorMode::Auto => {
                if self.config.grpc_endpoint.is_empty() {
                    info!("No GRPC_ENDPOINT configured, using WebSocket detector");
                    false
                } else {
                    info!("Detector mode: auto — trying gRPC first...");
                    match grpc::start_grpc_listener(
                        &self.config.grpc_endpoint,
                        &self.config.grpc_token,
                        raw_tx_sender.clone(),
                    )
                    .await
                    {
                        Ok(()) => true,
                        Err(e) => {
                            warn!(
                                error = %e,
                                "gRPC unavailable, falling back to WebSocket detector"
                            );
                            false
                        }
                    }
                }
            }
        };

        let backend_name = if use_grpc { "gRPC" } else { "WebSocket" };
        info!(backend = backend_name, "Helius detector backend selected");

        let config = self.config.clone();
        let helius_handle = tokio::spawn(async move {
            let mut backoff_secs: u64 = 1;
            const MAX_BACKOFF_SECS: u64 = 60;

            loop {
                let result = if use_grpc {
                    grpc::start_grpc_listener(
                        &config.grpc_endpoint,
                        &config.grpc_token,
                        raw_tx_sender.clone(),
                    )
                    .await
                } else {
                    ws::start_ws_listener(
                        &config.solana_ws_url,
                        config.effective_rpc_url(),
                        raw_tx_sender.clone(),
                    )
                    .await
                };

                match result {
                    Ok(()) => {
                        warn!(
                            backend = backend_name,
                            backoff_secs, "Helius listener exited cleanly, reconnecting..."
                        );
                        backoff_secs = 1;
                    }
                    Err(e) => {
                        error!(
                            backend = backend_name,
                            error = %e,
                            backoff_secs,
                            "Helius listener error, reconnecting after backoff"
                        );
                    }
                }

                tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
            }
        });

        // Wait for any task to complete (shouldn't happen)
        tokio::select! {
            result = pumpportal_handle => {
                error!("PumpPortal task ended unexpectedly: {:?}", result);
            }
            result = helius_handle => {
                error!("Helius listener task ended unexpectedly: {:?}", result);
            }
            result = processing_handle => {
                error!("Transaction processing task ended unexpectedly: {:?}", result);
            }
            result = dedup_handle => {
                error!("Dedup task ended unexpectedly: {:?}", result);
            }
        }

        Ok(())
    }
}

/// Process raw transactions from gRPC/WS through program-specific detectors.
/// Detected tokens are forwarded to the dedup layer.
async fn process_raw_transactions(
    mut rx: mpsc::Receiver<RawTransaction>,
    token_sender: mpsc::Sender<TokenInfo>,
) {
    info!("Transaction processor started, waiting for events...");

    while let Some(raw_tx) = rx.recv().await {
        let token_info = match raw_tx.program_id.as_str() {
            pumpfun::PUMPFUN_PROGRAM_ID => process_pumpfun_transaction(&raw_tx),
            raydium::RAYDIUM_AMM_PROGRAM_ID | raydium::RAYDIUM_CPMM_PROGRAM_ID => {
                process_raydium_transaction(&raw_tx)
            }
            pumpswap::PUMPSWAP_PROGRAM_ID => process_pumpswap_transaction(&raw_tx),
            _ => None,
        };

        if let Some(info) = token_info {
            if let Err(e) = token_sender.send(info).await {
                warn!(error = %e, "Failed to forward token to dedup layer");
                break;
            }
        }
    }

    warn!("Transaction processor shutting down");
}

use std::collections::HashMap;
use std::time::Duration;
use crate::models::token::DetectionBackend;

/// Merge window duration: wait this long for PumpPortal to enrich a Helius-triggered token.
const MERGE_WINDOW: Duration = Duration::from_millis(250);

/// A pending token waiting for enrichment from the second source.
struct PendingToken {
    /// The token data (may be partial from Helius or full from PumpPortal)
    token: TokenInfo,
    /// When this pending entry was created
    created_at: tokio::time::Instant,
    /// Whether we've received data from Helius
    has_helius: bool,
    /// Whether we've received data from PumpPortal
    has_pumpportal: bool,
}

/// Hybrid merge engine: Helius triggers fast, PumpPortal enriches.
///
/// Flow:
/// 1. First event (usually Helius) creates a PendingToken with a merge window
/// 2. If second event (usually PumpPortal) arrives within the window, merge data
/// 3. After window expires OR both sources received, emit the merged token
///
/// Merge strategy:
/// - mint, source, detected_at: from first arrival (fastest)
/// - name, symbol, creator, metadata_uri: prefer non-empty, PumpPortal wins ties
/// - initial_liquidity_sol, market_cap_sol: prefer PumpPortal (has real data)
/// - pool_address: prefer non-empty
async fn deduplicate_tokens(
    mut rx: mpsc::Receiver<TokenInfo>,
    token_sender: mpsc::Sender<TokenInfo>,
) {
    // Two independent dedup namespaces — newborn create events and migration
    // events must NOT collide. Prior bug: when a PumpFun token was born and
    // then migrated within the 5-minute TTL, the migration event hit the
    // same dedup key and was silently dropped. ~50% of migration events
    // were lost to this race before the fix. See commit history for detail.
    //
    // Each queue keeps its own TTL; cleanup runs for both below.
    let mut newborn_dedup = DeduplicationQueue::with_default_ttl();
    let mut migration_dedup = DeduplicationQueue::with_default_ttl();
    let mut pending: HashMap<String, PendingToken> = HashMap::new();
    let mut cleanup_counter: u64 = 0;

    info!("Hybrid merge engine started (window={}ms)", MERGE_WINDOW.as_millis());

    loop {
        // Calculate next timeout: earliest pending token's expiry
        let next_expiry = pending
            .values()
            .map(|p| p.created_at + MERGE_WINDOW)
            .min();

        let timeout_fut = async {
            match next_expiry {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            biased; // Prioritize incoming tokens over timeouts

            Some(token) = rx.recv() => {
                if token.mint.is_empty() {
                    continue;
                }

                let mint = token.mint.clone();

                // ── Migration fast path ────────────────────────────
                // Migration events are standalone (PumpPortal-only, no
                // Helius counterpart), so they must bypass the merge
                // engine entirely. They also live in a dedicated dedup
                // namespace so they cannot be blocked by a recent newborn
                // emission for the same mint — which IS the whole point
                // of this fix.
                //
                // Additionally: routing migrations into the `pending`
                // HashMap would let `merge_token_data` overwrite a
                // newborn's `creator` field with MIGRATION_EVENT_MARKER
                // in the rare 250ms merge window — a silent data
                // corruption bug this early-return also prevents.
                let is_migration = token.creator
                    == crate::detector::pumpportal::MIGRATION_EVENT_MARKER;
                if is_migration {
                    if migration_dedup.contains(&mint) {
                        continue;
                    }
                    emit_token(token, &mut migration_dedup, &token_sender).await;
                    continue;
                }

                // ── Newborn path (unchanged behaviour) ─────────────
                // Already emitted this mint? Skip.
                if newborn_dedup.contains(&mint) {
                    continue;
                }

                if let Some(pending_entry) = pending.get_mut(&mint) {
                    // Second source arrived — merge and emit immediately
                    let first = if pending_entry.has_pumpportal { "PumpPortal" } else { "Helius" };
                    let second = &token.backend;
                    let elapsed = pending_entry.created_at.elapsed().as_millis();
                    info!(
                        mint = %mint,
                        first_source = first,
                        second_source = %second,
                        merge_latency_ms = elapsed as u64,
                        "Merge: both sources received"
                    );
                    merge_token_data(pending_entry, &token);

                    let merged = pending.remove(&mint).unwrap();
                    emit_token(merged.token, &mut newborn_dedup, &token_sender).await;
                } else {
                    // First source — create pending entry
                    let is_helius = token.backend == DetectionBackend::Helius;
                    pending.insert(mint, PendingToken {
                        token,
                        created_at: tokio::time::Instant::now(),
                        has_helius: is_helius,
                        has_pumpportal: !is_helius,
                    });
                }
            }

            _ = timeout_fut => {
                // Emit all expired pending tokens
                let now = tokio::time::Instant::now();
                let expired_mints: Vec<String> = pending
                    .iter()
                    .filter(|(_, p)| now >= p.created_at + MERGE_WINDOW)
                    .map(|(mint, _)| mint.clone())
                    .collect();

                for mint in expired_mints {
                    if let Some(entry) = pending.remove(&mint) {
                        let source = if entry.has_helius && entry.has_pumpportal {
                            "merged"
                        } else if entry.has_pumpportal {
                            "pumpportal-only"
                        } else {
                            "helius-only"
                        };
                        info!(
                            mint = %entry.token.mint,
                            merge_source = source,
                            elapsed_ms = entry.created_at.elapsed().as_millis() as u64,
                            "Merge window expired, emitting"
                        );
                        emit_token(entry.token, &mut newborn_dedup, &token_sender).await;
                    }
                }
            }
        }

        cleanup_counter += 1;
        if cleanup_counter % 1000 == 0 {
            newborn_dedup.cleanup();
            migration_dedup.cleanup();
            // Also clean up very old pending entries (shouldn't happen, but safety)
            let now = tokio::time::Instant::now();
            pending.retain(|_, p| now.duration_since(p.created_at) < Duration::from_secs(10));
        }
    }
}

/// Merge data from a second source into a pending token.
/// PumpPortal data wins for liquidity/market cap fields.
/// Non-empty strings win over empty strings.
fn merge_token_data(pending: &mut PendingToken, incoming: &TokenInfo) {
    let is_pp = incoming.backend == DetectionBackend::PumpPortal;

    if is_pp {
        pending.has_pumpportal = true;
    } else {
        pending.has_helius = true;
    }

    let t = &mut pending.token;

    // Name/symbol: prefer non-empty, PumpPortal wins ties
    if t.name.is_empty() || (is_pp && !incoming.name.is_empty()) {
        t.name = incoming.name.clone();
    }
    if t.symbol.is_empty() || (is_pp && !incoming.symbol.is_empty()) {
        t.symbol = incoming.symbol.clone();
    }

    // Creator: prefer non-empty, PumpPortal wins ties
    if t.creator.is_empty() || (is_pp && !incoming.creator.is_empty()) {
        t.creator = incoming.creator.clone();
    }

    // Liquidity/market cap: PumpPortal has real data, always prefer
    if is_pp {
        if incoming.initial_liquidity_sol > 0.0 {
            t.initial_liquidity_sol = incoming.initial_liquidity_sol;
        }
        if incoming.market_cap_sol > 0.0 {
            t.market_cap_sol = incoming.market_cap_sol;
        }
    }

    // Pool address: prefer non-empty
    if t.pool_address.is_none() {
        t.pool_address = incoming.pool_address.clone();
    }

    // Metadata URI: prefer non-empty
    if t.metadata_uri.is_none() {
        t.metadata_uri = incoming.metadata_uri.clone();
    }
}

/// Emit a finalized token to the analyzer.
async fn emit_token(
    token: TokenInfo,
    dedup_queue: &mut DeduplicationQueue,
    token_sender: &mpsc::Sender<TokenInfo>,
) {
    if !dedup_queue.insert(&token.mint) {
        return; // Already emitted
    }

    let merge_tag = if token.market_cap_sol > 0.0 { "enriched" } else { "basic" };

    info!(
        mint = %token.mint,
        source = %token.source,
        name = %token.name,
        symbol = %token.symbol,
        liquidity_sol = token.initial_liquidity_sol,
        market_cap_sol = token.market_cap_sol,
        merge = merge_tag,
        "New token detected"
    );

    match token_sender.try_send(token) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            warn!("Token channel full — dropping token, analyzer may be overloaded");
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            error!("Token channel closed — analyzer has shut down");
        }
    }
}

#[cfg(test)]
mod dedup_tests {
    //! Integration tests for `deduplicate_tokens`.
    //!
    //! These verify the dedup collision fix: newborn and migration events
    //! for the same mint live in independent namespaces and must both be
    //! emitted, rather than the migration being silently dropped.

    use super::*;
    use crate::detector::pumpportal::MIGRATION_EVENT_MARKER;
    use crate::models::token::{DetectionBackend, TokenInfo, TokenSource};
    use chrono::Utc;
    use tokio::sync::mpsc;

    fn make_newborn(mint: &str) -> TokenInfo {
        TokenInfo {
            mint: mint.to_string(),
            name: "Newborn".to_string(),
            symbol: "NB".to_string(),
            source: TokenSource::PumpFun,
            creator: "Creator111111111111111111111111111111111111".to_string(),
            initial_liquidity_sol: 14.8,
            initial_liquidity_usd: 0.0,
            pool_address: None,
            metadata_uri: None,
            decimals: 6,
            detected_at: Utc::now(),
            backend: DetectionBackend::PumpPortal,
            market_cap_sol: 30.0,
            v_sol_in_bonding_curve: 0.0,
            initial_buy_sol: 1.0,
        }
    }

    fn make_migration(mint: &str) -> TokenInfo {
        TokenInfo {
            mint: mint.to_string(),
            name: String::new(),
            symbol: String::new(),
            source: TokenSource::PumpSwap,
            creator: MIGRATION_EVENT_MARKER.to_string(),
            initial_liquidity_sol: 0.0,
            initial_liquidity_usd: 0.0,
            pool_address: None,
            metadata_uri: None,
            decimals: 6,
            detected_at: Utc::now(),
            backend: DetectionBackend::PumpPortal,
            market_cap_sol: 0.0,
            v_sol_in_bonding_curve: 0.0,
            initial_buy_sol: 0.0,
        }
    }

    /// Collect all tokens emitted on `out_rx` within `timeout`. Returns a
    /// vector in emission order. Used by every test to drain the channel
    /// after feeding the dedup task.
    async fn drain(
        out_rx: &mut mpsc::Receiver<TokenInfo>,
        timeout: std::time::Duration,
    ) -> Vec<TokenInfo> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, out_rx.recv()).await {
                Ok(Some(t)) => out.push(t),
                Ok(None) => break,
                Err(_) => break,
            }
        }
        out
    }

    #[tokio::test(flavor = "current_thread")]
    async fn newborn_then_migration_both_emit() {
        // REGRESSION TEST: before the fix, a migration event arriving
        // within 5-minute TTL of a newborn emit for the same mint was
        // silently dropped at dedup_queue.contains(). This test enforces
        // that both events now reach the analyzer channel.
        let (in_tx, in_rx) = mpsc::channel::<TokenInfo>(8);
        let (out_tx, mut out_rx) = mpsc::channel::<TokenInfo>(8);

        let task = tokio::spawn(async move {
            deduplicate_tokens(in_rx, out_tx).await;
        });

        let mint = "TEST_COLLIDE_MINT_11111111111111111111111111";
        in_tx.send(make_newborn(mint)).await.unwrap();
        // Wait past the 250ms merge window so the newborn is emitted.
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        in_tx.send(make_migration(mint)).await.unwrap();

        let emitted = drain(&mut out_rx, std::time::Duration::from_millis(400)).await;
        drop(in_tx);
        task.abort();

        assert_eq!(emitted.len(), 2, "expected both newborn AND migration: got {:?}",
                   emitted.iter().map(|t| &t.creator).collect::<Vec<_>>());
        // First out must be the newborn (creator != MIGRATION_MARKER)
        assert_ne!(emitted[0].creator, MIGRATION_EVENT_MARKER);
        // Second out must be the migration
        assert_eq!(emitted[1].creator, MIGRATION_EVENT_MARKER);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplicate_migration_deduped() {
        // Migration-specific dedup must still protect against duplicate
        // PumpPortal migration events for the same mint.
        let (in_tx, in_rx) = mpsc::channel::<TokenInfo>(8);
        let (out_tx, mut out_rx) = mpsc::channel::<TokenInfo>(8);

        let task = tokio::spawn(async move {
            deduplicate_tokens(in_rx, out_tx).await;
        });

        let mint = "TEST_DUP_MIGRATION_11111111111111111111111111";
        in_tx.send(make_migration(mint)).await.unwrap();
        in_tx.send(make_migration(mint)).await.unwrap();
        in_tx.send(make_migration(mint)).await.unwrap();

        let emitted = drain(&mut out_rx, std::time::Duration::from_millis(200)).await;
        drop(in_tx);
        task.abort();

        assert_eq!(emitted.len(), 1, "duplicate migrations must be deduped");
        assert_eq!(emitted[0].creator, MIGRATION_EVENT_MARKER);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn migration_bypasses_merge_engine() {
        // A migration event must never land in the `pending` merge HashMap.
        // Regression guard: the previous code path would feed the migration
        // into merge_token_data, which promotes its MIGRATION_EVENT_MARKER
        // creator onto a pending newborn entry (data corruption).
        //
        // Sequence: newborn arrives (enters pending), then migration arrives
        // for the same mint within 250ms merge window. Expected: the newborn
        // still emits as a newborn (creator untouched), and the migration
        // emits independently in the migration namespace.
        let (in_tx, in_rx) = mpsc::channel::<TokenInfo>(8);
        let (out_tx, mut out_rx) = mpsc::channel::<TokenInfo>(8);

        let task = tokio::spawn(async move {
            deduplicate_tokens(in_rx, out_tx).await;
        });

        let mint = "TEST_RACE_MERGE_111111111111111111111111111";
        let mut newborn = make_newborn(mint);
        newborn.backend = DetectionBackend::Helius; // force merge window
        in_tx.send(newborn).await.unwrap();
        // Send migration BEFORE 250ms merge window closes.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        in_tx.send(make_migration(mint)).await.unwrap();

        let emitted = drain(&mut out_rx, std::time::Duration::from_millis(500)).await;
        drop(in_tx);
        task.abort();

        assert_eq!(emitted.len(), 2, "both events must emit independently");
        // Find which one is newborn and which is migration — order is not
        // strictly guaranteed because the migration fast-path runs before
        // the merge window expires for the newborn.
        let newborn_out = emitted
            .iter()
            .find(|t| t.creator != MIGRATION_EVENT_MARKER)
            .expect("newborn must be emitted with its original creator");
        let migration_out = emitted
            .iter()
            .find(|t| t.creator == MIGRATION_EVENT_MARKER)
            .expect("migration must be emitted");
        assert_ne!(newborn_out.creator, MIGRATION_EVENT_MARKER,
                   "newborn creator must not be corrupted by merge");
        assert_eq!(migration_out.creator, MIGRATION_EVENT_MARKER);
    }
}
