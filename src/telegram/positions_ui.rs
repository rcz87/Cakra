use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use crate::models::Position;

/// Build the positions dashboard message and keyboard.
pub fn build_positions_message(positions: &[Position]) -> (String, InlineKeyboardMarkup) {
    if positions.is_empty() {
        let text = "\u{1f4ad} <b>Posisi Aktif</b>\n\nTidak ada posisi aktif saat ini.".to_string();
        let kb = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            "\u{2b05}\u{fe0f} Kembali",
            "menu",
        )]]);
        return (text, kb);
    }

    let mut text = String::from("\u{1f4c2} <b>Posisi Aktif</b>\n\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\n\n");
    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    let mut total_pnl = 0.0_f64;

    for (i, pos) in positions.iter().enumerate() {
        let pnl_emoji = if pos.pnl_pct >= 0.0 { "\u{1f7e2}" } else { "\u{1f534}" };
        let pnl_sign = if pos.pnl_pct >= 0.0 { "+" } else { "" };

        text.push_str(&format!(
            "{} <b>#{} {}</b>\n\
             \u{1f4b5} Entry: {:.8} SOL\n\
             \u{1f4b2} Current: {:.8} SOL\n\
             {} PnL: <b>{}{:.2}%</b> ({}{:.4} SOL)\n\
             \u{1f6e1}\u{fe0f} Score: {}/100\n\n",
            pnl_emoji,
            i + 1,
            pos.token_symbol,
            pos.entry_price_sol,
            pos.current_price_sol,
            pnl_emoji,
            pnl_sign,
            pos.pnl_pct,
            pnl_sign,
            pos.pnl_sol,
            pos.security_score,
        ));

        total_pnl += pos.pnl_sol;

        // Sell buttons for this position
        buttons.push(vec![
            InlineKeyboardButton::callback(
                format!("\u{1f4e4} {} 25%", pos.token_symbol),
                format!("sell:25:{}", pos.token_mint),
            ),
            InlineKeyboardButton::callback(
                format!("50%"),
                format!("sell:50:{}", pos.token_mint),
            ),
            InlineKeyboardButton::callback(
                format!("100%"),
                format!("sell:100:{}", pos.token_mint),
            ),
        ]);
    }

    let total_sign = if total_pnl >= 0.0 { "+" } else { "" };
    text.push_str(&format!(
        "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\n\
         \u{1f4ca} <b>Total PnL: {}{:.4} SOL</b>",
        total_sign, total_pnl
    ));

    // Back button
    buttons.push(vec![InlineKeyboardButton::callback(
        "\u{2b05}\u{fe0f} Kembali",
        "menu",
    )]);

    let kb = InlineKeyboardMarkup::new(buttons);
    (text, kb)
}
