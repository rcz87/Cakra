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
    let mut dedup_queue = DeduplicationQueue::with_default_ttl();
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

                // Already emitted this mint? Skip.
                if dedup_queue.contains(&mint) {
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
                    emit_token(merged.token, &mut dedup_queue, &token_sender).await;
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
                        emit_token(entry.token, &mut dedup_queue, &token_sender).await;
                    }
                }
            }
        }

        cleanup_counter += 1;
        if cleanup_counter % 1000 == 0 {
            dedup_queue.cleanup();
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
