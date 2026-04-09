use anyhow::{Context, Result};
use base64::Engine;
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::Signer;
use tracing::{info, warn};

const PUMPPORTAL_TRADE_URL: &str = "https://pumpportal.fun/api/trade-local";

/// PumpPortal trade client for building and submitting PumpFun transactions
/// via PumpPortal's Local Trade API + Jito bundle.
///
/// Flow:
/// 1. POST to trade-local → get base58-encoded VersionedTransaction
/// 2. Deserialize, sign with wallet
/// 3. Submit to Jito as bundle (tip is built into the transaction via priorityFee)
#[derive(Clone)]
pub struct PumpPortalTradeClient {
    http: reqwest::Client,
}

impl PumpPortalTradeClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to build PumpPortal HTTP client"),
        }
    }

    /// Execute a buy via PumpPortal Local Trade API + Jito bundle.
    ///
    /// Returns the transaction signature on success.
    pub async fn execute_buy(
        &self,
        mint: &str,
        amount_sol: f64,
        slippage_bps: u16,
        jito_tip_sol: f64,
        wallet: &Keypair,
        pool: &str,
    ) -> Result<String> {
        let pubkey = wallet.pubkey().to_string();

        info!(
            mint = %mint,
            amount_sol = amount_sol,
            slippage = slippage_bps,
            pool = %pool,
            "PumpPortal: building buy transaction"
        );

        // 1. Get unsigned transaction from PumpPortal
        let body = serde_json::json!({
            "publicKey": pubkey,
            "action": "buy",
            "mint": mint,
            "denominatedInSol": "true",
            "amount": amount_sol,
            "slippage": slippage_bps as f64 / 100.0,  // PumpPortal uses % not bps
            "priorityFee": jito_tip_sol,  // This becomes the Jito tip
            "pool": pool
        });

        let response = self
            .http
            .post(PUMPPORTAL_TRADE_URL)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("PumpPortal trade-local request failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("PumpPortal returned HTTP {status}: {text}");
        }

        // PumpPortal returns raw binary (application/octet-stream) — NOT base58/JSON
        let tx_bytes = response
            .bytes()
            .await
            .context("Failed to read PumpPortal response bytes")?;

        info!(size = tx_bytes.len(), "PumpPortal: transaction received, signing...");

        // Deserialize as VersionedTransaction (raw binary / bincode)
        let mut versioned_tx: solana_sdk::transaction::VersionedTransaction =
            bincode::deserialize(&tx_bytes)
                .context("Failed to deserialize VersionedTransaction from PumpPortal")?;

        info!(
            num_sigs = versioned_tx.signatures.len(),
            "PumpPortal: deserialized OK, signing..."
        );

        // Sign the transaction
        let message_bytes = versioned_tx.message.serialize();
        let signature = wallet.sign_message(&message_bytes);

        if versioned_tx.signatures.is_empty() {
            versioned_tx.signatures.push(signature);
        } else {
            versioned_tx.signatures[0] = signature;
        }

        info!(signature = %signature, "PumpPortal: signed OK");

        // Get the signature string for tracking
        let sig_str = signature.to_string();

        // Re-serialize and send via standard RPC (PumpPortal tx has its own priority fee)
        let signed_bytes = bincode::serialize(&versioned_tx)
            .context("Failed to serialize signed transaction")?;
        let base64_tx = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);

        let rpc_url = std::env::var("SOLANA_RPC_URL")
            .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

        info!(signature = %sig_str, "PumpPortal: submitting via RPC sendTransaction");

        // 3. Submit via RPC sendTransaction (skipPreflight for speed)
        let rpc_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [
                base64_tx,
                {
                    "encoding": "base64",
                    "skipPreflight": true,
                    "maxRetries": 2
                }
            ]
        });

        let rpc_response = self
            .http
            .post(&rpc_url)
            .header("Content-Type", "application/json")
            .json(&rpc_request)
            .send()
            .await
            .context("RPC sendTransaction failed")?;

        let rpc_body: serde_json::Value = rpc_response
            .json()
            .await
            .context("Failed to parse RPC response")?;

        if let Some(err) = rpc_body.get("error") {
            warn!(error = %err, "RPC sendTransaction error");
            anyhow::bail!("RPC error: {err}");
        }

        let returned_sig = rpc_body["result"]
            .as_str()
            .unwrap_or(&sig_str)
            .to_string();

        info!(signature = %returned_sig, "PumpPortal buy sent via RPC");

        // 4. Poll for confirmation
        let confirmation = self.confirm_transaction(&returned_sig).await?;
        if !confirmation {
            warn!(signature = %returned_sig, "PumpPortal buy not confirmed in time — may still land");
        }

        Ok(sig_str)
    }

    /// Execute a sell via PumpPortal Local Trade API + Jito bundle.
    pub async fn execute_sell(
        &self,
        mint: &str,
        amount_pct: u8,
        slippage_bps: u16,
        jito_tip_sol: f64,
        wallet: &Keypair,
        pool: &str,
    ) -> Result<String> {
        let pubkey = wallet.pubkey().to_string();

        info!(
            mint = %mint,
            amount_pct = amount_pct,
            pool = %pool,
            "PumpPortal: building sell transaction"
        );

        let amount_str = format!("{}%", amount_pct);

        let body = serde_json::json!({
            "publicKey": pubkey,
            "action": "sell",
            "mint": mint,
            "denominatedInSol": "false",
            "amount": amount_str,
            "slippage": slippage_bps as f64 / 100.0,
            "priorityFee": jito_tip_sol,
            "pool": pool
        });

        let response = self
            .http
            .post(PUMPPORTAL_TRADE_URL)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("PumpPortal sell request failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("PumpPortal sell HTTP {status}: {text}");
        }

        let tx_bytes = response
            .bytes()
            .await
            .context("Failed to read PumpPortal sell response bytes")?;

        let mut versioned_tx: solana_sdk::transaction::VersionedTransaction =
            bincode::deserialize(&tx_bytes)
                .context("Failed to deserialize sell VersionedTransaction")?;

        let message_bytes = versioned_tx.message.serialize();
        let signature = wallet.sign_message(&message_bytes);
        versioned_tx.signatures[0] = signature;

        let sig_str = signature.to_string();

        let signed_bytes = bincode::serialize(&versioned_tx)
            .context("Failed to serialize signed sell transaction")?;
        let base64_tx = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);

        let rpc_url = std::env::var("SOLANA_RPC_URL")
            .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

        info!(signature = %sig_str, "PumpPortal: sell submitting via RPC");

        let rpc_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [
                base64_tx,
                {
                    "encoding": "base64",
                    "skipPreflight": true,
                    "maxRetries": 2
                }
            ]
        });

        let rpc_response = self
            .http
            .post(&rpc_url)
            .header("Content-Type", "application/json")
            .json(&rpc_request)
            .send()
            .await
            .context("RPC sell sendTransaction failed")?;

        let rpc_body: serde_json::Value = rpc_response
            .json()
            .await
            .context("Failed to parse RPC sell response")?;

        if let Some(err) = rpc_body.get("error") {
            anyhow::bail!("RPC sell error: {err}");
        }

        let returned_sig = rpc_body["result"]
            .as_str()
            .unwrap_or(&sig_str)
            .to_string();

        info!(signature = %returned_sig, "PumpPortal sell sent via RPC");

        let confirmation = self.confirm_transaction(&returned_sig).await?;
        if !confirmation {
            warn!(signature = %returned_sig, "PumpPortal sell not confirmed in time");
        }

        Ok(returned_sig)
    }

    /// Poll for transaction confirmation via RPC (up to 60s).
    async fn confirm_transaction(&self, signature: &str) -> Result<bool> {
        let rpc_url = std::env::var("SOLANA_RPC_URL")
            .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        let poll_interval = std::time::Duration::from_millis(1500);

        loop {
            if tokio::time::Instant::now() >= deadline {
                warn!(signature = %signature, "Transaction confirmation timed out");
                return Ok(false);
            }

            let status_req = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getSignatureStatuses",
                "params": [[signature]]
            });

            if let Ok(response) = self.http.post(&rpc_url).json(&status_req).send().await {
                if let Ok(body) = response.json::<serde_json::Value>().await {
                    let status = body["result"]["value"][0]["confirmationStatus"]
                        .as_str()
                        .unwrap_or("pending");
                    if status == "confirmed" || status == "finalized" {
                        info!(signature = %signature, status = %status, "Transaction confirmed");
                        return Ok(true);
                    }
                }
            }

            tokio::time::sleep(poll_interval).await;
        }
    }
}
