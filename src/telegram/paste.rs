use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};
use tracing::warn;

use super::buy_ui;
use super::{BotState, PendingAction};
use crate::db::queries as db;
use crate::models::UserSettings;

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
/// Also handles pending settings input.
pub async fn handle_message(
    bot: Bot,
    msg: Message,
    state: Arc<BotState>,
) -> Result<(), teloxide::RequestError> {
    // AUTHORIZATION: silently drop messages from any non-admin chat.
    if !super::bot::is_authorized(msg.chat.id.0, &state) {
        return Ok(());
    }

    // RATE LIMIT: even authorised users must not be able to spam paste
    // messages. Each paste of a token address kicks off analyzer work
    // (RPC calls, security checks, DB writes), so rapid paste can
    // exhaust resources or rack up API cost. Same token-bucket as
    // /buy and callbacks: 5 burst, 1/s steady.
    if !state.rate_limiter.try_acquire(&msg.chat.id.0) {
        warn!("Rate-limited paste from chat {}", msg.chat.id.0);
        let _ = bot
            .send_message(
                msg.chat.id,
                "\u{23f3} Terlalu banyak paste. Coba lagi sebentar.",
            )
            .await;
        return Ok(());
    }

    let text = match msg.text() {
        Some(t) => t.trim(),
        None => return Ok(()),
    };

    let chat_id = msg.chat.id.0;

    // Check if there's a pending action for this chat
    let pending = {
        let mut actions = state.pending_actions.lock().await;
        actions.remove(&chat_id)
    };

    if let Some(action) = pending {
        handle_pending_input(&bot, &msg, &state, action, text).await?;
        return Ok(());
    }

    // Check for wallet import (base58 private key = 64 bytes = 87-88 chars in base58)
    // Regular Solana address is 32-44 chars

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

/// Handle user input for a pending settings edit action.
async fn handle_pending_input(
    bot: &Bot,
    msg: &Message,
    state: &BotState,
    action: PendingAction,
    input: &str,
) -> Result<(), teloxide::RequestError> {
    let chat_id = msg.chat.id;
    let chat_id_i64 = chat_id.0;

    let mut settings = db::get_settings(&state.db, chat_id_i64).unwrap_or_else(|_| {
        UserSettings { chat_id: chat_id_i64, ..Default::default() }
    });

    match action {
        PendingAction::EditSlippage => {
            match input.parse::<f64>() {
                Ok(pct) if pct >= 0.1 && pct <= 50.0 => {
                    let bps = (pct * 100.0) as u16;
                    settings.slippage_bps = bps;
                    save_and_confirm(bot, chat_id, state, &settings,
                        &format!("\u{2705} Slippage diubah: <b>{:.1}% ({} bps)</b>", pct, bps),
                    ).await?;
                }
                _ => {
                    bot.send_message(chat_id, "\u{274c} Input tidak valid. Masukkan angka antara <b>0.1</b> dan <b>50</b>.")
                        .parse_mode(ParseMode::Html).await?;
                    // Re-set pending action so user can try again
                    state.pending_actions.lock().await.insert(chat_id_i64, PendingAction::EditSlippage);
                }
            }
        }
        PendingAction::EditTakeProfit => {
            match input.parse::<f64>() {
                Ok(pct) if pct >= 1.0 && pct <= 10000.0 => {
                    settings.take_profit_pct = pct;
                    save_and_confirm(bot, chat_id, state, &settings,
                        &format!("\u{2705} Take Profit diubah: <b>+{:.1}%</b>", pct),
                    ).await?;
                }
                _ => {
                    bot.send_message(chat_id, "\u{274c} Input tidak valid. Masukkan angka antara <b>1</b> dan <b>10000</b>.")
                        .parse_mode(ParseMode::Html).await?;
                    state.pending_actions.lock().await.insert(chat_id_i64, PendingAction::EditTakeProfit);
                }
            }
        }
        PendingAction::EditStopLoss => {
            match input.parse::<f64>() {
                Ok(pct) if pct >= 1.0 && pct <= 99.0 => {
                    settings.stop_loss_pct = pct;
                    save_and_confirm(bot, chat_id, state, &settings,
                        &format!("\u{2705} Stop Loss diubah: <b>-{:.1}%</b>", pct),
                    ).await?;
                }
                _ => {
                    bot.send_message(chat_id, "\u{274c} Input tidak valid. Masukkan angka antara <b>1</b> dan <b>99</b>.")
                        .parse_mode(ParseMode::Html).await?;
                    state.pending_actions.lock().await.insert(chat_id_i64, PendingAction::EditStopLoss);
                }
            }
        }
        PendingAction::EditTrailingStop => {
            match input.parse::<f64>() {
                Ok(pct) if pct == 0.0 => {
                    settings.trailing_stop_pct = None;
                    save_and_confirm(bot, chat_id, state, &settings,
                        "\u{2705} Trailing Stop: <b>OFF</b>",
                    ).await?;
                }
                Ok(pct) if pct >= 5.0 && pct <= 80.0 => {
                    settings.trailing_stop_pct = Some(pct);
                    save_and_confirm(bot, chat_id, state, &settings,
                        &format!("\u{2705} Trailing Stop diubah: <b>{:.1}%</b>", pct),
                    ).await?;
                }
                _ => {
                    bot.send_message(chat_id, "\u{274c} Input tidak valid. Masukkan <b>0</b> (off) atau angka antara <b>5</b> dan <b>80</b>.")
                        .parse_mode(ParseMode::Html).await?;
                    state.pending_actions.lock().await.insert(chat_id_i64, PendingAction::EditTrailingStop);
                }
            }
        }
        PendingAction::EditAutoBuyAmount => {
            match input.parse::<f64>() {
                Ok(sol) if sol >= 0.001 && sol <= 10.0 => {
                    settings.auto_buy_amount_sol = sol;
                    save_and_confirm(bot, chat_id, state, &settings,
                        &format!("\u{2705} Auto-Buy Amount diubah: <b>{:.4} SOL</b>", sol),
                    ).await?;
                }
                _ => {
                    bot.send_message(chat_id, "\u{274c} Input tidak valid. Masukkan angka antara <b>0.001</b> dan <b>10</b>.")
                        .parse_mode(ParseMode::Html).await?;
                    state.pending_actions.lock().await.insert(chat_id_i64, PendingAction::EditAutoBuyAmount);
                }
            }
        }
        PendingAction::EditMinScore => {
            match input.parse::<u8>() {
                Ok(score) if score >= 1 && score <= 100 => {
                    settings.min_score_auto_buy = score;
                    save_and_confirm(bot, chat_id, state, &settings,
                        &format!("\u{2705} Min Score Auto-Buy diubah: <b>{}/100</b>", score),
                    ).await?;
                }
                _ => {
                    bot.send_message(chat_id, "\u{274c} Input tidak valid. Masukkan angka antara <b>1</b> dan <b>100</b>.")
                        .parse_mode(ParseMode::Html).await?;
                    state.pending_actions.lock().await.insert(chat_id_i64, PendingAction::EditMinScore);
                }
            }
        }
        PendingAction::ImportWallet => {
            // Try to import the private key
            match state.wallet_manager.import_wallet(input, &state.wallet_password, Some("Imported")) {
                Ok(pubkey) => {
                    // Auto-set active if first wallet
                    if let Ok(wallets) = state.wallet_manager.list_wallets() {
                        if wallets.len() == 1 {
                            if let Some(w) = wallets.first() {
                                let _ = state.wallet_manager.set_active(w.id);
                            }
                        }
                    }
                    let kb = InlineKeyboardMarkup::new(vec![
                        vec![InlineKeyboardButton::callback("\u{1f45b} Wallet", "wallet")],
                        vec![InlineKeyboardButton::callback("\u{2b05}\u{fe0f} Menu", "menu")],
                    ]);
                    bot.send_message(
                        chat_id,
                        format!(
                            "\u{2705} <b>Wallet Imported!</b>\n\n\
                             \u{1f4cb} Address:\n<code>{}</code>",
                            pubkey
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .reply_markup(kb)
                    .await?;
                }
                Err(e) => {
                    bot.send_message(
                        chat_id,
                        format!("\u{274c} Import gagal: {}\n\nPastikan private key base58 valid (64 bytes).", e),
                    ).await?;
                }
            }
        }
    }

    Ok(())
}

/// Save settings to DB and send confirmation + re-show settings panel.
async fn save_and_confirm(
    bot: &Bot,
    chat_id: ChatId,
    state: &BotState,
    settings: &UserSettings,
    confirm_text: &str,
) -> Result<(), teloxide::RequestError> {
    if let Err(e) = db::save_settings(&state.db, settings) {
        warn!("Failed to save settings: {}", e);
        bot.send_message(chat_id, format!("\u{274c} Gagal simpan: {}", e)).await?;
        return Ok(());
    }

    bot.send_message(chat_id, confirm_text)
        .parse_mode(ParseMode::Html)
        .await?;

    // Re-show settings panel
    let (text, kb) = super::settings_ui::build_settings_message(settings);
    bot.send_message(chat_id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;

    Ok(())
}

/// Process a detected token mint address: show info and buy keyboard.
pub async fn handle_paste(bot: &Bot, msg: &Message, mint: &str) -> Result<()> {
    let short_mint = if mint.len() > 10 {
        format!("{}...{}", &mint[..6], &mint[mint.len() - 4..])
    } else {
        mint.to_string()
    };

    // TODO: Fetch real token info from on-chain / API
    let text = format!(
        "\u{1f50d} <b>Token Detected</b>\n\
         \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
         \u{1f4e6} <b>Mint:</b> <code>{}</code>\n\
         \u{1f3f7}\u{fe0f} <b>Name:</b> <i>Loading...</i>\n\
         \u{1f4b2} <b>Price:</b> <i>Loading...</i>\n\
         \u{1f4b0} <b>Liquidity:</b> <i>Loading...</i>\n\
         \u{1f6e1}\u{fe0f} <b>Score:</b> <i>Analyzing...</i>\n\
         \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
         \u{1f4b0} <b>Pilih jumlah SOL untuk beli {}:</b>",
        mint, short_mint
    );

    let kb = buy_ui::build_buy_keyboard(mint);

    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;

    Ok(())
}
