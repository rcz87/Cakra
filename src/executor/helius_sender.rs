use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::Rng;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::system_instruction;
use tracing::{debug, info, warn};

/// Helius Sender: ultra-low latency tx submission with dual routing
/// (validators + Jito simultaneously). Free, no credits consumed.
///
/// DEFAULT path for single-tx trades.
/// For multi-tx atomic bundles → use JitoClient.
const SENDER_ENDPOINT: &str = "https://sender.helius-rpc.com/fast";

/// Helius Sender tip accounts (different from standard Jito tip accounts)
const HELIUS_TIP_ACCOUNTS: &[&str] = &[
    "4ACfpUFoaSD9bfPdeu6DBt89gB6ENTeHBXCAi87NhDEE",
    "D2L6yPZ2FmmmTKPgzaMKdhu6EWZcTpLy1Vhx8uvZe7NZ",
    "9bnz4RShgq1hAnLnZbP8kbgBg1kEmcJBYQq3gQbmnSta",
    "5VY91ws6B2hMmBFRsXkoAAdsPHBJwRfBht4DXox3xkwn",
    "2nyhqdwKcJZR2vcqCyrYsaPVdAnFoJjiksCXJ7hfEYgD",
    "2q5pghRs6arqVjRvT5gfgWfWcHWmw1ZuCzphgd5KfWGJ",
    "wyvPkWjVZz1M8fHQnMMCDTQDbkManefNNhweYk5WkcF",
    "3KCKozbAaF75qEU33jtzozcJ29yJuaLJTy2jFdzUY8bT",
    "4vieeGHPYPG2MmyPRcYjdiDmmhN3ww7hsFNap8pVN3Ey",
    "4TQLFNWK8AovT1gFvda5jfw2oJeRMKEmw7aH6MGBJ3or",
];

/// Minimum tip for Helius Sender (0.0002 SOL)
const MIN_TIP_LAMPORTS: u64 = 200_000;

#[derive(Clone)]
pub struct HeliusSender {
    http: reqwest::Client,
    rpc_url: String,
}

impl HeliusSender {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to build HeliusSender HTTP client"),
            rpc_url: rpc_url.to_string(),
        }
    }

    /// Send serialized transaction bytes via Helius Sender.
    /// Tx MUST include tip instruction + priority fee + be signed.
    pub async fn send_raw(&self, serialized_tx: &[u8]) -> Result<String> {
        let base64_tx = BASE64.encode(serialized_tx);

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": chrono::Utc::now().timestamp_millis().to_string(),
            "method": "sendTransaction",
            "params": [
                base64_tx,
                {
                    "encoding": "base64",
                    "skipPreflight": true,
                    "maxRetries": 0
                }
            ]
        });

        let response = self
            .http
            .post(SENDER_ENDPOINT)
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .context("Helius Sender request failed")?;

        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse Sender response")?;

        if !status.is_success() {
            anyhow::bail!("Helius Sender HTTP {status}: {body}");
        }

        if let Some(err) = body.get("error") {
            anyhow::bail!("Helius Sender error: {err}");
        }

        let signature = body["result"]
            .as_str()
            .context("Sender response missing signature")?
            .to_string();

        info!(signature = %signature, "Tx sent via Helius Sender (dual routing)");
        Ok(signature)
    }

    /// Send + poll for confirmation. Returns signature on confirmed or timeout.
    pub async fn send_and_confirm(
        &self,
        serialized_tx: &[u8],
        timeout_secs: u64,
    ) -> Result<String> {
        let signature = self.send_raw(serialized_tx).await?;

        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(timeout_secs);
        let poll_interval = std::time::Duration::from_millis(1500);

        loop {
            if tokio::time::Instant::now() >= deadline {
                warn!(signature = %signature, "Confirmation timed out after {timeout_secs}s");
                return Ok(signature);
            }

            match self.check_signature_status(&signature).await {
                Ok(Some(status)) if status == "confirmed" || status == "finalized" => {
                    info!(signature = %signature, status = %status, "Tx confirmed");
                    return Ok(signature);
                }
                Ok(Some(status)) => {
                    debug!(signature = %signature, status = %status, "Still processing");
                }
                Ok(None) => {}
                Err(e) => {
                    debug!(error = %e, "Status check failed, retrying");
                }
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    async fn check_signature_status(&self, signature: &str) -> Result<Option<String>> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getSignatureStatuses",
            "params": [[signature]]
        });

        let response = self.http.post(&self.rpc_url).json(&request).send().await?;
        let body: serde_json::Value = response.json().await?;
        Ok(body["result"]["value"][0]["confirmationStatus"]
            .as_str()
            .map(|s| s.to_string()))
    }

    /// Helius Priority Fee API — returns recommended microLamports/CU.
    pub async fn get_priority_fee(&self, serialized_tx_base58: &str) -> Result<u64> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "getPriorityFeeEstimate",
            "params": [{
                "transaction": serialized_tx_base58,
                "options": {
                    "priorityLevel": "High",
                    "recommended": true
                }
            }]
        });

        let response = self
            .http
            .post(&self.rpc_url)
            .json(&request)
            .send()
            .await
            .context("Priority fee API failed")?;

        let body: serde_json::Value = response.json().await?;
        let fee = body["result"]["priorityFeeEstimate"]
            .as_f64()
            .map(|f| f as u64)
            .unwrap_or(50_000);

        debug!(priority_fee = fee, "Helius priority fee estimate");
        Ok(fee)
    }

    /// Build tip instruction to random Helius Sender tip account.
    pub fn build_tip_instruction(
        payer: &Pubkey,
        tip_lamports: u64,
    ) -> Result<solana_sdk::instruction::Instruction> {
        let tip = std::cmp::max(tip_lamports, MIN_TIP_LAMPORTS);
        let tip_account = Self::random_tip_account()?;
        Ok(system_instruction::transfer(payer, &tip_account, tip))
    }

    fn random_tip_account() -> Result<Pubkey> {
        let mut rng = rand::thread_rng();
        let idx = rng.gen_range(0..HELIUS_TIP_ACCOUNTS.len());
        HELIUS_TIP_ACCOUNTS[idx]
            .parse()
            .context("Failed to parse Helius tip account")
    }
}
