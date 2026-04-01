mod analyzer;
mod config;
mod db;
mod detector;
mod executor;
mod models;
mod monitoring;
mod risk;
mod security;
mod telegram;
mod wallet;

use std::sync::Arc;

use anyhow::Result;
use tokio::signal;
use tokio::sync::broadcast;
use tracing::{error, info};

use crate::config::Config;
use crate::monitoring::init_logging;
use crate::risk::{CooldownManager, ListManager, RiskManager};
use crate::telegram::TelegramBot;

/// Shared application state available across all services.
pub struct AppState {
    pub config: Config,
    pub db: db::DbPool,
    pub risk: RiskManager,
    pub lists: ListManager,
    pub cooldown: CooldownManager,
}

#[tokio::main]
async fn main() -> Result<()> {
    // ── Load configuration from .env ───────────────────────────
    let config = Config::from_env()?;

    // ── Initialize logging ─────────────────────────────────────
    let log_file = std::env::var("LOG_FILE").ok();
    init_logging(log_file.as_deref())?;

    info!("╔══════════════════════════════════════════╗");
    info!("║         RICOZ SNIPER  v0.1.0             ║");
    info!("║    Solana Auto-Trading Sniper Bot        ║");
    info!("╚══════════════════════════════════════════╝");

    // ── Initialize database ────────────────────────────────────
    let db = db::init_database(&config.database_path)?;
    info!(path = %config.database_path, "Database initialized");

    // ── Build shared application state ─────────────────────────
    let risk = RiskManager::new(config.clone(), db.clone());
    let lists = ListManager::new(db.clone());
    let cooldown = CooldownManager::new(config.trade_cooldown_secs);

    let state = Arc::new(AppState {
        config: config.clone(),
        db: db.clone(),
        risk,
        lists,
        cooldown,
    });

    // ── Shutdown signal channel ────────────────────────────────
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // ── Spawn detector service ─────────────────────────────────
    let detector_shutdown = shutdown_tx.subscribe();
    let detector_config = config.clone();
    let detector_db = db.clone();
    let detector_handle = tokio::spawn(async move {
        info!("Detector service starting...");
        // The detector listens for new token launches via gRPC / WebSocket
        // and pushes them into the analysis pipeline.
        // detector::run(detector_config, detector_db, detector_shutdown).await
        let _ = (detector_config, detector_db, detector_shutdown);
        info!("Detector service placeholder active");
        // Block until shutdown
        tokio::signal::ctrl_c().await.ok();
    });

    // ── Spawn analyzer pipeline ────────────────────────────────
    let analyzer_shutdown = shutdown_tx.subscribe();
    let analyzer_config = config.clone();
    let analyzer_db = db.clone();
    let analyzer_handle = tokio::spawn(async move {
        info!("Analyzer pipeline starting...");
        // The analyzer receives detected tokens, runs security checks,
        // scores them, and passes qualifying tokens to the executor.
        // analyzer::run(analyzer_config, analyzer_db, analyzer_shutdown).await
        let _ = (analyzer_config, analyzer_db, analyzer_shutdown);
        info!("Analyzer pipeline placeholder active");
        tokio::signal::ctrl_c().await.ok();
    });

    // ── Spawn TP/SL monitor ────────────────────────────────────
    let tpsl_shutdown = shutdown_tx.subscribe();
    let tpsl_config = config.clone();
    let tpsl_db = db.clone();
    let tpsl_handle = tokio::spawn(async move {
        info!("TP/SL monitor starting...");
        // Monitors open positions and triggers take-profit or stop-loss sells.
        // executor::tp_sl::run(tpsl_config, tpsl_db, tpsl_shutdown).await
        let _ = (tpsl_config, tpsl_db, tpsl_shutdown);
        info!("TP/SL monitor placeholder active");
        tokio::signal::ctrl_c().await.ok();
    });

    // ── Start Telegram bot (blocking) ──────────────────────────
    info!("Starting Telegram bot (this blocks until shutdown)...");
    let bot_config = config.clone();
    let bot_db = db.clone();
    let bot_handle = tokio::spawn(async move {
        if let Err(e) = TelegramBot::start(bot_config, bot_db).await {
            error!(err = %e, "Telegram bot exited with error");
        }
    });

    // ── Graceful shutdown ──────────────────────────────────────
    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("Received Ctrl+C, initiating graceful shutdown...");
        }
    }

    // Broadcast shutdown to all services.
    let _ = shutdown_tx.send(());

    info!("Waiting for services to shut down...");

    // Give services a moment to clean up, then abort if needed.
    let shutdown_timeout = tokio::time::Duration::from_secs(10);
    let _ = tokio::time::timeout(shutdown_timeout, async {
        let _ = tokio::join!(detector_handle, analyzer_handle, tpsl_handle, bot_handle);
    })
    .await;

    info!("RICOZ SNIPER shut down cleanly. Goodbye!");
    Ok(())
}
