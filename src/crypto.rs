use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

pub fn encrypt_password(password: &str, master_password: &str) -> Result<String> {
    // Derive a key from the master password
    let key = derive_key(master_password);

    let cipher = Aes256Gcm::new(&key.into());

    // Generate a random nonce
    let nonce_bytes: [u8; 12] = rand::random();
    let nonce = Nonce::from(nonce_bytes);

    // Encrypt the password
    let ciphertext = cipher
        .encrypt(&nonce, password.as_bytes())
        .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

    // Combine nonce + ciphertext and encode as base64
    let mut result = nonce_bytes.to_vec();
    result.extend_from_slice(&ciphertext);

    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &result,
    ))
}

pub fn decrypt_password(encrypted: &str, master_password: &str) -> Result<String> {
    // Derive the same key from master password
    let key = derive_key(master_password);

    let cipher = Aes256Gcm::new(&key.into());

    // Decode from base64
    let data = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encrypted)
        .context("Failed to decode base64")?;

    if data.len() < 12 {
        return Err(anyhow::anyhow!("Invalid encrypted data"));
    }

    // Split nonce and ciphertext
    let (nonce_bytes, ciphertext) = data.split_at(12);
    let nonce_array: [u8; 12] = nonce_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid nonce size"))?;
    let nonce = Nonce::from(nonce_array);

    // Decrypt
    let plaintext = cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!("Decryption failed - wrong master password?"))?;

    String::from_utf8(plaintext).context("Invalid UTF-8 in decrypted password")
}

fn derive_key(master_password: &str) -> [u8; 32] {
    // Use SHA-256 to derive a 256-bit key from the master password
    let mut hasher = Sha256::new();
    hasher.update(master_password.as_bytes());
    hasher.update(b"heimdall-salt-v1"); // Add a salt
    hasher.finalize().into()
}
