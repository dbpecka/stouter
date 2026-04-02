use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Compute HMAC-SHA256 of `data` keyed by `secret` and return the hex-encoded digest.
pub fn sign(secret: &str, data: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(data.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Verify that `signature` (hex-encoded) matches HMAC-SHA256(`secret`, `data`).
///
/// Uses constant-time comparison to prevent timing attacks.
pub fn verify(secret: &str, data: &str, signature: &str) -> bool {
    let Ok(sig_bytes) = hex::decode(signature) else {
        return false;
    };
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(data.as_bytes());
    mac.verify_slice(&sig_bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_produces_64_char_hex() {
        let sig = sign("secret", "hello");
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sign_is_deterministic() {
        assert_eq!(sign("key", "data"), sign("key", "data"));
    }

    #[test]
    fn verify_accepts_valid_signature() {
        let sig = sign("secret", "payload");
        assert!(verify("secret", "payload", &sig));
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let sig = sign("secret", "payload");
        assert!(!verify("other", "payload", &sig));
    }

    #[test]
    fn verify_rejects_tampered_data() {
        let sig = sign("secret", "payload");
        assert!(!verify("secret", "tampered", &sig));
    }

    #[test]
    fn verify_rejects_invalid_hex() {
        assert!(!verify("secret", "data", "not-valid-hex!!"));
    }
}
