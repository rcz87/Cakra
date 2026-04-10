use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

use crate::config::{Config, TradingMode, TradingProfile};
use crate::models::Position;

use super::positions::PositionManager;
use super::price_feed::PriceUpdate;

/// Graduated TP/SL tier for partial exits.
#[derive(Debug, Clone)]
pub struct TpSlTier {
    pub trigger_pct: f64,   // PnL % that triggers this tier
    pub sell_pct: u8,       // % of remaining position to sell
}

/// TP tiers for Snipe mode — instant all-or-nothing exit at +5%.
pub fn snipe_tp_tiers() -> Vec<TpSlTier> {
    vec![
        TpSlTier { trigger_pct: 5.0, sell_pct: 100 },    // +5% → sell ALL immediately
    ]
}

/// TP tiers for Scalp mode — fast, aggressive exits.
pub fn scalp_tp_tiers() -> Vec<TpSlTier> {
    vec![
        TpSlTier { trigger_pct: 10.0, sell_pct: 50 },   // +10% → sell 50%
        TpSlTier { trigger_pct: 20.0, sell_pct: 70 },   // +20% → sell 70% of remaining
        TpSlTier { trigger_pct: 50.0, sell_pct: 100 },   // +50% → sell all remaining
    ]
}

/// TP tiers for Hold mode — patient, wide exits for liquid tokens.
pub fn hold_tp_tiers() -> Vec<TpSlTier> {
    vec![
        TpSlTier { trigger_pct: 50.0, sell_pct: 50 },   // +50% → sell 50%
        TpSlTier { trigger_pct: 100.0, sell_pct: 60 },  // +100% → sell 60% of remaining
        TpSlTier { trigger_pct: 200.0, sell_pct: 100 },  // +200% → sell all remaining
    ]
}

/// Get TP tiers for a given trading mode.
pub fn tp_tiers_for_mode(mode: TradingMode) -> Vec<TpSlTier> {
    match mode {
        TradingMode::Snipe => snipe_tp_tiers(),
        TradingMode::Scalp => scalp_tp_tiers(),
        TradingMode::Hold => hold_tp_tiers(),
    }
}

/// Commands sent from the TP/SL monitor to the executor for auto-selling.
#[derive(Debug, Clone)]
pub enum TpSlCommand {
    /// Trigger a take-profit sell.
    TakeProfit {
        mint: String,
        symbol: String,
        pnl_pct: f64,
    },
    /// Trigger a stop-loss sell.
    StopLoss {
        mint: String,
        symbol: String,
        pnl_pct: f64,
    },
    /// Trigger a trailing stop sell.
    TrailingStop {
        mint: String,
        symbol: String,
        drop_from_high_pct: f64,
    },
    /// Trigger a partial take-profit at a tier.
    PartialTakeProfit {
        mint: String,
        symbol: String,
        pnl_pct: f64,
        sell_pct: u8,
        tier_index: usize,
    },
    /// Time stop — position has been open too long without sufficient profit.
    TimeStop {
        mint: String,
        symbol: String,
        age_secs: u64,
        pnl_pct: f64,
    },
    /// Max age exit — profitable position exceeded max hold time.
    MaxAgeExit {
        mint: String,
        symbol: String,
        age_secs: u64,
        pnl_pct: f64,
    },
}

/// Take-profit / stop-loss monitor that runs as a background tokio task.
///
/// Periodically checks open positions and sends sell commands when
/// TP, SL, or trailing stop conditions are met.
pub struct TpSlMonitor {
    profile: TradingProfile,
    positions: PositionManager,
    command_tx: mpsc::Sender<TpSlCommand>,
    check_interval: Duration,
    tp_tiers: Vec<TpSlTier>,
    triggered_tiers: Arc<Mutex<HashMap<String, Vec<usize>>>>, // mint -> list of triggered tier indices
}

impl TpSlMonitor {
    /// Create a new TP/SL monitor.
    ///
    /// # Arguments
    /// * `config` - Bot configuration
    /// * `positions` - Position manager for querying open positions
    /// * `command_tx` - Channel sender for dispatching sell commands
    /// * `check_interval_secs` - How often to check positions (in seconds)
    pub fn new(
        config: Arc<Config>,
        positions: PositionManager,
        command_tx: mpsc::Sender<TpSlCommand>,
    ) -> Self {
        let profile = config.trading_profile();
        let check_interval = profile.tpsl_check_secs;
        let tp_tiers = tp_tiers_for_mode(profile.mode);
        info!(
            mode = %profile.mode,
            check_secs = check_interval,
            tiers = tp_tiers.len(),
            sl = profile.stop_loss_pct,
            trailing = profile.trailing_stop_pct,
            time_stop_secs = profile.time_stop_secs,
            "TpSlMonitor configured"
        );
        Self {
            profile,
            positions,
            command_tx,
            check_interval: Duration::from_secs(check_interval),
            tp_tiers,
            triggered_tiers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create the TP/SL monitor along with its command channel.
    ///
    /// Returns (monitor, receiver) where the receiver should be consumed
    /// by the executor to process sell commands.
    pub fn create(
        config: Arc<Config>,
        positions: PositionManager,
    ) -> (Self, mpsc::Receiver<TpSlCommand>) {
        let (tx, rx) = mpsc::channel(64);
        let monitor = Self::new(config, positions, tx);
        (monitor, rx)
    }

    /// Start the monitoring loop. This should be spawned as a tokio task.
    ///
    /// # Arguments
    /// * `price_rx` - Channel receiver for real-time price updates.
    ///   Each message is a PriceUpdate with source + stale flag.
    pub async fn run(self, mut price_rx: mpsc::Receiver<PriceUpdate>) -> Result<()> {
        info!(
            interval_secs = self.check_interval.as_secs(),
            "TP/SL monitor started"
        );

        let mut check_timer = tokio::time::interval(self.check_interval);

        loop {
            tokio::select! {
                // Process incoming price updates
                Some(update) = price_rx.recv() => {
                    self.handle_price_update(update).await;
                }

                // Periodic full check of all positions
                _ = check_timer.tick() => {
                    self.check_all_positions().await;
                }
            }
        }
    }

    /// Handle a single price update for a specific token.
    async fn handle_price_update(&self, update: PriceUpdate) {
        // Update the position with the new price + stale flag
        if let Err(e) = self.positions.update_price_with_stale(
            &update.mint, update.price_sol, update.stale,
        ) {
            warn!(mint = %update.mint, error = %e, "Failed to update position price");
            return;
        }

        if update.stale {
            warn!(
                mint = %update.mint,
                source = ?update.source,
                "Price update arrived stale — TP/SL on safety net only"
            );
        }

        // Check if this position should trigger TP/SL
        let positions = self.positions.get_open_positions();
        if let Some(pos) = positions.iter().find(|p| p.token_mint == update.mint) {
            self.evaluate_position(pos).await;
        }
    }

    /// Check all open positions for TP/SL triggers.
    async fn check_all_positions(&self) {
        let positions = self.positions.get_open_positions();

        // Clean up triggered tiers for closed positions
        {
            let open_mints: HashSet<String> =
                positions.iter().map(|p| p.token_mint.clone()).collect();
            let mut triggered = self.triggered_tiers.lock().await;
            triggered.retain(|mint, _| open_mints.contains(mint));
        }

        if positions.is_empty() {
            return;
        }

        for pos in &positions {
            self.evaluate_position(pos).await;
        }
    }

    /// Evaluate a single position for TP/SL conditions.
    async fn evaluate_position(&self, pos: &Position) {
        // ── Stale-price safety net ─────────────────────────────────
        // If price is stale, we don't trust take-profit triggers (data may be ahead
        // of reality). But stop-loss + time-stop must STILL fire — otherwise the bot
        // goes blind during a dump and rides positions to zero.
        //
        // Policy:
        //   stale + elapsed < 60s  → only SL + time-stop allowed (skip TP/trailing)
        //   stale + elapsed >= 60s → SL + time-stop only, log warning loudly
        //   not stale              → all triggers active (normal)
        let is_stale = pos.price_stale;
        let stale_secs = pos.last_price_at
            .map(|t| (chrono::Utc::now() - t).num_seconds().max(0) as u64)
            .unwrap_or(0);

        if is_stale && stale_secs >= 60 {
            warn!(
                mint = %pos.token_mint,
                stale_secs,
                "Price stale > 60s — running SL safety net only"
            );
        }

        // Check stop-loss first (highest priority — runs even when stale)
        if pos.should_stop_loss() {
            warn!(
                mint = %pos.token_mint,
                symbol = %pos.token_symbol,
                pnl_pct = pos.pnl_pct,
                sl_target = pos.stop_loss_pct,
                "Stop-loss triggered"
            );

            let cmd = TpSlCommand::StopLoss {
                mint: pos.token_mint.clone(),
                symbol: pos.token_symbol.clone(),
                pnl_pct: pos.pnl_pct,
            };

            if let Err(e) = self.command_tx.send(cmd).await {
                error!("Failed to send SL command: {e}");
            }
            return;
        }

        // Time stop: exit stale position that isn't profitable enough
        let ts_secs = self.profile.time_stop_secs;
        let ts_min_pnl = self.profile.time_stop_min_pnl;
        if pos.age_secs > ts_secs && pos.pnl_pct < ts_min_pnl {
            warn!(
                mint = %pos.token_mint,
                symbol = %pos.token_symbol,
                age_secs = pos.age_secs,
                pnl_pct = pos.pnl_pct,
                threshold_secs = ts_secs,
                min_pnl = ts_min_pnl,
                mode = %self.profile.mode,
                "Time stop triggered — position stale"
            );

            let cmd = TpSlCommand::TimeStop {
                mint: pos.token_mint.clone(),
                symbol: pos.token_symbol.clone(),
                age_secs: pos.age_secs,
                pnl_pct: pos.pnl_pct,
            };

            if let Err(e) = self.command_tx.send(cmd).await {
                error!("Failed to send time stop command: {e}");
            }
            return;
        }

        // Max age exit: force sell profitable position past max hold time
        let max_hold = self.profile.max_hold_secs;
        let max_age_min = self.profile.max_age_min_pnl;
        if max_hold > 0 && pos.age_secs > max_hold && pos.pnl_pct > max_age_min {
            warn!(
                mint = %pos.token_mint,
                symbol = %pos.token_symbol,
                age_secs = pos.age_secs,
                max_hold_secs = max_hold,
                pnl_pct = pos.pnl_pct,
                "Max age exit triggered — profitable position exceeded max hold time"
            );

            let cmd = TpSlCommand::MaxAgeExit {
                mint: pos.token_mint.clone(),
                symbol: pos.token_symbol.clone(),
                age_secs: pos.age_secs,
                pnl_pct: pos.pnl_pct,
            };

            if let Err(e) = self.command_tx.send(cmd).await {
                error!("Failed to send max age exit command: {e}");
            }
            return;
        }

        // ── TP / trailing only when price is FRESH ────────────
        // Stale prices can't be trusted for profit-taking decisions.
        // SL + time-stop above already ran as safety net.
        if is_stale {
            return;
        }

        // Graduated TP: check tiers in order
        {
            let mut triggered = self.triggered_tiers.lock().await;
            let triggered_for_mint = triggered.entry(pos.token_mint.clone()).or_default();

            for (i, tier) in self.tp_tiers.iter().enumerate() {
                if triggered_for_mint.contains(&i) {
                    continue; // already triggered this tier
                }
                if pos.pnl_pct >= tier.trigger_pct {
                    info!(
                        mint = %pos.token_mint,
                        symbol = %pos.token_symbol,
                        pnl_pct = pos.pnl_pct,
                        tier_index = i,
                        trigger_pct = tier.trigger_pct,
                        sell_pct = tier.sell_pct,
                        "Graduated TP tier triggered"
                    );

                    triggered_for_mint.push(i);

                    let cmd = TpSlCommand::PartialTakeProfit {
                        mint: pos.token_mint.clone(),
                        symbol: pos.token_symbol.clone(),
                        pnl_pct: pos.pnl_pct,
                        sell_pct: tier.sell_pct,
                        tier_index: i,
                    };

                    if let Err(e) = self.command_tx.send(cmd).await {
                        error!("Failed to send partial TP command: {e}");
                    }
                    // Only trigger one tier per evaluation cycle
                    return;
                }
            }
        }

        // Trailing stop: only activates after PnL > trailing_gate_pct
        let gate = self.profile.trailing_gate_pct;
        if pos.pnl_pct > gate && pos.should_trailing_stop() {
            let drop_from_high = if pos.highest_price_sol > 0.0 {
                ((pos.highest_price_sol - pos.current_price_sol) / pos.highest_price_sol) * 100.0
            } else {
                0.0
            };

            warn!(
                mint = %pos.token_mint,
                symbol = %pos.token_symbol,
                drop_pct = drop_from_high,
                highest = pos.highest_price_sol,
                current = pos.current_price_sol,
                gate_pct = gate,
                "Trailing stop triggered"
            );

            let cmd = TpSlCommand::TrailingStop {
                mint: pos.token_mint.clone(),
                symbol: pos.token_symbol.clone(),
                drop_from_high_pct: drop_from_high,
            };

            if let Err(e) = self.command_tx.send(cmd).await {
                error!("Failed to send trailing stop command: {e}");
            }
            return;
        }

        // Check classic take-profit (full exit at configured TP level)
        if pos.should_take_profit() {
            info!(
                mint = %pos.token_mint,
                symbol = %pos.token_symbol,
                pnl_pct = pos.pnl_pct,
                tp_target = pos.take_profit_pct,
                "Take-profit triggered"
            );

            let cmd = TpSlCommand::TakeProfit {
                mint: pos.token_mint.clone(),
                symbol: pos.token_symbol.clone(),
                pnl_pct: pos.pnl_pct,
            };

            if let Err(e) = self.command_tx.send(cmd).await {
                error!("Failed to send TP command: {e}");
            }
        }
    }
}

/// Process TP/SL commands by executing sells.
/// This should run in a separate tokio task.
pub async fn process_tp_sl_commands(
    mut rx: mpsc::Receiver<TpSlCommand>,
    sell_tx: mpsc::Sender<(String, u8)>, // (mint, sell_pct)
) {
    info!("TP/SL command processor started");

    while let Some(cmd) = rx.recv().await {
        match cmd {
            TpSlCommand::TakeProfit { mint, symbol, pnl_pct } => {
                info!(
                    symbol = %symbol,
                    pnl_pct = pnl_pct,
                    "Processing take-profit: selling 100%"
                );
                if let Err(e) = sell_tx.send((mint, 100)).await {
                    error!("Failed to dispatch TP sell: {e}");
                }
            }
            TpSlCommand::StopLoss { mint, symbol, pnl_pct } => {
                warn!(
                    symbol = %symbol,
                    pnl_pct = pnl_pct,
                    "Processing stop-loss: selling 100%"
                );
                if let Err(e) = sell_tx.send((mint, 100)).await {
                    error!("Failed to dispatch SL sell: {e}");
                }
            }
            TpSlCommand::TrailingStop { mint, symbol, drop_from_high_pct } => {
                warn!(
                    symbol = %symbol,
                    drop_pct = drop_from_high_pct,
                    "Processing trailing stop: selling 100%"
                );
                if let Err(e) = sell_tx.send((mint, 100)).await {
                    error!("Failed to dispatch trailing stop sell: {e}");
                }
            }
            TpSlCommand::PartialTakeProfit { mint, symbol, pnl_pct, sell_pct, tier_index } => {
                info!(
                    symbol = %symbol,
                    pnl_pct = pnl_pct,
                    sell_pct = sell_pct,
                    tier = tier_index,
                    "Processing graduated TP: partial sell"
                );
                if let Err(e) = sell_tx.send((mint, sell_pct)).await {
                    error!("Failed to dispatch partial TP sell: {e}");
                }
            }
            TpSlCommand::TimeStop { mint, symbol, age_secs, pnl_pct } => {
                warn!(
                    symbol = %symbol,
                    age_secs = age_secs,
                    pnl_pct = pnl_pct,
                    "Processing time stop: selling 100%"
                );
                if let Err(e) = sell_tx.send((mint, 100)).await {
                    error!("Failed to dispatch time stop sell: {e}");
                }
            }
            TpSlCommand::MaxAgeExit { mint, symbol, age_secs, pnl_pct } => {
                warn!(
                    symbol = %symbol,
                    age_secs = age_secs,
                    pnl_pct = pnl_pct,
                    "Processing max age exit: selling 100%"
                );
                if let Err(e) = sell_tx.send((mint, 100)).await {
                    error!("Failed to dispatch max age exit sell: {e}");
                }
            }
        }
    }
}
