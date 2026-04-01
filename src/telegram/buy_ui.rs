use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

/// Build the buy-amount selection keyboard for a given token mint.
pub fn build_buy_keyboard(mint: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        // Row 1: small amounts
        vec![
            InlineKeyboardButton::callback(
                "0.01 SOL",
                format!("buy:0.01:{}", mint),
            ),
            InlineKeyboardButton::callback(
                "0.05 SOL",
                format!("buy:0.05:{}", mint),
            ),
            InlineKeyboardButton::callback(
                "0.1 SOL",
                format!("buy:0.1:{}", mint),
            ),
        ],
        // Row 2: medium amounts
        vec![
            InlineKeyboardButton::callback(
                "0.25 SOL",
                format!("buy:0.25:{}", mint),
            ),
            InlineKeyboardButton::callback(
                "0.5 SOL",
                format!("buy:0.5:{}", mint),
            ),
            InlineKeyboardButton::callback(
                "1.0 SOL",
                format!("buy:1:{}", mint),
            ),
        ],
        // Row 3: large amounts
        vec![
            InlineKeyboardButton::callback(
                "2.0 SOL",
                format!("buy:2:{}", mint),
            ),
            InlineKeyboardButton::callback(
                "5.0 SOL",
                format!("buy:5:{}", mint),
            ),
            InlineKeyboardButton::callback(
                "\u{270f}\u{fe0f} Custom",
                format!("buy_custom:{}", mint),
            ),
        ],
        // Row 4: cancel
        vec![
            InlineKeyboardButton::callback("\u{274c} Batal", "cancel"),
            InlineKeyboardButton::callback("\u{2b05}\u{fe0f} Menu Utama", "menu"),
        ],
    ])
}

