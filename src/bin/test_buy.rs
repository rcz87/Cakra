//! Test: execute 1 real buy through the main executor path.
//!
//! Uses a known liquid PumpFun token (already on Jupiter) to test
//! the full flow: router → instructions → Helius Sender → confirm.
//!
//! Usage: cargo run --release --bin test_buy
//!
//! This will spend ~0.001 SOL + fees on a real token purchase.

use anyhow::{Context, Result};
use chrono::Utc;
use std::sync::Arc;
use std::time::Instant;

use solana_sdk::signer::Signer;

use ricoz_sniper::config::Config;
use ricoz_sniper::db;
use ricoz_sniper::executor::ExecutorService;
use ricoz_sniper::executor::positions::PositionManager;
use ricoz_sniper::models::token::{DetectionBackend, TokenInfo, TokenSource};
use ricoz_sniper::risk::{CooldownManager, ListManager, RiskManager};

#[tokio::main]
async fn main() -> Result<()> {
    // Init tracing so we see executor logs
    tracing_subscriber::fmt()
        .with_env_filter("info,ricoz_sniper::executor=debug")
        .init();

    println!("╔══════════════════════════════════════════╗");
    println!("║   REAL BUY TEST — Main Executor Path     ║");
    println!("╚══════════════════════════════════════════╝");
    println!();

    // ── Load config ───────────────────────────────────────────
    let config = Config::from_env()?;
    let database = db::init_database(&config.database_path)?;

    // ── Decrypt wallet ────────────────────────────────────────
    let password = std::env::var("WALLET_PASSWORD")
        .context("WALLET_PASSWORD not set")?;
    let wallet_mgr = ricoz_sniper::wallet::WalletManager::new(&config, database.clone())?;
    let active = wallet_mgr.get_active_wallet()?
        .context("No active wallet")?;
    let keypair = wallet_mgr.get_keypair(&active.pubkey, &password)?;

    println!("[1] Wallet: {}", active.pubkey);

    // Check balance
    let rpc = solana_client::nonblocking::rpc_client::RpcClient::new(config.effective_rpc_url().to_string());
    let balance = rpc.get_balance(&keypair.pubkey()).await?;
    let balance_sol = balance as f64 / 1_000_000_000.0;
    println!("[2] Balance: {:.6} SOL", balance_sol);

    if balance_sol < 0.002 {
        println!("    INSUFFICIENT — need at least 0.002 SOL");
        return Ok(());
    }

    // ── Build executor ────────────────────────────────────────
    let risk = RiskManager::new(config.clone(), database.clone());
    let cooldown = CooldownManager::new(config.trade_cooldown_secs);
    let lists = ListManager::new(database.clone());
    let positions = PositionManager::new(database.clone(), config.trading_profile());

    let executor = ExecutorService::new(
        Arc::new(config.clone()),
        database.clone(),
        risk,
        cooldown,
        lists,
        positions,
    );

    // ── Pick a test token ─────────────────────────────────────
    // Use WSOL → USDC swap via Jupiter as the simplest test.
    // This avoids PumpFun-specific routing and tests the Helius Sender path directly.
    let usdc_mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    let test_token = TokenInfo {
        mint: usdc_mint.to_string(),
        name: "USD Coin".to_string(),
        symbol: "USDC".to_string(),
        source: TokenSource::Unknown, // Forces Jupiter route
        creator: String::new(),
        initial_liquidity_sol: 1000.0,
        initial_liquidity_usd: 0.0,
        pool_address: None,
        metadata_uri: None,
        decimals: 6,
        detected_at: Utc::now(),
        backend: DetectionBackend::Helius,
        market_cap_sol: 0.0,
        v_sol_in_bonding_curve: 0.0,
        initial_buy_sol: 0.0,
    };

    let buy_amount_sol = 0.001; // Tiny test amount
    let slippage_bps = 500; // 5% slippage (generous for test)

    println!("[3] Test: buy {:.4} SOL worth of {} ({})", buy_amount_sol, test_token.symbol, test_token.mint);
    println!("    Route: Jupiter → Helius Sender");
    println!("    Slippage: {}bps", slippage_bps);
    println!();

    // ── Execute buy ───────────────────────────────────────────
    let t0 = Instant::now();
    println!("[4] Executing buy...");

    match executor.execute_buy(&test_token, buy_amount_sol, slippage_bps, &keypair).await {
        Ok(signature) => {
            let elapsed = t0.elapsed().as_millis();
            println!();
            println!("════════════════════════════════════════════");
            println!("  OK BUY SUCCEEDED!");
            println!("  OK Signature: {}", signature);
            println!("  OK Latency: {}ms", elapsed);
            println!("  OK https://solscan.io/tx/{}", signature);
            println!("════════════════════════════════════════════");

            // Check final balance
            if let Ok(new_bal) = rpc.get_balance(&keypair.pubkey()).await {
                let new_sol = new_bal as f64 / 1_000_000_000.0;
                println!("  Balance after: {:.6} SOL (spent: {:.6} SOL)", new_sol, balance_sol - new_sol);
            }
        }
        Err(e) => {
            let elapsed = t0.elapsed().as_millis();
            println!();
            println!("════════════════════════════════════════════");
            println!("  XX BUY FAILED ({}ms)", elapsed);
            println!("  XX Error: {:#}", e);
            println!("════════════════════════════════════════════");
        }
    }

    Ok(())
}
