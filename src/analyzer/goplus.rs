use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

/// Result from the GoPlus Security API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoPlusResult {
    pub is_honeypot: bool,
    pub is_blacklisted: bool,
    pub buy_tax: f64,
    pub sell_tax: f64,
    pub is_open_source: bool,
    pub is_proxy: bool,
    pub is_mintable: bool,
    pub can_take_back_ownership: bool,
    pub owner_change_balance: bool,
    /// Computed safety score 0..100 based on the API flags.
    pub safety_score: f64,
}

/// Raw API response structure from GoPlus Labs.
#[derive(Debug, Deserialize)]
struct GoPlusApiResponse {
    code: i32,
    result: Option<std::collections::HashMap<String, GoPlusTokenData>>,
}

#[derive(Debug, Deserialize)]
struct GoPlusTokenData {
    is_honeypot: Option<String>,
    is_blacklisted: Option<String>,
    buy_tax: Option<String>,
    sell_tax: Option<String>,
    is_open_source: Option<String>,
    is_proxy: Option<String>,
    is_mintable: Option<String>,
    can_take_back_ownership: Option<String>,
    owner_change_balance: Option<String>,
}

/// Query the GoPlus Security API for the given Solana token mint.
pub async fn check_goplus(mint: &str, api_key: &str) -> Result<GoPlusResult> {
    let url = format!(
        "https://api.gopluslabs.io/api/v1/solana/token_security/{}",
        mint
    );

    let client = reqwest::Client::new();
    let mut request = client.get(&url);

    if !api_key.is_empty() {
        request = request.header("Authorization", format!("Bearer {}", api_key));
    }

    let response = request
        .send()
        .await
        .context("GoPlus API request failed")?;

    if !response.status().is_success() {
        anyhow::bail!(
            "GoPlus API returned status {}",
            response.status()
        );
    }

    let api_resp: GoPlusApiResponse = response
        .json()
        .await
        .context("Failed to parse GoPlus API response")?;

    if api_resp.code != 1 {
        anyhow::bail!("GoPlus API returned error code {}", api_resp.code);
    }

    let data = api_resp
        .result
        .and_then(|mut m| {
            // The key is the lowercase mint address
            m.remove(&mint.to_lowercase())
                .or_else(|| m.remove(mint))
                .or_else(|| m.into_values().next())
        })
        .context("No token data found in GoPlus response")?;

    let is_honeypot = parse_bool_flag(&data.is_honeypot);
    let is_blacklisted = parse_bool_flag(&data.is_blacklisted);
    let buy_tax = parse_f64_field(&data.buy_tax);
    let sell_tax = parse_f64_field(&data.sell_tax);
    let is_open_source = parse_bool_flag(&data.is_open_source);
    let is_proxy = parse_bool_flag(&data.is_proxy);
    let is_mintable = parse_bool_flag(&data.is_mintable);
    let can_take_back_ownership = parse_bool_flag(&data.can_take_back_ownership);
    let owner_change_balance = parse_bool_flag(&data.owner_change_balance);

    // Compute a safety score: start at 100, deduct for red flags.
    let mut score: f64 = 100.0;
    if is_honeypot {
        score -= 100.0;
    }
    if is_blacklisted {
        score -= 30.0;
    }
    if buy_tax > 10.0 {
        score -= 20.0;
    }
    if sell_tax > 10.0 {
        score -= 20.0;
    }
    if is_proxy {
        score -= 10.0;
    }
    if is_mintable {
        score -= 15.0;
    }
    if can_take_back_ownership {
        score -= 20.0;
    }
    if owner_change_balance {
        score -= 15.0;
    }
    let safety_score = score.max(0.0);

    let result = GoPlusResult {
        is_honeypot,
        is_blacklisted,
        buy_tax,
        sell_tax,
        is_open_source,
        is_proxy,
        is_mintable,
        can_take_back_ownership,
        owner_change_balance,
        safety_score,
    };

    info!(
        mint = %mint,
        safety_score,
        is_honeypot,
        "GoPlus security check complete"
    );

    Ok(result)
}

fn parse_bool_flag(val: &Option<String>) -> bool {
    val.as_deref()
        .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn parse_f64_field(val: &Option<String>) -> f64 {
    val.as_deref()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| v * 100.0) // GoPlus returns tax as decimal (0.05 = 5%)
        .unwrap_or(0.0)
}
