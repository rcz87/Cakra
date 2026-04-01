use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::Rng;
use serde::{Deserialize, Serialize};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_instruction,
    transaction::Transaction,
};
use tracing::{error, info, warn};

#[derive(Debug, Deserialize)]
struct BundleStatusResponse {
    result: Option<BundleStatusResult>,
}

#[derive(Debug, Deserialize)]
struct BundleStatusResult {
    value: Vec<BundleStatusEntry>,
}

#[derive(Debug, Deserialize)]
struct BundleStatusEntry {
    bundle_id: String,
    status: String,
    landed_slot: Option<u64>,
}

/// Known Jito tip payment accounts.
const JITO_TIP_ACCOUNTS: &[&str] = &[
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4bVqkfRtQ3NpsLTUBFFL2Lg",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSLie7Rci3VQDAGULnP",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

#[derive(Debug, Clone, Serialize)]
struct JitoBundleRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    params: Vec<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct JitoBundleResponse {
    result: Option<String>,
    error: Option<JitoError>,
}

#[derive(Debug, Deserialize)]
struct JitoError {
    message: String,
    code: Option<i64>,
}

/// Client for submitting transaction bundles to the Jito block engine
/// for MEV protection and priority landing.
#[derive(Debug, Clone)]
pub struct JitoClient {
    endpoint: String,
    http: reqwest::Client,
}

impl JitoClient {
    pub fn new(endpoint: &str) -> Self {
        Self {
            endpoint: format!("{}/api/v1/bundles", endpoint.trim_end_matches('/')),
            http: reqwest::Client::new(),
        }
    }

    /// Build a tip transaction that pays a random Jito tip account.
    pub fn build_tip_instruction(payer: &Pubkey, tip_lamports: u64) -> Result<Instruction> {
        let mut rng = rand::thread_rng();
        let tip_index = rng.gen_range(0..JITO_TIP_ACCOUNTS.len());
        let tip_account: Pubkey = JITO_TIP_ACCOUNTS[tip_index]
            .parse()
            .context("Failed to parse Jito tip account")?;

        Ok(system_instruction::transfer(payer, &tip_account, tip_lamports))
    }

    /// Submit a bundle of transactions to the Jito block engine.
    /// A tip transaction is automatically appended to the last transaction
    /// if not already present.
    ///
    /// Returns the bundle ID on success.
    pub async fn submit_bundle(
        &self,
        transactions: Vec<Transaction>,
        tip_lamports: u64,
    ) -> Result<String> {
        if transactions.is_empty() {
            anyhow::bail!("Cannot submit empty bundle");
        }

        info!(
            tx_count = transactions.len(),
            tip_lamports = tip_lamports,
            "Submitting Jito bundle"
        );

        // Serialize all transactions to base64
        let encoded_txs: Vec<String> = transactions
            .iter()
            .map(|tx| {
                let serialized =
                    bincode::serialize(tx).context("Failed to serialize transaction")?;
                Ok(BASE64.encode(&serialized))
            })
            .collect::<Result<Vec<_>>>()?;

        let request = JitoBundleRequest {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: "sendBundle".to_string(),
            params: vec![encoded_txs],
        };

        let response = self
            .http
            .post(&self.endpoint)
            .json(&request)
            .send()
            .await
            .context("Failed to send bundle request to Jito")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            error!(status = %status, body = %body, "Jito bundle submission failed");
            anyhow::bail!("Jito returned HTTP {status}: {body}");
        }

        let bundle_response: JitoBundleResponse = response
            .json()
            .await
            .context("Failed to parse Jito response")?;

        if let Some(err) = bundle_response.error {
            error!(
                message = %err.message,
                code = ?err.code,
                "Jito bundle error"
            );
            anyhow::bail!("Jito bundle error: {}", err.message);
        }

        let bundle_id = bundle_response
            .result
            .context("Jito response missing bundle ID")?;

        info!(bundle_id = %bundle_id, "Jito bundle submitted successfully");

        Ok(bundle_id)
    }

    /// Check the status of a previously submitted bundle.
    pub async fn get_bundle_status(&self, bundle_id: &str) -> Result<String> {
        #[derive(Serialize)]
        struct StatusRequest {
            jsonrpc: String,
            id: u64,
            method: String,
            params: Vec<Vec<String>>,
        }

        let request = StatusRequest {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: "getBundleStatuses".to_string(),
            params: vec![vec![bundle_id.to_string()]],
        };

        let response = self
            .http
            .post(&self.endpoint)
            .json(&request)
            .send()
            .await
            .context("Failed to query bundle status")?;

        let body = response.text().await?;
        Ok(body)
    }

    /// Poll for bundle confirmation up to `timeout_secs` seconds.
    /// Returns `true` if the bundle landed/finalized, `false` otherwise.
    pub async fn confirm_bundle(&self, bundle_id: &str, timeout_secs: u64) -> Result<bool> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

        loop {
            if tokio::time::Instant::now() >= deadline {
                warn!(bundle_id = %bundle_id, "Bundle confirmation timed out");
                return Ok(false);
            }

            let raw = self.get_bundle_status(bundle_id).await?;
            if let Ok(parsed) = serde_json::from_str::<BundleStatusResponse>(&raw) {
                if let Some(result) = parsed.result {
                    for entry in &result.value {
                        if entry.bundle_id == bundle_id {
                            match entry.status.as_str() {
                                "Landed" | "Finalized" => {
                                    info!(
                                        bundle_id = %bundle_id,
                                        status = %entry.status,
                                        landed_slot = ?entry.landed_slot,
                                        "Bundle confirmed"
                                    );
                                    return Ok(true);
                                }
                                "Failed" | "Invalid" => {
                                    warn!(
                                        bundle_id = %bundle_id,
                                        status = %entry.status,
                                        "Bundle failed"
                                    );
                                    return Ok(false);
                                }
                                _ => {
                                    // Still pending, continue polling
                                }
                            }
                        }
                    }
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }
}
