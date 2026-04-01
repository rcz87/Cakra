use anyhow::Result;
use tracing::{info, warn};

use crate::config::Config;
use crate::db::DbPool;

/// Result of a risk check evaluation.
#[derive(Debug, Clone)]
pub enum RiskCheck {
    /// Trade is allowed to proceed.
    Allowed,
    /// Trade is denied with the given reason.
    Denied(String),
}

/// Central risk manager that enforces trading limits and protections
/// for RICOZ SNIPER.
pub struct RiskManager {
    pub config: Config,
    pub db: DbPool,
}

impl RiskManager {
    pub fn new(config: Config, db: DbPool) -> Self {
        Self { config, db }
    }

    /// Check whether a trade of the given SOL amount is allowed.
    ///
    /// Evaluates the following rules:
    /// 1. Amount must not exceed the per-trade max buy limit.
    /// 2. Open positions must not exceed the max positions limit.
    /// 3. Cumulative daily realized losses must not exceed the daily loss limit.
    pub fn can_trade(&self, amount_sol: f64) -> Result<RiskCheck> {
        // Rule 1: Max buy limit
        if amount_sol > self.config.max_buy_sol {
            let reason = format!(
                "Buy amount {:.4} SOL exceeds max buy limit of {:.4} SOL",
                amount_sol, self.config.max_buy_sol
            );
            warn!(reason = %reason, "Risk check denied");
            return Ok(RiskCheck::Denied(reason));
        }

        // Rule 2: Max open positions
        let open_positions = self.count_open_positions()?;
        if open_positions >= self.config.max_positions as i64 {
            let reason = format!(
                "Already at max open positions ({}/{})",
                open_positions, self.config.max_positions
            );
            warn!(reason = %reason, "Risk check denied");
            return Ok(RiskCheck::Denied(reason));
        }

        // Rule 3: Daily loss limit
        let daily_loss = self.get_daily_loss()?;
        if daily_loss >= self.config.daily_loss_limit_sol {
            let reason = format!(
                "Daily loss limit reached ({:.4}/{:.4} SOL)",
                daily_loss, self.config.daily_loss_limit_sol
            );
            warn!(reason = %reason, "Risk check denied");
            return Ok(RiskCheck::Denied(reason));
        }

        info!(
            amount_sol = amount_sol,
            open_positions = open_positions,
            daily_loss = daily_loss,
            "Risk check passed"
        );
        Ok(RiskCheck::Allowed)
    }

    /// Count the number of currently open positions from the database.
    fn count_open_positions(&self) -> Result<i64> {
        let conn = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock error: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM positions WHERE status = 'Open'",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Calculate the total realized loss for today (UTC).
    /// Only counts negative PnL from closed trades today.
    fn get_daily_loss(&self) -> Result<f64> {
        let conn = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock error: {}", e))?;
        let loss: f64 = conn
            .query_row(
                "SELECT COALESCE(SUM(ABS(pnl_sol)), 0.0) FROM trades
                 WHERE pnl_sol < 0
                   AND DATE(created_at) = DATE('now')",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0.0);
        Ok(loss)
    }
}

impl RiskCheck {
    /// Returns `true` if the trade is allowed.
    pub fn is_allowed(&self) -> bool {
        matches!(self, RiskCheck::Allowed)
    }

    /// Returns the denial reason, if any.
    pub fn reason(&self) -> Option<&str> {
        match self {
            RiskCheck::Allowed => None,
            RiskCheck::Denied(reason) => Some(reason),
        }
    }
}
