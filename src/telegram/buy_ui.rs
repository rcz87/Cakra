use anyhow::Result;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

/// Build the buy-amount selection keyboard for a given token mint.
pub fn build_buy_keyboard(mint: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        // Row 1: preset amounts
        vec![
            InlineKeyboardButton::callback(
                "\u{25aa}\u{fe0f} 0.1 SOL",
                format!("buy:0.1:{}", mint),
            ),
            InlineKeyboardButton::callback(
                "\u{25ab}\u{fe0f} 0.5 SOL",
                format!("buy:0.5:{}", mint),
            ),
            InlineKeyboardButton::callback(
                "\u{1f7e1} 1 SOL",
                format!("buy:1:{}", mint),
            ),
        ],
        // Row 2: custom + cancel
        vec![
            InlineKeyboardButton::callback(
                "\u{270f}\u{fe0f} Custom",
                format!("buy_custom:{}", mint),
            ),
            InlineKeyboardButton::callback("\u{274c} Cancel", "cancel"),
        ],
    ])
}

/// Execute a buy for the given amount and token mint.
/// Returns Ok on success, Err on failure. The actual execution is delegated
/// to the executor module.
pub async fn handle_buy_callback(amount_sol: f64, mint: &str) -> Result<()> {
    tracing::info!(
        "Buy callback: {} SOL for mint {}",
        amount_sol,
        mint
    );

    // Validate amount
    if amount_sol <= 0.0 {
        anyhow::bail!("Amount harus > 0");
    }

    if amount_sol > 10.0 {
        anyhow::bail!("Amount terlalu besar (max 10 SOL per trade)");
    }

    // TODO: Call executor::execute_buy(mint, amount_sol).await
    // TODO: Record trade in DB
    // TODO: Create position entry

    tracing::info!(
        "Buy order queued: {} SOL -> {}",
        amount_sol,
        mint
    );

    Ok(())
}
