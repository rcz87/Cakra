pub mod jito;
pub mod jupiter;
pub mod positions;
pub mod price_feed;
pub mod priority;
pub mod pumpfun_buy;
pub mod raydium;
pub mod retry;
pub mod router;
pub mod tp_sl;

use std::sync::Arc;

use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::Signer;
use tracing::{info, warn};

use crate::config::Config;
use crate::db::DbPool;
use crate::models::token::TokenInfo;
use crate::risk::{RiskManager, CooldownManager, ListManager, RiskCheck};

use self::jito::JitoClient;
use self::jupiter::JupiterClient;
use self::positions::PositionManager;
use self::retry::retry_with_backoff;

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
        let jupiter = JupiterClient::new(&config.jupiter_api_url, &config.jupiter_api_key);
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
        let wsol_mint = "So11111111111111111111111111111111111111112";
        let taker = wallet.pubkey().to_string();

        // Request Ultra order (Jupiter handles routing, priority fees, MEV protection)
        let order = retry_with_backoff(
            || {
                let jupiter = self.jupiter.clone();
                let mint = token.mint.clone();
                let taker = taker.clone();
                async move {
                    jupiter
                        .get_order(wsol_mint, &mint, amount_lamports, &taker)
                        .await
                }
            },
            3,
        )
        .await
        .context("Failed to get Ultra order")?;

        let expected_output: u64 = order.out_amount.parse().unwrap_or(0);
        info!(
            request_id = %order.request_id,
            expected_output = expected_output,
            slippage_bps = ?order.slippage_bps,
            "Ultra order received"
        );

        // Sign and execute — Jupiter handles landing + MEV protection
        let (signature, actual_input, actual_output) = self
            .jupiter
            .sign_and_execute(&order, wallet)
            .await
            .context("Ultra swap execution failed")?;

        info!(
            signature = %signature,
            actual_input = actual_input,
            actual_output = actual_output,
            "Buy confirmed via Ultra"
        );

        if actual_output == 0 {
            anyhow::bail!(
                "Ultra swap returned 0 output for mint {} — swap may have failed",
                token.mint
            );
        }

        // Validate slippage: compare expected vs actual output
        if expected_output > 0 {
            let slippage_pct =
                ((expected_output as f64 - actual_output as f64) / expected_output as f64) * 100.0;
            info!(
                expected = expected_output,
                actual = actual_output,
                slippage_pct = format!("{:.2}", slippage_pct),
                "Post-execution slippage check"
            );
            if slippage_pct > 5.0 {
                warn!(
                    slippage_pct = format!("{:.2}", slippage_pct),
                    expected = expected_output,
                    actual = actual_output,
                    mint = %token.mint,
                    "High post-execution slippage detected (>5%)"
                );
            }
        }

        // Track the position using actual execution results
        let entry_price = if actual_output > 0 {
            amount_sol / (actual_output as f64)
        } else {
            0.0
        };

        self.positions.open_position(
            &token.mint,
            &token.symbol,
            &taker,
            entry_price,
            amount_sol,
            actual_output as f64,
            self.config.default_slippage_bps,
            &signature,
        )?;

        // Record trade for cooldown tracking
        self.cooldown.record_trade(&taker);

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

        let wsol_mint = "So11111111111111111111111111111111111111112";
        let taker = wallet.pubkey().to_string();

        // Get token balance to calculate sell amount
        let mint_pubkey: Pubkey = mint.parse().context("Invalid mint address")?;
        let token_program: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
            .parse()
            .unwrap();
        let ata_program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
            .parse()
            .unwrap();
        let (ata, _) = Pubkey::find_program_address(
            &[
                wallet.pubkey().as_ref(),
                token_program.as_ref(),
                mint_pubkey.as_ref(),
            ],
            &ata_program,
        );

        let balance = self
            .rpc
            .get_token_account_balance(&ata)
            .context("Failed to get token balance for sell")?;
        let total_amount: u64 = balance
            .amount
            .parse()
            .context("Failed to parse token balance")?;

        if total_amount == 0 {
            anyhow::bail!("Token balance is zero for mint {mint}");
        }

        let sell_amount = (total_amount as u128 * amount_pct as u128 / 100) as u64;
        if sell_amount == 0 {
            anyhow::bail!("Calculated sell amount is zero");
        }

        info!(
            total_balance = total_amount,
            sell_pct = amount_pct,
            sell_amount = sell_amount,
            "Selling via Ultra API"
        );

        // Use Ultra API: token → SOL
        let order = retry_with_backoff(
            || {
                let jupiter = self.jupiter.clone();
                let mint = mint.to_string();
                let taker = taker.clone();
                async move {
                    jupiter
                        .get_order(&mint, wsol_mint, sell_amount, &taker)
                        .await
                }
            },
            3,
        )
        .await
        .context("Failed to get Ultra sell order")?;

        let (signature, _actual_input, actual_sol_output) = self
            .jupiter
            .sign_and_execute(&order, wallet)
            .await
            .context("Ultra sell execution failed")?;

        info!(
            signature = %signature,
            sol_received = actual_sol_output,
            "Sell confirmed via Ultra"
        );

        // Close position if 100% sell
        if amount_pct == 100 {
            self.positions.close_position(mint, &signature)?;
        }

        Ok(signature)
    }

}
