use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};
use solana_sdk::transaction::Transaction;
use tracing::{debug, error, info};

/// Response from Jupiter V6 quote endpoint.
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

/// Request body for Jupiter V6 swap endpoint.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SwapRequest {
    quote_response: QuoteResponse,
    user_public_key: String,
    wrap_and_unwrap_sol: bool,
    use_shared_accounts: bool,
    dynamic_compute_unit_limit: bool,
    prioritization_fee_lamports: String,
}

/// Response from Jupiter V6 swap endpoint.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapResponse {
    swap_transaction: String,
    last_valid_block_height: Option<u64>,
}

/// Client for the Jupiter V6 Swap API.
#[derive(Debug, Clone)]
pub struct JupiterClient {
    base_url: String,
    http: reqwest::Client,
}

impl JupiterClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("Failed to build HTTP client"),
        }
    }

    /// Get a swap quote from Jupiter.
    ///
    /// # Arguments
    /// * `input_mint` - Input token mint address (e.g. SOL wrapped mint)
    /// * `output_mint` - Output token mint address
    /// * `amount` - Amount of input token in smallest unit (lamports for SOL)
    /// * `slippage_bps` - Slippage tolerance in basis points
    pub async fn get_quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<QuoteResponse> {
        let url = format!(
            "{}/quote?inputMint={}&outputMint={}&amount={}&slippageBps={}&onlyDirectRoutes=false&asLegacyTransaction=false",
            self.base_url, input_mint, output_mint, amount, slippage_bps
        );

        debug!(url = %url, "Requesting Jupiter quote");

        let response = self
            .http
            .get(&url)
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

    /// Build a swap transaction from a quote response.
    ///
    /// # Arguments
    /// * `quote` - The quote response from `get_quote`
    /// * `user_pubkey` - The user's wallet public key as a string
    ///
    /// # Returns
    /// A deserialized `Transaction` ready for signing and sending.
    pub async fn build_swap_tx(
        &self,
        quote: &QuoteResponse,
        user_pubkey: &str,
    ) -> Result<Transaction> {
        let url = format!("{}/swap", self.base_url);

        let request_body = SwapRequest {
            quote_response: quote.clone(),
            user_public_key: user_pubkey.to_string(),
            wrap_and_unwrap_sol: true,
            use_shared_accounts: true,
            dynamic_compute_unit_limit: true,
            prioritization_fee_lamports: "auto".to_string(),
        };

        debug!("Requesting Jupiter swap transaction");

        let response = self
            .http
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .context("Failed to request Jupiter swap transaction")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            error!(status = %status, body = %body, "Jupiter swap request failed");
            anyhow::bail!("Jupiter swap failed with HTTP {status}: {body}");
        }

        let swap_response: SwapResponse = response
            .json()
            .await
            .context("Failed to parse Jupiter swap response")?;

        // Decode the base64 transaction
        let tx_bytes = BASE64
            .decode(&swap_response.swap_transaction)
            .context("Failed to decode Jupiter swap transaction from base64")?;

        let tx: Transaction =
            bincode::deserialize(&tx_bytes).context("Failed to deserialize swap transaction")?;

        info!(
            user = %user_pubkey,
            block_height = ?swap_response.last_valid_block_height,
            "Jupiter swap transaction built"
        );

        Ok(tx)
    }

    /// Get the expected output amount for a given input.
    /// Convenience wrapper around `get_quote` that returns just the output amount.
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
