use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PositionStatus {
    Open,
    ClosedTp,
    ClosedSl,
    ClosedManual,
    ClosedError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub id: String,
    pub token_mint: String,
    pub token_symbol: String,
    pub wallet_pubkey: String,
    pub entry_price_sol: f64,
    pub entry_amount_sol: f64,
    pub token_amount: f64,
    pub current_price_sol: f64,
    pub highest_price_sol: f64,
    pub take_profit_pct: f64,
    pub stop_loss_pct: f64,
    pub trailing_stop_pct: Option<f64>,
    pub pnl_sol: f64,
    pub pnl_pct: f64,
    pub status: PositionStatus,
    pub buy_tx: String,
    pub sell_tx: Option<String>,
    pub opened_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub security_score: u8,
}

impl Position {
    pub fn update_pnl(&mut self, current_price: f64) {
        self.current_price_sol = current_price;
        if current_price > self.highest_price_sol {
            self.highest_price_sol = current_price;
        }
        self.pnl_sol = (current_price - self.entry_price_sol) * self.token_amount;
        if self.entry_price_sol > 0.0 {
            self.pnl_pct = ((current_price / self.entry_price_sol) - 1.0) * 100.0;
        }
    }

    pub fn should_take_profit(&self) -> bool {
        self.pnl_pct >= self.take_profit_pct
    }

    pub fn should_stop_loss(&self) -> bool {
        self.pnl_pct <= -self.stop_loss_pct
    }

    pub fn should_trailing_stop(&self) -> bool {
        if let Some(trailing_pct) = self.trailing_stop_pct {
            if self.highest_price_sol > 0.0 {
                let drop_from_high =
                    ((self.highest_price_sol - self.current_price_sol) / self.highest_price_sol) * 100.0;
                return drop_from_high >= trailing_pct;
            }
        }
        false
    }
}
