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
use teloxide::payloads::SendMessageSetters as _;
use teloxide::prelude::Requester;
use tokio::signal;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::monitoring::init_logging;
use crate::risk::{CooldownManager, ListManager, RiskManager};
use crate::telegram::TelegramBot;
use crate::wallet::WalletManager;

use crate::analyzer::entry_confirmation::{confirm_entry, EntryConfirmation, EntryDecision};
use crate::analyzer::AnalyzerService;
use crate::detector::DetectorService;
use crate::executor::positions::PositionManager;
use crate::executor::price_feed::PriceFeed;
use crate::executor::tp_sl::{process_tp_sl_commands, TpSlMonitor};
use crate::executor::ExecutorService;

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
    info!("║         RICOZ SNIPER  v0.2.0             ║");
    info!("║    Solana Auto-Trading Sniper Bot        ║");
    info!("╚══════════════════════════════════════════╝");

    // ── Initialize database ────────────────────────────────────
    let db = db::init_database(&config.database_path)?;
    info!(path = %config.database_path, "Database initialized");

    // ── Build shared application state ─────────────────────────
    let risk = RiskManager::new(config.clone(), db.clone());
    let lists = ListManager::new(db.clone());
    let cooldown = CooldownManager::new(config.trade_cooldown_secs);

    let _state = Arc::new(AppState {
        config: config.clone(),
        db: db.clone(),
        risk,
        lists,
        cooldown,
    });

    // ── Shutdown signal channel ────────────────────────────────
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // ── Create shared components ───────────────────────────────
    let position_manager = PositionManager::new(db.clone());

    // Load any persisted open positions from previous session
    if let Err(e) = position_manager.load_from_db() {
        warn!(error = %e, "Failed to load positions from database");
    }

    // ── Wallet Manager (shared for buy/sell) ─────────────────────
    let wallet_password = std::env::var("WALLET_PASSWORD")
        .expect("WALLET_PASSWORD must be set — cannot start without wallet encryption password");
    if wallet_password.len() < 8 {
        panic!("WALLET_PASSWORD must be at least 8 characters for wallet security");
    }
    let wallet_manager = Arc::new(
        WalletManager::new(&config, db.clone()).expect("Failed to create wallet manager"),
    );

    let executor = Arc::new(ExecutorService::new(
        Arc::new(config.clone()),
        db.clone(),
        RiskManager::new(config.clone(), db.clone()),
        CooldownManager::new(config.trade_cooldown_secs),
        ListManager::new(db.clone()),
        position_manager.clone(),
    ));

    // ── Channel: Detector → Analyzer ───────────────────────────
    // DetectorService creates its own internal channel and returns the receiver
    let (detector_service, mut token_rx) = DetectorService::new(config.clone());

    // ── Channel: TP/SL → Executor (sell commands) ──────────────
    let (sell_tx, mut sell_rx) = mpsc::channel::<(String, u8)>(64);

    // ── Channel: PriceFeed → TpSlMonitor ───────────────────────
    let (price_tx, price_rx) = mpsc::channel::<(String, f64)>(256);

    // ── Spawn Detector Service ─────────────────────────────────
    let mut detector_shutdown = shutdown_tx.subscribe();
    let detector_handle = tokio::spawn(async move {
        info!("Detector service starting...");
        tokio::select! {
            result = detector_service.start() => {
                if let Err(e) = result {
                    error!(error = %e, "Detector service exited with error");
                }
            }
            _ = detector_shutdown.recv() => {
                info!("Detector service received shutdown signal");
            }
        }
    });

    // ── Spawn Analyzer Pipeline ────────────────────────────────
    let analyzer = AnalyzerService::new(config.clone());
    let analyzer_config = config.clone();
    let analyzer_db = db.clone();
    let analyzer_executor = executor.clone();
    let analyzer_wallet = wallet_manager.clone();
    let analyzer_password = wallet_password.clone();
    let analyzer_positions = position_manager.clone();
    let mut analyzer_shutdown = shutdown_tx.subscribe();
    let analyzer_handle = tokio::spawn(async move {
        info!("Analyzer pipeline starting...");

        let rpc = solana_client::rpc_client::RpcClient::new(
            analyzer_config.effective_rpc_url().to_string(),
        );

        // Telegram bot instance for sending notifications
        let tg_bot = teloxide::Bot::new(&analyzer_config.telegram_bot_token);
        let tg_chat = teloxide::types::ChatId(analyzer_config.telegram_admin_chat_id);

        loop {
            tokio::select! {
                Some(token) = token_rx.recv() => {
                    info!(
                        mint = %token.mint,
                        symbol = %token.symbol,
                        source = %token.source,
                        "Analyzer received token from detector"
                    );

                    // Check blacklist first
                    match db::queries::is_blacklisted(&analyzer_db, &token.mint) {
                        Ok(true) => {
                            info!(mint = %token.mint, "Token is blacklisted, skipping");
                            continue;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            warn!(error = %e, "Blacklist check failed, proceeding anyway");
                        }
                    }

                    // Store token in database
                    if let Err(e) = db::queries::insert_token(&analyzer_db, &token) {
                        warn!(error = %e, "Failed to store token in database");
                    }

                    // Run security analysis
                    match analyzer.analyze_token(&token, &rpc).await {
                        Ok(analysis) => {
                            let score = analysis.final_score;
                            info!(
                                mint = %token.mint,
                                score = score,
                                "Security analysis complete"
                            );

                            // Store security data
                            if let Ok(json) = serde_json::to_string(&analysis) {
                                let _ = db::queries::update_token_security(
                                    &analyzer_db,
                                    &token.mint,
                                    score,
                                    &json,
                                );
                            }

                            // Decision: Auto buy, notify, or skip
                            if score >= analyzer_config.min_score_auto_buy {
                                info!(
                                    mint = %token.mint,
                                    score = score,
                                    "Score >= {} \u{2192} AUTO BUY",
                                    analyzer_config.min_score_auto_buy
                                );

                                // Entry confirmation check
                                match confirm_entry(&token, &analyzer_config.jupiter_api_url, &EntryConfirmation::default()).await {
                                    Ok(EntryDecision::Proceed) => {
                                        // Confirmed — continue to buy
                                    }
                                    Ok(EntryDecision::Reject(reason)) => {
                                        warn!(
                                            mint = %token.mint,
                                            reason = %reason,
                                            "Entry confirmation rejected, skipping buy"
                                        );
                                        let _ = tg_bot.send_message(
                                            tg_chat,
                                            format!(
                                                "\u{26a0}\u{fe0f} <b>Entry Rejected</b>\n\n\
                                                 \u{1f4e6} {} (<code>{}</code>)\n\
                                                 \u{1f6e1}\u{fe0f} Score: {}/100\n\
                                                 \u{274c} Reason: {}",
                                                token.symbol, token.mint, score, reason
                                            ),
                                        ).parse_mode(teloxide::types::ParseMode::Html).await;
                                        continue;
                                    }
                                    Err(e) => {
                                        warn!(
                                            mint = %token.mint,
                                            error = %e,
                                            "Entry confirmation check failed, skipping buy"
                                        );
                                        continue;
                                    }
                                }

                                // Get active wallet keypair
                                match analyzer_wallet.get_active_wallet() {
                                    Ok(Some(active)) => {
                                        match analyzer_wallet.get_keypair(&active.pubkey, &analyzer_password) {
                                            Ok(keypair) => {
                                                let amount = analyzer_config.max_buy_sol;
                                                let slippage = analyzer_config.default_slippage_bps;
                                                match analyzer_executor.execute_buy(
                                                    &token, amount, slippage, &keypair,
                                                ).await {
                                                    Ok(sig) => {
                                                        info!(
                                                            mint = %token.mint,
                                                            signature = %sig,
                                                            amount_sol = amount,
                                                            "AUTO BUY executed successfully"
                                                        );

                                                        // Get position details for notification
                                                        let pos = analyzer_positions.get_open_positions()
                                                            .into_iter()
                                                            .find(|p| p.token_mint == token.mint);

                                                        let (entry_price, tokens_received) = match &pos {
                                                            Some(p) => (p.entry_price_sol, p.token_amount),
                                                            None => (0.0, 0.0),
                                                        };

                                                        let _ = tg_bot.send_message(
                                                            tg_chat,
                                                            format!(
                                                                "\u{2705} <b>AUTO BUY Executed!</b>\n\
                                                                 \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
                                                                 \u{1f4e6} <b>Token:</b> {} (<code>{}</code>)\n\
                                                                 \u{1f30d} <b>Source:</b> {}\n\
                                                                 \u{1f6e1}\u{fe0f} <b>Score:</b> {}/100\n\
                                                                 \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                                                                 \u{1f4b0} <b>Spent:</b> {} SOL\n\
                                                                 \u{1f4b2} <b>Entry Price:</b> {:.10} SOL\n\
                                                                 \u{1f4e6} <b>Received:</b> {:.2} tokens\n\
                                                                 \u{1f4dd} <b>Tx:</b> <code>{}</code>\n\n\
                                                                 <i>TP/SL aktif. Cek /positions</i>",
                                                                token.symbol, token.mint,
                                                                token.source,
                                                                score,
                                                                amount,
                                                                entry_price,
                                                                tokens_received,
                                                                sig,
                                                            ),
                                                        ).parse_mode(teloxide::types::ParseMode::Html).await;
                                                    }
                                                    Err(e) => {
                                                        error!(
                                                            mint = %token.mint,
                                                            error = %e,
                                                            "AUTO BUY failed"
                                                        );

                                                        let _ = tg_bot.send_message(
                                                            tg_chat,
                                                            format!(
                                                                "\u{274c} <b>AUTO BUY Failed</b>\n\
                                                                 \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
                                                                 \u{1f4e6} <b>Token:</b> {} (<code>{}</code>)\n\
                                                                 \u{1f6e1}\u{fe0f} <b>Score:</b> {}/100\n\
                                                                 \u{1f4b0} <b>Amount:</b> {} SOL\n\
                                                                 \u{26a0}\u{fe0f} <b>Error:</b> {}",
                                                                token.symbol, token.mint,
                                                                score,
                                                                amount,
                                                                e,
                                                            ),
                                                        ).parse_mode(teloxide::types::ParseMode::Html).await;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                error!(error = %e, "Failed to decrypt wallet keypair");
                                            }
                                        }
                                    }
                                    Ok(None) => {
                                        warn!("No active wallet set \u{2014} cannot auto-buy");
                                    }
                                    Err(e) => {
                                        error!(error = %e, "Failed to get active wallet");
                                    }
                                }
                            } else if score >= analyzer_config.min_score_notify {
                                info!(
                                    mint = %token.mint,
                                    score = score,
                                    "Score {}-{} \u{2192} NOTIFY user",
                                    analyzer_config.min_score_notify,
                                    analyzer_config.min_score_auto_buy
                                );

                                // Send notification with buy buttons
                                let lp_status = format!("{:?}", analysis.lp_status);
                                let honeypot = format!("{:?}", analysis.honeypot_result);
                                let _ = tg_bot.send_message(
                                    tg_chat,
                                    format!(
                                        "\u{1f50d} <b>Token Detected</b>\n\
                                         \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
                                         \u{1f4e6} <b>{}</b> (<code>{}</code>)\n\
                                         \u{1f30d} Source: {}\n\
                                         \u{1f6e1}\u{fe0f} Score: <b>{}/100</b>\n\
                                         \u{1f4b0} Liquidity: {:.2} SOL\n\
                                         \u{1f512} LP: {}\n\
                                         \u{1f41d} Honeypot: {}\n\n\
                                         \u{1f4a1} <i>Score di bawah auto-buy ({}).\nGunakan /buy {} untuk beli manual.</i>",
                                        token.symbol, token.mint,
                                        token.source,
                                        score,
                                        token.initial_liquidity_sol,
                                        lp_status,
                                        honeypot,
                                        analyzer_config.min_score_auto_buy,
                                        token.mint,
                                    ),
                                ).parse_mode(teloxide::types::ParseMode::Html).await;
                            } else {
                                info!(
                                    mint = %token.mint,
                                    score = score,
                                    "Score < {} \u{2192} SKIP",
                                    analyzer_config.min_score_notify
                                );
                            }
                        }
                        Err(e) => {
                            error!(
                                mint = %token.mint,
                                error = %e,
                                "Security analysis failed"
                            );
                        }
                    }
                }
                _ = analyzer_shutdown.recv() => {
                    info!("Analyzer pipeline received shutdown signal");
                    break;
                }
            }
        }
    });

    // ── Spawn Price Feed ───────────────────────────────────────
    let price_feed = PriceFeed::new(&config.jupiter_api_url, &config.jupiter_api_key, 3);
    let price_positions = position_manager.clone();
    let mut price_shutdown = shutdown_tx.subscribe();
    let price_handle = tokio::spawn(async move {
        info!("Price feed starting...");
        tokio::select! {
            result = price_feed.run(price_positions, price_tx) => {
                if let Err(e) = result {
                    error!(error = %e, "Price feed exited with error");
                }
            }
            _ = price_shutdown.recv() => {
                info!("Price feed received shutdown signal");
            }
        }
    });

    // ── Spawn TP/SL Monitor ────────────────────────────────────
    let (tpsl_monitor, tpsl_command_rx) = TpSlMonitor::create(
        Arc::new(config.clone()),
        position_manager.clone(),
        3, // check every 3 seconds
    );
    let mut tpsl_shutdown = shutdown_tx.subscribe();
    let tpsl_handle = tokio::spawn(async move {
        info!("TP/SL monitor starting...");
        tokio::select! {
            result = tpsl_monitor.run(price_rx) => {
                if let Err(e) = result {
                    error!(error = %e, "TP/SL monitor exited with error");
                }
            }
            _ = tpsl_shutdown.recv() => {
                info!("TP/SL monitor received shutdown signal");
            }
        }
    });

    // ── Spawn TP/SL Command Processor ──────────────────────────
    let bot_sell_tx = sell_tx.clone();
    let tpsl_cmd_handle = tokio::spawn(async move {
        info!("TP/SL command processor starting...");
        process_tp_sl_commands(tpsl_command_rx, sell_tx).await;
    });

    // ── Spawn Sell Executor ────────────────────────────────────
    let sell_executor = executor.clone();
    let sell_wallet = wallet_manager.clone();
    let sell_password = wallet_password.clone();
    let sell_handle = tokio::spawn(async move {
        info!("Sell executor starting...");
        while let Some((mint, sell_pct)) = sell_rx.recv().await {
            info!(mint = %mint, sell_pct = sell_pct, "Processing sell command");

            // Get active wallet keypair for selling
            let keypair = match sell_wallet.get_active_wallet() {
                Ok(Some(active)) => match sell_wallet.get_keypair(&active.pubkey, &sell_password) {
                    Ok(kp) => kp,
                    Err(e) => {
                        error!(error = %e, "Failed to decrypt wallet for sell");
                        continue;
                    }
                },
                Ok(None) => {
                    warn!("No active wallet set — cannot execute sell");
                    continue;
                }
                Err(e) => {
                    error!(error = %e, "Failed to get active wallet for sell");
                    continue;
                }
            };

            match sell_executor.execute_sell(&mint, sell_pct, &keypair).await {
                Ok(sig) => {
                    info!(
                        mint = %mint,
                        sell_pct = sell_pct,
                        signature = %sig,
                        "Sell executed successfully"
                    );
                }
                Err(e) => {
                    error!(
                        mint = %mint,
                        sell_pct = sell_pct,
                        error = %e,
                        "Sell execution failed"
                    );
                }
            }
        }
    });

    // ── Start Telegram bot (blocking) ──────────────────────────
    info!("Starting Telegram bot...");
    let bot_config = config.clone();
    let bot_db = db.clone();
    let bot_wallet = wallet_manager.clone();
    let bot_password = wallet_password.clone();
    let bot_executor = executor.clone();
    let bot_handle = tokio::spawn(async move {
        if let Err(e) = TelegramBot::start(bot_config, bot_db, bot_sell_tx, bot_wallet, bot_password, bot_executor).await {
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
        let _ = tokio::join!(
            detector_handle,
            analyzer_handle,
            price_handle,
            tpsl_handle,
            tpsl_cmd_handle,
            sell_handle,
            bot_handle,
        );
    })
    .await;

    info!("RICOZ SNIPER shut down cleanly. Goodbye!");
    Ok(())
}
