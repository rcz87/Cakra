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

pub fn delete_wallet(db: &DbPool, wallet_id: i64) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute("DELETE FROM wallets WHERE id = ?1", params![wallet_id])?;
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

pub fn add_blacklist(db: &DbPool, mint: &str, reason: Option<&str>) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO blacklist (mint, reason) VALUES (?1, ?2)",
        params![mint, reason],
    )?;
    Ok(())
}

// ── Trade Queries ──

pub fn insert_trade(db: &DbPool, trade: &Trade) -> Result<()> {
    let conn = db.lock().unwrap();
    let trade_type = serde_json::to_string(&trade.trade_type)?;
    let status = serde_json::to_string(&trade.status)?;
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

pub fn update_trade_status(db: &DbPool, trade_id: &str, status: &str, tx_sig: Option<&str>) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute(
        "UPDATE trades SET status = ?1, tx_signature = ?2, confirmed_at = datetime('now') WHERE id = ?3",
        params![status, tx_sig, trade_id],
    )?;
    Ok(())
}

pub fn get_recent_trades(db: &DbPool, limit: u32) -> Result<Vec<Trade>> {
    let conn = db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id, token_mint, token_symbol, trade_type, amount_sol, amount_tokens, price_per_token, slippage_bps, tx_signature, status, wallet_pubkey, created_at, pnl_sol, security_score
         FROM trades ORDER BY created_at DESC LIMIT ?1"
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok(Trade {
            id: row.get(0)?,
            token_mint: row.get(1)?,
            token_symbol: row.get(2)?,
            trade_type: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or(crate::models::trade::TradeType::Buy),
            amount_sol: row.get(4)?,
            amount_tokens: row.get(5)?,
            price_per_token: row.get(6)?,
            slippage_bps: row.get(7)?,
            tx_signature: row.get(8)?,
            status: serde_json::from_str(&row.get::<_, String>(9)?).unwrap_or(crate::models::trade::TradeStatus::Pending),
            wallet_pubkey: row.get(10)?,
            created_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(11)?)
                .unwrap_or_default()
                .with_timezone(&chrono::Utc),
            confirmed_at: None,
            pnl_sol: row.get(12)?,
            security_score: row.get(13)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
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

pub fn insert_position(db: &DbPool, pos: &Position) -> Result<()> {
    let conn = db.lock().unwrap();
    let status = serde_json::to_string(&pos.status)?;
    conn.execute(
        "INSERT INTO positions (id, token_mint, token_symbol, wallet_pubkey, entry_price_sol, entry_amount_sol, token_amount, current_price_sol, highest_price_sol, take_profit_pct, stop_loss_pct, trailing_stop_pct, pnl_sol, pnl_pct, status, buy_tx, sell_tx, opened_at, closed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            pos.id, pos.token_mint, pos.token_symbol, pos.wallet_pubkey,
            pos.entry_price_sol, pos.entry_amount_sol, pos.token_amount,
            pos.current_price_sol, pos.highest_price_sol,
            pos.take_profit_pct, pos.stop_loss_pct, pos.trailing_stop_pct,
            pos.pnl_sol, pos.pnl_pct, status,
            pos.buy_tx, pos.sell_tx,
            pos.opened_at.to_rfc3339(),
            pos.closed_at.map(|t| t.to_rfc3339()),
        ],
    )?;
    Ok(())
}

pub fn get_open_positions(db: &DbPool) -> Result<Vec<Position>> {
    let conn = db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id, token_mint, token_symbol, wallet_pubkey, entry_price_sol, entry_amount_sol, token_amount, current_price_sol, highest_price_sol, take_profit_pct, stop_loss_pct, trailing_stop_pct, pnl_sol, pnl_pct, buy_tx, opened_at, security_score
         FROM positions WHERE status = 'Open' ORDER BY opened_at DESC"
    )?;
    let rows = stmt.query_map([], |row| {
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
            status: crate::models::position::PositionStatus::Open,
            buy_tx: row.get(14)?,
            sell_tx: None,
            opened_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(15)?)
                .unwrap_or_default()
                .with_timezone(&chrono::Utc),
            closed_at: None,
            security_score: row.get::<_, Option<u8>>(16)?.unwrap_or(0),
            age_secs: 0,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn close_position(db: &DbPool, position_id: &str, status: &str, sell_tx: &str, pnl_sol: f64) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute(
        "UPDATE positions SET status = ?1, sell_tx = ?2, pnl_sol = ?3, closed_at = datetime('now') WHERE id = ?4",
        params![status, sell_tx, pnl_sol, position_id],
    )?;
    Ok(())
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
