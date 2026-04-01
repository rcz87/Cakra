use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Result from the RugCheck.xyz API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RugCheckResult {
    /// Overall safety score (0..100). Higher is safer.
    pub score: f64,
    /// Individual risk factors identified.
    pub risks: Vec<RiskFactor>,
    /// Raw status string from the API (e.g. "Good", "Warning", "Danger").
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskFactor {
    pub name: String,
    pub description: String,
    pub level: String,
    pub score: f64,
}

/// Raw API response structure from RugCheck.
#[derive(Debug, Deserialize)]
struct RugCheckApiResponse {
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    risks: Option<Vec<RugCheckApiRisk>>,
}

#[derive(Debug, Deserialize)]
struct RugCheckApiRisk {
    name: Option<String>,
    description: Option<String>,
    level: Option<String>,
    score: Option<f64>,
}

/// Query the RugCheck.xyz API for a token report.
///
/// * `mint` - The Solana token mint address.
/// * `api_url` - Base URL for the RugCheck API (e.g. `https://api.rugcheck.xyz/v1`).
pub async fn check_rugcheck(mint: &str, api_url: &str) -> Result<RugCheckResult> {
    let url = format!("{}/tokens/{}/report", api_url.trim_end_matches('/'), mint);

    let client = reqwest::Client::new();
    let response = client
        .get(&url)
        .send()
        .await
        .context("RugCheck API request failed")?;

    if !response.status().is_success() {
        anyhow::bail!(
            "RugCheck API returned status {}",
            response.status()
        );
    }

    let api_resp: RugCheckApiResponse = response
        .json()
        .await
        .context("Failed to parse RugCheck API response")?;

    let score = api_resp.score.unwrap_or(0.0);
    let status = api_resp.status.unwrap_or_else(|| "Unknown".to_string());

    let risks = api_resp
        .risks
        .unwrap_or_default()
        .into_iter()
        .map(|r| RiskFactor {
            name: r.name.unwrap_or_else(|| "Unknown".to_string()),
            description: r.description.unwrap_or_default(),
            level: r.level.unwrap_or_else(|| "unknown".to_string()),
            score: r.score.unwrap_or(0.0),
        })
        .collect();

    let result = RugCheckResult {
        score,
        risks,
        status,
    };

    info!(
        mint = %mint,
        score,
        "RugCheck report fetched"
    );

    Ok(result)
}
