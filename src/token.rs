use sha2::{Digest, Sha256};

pub const TOKEN_PREFIX: &str = "cnc_";

/// A fresh bearer token: `cnc_` + 64 hex chars (256 bits from the OS RNG).
pub fn generate() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random generator unavailable");
    let mut s = String::with_capacity(TOKEN_PREFIX.len() + 64);
    s.push_str(TOKEN_PREFIX);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Only the SHA-256 of a token is stored. Lookups go by hash, so the raw token is
/// never compared byte by byte and never sits in the database.
pub fn hash(token: &str) -> String {
    let digest = Sha256::digest(token.trim().as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn tokens_are_unique_and_hashable() {
        let a = super::generate();
        let b = super::generate();
        assert_ne!(a, b);
        assert!(a.starts_with("cnc_") && a.len() == 68);
        assert_eq!(super::hash(&a), super::hash(&format!(" {a} ")));
        assert_eq!(super::hash(&a).len(), 64);
    }
}
