//! Encryption of tracker credentials at rest (XChaCha20-Poly1305).
//!
//! The key comes from `CONCORD_SECRET_KEY` (base64, 32 bytes) or a key file that is created on
//! first start (mode 600). Losing the key means re-entering the tracker credentials.

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::error::{HubError, Result};

pub struct SecretBox {
    cipher: XChaCha20Poly1305,
}

impl std::fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBox(..)")
    }
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("OS random generator unavailable");
    b
}

impl SecretBox {
    pub fn from_key(key: &[u8; 32]) -> SecretBox {
        SecretBox { cipher: XChaCha20Poly1305::new(key.into()) }
    }

    /// A throwaway key (tests, or when credentials never need to survive a restart).
    pub fn ephemeral() -> SecretBox {
        SecretBox::from_key(&random::<32>())
    }

    /// Key from base64 text, else from `file` (created with a fresh key if missing).
    pub fn load(env_key: Option<&str>, file: &Path) -> Result<SecretBox> {
        if let Some(k) = env_key.map(str::trim).filter(|k| !k.is_empty()) {
            return Self::from_b64(k);
        }
        if let Ok(text) = std::fs::read_to_string(file) {
            return Self::from_b64(text.trim());
        }
        let key = random::<32>();
        write_private(file, &B64.encode(key))
            .map_err(|e| HubError::Internal(format!("cannot create the secret key file {}: {e}", file.display())))?;
        tracing::info!(path = %file.display(), "created a new secret key for tracker credentials");
        Ok(SecretBox::from_key(&key))
    }

    fn from_b64(text: &str) -> Result<SecretBox> {
        let bytes = B64.decode(text).map_err(|_| HubError::Internal("the secret key is not valid base64".into()))?;
        let key: [u8; 32] =
            bytes.try_into().map_err(|_| HubError::Internal("the secret key must be 32 bytes (base64)".into()))?;
        Ok(SecretBox::from_key(&key))
    }

    pub fn seal(&self, plain: &str) -> Vec<u8> {
        let nonce = random::<24>();
        let ct = self.cipher.encrypt(XNonce::from_slice(&nonce), plain.as_bytes()).expect("encryption cannot fail");
        let mut out = nonce.to_vec();
        out.extend(ct);
        out
    }

    pub fn open(&self, data: &[u8]) -> Result<String> {
        if data.len() < 24 {
            return Err(HubError::Internal("stored credential is corrupt".into()));
        }
        let (nonce, ct) = data.split_at(24);
        let plain = self.cipher.decrypt(XNonce::from_slice(nonce), ct).map_err(|_| {
            HubError::conflict(
                "cannot decrypt the stored tracker credential (was the secret key changed?); enter it again",
                serde_json::json!({}),
            )
        })?;
        String::from_utf8(plain).map_err(|_| HubError::Internal("stored credential is not text".into()))
    }
}

/// "…last four", for showing that a credential is set without revealing it.
pub fn hint(secret: &str) -> String {
    let s = secret.trim();
    let tail: String = s.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    if s.chars().count() <= 8 { "set".into() } else { format!("…{tail}") }
}

fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
        f.write_all(body.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tamper() {
        let b = SecretBox::ephemeral();
        let sealed = b.seal("ghp_secret");
        assert_eq!(b.open(&sealed).unwrap(), "ghp_secret");
        assert_ne!(b.seal("ghp_secret"), sealed, "fresh nonce every time");
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(b.open(&bad).is_err());
        assert!(SecretBox::ephemeral().open(&sealed).is_err(), "another key cannot read it");
        assert_eq!(hint("ghp_1234567890abcd"), "…abcd");
        assert_eq!(hint("short"), "set");
    }

    #[test]
    fn key_file_is_created_once() {
        let dir = std::env::temp_dir().join(format!("concord-key-{}", std::process::id()));
        let file = dir.join("secret.key");
        let a = SecretBox::load(None, &file).unwrap();
        let sealed = a.seal("x");
        let b = SecretBox::load(None, &file).unwrap();
        assert_eq!(b.open(&sealed).unwrap(), "x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
