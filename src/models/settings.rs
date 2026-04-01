use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSettings {
    pub chat_id: i64,
    pub sniper_enabled: bool,
    pub auto_buy_amount_sol: f64,
    pub slippage_bps: u16,
    pub take_profit_pct: f64,
    pub stop_loss_pct: f64,
    pub trailing_stop_pct: Option<f64>,
    pub min_score_auto_buy: u8,
    pub min_score_notify: u8,
    pub max_buy_sol: f64,
    pub max_positions: u32,
    pub daily_loss_limit_sol: f64,
    pub trade_cooldown_secs: u64,
    pub active_wallet_index: u32,
    pub notify_new_tokens: bool,
    pub notify_trades: bool,
    pub notify_pnl: bool,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            chat_id: 0,
            sniper_enabled: false,
            auto_buy_amount_sol: 0.1,
            slippage_bps: 500,
            take_profit_pct: 100.0,
            stop_loss_pct: 30.0,
            trailing_stop_pct: None,
            min_score_auto_buy: 75,
            min_score_notify: 50,
            max_buy_sol: 1.0,
            max_positions: 10,
            daily_loss_limit_sol: 5.0,
            trade_cooldown_secs: 30,
            active_wallet_index: 0,
            notify_new_tokens: true,
            notify_trades: true,
            notify_pnl: true,
        }
    }
}
