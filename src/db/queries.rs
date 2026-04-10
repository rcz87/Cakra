use anyhow::Result;
use rusqlite::params;

use super::DbPool;
use crate::models::{Position, TokenInfo, Trade, UserSettings};

// ── Wallet Queries ──

pub fn insert_wallet(db: &DbPool, pubkey: &str, encrypted_privkey: &str, label: Option<&str>) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO wallets (pubkey, encrypted_privkey, label) VALUES (?1, ?2, ?3)",
        params![pubkey, encrypted_privkey, label],
    )?;
    Ok(())
}

pub fn get_wallets(db: &DbPool) -> Result<Vec<(i64, String, String, Option<String>, bool)>> {
    let conn = db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id, pubkey, encrypted_privkey, label, is_active FROM wallets ORDER BY id"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get::<_, i32>(4)? != 0,
        ))
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn set_active_wallet(db: &DbPool, wallet_id: i64) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute("UPDATE wallets SET is_active = 0", [])?;
    conn.execute("UPDATE wallets SET is_active = 1 WHERE id = ?1", params![wallet_id])?;
    Ok(())
}

// ── Token Queries ──

pub fn insert_token(db: &DbPool, token: &TokenInfo) -> Result<()> {
    let conn = db.lock().unwrap();
    let source = format!("{}", token.source);
    conn.execute(
        "INSERT OR REPLACE INTO tokens (mint, name, symbol, source, creator, initial_liquidity_sol, initial_liquidity_usd, pool_address, metadata_uri, decimals, detected_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            token.mint,
            token.name,
            token.symbol,
            source,
            token.creator,
            token.initial_liquidity_sol,
            token.initial_liquidity_usd,
            token.pool_address,
            token.metadata_uri,
            token.decimals,
            token.detected_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn update_token_security(db: &DbPool, mint: &str, score: u8, data_json: &str) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute(
        "UPDATE tokens SET security_score = ?1, security_data = ?2, analyzed_at = datetime('now') WHERE mint = ?3",
        params![score, data_json, mint],
    )?;
    Ok(())
}

pub fn is_blacklisted(db: &DbPool, mint: &str) -> Result<bool> {
    let conn = db.lock().unwrap();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM blacklist WHERE mint = ?1",
        params![mint],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

// ── Trade Queries ──

pub fn insert_trade(db: &DbPool, trade: &Trade) -> Result<()> {
    let conn = db.lock().unwrap();
    let trade_type = match trade.trade_type {
        crate::models::trade::TradeType::Buy => "Buy",
        crate::models::trade::TradeType::Sell => "Sell",
    };
    let status = match trade.status {
        crate::models::trade::TradeStatus::Pending => "Pending",
        crate::models::trade::TradeStatus::Submitted => "Submitted",
        crate::models::trade::TradeStatus::Confirmed => "Confirmed",
        crate::models::trade::TradeStatus::Failed => "Failed",
    };
    conn.execute(
        "INSERT INTO trades (id, token_mint, token_symbol, trade_type, amount_sol, amount_tokens, price_per_token, slippage_bps, tx_signature, status, wallet_pubkey, created_at, confirmed_at, pnl_sol, security_score)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            trade.id,
            trade.token_mint,
            trade.token_symbol,
            trade_type,
            trade.amount_sol,
            trade.amount_tokens,
            trade.price_per_token,
            trade.slippage_bps,
            trade.tx_signature,
            status,
            trade.wallet_pubkey,
            trade.created_at.to_rfc3339(),
            trade.confirmed_at.map(|t| t.to_rfc3339()),
            trade.pnl_sol,
            trade.security_score,
        ],
    )?;
    Ok(())
}

pub fn get_recent_trades(db: &DbPool, limit: u32) -> Result<Vec<Trade>> {
    let conn = db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id, token_mint, token_symbol, trade_type, amount_sol, amount_tokens, \
         price_per_token, slippage_bps, tx_signature, status, wallet_pubkey, \
         created_at, confirmed_at, pnl_sol, security_score \
         FROM trades ORDER BY created_at DESC LIMIT ?1"
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        let trade_type_str: String = row.get(3)?;
        let status_str: String = row.get(9)?;
        let created_str: String = row.get(11)?;
        let confirmed_str: Option<String> = row.get(12)?;

        Ok(Trade {
            id: row.get(0)?,
            token_mint: row.get(1)?,
            token_symbol: row.get(2)?,
            trade_type: parse_trade_type(&trade_type_str),
            amount_sol: row.get(4)?,
            amount_tokens: row.get(5)?,
            price_per_token: row.get(6)?,
            slippage_bps: row.get(7)?,
            tx_signature: row.get(8)?,
            status: parse_trade_status(&status_str),
            wallet_pubkey: row.get(10)?,
            created_at: chrono::DateTime::parse_from_rfc3339(&created_str)
                .unwrap_or_default()
                .with_timezone(&chrono::Utc),
            confirmed_at: confirmed_str
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc)),
            pnl_sol: row.get(13)?,
            security_score: row.get(14)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

fn parse_trade_type(s: &str) -> crate::models::trade::TradeType {
    use crate::models::trade::TradeType;
    match s {
        "Buy" => TradeType::Buy,
        "Sell" => TradeType::Sell,
        // Handle JSON-quoted legacy values
        s if s.contains("Buy") => TradeType::Buy,
        _ => TradeType::Sell,
    }
}

fn parse_trade_status(s: &str) -> crate::models::trade::TradeStatus {
    use crate::models::trade::TradeStatus;
    match s {
        "Pending" => TradeStatus::Pending,
        "Submitted" => TradeStatus::Submitted,
        "Confirmed" => TradeStatus::Confirmed,
        "Failed" => TradeStatus::Failed,
        // Handle JSON-quoted legacy values
        s if s.contains("Confirmed") => TradeStatus::Confirmed,
        s if s.contains("Failed") => TradeStatus::Failed,
        s if s.contains("Submitted") => TradeStatus::Submitted,
        _ => TradeStatus::Pending,
    }
}

pub fn get_daily_pnl(db: &DbPool) -> Result<f64> {
    let conn = db.lock().unwrap();
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let pnl: f64 = conn.query_row(
        "SELECT COALESCE(SUM(pnl_sol), 0) FROM trades WHERE created_at >= ?1 AND pnl_sol IS NOT NULL",
        params![today],
        |row| row.get(0),
    )?;
    Ok(pnl)
}

// ── Position Queries ──

pub fn get_open_positions(db: &DbPool) -> Result<Vec<Position>> {
    let conn = db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id, token_mint, token_symbol, wallet_pubkey, entry_price_sol, \
         entry_amount_sol, token_amount, current_price_sol, highest_price_sol, \
         take_profit_pct, stop_loss_pct, trailing_stop_pct, pnl_sol, pnl_pct, \
         status, buy_tx, sell_tx, opened_at, closed_at, security_score, \
         token_source, pool_address, token_decimals, price_source, price_stale, last_price_at, \
         wallet_sol_at_open \
         FROM positions WHERE status = 'Open' ORDER BY opened_at DESC"
    )?;
    let rows = stmt.query_map([], |row| {
        let status_str: String = row.get(14)?;
        let status = parse_position_status(&status_str);
        let opened_str: String = row.get(17)?;
        let closed_str: Option<String> = row.get(18)?;
        let last_price_str: Option<String> = row.get(25)?;

        Ok(Position {
            id: row.get(0)?,
            token_mint: row.get(1)?,
            token_symbol: row.get(2)?,
            wallet_pubkey: row.get(3)?,
            entry_price_sol: row.get(4)?,
            entry_amount_sol: row.get(5)?,
            token_amount: row.get(6)?,
            current_price_sol: row.get(7)?,
            highest_price_sol: row.get(8)?,
            take_profit_pct: row.get(9)?,
            stop_loss_pct: row.get(10)?,
            trailing_stop_pct: row.get(11)?,
            pnl_sol: row.get(12)?,
            pnl_pct: row.get(13)?,
            status,
            buy_tx: row.get(15)?,
            sell_tx: row.get(16)?,
            opened_at: chrono::DateTime::parse_from_rfc3339(&opened_str)
                .unwrap_or_default()
                .with_timezone(&chrono::Utc),
            closed_at: closed_str
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc)),
            security_score: row.get::<_, Option<u8>>(19)?.unwrap_or(0),
            age_secs: 0,
            token_source: row.get(20)?,
            pool_address: row.get(21)?,
            token_decimals: row.get::<_, u32>(22)? as u8,
            price_source: row.get(23)?,
            price_stale: row.get::<_, i32>(24)? != 0,
            last_price_at: last_price_str
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc)),
            wallet_sol_at_open: row.get::<_, Option<f64>>(26)?.unwrap_or(0.0),
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Parse a status string from DB into PositionStatus enum.
fn parse_position_status(s: &str) -> crate::models::position::PositionStatus {
    use crate::models::position::PositionStatus;
    match s {
        "Open" => PositionStatus::Open,
        "ClosedTp" => PositionStatus::ClosedTp,
        "ClosedSl" => PositionStatus::ClosedSl,
        "ClosedManual" => PositionStatus::ClosedManual,
        "ClosedError" => PositionStatus::ClosedError,
        _ => PositionStatus::Open, // fallback
    }
}

// ── Settings Queries ──

pub fn get_settings(db: &DbPool, chat_id: i64) -> Result<UserSettings> {
    let conn = db.lock().unwrap();
    let result = conn.query_row(
        "SELECT sniper_enabled, auto_buy_amount_sol, slippage_bps, take_profit_pct, stop_loss_pct, trailing_stop_pct, min_score_auto_buy, min_score_notify, max_buy_sol, max_positions, daily_loss_limit_sol, trade_cooldown_secs, active_wallet_index, notify_new_tokens, notify_trades, notify_pnl
         FROM settings WHERE chat_id = ?1",
        params![chat_id],
        |row| {
            Ok(UserSettings {
                chat_id,
                sniper_enabled: row.get::<_, i32>(0)? != 0,
                auto_buy_amount_sol: row.get(1)?,
                slippage_bps: row.get(2)?,
                take_profit_pct: row.get(3)?,
                stop_loss_pct: row.get(4)?,
                trailing_stop_pct: row.get(5)?,
                min_score_auto_buy: row.get(6)?,
                min_score_notify: row.get(7)?,
                max_buy_sol: row.get(8)?,
                max_positions: row.get(9)?,
                daily_loss_limit_sol: row.get(10)?,
                trade_cooldown_secs: row.get(11)?,
                active_wallet_index: row.get(12)?,
                notify_new_tokens: row.get::<_, i32>(13)? != 0,
                notify_trades: row.get::<_, i32>(14)? != 0,
                notify_pnl: row.get::<_, i32>(15)? != 0,
            })
        },
    );

    match result {
        Ok(settings) => Ok(settings),
        Err(_) => {
            let settings = UserSettings { chat_id, ..Default::default() };
            save_settings(db, &settings)?;
            Ok(settings)
        }
    }
}

pub fn save_settings(db: &DbPool, s: &UserSettings) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO settings (chat_id, sniper_enabled, auto_buy_amount_sol, slippage_bps, take_profit_pct, stop_loss_pct, trailing_stop_pct, min_score_auto_buy, min_score_notify, max_buy_sol, max_positions, daily_loss_limit_sol, trade_cooldown_secs, active_wallet_index, notify_new_tokens, notify_trades, notify_pnl)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            s.chat_id, s.sniper_enabled as i32, s.auto_buy_amount_sol, s.slippage_bps,
            s.take_profit_pct, s.stop_loss_pct, s.trailing_stop_pct,
            s.min_score_auto_buy, s.min_score_notify, s.max_buy_sol,
            s.max_positions, s.daily_loss_limit_sol, s.trade_cooldown_secs,
            s.active_wallet_index, s.notify_new_tokens as i32, s.notify_trades as i32, s.notify_pnl as i32,
        ],
    )?;
    Ok(())
}

// ── Observation Queries (observe-only mode) ──

#[derive(Debug, Clone)]
pub struct Observation {
    pub id: String,
    pub mint: String,
    pub symbol: String,
    pub source: String,
    pub security_score: u8,
    pub opportunity_score: u8,
    pub combined_score: u8,
    pub route_type: String,
    pub expected_output: u64,
    pub market_cap_sol: f64,
    pub liquidity_sol: f64,
    pub spot_price_sol: f64,
    pub wallet_sol_at_observation: f64,
    // Migration sniping fields
    pub is_migration: bool,
    pub migration_pool: Option<String>,
    pub pre_migration_v_sol: Option<f64>,
    pub filter_passed: bool,
    pub filter_reason: Option<String>,
}

pub fn insert_observation(db: &DbPool, obs: &Observation) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO observations \
         (id, mint, symbol, source, security_score, opportunity_score, combined_score, \
          route_type, expected_output, market_cap_sol, liquidity_sol, spot_price_sol, \
          wallet_sol_at_observation, observed_at, \
          is_migration, migration_pool, pre_migration_v_sol, filter_passed, filter_reason) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            obs.id,
            obs.mint,
            obs.symbol,
            obs.source,
            obs.security_score as i64,
            obs.opportunity_score as i64,
            obs.combined_score as i64,
            obs.route_type,
            obs.expected_output as i64,
            obs.market_cap_sol,
            obs.liquidity_sol,
            obs.spot_price_sol,
            obs.wallet_sol_at_observation,
            chrono::Utc::now().to_rfc3339(),
            obs.is_migration as i32,
            obs.migration_pool,
            obs.pre_migration_v_sol,
            obs.filter_passed as i32,
            obs.filter_reason,
        ],
    )?;
    Ok(())
}

// ── Stats Queries ──

#[derive(Debug, Clone, Default)]
pub struct PerformanceStats {
    // Position counts by status
    pub total_positions: u32,
    pub open: u32,
    pub closed_tp: u32,
    pub closed_sl: u32,
    pub closed_manual: u32,
    pub closed_error: u32,
    // PnL aggregates (in SOL) — only from closed positions with non-null pnl_sol
    pub total_pnl_sol: f64,
    pub winners: u32,    // closed positions with pnl_sol > 0
    pub losers: u32,     // closed positions with pnl_sol < 0
    pub avg_win_sol: f64,
    pub avg_loss_sol: f64,
    pub best_win_sol: f64,
    pub worst_loss_sol: f64,
    // Source breakdown
    pub by_source: Vec<(String, u32, f64)>,  // (source, count, sum_pnl)
    // Recent activity
    pub trades_24h: u32,
}

pub fn get_performance_stats(db: &DbPool) -> Result<PerformanceStats> {
    let conn = db.lock().unwrap();
    let mut stats = PerformanceStats::default();

    // Counts by status
    let mut stmt = conn.prepare(
        "SELECT status, COUNT(*) FROM positions GROUP BY status"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
    })?;
    for r in rows {
        let (status, count) = r?;
        stats.total_positions += count;
        match status.as_str() {
            "Open" => stats.open = count,
            "ClosedTp" => stats.closed_tp = count,
            "ClosedSl" => stats.closed_sl = count,
            "ClosedManual" => stats.closed_manual = count,
            "ClosedError" => stats.closed_error = count,
            _ => {}
        }
    }
    drop(stmt);

    // PnL aggregates from closed positions.
    // Sanity filter: |pnl_sol| < 1.0 SOL — protects against phantom data from
    // pre-Sprint-1 unit bug (entry_price was 10^6 off, producing -4999 SOL fakes).
    // Position size cap is 0.005 SOL, so even a 100x win is < 0.5 SOL.
    let mut stmt = conn.prepare(
        "SELECT pnl_sol FROM positions \
         WHERE status != 'Open' AND pnl_sol IS NOT NULL \
         AND ABS(pnl_sol) < 1.0"
    )?;
    let pnl_iter = stmt.query_map([], |row| row.get::<_, f64>(0))?;
    let mut wins: Vec<f64> = Vec::new();
    let mut losses: Vec<f64> = Vec::new();
    for r in pnl_iter {
        let pnl = r?;
        if pnl > 0.0 {
            wins.push(pnl);
        } else if pnl < 0.0 {
            losses.push(pnl);
        }
        stats.total_pnl_sol += pnl;
    }
    drop(stmt);

    stats.winners = wins.len() as u32;
    stats.losers = losses.len() as u32;
    if !wins.is_empty() {
        stats.avg_win_sol = wins.iter().sum::<f64>() / wins.len() as f64;
        stats.best_win_sol = wins.iter().cloned().fold(f64::MIN, f64::max);
    }
    if !losses.is_empty() {
        stats.avg_loss_sol = losses.iter().sum::<f64>() / losses.len() as f64;
        stats.worst_loss_sol = losses.iter().cloned().fold(f64::MAX, f64::min);
    }

    // Source breakdown — token_source column added by Sprint 1 migration.
    // Same sanity filter applied to PnL sum.
    let mut stmt = conn.prepare(
        "SELECT token_source, COUNT(*), COALESCE(SUM(CASE WHEN ABS(pnl_sol) < 1.0 THEN pnl_sol ELSE 0 END), 0) \
         FROM positions WHERE status != 'Open' \
         GROUP BY token_source ORDER BY COUNT(*) DESC"
    )?;
    let source_rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u32>(1)?,
            row.get::<_, f64>(2)?,
        ))
    })?;
    for r in source_rows {
        stats.by_source.push(r?);
    }
    drop(stmt);

    // Trades in last 24h (from trades table)
    let yesterday = chrono::Utc::now() - chrono::Duration::hours(24);
    let cutoff = yesterday.to_rfc3339();
    let count: u32 = conn.query_row(
        "SELECT COUNT(*) FROM trades WHERE created_at >= ?1",
        params![cutoff],
        |row| row.get(0),
    ).unwrap_or(0);
    stats.trades_24h = count;

    Ok(stats)
}
