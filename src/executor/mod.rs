pub mod jito;
pub mod jupiter;
pub mod positions;
pub mod price_feed;
pub mod priority;
pub mod pumpfun_buy;
pub mod raydium;
pub mod retry;
pub mod router;
pub mod sell;
pub mod tp_sl;

use std::sync::Arc;

use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::Transaction;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::db::DbPool;
use crate::models::token::{TokenInfo, TokenSource};
use crate::risk::{RiskManager, CooldownManager, ListManager, RiskCheck};

use self::jito::JitoClient;
use self::jupiter::JupiterClient;
use self::positions::PositionManager;
use self::priority::calculate_priority_fee;
use self::pumpfun_buy::build_pumpfun_buy;
use self::raydium::build_raydium_swap;
use self::retry::retry_with_backoff;
use self::router::{find_best_route, Route, RouteType};
use self::sell::build_sell_instruction;

/// Central executor service that coordinates all trading operations
/// for the RICOZ SNIPER bot.
pub struct ExecutorService {
    pub config: Arc<Config>,
    pub rpc: Arc<RpcClient>,
    pub jito: JitoClient,
    pub jupiter: JupiterClient,
    pub positions: PositionManager,
    pub db: DbPool,
    pub risk: RiskManager,
    pub cooldown: CooldownManager,
    pub lists: ListManager,
}

impl ExecutorService {
    pub fn new(
        config: Arc<Config>,
        db: DbPool,
        risk: RiskManager,
        cooldown: CooldownManager,
        lists: ListManager,
    ) -> Self {
        let rpc = Arc::new(RpcClient::new(config.effective_rpc_url().to_string()));
        let jito = JitoClient::new(&config.jito_block_engine_url);
        let jupiter = JupiterClient::new(&config.jupiter_api_url);
        let positions = PositionManager::new(db.clone());

        Self {
            config,
            rpc,
            jito,
            jupiter,
            positions,
            db,
            risk,
            cooldown,
            lists,
        }
    }

    /// Execute a buy for a given token. Returns the transaction signature on success.
    pub async fn execute_buy(
        &self,
        token: &TokenInfo,
        amount_sol: f64,
        slippage_bps: u16,
        wallet: &Keypair,
    ) -> Result<String> {
        info!(
            mint = %token.mint,
            symbol = %token.symbol,
            amount_sol = amount_sol,
            slippage_bps = slippage_bps,
            "Executing buy"
        );

        // Check blacklist
        if self.lists.is_blacklisted(&token.mint)? {
            anyhow::bail!("Token {} is blacklisted", token.mint);
        }

        // Risk manager check
        match self.risk.can_trade(amount_sol)? {
            RiskCheck::Denied(reason) => {
                anyhow::bail!("Risk check denied: {}", reason);
            }
            RiskCheck::Allowed => {}
        }

        // Cooldown check
        if !self.cooldown.can_trade(&wallet.pubkey().to_string()) {
            anyhow::bail!(
                "Wallet {} is on trade cooldown",
                wallet.pubkey()
            );
        }

        // Validate amount against risk limits
        if amount_sol > self.config.max_buy_sol {
            anyhow::bail!(
                "Buy amount {amount_sol} SOL exceeds max allowed {} SOL",
                self.config.max_buy_sol
            );
        }

        let open_count = self.positions.get_open_positions().len();
        if open_count >= self.config.max_positions as usize {
            anyhow::bail!(
                "Max open positions ({}) reached, cannot open new position",
                self.config.max_positions
            );
        }

        let amount_lamports = (amount_sol * 1_000_000_000.0) as u64;

        // Find the best route across all DEXes
        let route = retry_with_backoff(
            || {
                let token = token.clone();
                let jupiter = self.jupiter.clone();
                let rpc = self.rpc.clone();
                let config = self.config.clone();
                async move {
                    find_best_route(&token, amount_lamports, slippage_bps, &jupiter, &rpc, &config)
                        .await
                }
            },
            3,
        )
        .await
        .context("Failed to find route")?;

        info!(
            route = ?route.route_type,
            expected_output = route.expected_output,
            "Best route found"
        );

        // Build the transaction based on the chosen route
        let tx = self
            .build_transaction_for_route(&route, token, amount_lamports, slippage_bps, wallet)
            .await
            .context("Failed to build transaction")?;

        // Calculate dynamic priority fee
        let priority_fee = calculate_priority_fee(&self.rpc, 1.5).await.unwrap_or_else(|e| {
            warn!("Failed to calculate priority fee, using default: {e}");
            5000
        });

        info!(priority_fee = priority_fee, "Using priority fee");

        // Submit via Jito bundle for MEV protection
        let tip_lamports = self.config.jito_tip_lamports.max(priority_fee);
        let signature = retry_with_backoff(
            || {
                let jito = self.jito.clone();
                let tx = tx.clone();
                let tip = tip_lamports;
                async move { jito.submit_bundle(vec![tx], tip).await }
            },
            3,
        )
        .await
        .context("Failed to submit Jito bundle")?;

        info!(signature = %signature, "Buy transaction submitted");

        // Confirm the bundle landed on-chain before creating a position
        let confirmed = self.jito.confirm_bundle(&signature, 30).await?;
        if !confirmed {
            anyhow::bail!("Transaction not confirmed");
        }

        // Track the new position
        let entry_price = if route.expected_output > 0 {
            amount_sol / (route.expected_output as f64)
        } else {
            0.0
        };

        self.positions.open_position(
            &token.mint,
            &token.symbol,
            &wallet.pubkey().to_string(),
            entry_price,
            amount_sol,
            route.expected_output as f64,
            self.config.default_slippage_bps,
            &signature,
        )?;

        // Record trade for cooldown tracking
        self.cooldown.record_trade(&wallet.pubkey().to_string());

        // TODO: validate_slippage post-transaction by comparing expected vs actual output on-chain

        Ok(signature)
    }

    /// Execute a sell for a given token mint. Sells `amount_pct`% of holdings.
    /// Returns the transaction signature on success.
    pub async fn execute_sell(
        &self,
        mint: &str,
        amount_pct: u8,
        wallet: &Keypair,
    ) -> Result<String> {
        info!(
            mint = %mint,
            amount_pct = amount_pct,
            "Executing sell"
        );

        if amount_pct == 0 || amount_pct > 100 {
            anyhow::bail!("Invalid sell percentage: {amount_pct}. Must be 1-100");
        }

        // Build sell instructions (handles token account lookup and amount calc)
        let instructions =
            build_sell_instruction(mint, amount_pct, &wallet.pubkey(), &self.rpc, &self.jupiter)
                .await
                .context("Failed to build sell instruction")?;

        if instructions.is_empty() {
            anyhow::bail!("No sell instructions generated - token balance may be zero");
        }

        let recent_blockhash = self.rpc.get_latest_blockhash()?;
        let tx = Transaction::new_signed_with_payer(
            &instructions,
            Some(&wallet.pubkey()),
            &[wallet],
            recent_blockhash,
        );

        let tip_lamports = self.config.jito_tip_lamports;
        let signature = retry_with_backoff(
            || {
                let jito = self.jito.clone();
                let tx = tx.clone();
                let tip = tip_lamports;
                async move { jito.submit_bundle(vec![tx], tip).await }
            },
            3,
        )
        .await
        .context("Failed to submit sell bundle")?;

        info!(signature = %signature, "Sell transaction submitted");

        // Close position if 100% sell
        if amount_pct == 100 {
            self.positions.close_position(mint, &signature)?;
        }

        Ok(signature)
    }

    async fn build_transaction_for_route(
        &self,
        route: &Route,
        token: &TokenInfo,
        amount_lamports: u64,
        slippage_bps: u16,
        wallet: &Keypair,
    ) -> Result<Transaction> {
        let mint: solana_sdk::pubkey::Pubkey = token.mint.parse()?;

        match route.route_type {
            RouteType::PumpFun => {
                let ix =
                    build_pumpfun_buy(&mint, amount_lamports, slippage_bps, &wallet.pubkey())?;
                let recent_blockhash = self.rpc.get_latest_blockhash()?;
                Ok(Transaction::new_signed_with_payer(
                    &[ix],
                    Some(&wallet.pubkey()),
                    &[wallet],
                    recent_blockhash,
                ))
            }
            RouteType::Jupiter => {
                let wsol_mint = "So11111111111111111111111111111111111111112";
                let quote = self
                    .jupiter
                    .get_quote(wsol_mint, &token.mint, amount_lamports, slippage_bps)
                    .await?;
                let tx = self
                    .jupiter
                    .build_swap_tx(&quote, &wallet.pubkey().to_string())
                    .await?;
                Ok(tx)
            }
            RouteType::Raydium => {
                let pool_address = token
                    .pool_address
                    .as_ref()
                    .context("No pool address for Raydium route")?;
                let wsol_mint: solana_sdk::pubkey::Pubkey =
                    "So11111111111111111111111111111111111111112".parse()?;
                let ix = build_raydium_swap(
                    pool_address,
                    &wsol_mint,
                    &mint,
                    amount_lamports,
                    route.min_output,
                    &wallet.pubkey(),
                )?;
                let recent_blockhash = self.rpc.get_latest_blockhash()?;
                Ok(Transaction::new_signed_with_payer(
                    &[ix],
                    Some(&wallet.pubkey()),
                    &[wallet],
                    recent_blockhash,
                ))
            }
        }
    }
}
