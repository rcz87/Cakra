use anyhow::Result;
use rusqlite::Connection;
use tracing::{info, warn};

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
            closed_at TEXT,
            security_score INTEGER NOT NULL DEFAULT 0
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

        CREATE TABLE IF NOT EXISTS observations (
            id TEXT PRIMARY KEY,
            mint TEXT NOT NULL,
            symbol TEXT,
            source TEXT NOT NULL,
            security_score INTEGER,
            opportunity_score INTEGER,
            combined_score INTEGER,
            route_type TEXT,
            expected_output INTEGER,
            market_cap_sol REAL,
            liquidity_sol REAL,
            spot_price_sol REAL,
            wallet_sol_at_observation REAL,
            observed_at TEXT NOT NULL,
            -- post-hoc analysis fields (filled later by validator script)
            price_after_60s_sol REAL,
            price_after_300s_sol REAL,
            hypothetical_pnl_60s REAL,
            hypothetical_pnl_300s REAL,
            analyzed_at TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_observations_mint ON observations(mint);
        CREATE INDEX IF NOT EXISTS idx_observations_observed_at ON observations(observed_at);

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

    // Run idempotent migrations (ALTER TABLE ADD COLUMN)
    run_migrations(conn)?;

    Ok(())
}

/// Run idempotent migrations. Each ALTER is wrapped in a try/catch
/// because SQLite has no `IF NOT EXISTS` for ADD COLUMN — we detect
/// "duplicate column name" errors and continue.
fn run_migrations(conn: &Connection) -> Result<()> {
    let migrations: &[(&str, &str)] = &[
        // Migration 002: position metadata for source-aware execution
        ("002_positions_token_source",
         "ALTER TABLE positions ADD COLUMN token_source TEXT NOT NULL DEFAULT 'Unknown'"),
        ("002_positions_pool_address",
         "ALTER TABLE positions ADD COLUMN pool_address TEXT"),
        ("002_positions_token_decimals",
         "ALTER TABLE positions ADD COLUMN token_decimals INTEGER NOT NULL DEFAULT 6"),
        ("002_positions_price_source",
         "ALTER TABLE positions ADD COLUMN price_source TEXT"),
        ("002_positions_price_stale",
         "ALTER TABLE positions ADD COLUMN price_stale INTEGER NOT NULL DEFAULT 0"),
        ("002_positions_last_price_at",
         "ALTER TABLE positions ADD COLUMN last_price_at TEXT"),
        // Migration 003: realized PnL via wallet snapshot
        ("003_positions_wallet_sol_at_open",
         "ALTER TABLE positions ADD COLUMN wallet_sol_at_open REAL NOT NULL DEFAULT 0"),
        // Migration 004: migration sniping observations
        ("004_observations_is_migration",
         "ALTER TABLE observations ADD COLUMN is_migration INTEGER NOT NULL DEFAULT 0"),
        ("004_observations_migration_pool",
         "ALTER TABLE observations ADD COLUMN migration_pool TEXT"),
        ("004_observations_pre_migration_v_sol",
         "ALTER TABLE observations ADD COLUMN pre_migration_v_sol REAL"),
        ("004_observations_filter_passed",
         "ALTER TABLE observations ADD COLUMN filter_passed INTEGER NOT NULL DEFAULT 0"),
        ("004_observations_filter_reason",
         "ALTER TABLE observations ADD COLUMN filter_reason TEXT"),
    ];

    for (name, sql) in migrations {
        match conn.execute(sql, []) {
            Ok(_) => info!(migration = name, "Applied migration"),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("duplicate column name") {
                    // Already applied — this is the idempotent path
                } else {
                    warn!(migration = name, error = %msg, "Migration failed");
                    return Err(e.into());
                }
            }
        }
    }

    Ok(())
}
