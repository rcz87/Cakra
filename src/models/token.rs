use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TokenSource {
    PumpFun,
    Raydium,
    PumpSwap,
    Unknown,
}

impl std::fmt::Display for TokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenSource::PumpFun => write!(f, "Pump.fun"),
            TokenSource::Raydium => write!(f, "Raydium"),
            TokenSource::PumpSwap => write!(f, "PumpSwap"),
            TokenSource::Unknown => write!(f, "Unknown"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    pub mint: String,
    pub name: String,
    pub symbol: String,
    pub source: TokenSource,
    pub creator: String,
    pub initial_liquidity_sol: f64,
    pub initial_liquidity_usd: f64,
    pub pool_address: Option<String>,
    pub metadata_uri: Option<String>,
    pub decimals: u8,
    pub detected_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SecurityAnalysis {
    pub mint_renounced: bool,
    pub freeze_authority_null: bool,
    pub metadata_immutable: bool,
    pub lp_status: LpStatus,
    pub honeypot_result: HoneypotResult,
    pub goplus_score: Option<f64>,
    pub rugcheck_score: Option<f64>,
    pub creator_history: CreatorHistory,
    pub social_links: SocialLinks,
    pub final_score: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum LpStatus {
    Burned,
    Locked,
    #[default]
    Unknown,
    NotBurned,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum HoneypotResult {
    Safe { buy_tax: f64, sell_tax: f64 },
    HighTax { buy_tax: f64, sell_tax: f64 },
    Honeypot,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum CreatorHistory {
    Clean { tokens_created: u32 },
    Suspicious { tokens_created: u32, rugs: u32 },
    Rugger { tokens_created: u32, rugs: u32 },
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SocialLinks {
    pub website: Option<String>,
    pub twitter: Option<String>,
    pub telegram: Option<String>,
}

impl SocialLinks {
    pub fn count(&self) -> u8 {
        let mut c = 0;
        if self.website.is_some() { c += 1; }
        if self.twitter.is_some() { c += 1; }
        if self.telegram.is_some() { c += 1; }
        c
    }
}
