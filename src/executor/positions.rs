use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use chrono::Utc;
use solana_client::rpc_client::RpcClient;
use solana_sdk::program_pack::Pack;
use solana_sdk::pubkey::Pubkey;
use spl_token::state::Mint;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::TradingProfile;
use crate::db::DbPool;
use crate::models::position::PositionStatus;
use crate::models::Position;

/// Manages open and closed positions with in-memory cache and database persistence.
#[derive(Clone)]
pub struct PositionManager {
    /// In-memory position cache keyed by token mint.
    positions: Arc<RwLock<HashMap<String, Position>>>,
    db: DbPool,
    profile: TradingProfile,
}

#[allow(dead_code)]
impl PositionManager {
    pub fn new(db: DbPool, profile: TradingProfile) -> Self {
        Self {
            positions: Arc::new(RwLock::new(HashMap::new())),
            db,
            profile,
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
        token_source: &str,
        pool_address: Option<String>,
        token_decimals: u8,
        price_source: Option<String>,
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
            take_profit_pct: self.profile.take_profit_pct,
            stop_loss_pct: self.profile.stop_loss_pct,
            trailing_stop_pct: Some(self.profile.trailing_stop_pct),
            pnl_sol: 0.0,
            pnl_pct: 0.0,
            status: PositionStatus::Open,
            token_source: token_source.to_string(),
            pool_address,
            token_decimals,
            price_source,
            price_stale: false,
            last_price_at: None,
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

    /// Mark a position as ClosedError — sell failed after all retries.
    /// This stops TP/SL monitor from re-triggering sell attempts.
    pub fn close_position_error(&self, mint: &str) -> Result<()> {
        self.close_position_with_status(mint, "sell_failed", PositionStatus::ClosedError)
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

    /// Backfill `token_decimals` for any open positions where it's still the default.
    /// Reads decimals from the on-chain mint account. Falls back to 6 with warning.
    /// Should be called after `load_from_db` on startup.
    pub fn backfill_decimals(&self, rpc: &RpcClient) -> Result<()> {
        let positions: Vec<Position> = {
            let map = self
                .positions
                .read()
                .map_err(|e| anyhow::anyhow!("Position lock poisoned: {e}"))?;
            map.values().cloned().collect()
        };

        let mut updated = 0;
        for pos in positions {
            // Only backfill open positions; closed ones don't matter
            if !matches!(pos.status, PositionStatus::Open) {
                continue;
            }

            let pk = match Pubkey::from_str(&pos.token_mint) {
                Ok(p) => p,
                Err(e) => {
                    warn!(mint = %pos.token_mint, error = %e, "Backfill: invalid mint pubkey");
                    continue;
                }
            };

            let decimals = match rpc.get_account(&pk) {
                Ok(account) => match Mint::unpack(&account.data) {
                    Ok(mint_state) => mint_state.decimals,
                    Err(_) => {
                        // Token-2022: decimals is at offset 44
                        if account.data.len() >= 45 {
                            account.data[44]
                        } else {
                            warn!(mint = %pos.token_mint, "Backfill: cannot decode decimals, using 6");
                            6
                        }
                    }
                },
                Err(e) => {
                    warn!(
                        mint = %pos.token_mint,
                        error = %e,
                        "Backfill: failed to fetch mint account, keeping current decimals"
                    );
                    continue;
                }
            };

            if decimals == pos.token_decimals {
                continue;  // already correct
            }

            // Update in-memory and persist
            {
                let mut map = self
                    .positions
                    .write()
                    .map_err(|e| anyhow::anyhow!("Position lock poisoned: {e}"))?;
                if let Some(p) = map.get_mut(&pos.token_mint) {
                    p.token_decimals = decimals;
                    let p_clone = p.clone();
                    drop(map);
                    self.persist_position(&p_clone)?;
                    updated += 1;
                    info!(
                        mint = %pos.token_mint,
                        old = pos.token_decimals,
                        new = decimals,
                        "Backfilled token_decimals"
                    );
                }
            }
        }

        info!(updated, "Decimals backfill complete");
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
                 status, buy_tx, sell_tx, opened_at, closed_at, security_score, \
                 token_source, pool_address, token_decimals, price_source, price_stale, last_price_at \
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
                    status: match row.get::<_, String>(14).unwrap_or_default().as_str() {
                        "ClosedTp" => PositionStatus::ClosedTp,
                        "ClosedSl" => PositionStatus::ClosedSl,
                        "ClosedManual" => PositionStatus::ClosedManual,
                        "ClosedError" => PositionStatus::ClosedError,
                        _ => PositionStatus::Open,
                    },
                    buy_tx: row.get(15)?,
                    sell_tx: row.get(16)?,
                    opened_at: row.get::<_, String>(17)
                        .ok()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(Utc::now),
                    closed_at: row.get::<_, String>(18)
                        .ok()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                        .map(|dt| dt.with_timezone(&Utc)),
                    security_score: row.get::<_, u32>(19).unwrap_or(0) as u8,
                    age_secs: 0,
                    token_source: row.get::<_, Option<String>>(20)
                        .ok().flatten().unwrap_or_else(|| "Unknown".to_string()),
                    pool_address: row.get::<_, Option<String>>(21).ok().flatten(),
                    token_decimals: row.get::<_, Option<u32>>(22)
                        .ok().flatten().unwrap_or(6) as u8,
                    price_source: row.get::<_, Option<String>>(23).ok().flatten(),
                    price_stale: row.get::<_, Option<i32>>(24)
                        .ok().flatten().unwrap_or(0) != 0,
                    last_price_at: row.get::<_, Option<String>>(25)
                        .ok().flatten()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                        .map(|dt| dt.with_timezone(&Utc)),
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
              status, buy_tx, sell_tx, opened_at, closed_at, security_score, \
              token_source, pool_address, token_decimals, price_source, price_stale, last_price_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
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
                position.token_source,
                position.pool_address,
                position.token_decimals as u32,
                position.price_source,
                position.price_stale as i32,
                position.last_price_at.map(|t| t.to_rfc3339()),
            ],
        )
        .context("Failed to persist position")?;

        Ok(())
    }
}
