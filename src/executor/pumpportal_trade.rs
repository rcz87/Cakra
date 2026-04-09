use anyhow::{Context, Result};
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::Signer;
use tracing::{info, warn};

const PUMPPORTAL_TRADE_URL: &str = "https://pumpportal.fun/api/trade-local";
const JITO_BUNDLE_URL: &str = "https://mainnet.block-engine.jito.wtf/api/v1/bundles";

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

        // PumpPortal returns a base58-encoded VersionedTransaction (single tx, not array)
        let tx_base58: String = response
            .text()
            .await
            .context("Failed to read PumpPortal response")?;

        // Strip quotes if JSON string
        let tx_base58 = tx_base58.trim().trim_matches('"');

        info!("PumpPortal: transaction received, signing...");

        // 2. Decode, sign, re-encode
        let tx_bytes = bs58::decode(tx_base58)
            .into_vec()
            .context("Failed to decode base58 transaction from PumpPortal")?;

        // Deserialize as VersionedTransaction
        let mut versioned_tx: solana_sdk::transaction::VersionedTransaction =
            bincode::deserialize(&tx_bytes)
                .context("Failed to deserialize VersionedTransaction")?;

        // Sign the transaction
        let message_bytes = versioned_tx.message.serialize();
        let signature = wallet.sign_message(&message_bytes);
        versioned_tx.signatures[0] = signature;

        // Get the signature string for tracking
        let sig_str = signature.to_string();

        // Re-serialize for Jito
        let signed_bytes = bincode::serialize(&versioned_tx)
            .context("Failed to serialize signed transaction")?;
        let signed_base58 = bs58::encode(&signed_bytes).into_string();

        info!(
            signature = %sig_str,
            "PumpPortal: transaction signed, submitting to Jito"
        );

        // 3. Submit to Jito as bundle
        let jito_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [[signed_base58]]
        });

        let jito_response = self
            .http
            .post(JITO_BUNDLE_URL)
            .header("Content-Type", "application/json")
            .json(&jito_request)
            .send()
            .await
            .context("Jito bundle submission failed")?;

        let jito_status = jito_response.status();
        let jito_body: serde_json::Value = jito_response
            .json()
            .await
            .context("Failed to parse Jito response")?;

        if !jito_status.is_success() {
            anyhow::bail!("Jito HTTP {jito_status}: {jito_body}");
        }

        if let Some(err) = jito_body.get("error") {
            anyhow::bail!("Jito error: {err}");
        }

        let bundle_id = jito_body["result"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        info!(
            bundle_id = %bundle_id,
            signature = %sig_str,
            "PumpPortal buy submitted to Jito"
        );

        // 4. Confirm bundle landed
        let confirmation = self.confirm_jito_bundle(&bundle_id).await?;
        if !confirmation {
            anyhow::bail!("PumpPortal buy bundle did not land (bundle_id: {bundle_id})");
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

        let tx_base58: String = response
            .text()
            .await
            .context("Failed to read PumpPortal sell response")?;
        let tx_base58 = tx_base58.trim().trim_matches('"');

        let tx_bytes = bs58::decode(tx_base58)
            .into_vec()
            .context("Failed to decode base58 sell transaction")?;

        let mut versioned_tx: solana_sdk::transaction::VersionedTransaction =
            bincode::deserialize(&tx_bytes)
                .context("Failed to deserialize sell VersionedTransaction")?;

        let message_bytes = versioned_tx.message.serialize();
        let signature = wallet.sign_message(&message_bytes);
        versioned_tx.signatures[0] = signature;

        let sig_str = signature.to_string();

        let signed_bytes = bincode::serialize(&versioned_tx)
            .context("Failed to serialize signed sell transaction")?;
        let signed_base58 = bs58::encode(&signed_bytes).into_string();

        info!(
            signature = %sig_str,
            "PumpPortal: sell signed, submitting to Jito"
        );

        let jito_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [[signed_base58]]
        });

        let jito_response = self
            .http
            .post(JITO_BUNDLE_URL)
            .header("Content-Type", "application/json")
            .json(&jito_request)
            .send()
            .await
            .context("Jito sell bundle submission failed")?;

        let jito_body: serde_json::Value = jito_response
            .json()
            .await
            .context("Failed to parse Jito sell response")?;

        if let Some(err) = jito_body.get("error") {
            anyhow::bail!("Jito sell error: {err}");
        }

        let bundle_id = jito_body["result"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        info!(
            bundle_id = %bundle_id,
            signature = %sig_str,
            "PumpPortal sell submitted to Jito"
        );

        let confirmation = self.confirm_jito_bundle(&bundle_id).await?;
        if !confirmation {
            anyhow::bail!("PumpPortal sell bundle did not land (bundle_id: {bundle_id})");
        }

        Ok(sig_str)
    }

    /// Poll Jito for bundle confirmation (up to 60s).
    async fn confirm_jito_bundle(&self, bundle_id: &str) -> Result<bool> {
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        let poll_interval = std::time::Duration::from_millis(1500);

        loop {
            if tokio::time::Instant::now() >= deadline {
                warn!(bundle_id = %bundle_id, "PumpPortal bundle confirmation timed out");
                return Ok(false);
            }

            let status_request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getBundleStatuses",
                "params": [[bundle_id]]
            });

            if let Ok(response) = self
                .http
                .post(JITO_BUNDLE_URL)
                .json(&status_request)
                .send()
                .await
            {
                if let Ok(body) = response.json::<serde_json::Value>().await {
                    if let Some(entries) = body["result"]["value"].as_array() {
                        for entry in entries {
                            if entry["bundle_id"].as_str() == Some(bundle_id) {
                                match entry["status"].as_str() {
                                    Some("Landed") | Some("Finalized") => {
                                        info!(
                                            bundle_id = %bundle_id,
                                            status = %entry["status"],
                                            "PumpPortal bundle confirmed on-chain"
                                        );
                                        return Ok(true);
                                    }
                                    Some("Failed") | Some("Invalid") => {
                                        warn!(
                                            bundle_id = %bundle_id,
                                            status = %entry["status"],
                                            "PumpPortal bundle rejected"
                                        );
                                        return Ok(false);
                                    }
                                    _ => {} // Still pending
                                }
                            }
                        }
                    }
                }
            }

            tokio::time::sleep(poll_interval).await;
        }
    }
}
