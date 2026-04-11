use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use anyhow::{Context, Result};
use argon2::Argon2;
use rand::RngCore;

const NONCE_SIZE: usize = 12;
const KEY_SIZE: usize = 32;

fn derive_key(password: &str, salt: &str) -> Result<[u8; KEY_SIZE]> {
    let mut key = [0u8; KEY_SIZE];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt.as_bytes(), &mut key)
        .map_err(|e| anyhow::anyhow!("Key derivation failed: {}", e))?;
    Ok(key)
}

pub fn encrypt_private_key(private_key_bytes: &[u8], password: &str, salt: &str) -> Result<String> {
    let key = derive_key(password, salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .context("Failed to create cipher")?;

    let mut nonce_bytes = [0u8; NONCE_SIZE];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, private_key_bytes)
        .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

    let mut combined = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &combined,
    ))
}

pub fn decrypt_private_key(encrypted: &str, password: &str, salt: &str) -> Result<Vec<u8>> {
    let key = derive_key(password, salt)?;
    let combined = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        encrypted,
    )
    .context("Failed to decode base64")?;

    if combined.len() < NONCE_SIZE {
        anyhow::bail!("Invalid encrypted data: too short");
    }

    let (nonce_bytes, ciphertext) = combined.split_at(NONCE_SIZE);
    let nonce = Nonce::from_slice(nonce_bytes);

    let cipher = Aes256Gcm::new_from_slice(&key)
        .context("Failed to create cipher")?;

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("Decryption failed (wrong password?): {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let original = b"test-private-key-data-64-bytes-long-for-solana-keypair-testing!";
        let password = "my-secure-password";
        let salt = "test-salt-12345678";

        let encrypted = encrypt_private_key(original, password, salt).unwrap();
        let decrypted = decrypt_private_key(&encrypted, password, salt).unwrap();

        assert_eq!(original.as_slice(), decrypted.as_slice());
    }

    #[test]
    fn test_wrong_password_fails() {
        let original = b"test-private-key";
        // Argon2 requires a salt of at least 8 bytes — use a realistic one.
        let salt = "test-salt-12345678";
        let encrypted = encrypt_private_key(original, "correct", salt).unwrap();
        let result = decrypt_private_key(&encrypted, "wrong", salt);
        assert!(result.is_err());
    }
}
