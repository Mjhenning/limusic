use rand::Rng;

pub fn generate_token() -> String {
    // 32 random bytes -> hex string (64 chars)
    let mut rng = rand::thread_rng();
    let bytes: [u8; 32] = rng.gen();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Constant-time comparison for bearer tokens.
pub fn tokens_equal(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_roundtrip() {
        let t = generate_token();
        assert_eq!(t.len(), 64);
        assert!(tokens_equal(&t, &t));
        assert!(!tokens_equal(&t, "wrong"));
    }
}
