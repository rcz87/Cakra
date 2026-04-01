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
use solana_sdk::transaction::Transaction;
use tracing::info;

use crate::config::Config;
use crate::db::DbPool;
use crate::models::token::TokenInfo;
use crate::risk::{RiskManager, CooldownManager, ListManager, RiskCheck};

use self::jito::JitoClient;
use self::jupiter::{JupiterClient, to_solana_instruction};
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

        // 1. Get a quote from Jupiter Swap API
        let quote = retry_with_backoff(
            || {
                let jupiter = self.jupiter.clone();
                let mint = token.mint.clone();
                async move {
                    jupiter
                        .get_quote(wsol_mint, &mint, amount_lamports, slippage_bps)
                        .await
                }
            },
            3,
        )
        .await
        .context("Failed to get Jupiter quote")?;

        let expected_output: u64 = quote
            .out_amount
            .parse()
            .context("Failed to parse Jupiter out_amount")?;
        info!(
            expected_output = expected_output,
            slippage_bps = slippage_bps,
            price_impact = %quote.price_impact_pct,
            "Jupiter quote received for buy"
        );

        // 2. Get swap instructions from Jupiter
        let swap_ixs = self
            .jupiter
            .get_swap_instructions(&quote, &taker)
            .await
            .context("Failed to get Jupiter swap instructions")?;

        // 3. Build transaction from instructions
        let mut instructions = Vec::new();
        for ix in &swap_ixs.compute_budget_instructions {
            instructions.push(to_solana_instruction(ix)?);
        }
        for ix in &swap_ixs.setup_instructions {
            instructions.push(to_solana_instruction(ix)?);
        }
        instructions.push(to_solana_instruction(&swap_ixs.swap_instruction)?);
        if let Some(cleanup) = &swap_ixs.cleanup_instruction {
            instructions.push(to_solana_instruction(cleanup)?);
        }
        if let Some(other) = &swap_ixs.other_instructions {
            for ix in other {
                instructions.push(to_solana_instruction(ix)?);
            }
        }

        let recent_blockhash = self
            .rpc
            .get_latest_blockhash()
            .context("Failed to get recent blockhash")?;

        let swap_tx = Transaction::new_signed_with_payer(
            &instructions,
            Some(&wallet.pubkey()),
            &[wallet],
            recent_blockhash,
        );

        // 4. Submit via Jito bundle for MEV protection
        let bundle_id = self
            .jito
            .submit_bundle(
                vec![swap_tx],
                self.config.jito_tip_lamports,
                wallet,
                recent_blockhash,
            )
            .await
            .context("Failed to submit buy bundle to Jito")?;

        // 5. Confirm the bundle landed
        let confirmation = self
            .jito
            .confirm_bundle(&bundle_id, 60)
            .await
            .context("Failed to confirm buy bundle")?;

        if !confirmation.is_landed() {
            anyhow::bail!(
                "Buy bundle did not land for mint {} (bundle_id: {})",
                token.mint,
                bundle_id
            );
        }

        let signature = bundle_id.clone();

        info!(
            bundle_id = %bundle_id,
            expected_output = expected_output,
            "Buy confirmed via Jito bundle"
        );

        // Track the position using expected output from quote
        let entry_price = if expected_output > 0 {
            amount_sol / (expected_output as f64)
        } else {
            0.0
        };

        self.positions.open_position(
            &token.mint,
            &token.symbol,
            &taker,
            entry_price,
            amount_sol,
            expected_output as f64,
            self.config.default_slippage_bps,
            &signature,
            0, // security_score: caller can override when available
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
            "Selling via Jupiter Swap API"
        );

        // 1. Get a quote: token → SOL
        let default_slippage = self.config.default_slippage_bps;
        let quote = retry_with_backoff(
            || {
                let jupiter = self.jupiter.clone();
                let mint = mint.to_string();
                async move {
                    jupiter
                        .get_quote(&mint, wsol_mint, sell_amount, default_slippage)
                        .await
                }
            },
            3,
        )
        .await
        .context("Failed to get Jupiter sell quote")?;

        // 2. Get swap instructions
        let swap_ixs = self
            .jupiter
            .get_swap_instructions(&quote, &taker)
            .await
            .context("Failed to get Jupiter swap instructions for sell")?;

        // 3. Build transaction from instructions
        let mut instructions = Vec::new();
        for ix in &swap_ixs.compute_budget_instructions {
            instructions.push(to_solana_instruction(ix)?);
        }
        for ix in &swap_ixs.setup_instructions {
            instructions.push(to_solana_instruction(ix)?);
        }
        instructions.push(to_solana_instruction(&swap_ixs.swap_instruction)?);
        if let Some(cleanup) = &swap_ixs.cleanup_instruction {
            instructions.push(to_solana_instruction(cleanup)?);
        }
        if let Some(other) = &swap_ixs.other_instructions {
            for ix in other {
                instructions.push(to_solana_instruction(ix)?);
            }
        }

        let recent_blockhash = self
            .rpc
            .get_latest_blockhash()
            .context("Failed to get recent blockhash for sell")?;

        let swap_tx = Transaction::new_signed_with_payer(
            &instructions,
            Some(&wallet.pubkey()),
            &[wallet],
            recent_blockhash,
        );

        // 4. Submit via Jito bundle
        let bundle_id = self
            .jito
            .submit_bundle(
                vec![swap_tx],
                self.config.jito_tip_lamports,
                wallet,
                recent_blockhash,
            )
            .await
            .context("Failed to submit sell bundle to Jito")?;

        // 5. Confirm the bundle landed
        let confirmation = self
            .jito
            .confirm_bundle(&bundle_id, 60)
            .await
            .context("Failed to confirm sell bundle")?;

        if !confirmation.is_landed() {
            anyhow::bail!(
                "Sell bundle did not land for mint {} (bundle_id: {})",
                mint,
                bundle_id
            );
        }

        let signature = bundle_id.clone();

        info!(
            bundle_id = %bundle_id,
            expected_sol = %quote.out_amount,
            "Sell confirmed via Jito bundle"
        );

        // Update position tracking
        if amount_pct >= 100 {
            self.positions.close_position(mint, &signature)?;
        } else {
            self.positions.reduce_position(mint, amount_pct, &signature)?;
        }

        Ok(signature)
    }

}
