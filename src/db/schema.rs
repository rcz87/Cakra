use anyhow::Result;
use rusqlite::Connection;

pub fn create_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS wallets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            pubkey TEXT NOT NULL UNIQUE,
            encrypted_privkey TEXT NOT NULL,
            label TEXT,
            is_active INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS trades (
            id TEXT PRIMARY KEY,
            token_mint TEXT NOT NULL,
            token_symbol TEXT NOT NULL,
            trade_type TEXT NOT NULL,
            amount_sol REAL NOT NULL,
            amount_tokens REAL NOT NULL,
            price_per_token REAL NOT NULL,
            slippage_bps INTEGER NOT NULL,
            tx_signature TEXT,
            status TEXT NOT NULL,
            wallet_pubkey TEXT NOT NULL,
            created_at TEXT NOT NULL,
            confirmed_at TEXT,
            pnl_sol REAL,
            security_score INTEGER
        );

        CREATE TABLE IF NOT EXISTS tokens (
            mint TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            symbol TEXT NOT NULL,
            source TEXT NOT NULL,
            creator TEXT NOT NULL,
            initial_liquidity_sol REAL,
            initial_liquidity_usd REAL,
            pool_address TEXT,
            metadata_uri TEXT,
            decimals INTEGER NOT NULL DEFAULT 9,
            security_score INTEGER,
            security_data TEXT,
            detected_at TEXT NOT NULL,
            analyzed_at TEXT
        );

        CREATE TABLE IF NOT EXISTS positions (
            id TEXT PRIMARY KEY,
            token_mint TEXT NOT NULL,
            token_symbol TEXT NOT NULL,
            wallet_pubkey TEXT NOT NULL,
            entry_price_sol REAL NOT NULL,
            entry_amount_sol REAL NOT NULL,
            token_amount REAL NOT NULL,
            current_price_sol REAL NOT NULL,
            highest_price_sol REAL NOT NULL,
            take_profit_pct REAL NOT NULL,
            stop_loss_pct REAL NOT NULL,
            trailing_stop_pct REAL,
            pnl_sol REAL NOT NULL DEFAULT 0,
            pnl_pct REAL NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'Open',
            buy_tx TEXT NOT NULL,
            sell_tx TEXT,
            opened_at TEXT NOT NULL,
            closed_at TEXT
        );

        CREATE TABLE IF NOT EXISTS settings (
            chat_id INTEGER PRIMARY KEY,
            sniper_enabled INTEGER NOT NULL DEFAULT 0,
            auto_buy_amount_sol REAL NOT NULL DEFAULT 0.1,
            slippage_bps INTEGER NOT NULL DEFAULT 500,
            take_profit_pct REAL NOT NULL DEFAULT 100.0,
            stop_loss_pct REAL NOT NULL DEFAULT 30.0,
            trailing_stop_pct REAL,
            min_score_auto_buy INTEGER NOT NULL DEFAULT 75,
            min_score_notify INTEGER NOT NULL DEFAULT 50,
            max_buy_sol REAL NOT NULL DEFAULT 1.0,
            max_positions INTEGER NOT NULL DEFAULT 10,
            daily_loss_limit_sol REAL NOT NULL DEFAULT 5.0,
            trade_cooldown_secs INTEGER NOT NULL DEFAULT 30,
            active_wallet_index INTEGER NOT NULL DEFAULT 0,
            notify_new_tokens INTEGER NOT NULL DEFAULT 1,
            notify_trades INTEGER NOT NULL DEFAULT 1,
            notify_pnl INTEGER NOT NULL DEFAULT 1
        );

        CREATE TABLE IF NOT EXISTS blacklist (
            mint TEXT PRIMARY KEY,
            reason TEXT,
            added_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS whitelist (
            mint TEXT PRIMARY KEY,
            reason TEXT,
            added_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_trades_token ON trades(token_mint);
        CREATE INDEX IF NOT EXISTS idx_trades_wallet ON trades(wallet_pubkey);
        CREATE INDEX IF NOT EXISTS idx_positions_status ON positions(status);
        CREATE INDEX IF NOT EXISTS idx_tokens_source ON tokens(source);
        ",
    )?;
    Ok(())
}
