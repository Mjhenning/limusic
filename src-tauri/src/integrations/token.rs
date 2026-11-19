use rand::rngs::OsRng;
use rand::RngCore;
use subtle::ConstantTimeEq;

pub fn generate_token() -> String {
    // 32 random bytes from OS CSPRNG -> hex string (64 chars)
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Whether a persisted token still has the 64-hex shape we generate.
pub fn is_well_formed(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Constant-time comparison for bearer tokens.
pub fn tokens_equal(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_roundtrip() {
        let t = generate_token();
        assert_eq!(t.len(), 64);
        assert!(is_well_formed(&t));
        assert!(tokens_equal(&t, &t));
        assert!(!tokens_equal(&t, "wrong"));
        assert!(!is_well_formed("short"));
        assert!(!is_well_formed(&"z".repeat(64)));
    }
}
