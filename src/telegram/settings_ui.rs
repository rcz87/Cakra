use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use crate::models::UserSettings;

/// Build the settings panel message and keyboard.
pub fn build_settings_message(settings: &UserSettings) -> (String, InlineKeyboardMarkup) {
    let trailing = match settings.trailing_stop_pct {
        Some(pct) => format!("{:.1}%", pct),
        None => "OFF".to_string(),
    };

    let notif_tokens = if settings.notify_new_tokens { "\u{1f7e2}" } else { "\u{1f534}" };
    let notif_trades = if settings.notify_trades { "\u{1f7e2}" } else { "\u{1f534}" };
    let notif_pnl = if settings.notify_pnl { "\u{1f7e2}" } else { "\u{1f534}" };

    let text = format!(
        "\u{2699}\u{fe0f} <b>Settings</b>\n\
         \u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\n\n\
         \u{1f4b0} <b>Trading</b>\n\
         \u{2022} Auto-Buy Amount: <b>{:.2} SOL</b>\n\
         \u{2022} Max Buy: <b>{:.2} SOL</b>\n\
         \u{2022} Slippage: <b>{} bps</b> ({:.1}%)\n\
         \u{2022} Max Posisi: <b>{}</b>\n\
         \u{2022} Cooldown: <b>{}s</b>\n\n\
         \u{1f6e1}\u{fe0f} <b>Risk Management</b>\n\
         \u{2022} Take Profit: <b>{:.1}%</b>\n\
         \u{2022} Stop Loss: <b>{:.1}%</b>\n\
         \u{2022} Trailing Stop: <b>{}</b>\n\
         \u{2022} Daily Loss Limit: <b>{:.2} SOL</b>\n\n\
         \u{1f916} <b>Sniper</b>\n\
         \u{2022} Min Score Auto-Buy: <b>{}/100</b>\n\
         \u{2022} Min Score Notify: <b>{}/100</b>\n\n\
         \u{1f514} <b>Notifikasi</b>\n\
         {} New Tokens  {} Trades  {} PnL\n\n\
         <i>Tap tombol di bawah untuk edit:</i>",
        settings.auto_buy_amount_sol,
        settings.max_buy_sol,
        settings.slippage_bps,
        settings.slippage_bps as f64 / 100.0,
        settings.max_positions,
        settings.trade_cooldown_secs,
        settings.take_profit_pct,
        settings.stop_loss_pct,
        trailing,
        settings.daily_loss_limit_sol,
        settings.min_score_auto_buy,
        settings.min_score_notify,
        notif_tokens,
        notif_trades,
        notif_pnl,
    );

    let kb = InlineKeyboardMarkup::new(vec![
        // Row 1: Trading
        vec![
            InlineKeyboardButton::callback("\u{1f4b0} Slippage", "set:slippage"),
            InlineKeyboardButton::callback("\u{1f4b5} Buy Amount", "set:auto_buy"),
        ],
        // Row 2: TP/SL
        vec![
            InlineKeyboardButton::callback("\u{2705} Take Profit", "set:tp"),
            InlineKeyboardButton::callback("\u{274c} Stop Loss", "set:sl"),
        ],
        // Row 3: Advanced
        vec![
            InlineKeyboardButton::callback("\u{1f4c9} Trailing Stop", "set:trailing"),
            InlineKeyboardButton::callback("\u{1f6e1}\u{fe0f} Min Score", "set:min_score"),
        ],
        // Row 4: Notif toggles
        vec![
            InlineKeyboardButton::callback(
                format!("{} Tokens", notif_tokens),
                "set:notif_tokens",
            ),
            InlineKeyboardButton::callback(
                format!("{} Trades", notif_trades),
                "set:notif_trades",
            ),
            InlineKeyboardButton::callback(
                format!("{} PnL", notif_pnl),
                "set:notif_pnl",
            ),
        ],
        // Row 5: back
        vec![InlineKeyboardButton::callback("\u{2b05}\u{fe0f} Menu Utama", "menu")],
    ]);

    (text, kb)
}
