use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use crate::models::UserSettings;

/// Build the sniper mode status message and toggle keyboard.
pub fn build_sniper_message(
    enabled: bool,
    settings: &UserSettings,
) -> (String, InlineKeyboardMarkup) {
    let status = if enabled {
        "\u{1f7e2} <b>ACTIVE</b>"
    } else {
        "\u{1f534} <b>INACTIVE</b>"
    };

    let text = format!(
        "\u{1f3af} <b>Sniper Mode</b>\n\
         \u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\n\n\
         \u{1f4a1} Status: {}\n\n\
         <b>Auto-Buy Settings:</b>\n\
         \u{1f4b0} Amount: <b>{:.2} SOL</b>\n\
         \u{1f6e1}\u{fe0f} Min Score: <b>{}/100</b>\n\
         \u{1f4ca} Max Buy: <b>{:.2} SOL</b>\n\
         \u{2705} TP: <b>{:.0}%</b>\n\
         \u{274c} SL: <b>{:.0}%</b>\n\
         \u{1f4ca} Slippage: <b>{} bps</b>\n\n\
         <i>Ketika aktif, bot akan otomatis membeli token baru yang lolos security check dengan skor >= {}.</i>",
        status,
        settings.auto_buy_amount_sol,
        settings.min_score_auto_buy,
        settings.max_buy_sol,
        settings.take_profit_pct,
        settings.stop_loss_pct,
        settings.slippage_bps,
        settings.min_score_auto_buy,
    );

    let toggle_label = if enabled {
        "\u{1f534} Matikan Sniper"
    } else {
        "\u{1f7e2} Aktifkan Sniper"
    };

    let kb = InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(toggle_label, "snipe_toggle")],
        vec![
            InlineKeyboardButton::callback("\u{2699}\u{fe0f} Edit Settings", "settings"),
            InlineKeyboardButton::callback("\u{2b05}\u{fe0f} Menu Utama", "menu"),
        ],
    ]);

    (text, kb)
}
