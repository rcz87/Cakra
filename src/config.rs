use anyhow::{Context, Result};
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    // Solana RPC
    pub solana_rpc_url: String,
    pub solana_ws_url: String,

    // Yellowstone gRPC
    pub grpc_endpoint: String,
    pub grpc_token: String,

    // Jito
    pub jito_block_engine_url: String,
    pub jito_tip_lamports: u64,

    // Telegram
    pub telegram_bot_token: String,
    pub telegram_admin_chat_id: i64,

    // API Keys
    pub goplus_api_key: String,
    pub rugcheck_api_url: String,

    // Jupiter
    pub jupiter_api_url: String,

    // Security
    pub encryption_salt: String,

    // Database
    pub database_path: String,

    // Risk Management
    pub max_buy_sol: f64,
    pub max_positions: u32,
    pub daily_loss_limit_sol: f64,
    pub default_slippage_bps: u16,
    pub trade_cooldown_secs: u64,
    pub min_score_auto_buy: u8,
    pub min_score_notify: u8,

    // Network
    pub use_devnet: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        Ok(Self {
            solana_rpc_url: env::var("SOLANA_RPC_URL")
                .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string()),
            solana_ws_url: env::var("SOLANA_WS_URL")
                .unwrap_or_else(|_| "wss://api.mainnet-beta.solana.com".to_string()),

            grpc_endpoint: env::var("GRPC_ENDPOINT").unwrap_or_default(),
            grpc_token: env::var("GRPC_TOKEN").unwrap_or_default(),

            jito_block_engine_url: env::var("JITO_BLOCK_ENGINE_URL")
                .unwrap_or_else(|_| "https://mainnet.block-engine.jito.wtf".to_string()),
            jito_tip_lamports: env::var("JITO_TIP_LAMPORTS")
                .unwrap_or_else(|_| "10000".to_string())
                .parse()
                .context("Invalid JITO_TIP_LAMPORTS")?,

            telegram_bot_token: env::var("TELEGRAM_BOT_TOKEN")
                .context("TELEGRAM_BOT_TOKEN is required")?,
            telegram_admin_chat_id: env::var("TELEGRAM_ADMIN_CHAT_ID")
                .unwrap_or_else(|_| "0".to_string())
                .parse()
                .context("Invalid TELEGRAM_ADMIN_CHAT_ID")?,

            goplus_api_key: env::var("GOPLUS_API_KEY").unwrap_or_default(),
            rugcheck_api_url: env::var("RUGCHECK_API_URL")
                .unwrap_or_else(|_| "https://api.rugcheck.xyz/v1".to_string()),

            jupiter_api_url: env::var("JUPITER_API_URL")
                .unwrap_or_else(|_| "https://quote-api.jup.ag/v6".to_string()),

            encryption_salt: env::var("ENCRYPTION_SALT")
                .unwrap_or_else(|_| "default-salt-change-me".to_string()),

            database_path: env::var("DATABASE_PATH")
                .unwrap_or_else(|_| "data/ricoz-sniper.db".to_string()),

            max_buy_sol: env::var("MAX_BUY_SOL")
                .unwrap_or_else(|_| "1.0".to_string())
                .parse()
                .context("Invalid MAX_BUY_SOL")?,
            max_positions: env::var("MAX_POSITIONS")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .context("Invalid MAX_POSITIONS")?,
            daily_loss_limit_sol: env::var("DAILY_LOSS_LIMIT_SOL")
                .unwrap_or_else(|_| "5.0".to_string())
                .parse()
                .context("Invalid DAILY_LOSS_LIMIT_SOL")?,
            default_slippage_bps: env::var("DEFAULT_SLIPPAGE_BPS")
                .unwrap_or_else(|_| "500".to_string())
                .parse()
                .context("Invalid DEFAULT_SLIPPAGE_BPS")?,
            trade_cooldown_secs: env::var("TRADE_COOLDOWN_SECS")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .context("Invalid TRADE_COOLDOWN_SECS")?,
            min_score_auto_buy: env::var("MIN_SCORE_AUTO_BUY")
                .unwrap_or_else(|_| "75".to_string())
                .parse()
                .context("Invalid MIN_SCORE_AUTO_BUY")?,
            min_score_notify: env::var("MIN_SCORE_NOTIFY")
                .unwrap_or_else(|_| "50".to_string())
                .parse()
                .context("Invalid MIN_SCORE_NOTIFY")?,

            use_devnet: env::var("USE_DEVNET")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .context("Invalid USE_DEVNET")?,
        })
    }

    pub fn effective_rpc_url(&self) -> &str {
        if self.use_devnet {
            "https://api.devnet.solana.com"
        } else {
            &self.solana_rpc_url
        }
    }
}
