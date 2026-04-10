use anyhow::{Context, Result};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::models::token::{DetectionBackend, TokenInfo, TokenSource};

/// PumpPortal WebSocket endpoint for real-time data.
const PUMPPORTAL_WS_URL: &str = "wss://pumpportal.fun/api/data";

/// Start the PumpPortal detector.
/// Subscribes to `subscribeNewToken` and `subscribeMigration` events.
/// Returns parsed TokenInfo directly — no discriminator parsing needed.
pub async fn start_pumpportal_listener(
    token_sender: mpsc::Sender<TokenInfo>,
) -> Result<()> {
    info!("Starting PumpPortal detector (subscribeNewToken + subscribeMigration)...");

    use tokio_tungstenite::connect_async;

    let (mut ws_stream, _) = connect_async(PUMPPORTAL_WS_URL)
        .await
        .context("Failed to connect to PumpPortal WebSocket")?;

    // Subscribe to new token creation events
    let subscribe_new = serde_json::json!({"method": "subscribeNewToken"});
    ws_stream
        .send(tokio_tungstenite::tungstenite::Message::Text(
            subscribe_new.to_string(),
        ))
        .await
        .context("Failed to subscribe to new tokens")?;

    // Subscribe to migration events (PumpSwap)
    let subscribe_migration = serde_json::json!({"method": "subscribeMigration"});
    ws_stream
        .send(tokio_tungstenite::tungstenite::Message::Text(
            subscribe_migration.to_string(),
        ))
        .await
        .context("Failed to subscribe to migrations")?;

    let mut token_count: u64 = 0;
    let mut migration_count: u64 = 0;

    while let Some(msg_result) = ws_stream.next().await {
        let msg = match msg_result {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "PumpPortal WebSocket error");
                break;
            }
        };

        let text = match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
            tokio_tungstenite::tungstenite::Message::Ping(data) => {
                let _ = ws_stream
                    .send(tokio_tungstenite::tungstenite::Message::Pong(data))
                    .await;
                continue;
            }
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => continue,
        };

        let event: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Skip confirmation messages
        if event.get("message").is_some() {
            let msg = event["message"].as_str().unwrap_or("");
            info!(msg = %msg, "PumpPortal subscription confirmed");
            continue;
        }

        // Determine event type
        let tx_type = event["txType"].as_str().unwrap_or("");

        match tx_type {
            "create" => {
                if let Some(token_info) = parse_create_event(&event) {
                    token_count += 1;
                    debug!(
                        mint = %token_info.mint,
                        name = %token_info.name,
                        symbol = %token_info.symbol,
                        liquidity_sol = token_info.initial_liquidity_sol,
                        total = token_count,
                        "PumpPortal: new token"
                    );
                    if let Err(e) = token_sender.send(token_info).await {
                        warn!(error = %e, "Failed to send PumpPortal token");
                    }
                }
            }
            "migration" => {
                if let Some(token_info) = parse_migration_event(&event) {
                    migration_count += 1;
                    info!(
                        mint = %token_info.mint,
                        pool = %token_info.pool_address.as_deref().unwrap_or("?"),
                        liquidity_sol = token_info.initial_liquidity_sol,
                        total = migration_count,
                        "PumpPortal: migration detected"
                    );
                    if let Err(e) = token_sender.send(token_info).await {
                        warn!(error = %e, "Failed to send PumpPortal migration");
                    }
                }
            }
            _ => {
                // Unknown event type, skip
            }
        }
    }

    warn!(tokens = token_count, migrations = migration_count, "PumpPortal stream ended");
    Ok(())
}

/// Parse a PumpPortal "create" event into TokenInfo.
///
/// Example event:
/// ```json
/// {
///   "signature": "...",
///   "mint": "...pump",
///   "traderPublicKey": "...",
///   "txType": "create",
///   "initialBuy": 176989690.698557,
///   "solAmount": 5.925925925,
///   "bondingCurveKey": "...",
///   "vSolInBondingCurve": 35.925,
///   "marketCapSol": 40.09,
///   "name": "Token Name",
///   "symbol": "SYMBOL",
///   "uri": "https://...",
///   "pool": "pump"
/// }
/// ```
fn parse_create_event(event: &serde_json::Value) -> Option<TokenInfo> {
    let mint = event["mint"].as_str()?.to_string();
    let name = event["name"].as_str().unwrap_or("").to_string();
    let symbol = event["symbol"].as_str().unwrap_or("").to_string();
    let creator = event["traderPublicKey"].as_str().unwrap_or("").to_string();
    let uri = event["uri"].as_str().map(|s| s.to_string());
    let sol_amount = event["solAmount"].as_f64().unwrap_or(0.0);
    let bonding_curve = event["bondingCurveKey"].as_str().map(|s| s.to_string());

    let market_cap_sol = event["marketCapSol"].as_f64().unwrap_or(0.0);
    let v_sol_in_bonding_curve = event["vSolInBondingCurve"].as_f64().unwrap_or(0.0);

    Some(TokenInfo {
        mint,
        name,
        symbol,
        source: TokenSource::PumpFun,
        creator,
        initial_liquidity_sol: sol_amount,
        initial_liquidity_usd: 0.0,
        pool_address: bonding_curve,
        metadata_uri: uri,
        decimals: 6,
        detected_at: Utc::now(),
        backend: DetectionBackend::PumpPortal,
        market_cap_sol,
        v_sol_in_bonding_curve,
        initial_buy_sol: sol_amount,
    })
}

/// Sentinel value used in TokenInfo.creator to mark a migration event.
/// Allows reliable downstream detection without fragile empty-string checks.
pub const MIGRATION_EVENT_MARKER: &str = "MIGRATION_EVENT";

/// Parse a PumpPortal "migration" event into TokenInfo.
/// Sets creator = MIGRATION_EVENT_MARKER so downstream pipelines can
/// identify this as a migration vs a fresh launch reliably.
fn parse_migration_event(event: &serde_json::Value) -> Option<TokenInfo> {
    let mint = event["mint"].as_str()?.to_string();
    let pool = event["pool"].as_str().unwrap_or("pump-amm").to_string();

    // Migration events may have different fields
    let name = event["name"].as_str().unwrap_or("").to_string();
    let symbol = event["symbol"].as_str().unwrap_or("").to_string();
    let sol_amount = event["solAmount"].as_f64().unwrap_or(0.0);
    let pool_address = event["poolAddress"]
        .as_str()
        .or(event["bondingCurveKey"].as_str())
        .map(|s| s.to_string());

    let source = if pool == "raydium" || pool == "raydium-cpmm" {
        TokenSource::Raydium
    } else {
        TokenSource::PumpSwap
    };

    Some(TokenInfo {
        mint,
        name,
        symbol,
        source,
        creator: MIGRATION_EVENT_MARKER.to_string(),
        initial_liquidity_sol: sol_amount,
        initial_liquidity_usd: 0.0,
        pool_address,
        metadata_uri: None,
        decimals: 6,
        backend: DetectionBackend::PumpPortal,
        market_cap_sol: event["marketCapSol"].as_f64().unwrap_or(0.0),
        v_sol_in_bonding_curve: 0.0,
        initial_buy_sol: sol_amount,
        detected_at: Utc::now(),
    })
}
