pub mod alerts;
pub mod bot;
pub mod buy_ui;
pub mod history_ui;
pub mod menu;
pub mod paste;
pub mod positions_ui;
pub mod settings_ui;
pub mod sniper_ui;
pub mod wallet_ui;

pub use bot::Command;

use anyhow::Result;
use teloxide::prelude::*;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::db::DbPool;

/// Shared state passed to all handlers.
#[derive(Clone)]
pub struct BotState {
    pub config: Config,
    pub db: DbPool,
    /// Channel to dispatch sell commands (mint, sell_pct) to the sell executor.
    pub sell_tx: mpsc::Sender<(String, u8)>,
}

pub struct TelegramBot;

impl TelegramBot {
    /// Start the Telegram bot, register all handlers, and begin polling.
    pub async fn start(config: Config, db: DbPool, sell_tx: mpsc::Sender<(String, u8)>) -> Result<()> {
        tracing::info!("Starting RICOZ SNIPER Telegram bot...");

        let bot = Bot::new(&config.telegram_bot_token);
        let state = Arc::new(BotState {
            config: config.clone(),
            db,
            sell_tx,
        });

        let handler = dptree::entry()
            .branch(
                Update::filter_message()
                    .branch(
                        dptree::entry()
                            .filter_command::<Command>()
                            .endpoint(bot::handle_command),
                    )
                    .branch(dptree::endpoint(paste::handle_message)),
            )
            .branch(
                Update::filter_callback_query()
                    .endpoint(bot::handle_callback),
            );

        Dispatcher::builder(bot, handler)
            .dependencies(dptree::deps![state])
            .enable_ctrlc_handler()
            .build()
            .dispatch()
            .await;

        Ok(())
    }
}
