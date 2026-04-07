use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Compute HMAC-SHA256 of raw bytes and return the 32-byte digest.
pub fn sign_bytes(secret: &str, data: &[u8]) -> [u8; 32] {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Verify a raw-byte HMAC-SHA256 signature over raw bytes.
///
/// Uses constant-time comparison to prevent timing attacks.
pub fn verify_bytes(secret: &str, data: &[u8], signature: &[u8]) -> bool {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(data);
    mac.verify_slice(signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_bytes_produces_32_bytes() {
        let sig = sign_bytes("secret", b"hello");
        assert_eq!(sig.len(), 32);
    }

    #[test]
    fn sign_bytes_is_deterministic() {
        assert_eq!(sign_bytes("key", b"data"), sign_bytes("key", b"data"));
    }

    #[test]
    fn verify_bytes_accepts_valid_signature() {
        let sig = sign_bytes("secret", b"payload");
        assert!(verify_bytes("secret", b"payload", &sig));
    }

    #[test]
    fn verify_bytes_rejects_wrong_secret() {
        let sig = sign_bytes("secret", b"payload");
        assert!(!verify_bytes("other", b"payload", &sig));
    }

    #[test]
    fn verify_bytes_rejects_tampered_data() {
        let sig = sign_bytes("secret", b"payload");
        assert!(!verify_bytes("secret", b"tampered", &sig));
    }

    #[test]
    fn verify_bytes_rejects_wrong_length() {
        assert!(!verify_bytes("secret", b"data", &[0u8; 16]));
    }
}
