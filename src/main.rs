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
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
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

use crate::analyzer::entry_confirmation::{confirm_entry, confirm_entry_fast, EntryConfirmation, EntryDecision};
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

    // Kill switch: shared flag to pause/resume auto-buy via Telegram /stop and /go
    let trading_active = Arc::new(AtomicBool::new(true));

    // ── Trading profile from mode ────────────────────────────
    let trading_profile = config.trading_profile();
    info!(
        mode = %trading_profile.mode,
        tp = trading_profile.take_profit_pct,
        sl = trading_profile.stop_loss_pct,
        trailing = trading_profile.trailing_stop_pct,
        time_stop = trading_profile.time_stop_secs,
        max_hold = trading_profile.max_hold_secs,
        price_poll = trading_profile.price_poll_secs,
        "Trading profile loaded"
    );

    // ── Create shared components ───────────────────────────────
    let position_manager = PositionManager::new(db.clone(), trading_profile.clone());

    // Load any persisted open positions from previous session
    if let Err(e) = position_manager.load_from_db() {
        warn!(error = %e, "Failed to load positions from database");
    }

    // Backfill token_decimals for legacy positions opened before metadata was tracked.
    // Uses a temporary RPC client; failures fall back to default 6 decimals.
    {
        let backfill_rpc = solana_client::rpc_client::RpcClient::new(config.solana_rpc_url.clone());
        if let Err(e) = position_manager.backfill_decimals(&backfill_rpc) {
            warn!(error = %e, "Decimals backfill failed; positions may have wrong decimals");
        }
    }

    // ── Wallet Manager (shared for buy/sell) ─────────────────────
    // NOTE: WALLET_PASSWORD presence + minimum length already validated in
    // Config::from_env(); this unwrap is safe because the process would have
    // already exited on missing/weak password.
    let wallet_password = std::env::var("WALLET_PASSWORD")
        .expect("WALLET_PASSWORD validated by Config::from_env");
    let wallet_manager = Arc::new(
        WalletManager::new(&config, db.clone()).expect("Failed to create wallet manager"),
    );

    // ── Verify wallet decrypt works BEFORE starting trading ───────
    match wallet_manager.get_active_wallet() {
        Ok(Some(active)) => {
            match wallet_manager.get_keypair(&active.pubkey, &wallet_password) {
                Ok(_kp) => {
                    info!(
                        pubkey = %active.pubkey,
                        "Wallet decrypt verified — sell will work"
                    );
                }
                Err(e) => {
                    panic!(
                        "FATAL: Active wallet '{}' cannot be decrypted: {}. \
                         Bot CANNOT sell positions. Fix WALLET_PASSWORD before starting.",
                        active.pubkey, e
                    );
                }
            }
        }
        Ok(None) => {
            warn!(
                "No active wallet set. Bot will detect and analyze tokens, \
                 but CANNOT buy or sell until a wallet is activated via /wallet."
            );
        }
        Err(e) => {
            panic!(
                "FATAL: Cannot query wallets: {}. Fix database before starting.", e
            );
        }
    }

    let executor = Arc::new(
        ExecutorService::new(
            Arc::new(config.clone()),
            db.clone(),
            RiskManager::new(config.clone(), db.clone()),
            CooldownManager::new(config.trade_cooldown_secs),
            ListManager::new(db.clone()),
            position_manager.clone(),
        )
        .context("Failed to initialize ExecutorService")?,
    );

    // ── Channel: Detector → Analyzer ───────────────────────────
    // DetectorService creates its own internal channel and returns the receiver
    let (detector_service, mut token_rx) = DetectorService::new(config.clone());

    // ── Channel: TP/SL → Executor (sell commands) ──────────────
    let (sell_tx, mut sell_rx) = mpsc::channel::<(String, u8)>(64);

    // ── Channel: PriceFeed → TpSlMonitor ───────────────────────
    let (price_tx, price_rx) =
        mpsc::channel::<crate::executor::price_feed::PriceUpdate>(256);

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
    let analyzer_trading_active = trading_active.clone();
    let mut analyzer_shutdown = shutdown_tx.subscribe();
    let analyzer_handle = tokio::spawn(async move {
        info!("Analyzer pipeline starting...");

        let rpc = solana_client::rpc_client::RpcClient::new(
            analyzer_config.effective_rpc_url().to_string(),
        );

        // SOL price tracker for accurate liquidity USD conversion + opportunity scoring
        let sol_tracker = crate::analyzer::opportunity::SolTrendTracker::new(
            &analyzer_config.jupiter_api_key,
        );
        // Fetch initial SOL price
        let mut sol_usd_price: f64 = match sol_tracker.fetch_and_record().await {
            Ok(p) => { info!(sol_price = p, "Initial SOL price fetched"); p }
            Err(e) => { warn!(error = %e, "Failed to fetch SOL price, using $150 fallback"); 150.0 }
        };

        // Telegram bot instance for sending notifications
        let tg_bot = teloxide::Bot::new(&analyzer_config.telegram_bot_token);
        let tg_chat = teloxide::types::ChatId(analyzer_config.telegram_admin_chat_id);

        // Circuit breaker: track analyzer errors within a rolling window.
        // If errors exceed the threshold, pause processing and alert.
        const CB_ERROR_THRESHOLD: u32 = 5;
        const CB_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
        const CB_PAUSE: std::time::Duration = std::time::Duration::from_secs(30);

        let mut error_timestamps: Vec<tokio::time::Instant> = Vec::new();

        loop {
            tokio::select! {
                Some(token) = token_rx.recv() => {
                    // ── Circuit breaker check ──────────────────────────────
                    let now = tokio::time::Instant::now();
                    error_timestamps.retain(|t| now.duration_since(*t) < CB_WINDOW);

                    if error_timestamps.len() >= CB_ERROR_THRESHOLD as usize {
                        warn!(
                            errors_in_window = error_timestamps.len(),
                            pause_secs = CB_PAUSE.as_secs(),
                            "Circuit breaker OPEN — too many analyzer errors, pausing processing"
                        );
                        let _ = tg_bot.send_message(
                            tg_chat,
                            format!(
                                "\u{1f6a8} <b>Circuit Breaker Triggered</b>\n\n\
                                 \u{26a0}\u{fe0f} {} analyzer errors in {}s window.\n\
                                 \u{23f8}\u{fe0f} Pausing token processing for {}s.\n\n\
                                 <i>Bot will auto-resume. Check RPC/network health.</i>",
                                error_timestamps.len(),
                                CB_WINDOW.as_secs(),
                                CB_PAUSE.as_secs(),
                            ),
                        ).parse_mode(teloxide::types::ParseMode::Html).await;

                        tokio::time::sleep(CB_PAUSE).await;
                        error_timestamps.clear();
                        info!("Circuit breaker CLOSED — resuming token processing");
                        continue;
                    }

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

                    // ── MIGRATION SNIPING OBSERVATION PIPELINE ───────────
                    // Detect migration events using sentinel marker set by
                    // pumpportal::parse_migration_event. Reliable than checking
                    // creator.is_empty() because parse failures could also produce
                    // empty creator and become false positives.
                    //
                    // Apply migration-specific filters and record observations.
                    // Skip the normal scoring pipeline for these — they need
                    // different validation.
                    let is_migration_event = token.creator
                        == crate::detector::pumpportal::MIGRATION_EVENT_MARKER;

                    if is_migration_event {
                        let pool_str = match token.source {
                            crate::models::token::TokenSource::PumpSwap => "pump-amm",
                            crate::models::token::TokenSource::Raydium => "raydium",
                            _ => "unknown",
                        };

                        // ── Enrichment via DexScreener ────────────────────
                        // PumpPortal `migrate` events carry only {mint, pool,
                        // signature, txType}. We need real liquidity / mcap
                        // numbers to apply the migration filter. DexScreener
                        // public API (3s timeout, ~300 req/min limit) is
                        // cheaper than on-chain PDA derivation + decode and
                        // our expected rate (~13/hour) is >1000x under the
                        // rate limit.
                        //
                        // market_cap_sol is derived inside the DexScreener
                        // client from the pair's OWN implied SOL/USD rate
                        // (priceNative / priceUsd), so we don't depend on
                        // any external SOL price oracle that could be stale.
                        //
                        // On failure: filter_passed = false with an explicit
                        // "dexscreener: ..." reason — fail closed, never
                        // trade on unenriched data.
                        let ds_dex = match pool_str {
                            "pump-amm" => "pumpswap",
                            "raydium" => "raydium",
                            _ => "",
                        };
                        let enrichment =
                            crate::analyzer::dexscreener::fetch_enrichment(
                                &token.mint,
                                ds_dex,
                            )
                            .await;

                        let (liquidity_sol, market_cap_sol, spot_price_sol, enrichment_ok, enrichment_err) =
                            match enrichment {
                                Ok(Some(pair)) => (
                                    pair.liquidity_quote, // SOL side of the pool
                                    pair.market_cap_sol,
                                    pair.price_native,
                                    true,
                                    None,
                                ),
                                Ok(None) => (
                                    0.0,
                                    0.0,
                                    0.0,
                                    false,
                                    Some("dexscreener: no pair".to_string()),
                                ),
                                Err(e) => {
                                    warn!(mint = %token.mint, error = %e, "DexScreener enrichment failed");
                                    (
                                        0.0,
                                        0.0,
                                        0.0,
                                        false,
                                        Some(format!("dexscreener: {}", e)),
                                    )
                                }
                            };

                        // ── Migration filters ────────────────────────────
                        // Filter 0: enrichment must succeed (fail closed)
                        // Filter 1: minimum migration liquidity > 50 SOL
                        // Filter 2: not the "creator buy exactly 3 SOL" launcher pattern
                        //           (heuristic: solAmount close to 3.0 = mass launcher default)
                        // Filter 3: market_cap_sol indicates real graduation (>= 350 SOL)
                        let mut filter_passed = true;
                        let mut filter_reason: Option<String> = None;

                        if !enrichment_ok {
                            filter_passed = false;
                            filter_reason = enrichment_err.clone();
                        } else if liquidity_sol < 50.0 {
                            filter_passed = false;
                            filter_reason = Some(format!(
                                "liquidity {:.1} SOL < 50.0 min",
                                liquidity_sol
                            ));
                        } else if (liquidity_sol - 3.0).abs() < 0.1 {
                            filter_passed = false;
                            filter_reason = Some("default launcher pattern (creator ~3 SOL)".to_string());
                        } else if market_cap_sol > 0.0 && market_cap_sol < 350.0 {
                            filter_passed = false;
                            filter_reason = Some(format!(
                                "mcap {:.1} SOL < 350 (incomplete migration)",
                                market_cap_sol
                            ));
                        }

                        info!(
                            mint = %token.mint,
                            symbol = %token.symbol,
                            pool = %pool_str,
                            liquidity_sol,
                            mcap_sol = market_cap_sol,
                            enrichment_ok,
                            filter_passed,
                            filter_reason = ?filter_reason,
                            "MIGRATION EVENT observed"
                        );

                        // Record observation regardless of filter outcome
                        // (so we can analyze rejection patterns later)
                        let observation = db::queries::Observation {
                            id: uuid::Uuid::new_v4().to_string(),
                            mint: token.mint.clone(),
                            symbol: token.symbol.clone(),
                            source: format!("{:?}", token.source),
                            security_score: 0,
                            opportunity_score: 0,
                            combined_score: 0,
                            route_type: "migration".to_string(),
                            expected_output: 0,
                            market_cap_sol,
                            liquidity_sol,
                            spot_price_sol,
                            wallet_sol_at_observation: 0.0,
                            is_migration: true,
                            migration_pool: Some(pool_str.to_string()),
                            pre_migration_v_sol: None,  // not available from migration event
                            filter_passed,
                            filter_reason,
                        };

                        if let Err(e) = db::queries::insert_observation(&analyzer_db, &observation) {
                            warn!(error = %e, "Failed to record migration observation");
                        }

                        // Migration events don't go through normal scoring pipeline
                        continue;
                    }

                    // Store token in database (non-migration tokens only)
                    if let Err(e) = db::queries::insert_token(&analyzer_db, &token) {
                        warn!(error = %e, "Failed to store token in database");
                    }

                    // Run security analysis (fast filter for snipe mode, full for others)
                    let analysis_result = if analyzer_config.trading_mode == crate::config::TradingMode::Snipe {
                        analyzer.analyze_token_fast(&token, &rpc).await
                    } else {
                        analyzer.analyze_token(&token, &rpc).await
                    };
                    match analysis_result {
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

                            // Refresh SOL price periodically (every ~30 tokens)
                            if let Ok(p) = sol_tracker.fetch_and_record().await {
                                sol_usd_price = p;
                            }
                            let sol_trend = sol_tracker.get_1h_change_pct();

                            // Compute opportunity score with live SOL data
                            let effective_liq_usd = if token.initial_liquidity_usd > 0.0 {
                                token.initial_liquidity_usd
                            } else {
                                token.initial_liquidity_sol * sol_usd_price
                            };

                            // Derive opportunity data from what PumpPortal gives us:
                            // - initial_buy_sol > 0 means creator bought → at least 1 buyer
                            // - if initial_buy_sol is a large % of liquidity → whale-dominated
                            let has_creator_buy = token.initial_buy_sol > 0.0;
                            let creator_buy_pct = if token.v_sol_in_bonding_curve > 0.0 {
                                (token.initial_buy_sol / token.v_sol_in_bonding_curve * 100.0).min(100.0)
                            } else if token.initial_liquidity_sol > 0.0 {
                                (token.initial_buy_sol / token.initial_liquidity_sol * 100.0).min(100.0)
                            } else {
                                0.0
                            };

                            let opp_analysis = crate::analyzer::opportunity::OpportunityAnalysis {
                                buy_count: if has_creator_buy { 1 } else { 0 },
                                unique_buyers: if has_creator_buy { 1 } else { 0 },
                                seconds_since_creation: chrono::Utc::now()
                                    .signed_duration_since(token.detected_at)
                                    .num_seconds()
                                    .unsigned_abs(),
                                liquidity_usd: effective_liq_usd,
                                price_change_pct: 0.0,  // not available from create event
                                sol_trend_1h_pct: sol_trend,
                                largest_buyer_pct: creator_buy_pct,
                                opportunity_score: 0,
                            };
                            let opp_score = crate::analyzer::opportunity::calculate_opportunity_score(&opp_analysis);

                            // Combined score: 80% security + 20% opportunity.
                            // Real data: liquidity_usd, sol_trend, largest_buyer_pct, market_cap.
                            // Still placeholder: buy_count (1 from creator), unique_buyers (1).
                            // Bump to 60/40 when trade stream provides real buy_count/unique_buyers.
                            let combined_score = ((score as f64 * 0.8) + (opp_score as f64 * 0.2)).round() as u8;

                            info!(
                                mint = %token.mint,
                                security_score = score,
                                opportunity_score = opp_score,
                                combined_score = combined_score,
                                "Combined scoring complete"
                            );

                            // Decision: Auto buy, notify, or skip
                            // Kill switch check — /stop pauses auto-buy
                            if !analyzer_trading_active.load(Ordering::Relaxed) {
                                info!(
                                    mint = %token.mint,
                                    combined = combined_score,
                                    "Auto-buy PAUSED (kill switch active). Use /go to resume."
                                );
                                continue;
                            }

                            if combined_score >= analyzer_config.min_score_auto_buy {
                                // Sanity check: reject tokens with no liquidity data at all
                                if token.initial_liquidity_sol <= 0.0 && token.market_cap_sol <= 0.0 {
                                    warn!(
                                        mint = %token.mint,
                                        combined = combined_score,
                                        "Rejected: zero liquidity AND zero market cap — no data to trade on"
                                    );
                                    continue;
                                }

                                info!(
                                    mint = %token.mint,
                                    combined = combined_score,
                                    security = score,
                                    opportunity = opp_score,
                                    liq_sol = token.initial_liquidity_sol,
                                    mcap_sol = token.market_cap_sol,
                                    "Score >= {} \u{2192} AUTO BUY",
                                    analyzer_config.min_score_auto_buy
                                );

                                // Entry confirmation: fast (no Jupiter) for snipe, full for others
                                let entry_decision = if analyzer_config.trading_mode == crate::config::TradingMode::Snipe {
                                    Ok(confirm_entry_fast(&token))
                                } else {
                                    confirm_entry(&token, &analyzer_config.jupiter_api_url, &EntryConfirmation::default()).await
                                };
                                match entry_decision {
                                    Ok(EntryDecision::Proceed) => {
                                        // Confirmed — continue to buy
                                    }
                                    Ok(EntryDecision::Reject(reason)) => {
                                        warn!(
                                            mint = %token.mint,
                                            symbol = %token.symbol,
                                            score = combined_score,
                                            reason = %reason,
                                            "Entry confirmation rejected, skipping buy"
                                        );
                                        // No Telegram notification — log only to avoid spam
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
                                                        error_timestamps.push(tokio::time::Instant::now());

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
                            } else if combined_score >= analyzer_config.min_score_notify {
                                // Log only — no Telegram spam for NOTIFY tier
                                info!(
                                    mint = %token.mint,
                                    combined = combined_score,
                                    security = score,
                                    liq_sol = token.initial_liquidity_sol,
                                    "Score {}-{} \u{2192} NOTIFY (log only)",
                                    analyzer_config.min_score_notify,
                                    analyzer_config.min_score_auto_buy
                                );
                            } else {
                                info!(
                                    mint = %token.mint,
                                    combined = combined_score,
                                    security = score,
                                    opportunity = opp_score,
                                    "Combined score < {} \u{2192} SKIP",
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
                            error_timestamps.push(tokio::time::Instant::now());
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
    let price_rpc = Arc::new(solana_client::rpc_client::RpcClient::new(
        config.effective_rpc_url().to_string(),
    ));
    let price_feed = PriceFeed::new(
        &config.jupiter_api_url,
        &config.jupiter_api_key,
        trading_profile.price_poll_secs,
        price_rpc,
    );
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
    let sell_positions = position_manager.clone();
    let sell_tg_bot = teloxide::Bot::new(&config.telegram_bot_token);
    let sell_tg_chat = teloxide::types::ChatId(config.telegram_admin_chat_id);
    let sell_handle = tokio::spawn(async move {
        info!("Sell executor starting...");
        while let Some((mint, sell_pct)) = sell_rx.recv().await {
            info!(mint = %mint, sell_pct = sell_pct, "Processing sell command");

            // Get active wallet keypair for selling
            let keypair = match sell_wallet.get_active_wallet() {
                Ok(Some(active)) => match sell_wallet.get_keypair(&active.pubkey, &sell_password) {
                    Ok(kp) => kp,
                    Err(e) => {
                        error!(error = %e, mint = %mint, "CRITICAL: Failed to decrypt wallet for sell");
                        let _ = sell_tg_bot.send_message(
                            sell_tg_chat,
                            format!(
                                "\u{1f6a8} <b>CRITICAL: Sell GAGAL — wallet decrypt error</b>\n\n\
                                 \u{274c} Mint: <code>{}</code>\n\
                                 \u{26a0}\u{fe0f} Error: {}\n\n\
                                 <b>Posisi TIDAK bisa dijual otomatis!</b>\n\
                                 Segera jual manual atau fix WALLET_PASSWORD.",
                                mint, e
                            ),
                        ).parse_mode(teloxide::types::ParseMode::Html).await;
                        continue;
                    }
                },
                Ok(None) => {
                    error!(mint = %mint, "CRITICAL: No active wallet — cannot sell");
                    let _ = sell_tg_bot.send_message(
                        sell_tg_chat,
                        format!(
                            "\u{1f6a8} <b>CRITICAL: Sell GAGAL — no active wallet</b>\n\n\
                             \u{274c} Mint: <code>{}</code>\n\n\
                             <b>Set wallet aktif via /wallet segera!</b>",
                            mint
                        ),
                    ).parse_mode(teloxide::types::ParseMode::Html).await;
                    continue;
                }
                Err(e) => {
                    error!(error = %e, mint = %mint, "CRITICAL: Failed to get wallet for sell");
                    let _ = sell_tg_bot.send_message(
                        sell_tg_chat,
                        format!(
                            "\u{1f6a8} <b>CRITICAL: Sell GAGAL — wallet error</b>\n\n\
                             \u{274c} Mint: <code>{}</code>\n\
                             \u{26a0}\u{fe0f} Error: {}",
                            mint, e
                        ),
                    ).parse_mode(teloxide::types::ParseMode::Html).await;
                    continue;
                }
            };

            // Retry sell up to 3 times with backoff on failure
            let mut sell_ok = false;
            for attempt in 0..3u32 {
                if attempt > 0 {
                    let delay = std::time::Duration::from_secs(1 << attempt);
                    warn!(
                        mint = %mint,
                        attempt = attempt + 1,
                        delay_secs = delay.as_secs(),
                        "Retrying sell..."
                    );
                    tokio::time::sleep(delay).await;
                }

                match sell_executor.execute_sell(&mint, sell_pct, &keypair).await {
                    Ok(sig) => {
                        info!(
                            mint = %mint,
                            sell_pct = sell_pct,
                            signature = %sig,
                            "Sell executed successfully"
                        );

                        let _ = sell_tg_bot.send_message(
                            sell_tg_chat,
                            format!(
                                "\u{1f4b0} <b>AUTO SELL Executed</b>\n\
                                 \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
                                 \u{1f4e6} <b>Mint:</b> <code>{}</code>\n\
                                 \u{1f4ca} <b>Sell:</b> {}%\n\
                                 \u{1f4dd} <b>Tx:</b> <code>{}</code>",
                                mint, sell_pct, sig
                            ),
                        ).parse_mode(teloxide::types::ParseMode::Html).await;

                        sell_ok = true;
                        break;
                    }
                    Err(e) => {
                        error!(
                            mint = %mint,
                            sell_pct = sell_pct,
                            attempt = attempt + 1,
                            error = %e,
                            "Sell execution failed"
                        );
                    }
                }
            }

            if !sell_ok {
                // All 3 retries failed — mark position as ClosedError
                // so TP/SL monitor stops re-triggering sell attempts.
                error!(
                    mint = %mint,
                    sell_pct = sell_pct,
                    "CRITICAL: Sell failed after 3 retries — marking ClosedError"
                );

                if let Err(e) = sell_positions.close_position_error(&mint) {
                    error!(mint = %mint, error = %e, "Failed to mark position as ClosedError");
                }

                let _ = sell_tg_bot.send_message(
                    sell_tg_chat,
                    format!(
                        "\u{1f6a8} <b>CRITICAL: Sell GAGAL 3x</b>\n\n\
                         \u{274c} Mint: <code>{}</code>\n\
                         \u{1f4ca} Sell %: {}%\n\n\
                         <b>Posisi di-mark ClosedError — tidak akan retry otomatis.</b>\n\
                         Jual manual via /sell {}",
                        mint, sell_pct, mint
                    ),
                ).parse_mode(teloxide::types::ParseMode::Html).await;
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
        if let Err(e) = TelegramBot::start(bot_config, bot_db, bot_sell_tx, bot_wallet, bot_password, bot_executor, trading_active.clone()).await {
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
