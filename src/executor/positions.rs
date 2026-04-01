use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use chrono::Utc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::db::DbPool;
use crate::models::position::PositionStatus;
use crate::models::Position;

/// Manages open and closed positions with in-memory cache and database persistence.
#[derive(Clone)]
pub struct PositionManager {
    /// In-memory position cache keyed by token mint.
    positions: Arc<RwLock<HashMap<String, Position>>>,
    db: DbPool,
}

impl PositionManager {
    pub fn new(db: DbPool) -> Self {
        Self {
            positions: Arc::new(RwLock::new(HashMap::new())),
            db,
        }
    }

    /// Open a new position and track it.
    ///
    /// # Arguments
    /// * `token_mint` - Token mint address
    /// * `token_symbol` - Token symbol for display
    /// * `wallet_pubkey` - Wallet public key that holds the position
    /// * `entry_price_sol` - Price per token in SOL at entry
    /// * `entry_amount_sol` - Total SOL spent on the buy
    /// * `token_amount` - Number of tokens received
    /// * `slippage_bps` - Slippage used (for TP/SL defaults)
    /// * `buy_tx` - Buy transaction signature
    pub fn open_position(
        &self,
        token_mint: &str,
        token_symbol: &str,
        wallet_pubkey: &str,
        entry_price_sol: f64,
        entry_amount_sol: f64,
        token_amount: f64,
        _slippage_bps: u16,
        buy_tx: &str,
        security_score: u8,
    ) -> Result<Position> {
        let position = Position {
            id: Uuid::new_v4().to_string(),
            token_mint: token_mint.to_string(),
            token_symbol: token_symbol.to_string(),
            wallet_pubkey: wallet_pubkey.to_string(),
            entry_price_sol,
            entry_amount_sol,
            token_amount,
            current_price_sol: entry_price_sol,
            highest_price_sol: entry_price_sol,
            take_profit_pct: 100.0, // default 100% TP
            stop_loss_pct: 50.0,    // default 50% SL
            trailing_stop_pct: Some(30.0), // default 30% trailing
            pnl_sol: 0.0,
            pnl_pct: 0.0,
            status: PositionStatus::Open,
            buy_tx: buy_tx.to_string(),
            sell_tx: None,
            opened_at: Utc::now(),
            closed_at: None,
            security_score,
            age_secs: 0,
        };

        // Insert into in-memory cache
        {
            let mut positions = self
                .positions
                .write()
                .map_err(|e| anyhow::anyhow!("Position lock poisoned: {e}"))?;
            positions.insert(token_mint.to_string(), position.clone());
        }

        // Persist to database
        self.persist_position(&position)?;

        info!(
            mint = %token_mint,
            symbol = %token_symbol,
            entry_price = entry_price_sol,
            amount_sol = entry_amount_sol,
            tokens = token_amount,
            "Position opened"
        );

        Ok(position)
    }

    /// Update the current price for a position.
    pub fn update_price(&self, mint: &str, price: f64) -> Result<()> {
        let mut positions = self
            .positions
            .write()
            .map_err(|e| anyhow::anyhow!("Position lock poisoned: {e}"))?;

        if let Some(pos) = positions.get_mut(mint) {
            pos.update_pnl(price);
        } else {
            warn!(mint = %mint, "No open position found to update price");
        }

        Ok(())
    }

    /// Close a position (mark as manually closed).
    pub fn close_position(&self, mint: &str, sell_tx: &str) -> Result<()> {
        let mut positions = self
            .positions
            .write()
            .map_err(|e| anyhow::anyhow!("Position lock poisoned: {e}"))?;

        if let Some(pos) = positions.get_mut(mint) {
            pos.status = PositionStatus::ClosedManual;
            pos.sell_tx = Some(sell_tx.to_string());
            pos.closed_at = Some(Utc::now());

            info!(
                mint = %mint,
                symbol = %pos.token_symbol,
                pnl_sol = pos.pnl_sol,
                pnl_pct = pos.pnl_pct,
                "Position closed"
            );

            // Persist the updated position
            let pos_clone = pos.clone();
            drop(positions);
            self.persist_position(&pos_clone)?;
        } else {
            warn!(mint = %mint, "No open position found to close");
        }

        Ok(())
    }

    /// Close a position due to take-profit.
    pub fn close_position_tp(&self, mint: &str, sell_tx: &str) -> Result<()> {
        self.close_position_with_status(mint, sell_tx, PositionStatus::ClosedTp)
    }

    /// Close a position due to stop-loss.
    pub fn close_position_sl(&self, mint: &str, sell_tx: &str) -> Result<()> {
        self.close_position_with_status(mint, sell_tx, PositionStatus::ClosedSl)
    }

    fn close_position_with_status(
        &self,
        mint: &str,
        sell_tx: &str,
        status: PositionStatus,
    ) -> Result<()> {
        let mut positions = self
            .positions
            .write()
            .map_err(|e| anyhow::anyhow!("Position lock poisoned: {e}"))?;

        if let Some(pos) = positions.get_mut(mint) {
            pos.status = status;
            pos.sell_tx = Some(sell_tx.to_string());
            pos.closed_at = Some(Utc::now());

            let pos_clone = pos.clone();
            drop(positions);
            self.persist_position(&pos_clone)?;
        }

        Ok(())
    }

    /// Reduce token amount after a partial sell. Does NOT close the position.
    pub fn reduce_position(&self, mint: &str, sell_pct: u8, sell_tx: &str) -> Result<()> {
        let mut positions = self
            .positions
            .write()
            .map_err(|e| anyhow::anyhow!("Position lock poisoned: {e}"))?;

        if let Some(pos) = positions.get_mut(mint) {
            let reduction = pos.token_amount * (sell_pct as f64 / 100.0);
            pos.token_amount -= reduction;
            if pos.token_amount <= 0.0 {
                // Fully sold
                pos.token_amount = 0.0;
                pos.status = PositionStatus::ClosedTp;
                pos.sell_tx = Some(sell_tx.to_string());
                pos.closed_at = Some(Utc::now());
            }
            let pos_clone = pos.clone();
            drop(positions);
            self.persist_position(&pos_clone)?;
        }

        Ok(())
    }

    /// Get all currently open positions.
    pub fn get_open_positions(&self) -> Vec<Position> {
        let positions = self.positions.read().unwrap_or_else(|e| e.into_inner());

        positions
            .values()
            .filter(|p| matches!(p.status, PositionStatus::Open))
            .cloned()
            .collect()
    }

    /// Get all positions (open and closed).
    pub fn get_all_positions(&self) -> Vec<Position> {
        let positions = self.positions.read().unwrap_or_else(|e| e.into_inner());
        positions.values().cloned().collect()
    }

    /// Get a specific position by mint.
    pub fn get_position(&self, mint: &str) -> Option<Position> {
        let positions = self.positions.read().unwrap_or_else(|e| e.into_inner());
        positions.get(mint).cloned()
    }

    /// Calculate total PnL across all open positions.
    pub fn get_total_pnl(&self) -> (f64, f64) {
        let positions = self.positions.read().unwrap_or_else(|e| e.into_inner());

        let mut total_pnl_sol = 0.0;
        let mut total_invested = 0.0;

        for pos in positions.values() {
            if matches!(pos.status, PositionStatus::Open) {
                total_pnl_sol += pos.pnl_sol;
                total_invested += pos.entry_amount_sol;
            }
        }

        let total_pnl_pct = if total_invested > 0.0 {
            (total_pnl_sol / total_invested) * 100.0
        } else {
            0.0
        };

        (total_pnl_sol, total_pnl_pct)
    }

    /// Set take-profit and stop-loss percentages for a position.
    pub fn set_tp_sl(
        &self,
        mint: &str,
        take_profit_pct: f64,
        stop_loss_pct: f64,
        trailing_stop_pct: Option<f64>,
    ) -> Result<()> {
        let mut positions = self
            .positions
            .write()
            .map_err(|e| anyhow::anyhow!("Position lock poisoned: {e}"))?;

        if let Some(pos) = positions.get_mut(mint) {
            pos.take_profit_pct = take_profit_pct;
            pos.stop_loss_pct = stop_loss_pct;
            pos.trailing_stop_pct = trailing_stop_pct;

            info!(
                mint = %mint,
                tp = take_profit_pct,
                sl = stop_loss_pct,
                trailing = ?trailing_stop_pct,
                "TP/SL updated"
            );

            let pos_clone = pos.clone();
            drop(positions);
            self.persist_position(&pos_clone)?;
        } else {
            anyhow::bail!("No position found for mint {mint}");
        }

        Ok(())
    }

    /// Load positions from the database into memory.
    pub fn load_from_db(&self) -> Result<()> {
        let conn = self
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock poisoned: {e}"))?;

        let mut stmt = conn
            .prepare(
                "SELECT id, token_mint, token_symbol, wallet_pubkey, entry_price_sol, \
                 entry_amount_sol, token_amount, current_price_sol, highest_price_sol, \
                 take_profit_pct, stop_loss_pct, trailing_stop_pct, pnl_sol, pnl_pct, \
                 status, buy_tx, sell_tx, opened_at, closed_at, security_score \
                 FROM positions WHERE status = 'Open'",
            )
            .context("Failed to prepare positions query")?;

        let position_iter = stmt
            .query_map([], |row| {
                Ok(Position {
                    id: row.get(0)?,
                    token_mint: row.get(1)?,
                    token_symbol: row.get(2)?,
                    wallet_pubkey: row.get(3)?,
                    entry_price_sol: row.get(4)?,
                    entry_amount_sol: row.get(5)?,
                    token_amount: row.get(6)?,
                    current_price_sol: row.get(7)?,
                    highest_price_sol: row.get(8)?,
                    take_profit_pct: row.get(9)?,
                    stop_loss_pct: row.get(10)?,
                    trailing_stop_pct: row.get(11)?,
                    pnl_sol: row.get(12)?,
                    pnl_pct: row.get(13)?,
                    status: PositionStatus::Open,
                    buy_tx: row.get(15)?,
                    sell_tx: row.get(16)?,
                    opened_at: row.get::<_, String>(17)
                        .ok()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(Utc::now),
                    closed_at: None,
                    security_score: row.get::<_, u32>(19).unwrap_or(0) as u8,
                    age_secs: 0,
                })
            })
            .context("Failed to query positions")?;

        let mut positions = self
            .positions
            .write()
            .map_err(|e| anyhow::anyhow!("Position lock poisoned: {e}"))?;

        let mut count = 0;
        for pos_result in position_iter {
            if let Ok(pos) = pos_result {
                positions.insert(pos.token_mint.clone(), pos);
                count += 1;
            }
        }

        info!(count = count, "Loaded positions from database");
        Ok(())
    }

    /// Persist a position to the database.
    fn persist_position(&self, position: &Position) -> Result<()> {
        let conn = self
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock poisoned: {e}"))?;

        let status_str = match position.status {
            PositionStatus::Open => "Open",
            PositionStatus::ClosedTp => "ClosedTp",
            PositionStatus::ClosedSl => "ClosedSl",
            PositionStatus::ClosedManual => "ClosedManual",
            PositionStatus::ClosedError => "ClosedError",
        };

        conn.execute(
            "INSERT OR REPLACE INTO positions \
             (id, token_mint, token_symbol, wallet_pubkey, entry_price_sol, \
              entry_amount_sol, token_amount, current_price_sol, highest_price_sol, \
              take_profit_pct, stop_loss_pct, trailing_stop_pct, pnl_sol, pnl_pct, \
              status, buy_tx, sell_tx, opened_at, closed_at, security_score) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            rusqlite::params![
                position.id,
                position.token_mint,
                position.token_symbol,
                position.wallet_pubkey,
                position.entry_price_sol,
                position.entry_amount_sol,
                position.token_amount,
                position.current_price_sol,
                position.highest_price_sol,
                position.take_profit_pct,
                position.stop_loss_pct,
                position.trailing_stop_pct,
                position.pnl_sol,
                position.pnl_pct,
                status_str,
                position.buy_tx,
                position.sell_tx,
                position.opened_at.to_rfc3339(),
                position.closed_at.map(|t| t.to_rfc3339()),
                position.security_score as u32,
            ],
        )
        .context("Failed to persist position")?;

        Ok(())
    }
}
