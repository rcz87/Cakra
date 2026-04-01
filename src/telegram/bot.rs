use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use teloxide::utils::command::BotCommands;
use tracing::warn;

use super::BotState;
use super::{buy_ui, history_ui, menu, positions_ui, settings_ui, sniper_ui, wallet_ui};
use crate::db::queries as db;
use crate::models::UserSettings;

/// All available slash-commands for the bot.
#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "RICOZ SNIPER commands:")]
pub enum Command {
    #[command(description = "Show main menu")]
    Start,
    #[command(description = "Show help")]
    Help,
    #[command(description = "Toggle sniper mode")]
    Snipe,
    #[command(description = "Buy token by contract address")]
    Buy(String),
    #[command(description = "Sell token (percentage)")]
    Sell(String),
    #[command(description = "Show open positions")]
    Positions,
    #[command(description = "Wallet management")]
    Wallet,
    #[command(description = "Bot settings")]
    Settings,
    #[command(description = "Trade history")]
    History,
}

/// Entry-point for all recognised commands.
pub async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    state: Arc<BotState>,
) -> Result<(), teloxide::RequestError> {
    match cmd {
        Command::Start => handle_start(bot, msg, state).await?,
        Command::Help => handle_help(bot, msg).await?,
        Command::Snipe => handle_snipe(bot, msg, state).await?,
        Command::Buy(ca) => handle_buy(bot, msg, state, ca.trim().to_string()).await?,
        Command::Sell(args) => handle_sell(bot, msg, state, args).await?,
        Command::Positions => handle_positions(bot, msg, state).await?,
        Command::Wallet => handle_wallet(bot, msg, state).await?,
        Command::Settings => handle_settings(bot, msg, state).await?,
        Command::History => handle_history(bot, msg, state).await?,
    }
    Ok(())
}

/// Callback query router - dispatches inline button presses.
pub async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    state: Arc<BotState>,
) -> Result<(), teloxide::RequestError> {
    let data = match q.data.as_deref() {
        Some(d) => d.to_string(),
        None => return Ok(()),
    };

    // Acknowledge the callback immediately
    bot.answer_callback_query(&q.id).await?;

    let chat_id = match q.message {
        Some(ref m) => m.chat().id,
        None => return Ok(()),
    };

    // Route based on prefix
    if data == "menu" {
        let settings = db::get_settings(&state.db, chat_id.0).unwrap_or_else(|e| {
            warn!("Failed to load settings for chat {}: {}", chat_id.0, e);
            UserSettings { chat_id: chat_id.0, ..Default::default() }
        });
        let balance = 0.0_f64; // TODO: fetch real balance via RPC when available in BotState
        let positions_count = db::get_open_positions(&state.db)
            .map(|p| p.len())
            .unwrap_or(0);
        let daily_pnl = db::get_daily_pnl(&state.db).unwrap_or(0.0);

        let text = menu::format_main_menu_text(balance, positions_count, daily_pnl, settings.sniper_enabled);
        let kb = menu::build_main_menu(balance, positions_count, daily_pnl, settings.sniper_enabled);

        bot.send_message(chat_id, text)
            .parse_mode(ParseMode::Html)
            .reply_markup(kb)
            .await?;
    } else if data == "snipe_toggle" {
        let mut settings = db::get_settings(&state.db, chat_id.0).unwrap_or_else(|e| {
            warn!("Failed to load settings for snipe_toggle: {}", e);
            UserSettings { chat_id: chat_id.0, ..Default::default() }
        });
        settings.sniper_enabled = !settings.sniper_enabled;
        if let Err(e) = db::save_settings(&state.db, &settings) {
            warn!("Failed to save settings after snipe_toggle: {}", e);
        }
        let (text, kb) = sniper_ui::build_sniper_message(settings.sniper_enabled, &settings);
        bot.send_message(chat_id, text)
            .parse_mode(ParseMode::Html)
            .reply_markup(kb)
            .await?;
    } else if let Some(mint) = data.strip_prefix("buy_select:") {
        let kb = buy_ui::build_buy_keyboard(mint);
        bot.send_message(chat_id, "\u{1f4b0} Pilih jumlah SOL untuk beli:")
            .reply_markup(kb)
            .await?;
    } else if let Some(rest) = data.strip_prefix("buy:") {
        // Format: buy:<amount>:<mint>
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() == 2 {
            if let Ok(amount) = parts[0].parse::<f64>() {
                let mint = parts[1];
                match buy_ui::handle_buy_callback(amount, mint).await {
                    Ok(_) => {
                        bot.send_message(
                            chat_id,
                            format!("\u{2705} Buy order submitted!\n\u{1f4e6} Token: `{}`\n\u{1f4b0} Amount: {} SOL", mint, amount),
                        )
                        .parse_mode(ParseMode::MarkdownV2)
                        .await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("\u{274c} Buy gagal: {}", e)).await?;
                    }
                }
            }
        }
    } else if let Some(rest) = data.strip_prefix("sell:") {
        // Format: sell:<pct>:<mint>
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() == 2 {
            let pct = parts[0];
            let mint = parts[1];
            bot.send_message(
                chat_id,
                format!("\u{1f4e4} Sell {}% of `{}`\n\u{23f3} Processing...", pct, mint),
            )
            .await?;
            // TODO: execute sell via executor module
        }
    } else if data == "positions" {
        handle_positions_cb(&bot, chat_id, &state).await?;
    } else if data == "settings" {
        let settings = db::get_settings(&state.db, chat_id.0).unwrap_or_else(|e| {
            warn!("Failed to load settings for chat {}: {}", chat_id.0, e);
            UserSettings { chat_id: chat_id.0, ..Default::default() }
        });
        let (text, kb) = settings_ui::build_settings_message(&settings);
        bot.send_message(chat_id, text)
            .parse_mode(ParseMode::Html)
            .reply_markup(kb)
            .await?;
    } else if data == "wallet" {
        let wallets = load_wallet_infos(&state.db);
        let (text, kb) = wallet_ui::build_wallet_message(&wallets);
        bot.send_message(chat_id, text)
            .parse_mode(ParseMode::Html)
            .reply_markup(kb)
            .await?;
    } else if data == "history" {
        let trades = db::get_recent_trades(&state.db, 20).unwrap_or_default();
        let text = history_ui::build_history_message(&trades);
        bot.send_message(chat_id, text)
            .parse_mode(ParseMode::Html)
            .await?;
    } else if data == "cancel" {
        bot.send_message(chat_id, "\u{274c} Dibatalkan.").await?;
    }

    Ok(())
}

// ── Individual command handlers ──────────────────────────────────────

async fn handle_start(
    bot: Bot,
    msg: Message,
    state: Arc<BotState>,
) -> Result<(), teloxide::RequestError> {
    let chat_id = msg.chat.id.0;
    let settings = db::get_settings(&state.db, chat_id).unwrap_or_else(|e| {
        warn!("Failed to load settings for chat {}: {}", chat_id, e);
        UserSettings { chat_id, ..Default::default() }
    });
    let balance = 0.0_f64; // TODO: fetch real balance via RPC when available in BotState
    let positions_count = db::get_open_positions(&state.db)
        .map(|p| p.len())
        .unwrap_or(0);
    let daily_pnl = db::get_daily_pnl(&state.db).unwrap_or(0.0);

    let text = menu::format_main_menu_text(balance, positions_count, daily_pnl, settings.sniper_enabled);
    let kb = menu::build_main_menu(balance, positions_count, daily_pnl, settings.sniper_enabled);

    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;
    Ok(())
}

async fn handle_help(bot: Bot, msg: Message) -> Result<(), teloxide::RequestError> {
    let help_text = "\
\u{1f916} <b>RICOZ SNIPER - Help</b>

<b>Commands:</b>
/start - Menu utama
/help - Bantuan
/snipe - Toggle sniper mode
/buy &lt;CA&gt; - Beli token (contract address)
/sell &lt;CA&gt; &lt;pct&gt; - Jual token (persentase)
/positions - Lihat posisi aktif
/wallet - Kelola wallet
/settings - Pengaturan bot
/history - Riwayat trading

<b>Quick Trade:</b>
Paste alamat token Solana langsung ke chat untuk auto-detect dan lihat info token + tombol beli.

<b>Sniper Mode:</b>
Aktifkan sniper untuk auto-buy token baru yang lolos security check dengan skor tinggi.

\u{26a0}\u{fe0f} <i>Trading crypto beresiko tinggi. DYOR!</i>";

    bot.send_message(msg.chat.id, help_text)
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

async fn handle_snipe(
    bot: Bot,
    msg: Message,
    state: Arc<BotState>,
) -> Result<(), teloxide::RequestError> {
    let chat_id = msg.chat.id.0;
    let mut settings = db::get_settings(&state.db, chat_id).unwrap_or_else(|e| {
        warn!("Failed to load settings for snipe toggle: {}", e);
        UserSettings { chat_id, ..Default::default() }
    });
    settings.sniper_enabled = !settings.sniper_enabled;
    if let Err(e) = db::save_settings(&state.db, &settings) {
        warn!("Failed to save settings after snipe toggle: {}", e);
    }

    let (text, kb) = sniper_ui::build_sniper_message(settings.sniper_enabled, &settings);
    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;
    Ok(())
}

async fn handle_buy(
    bot: Bot,
    msg: Message,
    _state: Arc<BotState>,
    ca: String,
) -> Result<(), teloxide::RequestError> {
    if ca.is_empty() {
        bot.send_message(msg.chat.id, "\u{274c} Usage: /buy <contract_address>")
            .await?;
        return Ok(());
    }

    let kb = buy_ui::build_buy_keyboard(&ca);
    bot.send_message(
        msg.chat.id,
        format!(
            "\u{1f4b0} <b>Buy Token</b>\n\n\u{1f4e6} Mint: <code>{}</code>\n\nPilih jumlah SOL:",
            ca
        ),
    )
    .parse_mode(ParseMode::Html)
    .reply_markup(kb)
    .await?;
    Ok(())
}

async fn handle_sell(
    bot: Bot,
    msg: Message,
    _state: Arc<BotState>,
    args: String,
) -> Result<(), teloxide::RequestError> {
    let parts: Vec<&str> = args.trim().split_whitespace().collect();
    if parts.len() < 2 {
        bot.send_message(msg.chat.id, "\u{274c} Usage: /sell <contract_address> <percentage>")
            .await?;
        return Ok(());
    }

    let mint = parts[0];
    let pct = parts[1];

    bot.send_message(
        msg.chat.id,
        format!(
            "\u{1f4e4} <b>Sell Order</b>\n\n\u{1f4e6} Mint: <code>{}</code>\n\u{1f4ca} Sell: {}%\n\u{23f3} Processing...",
            mint, pct
        ),
    )
    .parse_mode(ParseMode::Html)
    .await?;

    // TODO: execute sell via executor module
    Ok(())
}

async fn handle_positions(
    bot: Bot,
    msg: Message,
    state: Arc<BotState>,
) -> Result<(), teloxide::RequestError> {
    handle_positions_cb(&bot, msg.chat.id, &state).await
}

async fn handle_positions_cb(
    bot: &Bot,
    chat_id: ChatId,
    state: &Arc<BotState>,
) -> Result<(), teloxide::RequestError> {
    let positions = db::get_open_positions(&state.db).unwrap_or_else(|e| {
        warn!("Failed to load open positions: {}", e);
        vec![]
    });

    if positions.is_empty() {
        bot.send_message(chat_id, "\u{1f4ad} Tidak ada posisi aktif saat ini.")
            .await?;
        return Ok(());
    }

    let (text, kb) = positions_ui::build_positions_message(&positions);
    bot.send_message(chat_id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;
    Ok(())
}

async fn handle_wallet(
    bot: Bot,
    msg: Message,
    state: Arc<BotState>,
) -> Result<(), teloxide::RequestError> {
    let wallets = load_wallet_infos(&state.db);
    let (text, kb) = wallet_ui::build_wallet_message(&wallets);
    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;
    Ok(())
}

async fn handle_settings(
    bot: Bot,
    msg: Message,
    state: Arc<BotState>,
) -> Result<(), teloxide::RequestError> {
    let chat_id = msg.chat.id.0;
    let settings = db::get_settings(&state.db, chat_id).unwrap_or_else(|e| {
        warn!("Failed to load settings for chat {}: {}", chat_id, e);
        UserSettings { chat_id, ..Default::default() }
    });
    let (text, kb) = settings_ui::build_settings_message(&settings);
    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;
    Ok(())
}

async fn handle_history(
    bot: Bot,
    msg: Message,
    state: Arc<BotState>,
) -> Result<(), teloxide::RequestError> {
    let trades = db::get_recent_trades(&state.db, 20).unwrap_or_else(|e| {
        warn!("Failed to load recent trades: {}", e);
        vec![]
    });
    let text = history_ui::build_history_message(&trades);
    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Load wallets from DB and convert to `WalletInfo` structs for the UI.
fn load_wallet_infos(db_pool: &crate::db::DbPool) -> Vec<wallet_ui::WalletInfo> {
    match db::get_wallets(db_pool) {
        Ok(rows) => rows
            .into_iter()
            .enumerate()
            .map(|(i, (_id, pubkey, _enc_privkey, label, is_active))| {
                wallet_ui::WalletInfo {
                    index: i as u32,
                    pubkey,
                    balance_sol: 0.0, // TODO: fetch real balance via RPC when available
                    is_active,
                    label,
                }
            })
            .collect(),
        Err(e) => {
            warn!("Failed to load wallets: {}", e);
            vec![]
        }
    }
}
