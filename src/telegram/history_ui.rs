use crate::models::trade::{Trade, TradeType};

/// Build the trade history message (last 20 trades).
pub fn build_history_message(trades: &[Trade]) -> String {
    if trades.is_empty() {
        return "\u{1f4dc} <b>Trade History</b>\n\nBelum ada riwayat trading.".to_string();
    }

    let mut text = String::from(
        "\u{1f4dc} <b>Trade History</b>\n\
         \u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\n\n",
    );

    let mut total_pnl = 0.0_f64;
    let display_trades = if trades.len() > 20 {
        &trades[trades.len() - 20..]
    } else {
        trades
    };

    for (i, trade) in display_trades.iter().enumerate() {
        let type_emoji = match trade.trade_type {
            TradeType::Buy => "\u{1f7e2} BUY",
            TradeType::Sell => "\u{1f534} SELL",
        };

        let pnl_text = match trade.pnl_sol {
            Some(pnl) => {
                total_pnl += pnl;
                let sign = if pnl >= 0.0 { "+" } else { "" };
                format!(" | PnL: {}{:.4} SOL", sign, pnl)
            }
            None => String::new(),
        };

        let time_str = trade.created_at.format("%m/%d %H:%M");

        text.push_str(&format!(
            "<b>{}.</b> {} <b>{}</b>\n\
             \u{00a0}\u{00a0}\u{00a0}\u{1f4b0} {:.4} SOL{}\n\
             \u{00a0}\u{00a0}\u{00a0}\u{1f552} {}\n\n",
            i + 1,
            type_emoji,
            trade.token_symbol,
            trade.amount_sol,
            pnl_text,
            time_str,
        ));
    }

    let total_sign = if total_pnl >= 0.0 { "+" } else { "" };
    let total_emoji = if total_pnl >= 0.0 { "\u{1f4b9}" } else { "\u{1f4c9}" };

    text.push_str(&format!(
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\n\
         {} <b>Total PnL: {}{:.4} SOL</b>",
        total_emoji, total_sign, total_pnl
    ));

    text
}
