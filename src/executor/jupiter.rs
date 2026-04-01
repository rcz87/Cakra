use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::VersionedTransaction;
use tracing::{debug, error, info};

// ── Swap API types (used by PriceFeed + quote-only flows) ───────────

/// Response from Jupiter Swap API /quote endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponse {
    pub input_mint: String,
    pub in_amount: String,
    pub output_mint: String,
    pub out_amount: String,
    pub other_amount_threshold: String,
    pub swap_mode: String,
    pub slippage_bps: u16,
    pub price_impact_pct: String,
    pub route_plan: Vec<RoutePlan>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutePlan {
    pub swap_info: SwapInfo,
    pub percent: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapInfo {
    pub amm_key: String,
    pub label: Option<String>,
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: String,
    pub out_amount: String,
    pub fee_amount: String,
    pub fee_mint: String,
}

// ── Ultra API types ─────────────────────────────────────────────────

/// Response from Ultra API GET /ultra/v1/order.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UltraOrderResponse {
    pub request_id: String,
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: String,
    pub out_amount: String,
    pub other_amount_threshold: Option<String>,
    pub swap_mode: Option<String>,
    pub slippage_bps: Option<u16>,
    pub price_impact_pct: Option<String>,
    /// Base64 encoded transaction to sign.
    pub transaction: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

/// Request body for Ultra API POST /ultra/v1/execute.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UltraExecuteRequest {
    signed_transaction: String,
    request_id: String,
}

/// Response from Ultra API POST /ultra/v1/execute.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UltraExecuteResponse {
    pub status: String,
    pub signature: Option<String>,
    pub slot: Option<u64>,
    pub input_amount_result: Option<String>,
    pub output_amount_result: Option<String>,
    pub swap_events: Option<serde_json::Value>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

// ── Client ──────────────────────────────────────────────────────────

const ULTRA_BASE_URL: &str = "https://api.jup.ag";
const ULTRA_LITE_BASE_URL: &str = "https://lite-api.jup.ag";

/// Client for Jupiter APIs (Ultra + Swap).
///
/// Primary flow (Ultra): GET /ultra/v1/order → sign → POST /ultra/v1/execute
/// Jupiter handles MEV protection, priority fees, and transaction landing.
///
/// Fallback (Swap API): GET /swap/v1/quote for price-only queries (PriceFeed).
#[derive(Debug, Clone)]
pub struct JupiterClient {
    /// Base URL for Swap API (quote/swap endpoints).
    swap_base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl JupiterClient {
    pub fn new(swap_base_url: &str, api_key: &str) -> Self {
        Self {
            swap_base_url: swap_base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("Failed to build HTTP client"),
        }
    }

    fn ultra_base(&self) -> &str {
        if self.api_key.is_empty() {
            ULTRA_LITE_BASE_URL
        } else {
            ULTRA_BASE_URL
        }
    }

    fn add_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if !self.api_key.is_empty() {
            req.header("x-api-key", &self.api_key)
        } else {
            req
        }
    }

    // ── Ultra API ───────────────────────────────────────────────

    /// Request a swap order via Ultra API.
    /// Returns the order with a pre-built transaction to sign.
    pub async fn get_order(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        taker: &str,
    ) -> Result<UltraOrderResponse> {
        let url = format!(
            "{}/ultra/v1/order?inputMint={}&outputMint={}&amount={}&taker={}",
            self.ultra_base(),
            input_mint,
            output_mint,
            amount,
            taker,
        );

        debug!(url = %url, "Requesting Ultra order");

        let response = self
            .add_auth(self.http.get(&url))
            .send()
            .await
            .context("Failed to request Ultra order")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            error!(status = %status, body = %body, "Ultra order request failed");
            anyhow::bail!("Ultra order failed with HTTP {status}: {body}");
        }

        let order: UltraOrderResponse = response
            .json()
            .await
            .context("Failed to parse Ultra order response")?;

        // Check for API-level errors
        if let Some(err) = &order.error_code {
            let msg = order.error_message.as_deref().unwrap_or("unknown error");
            anyhow::bail!("Ultra order error {}: {}", err, msg);
        }

        if order.transaction.is_none() {
            anyhow::bail!("Ultra order returned no transaction — token may not be tradeable");
        }

        info!(
            request_id = %order.request_id,
            in_amount = %order.in_amount,
            out_amount = %order.out_amount,
            slippage_bps = ?order.slippage_bps,
            "Ultra order received"
        );

        Ok(order)
    }

    /// Sign the Ultra order transaction and execute it.
    /// Jupiter handles MEV protection and transaction landing.
    /// Returns (signature, actual_input, actual_output).
    pub async fn sign_and_execute(
        &self,
        order: &UltraOrderResponse,
        wallet: &Keypair,
    ) -> Result<(String, u64, u64)> {
        let tx_base64 = order
            .transaction
            .as_ref()
            .context("No transaction in Ultra order")?;

        // Decode and sign the versioned transaction
        let tx_bytes = BASE64
            .decode(tx_base64)
            .context("Failed to decode Ultra order transaction")?;

        let mut vtx: VersionedTransaction =
            bincode::deserialize(&tx_bytes).context("Failed to deserialize Ultra transaction")?;

        // Sign the transaction
        let signature = wallet.sign_message(&vtx.message.serialize());
        vtx.signatures[0] = signature;

        // Re-encode the signed transaction
        let signed_bytes = bincode::serialize(&vtx).context("Failed to serialize signed tx")?;
        let signed_base64 = BASE64.encode(&signed_bytes);

        // Execute via Ultra API
        let execute_url = format!("{}/ultra/v1/execute", self.ultra_base());
        let execute_body = UltraExecuteRequest {
            signed_transaction: signed_base64.clone(),
            request_id: order.request_id.clone(),
        };

        info!(
            request_id = %order.request_id,
            "Submitting signed transaction to Ultra execute"
        );

        // Poll for up to 60 seconds (Ultra says you can resubmit for status)
        let mut last_status = String::new();
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(60);

        loop {
            let response = self
                .add_auth(self.http.post(&execute_url))
                .json(&execute_body)
                .send()
                .await
                .context("Failed to submit Ultra execute")?;

            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                error!(status = %status, body = %body, "Ultra execute failed");
                anyhow::bail!("Ultra execute failed with HTTP {status}: {body}");
            }

            let result: UltraExecuteResponse = response
                .json()
                .await
                .context("Failed to parse Ultra execute response")?;

            match result.status.as_str() {
                "Success" => {
                    let sig = result
                        .signature
                        .unwrap_or_else(|| signature.to_string());
                    let actual_input = result
                        .input_amount_result
                        .as_deref()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    let actual_output = result
                        .output_amount_result
                        .as_deref()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);

                    info!(
                        signature = %sig,
                        actual_input = actual_input,
                        actual_output = actual_output,
                        slot = ?result.slot,
                        "Ultra swap executed successfully"
                    );

                    return Ok((sig, actual_input, actual_output));
                }
                "Failed" => {
                    let err_msg = result.error_message.unwrap_or_else(|| "unknown".to_string());
                    anyhow::bail!("Ultra swap failed: {}", err_msg);
                }
                other => {
                    // Pending/Processing — poll again
                    if last_status != other {
                        debug!(status = %other, "Ultra execute status");
                        last_status = other.to_string();
                    }
                }
            }

            if start.elapsed() > timeout {
                anyhow::bail!("Ultra execute timed out after {}s", timeout.as_secs());
            }

            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    // ── Swap API (quote-only, for PriceFeed) ────────────────────

    /// Get a swap quote from Jupiter Swap API.
    /// Used primarily by PriceFeed for price polling.
    pub async fn get_quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<QuoteResponse> {
        let url = format!(
            "{}/quote?inputMint={}&outputMint={}&amount={}&slippageBps={}",
            self.swap_base_url, input_mint, output_mint, amount, slippage_bps
        );

        debug!(url = %url, "Requesting Jupiter quote");

        let response = self
            .add_auth(self.http.get(&url))
            .send()
            .await
            .context("Failed to request Jupiter quote")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            error!(status = %status, body = %body, "Jupiter quote request failed");
            anyhow::bail!("Jupiter quote failed with HTTP {status}: {body}");
        }

        let quote: QuoteResponse = response
            .json()
            .await
            .context("Failed to parse Jupiter quote response")?;

        info!(
            input_mint = %input_mint,
            output_mint = %output_mint,
            in_amount = %quote.in_amount,
            out_amount = %quote.out_amount,
            price_impact = %quote.price_impact_pct,
            "Jupiter quote received"
        );

        Ok(quote)
    }

    /// Get the expected output amount for a given input.
    pub async fn get_expected_output(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<u64> {
        let quote = self
            .get_quote(input_mint, output_mint, amount, slippage_bps)
            .await?;

        let out_amount: u64 = quote
            .out_amount
            .parse()
            .context("Failed to parse Jupiter output amount")?;

        Ok(out_amount)
    }
}
