pub mod helius_sender;
pub mod jito;
pub mod jupiter;
pub mod positions;
pub mod price_feed;
pub mod priority;
pub mod pumpfun_buy;
pub mod pumpportal_trade;
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

use self::helius_sender::HeliusSender;
use self::jito::JitoClient;
use self::jupiter::{JupiterClient, to_solana_instruction};
use self::positions::PositionManager;
use self::pumpportal_trade::PumpPortalTradeClient;
use self::retry::retry_with_backoff;
use self::router::{find_best_route, RouteType};

/// Central executor service that coordinates all trading operations
/// for the RICOZ SNIPER bot.
pub struct ExecutorService {
    pub config: Arc<Config>,
    pub rpc: Arc<RpcClient>,
    pub sender: HeliusSender,
    pub jito: JitoClient,
    pub jupiter: JupiterClient,
    pub pumpportal: PumpPortalTradeClient,
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
        let rpc_url = config.effective_rpc_url().to_string();
        let rpc = Arc::new(RpcClient::new(rpc_url.clone()));
        let sender = HeliusSender::new(&rpc_url);
        let jito = JitoClient::new(&config.jito_block_engine_url);
        let jupiter = JupiterClient::new(&config.jupiter_api_url, &config.jupiter_api_key);
        let pumpportal = PumpPortalTradeClient::new();

        Self {
            config,
            rpc,
            sender,
            jito,
            jupiter,
            pumpportal,
            positions,
            db,
            risk,
            cooldown,
            lists,
        }
    }

    /// Read wallet SOL balance (in SOL, not lamports). Used as a snapshot
    /// at buy time to compute realized PnL on close.
    fn read_wallet_sol_balance(&self, wallet: &Pubkey) -> f64 {
        match self.rpc.get_balance(wallet) {
            Ok(lamports) => lamports as f64 / 1_000_000_000.0,
            Err(e) => {
                warn!(error = %e, "Failed to read wallet SOL balance, returning 0");
                0.0
            }
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

        // Snapshot wallet SOL balance BEFORE any spending — used to compute
        // realized PnL on close (truth-from-balance, not spot-price fantasy).
        let wallet_sol_at_open = self.read_wallet_sol_balance(&wallet.pubkey());
        info!(
            mint = %token.mint,
            wallet_sol_at_open,
            "Captured wallet snapshot for realized PnL tracking"
        );

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

        // 2. Build and execute based on route type
        //
        // PumpPortalDirect: PumpPortal builds the entire tx, we sign and Jito-submit.
        // PumpFun/Jupiter: we build instructions, sign tx, and Jito-submit ourselves.

        if route.route_type == RouteType::PumpPortalDirect {
            // === PumpPortal Direct path: fastest for PumpFun tokens ===
            let jito_tip_sol = self.config.jito_tip_lamports as f64 / 1_000_000_000.0;

            // Determine pool type
            let pool = match token.source {
                crate::models::token::TokenSource::PumpFun => "pump",
                crate::models::token::TokenSource::PumpSwap => "pump-amm",
                _ => "auto",
            };

            let signature = self
                .pumpportal
                .execute_buy(
                    &token.mint,
                    amount_sol,
                    slippage_bps,
                    jito_tip_sol,
                    wallet,
                    pool,
                )
                .await
                .context("PumpPortal buy execution failed")?;

            info!(
                mint = %token.mint,
                signature = %signature,
                route = "PumpPortalDirect",
                "Buy executed via PumpPortal + Jito"
            );

            // Record position (same as existing flow)
            let actual_output = self.verify_buy_balance(
                wallet, &token.mint, expected_output
            ).await.unwrap_or(expected_output);

            // CRITICAL: entry_price is per BASE UNIT to match PriceFeed.
            // Previous bug: divided by 1_000_000 (whole token assumption) which made
            // PnL calculations 10^6 off and triggered phantom -99% losses.
            let entry_price = if actual_output > 0 {
                amount_sol / actual_output as f64
            } else {
                0.0
            };

            // Source-aware metadata for price feed and exit routing
            let token_source_str = format!("{:?}", token.source);
            let price_source = match token.source {
                crate::models::token::TokenSource::PumpFun => Some("PumpFunBondingCurve".to_string()),
                crate::models::token::TokenSource::PumpSwap => Some("RaydiumPool".to_string()),
                _ => Some("Jupiter".to_string()),
            };

            self.positions.open_position(
                &token.mint,
                &token.symbol,
                &taker,
                entry_price,
                amount_sol,
                actual_output as f64,
                slippage_bps,
                &signature,
                0,
                &token_source_str,
                token.pool_address.clone(),
                token.decimals,
                price_source,
                wallet_sol_at_open,
            )?;

            self.cooldown.record_trade(&taker);

            let trade = Trade {
                id: Uuid::new_v4().to_string(),
                token_mint: token.mint.clone(),
                token_symbol: token.symbol.clone(),
                trade_type: TradeType::Buy,
                amount_sol,
                amount_tokens: actual_output as f64,
                price_per_token: entry_price,
                slippage_bps,
                tx_signature: Some(signature.clone()),
                status: TradeStatus::Confirmed,
                wallet_pubkey: taker.clone(),
                created_at: Utc::now(),
                confirmed_at: Some(Utc::now()),
                pnl_sol: None,
                security_score: None,
            };

            if let Err(e) = db::queries::insert_trade(&self.db, &trade) {
                warn!(error = %e, "Failed to record PumpPortal trade in DB");
            }

            return Ok(signature);
        }

        // === Legacy instruction-building path (PumpFun direct / Jupiter) ===
        let mut instructions = Vec::new();

        match route.route_type {
            RouteType::PumpPortalDirect => {
                unreachable!("PumpPortalDirect handled above with early return");
            }
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
            RouteType::RaydiumDirect => {
                // Sprint 3b: Direct Raydium CPMM swap (gated by ENABLE_RAYDIUM_DIRECT)
                let pool_address = token.pool_address.as_ref()
                    .context("RaydiumDirect requires pool_address on token")?;

                let meta = raydium::load_pool_meta(&self.rpc, pool_address)
                    .context("Failed to load Raydium pool meta for direct swap")?;

                // Compute minimum output from current reserves + slippage
                let (sol_reserves, token_reserves) = raydium::read_reserves(&self.rpc, &meta)
                    .context("Failed to read pool reserves")?;
                let expected_token_out = raydium::quote_buy_exact_in(
                    amount_lamports, sol_reserves, token_reserves
                );
                let min_token_out = (expected_token_out as u128
                    * (10_000 - slippage_bps as u128) / 10_000) as u64;

                info!(
                    mint = %token.mint,
                    pool = %meta.pool,
                    sol_in = amount_lamports,
                    expected_out = expected_token_out,
                    min_out = min_token_out,
                    "Building RaydiumDirect buy"
                );

                let raydium_ixs = raydium::build_raydium_buy_instructions(
                    &meta, &wallet.pubkey(), amount_lamports, min_token_out,
                ).context("Failed to build Raydium buy instructions")?;

                for ix in raydium_ixs {
                    instructions.push(ix);
                }
            }
        }

        // Add Helius Sender tip to instructions (for dual routing)
        instructions.push(
            HeliusSender::build_tip_instruction(
                &wallet.pubkey(),
                self.config.jito_tip_lamports,
            )?
        );

        // Fetch a fresh blockhash right before signing
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

        let blockhash_age = blockhash_fetched.elapsed();
        if blockhash_age.as_secs() >= 60 {
            anyhow::bail!(
                "Blockhash became stale (age: {}s). Aborting buy.",
                blockhash_age.as_secs()
            );
        }

        // 4. Submit: Helius Sender (default) or Jito Bundle (premium)
        //
        // Routing logic:
        //   single tx + normal score → Helius Sender (fast, free, dual routing)
        //   multi-tx or high conviction → Jito Bundle (atomic, MEV-aware)
        //
        // For now, all single-tx buys use Sender. Jito stays available for
        // future multi-wallet/multi-step flows.
        let signature = {
            let serialized = bincode::serialize(&swap_tx)
                .context("Failed to serialize swap tx")?;

            info!(
                mint = %token.mint,
                executor = "HeliusSender",
                "Submitting buy via Helius Sender (dual routing)"
            );

            self.sender
                .send_and_confirm(&serialized, 60)
                .await
                .context("Helius Sender buy failed")?
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

        // entry_price per BASE UNIT (consistent with PriceFeed)
        let entry_price = if actual_output > 0 {
            amount_sol / (actual_output as f64)
        } else {
            0.0
        };

        // Source-aware metadata
        let token_source_str = format!("{:?}", token.source);
        let price_source = match token.source {
            crate::models::token::TokenSource::PumpFun => Some("PumpFunBondingCurve".to_string()),
            crate::models::token::TokenSource::PumpSwap => Some("RaydiumPool".to_string()),
            crate::models::token::TokenSource::Raydium => Some("RaydiumPool".to_string()),
            _ => Some("Jupiter".to_string()),
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
            &token_source_str,
            token.pool_address.clone(),
            token.decimals,
            price_source,
            wallet_sol_at_open,
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

    /// Get token balance for a wallet, works for both Token and Token-2022 programs.
    /// Uses RPC getTokenAccountsByOwner with jsonParsed encoding.
    fn get_token_balance_for_mint(
        &self,
        wallet_pubkey: &Pubkey,
        mint_str: &str,
    ) -> Result<u64> {
        use solana_client::rpc_request::TokenAccountsFilter;

        let mint_pubkey: Pubkey = mint_str.parse().context("Invalid mint")?;

        let accounts = self
            .rpc
            .get_token_accounts_by_owner(
                wallet_pubkey,
                TokenAccountsFilter::Mint(mint_pubkey),
            )
            .context("Failed to get token accounts by owner")?;

        if accounts.is_empty() {
            return Ok(0);
        }

        // Parse balance from the keyed account response
        let mut total: u64 = 0;
        for account in &accounts {
            // The account data is jsonParsed by default from get_token_accounts_by_owner
            let data_str = serde_json::to_string(&account.account.data)
                .unwrap_or_default();
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&data_str) {
                if let Some(amount_str) = parsed["parsed"]["info"]["tokenAmount"]["amount"].as_str() {
                    total += amount_str.parse::<u64>().unwrap_or(0);
                }
            }
        }

        Ok(total)
    }

    /// Verify on-chain token balance after a buy, with retries.
    async fn verify_buy_balance(
        &self,
        wallet: &Keypair,
        mint_str: &str,
        expected_output: u64,
    ) -> Result<u64> {
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
            }
            match self.get_token_balance_for_mint(&wallet.pubkey(), mint_str) {
                Ok(bal) if bal > 0 => return Ok(bal),
                Ok(_) => {
                    warn!(mint = %mint_str, attempt = attempt + 1, "Balance is 0, retrying");
                }
                Err(e) => {
                    warn!(mint = %mint_str, attempt = attempt + 1, error = %e, "Balance check retry");
                }
            }
        }

        Ok(expected_output)
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

        // Get token balance to calculate sell amount (works for Token + Token-2022)
        let total_amount = self
            .get_token_balance_for_mint(&wallet.pubkey(), mint)
            .context("Failed to get token balance for sell")?;

        if total_amount == 0 {
            anyhow::bail!("Token balance is zero for mint {mint}");
        }

        let sell_amount = (total_amount as u128 * amount_pct as u128 / 100) as u64;
        if sell_amount == 0 {
            anyhow::bail!("Calculated sell amount is zero");
        }

        // ── Source-aware sell routing (Sprint 3a + 3b) ──────────────
        // Look up the position to learn its source.
        let pos = self
            .positions
            .get_open_positions()
            .into_iter()
            .find(|p| p.token_mint == mint);
        let pos_source = pos.as_ref().map(|p| p.token_source.clone());
        let pos_pool = pos.as_ref().and_then(|p| p.pool_address.clone());

        let use_pumpportal = matches!(
            pos_source.as_deref(),
            Some("PumpFun") | Some("PumpSwap")
        );

        if use_pumpportal {
            info!(
                mint = %mint,
                source = ?pos_source,
                "Sell route: PumpPortal Direct (source-aware)"
            );
            return self.execute_sell_via_pumpportal(
                mint, amount_pct, wallet, pos_source.as_deref().unwrap_or("PumpFun"),
            ).await;
        }

        // Sprint 3b: Try Raydium direct sell if enabled, source is Raydium, and pool known
        if self.config.enable_raydium_direct
            && matches!(pos_source.as_deref(), Some("Raydium"))
            && pos_pool.is_some()
        {
            info!(
                mint = %mint,
                pool = ?pos_pool,
                "Sell route: RaydiumDirect (source-aware, gated)"
            );
            // Try direct, fall through to Jupiter on error
            match self.execute_sell_via_raydium_direct(
                mint, amount_pct, wallet, pos_pool.as_deref().unwrap(), sell_amount,
            ).await {
                Ok(sig) => return Ok(sig),
                Err(e) => {
                    warn!(
                        mint = %mint,
                        error = %e,
                        "RaydiumDirect sell failed, falling back to Jupiter"
                    );
                }
            }
        }

        info!(
            total_balance = total_amount,
            sell_pct = amount_pct,
            sell_amount = sell_amount,
            source = ?pos_source,
            "Sell route: Jupiter (source-aware)"
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

        // Add Helius Sender tip
        instructions.push(
            HeliusSender::build_tip_instruction(
                &wallet.pubkey(),
                self.config.jito_tip_lamports,
            )?
        );

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

        let blockhash_age = blockhash_fetched.elapsed();
        if blockhash_age.as_secs() >= 60 {
            anyhow::bail!(
                "Blockhash became stale (age: {}s). Aborting sell.",
                blockhash_age.as_secs()
            );
        }

        // 4. Submit sell via Helius Sender (default single-tx path)
        let signature = {
            let serialized = bincode::serialize(&swap_tx)
                .context("Failed to serialize sell tx")?;

            info!(
                mint = %mint,
                executor = "HeliusSender",
                "Submitting sell via Helius Sender"
            );

            self.sender
                .send_and_confirm(&serialized, 60)
                .await
                .context("Helius Sender sell failed")?
        };

        // 6. Verify sell outcome via balance check + signature polling fallback
        let sell_verified = self.verify_sell_outcome(
            mint, &signature, &wallet.pubkey(), total_amount,
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

        // REALIZED PnL: read wallet SOL after sell + verify, compare to wallet_sol_at_open.
        // Must happen BEFORE close_position* so the realized field is persisted.
        let current_wallet = self.read_wallet_sol_balance(&wallet.pubkey());
        if let Err(e) = self.positions.update_pnl_realized(mint, current_wallet) {
            warn!(mint = %mint, error = %e, "Failed to update realized PnL");
        }

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

    /// Sell via PumpPortal Local Trade API (used for PumpFun bonding curve and PumpSwap AMM).
    /// Uses source-aware sell slippage with inline escalation on Custom(6024) errors.
    ///
    /// PumpFun bonding curve volatility means a normal 5% slippage will routinely
    /// hit `Custom(6024)` (TooMuchSolRequired) when meme coins dump 50%+ in seconds.
    /// We start at 25% / 15% by source, then escalate by +500 bps per retry up to
    /// 3500 bps cap. Only escalates on the 6024 error — other failures bail fast.
    async fn execute_sell_via_pumpportal(
        &self,
        mint: &str,
        amount_pct: u8,
        wallet: &Keypair,
        token_source: &str,
    ) -> Result<String> {
        let pre_balance = self
            .get_token_balance_for_mint(&wallet.pubkey(), mint)
            .context("Failed to get token balance for PumpPortal sell")?;

        if pre_balance == 0 {
            anyhow::bail!("Token balance is zero for mint {mint}");
        }

        let pool = match token_source {
            "PumpFun" => "pump",
            "PumpSwap" => "pump-amm",
            _ => "auto",
        };

        // Source-aware base sell slippage (much wider than buy)
        let base_slippage_bps: u16 = match token_source {
            "PumpFun" => 2500,   // 25% — bonding curve dump volatility
            "PumpSwap" => 1500,  // 15% — migrated AMM, slightly less wild
            _ => self.config.default_slippage_bps,
        };
        const SLIPPAGE_ESCALATION_BPS: u16 = 500;
        const SLIPPAGE_HARD_CAP_BPS: u16 = 3500;
        const MAX_ESCALATION_ATTEMPTS: u32 = 3;

        let jito_tip_sol = self.config.jito_tip_lamports as f64 / 1_000_000_000.0;
        let mut current_slippage = base_slippage_bps;
        let mut signature = String::new();
        let mut last_error: Option<String> = None;

        for attempt in 0..MAX_ESCALATION_ATTEMPTS {
            info!(
                mint = %mint,
                attempt = attempt + 1,
                slippage_bps = current_slippage,
                "PumpPortal sell attempt"
            );

            match self
                .pumpportal
                .execute_sell(mint, amount_pct, current_slippage, jito_tip_sol, wallet, pool)
                .await
            {
                Ok(sig) => {
                    signature = sig;
                    break;
                }
                Err(e) => {
                    let err_str = format!("{e:?}");
                    let is_slippage_error = err_str.contains("6024")
                        || err_str.contains("TooMuchSolRequired")
                        || err_str.contains("SlippageExceeded");

                    last_error = Some(err_str.clone());

                    if !is_slippage_error {
                        warn!(
                            mint = %mint,
                            error = %err_str,
                            "PumpPortal sell failed (non-slippage error), bailing"
                        );
                        anyhow::bail!("PumpPortal sell execution failed: {e}");
                    }

                    // Escalate slippage and retry
                    let next_slippage = current_slippage.saturating_add(SLIPPAGE_ESCALATION_BPS);
                    if next_slippage > SLIPPAGE_HARD_CAP_BPS {
                        warn!(
                            mint = %mint,
                            slippage_cap = SLIPPAGE_HARD_CAP_BPS,
                            "Slippage hard cap reached, no further retries"
                        );
                        break;
                    }
                    warn!(
                        mint = %mint,
                        from_slippage = current_slippage,
                        to_slippage = next_slippage,
                        "Slippage error (6024) — escalating slippage and retrying"
                    );
                    current_slippage = next_slippage;
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
            }
        }

        if signature.is_empty() {
            anyhow::bail!(
                "PumpPortal sell failed after {} escalations (max slippage {} bps): {}",
                MAX_ESCALATION_ATTEMPTS,
                current_slippage,
                last_error.unwrap_or_else(|| "unknown".to_string())
            );
        }

        info!(
            mint = %mint,
            signature = %signature,
            pool = %pool,
            final_slippage_bps = current_slippage,
            "PumpPortal sell submitted"
        );

        // Verify outcome with the same path as Jupiter sell
        let outcome = self
            .verify_sell_outcome(mint, &signature, &wallet.pubkey(), pre_balance)
            .await;

        let (post_balance, _actual_sold, actual_sell_pct) = match outcome {
            SellOutcome::BalanceReduced { post_balance, actual_sold, actual_sell_pct } => {
                info!(
                    mint = %mint,
                    pre_balance,
                    post_balance,
                    actual_sold,
                    "PumpPortal sell verified"
                );
                (post_balance, actual_sold, actual_sell_pct)
            }
            SellOutcome::SignatureConfirmed => {
                warn!(
                    mint = %mint,
                    signature = %signature,
                    "PumpPortal sell tx confirmed but balance check ambiguous, assuming full"
                );
                (0, pre_balance, 100)
            }
            SellOutcome::Failed(reason) => {
                anyhow::bail!(
                    "PumpPortal sell verification failed for {} (sig: {}): {}",
                    mint, signature, reason
                );
            }
        };

        // REALIZED PnL update from wallet delta (PumpPortal path)
        let current_wallet = self.read_wallet_sol_balance(&wallet.pubkey());
        if let Err(e) = self.positions.update_pnl_realized(mint, current_wallet) {
            warn!(mint = %mint, error = %e, "Failed to update realized PnL (PumpPortal)");
        }

        // Update position bookkeeping
        if post_balance == 0 {
            self.positions.close_position(mint, &signature)?;
        } else {
            self.positions.reduce_position(mint, actual_sell_pct, &signature)?;
        }

        // Record sell trade
        let trade = Trade {
            id: Uuid::new_v4().to_string(),
            token_mint: mint.to_string(),
            token_symbol: mint.to_string(),
            trade_type: TradeType::Sell,
            amount_sol: 0.0, // PumpPortal doesn't return SOL out in advance
            amount_tokens: pre_balance.saturating_sub(post_balance) as f64,
            price_per_token: 0.0,
            slippage_bps: self.config.default_slippage_bps,
            tx_signature: Some(signature.clone()),
            status: TradeStatus::Confirmed,
            wallet_pubkey: wallet.pubkey().to_string(),
            created_at: Utc::now(),
            confirmed_at: Some(Utc::now()),
            pnl_sol: None,
            security_score: None,
        };
        if let Err(e) = db::queries::insert_trade(&self.db, &trade) {
            warn!(error = %e, "Failed to record PumpPortal sell trade");
        }

        Ok(signature)
    }

    /// Sprint 3b: Sell via direct Raydium CPMM swap.
    /// Builds the instruction sequence locally and submits via Helius Sender.
    /// Falls through to caller (Jupiter fallback) on any error.
    async fn execute_sell_via_raydium_direct(
        &self,
        mint: &str,
        amount_pct: u8,
        wallet: &Keypair,
        pool_address: &str,
        sell_amount: u64,
    ) -> Result<String> {
        let pre_balance = self
            .get_token_balance_for_mint(&wallet.pubkey(), mint)
            .context("Failed to get token balance for RaydiumDirect sell")?;

        if pre_balance == 0 {
            anyhow::bail!("Token balance is zero");
        }

        let meta = raydium::load_pool_meta(&self.rpc, pool_address)
            .context("Failed to load Raydium pool meta")?;

        // Read reserves to compute minimum SOL out with slippage
        let (sol_reserves, token_reserves) = raydium::read_reserves(&self.rpc, &meta)?;
        let expected_sol_out = raydium::quote_sell_exact_in(
            sell_amount, sol_reserves, token_reserves
        );
        let slippage_bps = self.config.default_slippage_bps as u128;
        let min_sol_out = (expected_sol_out as u128 * (10_000 - slippage_bps) / 10_000) as u64;

        info!(
            mint = %mint,
            pool = %meta.pool,
            token_in = sell_amount,
            expected_sol = expected_sol_out,
            min_sol = min_sol_out,
            "Building RaydiumDirect sell"
        );

        let mut instructions = raydium::build_raydium_sell_instructions(
            &meta, &wallet.pubkey(), sell_amount, min_sol_out,
        )?;

        // Add Helius Sender tip
        instructions.push(
            HeliusSender::build_tip_instruction(
                &wallet.pubkey(),
                self.config.jito_tip_lamports,
            )?
        );

        let recent_blockhash = self
            .rpc
            .get_latest_blockhash()
            .context("Failed to get blockhash for Raydium sell")?;
        let blockhash_fetched = std::time::Instant::now();

        let swap_tx = Transaction::new_signed_with_payer(
            &instructions,
            Some(&wallet.pubkey()),
            &[wallet],
            recent_blockhash,
        );

        if blockhash_fetched.elapsed().as_secs() >= 60 {
            anyhow::bail!("Blockhash stale for Raydium sell");
        }

        let serialized = bincode::serialize(&swap_tx)
            .context("Failed to serialize Raydium sell tx")?;

        let signature = self
            .sender
            .send_and_confirm(&serialized, 60)
            .await
            .context("Helius Sender Raydium sell failed")?;

        info!(
            mint = %mint,
            signature = %signature,
            "RaydiumDirect sell submitted"
        );

        // Verify outcome
        let outcome = self
            .verify_sell_outcome(mint, &signature, &wallet.pubkey(), pre_balance)
            .await;

        let (post_balance, _actual_sold, actual_sell_pct) = match outcome {
            SellOutcome::BalanceReduced { post_balance, actual_sold, actual_sell_pct } => {
                info!(
                    mint = %mint,
                    pre_balance,
                    post_balance,
                    actual_sold,
                    "RaydiumDirect sell verified"
                );
                (post_balance, actual_sold, actual_sell_pct)
            }
            SellOutcome::SignatureConfirmed => {
                warn!(mint = %mint, signature = %signature, "Sell tx confirmed but balance ambiguous");
                (0, pre_balance, 100)
            }
            SellOutcome::Failed(reason) => {
                anyhow::bail!("RaydiumDirect sell verification failed: {}", reason);
            }
        };

        // REALIZED PnL update from wallet delta (RaydiumDirect path)
        let current_wallet = self.read_wallet_sol_balance(&wallet.pubkey());
        if let Err(e) = self.positions.update_pnl_realized(mint, current_wallet) {
            warn!(mint = %mint, error = %e, "Failed to update realized PnL (RaydiumDirect)");
        }

        if post_balance == 0 {
            self.positions.close_position(mint, &signature)?;
        } else {
            self.positions.reduce_position(mint, actual_sell_pct, &signature)?;
        }

        // Record trade
        let trade = Trade {
            id: Uuid::new_v4().to_string(),
            token_mint: mint.to_string(),
            token_symbol: mint.to_string(),
            trade_type: TradeType::Sell,
            amount_sol: expected_sol_out as f64 / 1_000_000_000.0,
            amount_tokens: pre_balance.saturating_sub(post_balance) as f64,
            price_per_token: 0.0,
            slippage_bps: self.config.default_slippage_bps,
            tx_signature: Some(signature.clone()),
            status: TradeStatus::Confirmed,
            wallet_pubkey: wallet.pubkey().to_string(),
            created_at: Utc::now(),
            confirmed_at: Some(Utc::now()),
            pnl_sol: None,
            security_score: None,
        };
        if let Err(e) = db::queries::insert_trade(&self.db, &trade) {
            warn!(error = %e, "Failed to record RaydiumDirect sell trade");
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
        wallet_pubkey: &Pubkey,
        pre_balance: u64,
    ) -> SellOutcome {
        // Phase 1: Try balance check 3x with exponential backoff
        for attempt in 0..3u32 {
            if attempt > 0 {
                let delay = std::time::Duration::from_secs(1 << attempt);
                tokio::time::sleep(delay).await;
            }
            match self.get_token_balance_for_mint(wallet_pubkey, mint) {
                Ok(post) => {
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
                            if let Ok(post) = self.get_token_balance_for_mint(wallet_pubkey, mint) {
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
