use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

/// Format the main menu text.
pub fn format_main_menu_text(
    balance: f64,
    positions: usize,
    daily_pnl: f64,
    sniper_on: bool,
) -> String {
    let pnl_sign = if daily_pnl >= 0.0 { "+" } else { "" };
    let sniper_status = if sniper_on { "\u{1f7e2} ON" } else { "\u{1f534} OFF" };

    format!(
        "\u{1f3af} <b>RICOZ SNIPER</b> \u{1f3af}\n\
         \u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\n\n\
         \u{1f4b0} Balance: <b>{:.4} SOL</b>\n\
         \u{1f4c2} Posisi Aktif: <b>{}</b>\n\
         \u{1f4c8} PnL Hari Ini: <b>{}{:.4} SOL</b>\n\
         \u{1f916} Sniper: <b>{}</b>\n\n\
         \u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\n\
         <i>Pilih menu di bawah ini:</i>",
        balance, positions, pnl_sign, daily_pnl, sniper_status
    )
}

/// Build the main menu inline keyboard.
pub fn build_main_menu(
    _balance: f64,
    _positions: usize,
    _daily_pnl: f64,
    sniper_on: bool,
) -> InlineKeyboardMarkup {
    let snipe_label = if sniper_on {
        "\u{1f7e2} Sniper ON"
    } else {
        "\u{1f534} Sniper OFF"
    };

    InlineKeyboardMarkup::new(vec![
        // Row 1: Snipe toggle + Buy
        vec![
            InlineKeyboardButton::callback(snipe_label, "snipe_toggle"),
            InlineKeyboardButton::callback("\u{1f4b0} Buy", "buy_select:"),
        ],
        // Row 2: Positions + Wallet
        vec![
            InlineKeyboardButton::callback("\u{1f4c2} Positions", "positions"),
            InlineKeyboardButton::callback("\u{1f45b} Wallet", "wallet"),
        ],
        // Row 3: Settings + History
        vec![
            InlineKeyboardButton::callback("\u{2699}\u{fe0f} Settings", "settings"),
            InlineKeyboardButton::callback("\u{1f4dc} History", "history"),
        ],
    ])
}
