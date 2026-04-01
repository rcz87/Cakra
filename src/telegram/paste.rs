use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::ParseMode;

use super::buy_ui;
use super::BotState;

/// Check if a string looks like a valid Solana base58 address (32-44 chars).
fn is_solana_address(s: &str) -> bool {
    let len = s.len();
    if !(32..=44).contains(&len) {
        return false;
    }
    // Base58 alphabet (no 0, O, I, l)
    s.chars()
        .all(|c| matches!(c, '1'..='9' | 'A'..='H' | 'J'..='N' | 'P'..='Z' | 'a'..='k' | 'm'..='z'))
}

/// Handle a paste-to-trade: when user sends a plain message, try to detect a Solana
/// token mint address and show token info with buy buttons.
pub async fn handle_message(
    bot: Bot,
    msg: Message,
    state: Arc<BotState>,
) -> Result<(), teloxide::RequestError> {
    let text = match msg.text() {
        Some(t) => t.trim(),
        None => return Ok(()),
    };

    // Only process if it looks like a bare Solana address
    if !is_solana_address(text) {
        return Ok(());
    }

    let mint = text.to_string();

    if let Err(e) = handle_paste(&bot, &msg, &mint).await {
        tracing::error!("Paste handler error for {}: {}", mint, e);
        bot.send_message(msg.chat.id, format!("\u{274c} Error loading token info: {}", e))
            .await?;
    }

    Ok(())
}

/// Process a detected token mint address: show info and buy keyboard.
pub async fn handle_paste(bot: &Bot, msg: &Message, mint: &str) -> Result<()> {
    let short_mint = if mint.len() > 8 {
        format!("{}...{}", &mint[..4], &mint[mint.len() - 4..])
    } else {
        mint.to_string()
    };

    // TODO: Fetch real token info from on-chain / API
    let text = format!(
        "\u{1f50d} <b>Token Detected!</b>\n\n\
         \u{1f4e6} Mint: <code>{}</code>\n\
         \u{1f3f7}\u{fe0f} Name: <i>Loading...</i>\n\
         \u{1f4ca} Price: <i>Loading...</i>\n\
         \u{1f4b0} Liquidity: <i>Loading...</i>\n\
         \u{1f6e1}\u{fe0f} Security Score: <i>Loading...</i>\n\n\
         \u{23f3} <i>Fetching data untuk {}</i>...",
        mint, short_mint
    );

    let kb = buy_ui::build_buy_keyboard(mint);

    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;

    Ok(())
}
