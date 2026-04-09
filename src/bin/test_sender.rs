//! Standalone test: send 1 minimal transaction via Helius Sender.
//!
//! Validates the full pipeline:
//!   1. Wallet load from env
//!   2. Balance check
//!   3. Blockhash fetch
//!   4. Tip instruction build (Helius tip account)
//!   5. Priority fee (ComputeBudget)
//!   6. Transaction sign
//!   7. Send via Helius Sender
//!   8. Confirm on-chain
//!
//! Usage: TEST_PRIVATE_KEY=<base58> cargo run --release --bin test_sender
//!   Or set it in .env as TEST_PRIVATE_KEY

use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signer::{keypair::Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};
use std::time::Instant;

const SENDER_ENDPOINT: &str = "https://sender.helius-rpc.com/fast";
const MIN_TIP_LAMPORTS: u64 = 200_000; // 0.0002 SOL

const TIP_ACCOUNTS: &[&str] = &[
    "4ACfpUFoaSD9bfPdeu6DBt89gB6ENTeHBXCAi87NhDEE",
    "D2L6yPZ2FmmmTKPgzaMKdhu6EWZcTpLy1Vhx8uvZe7NZ",
    "9bnz4RShgq1hAnLnZbP8kbgBg1kEmcJBYQq3gQbmnSta",
    "5VY91ws6B2hMmBFRsXkoAAdsPHBJwRfBht4DXox3xkwn",
];

#[tokio::main]
async fn main() -> Result<()> {
    println!("╔══════════════════════════════════════════╗");
    println!("║   HELIUS SENDER TEST — Single TX Test    ║");
    println!("╚══════════════════════════════════════════╝");
    println!();

    dotenvy::dotenv().ok();

    let rpc_url = std::env::var("SOLANA_RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

    // Load wallet — decrypt from DB (same as main app)
    println!("[1/8] Loading wallet...");
    let keypair = {
        let password = std::env::var("WALLET_PASSWORD")
            .context("WALLET_PASSWORD not set")?;
        let salt = std::env::var("ENCRYPTION_SALT")
            .context("ENCRYPTION_SALT not set")?;
        let db_path = std::env::var("DATABASE_PATH")
            .unwrap_or_else(|_| "data/ricoz-sniper.db".to_string());

        let db = ricoz_sniper::db::init_database(&db_path)?;
        let wallets = ricoz_sniper::db::queries::get_wallets(&db)?;
        let (_, _pubkey, encrypted, _, _) = wallets
            .into_iter()
            .find(|(_, _, _, _, active)| *active)
            .context("No active wallet in DB")?;

        let key_bytes = ricoz_sniper::security::encrypt::decrypt_private_key(
            &encrypted, &password, &salt,
        ).context("Wallet decryption failed")?;
        Keypair::from_bytes(&key_bytes)
            .map_err(|e| anyhow::anyhow!("Invalid keypair: {}", e))?
    };

    let pubkey = keypair.pubkey();
    println!("  OK Wallet: {}", pubkey);

    // ── 2. Check balance ──────────────────────────────────────
    println!("[2/8] Checking balance...");
    let rpc = RpcClient::new(rpc_url.clone());
    let balance = rpc.get_balance(&pubkey)
        .context("Failed to get balance")?;
    let balance_sol = balance as f64 / LAMPORTS_PER_SOL as f64;
    println!("  OK Balance: {:.6} SOL ({} lamports)", balance_sol, balance);

    if balance < 500_000 {
        println!("  XX INSUFFICIENT — need at least 0.0005 SOL");
        println!("     Send SOL to: {}", pubkey);
        return Ok(());
    }

    // ── 3. Fetch blockhash ────────────────────────────────────
    println!("[3/8] Fetching recent blockhash...");
    let t0 = Instant::now();
    let recent_blockhash = rpc.get_latest_blockhash()
        .context("Failed to get blockhash")?;
    println!("  OK Blockhash: {}...  ({}ms)",
        &recent_blockhash.to_string()[..16], t0.elapsed().as_millis());

    // ── 4. Build tip instruction ──────────────────────────────
    println!("[4/8] Building tip instruction...");
    let tip_idx = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as usize) % TIP_ACCOUNTS.len();
    let tip_account: Pubkey = TIP_ACCOUNTS[tip_idx].parse()?;
    let tip_ix = system_instruction::transfer(&pubkey, &tip_account, MIN_TIP_LAMPORTS);
    println!("  OK Tip: {} lamports -> {}...", MIN_TIP_LAMPORTS, &TIP_ACCOUNTS[tip_idx][..16]);

    // ── 5. Build priority fee instruction ─────────────────────
    println!("[5/8] Setting priority fee...");
    let priority_fee: u64 = 50_000; // 50K microLamports/CU
    let cu_limit: u32 = 50_000;
    let cu_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(cu_limit);
    let cu_price_ix = ComputeBudgetInstruction::set_compute_unit_price(priority_fee);
    println!("  OK Priority: {} uLamports/CU, CU limit: {}", priority_fee, cu_limit);

    // ── 6. Build & sign transaction ───────────────────────────
    println!("[6/8] Building & signing transaction...");

    // Self-transfer 1 lamport (cheapest possible test)
    let transfer_ix = system_instruction::transfer(&pubkey, &pubkey, 1);

    let instructions = vec![cu_limit_ix, cu_price_ix, transfer_ix, tip_ix];

    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&pubkey),
        &[&keypair],
        recent_blockhash,
    );

    let serialized = bincode::serialize(&tx)
        .context("Failed to serialize tx")?;

    let sig_str = tx.signatures[0].to_string();
    println!("  OK Tx size: {} bytes", serialized.len());
    println!("  OK Signature: {}", sig_str);
    println!("  OK 4 instructions: CU_limit, CU_price, self_transfer(1), tip(200000)");

    // ── 7. Send via Helius Sender ─────────────────────────────
    println!("[7/8] Sending via Helius Sender...");
    println!("  -> POST {}", SENDER_ENDPOINT);
    let t_send = Instant::now();

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let base64_tx = base64::engine::general_purpose::STANDARD
        .encode(&serialized);

    use base64::Engine;

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "test-sender-1",
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

    let response = http
        .post(SENDER_ENDPOINT)
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await
        .context("Sender HTTP request failed")?;

    let http_status = response.status();
    let body: serde_json::Value = response.json().await
        .context("Failed to parse Sender response")?;

    let send_ms = t_send.elapsed().as_millis();

    if let Some(err) = body.get("error") {
        println!("  XX SENDER ERROR ({}ms):", send_ms);
        println!("     HTTP status: {}", http_status);
        println!("     Error: {}", err);
        return Ok(());
    }

    let returned_sig = body["result"].as_str().unwrap_or("?");
    println!("  OK SENT! ({}ms)", send_ms);
    println!("  OK Returned sig: {}", returned_sig);
    println!("  OK Sig match: {}", returned_sig == sig_str);

    // ── 8. Poll for confirmation ──────────────────────────────
    println!("[8/8] Polling for confirmation (max 30s)...");
    let t_confirm = Instant::now();
    let mut confirmed = false;
    let mut final_status = "timeout".to_string();

    for attempt in 1..=20 {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        let status_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getSignatureStatuses",
            "params": [[returned_sig]]
        });

        if let Ok(resp) = http.post(&rpc_url).json(&status_req).send().await {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                let status = body["result"]["value"][0]["confirmationStatus"]
                    .as_str()
                    .unwrap_or("pending");
                let err = &body["result"]["value"][0]["err"];

                let elapsed = t_confirm.elapsed().as_millis();
                if err.is_null() || err.is_string() && err.as_str() == Some("null") {
                    println!("  [{:2}/20] status={:12} err=none  ({}ms)", attempt, status, elapsed);
                } else {
                    println!("  [{:2}/20] status={:12} err={}  ({}ms)", attempt, status, err, elapsed);
                }

                if status == "confirmed" || status == "finalized" {
                    confirmed = true;
                    final_status = status.to_string();
                    break;
                }
                if !err.is_null() && err.as_str() != Some("null") {
                    final_status = format!("failed: {}", err);
                    break;
                }
            }
        }
    }

    // ── Result ────────────────────────────────────────────────
    println!();
    println!("════════════════════════════════════════════");
    if confirmed {
        let total_ms = t_send.elapsed().as_millis();
        let confirm_ms = t_confirm.elapsed().as_millis();
        println!("  OK SUCCESS — Transaction {}!", final_status);
        println!("  OK Send latency:    {}ms", send_ms);
        println!("  OK Confirm latency: {}ms", confirm_ms);
        println!("  OK Total latency:   {}ms", total_ms);
        println!("  OK Signature: {}", returned_sig);
        println!("  OK https://solscan.io/tx/{}", returned_sig);
    } else {
        println!("  XX FAILED — Status: {}", final_status);
        println!("  Check: https://solscan.io/tx/{}", returned_sig);
    }
    println!("════════════════════════════════════════════");

    // Final balance
    if let Ok(new_balance) = rpc.get_balance(&pubkey) {
        let new_sol = new_balance as f64 / LAMPORTS_PER_SOL as f64;
        let cost_sol = balance_sol - new_sol;
        println!("  Balance before: {:.6} SOL", balance_sol);
        println!("  Balance after:  {:.6} SOL", new_sol);
        println!("  Cost:           {:.6} SOL", cost_sol);
    }

    Ok(())
}
