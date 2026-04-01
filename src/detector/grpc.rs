use std::collections::HashMap;

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

use super::parser::RawTransaction;
use super::pumpfun::PUMPFUN_PROGRAM_ID;
use super::pumpswap::PUMPSWAP_PROGRAM_ID;
use super::raydium::{RAYDIUM_AMM_PROGRAM_ID, RAYDIUM_CPMM_PROGRAM_ID};

/// Start the Yellowstone gRPC subscription and forward raw transactions
/// through the provided channel.
pub async fn start_grpc_listener(
    endpoint: &str,
    token: &str,
    tx_sender: mpsc::Sender<RawTransaction>,
) -> Result<()> {
    info!(endpoint = %endpoint, "Connecting to Yellowstone gRPC...");

    let mut client = GeyserGrpcClient::build_from_shared(endpoint.to_string())?
        .x_token(Some(token.to_string()))?
        .tls_config(ClientTlsConfig::new().with_native_roots())?
        .connect()
        .await
        .context("Failed to connect to Yellowstone gRPC")?;

    info!("Connected to Yellowstone gRPC");

    // Build the subscription request for program transactions
    let mut transaction_filters: HashMap<String, SubscribeRequestFilterTransactions> =
        HashMap::new();

    // Subscribe to transactions involving our target programs
    let program_ids = vec![
        PUMPFUN_PROGRAM_ID.to_string(),
        RAYDIUM_AMM_PROGRAM_ID.to_string(),
        RAYDIUM_CPMM_PROGRAM_ID.to_string(),
        PUMPSWAP_PROGRAM_ID.to_string(),
    ];

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

    let (mut subscribe_tx, mut stream) = client
        .subscribe_with_request(Some(subscribe_request))
        .await
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

        // Process each instruction
        for instruction in &message.instructions {
            let program_idx = instruction.program_id_index as usize;
            let program_id = match account_keys.get(program_idx) {
                Some(id) => id.clone(),
                None => continue,
            };

            // Only process instructions for our target programs
            if program_id != PUMPFUN_PROGRAM_ID
                && program_id != RAYDIUM_AMM_PROGRAM_ID
                && program_id != RAYDIUM_CPMM_PROGRAM_ID
                && program_id != PUMPSWAP_PROGRAM_ID
            {
                continue;
            }

            // Resolve account indices for this instruction
            let instruction_accounts: Vec<String> = instruction
                .accounts
                .iter()
                .filter_map(|&idx| account_keys.get(idx as usize).cloned())
                .collect();

            let raw_tx = RawTransaction {
                signature: signature.clone(),
                program_id,
                data: instruction.data.clone(),
                accounts: instruction_accounts,
                slot,
            };

            if let Err(e) = tx_sender.send(raw_tx).await {
                warn!(error = %e, "Failed to send raw transaction to processing channel");
            }
        }

        // Also process inner instructions (CPI calls)
        if let Some(meta) = tx_info.meta {
            for inner_instructions in &meta.inner_instructions {
                for inner_ix in &inner_instructions.instructions {
                    let program_idx = inner_ix.program_id_index as usize;
                    let program_id = match account_keys.get(program_idx) {
                        Some(id) => id.clone(),
                        None => continue,
                    };

                    if program_id != PUMPFUN_PROGRAM_ID
                        && program_id != RAYDIUM_AMM_PROGRAM_ID
                        && program_id != RAYDIUM_CPMM_PROGRAM_ID
                        && program_id != PUMPSWAP_PROGRAM_ID
                    {
                        continue;
                    }

                    let instruction_accounts: Vec<String> = inner_ix
                        .accounts
                        .iter()
                        .filter_map(|&idx| account_keys.get(idx as usize).cloned())
                        .collect();

                    let raw_tx = RawTransaction {
                        signature: signature.clone(),
                        program_id,
                        data: inner_ix.data.clone(),
                        accounts: instruction_accounts,
                        slot,
                    };

                    if let Err(e) = tx_sender.send(raw_tx).await {
                        warn!(error = %e, "Failed to send inner instruction to processing channel");
                    }
                }
            }
        }
    }
}
