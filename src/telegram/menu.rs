use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

/// Format the main menu text.
pub fn format_main_menu_text(
    balance: f64,
    positions: usize,
    daily_pnl: f64,
    sniper_on: bool,
) -> String {
    let pnl_sign = if daily_pnl >= 0.0 { "+" } else { "" };
    let pnl_emoji = if daily_pnl >= 0.0 { "\u{1f4b9}" } else { "\u{1f4c9}" };
    let sniper_status = if sniper_on { "\u{1f7e2} ACTIVE" } else { "\u{1f534} OFF" };
    let sniper_dot = if sniper_on { "\u{1f7e2}" } else { "\u{26ab}" };

    format!(
        "\
\u{2b50} <b>RICOZ SNIPER v0.2</b> \u{2b50}\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
\u{1f4b0} <b>Balance:</b>  {:.4} SOL\n\
\u{1f4c2} <b>Posisi:</b>   {} aktif\n\
{} <b>PnL:</b>     {}{:.4} SOL\n\
{} <b>Sniper:</b>  {}\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
\u{1f447} <i>Pilih aksi cepat:</i>",
        balance,
        positions,
        pnl_emoji, pnl_sign, daily_pnl,
        sniper_dot, sniper_status,
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
        "\u{26ab} Sniper OFF"
    };

    InlineKeyboardMarkup::new(vec![
        // Row 1: Quick Trade
        vec![
            InlineKeyboardButton::callback("\u{1f4b0} Buy Token", "buy_prompt"),
            InlineKeyboardButton::callback("\u{1f4e4} Sell Token", "sell_prompt"),
        ],
        // Row 2: Monitoring
        vec![
            InlineKeyboardButton::callback("\u{1f4c2} Positions", "positions"),
            InlineKeyboardButton::callback("\u{1f4dc} History", "history"),
        ],
        // Row 3: Sniper + Wallet
        vec![
            InlineKeyboardButton::callback(snipe_label, "snipe_toggle"),
            InlineKeyboardButton::callback("\u{1f45b} Wallet", "wallet"),
        ],
        // Row 4: Settings + Refresh
        vec![
            InlineKeyboardButton::callback("\u{2699}\u{fe0f} Settings", "settings"),
            InlineKeyboardButton::callback("\u{1f504} Refresh", "menu"),
        ],
        // Row 5: Help
        vec![
            InlineKeyboardButton::callback("\u{2753} Help", "help"),
        ],
    ])
}
