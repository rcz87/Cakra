use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, error, info};

use std::str::FromStr;

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

// ── Swap Instructions API types ────────────────────────────────────

/// A single instruction from Jupiter's /swap-instructions response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JupiterInstruction {
    pub program_id: String,
    pub accounts: Vec<JupiterAccountMeta>,
    pub data: String, // base64 encoded
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JupiterAccountMeta {
    pub pubkey: String,
    pub is_signer: bool,
    pub is_writable: bool,
}

/// Response from POST /swap-instructions.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapInstructionsResponse {
    pub compute_budget_instructions: Vec<JupiterInstruction>,
    pub setup_instructions: Vec<JupiterInstruction>,
    pub swap_instruction: JupiterInstruction,
    pub cleanup_instruction: Option<JupiterInstruction>,
    pub other_instructions: Option<Vec<JupiterInstruction>>,
    pub address_lookup_table_addresses: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SwapInstructionsRequest {
    quote_response: QuoteResponse,
    user_public_key: String,
    wrap_and_unwrap_sol: bool,
    dynamic_compute_unit_limit: bool,
    prioritization_fee_lamports: serde_json::Value, // "auto" or number
}

// ── Instruction conversion ─────────────────────────────────────────

/// Convert a Jupiter API instruction into a `solana_sdk::instruction::Instruction`.
pub fn to_solana_instruction(jup_ix: &JupiterInstruction) -> Result<Instruction> {
    let program_id =
        Pubkey::from_str(&jup_ix.program_id).context("Invalid program_id pubkey")?;

    let accounts = jup_ix
        .accounts
        .iter()
        .map(|a| {
            let pubkey = Pubkey::from_str(&a.pubkey)
                .with_context(|| format!("Invalid account pubkey: {}", a.pubkey))?;
            Ok(if a.is_writable {
                AccountMeta::new(pubkey, a.is_signer)
            } else {
                AccountMeta::new_readonly(pubkey, a.is_signer)
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let data = BASE64
        .decode(&jup_ix.data)
        .context("Failed to decode instruction data from base64")?;

    Ok(Instruction {
        program_id,
        accounts,
        data,
    })
}

// ── Client ──────────────────────────────────────────────────────────

/// Client for Jupiter Swap API.
///
/// Primary flow: GET /quote → POST /swap-instructions → build & send transaction locally.
/// Also used by PriceFeed for price-only queries via GET /quote.
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

    fn add_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if !self.api_key.is_empty() {
            req.header("x-api-key", &self.api_key)
        } else {
            req
        }
    }

    // ── Swap Instructions API ───────────────────────────────────

    /// Request swap instructions from Jupiter for a given quote.
    /// Returns raw instructions that the caller can assemble into a transaction.
    pub async fn get_swap_instructions(
        &self,
        quote: &QuoteResponse,
        user_pubkey: &str,
    ) -> Result<SwapInstructionsResponse> {
        let url = format!("{}/swap-instructions", self.swap_base_url);

        let body = SwapInstructionsRequest {
            quote_response: quote.clone(),
            user_public_key: user_pubkey.to_string(),
            wrap_and_unwrap_sol: true,
            dynamic_compute_unit_limit: true,
            prioritization_fee_lamports: serde_json::json!("auto"),
        };

        debug!(url = %url, user_pubkey = %user_pubkey, "Requesting Jupiter swap instructions");

        let response = self
            .add_auth(self.http.post(&url))
            .json(&body)
            .send()
            .await
            .context("Failed to request Jupiter swap instructions")?;

        let status = response.status();
        if !status.is_success() {
            let resp_body = response.text().await.unwrap_or_default();
            error!(status = %status, body = %resp_body, "Jupiter swap-instructions request failed");
            anyhow::bail!("Jupiter swap-instructions failed with HTTP {status}: {resp_body}");
        }

        let instructions: SwapInstructionsResponse = response
            .json()
            .await
            .context("Failed to parse Jupiter swap-instructions response")?;

        info!(
            num_compute_budget = instructions.compute_budget_instructions.len(),
            num_setup = instructions.setup_instructions.len(),
            has_cleanup = instructions.cleanup_instruction.is_some(),
            num_alts = instructions.address_lookup_table_addresses.len(),
            "Jupiter swap instructions received"
        );

        Ok(instructions)
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
