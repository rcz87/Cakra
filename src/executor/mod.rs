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
use chrono::Utc;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::Transaction;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::config::Config;
use crate::db::{self, DbPool};
use crate::models::token::TokenInfo;
use crate::models::trade::{Trade, TradeType, TradeStatus};
use crate::risk::{RiskManager, CooldownManager, ListManager, RiskCheck};

use self::jito::JitoClient;
use self::jupiter::{JupiterClient, to_solana_instruction};
use self::positions::PositionManager;
use self::retry::retry_with_backoff;
use self::router::{find_best_route, RouteType};

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
        positions: PositionManager,
    ) -> Self {
        let rpc = Arc::new(RpcClient::new(config.effective_rpc_url().to_string()));
        let jito = JitoClient::new(&config.jito_block_engine_url);
        let jupiter = JupiterClient::new(&config.jupiter_api_url, &config.jupiter_api_key);

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
        let taker = wallet.pubkey().to_string();

        // 1. Find best route (Jupiter aggregator or PumpFun direct for new tokens)
        let route = find_best_route(
            token,
            amount_lamports,
            slippage_bps,
            &self.jupiter,
            &self.rpc,
            &self.config,
        )
        .await
        .context("No viable route found for buy")?;

        info!(
            mint = %token.mint,
            route = ?route.route_type,
            expected_output = route.expected_output,
            price_impact = route.price_impact_pct,
            "Route selected for buy"
        );

        let expected_output = route.expected_output;

        // 2. Build instructions based on route type
        let mut instructions = Vec::new();

        match route.route_type {
            RouteType::PumpFun => {
                // Direct PumpFun bonding curve buy — fastest path for new tokens
                let mint_pubkey: Pubkey = token.mint.parse()
                    .context("Invalid mint pubkey")?;

                // Create buyer ATA if needed
                let buyer_ata = pumpfun_buy::derive_buyer_token_account(
                    &wallet.pubkey(), &mint_pubkey,
                )?;
                let token_program: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
                    .parse().unwrap();
                let _ata_program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
                    .parse().unwrap();

                // Create ATA instruction (idempotent — will succeed even if ATA exists)
                instructions.push(
                    spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                        &wallet.pubkey(),
                        &wallet.pubkey(),
                        &mint_pubkey,
                        &token_program,
                    )
                );

                // PumpFun buy instruction
                instructions.push(
                    pumpfun_buy::build_pumpfun_buy(
                        &mint_pubkey,
                        amount_lamports,
                        slippage_bps,
                        &wallet.pubkey(),
                    )?
                );

                info!(
                    mint = %token.mint,
                    buyer_ata = %buyer_ata,
                    "Built PumpFun direct buy instructions"
                );
            }
            RouteType::Jupiter => {
                // Jupiter aggregated route — broader coverage, slightly slower
                let wsol_mint = "So11111111111111111111111111111111111111112";
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

                info!(
                    slippage_bps = slippage_bps,
                    price_impact = %quote.price_impact_pct,
                    "Jupiter quote received for buy"
                );

                let swap_ixs = self
                    .jupiter
                    .get_swap_instructions(&quote, &taker)
                    .await
                    .context("Failed to get Jupiter swap instructions")?;

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
            }
        }

        // Fetch a fresh blockhash right before signing to avoid stale blockhash
        // if the quote + swap-instructions steps took too long.
        let recent_blockhash = self
            .rpc
            .get_latest_blockhash()
            .context("Failed to get recent blockhash")?;
        let blockhash_fetched = std::time::Instant::now();

        let swap_tx = Transaction::new_signed_with_payer(
            &instructions,
            Some(&wallet.pubkey()),
            &[wallet],
            recent_blockhash,
        );

        // 4. Submit via Jito bundle for MEV protection
        let blockhash_age = blockhash_fetched.elapsed();
        if blockhash_age.as_secs() >= 60 {
            anyhow::bail!(
                "Blockhash became stale before bundle submission (age: {}s). Aborting buy.",
                blockhash_age.as_secs()
            );
        }

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

        // Extract real transaction signature — refuse bundle ID as fallback
        let signature = match confirmation.swap_signature() {
            Some(sig) => sig.to_string(),
            None => {
                error!(
                    mint = %token.mint,
                    bundle_id = %bundle_id,
                    "Buy bundle landed but transaction signature unavailable. \
                     Bundle ID is NOT a valid tx signature — aborting position tracking."
                );
                anyhow::bail!(
                    "Buy bundle landed but no transaction signature returned (bundle_id: {}). \
                     Funds may have been spent — check wallet manually.",
                    bundle_id
                );
            }
        };

        // 6. Verify actual on-chain token balance with retry
        let mint_pubkey: Pubkey = token.mint.parse().context("Invalid mint")?;
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

        // Retry balance check 3x with exponential backoff (1s, 2s, 4s)
        let mut actual_output: Option<u64> = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                let delay = std::time::Duration::from_secs(1 << attempt);
                warn!(
                    mint = %token.mint,
                    attempt = attempt + 1,
                    delay_secs = delay.as_secs(),
                    "Retrying on-chain balance check..."
                );
                tokio::time::sleep(delay).await;
            }
            match self.rpc.get_token_account_balance(&ata) {
                Ok(balance) => {
                    let bal: u64 = balance.amount.parse().unwrap_or(0);
                    if bal > 0 {
                        actual_output = Some(bal);
                        break;
                    }
                    // bal == 0 means ATA exists but empty — retry in case of propagation delay
                    warn!(
                        mint = %token.mint,
                        attempt = attempt + 1,
                        "ATA balance is 0, might be propagation delay"
                    );
                }
                Err(e) => {
                    warn!(
                        mint = %token.mint,
                        attempt = attempt + 1,
                        error = %e,
                        "RPC balance check failed"
                    );
                }
            }
        }

        let actual_output = match actual_output {
            Some(bal) => bal,
            None => {
                error!(
                    mint = %token.mint,
                    signature = %signature,
                    expected_output = expected_output,
                    "Failed to verify on-chain balance after 3 retries. \
                     Refusing to open position with unverified balance. \
                     Tx signature: {} — check manually.",
                    signature
                );
                anyhow::bail!(
                    "Buy tx {} confirmed but on-chain balance could not be verified after 3 retries. \
                     Check wallet and token balance manually.",
                    signature
                );
            }
        };

        // Slippage reality check — warn if actual output much worse than quoted
        let slippage_actual_pct = if expected_output > 0 {
            ((expected_output as f64 - actual_output as f64) / expected_output as f64) * 100.0
        } else {
            0.0
        };
        if slippage_actual_pct > slippage_bps as f64 / 100.0 {
            warn!(
                mint = %token.mint,
                signature = %signature,
                expected = expected_output,
                actual = actual_output,
                slippage_pct = format!("{:.2}", slippage_actual_pct),
                slippage_limit = slippage_bps as f64 / 100.0,
                "Actual slippage EXCEEDED configured limit — got fewer tokens than expected"
            );
        }

        info!(
            signature = %signature,
            expected_output = expected_output,
            actual_output = actual_output,
            slippage_pct = format!("{:.2}", slippage_actual_pct),
            "Buy confirmed — balance verified on-chain"
        );

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
            0, // security_score: caller can override when available
        )?;

        // Record trade in DB as Confirmed (bundle already landed)
        let trade = Trade {
            id: Uuid::new_v4().to_string(),
            token_mint: token.mint.clone(),
            token_symbol: token.symbol.clone(),
            trade_type: TradeType::Buy,
            amount_sol,
            amount_tokens: actual_output as f64,
            price_per_token: entry_price,
            slippage_bps: slippage_bps,
            tx_signature: Some(signature.clone()),
            status: TradeStatus::Confirmed,
            wallet_pubkey: taker.clone(),
            created_at: Utc::now(),
            confirmed_at: Some(Utc::now()),
            pnl_sol: None,
            security_score: None,
        };
        if let Err(e) = db::queries::insert_trade(&self.db, &trade) {
            warn!(error = %e, "Failed to record buy trade in DB");
        }

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

        // Fetch a fresh blockhash right before signing to avoid stale blockhash
        // if the quote + swap-instructions steps took too long.
        let recent_blockhash = self
            .rpc
            .get_latest_blockhash()
            .context("Failed to get recent blockhash for sell")?;
        let blockhash_fetched = std::time::Instant::now();

        let swap_tx = Transaction::new_signed_with_payer(
            &instructions,
            Some(&wallet.pubkey()),
            &[wallet],
            recent_blockhash,
        );

        // 4. Submit via Jito bundle
        let blockhash_age = blockhash_fetched.elapsed();
        if blockhash_age.as_secs() >= 60 {
            anyhow::bail!(
                "Blockhash became stale before sell bundle submission (age: {}s). Aborting sell.",
                blockhash_age.as_secs()
            );
        }

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

        // Extract real transaction signature — refuse bundle ID as fallback
        let signature = match confirmation.swap_signature() {
            Some(sig) => sig.to_string(),
            None => {
                error!(
                    mint = %mint,
                    bundle_id = %bundle_id,
                    "Sell bundle landed but transaction signature unavailable. \
                     Cannot verify sell outcome."
                );
                anyhow::bail!(
                    "Sell bundle landed but no transaction signature returned (bundle_id: {}). \
                     Check wallet balance manually.",
                    bundle_id
                );
            }
        };

        // 6. Verify sell outcome via balance check + signature polling fallback
        let sell_verified = self.verify_sell_outcome(
            mint, &signature, &ata, total_amount,
        ).await;

        let (post_balance, actual_sold, actual_sell_pct) = match sell_verified {
            SellOutcome::BalanceReduced { post_balance, actual_sold, actual_sell_pct } => {
                info!(
                    signature = %signature,
                    expected_sol = %quote.out_amount,
                    pre_balance = total_amount,
                    post_balance = post_balance,
                    actual_sold = actual_sold,
                    "Sell confirmed — balance verified on-chain"
                );
                (post_balance, actual_sold, actual_sell_pct)
            }
            SellOutcome::SignatureConfirmed => {
                // Tx confirmed on-chain but balance check failed/ambiguous.
                // Assume full sell since we can't determine partial.
                warn!(
                    mint = %mint,
                    signature = %signature,
                    "Sell tx confirmed on-chain but balance check failed. \
                     Assuming full sell."
                );
                (0, total_amount, 100)
            }
            SellOutcome::Failed(reason) => {
                error!(
                    mint = %mint,
                    signature = %signature,
                    reason = %reason,
                    "Sell verification failed"
                );
                anyhow::bail!(
                    "Sell verification failed for mint {} (sig: {}): {}",
                    mint, signature, reason
                );
            }
        };

        // Update position tracking based on actual sell
        if post_balance == 0 {
            self.positions.close_position(mint, &signature)?;
        } else {
            self.positions.reduce_position(mint, actual_sell_pct, &signature)?;
        }

        // Record sell trade in DB as Confirmed
        let sol_received: f64 = quote.out_amount.parse().unwrap_or(0.0) / 1_000_000_000.0;
        let sell_price = if actual_sold > 0 {
            sol_received / (actual_sold as f64)
        } else {
            0.0
        };
        let trade = Trade {
            id: Uuid::new_v4().to_string(),
            token_mint: mint.to_string(),
            token_symbol: mint.to_string(), // symbol not available here
            trade_type: TradeType::Sell,
            amount_sol: sol_received,
            amount_tokens: actual_sold as f64,
            price_per_token: sell_price,
            slippage_bps: default_slippage,
            tx_signature: Some(signature.clone()),
            status: TradeStatus::Confirmed,
            wallet_pubkey: taker.clone(),
            created_at: Utc::now(),
            confirmed_at: Some(Utc::now()),
            pnl_sol: None, // PnL calculated from position close
            security_score: None,
        };
        if let Err(e) = db::queries::insert_trade(&self.db, &trade) {
            warn!(error = %e, "Failed to record sell trade in DB");
        }

        Ok(signature)
    }

    /// Verify sell outcome: try balance check first, fall back to signature polling.
    ///
    /// Strategy:
    /// 1. Try balance check 3x (1s, 2s, 4s backoff)
    /// 2. If balance reduced → confirmed via balance
    /// 3. If balance unchanged OR RPC errors → poll signature status for 30s
    /// 4. If signature confirmed on-chain → sell landed (assume full sell)
    /// 5. If not confirmed after 30s → truly failed
    async fn verify_sell_outcome(
        &self,
        mint: &str,
        signature: &str,
        ata: &Pubkey,
        pre_balance: u64,
    ) -> SellOutcome {
        // Phase 1: Try balance check 3x with exponential backoff
        for attempt in 0..3u32 {
            if attempt > 0 {
                let delay = std::time::Duration::from_secs(1 << attempt);
                tokio::time::sleep(delay).await;
            }
            match self.rpc.get_token_account_balance(ata) {
                Ok(b) => {
                    let post = b.amount.parse::<u64>().unwrap_or(0);
                    if post < pre_balance {
                        let actual_sold = pre_balance - post;
                        let pct = ((actual_sold as f64 / pre_balance as f64) * 100.0) as u8;
                        return SellOutcome::BalanceReduced {
                            post_balance: post,
                            actual_sold,
                            actual_sell_pct: pct,
                        };
                    }
                    // Balance not reduced — might be propagation delay, keep trying
                    if attempt < 2 {
                        warn!(
                            mint = %mint,
                            attempt = attempt + 1,
                            pre = pre_balance,
                            post = post,
                            "Balance not yet reduced, retrying..."
                        );
                    }
                }
                Err(e) => {
                    // RPC error — ATA might be closed (full sell) or network issue
                    warn!(
                        mint = %mint,
                        attempt = attempt + 1,
                        error = %e,
                        "Post-sell balance check failed"
                    );
                }
            }
        }

        // Phase 2: Balance check inconclusive — poll signature status for 30s
        warn!(
            mint = %mint,
            signature = %signature,
            "Balance check inconclusive after 3 retries. \
             Falling back to signature status polling (30s)..."
        );

        let sig = match signature.parse::<Signature>() {
            Ok(s) => s,
            Err(e) => {
                return SellOutcome::Failed(
                    format!("Invalid signature '{}': {}", signature, e),
                );
            }
        };

        let poll_start = std::time::Instant::now();
        let poll_timeout = std::time::Duration::from_secs(30);

        while poll_start.elapsed() < poll_timeout {
            match self.rpc.get_signature_status(&sig) {
                Ok(Some(result)) => {
                    match result {
                        Ok(()) => {
                            info!(
                                mint = %mint,
                                signature = %signature,
                                elapsed_ms = poll_start.elapsed().as_millis(),
                                "Sell tx confirmed via signature polling"
                            );
                            // Tx confirmed — try one more balance check
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            if let Ok(b) = self.rpc.get_token_account_balance(ata) {
                                let post = b.amount.parse::<u64>().unwrap_or(0);
                                if post < pre_balance {
                                    let actual_sold = pre_balance - post;
                                    let pct = ((actual_sold as f64 / pre_balance as f64) * 100.0) as u8;
                                    return SellOutcome::BalanceReduced {
                                        post_balance: post,
                                        actual_sold,
                                        actual_sell_pct: pct,
                                    };
                                }
                            }
                            // Balance still not reduced or RPC failed — tx confirmed anyway
                            return SellOutcome::SignatureConfirmed;
                        }
                        Err(e) => {
                            // Tx landed but failed on-chain (e.g. slippage exceeded)
                            return SellOutcome::Failed(
                                format!("Sell tx failed on-chain: {:?}", e),
                            );
                        }
                    }
                }
                Ok(None) => {
                    // Not yet processed — keep polling
                }
                Err(e) => {
                    warn!(
                        signature = %signature,
                        error = %e,
                        "Signature status poll RPC error"
                    );
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        // 30s exhausted — truly unknown
        SellOutcome::Failed(format!(
            "Sell tx {} not confirmed after 30s polling. \
             Sell may or may not have landed — check wallet manually.",
            signature
        ))
    }
}

/// Result of sell verification.
enum SellOutcome {
    /// Balance check confirmed the sell reduced token balance.
    BalanceReduced {
        post_balance: u64,
        actual_sold: u64,
        actual_sell_pct: u8,
    },
    /// Balance check failed but signature confirmed on-chain.
    /// Assume full sell since we can't determine partial amount.
    SignatureConfirmed,
    /// Sell could not be verified.
    Failed(String),
}
