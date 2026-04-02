use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::Rng;
use serde::{Deserialize, Serialize};
use solana_sdk::{
    hash::Hash,
    pubkey::Pubkey,
    signer::keypair::Keypair,
    signer::Signer,
    system_instruction,
    transaction::Transaction,
};
use tracing::{debug, error, info, warn};

// ── Bundle status types ─────────────────────────────────────────────

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
    /// Transaction signatures included in the bundle (populated when Landed/Finalized).
    transactions: Option<Vec<String>>,
}

/// Result of a bundle confirmation poll.
#[derive(Debug, Clone)]
pub enum BundleConfirmation {
    /// Bundle landed on-chain at the given slot.
    Landed {
        slot: Option<u64>,
        /// Transaction signatures from the bundle (first = swap tx, last = tip tx).
        transactions: Vec<String>,
    },
    /// Bundle was explicitly rejected by the block engine.
    Failed { status: String },
    /// Polling timed out without a terminal status.
    Timeout,
}

impl BundleConfirmation {
    pub fn is_landed(&self) -> bool {
        matches!(self, Self::Landed { .. })
    }

    /// Extract the first (swap) transaction signature, if the bundle landed.
    pub fn swap_signature(&self) -> Option<&str> {
        match self {
            Self::Landed { transactions, .. } => transactions.first().map(|s| s.as_str()),
            _ => None,
        }
    }
}

// ── Known Jito tip accounts ─────────────────────────────────────────

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

// ── JSON-RPC types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct JitoRpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    params: Vec<serde_json::Value>,
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

// ── Client ──────────────────────────────────────────────────────────

/// Client for submitting transaction bundles to the Jito block engine
/// for MEV protection and priority landing.
#[derive(Debug, Clone)]
pub struct JitoClient {
    endpoint: String,
    http: reqwest::Client,
}

impl JitoClient {
    pub fn new(endpoint: &str) -> Self {
        if !endpoint.starts_with("https://") {
            panic!(
                "Jito endpoint must use HTTPS (got: '{}').\n\
                 Using HTTP exposes bundle data to network eavesdropping.",
                endpoint
            );
        }
        Self {
            endpoint: format!("{}/api/v1/bundles", endpoint.trim_end_matches('/')),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("Failed to build Jito HTTP client"),
        }
    }

    /// Pick a random Jito tip account and build a SOL transfer instruction to it.
    pub fn build_tip_instruction(_payer: &Pubkey, _tip_lamports: u64) -> Result<()> {
        // Kept for backward compat — use build_tip_tx() instead.
        let _ = Self::random_tip_account()?;
        Ok(())
    }

    /// Build a standalone tip transaction that transfers SOL to a random Jito tip account.
    /// This is appended as the last transaction in the bundle.
    pub fn build_tip_tx(
        payer: &Keypair,
        tip_lamports: u64,
        recent_blockhash: Hash,
    ) -> Result<Transaction> {
        if tip_lamports == 0 {
            anyhow::bail!("Tip must be > 0 lamports");
        }

        let tip_account = Self::random_tip_account()?;
        let ix = system_instruction::transfer(&payer.pubkey(), &tip_account, tip_lamports);

        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[payer],
            recent_blockhash,
        );

        Ok(tx)
    }

    /// Submit a bundle WITH a tip transaction automatically appended.
    ///
    /// The tip is built as a separate signed transaction appended to the end of the bundle.
    /// Jito requires the tip to be in the LAST transaction of the bundle.
    ///
    /// # Arguments
    /// * `transactions` - Pre-signed swap/trade transactions
    /// * `tip_lamports` - SOL tip to Jito (in lamports). Must be > 0.
    /// * `payer` - Keypair that pays the tip
    /// * `recent_blockhash` - Recent blockhash for the tip transaction
    pub async fn submit_bundle(
        &self,
        transactions: Vec<Transaction>,
        tip_lamports: u64,
        payer: &Keypair,
        recent_blockhash: Hash,
    ) -> Result<String> {
        if transactions.is_empty() {
            anyhow::bail!("Cannot submit empty bundle");
        }

        if tip_lamports == 0 {
            anyhow::bail!("Jito tip must be > 0 lamports");
        }

        // Validate that all transactions have at least one signature
        for (i, tx) in transactions.iter().enumerate() {
            if tx.signatures.is_empty() || tx.signatures[0] == solana_sdk::signature::Signature::default() {
                anyhow::bail!("Transaction {} in bundle is unsigned", i);
            }
        }

        // Build and append the tip transaction as the last entry
        let tip_tx = Self::build_tip_tx(payer, tip_lamports, recent_blockhash)?;

        let mut bundle = transactions;
        bundle.push(tip_tx);

        let bundle_size = bundle.len();

        info!(
            tx_count = bundle_size,
            tip_lamports = tip_lamports,
            payer = %payer.pubkey(),
            "Submitting Jito bundle (last tx = tip)"
        );

        // Serialize all transactions to base64 (Jito expects base64 for sendBundle)
        let encoded_txs: Vec<String> = bundle
            .iter()
            .map(|tx| {
                let serialized =
                    bincode::serialize(tx).context("Failed to serialize transaction")?;
                Ok(BASE64.encode(&serialized))
            })
            .collect::<Result<Vec<_>>>()?;

        let request = JitoRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: "sendBundle".to_string(),
            params: vec![serde_json::json!(encoded_txs)],
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

        info!(
            bundle_id = %bundle_id,
            tx_count = bundle_size,
            tip_lamports = tip_lamports,
            "Jito bundle submitted successfully"
        );

        Ok(bundle_id)
    }

    /// Poll for bundle confirmation.
    ///
    /// Returns a structured `BundleConfirmation` indicating whether the bundle
    /// landed, failed, or timed out.
    pub async fn confirm_bundle(
        &self,
        bundle_id: &str,
        timeout_secs: u64,
    ) -> Result<BundleConfirmation> {
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let poll_interval = std::time::Duration::from_millis(1500);
        let mut attempts = 0u32;

        loop {
            if tokio::time::Instant::now() >= deadline {
                warn!(
                    bundle_id = %bundle_id,
                    attempts = attempts,
                    "Bundle confirmation timed out after {}s",
                    timeout_secs,
                );
                return Ok(BundleConfirmation::Timeout);
            }

            attempts += 1;

            match self.get_bundle_status(bundle_id).await {
                Ok(raw) => {
                    if let Ok(parsed) = serde_json::from_str::<BundleStatusResponse>(&raw) {
                        if let Some(result) = parsed.result {
                            for entry in &result.value {
                                if entry.bundle_id == bundle_id {
                                    match entry.status.as_str() {
                                        "Landed" | "Finalized" => {
                                            let txs = entry.transactions.clone().unwrap_or_default();
                                            info!(
                                                bundle_id = %bundle_id,
                                                status = %entry.status,
                                                landed_slot = ?entry.landed_slot,
                                                tx_count = txs.len(),
                                                attempts = attempts,
                                                "Bundle confirmed on-chain"
                                            );
                                            return Ok(BundleConfirmation::Landed {
                                                slot: entry.landed_slot,
                                                transactions: txs,
                                            });
                                        }
                                        "Failed" | "Invalid" => {
                                            warn!(
                                                bundle_id = %bundle_id,
                                                status = %entry.status,
                                                attempts = attempts,
                                                "Bundle rejected by block engine"
                                            );
                                            return Ok(BundleConfirmation::Failed {
                                                status: entry.status.clone(),
                                            });
                                        }
                                        other => {
                                            debug!(
                                                bundle_id = %bundle_id,
                                                status = %other,
                                                attempt = attempts,
                                                "Bundle still pending"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        bundle_id = %bundle_id,
                        error = %e,
                        attempt = attempts,
                        "Failed to query bundle status, retrying"
                    );
                }
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    // ── Internal helpers ────────────────────────────────────────

    /// Query bundle status via JSON-RPC getBundleStatuses.
    async fn get_bundle_status(&self, bundle_id: &str) -> Result<String> {
        let request = JitoRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: "getBundleStatuses".to_string(),
            params: vec![serde_json::json!([bundle_id])],
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

    /// Pick a random tip account from the known Jito tip account list.
    fn random_tip_account() -> Result<Pubkey> {
        let mut rng = rand::thread_rng();
        let idx = rng.gen_range(0..JITO_TIP_ACCOUNTS.len());
        JITO_TIP_ACCOUNTS[idx]
            .parse()
            .context("Failed to parse Jito tip account")
    }
}
