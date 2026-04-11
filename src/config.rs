use anyhow::{Context, Result};
use std::env;
use std::fmt;

/// Detector backend mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectorMode {
    /// Try gRPC first, fall back to WebSocket if unavailable.
    Auto,
    /// Only use Yellowstone gRPC (fail if unavailable).
    Grpc,
    /// Only use WebSocket logsSubscribe + getTransaction.
    WebSocket,
}

impl fmt::Display for DetectorMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DetectorMode::Auto => write!(f, "auto"),
            DetectorMode::Grpc => write!(f, "grpc"),
            DetectorMode::WebSocket => write!(f, "websocket"),
        }
    }
}

/// Trading mode determines TP/SL thresholds, timing, and exit behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradingMode {
    /// PumpFun snipe — buy small, exit fast at +5%. All-or-nothing.
    Snipe,
    /// Fast in/out for new tokens. Tight TP/SL, fast exits.
    Scalp,
    /// Hold for established/liquid tokens. Wide TP tiers, longer holds.
    Hold,
}

impl fmt::Display for TradingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TradingMode::Snipe => write!(f, "snipe"),
            TradingMode::Scalp => write!(f, "scalp"),
            TradingMode::Hold => write!(f, "hold"),
        }
    }
}

/// Mode-specific trading parameters derived from TradingMode.
#[derive(Debug, Clone)]
pub struct TradingProfile {
    pub mode: TradingMode,
    // TP/SL defaults for new positions
    pub take_profit_pct: f64,
    pub stop_loss_pct: f64,
    pub trailing_stop_pct: f64,
    // Trailing stop only activates after this PnL %
    pub trailing_gate_pct: f64,
    // Time stop: exit if position age > time_stop_secs AND PnL < time_stop_min_pnl
    pub time_stop_secs: u64,
    pub time_stop_min_pnl: f64,
    // Max age exit: force sell if age > max_hold AND PnL > max_age_min_pnl
    pub max_hold_secs: u64,
    pub max_age_min_pnl: f64,
    // Intervals
    pub price_poll_secs: u64,
    pub tpsl_check_secs: u64,
}

impl TradingProfile {
    pub fn from_mode(mode: TradingMode, max_hold_override: u64) -> Self {
        match mode {
            TradingMode::Snipe => Self {
                mode,
                take_profit_pct: 5.0,
                stop_loss_pct: 5.0,
                trailing_stop_pct: 3.0,
                trailing_gate_pct: 3.0,
                time_stop_secs: 30,        // 30 detik
                time_stop_min_pnl: 1.0,    // exit jika PnL < 1% setelah 30s
                max_hold_secs: if max_hold_override > 0 { max_hold_override } else { 120 }, // 2 min
                max_age_min_pnl: 2.0,
                price_poll_secs: 1,
                tpsl_check_secs: 1,
            },
            TradingMode::Scalp => Self {
                mode,
                take_profit_pct: 20.0,
                stop_loss_pct: 10.0,
                trailing_stop_pct: 5.0,
                trailing_gate_pct: 10.0,
                time_stop_secs: 120,       // 2 menit
                time_stop_min_pnl: 3.0,    // exit jika PnL < 3% setelah 2 min
                max_hold_secs: if max_hold_override > 0 { max_hold_override } else { 600 }, // 10 min
                max_age_min_pnl: 5.0,
                price_poll_secs: 1,
                tpsl_check_secs: 1,
            },
            TradingMode::Hold => Self {
                mode,
                take_profit_pct: 100.0,
                stop_loss_pct: 50.0,
                trailing_stop_pct: 30.0,
                trailing_gate_pct: 30.0,
                time_stop_secs: 600,       // 10 menit
                time_stop_min_pnl: 10.0,
                max_hold_secs: if max_hold_override > 0 { max_hold_override } else { 14400 }, // 4 jam
                max_age_min_pnl: 10.0,
                price_poll_secs: 3,
                tpsl_check_secs: 3,
            },
        }
    }
}

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
    pub jupiter_api_key: String,

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

    // Position Management
    /// Maximum hold time in seconds. Overrides mode default if > 0.
    pub max_hold_secs: u64,

    // Trading Mode
    /// "scalp" for fast in/out on new tokens, "hold" for liquid tokens.
    pub trading_mode: TradingMode,

    // Detector
    pub detector_mode: DetectorMode,

    // Network
    pub use_devnet: bool,

    // Sprint 3b feature flag — direct Raydium CPMM swap (default OFF for safety)
    pub enable_raydium_direct: bool,

    // Observe-only mode — log "would have bought" but don't spend SOL
    // Used to validate strategy hypothesis without risking capital
    pub observe_only: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        // Validate critical security settings early — fail fast on misconfig.
        let wallet_password = env::var("WALLET_PASSWORD")
            .context("WALLET_PASSWORD is required for wallet encryption")?;
        if wallet_password.len() < 8 {
            anyhow::bail!(
                "WALLET_PASSWORD must be at least 8 characters (got {})",
                wallet_password.len()
            );
        }

        let admin_chat_id: i64 = env::var("TELEGRAM_ADMIN_CHAT_ID")
            .context("TELEGRAM_ADMIN_CHAT_ID is required — set your Telegram chat ID")?
            .parse()
            .context("Invalid TELEGRAM_ADMIN_CHAT_ID (must be integer)")?;
        if admin_chat_id == 0 {
            anyhow::bail!(
                "TELEGRAM_ADMIN_CHAT_ID must be a non-zero integer — \
                 without it, the Telegram bot would accept commands from anyone"
            );
        }

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
            telegram_admin_chat_id: admin_chat_id,

            goplus_api_key: env::var("GOPLUS_API_KEY").unwrap_or_default(),
            rugcheck_api_url: env::var("RUGCHECK_API_URL")
                .unwrap_or_else(|_| "https://api.rugcheck.xyz/v1".to_string()),

            jupiter_api_url: env::var("JUPITER_API_URL")
                .unwrap_or_else(|_| "https://api.jup.ag/swap/v1".to_string()),
            jupiter_api_key: env::var("JUPITER_API_KEY").unwrap_or_default(),

            encryption_salt: env::var("ENCRYPTION_SALT")
                .context("ENCRYPTION_SALT must be set — generate with: openssl rand -base64 32")?,

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

            max_hold_secs: env::var("MAX_HOLD_SECS")
                .unwrap_or_else(|_| "0".to_string())
                .parse()
                .context("Invalid MAX_HOLD_SECS")?,

            trading_mode: match env::var("TRADING_MODE")
                .unwrap_or_else(|_| "hold".to_string())
                .to_lowercase()
                .as_str()
            {
                "snipe" => TradingMode::Snipe,
                "scalp" => TradingMode::Scalp,
                "hold" => TradingMode::Hold,
                other => anyhow::bail!(
                    "Invalid TRADING_MODE '{}': must be 'snipe', 'scalp', or 'hold'",
                    other
                ),
            },

            detector_mode: match env::var("DETECTOR_MODE")
                .unwrap_or_else(|_| "auto".to_string())
                .to_lowercase()
                .as_str()
            {
                "auto" => DetectorMode::Auto,
                "grpc" => DetectorMode::Grpc,
                "websocket" | "ws" => DetectorMode::WebSocket,
                other => anyhow::bail!(
                    "Invalid DETECTOR_MODE '{}': must be 'auto', 'grpc', or 'websocket'",
                    other
                ),
            },

            use_devnet: env::var("USE_DEVNET")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .context("Invalid USE_DEVNET")?,

            enable_raydium_direct: env::var("ENABLE_RAYDIUM_DIRECT")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .context("Invalid ENABLE_RAYDIUM_DIRECT")?,

            observe_only: env::var("OBSERVE_ONLY")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .context("Invalid OBSERVE_ONLY")?,
        })
    }

    pub fn trading_profile(&self) -> TradingProfile {
        TradingProfile::from_mode(self.trading_mode, self.max_hold_secs)
    }

    pub fn effective_rpc_url(&self) -> &str {
        if self.use_devnet {
            "https://api.devnet.solana.com"
        } else {
            &self.solana_rpc_url
        }
    }
}
