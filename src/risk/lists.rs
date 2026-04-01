use anyhow::Result;
use tracing::info;

use crate::db::DbPool;

/// Manages token blacklist and whitelist entries for RICOZ SNIPER.
pub struct ListManager {
    pub db: DbPool,
}

impl ListManager {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }

    // ── Blacklist ──────────────────────────────────────────────

    /// Check whether a token mint is on the blacklist.
    pub fn is_blacklisted(&self, mint: &str) -> Result<bool> {
        let conn = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock error: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM blacklist WHERE mint = ?1",
            [mint],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Add a token mint to the blacklist with an optional reason.
    pub fn add_blacklist(&self, mint: &str, reason: Option<&str>) -> Result<()> {
        let conn = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock error: {}", e))?;
        conn.execute(
            "INSERT OR REPLACE INTO blacklist (mint, reason) VALUES (?1, ?2)",
            rusqlite::params![mint, reason],
        )?;
        info!(mint = %mint, reason = ?reason, "Token added to blacklist");
        Ok(())
    }

    /// Remove a token mint from the blacklist.
    pub fn remove_blacklist(&self, mint: &str) -> Result<()> {
        let conn = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock error: {}", e))?;
        conn.execute("DELETE FROM blacklist WHERE mint = ?1", [mint])?;
        info!(mint = %mint, "Token removed from blacklist");
        Ok(())
    }

    // ── Whitelist ──────────────────────────────────────────────

    /// Check whether a token mint is on the whitelist.
    pub fn is_whitelisted(&self, mint: &str) -> Result<bool> {
        let conn = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock error: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM whitelist WHERE mint = ?1",
            [mint],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Add a token mint to the whitelist with an optional reason.
    pub fn add_whitelist(&self, mint: &str, reason: Option<&str>) -> Result<()> {
        let conn = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock error: {}", e))?;
        conn.execute(
            "INSERT OR REPLACE INTO whitelist (mint, reason) VALUES (?1, ?2)",
            rusqlite::params![mint, reason],
        )?;
        info!(mint = %mint, reason = ?reason, "Token added to whitelist");
        Ok(())
    }

    /// Remove a token mint from the whitelist.
    pub fn remove_whitelist(&self, mint: &str) -> Result<()> {
        let conn = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock error: {}", e))?;
        conn.execute("DELETE FROM whitelist WHERE mint = ?1", [mint])?;
        info!(mint = %mint, "Token removed from whitelist");
        Ok(())
    }
}
