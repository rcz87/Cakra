use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signer::{keypair::Keypair, Signer},
};
use std::str::FromStr;

use crate::config::Config;
use crate::db::{self, DbPool};
use crate::security::encrypt;

/// Summary information for a stored wallet.
#[derive(Debug, Clone)]
pub struct WalletInfo {
    pub id: i64,
    pub pubkey: String,
    pub label: Option<String>,
    pub is_active: bool,
}

/// Manages Solana wallets: generation, import, encryption, storage, and RPC queries.
pub struct WalletManager {
    config: Config,
    db: DbPool,
    rpc: RpcClient,
}

impl WalletManager {
    /// Create a new `WalletManager` using the given config and database pool.
    /// An RPC client is constructed from `config.effective_rpc_url()`.
    pub fn new(config: &Config, db: DbPool) -> Result<Self> {
        let rpc = RpcClient::new(config.effective_rpc_url().to_string());
        Ok(Self {
            config: config.clone(),
            db,
            rpc,
        })
    }

    /// Generate a brand-new Solana keypair, encrypt the private key, persist it,
    /// and return the public key (base58).
    pub fn generate_wallet(&self, password: &str, label: Option<&str>) -> Result<String> {
        let keypair = Keypair::new();
        let pubkey = keypair.pubkey().to_string();

        let encrypted = encrypt::encrypt_private_key(
            &keypair.to_bytes(),
            password,
            &self.config.encryption_salt,
        )
        .context("Failed to encrypt generated keypair")?;

        db::queries::insert_wallet(&self.db, &pubkey, &encrypted, label)
            .context("Failed to store generated wallet")?;

        Ok(pubkey)
    }

    /// Import an existing wallet from a base58-encoded private key, encrypt it,
    /// persist it, and return the public key (base58).
    pub fn import_wallet(
        &self,
        private_key_b58: &str,
        password: &str,
        label: Option<&str>,
    ) -> Result<String> {
        let secret_bytes = bs58::decode(private_key_b58)
            .into_vec()
            .context("Invalid base58 private key")?;

        let keypair = Keypair::from_bytes(&secret_bytes)
            .context("Invalid keypair bytes (expected 64 bytes)")?;

        let pubkey = keypair.pubkey().to_string();

        let encrypted = encrypt::encrypt_private_key(
            &keypair.to_bytes(),
            password,
            &self.config.encryption_salt,
        )
        .context("Failed to encrypt imported keypair")?;

        db::queries::insert_wallet(&self.db, &pubkey, &encrypted, label)
            .context("Failed to store imported wallet")?;

        Ok(pubkey)
    }

    /// Retrieve the decrypted `Keypair` for the wallet identified by its pubkey.
    pub fn get_keypair(&self, pubkey: &str, password: &str) -> Result<Keypair> {
        let wallets = db::queries::get_wallets(&self.db)
            .context("Failed to query wallets")?;

        let (_id, _pub, encrypted, _label, _active) = wallets
            .into_iter()
            .find(|(_, pk, _, _, _)| pk == pubkey)
            .ok_or_else(|| anyhow::anyhow!("Wallet not found: {}", pubkey))?;

        let key_bytes = encrypt::decrypt_private_key(
            &encrypted,
            password,
            &self.config.encryption_salt,
        )
        .context("Failed to decrypt private key")?;

        Keypair::from_bytes(&key_bytes)
            .map_err(|e| anyhow::anyhow!("Invalid keypair bytes after decryption: {}", e))
    }

    /// List all stored wallets.
    pub fn list_wallets(&self) -> Result<Vec<WalletInfo>> {
        let rows = db::queries::get_wallets(&self.db)
            .context("Failed to list wallets")?;

        Ok(rows
            .into_iter()
            .map(|(id, pubkey, _encrypted, label, is_active)| WalletInfo {
                id,
                pubkey,
                label,
                is_active,
            })
            .collect())
    }

    /// Mark the wallet with the given `wallet_id` as the active wallet.
    /// All other wallets are deactivated.
    pub fn set_active(&self, wallet_id: i64) -> Result<()> {
        db::queries::set_active_wallet(&self.db, wallet_id)
            .context("Failed to set active wallet")
    }

    /// Return the currently active wallet, if any.
    pub fn get_active_wallet(&self) -> Result<Option<WalletInfo>> {
        let wallets = self.list_wallets()?;
        Ok(wallets.into_iter().find(|w| w.is_active))
    }

    /// Query the on-chain SOL balance for the given pubkey (returned in SOL, not lamports).
    pub fn get_balance(&self, pubkey: &str) -> Result<f64> {
        let pk = Pubkey::from_str(pubkey)
            .map_err(|e| anyhow::anyhow!("Invalid pubkey '{}': {}", pubkey, e))?;

        let lamports = self
            .rpc
            .get_balance(&pk)
            .context("RPC get_balance failed")?;

        Ok(lamports as f64 / LAMPORTS_PER_SOL as f64)
    }

    /// Delete a wallet by its database ID.
    pub fn delete_wallet(&self, wallet_id: i64) -> Result<()> {
        db::queries::delete_wallet(&self.db, wallet_id)
            .context("Failed to delete wallet")
    }
}
