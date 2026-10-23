use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::AuthError;

pub fn random_token() -> Result<String, AuthError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| AuthError::Random)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub fn hash_secret(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
}

pub fn constant_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Bind ciphertext to the owner and purpose so database row substitution fails closed.
pub fn seal(key: &str, context: &str, plaintext: &[u8]) -> Result<String, AuthError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(key)
        .map_err(|_| AuthError::Configuration)?;
    let cipher = Aes256Gcm::new_from_slice(&bytes).map_err(|_| AuthError::Configuration)?;
    let mut nonce = [0u8; 12];
    getrandom::getrandom(&mut nonce).map_err(|_| AuthError::Random)?;
    let nonce_value = Nonce::from(nonce);
    let ciphertext = cipher
        .encrypt(
            &nonce_value,
            Payload {
                msg: plaintext,
                aad: context.as_bytes(),
            },
        )
        .map_err(|_| AuthError::Encryption)?;
    let mut envelope = Vec::with_capacity(13 + ciphertext.len());
    envelope.push(1);
    envelope.extend_from_slice(&nonce);
    envelope.extend_from_slice(&ciphertext);
    Ok(URL_SAFE_NO_PAD.encode(envelope))
}

pub fn unseal(key: &str, context: &str, ciphertext: &str) -> Result<Vec<u8>, AuthError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(key)
        .map_err(|_| AuthError::Configuration)?;
    let cipher = Aes256Gcm::new_from_slice(&bytes).map_err(|_| AuthError::Configuration)?;
    let envelope = URL_SAFE_NO_PAD
        .decode(ciphertext)
        .map_err(|_| AuthError::Encryption)?;
    if envelope.len() < 29 || envelope[0] != 1 {
        return Err(AuthError::Encryption);
    }
    let nonce_bytes: [u8; 12] = envelope[1..13]
        .try_into()
        .map_err(|_| AuthError::Encryption)?;
    cipher
        .decrypt(
            &Nonce::from(nonce_bytes),
            Payload {
                msg: &envelope[13..],
                aad: context.as_bytes(),
            },
        )
        .map_err(|_| AuthError::Encryption)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ciphertext_cannot_cross_accounts_or_purposes() {
        let key = random_token().unwrap();
        let encrypted = seal(&key, "byok:alice:deepseek", b"provider-secret").unwrap();
        assert_eq!(
            unseal(&key, "byok:alice:deepseek", &encrypted).unwrap(),
            b"provider-secret"
        );
        assert!(unseal(&key, "byok:bob:deepseek", &encrypted).is_err());
        assert!(unseal(&key, "session:alice", &encrypted).is_err());
        assert_ne!(
            encrypted,
            seal(&key, "byok:alice:deepseek", b"provider-secret").unwrap()
        );
    }
    #[test]
    fn rejects_truncated_or_mutated_envelopes() {
        let key = random_token().unwrap();
        for size in 0..29 {
            assert!(unseal(&key, "a", &URL_SAFE_NO_PAD.encode(vec![1; size])).is_err());
        }
        let encrypted = seal(&key, "a", b"secret").unwrap();
        let mut bytes = URL_SAFE_NO_PAD.decode(encrypted).unwrap();
        bytes[15] ^= 1;
        assert!(unseal(&key, "a", &URL_SAFE_NO_PAD.encode(bytes)).is_err());
    }
}
