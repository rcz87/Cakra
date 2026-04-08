use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tonic::transport::ClientTlsConfig;
use tracing::{error, info, warn};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::prelude::{
    CommitmentLevel, SubscribeRequest, SubscribeRequestFilterTransactions,
    SubscribeRequestPing,
};

const GRPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const GRPC_SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(10);

use super::parser::{self, GenericInstruction, RawTransaction, TARGET_PROGRAMS};

/// Start the Yellowstone gRPC subscription and forward raw transactions
/// through the provided channel.
pub async fn start_grpc_listener(
    endpoint: &str,
    token: &str,
    tx_sender: mpsc::Sender<RawTransaction>,
) -> Result<()> {
    info!(endpoint = %endpoint, "Connecting to Yellowstone gRPC...");

    let connect_fut = GeyserGrpcClient::build_from_shared(endpoint.to_string())?
        .x_token(Some(token.to_string()))?
        .tls_config(ClientTlsConfig::new().with_native_roots())?
        .connect();

    let mut client = tokio::time::timeout(GRPC_CONNECT_TIMEOUT, connect_fut)
        .await
        .map_err(|_| anyhow::anyhow!("gRPC connect timed out after {:?}", GRPC_CONNECT_TIMEOUT))?
        .context("Failed to connect to Yellowstone gRPC")?;

    info!("Connected to Yellowstone gRPC");

    // Build the subscription request for program transactions
    let mut transaction_filters: HashMap<String, SubscribeRequestFilterTransactions> =
        HashMap::new();

    // Subscribe to transactions involving our target programs
    let program_ids: Vec<String> = TARGET_PROGRAMS.iter().map(|s| s.to_string()).collect();

    transaction_filters.insert(
        "token_detectors".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            account_include: program_ids,
            account_exclude: vec![],
            account_required: vec![],
            signature: None,
        },
    );

    let subscribe_request = SubscribeRequest {
        slots: HashMap::new(),
        accounts: HashMap::new(),
        transactions: transaction_filters,
        transactions_status: HashMap::new(),
        blocks: HashMap::new(),
        blocks_meta: HashMap::new(),
        entry: HashMap::new(),
        commitment: Some(CommitmentLevel::Processed as i32),
        accounts_data_slice: vec![],
        ping: None,
        from_slot: None,
    };

    let subscribe_fut = client.subscribe_with_request(Some(subscribe_request));
    let (mut subscribe_tx, mut stream) =
        tokio::time::timeout(GRPC_SUBSCRIBE_TIMEOUT, subscribe_fut)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "gRPC subscribe timed out after {:?} — endpoint may not support Yellowstone",
                    GRPC_SUBSCRIBE_TIMEOUT
                )
            })?
            .context("Failed to subscribe to gRPC stream")?;

    info!("gRPC subscription active - listening for token events");

    // Spawn a keepalive ping task
    let ping_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let ping_request = SubscribeRequest {
                ping: Some(SubscribeRequestPing { id: 1 }),
                ..Default::default()
            };
            if subscribe_tx.send(ping_request).await.is_err() {
                warn!("gRPC ping channel closed");
                break;
            }
        }
    });

    // Process incoming updates
    while let Some(update_result) = stream.next().await {
        match update_result {
            Ok(update) => {
                if let Some(update_oneof) = update.update_oneof {
                    process_grpc_update(update_oneof, &tx_sender).await;
                }
            }
            Err(e) => {
                error!(error = %e, "gRPC stream error");
                break;
            }
        }
    }

    ping_handle.abort();
    warn!("gRPC stream ended");

    Ok(())
}

/// Process a single gRPC update and extract raw transactions.
async fn process_grpc_update(
    update: yellowstone_grpc_proto::prelude::subscribe_update::UpdateOneof,
    tx_sender: &mpsc::Sender<RawTransaction>,
) {
    use yellowstone_grpc_proto::prelude::subscribe_update::UpdateOneof;

    if let UpdateOneof::Transaction(tx_update) = update {
        let Some(tx_info) = tx_update.transaction else {
            return;
        };
        let Some(transaction) = tx_info.transaction else {
            return;
        };
        let Some(message) = transaction.message else {
            return;
        };

        let signature = bs58::encode(&tx_info.signature).into_string();
        let slot = tx_update.slot;

        // Collect all account keys
        let account_keys: Vec<String> = message
            .account_keys
            .iter()
            .map(|k| bs58::encode(k).into_string())
            .collect();

        // Convert gRPC instructions to generic format
        let instructions: Vec<GenericInstruction> = message
            .instructions
            .iter()
            .map(|ix| GenericInstruction {
                program_id_index: ix.program_id_index as usize,
                accounts: ix.accounts.iter().map(|&a| a as usize).collect(),
                data: ix.data.clone(),
            })
            .collect();

        // Convert inner instructions (CPI calls)
        let mut inner_instructions = Vec::new();
        if let Some(meta) = &tx_info.meta {
            for inner_group in &meta.inner_instructions {
                for inner_ix in &inner_group.instructions {
                    inner_instructions.push(GenericInstruction {
                        program_id_index: inner_ix.program_id_index as usize,
                        accounts: inner_ix.accounts.iter().map(|&a| a as usize).collect(),
                        data: inner_ix.data.clone(),
                    });
                }
            }
        }

        // Use shared extraction logic
        let raw_txs =
            parser::extract_raw_transactions(&signature, slot, &account_keys, &instructions, &inner_instructions);

        for raw_tx in raw_txs {
            if let Err(e) = tx_sender.send(raw_tx).await {
                warn!(error = %e, "Failed to send raw transaction to processing channel");
            }
        }
    }
}
