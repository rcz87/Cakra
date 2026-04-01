use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use crate::models::Position;

/// Build the positions dashboard message and keyboard.
pub fn build_positions_message(positions: &[Position]) -> (String, InlineKeyboardMarkup) {
    if positions.is_empty() {
        let text = "\
\u{1f4c2} <b>Posisi Aktif</b>\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
\u{1f4ad} Tidak ada posisi aktif.\n\n\
\u{1f4a1} <i>Paste alamat token atau klik Buy\nuntuk membuka posisi baru.</i>".to_string();
        let kb = InlineKeyboardMarkup::new(vec![
            vec![
                InlineKeyboardButton::callback("\u{1f4b0} Buy Token", "buy_prompt"),
                InlineKeyboardButton::callback("\u{2b05}\u{fe0f} Menu Utama", "menu"),
            ],
        ]);
        return (text, kb);
    }

    let mut text = String::from(
        "\u{1f4c2} <b>Posisi Aktif</b>\n\
         \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n");
    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    let mut total_pnl = 0.0_f64;
    let mut total_invested = 0.0_f64;

    for (i, pos) in positions.iter().enumerate() {
        let pnl_emoji = if pos.pnl_pct >= 0.0 { "\u{1f7e2}" } else { "\u{1f534}" };
        let pnl_sign = if pos.pnl_pct >= 0.0 { "+" } else { "" };

        let age_str = if pos.age_secs > 3600 {
            format!("{}h {}m", pos.age_secs / 3600, (pos.age_secs % 3600) / 60)
        } else if pos.age_secs > 60 {
            format!("{}m", pos.age_secs / 60)
        } else {
            format!("{}s", pos.age_secs)
        };

        text.push_str(&format!(
            "{} <b>#{} {}</b>  \u{23f1} {}\n\
             \u{1f4b5} {:.4} SOL \u{27a1} {} <b>{}{:.2}%</b> ({}{:.4})\n\n",
            pnl_emoji,
            i + 1,
            pos.token_symbol,
            age_str,
            pos.entry_amount_sol,
            pnl_emoji,
            pnl_sign,
            pos.pnl_pct,
            pnl_sign,
            pos.pnl_sol,
        ));

        total_pnl += pos.pnl_sol;
        total_invested += pos.entry_amount_sol;

        // Sell buttons for this position
        let sym = if pos.token_symbol.len() > 6 {
            &pos.token_symbol[..6]
        } else {
            &pos.token_symbol
        };
        buttons.push(vec![
            InlineKeyboardButton::callback(
                format!("\u{1f4e4} {} 25%", sym),
                format!("sell:25:{}", pos.token_mint),
            ),
            InlineKeyboardButton::callback(
                "50%".to_string(),
                format!("sell:50:{}", pos.token_mint),
            ),
            InlineKeyboardButton::callback(
                "100%".to_string(),
                format!("sell:100:{}", pos.token_mint),
            ),
        ]);
    }

    let total_sign = if total_pnl >= 0.0 { "+" } else { "" };
    let total_emoji = if total_pnl >= 0.0 { "\u{1f4b9}" } else { "\u{1f4c9}" };
    let total_pct = if total_invested > 0.0 {
        (total_pnl / total_invested) * 100.0
    } else {
        0.0
    };

    text.push_str(&format!(
        "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
         {} <b>Total: {}{:.4} SOL ({}{:.1}%)</b>",
        total_emoji, total_sign, total_pnl, total_sign, total_pct
    ));

    // Sell All + Back
    buttons.push(vec![
        InlineKeyboardButton::callback("\u{1f6a8} Sell ALL 100%", "sell_all"),
        InlineKeyboardButton::callback("\u{1f504} Refresh", "positions"),
    ]);
    buttons.push(vec![InlineKeyboardButton::callback(
        "\u{2b05}\u{fe0f} Menu Utama",
        "menu",
    )]);

    let kb = InlineKeyboardMarkup::new(buttons);
    (text, kb)
}
