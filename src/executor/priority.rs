use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use tracing::{debug, info, warn};

#[derive(Debug, Serialize)]
struct PriorityFeeRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    params: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct PriorityFeeResponse {
    result: Option<Vec<PriorityFeeEntry>>,
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PriorityFeeEntry {
    slot: u64,
    prioritization_fee: u64,
}

/// Calculate a dynamic priority fee based on recent network conditions.
///
/// Queries the `getRecentPrioritizationFees` RPC method and returns
/// the median fee multiplied by the given `multiplier`.
///
/// # Arguments
/// * `rpc` - The Solana RPC client (used for the endpoint URL)
/// * `multiplier` - Multiplier applied to the median fee (e.g. 1.5)
///
/// # Returns
/// The calculated priority fee in micro-lamports.
pub async fn calculate_priority_fee(rpc: &RpcClient, multiplier: f64) -> Result<u64> {
    let rpc_url = rpc.url();

    let request = PriorityFeeRequest {
        jsonrpc: "2.0".to_string(),
        id: 1,
        method: "getRecentPrioritizationFees".to_string(),
        params: vec![],
    };

    let http = reqwest::Client::new();
    let response = http
        .post(&rpc_url)
        .json(&request)
        .send()
        .await
        .context("Failed to query recent prioritization fees")?;

    let fee_response: PriorityFeeResponse = response
        .json()
        .await
        .context("Failed to parse prioritization fee response")?;

    if let Some(err) = fee_response.error {
        warn!(error = ?err, "RPC error when fetching priority fees");
        anyhow::bail!("RPC error: {err:?}");
    }

    let entries = fee_response
        .result
        .context("No result in prioritization fee response")?;

    if entries.is_empty() {
        info!("No recent prioritization fee data, returning default");
        return Ok(5000);
    }

    // Collect non-zero fees and compute median
    let mut fees: Vec<u64> = entries
        .iter()
        .map(|e| e.prioritization_fee)
        .filter(|&f| f > 0)
        .collect();

    if fees.is_empty() {
        info!("All recent fees are zero, returning minimum");
        return Ok(1000);
    }

    fees.sort_unstable();
    let median = fees[fees.len() / 2];
    let adjusted = (median as f64 * multiplier) as u64;

    // Clamp to reasonable bounds: min 1000, max 500_000 micro-lamports
    let clamped = adjusted.clamp(1_000, 500_000);

    debug!(
        sample_count = fees.len(),
        median = median,
        multiplier = multiplier,
        adjusted = adjusted,
        clamped = clamped,
        "Priority fee calculated"
    );

    Ok(clamped)
}

/// Compute priority fee for a specific set of accounts (more targeted).
pub async fn calculate_priority_fee_for_accounts(
    rpc_url: &str,
    accounts: &[String],
    multiplier: f64,
) -> Result<u64> {
    let account_values: Vec<serde_json::Value> = accounts
        .iter()
        .map(|a| serde_json::Value::String(a.clone()))
        .collect();

    let request = PriorityFeeRequest {
        jsonrpc: "2.0".to_string(),
        id: 1,
        method: "getRecentPrioritizationFees".to_string(),
        params: vec![serde_json::Value::Array(account_values)],
    };

    let http = reqwest::Client::new();
    let response = http
        .post(rpc_url)
        .json(&request)
        .send()
        .await
        .context("Failed to query account-specific priority fees")?;

    let fee_response: PriorityFeeResponse = response
        .json()
        .await
        .context("Failed to parse priority fee response")?;

    let entries = fee_response.result.unwrap_or_default();
    let mut fees: Vec<u64> = entries
        .iter()
        .map(|e| e.prioritization_fee)
        .filter(|&f| f > 0)
        .collect();

    if fees.is_empty() {
        return Ok(5000);
    }

    fees.sort_unstable();
    let median = fees[fees.len() / 2];
    let adjusted = ((median as f64) * multiplier) as u64;

    Ok(adjusted.clamp(1_000, 500_000))
}
