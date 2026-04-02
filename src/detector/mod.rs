pub mod grpc;
pub mod parser;
pub mod pumpfun;
pub mod pumpswap;
pub mod queue;
pub mod raydium;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::models::token::TokenInfo;

use self::parser::RawTransaction;
use self::pumpfun::process_pumpfun_transaction;
use self::pumpswap::process_pumpswap_transaction;
use self::queue::DeduplicationQueue;
use self::raydium::process_raydium_transaction;

/// DetectorService spawns the gRPC listener and processes incoming
/// raw transactions through all detectors, sending newly detected
/// tokens through a channel.
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

    /// Start the detector service. This spawns the gRPC listener and
    /// the transaction processing loop.
    pub async fn start(self) -> Result<()> {
        info!("Starting RICOZ SNIPER detector service");

        let (raw_tx_sender, raw_tx_receiver) = mpsc::channel::<RawTransaction>(1024);

        // Spawn the gRPC listener
        let grpc_endpoint = self.config.grpc_endpoint.clone();
        let grpc_token = self.config.grpc_token.clone();
        let grpc_handle = tokio::spawn(async move {
            let mut backoff_secs: u64 = 1;
            const MAX_BACKOFF_SECS: u64 = 60;

            loop {
                match grpc::start_grpc_listener(&grpc_endpoint, &grpc_token, raw_tx_sender.clone())
                    .await
                {
                    Ok(()) => {
                        warn!(
                            backoff_secs,
                            "gRPC listener exited cleanly, reconnecting..."
                        );
                        // Successful run before exit — reset backoff
                        backoff_secs = 1;
                    }
                    Err(e) => {
                        error!(
                            error = %e,
                            backoff_secs,
                            "gRPC listener error, reconnecting after backoff"
                        );
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
            }
        });

        // Spawn the transaction processing loop
        let token_sender = self.token_sender;
        let processing_handle = tokio::spawn(async move {
            process_raw_transactions(raw_tx_receiver, token_sender).await;
        });

        // Wait for either task to complete (they shouldn't under normal operation)
        tokio::select! {
            result = grpc_handle => {
                error!("gRPC listener task ended unexpectedly: {:?}", result);
            }
            result = processing_handle => {
                error!("Transaction processing task ended unexpectedly: {:?}", result);
            }
        }

        Ok(())
    }
}

/// Process raw transactions from the gRPC stream through all detectors.
async fn process_raw_transactions(
    mut rx: mpsc::Receiver<RawTransaction>,
    token_sender: mpsc::Sender<TokenInfo>,
) {
    let mut dedup_queue = DeduplicationQueue::with_default_ttl();
    let mut cleanup_counter: u64 = 0;

    info!("Transaction processor started, waiting for events...");

    while let Some(raw_tx) = rx.recv().await {
        // Try each detector
        let token_info = match raw_tx.program_id.as_str() {
            pumpfun::PUMPFUN_PROGRAM_ID => process_pumpfun_transaction(&raw_tx),
            raydium::RAYDIUM_AMM_PROGRAM_ID | raydium::RAYDIUM_CPMM_PROGRAM_ID => {
                process_raydium_transaction(&raw_tx)
            }
            pumpswap::PUMPSWAP_PROGRAM_ID => process_pumpswap_transaction(&raw_tx),
            _ => None,
        };

        if let Some(info) = token_info {
            // Deduplicate by mint address
            if !info.mint.is_empty() && dedup_queue.insert(&info.mint) {
                info!(
                    mint = %info.mint,
                    source = %info.source,
                    name = %info.name,
                    symbol = %info.symbol,
                    liquidity_sol = info.initial_liquidity_sol,
                    "New token detected"
                );

                match token_sender.try_send(info) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        warn!("Token channel full — dropping token, analyzer may be overloaded");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        error!("Token channel closed — analyzer has shut down");
                        break;
                    }
                }
            }
        }

        // Periodic cleanup of the dedup queue
        cleanup_counter += 1;
        if cleanup_counter % 1000 == 0 {
            dedup_queue.cleanup();
        }
    }

    warn!("Transaction processor shutting down");
}
