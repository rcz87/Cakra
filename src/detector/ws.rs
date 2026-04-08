use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::StreamExt;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::signature::Signature;
use solana_transaction_status::{EncodedTransaction, UiMessage, UiTransactionEncoding};
use tokio::sync::{mpsc, Mutex, Semaphore};
use tracing::{debug, error, info, warn};

use super::parser::{self, GenericInstruction, RawTransaction, TARGET_PROGRAMS};

const MAX_CONCURRENT_FETCHES: usize = 10;
const DEDUP_CAPACITY: usize = 2000;

/// Start the WebSocket-based detector using `logsSubscribe` + `getTransaction`.
/// This is a fallback for when Yellowstone gRPC is not available.
pub async fn start_ws_listener(
    ws_url: &str,
    rpc_url: &str,
    tx_sender: mpsc::Sender<RawTransaction>,
) -> Result<()> {
    info!(ws_url = %ws_url, "Starting WebSocket detector via logsSubscribe...");

    let seen_sigs: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES));
    let rpc = Arc::new(RpcClient::new(rpc_url.to_string()));

    // Spawn one subscription per target program
    let mut handles = Vec::new();

    for &program_id in TARGET_PROGRAMS {
        let ws_url = ws_url.to_string();
        let tx_sender = tx_sender.clone();
        let seen_sigs = seen_sigs.clone();
        let semaphore = semaphore.clone();
        let rpc = rpc.clone();

        let handle = tokio::spawn(async move {
            if let Err(e) = subscribe_program_logs(
                &ws_url,
                program_id,
                tx_sender,
                seen_sigs,
                semaphore,
                rpc,
            )
            .await
            {
                error!(program_id = %program_id, error = %e, "Program log subscription failed");
            }
        });

        handles.push(handle);
    }

    info!(
        programs = TARGET_PROGRAMS.len(),
        "WebSocket logsSubscribe active — listening for token events"
    );

    // Wait for any subscription to fail
    futures::future::select_all(handles).await.0?;

    warn!("WebSocket listener ended");
    Ok(())
}

async fn subscribe_program_logs(
    ws_url: &str,
    program_id: &str,
    tx_sender: mpsc::Sender<RawTransaction>,
    seen_sigs: Arc<Mutex<HashSet<String>>>,
    semaphore: Arc<Semaphore>,
    rpc: Arc<RpcClient>,
) -> Result<()> {
    use futures::SinkExt;
    use tokio_tungstenite::connect_async;

    let (mut ws_stream, _) = connect_async(ws_url)
        .await
        .context("Failed to connect WebSocket")?;

    // Send logsSubscribe request
    let subscribe_msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "logsSubscribe",
        "params": [
            {"mentions": [program_id]},
            {"commitment": "processed"}
        ]
    });

    ws_stream
        .send(tokio_tungstenite::tungstenite::Message::Text(
            subscribe_msg.to_string(),
        ))
        .await
        .context("Failed to send subscribe message")?;

    // Read subscription confirmation
    if let Some(Ok(msg)) = ws_stream.next().await {
        let text = msg.to_text().unwrap_or("");
        if text.contains("\"error\"") {
            anyhow::bail!("Subscribe failed: {}", text);
        }
        debug!(program_id = %program_id, "logsSubscribe active");
    }

    // Process notifications
    let mut msg_count: u64 = 0;
    while let Some(msg_result) = ws_stream.next().await {
        msg_count += 1;
        if msg_count <= 3 || msg_count % 100 == 0 {
            debug!(program_id = %program_id, msg_count, "WS message received");
        }
        let msg = match msg_result {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "WebSocket message error");
                break;
            }
        };

        let text = match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
            tokio_tungstenite::tungstenite::Message::Ping(_) => continue,
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => continue,
        };

        // Parse the notification
        let notification: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Extract signature and check for errors
        let result = &notification["params"]["result"]["value"];
        if !result["err"].is_null() {
            continue; // Failed transaction
        }

        let signature = match result["signature"].as_str() {
            Some(s) => s.to_string(),
            None => continue,
        };

        // Dedup check
        {
            let mut seen = seen_sigs.lock().await;
            if seen.contains(&signature) {
                continue;
            }
            seen.insert(signature.clone());
            if seen.len() > DEDUP_CAPACITY {
                let to_remove: Vec<String> =
                    seen.iter().take(DEDUP_CAPACITY / 2).cloned().collect();
                for sig in to_remove {
                    seen.remove(&sig);
                }
            }
        }

        // Fetch full transaction in background
        let tx_sender = tx_sender.clone();
        let rpc = rpc.clone();
        let semaphore = semaphore.clone();

        tokio::spawn(async move {
            let _permit = match semaphore.acquire().await {
                Ok(p) => p,
                Err(_) => return,
            };

            // Brief delay for tx to be confirmed
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;

            match fetch_and_parse_transaction(&rpc, &signature).await {
                Ok(raw_txs) => {
                    for raw_tx in raw_txs {
                        if let Err(e) = tx_sender.send(raw_tx).await {
                            warn!(error = %e, "Failed to send WS-detected transaction");
                        }
                    }
                }
                Err(e) => {
                    debug!(sig = %signature, error = %e, "Failed to fetch transaction");
                }
            }
        });
    }

    warn!(program_id = %program_id, "logsSubscribe stream ended");
    Ok(())
}

/// Fetch a transaction by signature and extract RawTransactions from it.
async fn fetch_and_parse_transaction(
    rpc: &RpcClient,
    signature_str: &str,
) -> Result<Vec<RawTransaction>> {
    let sig = Signature::from_str(signature_str).context("Invalid signature")?;

    let config = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Json),
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };

    let tx_response = rpc
        .get_transaction_with_config(&sig, config)
        .await
        .context("getTransaction failed")?;

    let slot = tx_response.slot;

    // Extract UiTransaction from the encoded response
    let ui_tx = match &tx_response.transaction.transaction {
        EncodedTransaction::Json(ui_tx) => ui_tx,
        _ => anyhow::bail!("Expected Json-encoded transaction"),
    };

    // Extract account keys and instructions from UiMessage
    let (account_keys, instructions) = match &ui_tx.message {
        UiMessage::Raw(raw) => {
            let keys = raw.account_keys.clone();
            let ixs: Vec<GenericInstruction> = raw
                .instructions
                .iter()
                .map(|ix| GenericInstruction {
                    program_id_index: ix.program_id_index as usize,
                    accounts: ix.accounts.iter().map(|&a| a as usize).collect(),
                    data: bs58::decode(&ix.data).into_vec().unwrap_or_default(),
                })
                .collect();
            (keys, ixs)
        }
        UiMessage::Parsed(parsed) => {
            let keys: Vec<String> = parsed
                .account_keys
                .iter()
                .map(|k| k.pubkey.clone())
                .collect();
            (keys, Vec::new())
        }
    };

    // Append loaded addresses for versioned transactions
    let mut account_keys = account_keys;
    if let Some(meta) = &tx_response.transaction.meta {
        use solana_transaction_status::option_serializer::OptionSerializer;
        if let OptionSerializer::Some(loaded) = &meta.loaded_addresses {
            for addr in &loaded.writable {
                account_keys.push(addr.to_string());
            }
            for addr in &loaded.readonly {
                account_keys.push(addr.to_string());
            }
        }
    }

    // Extract inner instructions from transaction meta
    let mut inner_instructions = Vec::new();
    if let Some(meta) = &tx_response.transaction.meta {
        use solana_transaction_status::option_serializer::OptionSerializer;
        if let OptionSerializer::Some(inner_ixs) = &meta.inner_instructions {
            for group in inner_ixs {
                for ix in &group.instructions {
                    if let solana_transaction_status::UiInstruction::Compiled(compiled) = ix {
                        inner_instructions.push(GenericInstruction {
                            program_id_index: compiled.program_id_index as usize,
                            accounts: compiled.accounts.iter().map(|&a| a as usize).collect(),
                            data: bs58::decode(&compiled.data)
                                .into_vec()
                                .unwrap_or_default(),
                        });
                    }
                }
            }
        }
    }

    Ok(parser::extract_raw_transactions(
        signature_str,
        slot,
        &account_keys,
        &instructions,
        &inner_instructions,
    ))
}
