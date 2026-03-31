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
