//! HMAC-SHA256 for HS256 JWTs — a thin adapter over the public
//! [`ferroly::hash`](crate::hash) primitive so there is a single implementation.

/// Computes HMAC-SHA256 of `msg` under `key`.
pub(crate) fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    ferroly::hash::hmac_sha256(key, msg).into_bytes()
}
