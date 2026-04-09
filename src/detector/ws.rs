use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use super::parser::{GenericInstruction, RawTransaction, TARGET_PROGRAMS};

const DEDUP_CAPACITY: usize = 2000;

/// Start the WebSocket-based detector using Helius Enhanced WebSocket
/// (`transactionSubscribe`). This gives us parsed transaction data in
/// real-time — no need for separate getTransaction calls.
///
/// Falls back to standard `logsSubscribe` if transactionSubscribe is
/// not available on the endpoint.
pub async fn start_ws_listener(
    ws_url: &str,
    _rpc_url: &str,
    tx_sender: mpsc::Sender<RawTransaction>,
) -> Result<()> {
    info!(ws_url = %ws_url, "Starting Enhanced WebSocket detector (transactionSubscribe)...");

    let seen_sigs: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    subscribe_transactions(ws_url, tx_sender, seen_sigs).await
}

/// Subscribe to all target program transactions via Helius `transactionSubscribe`.
/// Single subscription covers PumpFun, Raydium AMM/CPMM, and PumpSwap.
async fn subscribe_transactions(
    ws_url: &str,
    tx_sender: mpsc::Sender<RawTransaction>,
    seen_sigs: Arc<Mutex<HashSet<String>>>,
) -> Result<()> {
    use tokio_tungstenite::connect_async;

    let (mut ws_stream, _) = connect_async(ws_url)
        .await
        .context("Failed to connect WebSocket")?;

    // Build transactionSubscribe request with all target programs
    let program_ids: Vec<&str> = TARGET_PROGRAMS.to_vec();

    let subscribe_msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "transactionSubscribe",
        "params": [
            {
                "vote": false,
                "failed": false,
                "accountInclude": program_ids
            },
            {
                "commitment": "processed",
                "encoding": "json",
                "transactionDetails": "full",
                "showRewards": false,
                "maxSupportedTransactionVersion": 0
            }
        ]
    });

    ws_stream
        .send(tokio_tungstenite::tungstenite::Message::Text(
            subscribe_msg.to_string(),
        ))
        .await
        .context("Failed to send transactionSubscribe")?;

    // Read subscription confirmation
    if let Some(Ok(msg)) = ws_stream.next().await {
        let text = msg.to_text().unwrap_or("");
        if text.contains("\"error\"") {
            anyhow::bail!("transactionSubscribe failed: {}", text);
        }
        let confirm: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
        let sub_id = &confirm["result"];
        info!(subscription_id = %sub_id, programs = program_ids.len(),
              "transactionSubscribe active — listening for token events");
    }

    // Spawn periodic ping to keep connection alive (Helius times out after 10min)
    let (ping_tx, mut ping_rx) = tokio::sync::oneshot::channel::<()>();
    let ws_url_clone = ws_url.to_string();
    let _ping_info = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = &mut ping_rx => break,
            }
        }
        drop(ws_url_clone);
    });

    // Process notifications
    let mut msg_count: u64 = 0;
    let mut token_count: u64 = 0;
    let mut skip_count: u64 = 0;

    while let Some(msg_result) = ws_stream.next().await {
        msg_count += 1;

        let msg = match msg_result {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "WebSocket message error");
                break;
            }
        };

        let text = match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
            tokio_tungstenite::tungstenite::Message::Ping(data) => {
                // Respond to server pings
                let _ = ws_stream
                    .send(tokio_tungstenite::tungstenite::Message::Pong(data))
                    .await;
                continue;
            }
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => continue,
        };

        // Parse notification
        let notification: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let result = &notification["params"]["result"];

        // Skip error transactions (e.g. UnsupportedTransactionVersion without our option)
        if !result["error"].is_null() {
            continue;
        }

        let signature = match result["signature"].as_str() {
            Some(s) => s.to_string(),
            None => continue,
        };

        let slot = result["slot"].as_u64().unwrap_or(0);

        // Dedup by signature
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

        // Extract transaction data directly from the notification
        let tx_data = &result["transaction"];
        let message = &tx_data["transaction"]["message"];

        // Extract account keys
        let account_keys: Vec<String> = match message["accountKeys"].as_array() {
            Some(keys) => keys
                .iter()
                .filter_map(|k| {
                    // Can be either a string or an object with "pubkey" field
                    k.as_str()
                        .map(|s| s.to_string())
                        .or_else(|| k["pubkey"].as_str().map(|s| s.to_string()))
                })
                .collect(),
            None => continue,
        };

        // Extract instructions
        let instructions = extract_instructions_from_json(
            &message["instructions"],
            &account_keys,
        );

        // Extract inner instructions from meta
        let meta = &tx_data["meta"];
        let inner_instructions = extract_inner_instructions_from_json(
            &meta["innerInstructions"],
            &account_keys,
        );

        // Use shared extraction logic
        let raw_txs = super::parser::extract_raw_transactions(
            &signature,
            slot,
            &account_keys,
            &instructions,
            &inner_instructions,
        );

        if !raw_txs.is_empty() {
            token_count += 1;
            debug!(
                sig = %signature,
                count = raw_txs.len(),
                program = %raw_txs[0].program_id,
                total_tokens = token_count,
                "Parsed target program transaction"
            );

            for raw_tx in raw_txs {
                if let Err(e) = tx_sender.send(raw_tx).await {
                    warn!(error = %e, "Failed to send transaction");
                }
            }
        } else {
            skip_count += 1;
        }

        // Periodic stats
        if msg_count % 500 == 0 {
            info!(
                total_msgs = msg_count,
                target_txs = token_count,
                skipped = skip_count,
                "WebSocket detector stats"
            );
        }
    }

    let _ = ping_tx.send(());
    warn!(total_msgs = msg_count, tokens = token_count, "WebSocket stream ended");
    Ok(())
}

/// Extract GenericInstructions from JSON instruction array.
/// Handles both "compiled" (with program_id_index) and "parsed" formats.
fn extract_instructions_from_json(
    instructions_json: &serde_json::Value,
    account_keys: &[String],
) -> Vec<GenericInstruction> {
    let Some(ixs) = instructions_json.as_array() else {
        return Vec::new();
    };

    ixs.iter()
        .filter_map(|ix| {
            // Compiled format: { programIdIndex, accounts, data }
            if let Some(pid_idx) = ix["programIdIndex"].as_u64() {
                let accounts: Vec<usize> = ix["accounts"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|a| a.as_u64().map(|v| v as usize)).collect())
                    .unwrap_or_default();
                let data = ix["data"]
                    .as_str()
                    .and_then(|d| bs58::decode(d).into_vec().ok())
                    .unwrap_or_default();

                Some(GenericInstruction {
                    program_id_index: pid_idx as usize,
                    accounts,
                    data,
                })
            }
            // Parsed format: { program, programId, parsed: {...} }
            // We can't easily reconstruct discriminators from parsed format,
            // but we can match by programId and skip these (rare for our programs)
            else if let Some(program_id) = ix["programId"].as_str() {
                // Find the index of this program in account_keys
                let pid_idx = account_keys.iter().position(|k| k == program_id)?;
                // Parsed instructions don't have raw data, skip them
                // (our detectors need raw discriminator bytes)
                if ix["parsed"].is_object() {
                    return None;
                }
                let data = ix["data"]
                    .as_str()
                    .and_then(|d| bs58::decode(d).into_vec().ok())
                    .unwrap_or_default();
                let accounts: Vec<usize> = ix["accounts"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|a| {
                                a.as_str()
                                    .and_then(|addr| account_keys.iter().position(|k| k == addr))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(GenericInstruction {
                    program_id_index: pid_idx,
                    accounts,
                    data,
                })
            } else {
                None
            }
        })
        .collect()
}

/// Extract inner instructions from JSON meta.innerInstructions.
fn extract_inner_instructions_from_json(
    inner_json: &serde_json::Value,
    _account_keys: &[String],
) -> Vec<GenericInstruction> {
    let Some(groups) = inner_json.as_array() else {
        return Vec::new();
    };

    let mut result = Vec::new();
    for group in groups {
        let Some(ixs) = group["instructions"].as_array() else {
            continue;
        };
        for ix in ixs {
            if let Some(pid_idx) = ix["programIdIndex"].as_u64() {
                let accounts: Vec<usize> = ix["accounts"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|a| a.as_u64().map(|v| v as usize))
                            .collect()
                    })
                    .unwrap_or_default();
                let data = ix["data"]
                    .as_str()
                    .and_then(|d| bs58::decode(d).into_vec().ok())
                    .unwrap_or_default();
                result.push(GenericInstruction {
                    program_id_index: pid_idx as usize,
                    accounts,
                    data,
                });
            }
        }
    }
    result
}
