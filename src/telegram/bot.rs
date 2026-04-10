use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};
use teloxide::utils::command::BotCommands;
use tracing::{info, warn, error};

use super::{BotState, PendingAction};
use super::{buy_ui, history_ui, menu, positions_ui, settings_ui, sniper_ui, wallet_ui};
use crate::db::queries as db;
use crate::models::UserSettings;
use crate::models::token::{DetectionBackend, TokenInfo, TokenSource};

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
    #[command(description = "Performance stats — winrate & PnL")]
    Stats,
    #[command(description = "Emergency stop — pause auto-buy")]
    Stop,
    #[command(description = "Resume auto-buy after /stop")]
    Go,
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
        Command::Stats => handle_stats(bot, msg, state).await?,
        Command::Stop => handle_stop(bot, msg, state).await?,
        Command::Go => handle_go(bot, msg, state).await?,
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
    } else if data == "help" {
        let help_text = "\
\u{2753} <b>RICOZ SNIPER \u{2014} Help</b>\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
\u{1f4b0} <b>Buy Token</b>\n\
Paste alamat token langsung ke chat,\natau klik tombol Buy Token.\n\n\
\u{1f4e4} <b>Sell Token</b>\n\
Klik Sell dari menu Positions untuk\njual sebagian atau semua posisi.\n\n\
\u{1f3af} <b>Sniper Mode</b>\n\
Otomatis beli token baru yang lolos\nsecurity check (butuh gRPC).\n\n\
\u{1f45b} <b>Wallet</b>\n\
Generate wallet baru atau import\nexisting private key.\n\n\
\u{2699}\u{fe0f} <b>Settings</b>\n\
Atur TP/SL, slippage, max buy,\ndan parameter lainnya.\n\n\
\u{26a0}\u{fe0f} <i>Trading crypto beresiko tinggi. DYOR!</i>";
        let kb = InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("\u{2b05}\u{fe0f} Kembali", "menu")],
        ]);
        bot.send_message(chat_id, help_text)
            .parse_mode(ParseMode::Html)
            .reply_markup(kb)
            .await?;
    } else if data == "buy_prompt" {
        let text = "\
\u{1f4b0} <b>Buy Token</b>\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
\u{1f4cb} <b>Cara beli:</b>\n\n\
1\u{fe0f}\u{20e3} Paste alamat token (contract address)\n   langsung ke chat ini\n\n\
2\u{fe0f}\u{20e3} Atau ketik:\n   <code>/buy TOKEN_ADDRESS</code>\n\n\
\u{1f4a1} <i>Contoh:</i>\n\
<code>/buy So11111111111111111111111111111111111111112</code>";
        let kb = InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("\u{2b05}\u{fe0f} Kembali", "menu")],
        ]);
        bot.send_message(chat_id, text)
            .parse_mode(ParseMode::Html)
            .reply_markup(kb)
            .await?;
    } else if data == "sell_prompt" {
        // Show positions with sell buttons, or message if no positions
        let positions = db::get_open_positions(&state.db).unwrap_or_default();
        if positions.is_empty() {
            let kb = InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback("\u{2b05}\u{fe0f} Kembali", "menu")],
            ]);
            bot.send_message(chat_id, "\u{1f4e4} <b>Sell Token</b>\n\n\u{26a0}\u{fe0f} Tidak ada posisi aktif untuk dijual.")
                .parse_mode(ParseMode::Html)
                .reply_markup(kb)
                .await?;
        } else {
            let (text, kb) = positions_ui::build_positions_message(&positions);
            bot.send_message(chat_id, text)
                .parse_mode(ParseMode::Html)
                .reply_markup(kb)
                .await?;
        }
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
                let mint = parts[1].to_string();

                // Validate amount
                if amount <= 0.0 || amount > state.config.max_buy_sol {
                    bot.send_message(
                        chat_id,
                        format!("\u{274c} Amount harus 0 < x <= {} SOL", state.config.max_buy_sol),
                    ).await?;
                } else {
                    // Get active wallet
                    let keypair = match state.wallet_manager.get_active_wallet() {
                        Ok(Some(active)) => {
                            match state.wallet_manager.get_keypair(&active.pubkey, &state.wallet_password) {
                                Ok(kp) => Some(kp),
                                Err(e) => {
                                    bot.send_message(chat_id, format!("\u{274c} Gagal decrypt wallet: {}", e)).await?;
                                    None
                                }
                            }
                        }
                        Ok(None) => {
                            bot.send_message(chat_id, "\u{274c} Belum ada wallet aktif. Buat wallet dulu via \u{1f45b} Wallet.").await?;
                            None
                        }
                        Err(e) => {
                            bot.send_message(chat_id, format!("\u{274c} Error wallet: {}", e)).await?;
                            None
                        }
                    };

                    if let Some(keypair) = keypair {
                        let short_mint = if mint.len() > 10 {
                            format!("{}...{}", &mint[..6], &mint[mint.len()-4..])
                        } else {
                            mint.clone()
                        };

                        // Send "processing" message
                        bot.send_message(
                            chat_id,
                            format!(
                                "\u{23f3} <b>Buying...</b>\n\n\
                                 \u{1f4e6} Token: <code>{}</code>\n\
                                 \u{1f4b0} Amount: {} SOL\n\n\
                                 <i>Submitting via Jito bundle...</i>",
                                short_mint, amount
                            ),
                        )
                        .parse_mode(ParseMode::Html)
                        .await?;

                        // Build minimal TokenInfo for manual buy
                        let token = TokenInfo {
                            mint: mint.clone(),
                            name: "Unknown".to_string(),
                            symbol: short_mint.clone(),
                            source: TokenSource::Unknown,
                            creator: String::new(),
                            initial_liquidity_sol: 0.0,
                            initial_liquidity_usd: 0.0,
                            pool_address: None,
                            metadata_uri: None,
                            decimals: 9,
                            detected_at: Utc::now(),
                            backend: DetectionBackend::Helius,
                            market_cap_sol: 0.0,
                            v_sol_in_bonding_curve: 0.0,
                            initial_buy_sol: 0.0,
                        };

                        let slippage = state.config.default_slippage_bps;

                        // Execute buy
                        match state.executor.execute_buy(&token, amount, slippage, &keypair).await {
                            Ok(sig) => {
                                info!(
                                    mint = %mint,
                                    signature = %sig,
                                    amount_sol = amount,
                                    "Telegram manual buy executed"
                                );
                                let kb = InlineKeyboardMarkup::new(vec![
                                    vec![
                                        InlineKeyboardButton::callback("\u{1f4c2} Positions", "positions"),
                                        InlineKeyboardButton::callback("\u{2b05}\u{fe0f} Menu", "menu"),
                                    ],
                                ]);
                                bot.send_message(
                                    chat_id,
                                    format!(
                                        "\u{2705} <b>Buy Berhasil!</b>\n\
                                         \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
                                         \u{1f4e6} Token: <code>{}</code>\n\
                                         \u{1f4b0} Amount: {} SOL\n\
                                         \u{1f4dd} Tx: <code>{}</code>\n\n\
                                         <i>Posisi dibuka. TP/SL aktif.</i>",
                                        mint, amount, sig
                                    ),
                                )
                                .parse_mode(ParseMode::Html)
                                .reply_markup(kb)
                                .await?;
                            }
                            Err(e) => {
                                error!(
                                    mint = %mint,
                                    error = %e,
                                    "Telegram manual buy failed"
                                );
                                let kb = InlineKeyboardMarkup::new(vec![
                                    vec![
                                        InlineKeyboardButton::callback("\u{1f504} Coba Lagi", &format!("buy_select:{}", mint)),
                                        InlineKeyboardButton::callback("\u{2b05}\u{fe0f} Menu", "menu"),
                                    ],
                                ]);
                                bot.send_message(
                                    chat_id,
                                    format!(
                                        "\u{274c} <b>Buy Gagal</b>\n\n\
                                         \u{1f4e6} Token: <code>{}</code>\n\
                                         \u{26a0}\u{fe0f} Error: {}\n\n\
                                         <i>Cek wallet balance dan coba lagi.</i>",
                                        short_mint, e
                                    ),
                                )
                                .parse_mode(ParseMode::Html)
                                .reply_markup(kb)
                                .await?;
                            }
                        }
                    }
                }
            }
        }
    } else if let Some(rest) = data.strip_prefix("sell:") {
        // Format: sell:<pct>:<mint>
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() == 2 {
            if let Ok(pct) = parts[0].parse::<u8>() {
                let mint = parts[1].to_string();
                bot.send_message(
                    chat_id,
                    format!("\u{1f4e4} Sell {}% of `{}`\n\u{23f3} Processing...", pct, &mint),
                )
                .await?;
                if let Err(e) = state.sell_tx.send((mint, pct)).await {
                    warn!("Failed to dispatch sell command: {}", e);
                    bot.send_message(chat_id, "\u{274c} Failed to submit sell order.").await?;
                }
            }
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
    } else if data == "set:slippage" {
        state.pending_actions.lock().await.insert(chat_id.0, PendingAction::EditSlippage);
        let kb = InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("\u{274c} Batal", "settings")],
        ]);
        bot.send_message(
            chat_id,
            "\u{1f4b0} <b>Edit Slippage</b>\n\n\
             Masukkan nilai slippage dalam <b>persen</b>.\n\n\
             \u{1f4a1} Contoh: <code>5</code> = 5% (500 bps)\n\
             \u{26a0}\u{fe0f} Range: <b>0.1% - 50%</b>",
        )
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;
    } else if data == "set:tp" {
        state.pending_actions.lock().await.insert(chat_id.0, PendingAction::EditTakeProfit);
        let kb = InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("\u{274c} Batal", "settings")],
        ]);
        bot.send_message(
            chat_id,
            "\u{2705} <b>Edit Take Profit</b>\n\n\
             Masukkan target profit dalam <b>persen</b>.\n\n\
             \u{1f4a1} Contoh: <code>100</code> = jual saat +100%\n\
             \u{26a0}\u{fe0f} Range: <b>1% - 10000%</b>",
        )
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;
    } else if data == "set:sl" {
        state.pending_actions.lock().await.insert(chat_id.0, PendingAction::EditStopLoss);
        let kb = InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("\u{274c} Batal", "settings")],
        ]);
        bot.send_message(
            chat_id,
            "\u{274c} <b>Edit Stop Loss</b>\n\n\
             Masukkan batas loss dalam <b>persen</b>.\n\n\
             \u{1f4a1} Contoh: <code>30</code> = cut loss saat -30%\n\
             \u{26a0}\u{fe0f} Range: <b>1% - 99%</b>",
        )
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;
    } else if data == "set:trailing" {
        state.pending_actions.lock().await.insert(chat_id.0, PendingAction::EditTrailingStop);
        let kb = InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("\u{274c} Batal", "settings")],
        ]);
        bot.send_message(
            chat_id,
            "\u{1f4c9} <b>Edit Trailing Stop</b>\n\n\
             Masukkan trailing stop dalam <b>persen</b>.\n\
             Ketik <code>0</code> untuk matikan.\n\n\
             \u{1f4a1} Contoh: <code>20</code> = jual saat turun 20% dari harga tertinggi\n\
             \u{26a0}\u{fe0f} Range: <b>0 (off) atau 5% - 80%</b>",
        )
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;
    } else if data == "set:auto_buy" {
        state.pending_actions.lock().await.insert(chat_id.0, PendingAction::EditAutoBuyAmount);
        let kb = InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("\u{274c} Batal", "settings")],
        ]);
        bot.send_message(
            chat_id,
            "\u{1f4b5} <b>Edit Auto-Buy Amount</b>\n\n\
             Masukkan jumlah SOL per auto-buy.\n\n\
             \u{1f4a1} Contoh: <code>0.1</code> = 0.1 SOL per trade\n\
             \u{26a0}\u{fe0f} Range: <b>0.001 - 10 SOL</b>",
        )
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;
    } else if data == "set:min_score" {
        state.pending_actions.lock().await.insert(chat_id.0, PendingAction::EditMinScore);
        let kb = InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("\u{274c} Batal", "settings")],
        ]);
        bot.send_message(
            chat_id,
            "\u{1f6e1}\u{fe0f} <b>Edit Min Score Auto-Buy</b>\n\n\
             Masukkan skor minimum (0-100) untuk auto-buy.\n\n\
             \u{1f4a1} Contoh: <code>75</code> = beli token skor >= 75\n\
             \u{26a0}\u{fe0f} Range: <b>1 - 100</b>",
        )
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;
    } else if data == "set:notif_tokens" {
        let mut settings = db::get_settings(&state.db, chat_id.0).unwrap_or_else(|_| {
            UserSettings { chat_id: chat_id.0, ..Default::default() }
        });
        settings.notify_new_tokens = !settings.notify_new_tokens;
        let _ = db::save_settings(&state.db, &settings);
        let status = if settings.notify_new_tokens { "\u{1f7e2} ON" } else { "\u{1f534} OFF" };
        bot.send_message(chat_id, format!("\u{2705} Notifikasi token baru: <b>{}</b>", status))
            .parse_mode(ParseMode::Html)
            .await?;
        // Re-show settings
        let (text, kb) = settings_ui::build_settings_message(&settings);
        bot.send_message(chat_id, text).parse_mode(ParseMode::Html).reply_markup(kb).await?;
    } else if data == "set:notif_trades" {
        let mut settings = db::get_settings(&state.db, chat_id.0).unwrap_or_else(|_| {
            UserSettings { chat_id: chat_id.0, ..Default::default() }
        });
        settings.notify_trades = !settings.notify_trades;
        let _ = db::save_settings(&state.db, &settings);
        let status = if settings.notify_trades { "\u{1f7e2} ON" } else { "\u{1f534} OFF" };
        bot.send_message(chat_id, format!("\u{2705} Notifikasi trades: <b>{}</b>", status))
            .parse_mode(ParseMode::Html)
            .await?;
        let (text, kb) = settings_ui::build_settings_message(&settings);
        bot.send_message(chat_id, text).parse_mode(ParseMode::Html).reply_markup(kb).await?;
    } else if data == "set:notif_pnl" {
        let mut settings = db::get_settings(&state.db, chat_id.0).unwrap_or_else(|_| {
            UserSettings { chat_id: chat_id.0, ..Default::default() }
        });
        settings.notify_pnl = !settings.notify_pnl;
        let _ = db::save_settings(&state.db, &settings);
        let status = if settings.notify_pnl { "\u{1f7e2} ON" } else { "\u{1f534} OFF" };
        bot.send_message(chat_id, format!("\u{2705} Notifikasi PnL: <b>{}</b>", status))
            .parse_mode(ParseMode::Html)
            .await?;
        let (text, kb) = settings_ui::build_settings_message(&settings);
        bot.send_message(chat_id, text).parse_mode(ParseMode::Html).reply_markup(kb).await?;
    } else if data == "wallet" {
        let wallets = load_wallet_infos(&state);
        let (text, kb) = wallet_ui::build_wallet_message(&wallets);
        bot.send_message(chat_id, text)
            .parse_mode(ParseMode::Html)
            .reply_markup(kb)
            .await?;
    } else if data == "wallet:generate" {
        bot.send_message(chat_id, "\u{23f3} Generating wallet...")
            .await?;
        match state.wallet_manager.generate_wallet(&state.wallet_password, Some("Sniper Wallet")) {
            Ok(pubkey) => {
                // Auto-set as active if first wallet
                if let Ok(wallets) = state.wallet_manager.list_wallets() {
                    if wallets.len() == 1 {
                        if let Some(w) = wallets.first() {
                            let _ = state.wallet_manager.set_active(w.id);
                        }
                    }
                }
                let text = format!(
                    "\u{2705} <b>Wallet Created!</b>\n\
                     \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
                     \u{1f4cb} <b>Address:</b>\n<code>{}</code>\n\n\
                     \u{1f4b0} <b>Balance:</b> 0.0000 SOL\n\n\
                     \u{26a0}\u{fe0f} <i>Transfer SOL ke alamat di atas untuk\nmulai trading.</i>\n\n\
                     \u{1f4a1} <i>Tap address untuk copy.</i>",
                    pubkey
                );
                let kb = InlineKeyboardMarkup::new(vec![
                    vec![InlineKeyboardButton::callback("\u{1f45b} Wallet", "wallet")],
                    vec![InlineKeyboardButton::callback("\u{2b05}\u{fe0f} Menu Utama", "menu")],
                ]);
                bot.send_message(chat_id, text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(kb)
                    .await?;
            }
            Err(e) => {
                bot.send_message(chat_id, format!("\u{274c} Gagal buat wallet: {}", e))
                    .await?;
            }
        }
    } else if data == "wallet:import" {
        state.pending_actions.lock().await.insert(chat_id.0, PendingAction::ImportWallet);
        let text = "\
\u{1f4e5} <b>Import Wallet</b>\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
Kirim private key (base58) ke chat ini.\n\n\
\u{26a0}\u{fe0f} <i>Private key akan dienkripsi dan\ntersimpan aman di database.</i>\n\n\
\u{1f4a1} <i>Format: string base58 (64 bytes)</i>";
        let kb = InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("\u{274c} Batal", "wallet")],
        ]);
        bot.send_message(chat_id, text)
            .parse_mode(ParseMode::Html)
            .reply_markup(kb)
            .await?;
    } else if data == "wallet:show_address" {
        match state.wallet_manager.get_active_wallet() {
            Ok(Some(w)) => {
                let text = format!(
                    "\u{1f4cb} <b>Deposit Address</b>\n\
                     \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
                     <code>{}</code>\n\n\
                     \u{1f4a1} <i>Tap untuk copy. Transfer SOL ke\nalamat ini untuk mulai trading.</i>",
                    w.pubkey
                );
                let kb = InlineKeyboardMarkup::new(vec![
                    vec![InlineKeyboardButton::callback("\u{2b05}\u{fe0f} Wallet", "wallet")],
                ]);
                bot.send_message(chat_id, text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(kb)
                    .await?;
            }
            _ => {
                bot.send_message(chat_id, "\u{26a0}\u{fe0f} Belum ada wallet aktif.")
                    .await?;
            }
        }
    } else if data == "wallet:switch" {
        if let Ok(wallets) = state.wallet_manager.list_wallets() {
            let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
            for w in &wallets {
                let label = w.label.as_deref().unwrap_or("Wallet");
                let short = if w.pubkey.len() > 10 {
                    format!("{}...{}", &w.pubkey[..4], &w.pubkey[w.pubkey.len()-4..])
                } else {
                    w.pubkey.clone()
                };
                let active = if w.is_active { " \u{2705}" } else { "" };
                rows.push(vec![InlineKeyboardButton::callback(
                    format!("{} {}{}", label, short, active),
                    format!("wallet:activate:{}", w.id),
                )]);
            }
            rows.push(vec![InlineKeyboardButton::callback("\u{2b05}\u{fe0f} Kembali", "wallet")]);
            let kb = InlineKeyboardMarkup::new(rows);
            bot.send_message(chat_id, "\u{1f504} <b>Pilih wallet untuk diaktifkan:</b>")
                .parse_mode(ParseMode::Html)
                .reply_markup(kb)
                .await?;
        }
    } else if let Some(id_str) = data.strip_prefix("wallet:activate:") {
        if let Ok(id) = id_str.parse::<i64>() {
            match state.wallet_manager.set_active(id) {
                Ok(()) => {
                    bot.send_message(chat_id, "\u{2705} Wallet diaktifkan!")
                        .await?;
                    // Show wallet menu again
                    let wallets = load_wallet_infos(&state);
                    let (text, kb) = wallet_ui::build_wallet_message(&wallets);
                    bot.send_message(chat_id, text)
                        .parse_mode(ParseMode::Html)
                        .reply_markup(kb)
                        .await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("\u{274c} Gagal switch wallet: {}", e))
                        .await?;
                }
            }
        }
    } else if data == "wallet:delete" {
        bot.send_message(
            chat_id,
            "\u{26a0}\u{fe0f} <b>Hapus wallet?</b>\n\nKetik <code>/delete_wallet ID</code> untuk hapus.\nCek ID wallet di menu Wallet.",
        )
        .parse_mode(ParseMode::Html)
        .await?;
    } else if data == "wallet:withdraw" {
        bot.send_message(
            chat_id,
            "\u{1f4b8} <b>Withdraw SOL</b>\n\n\u{1f6a7} <i>Fitur withdraw sedang dalam pengembangan.</i>",
        )
        .parse_mode(ParseMode::Html)
        .await?;
    } else if data == "sell_all" {
        let positions = db::get_open_positions(&state.db).unwrap_or_default();
        if positions.is_empty() {
            bot.send_message(chat_id, "\u{26a0}\u{fe0f} Tidak ada posisi untuk dijual.").await?;
        } else {
            for pos in &positions {
                let _ = state.sell_tx.send((pos.token_mint.clone(), 100)).await;
            }
            bot.send_message(
                chat_id,
                format!("\u{1f6a8} Sell ALL {} posisi submitted!", positions.len()),
            ).await?;
        }
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
/stats - Performance stats (winrate, PnL, source breakdown)
/stop - Pause auto-buy
/go - Resume auto-buy

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
    state: Arc<BotState>,
    args: String,
) -> Result<(), teloxide::RequestError> {
    let parts: Vec<&str> = args.trim().split_whitespace().collect();
    if parts.len() < 2 {
        bot.send_message(msg.chat.id, "\u{274c} Usage: /sell <contract_address> <percentage>")
            .await?;
        return Ok(());
    }

    let mint = parts[0];
    let pct_str = parts[1];

    let pct: u8 = match pct_str.parse() {
        Ok(v) if v >= 1 && v <= 100 => v,
        _ => {
            bot.send_message(msg.chat.id, "\u{274c} Percentage must be 1-100").await?;
            return Ok(());
        }
    };

    bot.send_message(
        msg.chat.id,
        format!(
            "\u{1f4e4} <b>Sell Order</b>\n\n\u{1f4e6} Mint: <code>{}</code>\n\u{1f4ca} Sell: {}%\n\u{23f3} Processing...",
            mint, pct
        ),
    )
    .parse_mode(ParseMode::Html)
    .await?;

    if let Err(e) = state.sell_tx.send((mint.to_string(), pct)).await {
        warn!("Failed to dispatch sell command: {}", e);
        bot.send_message(msg.chat.id, "\u{274c} Failed to submit sell order.").await?;
    }

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
    let wallets = load_wallet_infos(&state);
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

async fn handle_stats(
    bot: Bot,
    msg: Message,
    state: Arc<BotState>,
) -> Result<(), teloxide::RequestError> {
    let stats = match db::get_performance_stats(&state.db) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to load performance stats: {}", e);
            let _ = bot
                .send_message(msg.chat.id, format!("\u{26a0}\u{fe0f} Stats query failed: {}", e))
                .await;
            return Ok(());
        }
    };

    let closed_total = stats.closed_tp + stats.closed_sl + stats.closed_manual + stats.closed_error;
    let winrate_pct = if (stats.winners + stats.losers) > 0 {
        stats.winners as f64 / (stats.winners + stats.losers) as f64 * 100.0
    } else {
        0.0
    };

    let mut text = String::new();
    text.push_str("\u{1f4ca} <b>Performance Stats</b>\n");
    text.push_str("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n");

    // Position counts
    text.push_str(&format!(
        "\u{1f4e6} <b>Positions:</b> {} total\n",
        stats.total_positions
    ));
    text.push_str(&format!(
        "  \u{1f7e2} Open: {} | TP: {} | SL: {}\n",
        stats.open, stats.closed_tp, stats.closed_sl
    ));
    text.push_str(&format!(
        "  \u{1f6ab} Manual: {} | Error: {}\n\n",
        stats.closed_manual, stats.closed_error
    ));

    // PnL
    let pnl_emoji = if stats.total_pnl_sol >= 0.0 { "\u{1f4c8}" } else { "\u{1f4c9}" };
    text.push_str(&format!("{} <b>PnL:</b>\n", pnl_emoji));
    text.push_str(&format!(
        "  Total: {:+.6} SOL\n",
        stats.total_pnl_sol
    ));
    text.push_str(&format!(
        "  Closed: {} | Winners: {} | Losers: {}\n",
        closed_total, stats.winners, stats.losers
    ));
    text.push_str(&format!(
        "  Winrate: <b>{:.1}%</b>\n",
        winrate_pct
    ));
    if stats.winners > 0 {
        text.push_str(&format!(
            "  Avg win: {:+.6} SOL | Best: {:+.6}\n",
            stats.avg_win_sol, stats.best_win_sol
        ));
    }
    if stats.losers > 0 {
        text.push_str(&format!(
            "  Avg loss: {:+.6} SOL | Worst: {:+.6}\n",
            stats.avg_loss_sol, stats.worst_loss_sol
        ));
    }
    text.push('\n');

    // Source breakdown
    if !stats.by_source.is_empty() {
        text.push_str("\u{1f30d} <b>By source:</b>\n");
        for (src, count, pnl) in &stats.by_source {
            text.push_str(&format!(
                "  {}: {} trades, {:+.6} SOL\n",
                src, count, pnl
            ));
        }
        text.push('\n');
    }

    text.push_str(&format!(
        "\u{23f1}\u{fe0f} <b>24h activity:</b> {} trades\n",
        stats.trades_24h
    ));

    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Load wallets from WalletManager and convert to UI structs.
fn load_wallet_infos(state: &BotState) -> Vec<wallet_ui::WalletInfo> {
    match state.wallet_manager.list_wallets() {
        Ok(wallets) => wallets
            .into_iter()
            .enumerate()
            .map(|(i, w)| {
                let balance = state.wallet_manager.get_balance(&w.pubkey).unwrap_or(0.0);
                wallet_ui::WalletInfo {
                    index: i as u32,
                    pubkey: w.pubkey,
                    balance_sol: balance,
                    is_active: w.is_active,
                    label: w.label,
                }
            })
            .collect(),
        Err(e) => {
            warn!("Failed to load wallets: {}", e);
            vec![]
        }
    }
}

async fn handle_stop(
    bot: Bot,
    msg: Message,
    state: Arc<BotState>,
) -> Result<(), teloxide::RequestError> {
    use std::sync::atomic::Ordering;
    state.trading_active.store(false, Ordering::Relaxed);
    info!("Kill switch ACTIVATED via /stop");
    bot.send_message(
        msg.chat.id,
        "\u{1f6d1} <b>Auto-buy PAUSED</b>\n\n\
         Bot masih mendeteksi dan menganalisis token,\n\
         tapi TIDAK akan auto-buy.\n\n\
         Gunakan /go untuk resume.",
    )
    .parse_mode(ParseMode::Html)
    .await?;
    Ok(())
}

async fn handle_go(
    bot: Bot,
    msg: Message,
    state: Arc<BotState>,
) -> Result<(), teloxide::RequestError> {
    use std::sync::atomic::Ordering;
    state.trading_active.store(true, Ordering::Relaxed);
    info!("Kill switch DEACTIVATED via /go");
    bot.send_message(
        msg.chat.id,
        "\u{2705} <b>Auto-buy RESUMED</b>\n\n\
         Bot sekarang akan auto-buy token yang memenuhi score threshold.\n\n\
         Gunakan /stop untuk pause kapan saja.",
    )
    .parse_mode(ParseMode::Html)
    .await?;
    Ok(())
}
