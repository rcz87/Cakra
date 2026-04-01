use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::models::Position;

use super::positions::PositionManager;

/// Graduated TP/SL tier for partial exits.
#[derive(Debug, Clone)]
pub struct TpSlTier {
    pub trigger_pct: f64,   // PnL % that triggers this tier
    pub sell_pct: u8,       // % of remaining position to sell
}

/// Default graduated TP tiers.
pub fn default_tp_tiers() -> Vec<TpSlTier> {
    vec![
        TpSlTier { trigger_pct: 50.0, sell_pct: 50 },   // +50% → sell 50%
        TpSlTier { trigger_pct: 100.0, sell_pct: 60 },  // +100% → sell 60% of remaining
        TpSlTier { trigger_pct: 200.0, sell_pct: 100 },  // +200% → sell all remaining
    ]
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
    /// Emergency exit due to liquidity removal.
    EmergencyExit {
        mint: String,
        symbol: String,
        reason: String,
    },
    /// Time stop — position has been open too long without sufficient profit.
    TimeStop {
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
    config: Arc<Config>,
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
        check_interval_secs: u64,
    ) -> Self {
        Self {
            config,
            positions,
            command_tx,
            check_interval: Duration::from_secs(check_interval_secs),
            tp_tiers: default_tp_tiers(),
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
        check_interval_secs: u64,
    ) -> (Self, mpsc::Receiver<TpSlCommand>) {
        let (tx, rx) = mpsc::channel(64);
        let monitor = Self::new(config, positions, tx, check_interval_secs);
        (monitor, rx)
    }

    /// Start the monitoring loop. This should be spawned as a tokio task.
    ///
    /// # Arguments
    /// * `price_rx` - Channel receiver for real-time price updates.
    ///   Each message is (mint, price_sol).
    pub async fn run(self, mut price_rx: mpsc::Receiver<(String, f64)>) -> Result<()> {
        info!(
            interval_secs = self.check_interval.as_secs(),
            "TP/SL monitor started"
        );

        let mut check_timer = tokio::time::interval(self.check_interval);

        loop {
            tokio::select! {
                // Process incoming price updates
                Some((mint, price)) = price_rx.recv() => {
                    self.handle_price_update(&mint, price).await;
                }

                // Periodic full check of all positions
                _ = check_timer.tick() => {
                    self.check_all_positions().await;
                }
            }
        }
    }

    /// Handle a single price update for a specific token.
    async fn handle_price_update(&self, mint: &str, price: f64) {
        // Update the position with the new price
        if let Err(e) = self.positions.update_price(mint, price) {
            warn!(mint = %mint, error = %e, "Failed to update position price");
            return;
        }

        // Check if this position should trigger TP/SL
        let positions = self.positions.get_open_positions();
        if let Some(pos) = positions.iter().find(|p| p.token_mint == mint) {
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
        // Check stop-loss first (highest priority)
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

        // Time stop: if position is open > 600 seconds (10 min) and PnL < 10%, exit
        if pos.age_secs > 600 && pos.pnl_pct < 10.0 {
            warn!(
                mint = %pos.token_mint,
                symbol = %pos.token_symbol,
                age_secs = pos.age_secs,
                pnl_pct = pos.pnl_pct,
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

        // Trailing stop: only activates after PnL > 30%
        if pos.pnl_pct > 30.0 && pos.should_trailing_stop() {
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
                "Trailing stop triggered (after 30% profit gate)"
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
            TpSlCommand::EmergencyExit { mint, symbol, reason } => {
                error!(
                    symbol = %symbol,
                    reason = %reason,
                    "Processing emergency exit: selling 100%"
                );
                if let Err(e) = sell_tx.send((mint, 100)).await {
                    error!("Failed to dispatch emergency exit sell: {e}");
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
        }
    }
}
